use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::error::ShellError;

pub const MANIFEST_FILENAME: &str = ".mothzip-install.json";
pub const MANIFEST_MARKER: &str = "mothzip-installation-manifest";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallationManifest {
    pub marker: String,
    pub app_name: String,
    pub version: String,
    pub install_dir: PathBuf,
    pub exe_path: PathBuf,
    pub installed_files: Vec<String>,
    pub created_at_utc: String,
    pub managed: bool,
}

impl InstallationManifest {
    pub fn new(install_dir: &Path, exe_name: &str, version: &str, extra_files: &[&str]) -> Self {
        let exe_path = install_dir.join(exe_name);
        let created_at_utc = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
            .unwrap_or_else(|_| "0".into());

        let mut installed_files = vec![exe_name.to_string(), MANIFEST_FILENAME.to_string()];
        for f in extra_files {
            if !installed_files.contains(&f.to_string()) {
                installed_files.push(f.to_string());
            }
        }

        Self {
            marker: MANIFEST_MARKER.to_string(),
            app_name: "MothZip".to_string(),
            version: version.to_string(),
            install_dir: install_dir.to_path_buf(),
            exe_path,
            installed_files,
            created_at_utc,
            managed: true,
        }
    }

    /// Read and thoroughly validate installation manifest from directory
    pub fn read_and_validate(install_dir: &Path) -> Result<Self, ShellError> {
        let manifest_path = install_dir.join(MANIFEST_FILENAME);
        if !manifest_path.exists() {
            return Err(ShellError::Manifest {
                path: manifest_path,
                message: "Installation manifest not found".to_string(),
            });
        }

        if fs::symlink_metadata(&manifest_path)?
            .file_type()
            .is_symlink()
        {
            return Err(ShellError::SecurityViolation {
                path: manifest_path,
                reason: "Manifest cannot be a link".into(),
            });
        }
        let content = fs::read_to_string(&manifest_path)?;
        let manifest: Self = serde_json::from_str(&content)?;

        if manifest.marker != MANIFEST_MARKER {
            return Err(ShellError::Manifest {
                path: manifest_path,
                message: format!(
                    "Invalid marker '{}', expected '{}'",
                    manifest.marker, MANIFEST_MARKER
                ),
            });
        }

        if manifest.app_name != "MothZip" {
            return Err(ShellError::Manifest {
                path: manifest_path,
                message: format!(
                    "Invalid app_name '{}', expected 'MothZip'",
                    manifest.app_name
                ),
            });
        }

        if !manifest.managed
            || !manifest.installed_files.iter().any(|f| f == "mothzip.exe")
            || !manifest
                .installed_files
                .iter()
                .any(|f| f == MANIFEST_FILENAME)
        {
            return Err(ShellError::Manifest {
                path: manifest_path,
                message: "Invalid managed installation file set".into(),
            });
        }
        // Validate that manifest paths correspond to the actual read location
        let canonical_install = normalize_path(install_dir);
        let canonical_manifest_dir = normalize_path(&manifest.install_dir);
        if canonical_install != canonical_manifest_dir {
            return Err(ShellError::SecurityViolation {
                path: manifest_path,
                reason: format!(
                    "Manifest install_dir '{:?}' does not match actual directory '{:?}'",
                    manifest.install_dir, install_dir
                ),
            });
        }

        let expected_exe = install_dir.join("mothzip.exe");
        if normalize_path(&manifest.exe_path) != normalize_path(&expected_exe) {
            return Err(ShellError::SecurityViolation {
                path: manifest_path,
                reason: format!(
                    "Manifest exe_path '{:?}' does not match expected '{:?}'",
                    manifest.exe_path, expected_exe
                ),
            });
        }

        // Validate each file entry against directory traversal, absolute paths, and ADS
        for file in &manifest.installed_files {
            validate_installed_file_entry(file, install_dir)?;
        }

        Ok(manifest)
    }

    /// Write manifest to target directory
    #[cfg(test)]
    pub fn write_to_dir(&self, install_dir: &Path) -> Result<PathBuf, ShellError> {
        let path = install_dir.join(MANIFEST_FILENAME);
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(path)
    }

    /// Default installation directory: %LOCALAPPDATA%\Programs\MothZip
    pub fn default_install_dir() -> Result<PathBuf, ShellError> {
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            Ok(Path::new(&local_app_data).join("Programs").join("MothZip"))
        } else if let Ok(user_profile) = env::var("USERPROFILE") {
            Ok(Path::new(&user_profile)
                .join("AppData")
                .join("Local")
                .join("Programs")
                .join("MothZip"))
        } else {
            Err(ShellError::Validation {
                field: "LOCALAPPDATA".to_string(),
                message: "Neither LOCALAPPDATA nor USERPROFILE environment variable is available"
                    .to_string(),
            })
        }
    }

    /// Unified installation root validation:
    /// Checks absolute path, root/system blacklist, user profile root protection,
    /// reparse point / junction redirection, and foreign file presence.
    pub fn validate_install_root(dir: &Path) -> Result<PathBuf, ShellError> {
        if !dir.is_absolute() {
            return Err(ShellError::Validation {
                field: "install_dir".to_string(),
                message: format!("Installation path must be an absolute path: '{:?}'", dir),
            });
        }

        if dir.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ShellError::SecurityViolation {
                path: dir.to_path_buf(),
                reason: "Parent components are not allowed in installation roots".into(),
            });
        }
        for ancestor in dir.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(ShellError::SecurityViolation {
                        path: ancestor.to_path_buf(),
                        reason: "Installation path cannot traverse a link or junction".into(),
                    })
                }
                Ok(meta) if !meta.is_dir() => {
                    return Err(ShellError::SecurityViolation {
                        path: ancestor.to_path_buf(),
                        reason: "Installation ancestor is not a directory".into(),
                    })
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let normalized = normalize_path(dir);
        let norm_str = normalized.to_string_lossy().to_lowercase();
        let trimmed = norm_str.trim_end_matches('\\');

        // Disallow drive root (e.g. C:, C:\, D:\)
        let components: Vec<_> = normalized.components().collect();
        if components.len() <= 1
            || (components.len() == 2 && matches!(components[1], Component::RootDir))
        {
            return Err(ShellError::SecurityViolation {
                path: dir.to_path_buf(),
                reason: "Cannot install directly into drive root".to_string(),
            });
        }

        // Check against protected system directories
        let prohibited = [
            "c:\\windows",
            "c:\\windows\\system32",
            "c:\\program files",
            "c:\\program files (x86)",
            "c:\\programdata",
            "c:\\users",
        ];

        for p in prohibited {
            if trimmed == p {
                return Err(ShellError::SecurityViolation {
                    path: dir.to_path_buf(),
                    reason: format!(
                        "Cannot install directly into protected system directory '{}'",
                        p
                    ),
                });
            }
        }

        // Disallow user profile root directly (e.g. C:\Users\Username)
        if let Ok(profile) = env::var("USERPROFILE") {
            let prof_norm = normalize_path(Path::new(&profile))
                .to_string_lossy()
                .to_lowercase();
            if trimmed == prof_norm.trim_end_matches('\\') {
                return Err(ShellError::SecurityViolation {
                    path: dir.to_path_buf(),
                    reason: "Cannot install directly into user profile root directory".to_string(),
                });
            }
        }

        // Naming constraint: must end with 'MothZip' or reside inside %LOCALAPPDATA%\Programs
        let last_name = normalized
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let ends_with_mothzip = last_name.eq_ignore_ascii_case("mothzip");

        let in_allowed_programs = if let Ok(local) = env::var("LOCALAPPDATA") {
            normalized.starts_with(normalize_path(&Path::new(&local).join("Programs")))
        } else {
            false
        };

        let in_temp = if let Ok(temp) = env::var("TEMP") {
            normalized.starts_with(normalize_path(Path::new(&temp)))
        } else {
            false
        };

        if !ends_with_mothzip && !in_allowed_programs && !in_temp {
            return Err(ShellError::SecurityViolation {
                path: dir.to_path_buf(),
                reason: "Installation directory must end with 'MothZip' or reside in %LOCALAPPDATA%\\Programs".to_string(),
            });
        }

        // Check if directory exists and is a symlink / junction
        if dir.exists() {
            if let Ok(meta) = fs::symlink_metadata(dir) {
                if meta.file_type().is_symlink() {
                    return Err(ShellError::SecurityViolation {
                        path: dir.to_path_buf(),
                        reason: "Installation directory cannot be a symbolic link or junction"
                            .to_string(),
                    });
                }
            }

            // If directory exists and contains files, verify it already has an authentic MothZip manifest
            let entries: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
            if !entries.is_empty() {
                let manifest_path = dir.join(MANIFEST_FILENAME);
                if !manifest_path.exists() {
                    return Err(ShellError::SecurityViolation {
                        path: dir.to_path_buf(),
                        reason: "Target directory already contains files but lacks an authentic MothZip installation manifest".to_string(),
                    });
                }
            }
        }

        Ok(normalized)
    }
}

/// Validate that an installed file entry is a safe relative filename within the installation root
pub fn validate_installed_file_entry(
    entry: &str,
    install_dir: &Path,
) -> Result<PathBuf, ShellError> {
    if entry.is_empty() {
        return Err(ShellError::Validation {
            field: "installed_files".to_string(),
            message: "Empty filename entry in manifest".to_string(),
        });
    }

    // Disallow absolute paths, drive letters, and Windows Alternate Data Streams (ADS)
    if entry.starts_with('/') || entry.starts_with('\\') || entry.contains(':') {
        return Err(ShellError::SecurityViolation {
            path: PathBuf::from(entry),
            reason: "Manifest file entry cannot be an absolute path, drive path, or Alternate Data Stream".to_string(),
        });
    }

    let p = Path::new(entry);
    if p.components().count() != 1 || entry.contains('/') || entry.contains('\\') {
        return Err(ShellError::SecurityViolation {
            path: p.to_path_buf(),
            reason: "Owned installation files must be plain filenames".into(),
        });
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) => {}
            _ => {
                return Err(ShellError::SecurityViolation {
                    path: PathBuf::from(entry),
                    reason: format!(
                        "Manifest file entry contains invalid path component '{:?}'",
                        comp
                    ),
                });
            }
        }
    }

    let joined = install_dir.join(entry);
    if let Ok(meta) = fs::symlink_metadata(&joined) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(ShellError::SecurityViolation {
                path: joined,
                reason: "Owned installation file cannot be a link or directory".into(),
            });
        }
    }
    let norm_joined = normalize_path(&joined);
    let norm_install = normalize_path(install_dir);

    if !norm_joined.starts_with(&norm_install) {
        return Err(ShellError::SecurityViolation {
            path: PathBuf::from(entry),
            reason: "Manifest file entry resolves outside installation directory".to_string(),
        });
    }

    Ok(norm_joined)
}

/// Normalize Windows path (collapsing . and .. where possible, forward/backward slash uniform)
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_validation_rejects_parent_traversal() {
        let dummy_dir = Path::new("C:\\Users\\Admin\\AppData\\Local\\Programs\\MothZip");
        assert!(validate_installed_file_entry("..\\outside.txt", dummy_dir).is_err());
        assert!(validate_installed_file_entry("/etc/passwd", dummy_dir).is_err());
        assert!(validate_installed_file_entry("C:\\Windows\\notepad.exe", dummy_dir).is_err());
        assert!(validate_installed_file_entry("mothzip.exe:ads", dummy_dir).is_err());
        assert!(validate_installed_file_entry("mothzip.exe", dummy_dir).is_ok());
        assert!(validate_installed_file_entry("sub\\file.txt", dummy_dir).is_err());
    }

    #[test]
    fn test_validate_install_root_rejects_dangerous_paths() {
        assert!(InstallationManifest::validate_install_root(Path::new("C:\\")).is_err());
        assert!(InstallationManifest::validate_install_root(Path::new("C:\\Windows")).is_err());
        assert!(
            InstallationManifest::validate_install_root(Path::new("C:\\Program Files")).is_err()
        );
        assert!(
            InstallationManifest::validate_install_root(Path::new("relative\\path\\MothZip"))
                .is_err()
        );
    }

    #[test]
    fn test_validate_install_root_accepts_valid_paths() {
        assert!(
            InstallationManifest::validate_install_root(Path::new("D:\\Tools\\MothZip")).is_ok()
        );
    }

    #[test]
    fn test_foreign_files_without_manifest_rejected() {
        let temp_dir = PathBuf::from("target")
            .join("agy-desktop")
            .join(format!("test_foreign_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("foreign.txt"), b"foreign file").unwrap();

        let res = InstallationManifest::validate_install_root(&temp_dir);
        assert!(
            res.is_err(),
            "Target directory with foreign files must be rejected"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
