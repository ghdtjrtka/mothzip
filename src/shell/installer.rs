use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use super::error::ShellError;
use super::manifest::{InstallationManifest, MANIFEST_FILENAME};
use super::model::{
    InstallOptions, InstallReport, RegisterOptions, RegisterReport, UninstallOptions,
    UninstallReport, UninstallVerificationReport, UnregisterReport, VerificationReport,
};
use super::path::{add_dir_to_user_path, is_dir_in_user_path, remove_dir_from_user_path};
use super::registry::{
    import_reg_file, unregister_all_keys, write_reg_file, RegistryProfile, SUPPORTED_EXTENSIONS,
};
use super::shortcut::{
    create_start_menu_shortcut, remove_start_menu_shortcut, verify_shortcut_target,
};
use super::system::get_powershell_exe;

/// Staged installation of MothZip with atomic commit and rollback protection
pub fn install(options: &InstallOptions) -> Result<InstallReport, ShellError> {
    let source = env::current_exe()?;
    let requested = options
        .dir
        .clone()
        .map(Ok)
        .unwrap_or_else(InstallationManifest::default_install_dir)?;
    let install_dir = InstallationManifest::validate_install_root(&requested)?;
    let existed = install_dir.exists();
    if existed && fs::read_dir(&install_dir)?.next().is_some() {
        InstallationManifest::read_and_validate(&install_dir)?;
    }
    fs::create_dir_all(&install_dir)?;
    let stage = install_dir.join(format!(
        ".install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos()
    ));
    fs::create_dir(&stage)?;
    let target_exe = install_dir.join("mothzip.exe");
    let profile = RegistryProfile::build(&target_exe, true, SUPPORTED_EXTENSIONS);
    let previous_registry = profile.snapshot()?;
    let previous_path = is_dir_in_user_path(&install_dir)?;
    let shortcut = super::shortcut::get_start_menu_shortcut_path();
    let shortcut_backup = stage.join("previous-shortcut.lnk");
    if let Some(path) = &shortcut {
        if path.is_file() {
            fs::copy(path, &shortcut_backup)?;
        }
    }
    let version = env!("CARGO_PKG_VERSION");
    let mut extras = Vec::new();
    let mut files = vec![(source.clone(), "mothzip.exe".to_string())];
    for name in [
        "LICENSE",
        "README.md",
        "README.ko.md",
        "THIRD-PARTY-LICENSES.txt",
        "manifest.json",
    ] {
        if let Some(parent) = source.parent() {
            let path = parent.join(name);
            if path.is_file() {
                extras.push(name);
                files.push((path, name.to_string()));
            }
        }
    }
    let manifest = InstallationManifest::new(&install_dir, "mothzip.exe", version, &extras);
    let mut replacements = Vec::new();
    let prepare: Result<(), ShellError> = (|| {
        for (index, (input, name)) in files.iter().enumerate() {
            let staged = stage.join(format!("{index}.new"));
            let mut reader = fs::File::open(input)?;
            let mut writer = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            let expected = reader.metadata()?.len();
            let copied = std::io::copy(&mut reader, &mut writer)?;
            if copied != expected {
                return Err(ShellError::Validation {
                    field: name.clone(),
                    message: "Source changed while staging".into(),
                });
            }
            writer.sync_all()?;
            replacements.push((staged, install_dir.join(name)));
        }
        let staged_manifest = stage.join("manifest.new");
        fs::write(&staged_manifest, serde_json::to_vec_pretty(&manifest)?)?;
        replacements.push((staged_manifest, install_dir.join(MANIFEST_FILENAME)));
        Ok(())
    })();
    if let Err(error) = prepare {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let transaction = match crate::core::commit::FileTransaction::apply(&replacements) {
        Ok(tx) => tx,
        Err(error) => {
            if !error.recovery_required {
                let _ = fs::remove_dir_all(&stage);
            }
            return Err(ShellError::Validation {
                field: "installation files".into(),
                message: error.to_string(),
            });
        }
    };
    let mut path_added = false;
    let configure: Result<(Option<std::path::PathBuf>, VerificationReport), ShellError> = (|| {
        let reg = stage.join("registration.reg");
        write_reg_file(&reg, &profile.generate_reg_content())?;
        import_reg_file(&reg)?;
        // Earlier releases registered two extra extraction verbs; an upgrade must not leave them
        // behind, so the retired keys are deleted in the same configuration step.
        super::registry::remove_legacy_verbs(SUPPORTED_EXTENSIONS)?;
        let shortcut_path = if options.no_shortcut {
            None
        } else {
            Some(create_start_menu_shortcut(&target_exe, None)?)
        };
        if !options.no_path {
            path_added = add_dir_to_user_path(&install_dir)?;
        }
        let verified = VerificationReport {
            exe_exists: target_exe.is_file(),
            version: version.into(),
            registry_registered: profile.verify_registered()?,
            shortcut_valid: match &shortcut_path {
                Some(p) => verify_shortcut_target(p, &target_exe)?,
                None => true,
            },
            path_configured: options.no_path || is_dir_in_user_path(&install_dir)?,
        };
        if !verified.exe_exists
            || !verified.registry_registered
            || !verified.shortcut_valid
            || !verified.path_configured
        {
            return Err(ShellError::Validation {
                field: "installation verification".into(),
                message: format!("{:?}", verified),
            });
        }
        Ok((shortcut_path, verified))
    })();
    let (shortcut_path, verified) = match configure {
        Ok(done) => done,
        Err(error) => {
            let mut recovery = Vec::new();
            if let Err(e) = profile.restore(&previous_registry) {
                recovery.push(e.to_string());
            }
            if path_added && !previous_path {
                if let Err(e) = remove_dir_from_user_path(&install_dir) {
                    recovery.push(e.to_string());
                }
            }
            if !options.no_shortcut {
                if shortcut_backup.exists() {
                    if let Some(path) = &shortcut {
                        if let Err(e) = fs::copy(&shortcut_backup, path) {
                            recovery.push(e.to_string());
                        }
                    }
                } else if let Err(e) = remove_start_menu_shortcut(Some(&target_exe)) {
                    recovery.push(e.to_string());
                }
            }
            if let Err(e) = transaction.rollback() {
                recovery.push(e.to_string());
            }
            if recovery.is_empty() {
                let _ = fs::remove_dir_all(&stage);
                if !existed {
                    let _ = fs::remove_dir(&install_dir);
                }
            }
            return Err(ShellError::Validation {
                field: "installation transaction".into(),
                message: format!(
                    "{error}; rollback {}. Recovery location: {}",
                    if recovery.is_empty() {
                        "completed".into()
                    } else {
                        recovery.join("; ")
                    },
                    stage.display()
                ),
            });
        }
    };
    let cleanup = transaction.finalize();
    let status = if cleanup.is_ok() {
        fs::remove_dir_all(&stage)?;
        "success"
    } else {
        "installed_cleanup_pending"
    };
    Ok(InstallReport {
        action: "install".into(),
        status: status.into(),
        version: version.into(),
        install_dir,
        exe_path: target_exe,
        installed_files: manifest.installed_files,
        registered_registry: verified.registry_registered,
        start_menu_shortcut: shortcut_path,
        user_path_updated: path_added,
        verified,
    })
}

/// Perform safe uninstallation of MothZip bounded strictly to manifest-owned files
pub fn uninstall(options: &UninstallOptions) -> Result<UninstallReport, ShellError> {
    use std::os::windows::process::CommandExt;
    let current = env::current_exe()?;
    let install_dir = if current
        .parent()
        .is_some_and(|p| p.join(MANIFEST_FILENAME).is_file())
    {
        current.parent().expect("parent checked").to_path_buf()
    } else {
        InstallationManifest::default_install_dir()?
    };
    let install_dir = InstallationManifest::validate_install_root(&install_dir)?;
    let manifest = InstallationManifest::read_and_validate(&install_dir)?;
    let manifest_path = install_dir.join(MANIFEST_FILENAME);
    let expected_manifest = fs::read_to_string(&manifest_path)?;
    let target = &manifest.exe_path;
    let self_removal = fs::canonicalize(&current)? == fs::canonicalize(target)?;
    unregister_all_keys(target, SUPPORTED_EXTENSIONS)?;
    super::registry::remove_legacy_verbs(SUPPORTED_EXTENSIONS)?;
    let removed_shortcut = remove_start_menu_shortcut(Some(target))?;
    let path_cleaned = if options.keep_path {
        false
    } else {
        remove_dir_from_user_path(&install_dir)?
    };
    let mut removed_files = Vec::new();
    let mut cleanup_result_path = None;
    if self_removal {
        let renamed_name = format!("mothzip.exe.removing-{}", std::process::id());
        let renamed = install_dir.join(&renamed_name);
        if renamed.exists() {
            return Err(ShellError::Validation {
                field: "uninstall".into(),
                message: "Previous removal artifact exists; refusing to overwrite it".into(),
            });
        }
        let helper = r#"
param([int]$ParentPid,[string]$InstallDir,[string]$RenamedExe,[string]$ExpectedManifest,[string]$LogPath)
try {
    $parent = Get-Process -Id $ParentPid -ErrorAction SilentlyContinue
    if ($null -ne $parent) { $parent.WaitForExit() }
    $root = [IO.Path]::GetFullPath($InstallDir)
    $check = $root
    while ($check) {
        $node = Get-Item -LiteralPath $check -Force -ErrorAction SilentlyContinue
        if ($node -and (($node.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'Installation boundary changed to a reparse point' }
        $check = [IO.Path]::GetDirectoryName($check.TrimEnd('\'))
    }
    $manifestPath = Join-Path $root '.mothzip-install.json'
    $currentManifest = [IO.File]::ReadAllText($manifestPath)
    if (-not [string]::Equals($currentManifest,$ExpectedManifest,[StringComparison]::Ordinal)) { throw 'Installation changed after uninstall was scheduled; files retained' }
    $manifest = $currentManifest | ConvertFrom-Json
    if ($manifest.marker -ne 'mothzip-installation-manifest' -or $manifest.app_name -ne 'MothZip' -or -not $manifest.managed) { throw 'Invalid installation identity' }
    $removed = @()
    foreach ($name in $manifest.installed_files) {
        if ($name -eq '.mothzip-install.json') { continue }
        if ([IO.Path]::GetFileName($name) -ne $name -or $name.Contains(':') -or $name -in @('.','..')) { throw 'Invalid owned filename' }
        $actualName = if ($name -eq 'mothzip.exe') { $RenamedExe } else { $name }
        $file = Join-Path $root $actualName
        $item = Get-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
        if ($item) {
            if ($item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'Owned file changed type' }
            Remove-Item -LiteralPath $file -Force
            $removed += $name
        }
    }
    Remove-Item -LiteralPath $manifestPath
    $removed += '.mothzip-install.json'
    $remaining = @(Get-ChildItem -LiteralPath $root -Force)
    $directoryRemoved = $false
    if ($remaining.Count -eq 0) { [IO.Directory]::Delete($root,$false);$directoryRemoved=$true }
    $result = @{status='completed';files_removed=$true;removed_files=$removed;directory_removed=$directoryRemoved;remaining_items=@($remaining.Name)}
} catch {
    $result = @{status='failed';files_removed=$false;error=$_.Exception.Message}
}
[IO.File]::WriteAllText($LogPath,($result|ConvertTo-Json -Depth 5),[Text.UTF8Encoding]::new($false))
try { Remove-Item -LiteralPath $PSCommandPath -Force } catch {}
if ($result.status -ne 'completed') {exit 1}
"#;
        let script = super::system::create_script(helper)?;
        let log = script.with_extension("result.json");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log)?;
        fs::rename(&current, &renamed)?;
        let child = Command::new(get_powershell_exe()?)
            .creation_flags(0x0800_0000)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(std::process::id().to_string())
            .arg(&install_dir)
            .arg(&renamed_name)
            .arg(&expected_manifest)
            .arg(&log)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = child {
            let _ = fs::rename(&renamed, &current);
            return Err(ShellError::CleanupHelperFailed {
                message: format!(
                    "{error}; installed files retained at {}",
                    install_dir.display()
                ),
            });
        }
        cleanup_result_path = Some(log);
    } else {
        for name in manifest
            .installed_files
            .iter()
            .filter(|name| name.as_str() != MANIFEST_FILENAME)
        {
            let path = super::manifest::validate_installed_file_entry(name, &install_dir)?;
            if path.is_file() {
                fs::remove_file(path)?;
                removed_files.push(name.clone());
            }
        }
        fs::remove_file(&manifest_path)?;
        removed_files.push(MANIFEST_FILENAME.into());
        if fs::read_dir(&install_dir)?.next().is_none() {
            fs::remove_dir(&install_dir)?;
        }
    }
    Ok(UninstallReport {
        action: "uninstall".into(),
        status: if self_removal { "scheduled" } else { "success" }.into(),
        install_dir: install_dir.clone(),
        removed_files,
        deferred_delete_scheduled: self_removal,
        cleanup_result_path,
        unregistered_registry: true,
        removed_shortcut,
        user_path_cleaned: path_cleaned,
        verified: UninstallVerificationReport {
            files_removed: !self_removal && !manifest_path.exists() && !target.exists(),
            registry_removed: true,
            shortcut_removed: removed_shortcut,
            path_cleaned: !is_dir_in_user_path(&install_dir)?,
        },
    })
}

/// Register current executable in-place as portable application
pub fn register(options: &RegisterOptions) -> Result<RegisterReport, ShellError> {
    let current_exe = env::current_exe()?;
    let reg_path = current_exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("mothzip_register.reg");

    let profile = RegistryProfile::build(&current_exe, false, SUPPORTED_EXTENSIONS);
    let content = profile.generate_reg_content();
    write_reg_file(&reg_path, &content)?;

    let shortcut_path = if !options.no_shortcut {
        Some(create_start_menu_shortcut(&current_exe, None)?)
    } else {
        None
    };

    import_reg_file(&reg_path)?;
    let verified = profile.verify_registered()?
        && match &shortcut_path {
            Some(path) => verify_shortcut_target(path, &current_exe)?,
            None => true,
        };
    if !verified {
        return Err(ShellError::Validation {
            field: "portable registration".into(),
            message: "Registration verification failed".into(),
        });
    }

    Ok(RegisterReport {
        action: "register".to_string(),
        status: "success".to_string(),
        exe_path: current_exe,
        reg_file: Some(reg_path),
        start_menu_shortcut: shortcut_path,
        verified,
    })
}

/// Unregister context menu and application registrations (does NOT delete executable files)
pub fn unregister() -> Result<UnregisterReport, ShellError> {
    let current_exe = env::current_exe()?;
    let removed_keys = unregister_all_keys(&current_exe, SUPPORTED_EXTENSIONS)?;
    let removed_shortcut = remove_start_menu_shortcut(Some(&current_exe))?;

    Ok(UnregisterReport {
        action: "unregister".to_string(),
        status: "success".to_string(),
        unregistered_keys: removed_keys,
        removed_shortcut,
        verified: true,
    })
}
