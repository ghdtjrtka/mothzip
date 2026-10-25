use std::path::Path;

use super::error::ShellError;
use super::manifest::normalize_path;
use super::system::run_powershell_script;

/// Query the current user's PATH environment variable (from HKCU\Environment)
pub fn get_user_path() -> Result<String, ShellError> {
    let script = r#"
[Environment]::GetEnvironmentVariable('Path', 'User')
"#;
    run_powershell_script(script, &[])
}

/// Check if a directory is present in the current user's PATH
pub fn is_dir_in_user_path(dir: &Path) -> Result<bool, ShellError> {
    let user_path = get_user_path()?;
    let norm_target = normalize_path(dir).to_string_lossy().to_lowercase();

    for entry in user_path.split(';') {
        let trimmed = entry.trim();
        if !trimmed.is_empty() {
            let norm_entry = normalize_path(Path::new(trimmed))
                .to_string_lossy()
                .to_lowercase();
            if norm_entry == norm_target {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Add directory to the current user's PATH (idempotent, broadcasts WM_SETTINGCHANGE)
pub fn add_dir_to_user_path(dir: &Path) -> Result<bool, ShellError> {
    if is_dir_in_user_path(dir)? {
        return Ok(false); // Already present
    }

    let dir_str = dir.to_string_lossy().replace('/', "\\");
    let script = r#"
param([string]$ToAdd)
$current = [Environment]::GetEnvironmentVariable('Path', 'User')
if ([string]::IsNullOrWhiteSpace($current)) {
    $newPath = $ToAdd
} else {
    $parts = $current.Split(';') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    $target = $ToAdd.TrimEnd('\').ToLowerInvariant()
    foreach ($p in $parts) {
        if ($p.Trim().TrimEnd('\').ToLowerInvariant() -eq $target) {
            exit 0
        }
    }
    $newPath = "$current;$ToAdd"
}
[Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
"#;

    run_powershell_script(script, &[&dir_str])?;
    Ok(true)
}

/// Remove directory from current user's PATH (only if matching, broadcasts WM_SETTINGCHANGE)
pub fn remove_dir_from_user_path(dir: &Path) -> Result<bool, ShellError> {
    if !is_dir_in_user_path(dir)? {
        return Ok(false); // Not present
    }

    let dir_str = dir.to_string_lossy().replace('/', "\\");
    let script = r#"
param([string]$ToRemove)
$current = [Environment]::GetEnvironmentVariable('Path', 'User')
if ([string]::IsNullOrWhiteSpace($current)) { exit 0 }
$target = $ToRemove.TrimEnd('\').ToLowerInvariant()
$parts = $current.Split(';') | Where-Object {
    $_.Trim().TrimEnd('\').ToLowerInvariant() -ne $target -and -not [string]::IsNullOrWhiteSpace($_)
}
$newPath = $parts -join ';'
[Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
"#;

    run_powershell_script(script, &[&dir_str])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_dir_in_path_parsing() {
        let user_path = "C:\\Tools;C:\\Users\\Admin\\AppData\\Local\\Programs\\MothZip;D:\\bin";
        let target = Path::new("c:/users/admin/appdata/local/programs/mothzip");

        let found = user_path.split(';').any(|e| {
            normalize_path(Path::new(e.trim()))
                .to_string_lossy()
                .eq_ignore_ascii_case(&normalize_path(target).to_string_lossy())
        });
        assert!(found);

        let not_found = user_path.split(';').any(|e| {
            normalize_path(Path::new(e.trim()))
                .to_string_lossy()
                .eq_ignore_ascii_case("C:\\NonExistent")
        });
        assert!(!not_found);
    }
}
