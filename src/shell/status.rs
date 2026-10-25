use std::env;
use std::path::{Path, PathBuf};

use super::error::ShellError;
use super::manifest::{InstallationManifest, MANIFEST_FILENAME};
use super::model::{RegistryStatusDetails, StatusReport};
use super::path::is_dir_in_user_path;
use super::registry::SUPPORTED_EXTENSIONS;
use super::shortcut::{get_start_menu_shortcut_path, verify_shortcut_target};
use super::system::run_powershell_script;

/// Query a specific registry value using PowerShell with Unicode and CP949 safety.
/// Returns Ok(None) when the key/value is absent; propagates tool failures.
fn query_reg_value_unicode(key_path: &str, value_name: &str) -> Result<Option<String>, ShellError> {
    let script = r#"
param([string]$KeyPath, [string]$ValueName)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$prop = Get-ItemProperty -LiteralPath $KeyPath -ErrorAction SilentlyContinue
if ($null -ne $prop) {
    if ($ValueName -eq "") {
        Write-Output $prop.'(default)'
    } else {
        Write-Output $prop.$ValueName
    }
}
"#;
    let out = run_powershell_script(script, &[key_path, value_name])?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Query if a registry key exists. Propagates tool failures.
fn check_reg_key_exists(key_path: &str) -> Result<bool, ShellError> {
    let script = r#"
param([string]$KeyPath)
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $KeyPath) {
    Write-Output "YES"
}
"#;
    let out = run_powershell_script(script, &[key_path])?;
    Ok(out.trim() == "YES")
}

/// Check if official Bandizip is currently installed on the host (filesystem and registry).
pub fn is_bandizip_detected() -> Result<bool, ShellError> {
    for p in &[
        "C:\\Program Files\\Bandizip\\Bandizip.exe",
        "C:\\Program Files\\Bandizip\\bz.exe",
        "C:\\Program Files (x86)\\Bandizip\\Bandizip.exe",
        "C:\\Program Files (x86)\\Bandizip\\bz.exe",
    ] {
        if Path::new(p).exists() {
            return Ok(true);
        }
    }

    for key in &[
        "HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Bandizip",
        "HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Bandizip",
        "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Bandizip",
    ] {
        if check_reg_key_exists(key)? {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Retrieve comprehensive status report of MothZip installation and integration.
/// Only verified values are reported; verification tool failures propagate.
pub fn get_status() -> Result<StatusReport, ShellError> {
    let current_exe = env::current_exe()?;
    let default_dir = InstallationManifest::default_install_dir().ok();

    let uninstall_key = "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MothZip";
    let has_uninstall = check_reg_key_exists(uninstall_key)?;
    let uninstall_loc = query_reg_value_unicode(uninstall_key, "InstallLocation")?;
    let uninstall_str = query_reg_value_unicode(uninstall_key, "UninstallString")?;
    let registered_dir = uninstall_loc
        .as_ref()
        .map(|s| PathBuf::from(s.trim_matches('"')));
    let manifest = registered_dir
        .as_ref()
        .or(default_dir.as_ref())
        .and_then(|dir| InstallationManifest::read_and_validate(dir).ok());
    let is_managed = manifest.as_ref().is_some_and(|m| m.managed);
    let manifest_install_dir = manifest.map(|m| m.install_dir);

    let app_paths_key =
        "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\mothzip.exe";
    let has_app_paths = check_reg_key_exists(app_paths_key)?;

    let context_key = "HKCU:\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack";
    let has_context = check_reg_key_exists(context_key)?;

    let reg_apps_key = "HKCU:\\Software\\RegisteredApplications";
    let has_reg_apps = query_reg_value_unicode(reg_apps_key, "MothZip")?.is_some();

    let registry_details = RegistryStatusDetails {
        uninstall_entry: has_uninstall,
        uninstall_location: uninstall_loc,
        uninstall_string: uninstall_str,
        app_paths_entry: has_app_paths,
        context_menu_verbs: has_context,
        registered_applications: has_reg_apps,
    };

    let registered_in_registry = has_uninstall && has_app_paths && has_context;

    let install_dir = manifest_install_dir.or_else(|| {
        registry_details
            .uninstall_location
            .as_ref()
            .map(|s| PathBuf::from(s.trim_matches('"')))
    });

    let shortcut_path = get_start_menu_shortcut_path();
    let shortcut_exists = if let (Some(ref lnk), Some(ref dir)) = (&shortcut_path, &install_dir) {
        verify_shortcut_target(lnk, &dir.join("mothzip.exe"))?
    } else if let Some(ref lnk) = shortcut_path {
        verify_shortcut_target(lnk, &current_exe)?
    } else {
        false
    };

    let in_path = if let Some(ref dir) = install_dir {
        is_dir_in_user_path(dir)?
    } else {
        false
    };

    let is_installed = is_managed
        && install_dir
            .as_ref()
            .map(|d| d.join("mothzip.exe").exists() && d.join(MANIFEST_FILENAME).exists())
            .unwrap_or(false);

    let install_type = if is_installed {
        "managed".to_string()
    } else if registered_in_registry {
        "portable_registered".to_string()
    } else {
        "none".to_string()
    };

    let bandizip_detected = is_bandizip_detected()?;
    let supported_extensions = SUPPORTED_EXTENSIONS.iter().map(|s| s.to_string()).collect();

    Ok(StatusReport {
        is_installed,
        install_type,
        current_exe,
        install_dir,
        version: env!("CARGO_PKG_VERSION").to_string(),
        registered_in_registry,
        registry_details,
        start_menu_shortcut_exists: shortcut_exists,
        in_user_path: in_path,
        bandizip_detected,
        supported_extensions,
    })
}
