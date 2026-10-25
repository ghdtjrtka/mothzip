use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("Zip-Slip detected: entry '{0}' attempts path traversal outside target directory")]
    ZipSlip(String),
    #[error("Zip-Bomb detected: total uncompressed size ({size_gb:.2} GB) exceeds safe threshold ({limit_gb:.2} GB)")]
    ZipBomb { size_gb: f64, limit_gb: f64 },
}

/// Default maximum allowed uncompressed size in bytes (10 GB)
pub const DEFAULT_MAX_UNCOMPRESSED_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Sanitize an internal archive entry path and resolve it safely inside `base_dir`.
/// Prevents Zip-Slip vulnerability by rejecting `..`, absolute paths, and Windows drive prefixes.
pub fn sanitize_entry_path(base_dir: &Path, raw_path: &str) -> Result<PathBuf, GuardError> {
    let p = Path::new(raw_path);

    // Check for malicious components
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                return Err(GuardError::ZipSlip(raw_path.to_string()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(GuardError::ZipSlip(raw_path.to_string()));
            }
            Component::Normal(_) | Component::CurDir => {}
        }
    }

    let mut dest = base_dir.to_path_buf();
    for comp in p.components() {
        if let Component::Normal(c) = comp {
            dest.push(c);
        }
    }

    // Double check containment
    if !dest.starts_with(base_dir) {
        return Err(GuardError::ZipSlip(raw_path.to_string()));
    }

    Ok(dest)
}

/// Check whether the total uncompressed size exceeds safe bounds.
pub fn check_zip_bomb(total_uncompressed_bytes: u64, max_allowed: u64) -> Result<(), GuardError> {
    if total_uncompressed_bytes > max_allowed {
        let size_gb = total_uncompressed_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let limit_gb = max_allowed as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(GuardError::ZipBomb { size_gb, limit_gb });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zip_slip_rejection() {
        let base = Path::new("C:\\safe\\extract");
        assert!(sanitize_entry_path(base, "../evil.exe").is_err());
        assert!(sanitize_entry_path(base, "sub/../../evil.exe").is_err());
        assert!(sanitize_entry_path(base, "/etc/shadow").is_err());
        assert!(sanitize_entry_path(base, "C:\\Windows\\system32").is_err());
    }

    #[test]
    fn test_valid_paths() {
        let base = Path::new("C:\\safe\\extract");
        let safe = sanitize_entry_path(base, "folder/sub/file.txt").unwrap();
        assert_eq!(safe, base.join("folder").join("sub").join("file.txt"));
    }
}
