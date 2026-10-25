use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::error::ShellError;
use super::manifest::normalize_path;
use super::system::run_powershell_script;

/// Retrieve the current user's Start Menu Programs directory
pub fn get_start_menu_programs_dir() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|app_data| {
        Path::new(&app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    })
}

/// Retrieve the path to MothZip Start Menu shortcut (.lnk)
pub fn get_start_menu_shortcut_path() -> Option<PathBuf> {
    get_start_menu_programs_dir().map(|dir| dir.join("MothZip.lnk"))
}

/// Verify that a shortcut exists and its TargetPath matches the expected executable
pub fn verify_shortcut_target(lnk_path: &Path, expected_target: &Path) -> Result<bool, ShellError> {
    if !lnk_path.exists() {
        return Ok(false);
    }

    let script = r#"
param([string]$LnkPath)
$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($LnkPath)
Write-Output $s.TargetPath
"#;

    let target_out = run_powershell_script(script, &[&lnk_path.to_string_lossy()])?;
    let found_target = PathBuf::from(target_out.trim());

    Ok(normalize_path(&found_target) == normalize_path(expected_target))
}

/// Create a Windows Start Menu application shortcut (.lnk) with MothZip icon.
/// Strictly uses PowerShell -File execution protocol without temporary VBScript files.
/// Immediately verifies TargetPath upon creation.
pub fn create_start_menu_shortcut(
    exe_path: &Path,
    icon_path: Option<&Path>,
) -> Result<PathBuf, ShellError> {
    let programs_dir = get_start_menu_programs_dir().ok_or_else(|| ShellError::Shortcut {
        target: PathBuf::from("MothZip.lnk"),
        message: "APPDATA environment variable is missing".to_string(),
    })?;

    if !programs_dir.exists() {
        fs::create_dir_all(&programs_dir)?;
    }

    let lnk_path = programs_dir.join("MothZip.lnk");
    let working_dir = exe_path.parent().unwrap_or(exe_path);
    let icon = icon_path.unwrap_or(exe_path);

    let script = r#"
param([string]$LnkPath, [string]$TargetPath, [string]$IconPath, [string]$WorkDir, [string]$Description)
$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($LnkPath)
$s.TargetPath = $TargetPath
$s.IconLocation = "$IconPath,0"
$s.WorkingDirectory = $WorkDir
$s.Description = $Description
$s.Save()
"#;

    run_powershell_script(
        script,
        &[
            &lnk_path.to_string_lossy(),
            &exe_path.to_string_lossy(),
            &icon.to_string_lossy(),
            &working_dir.to_string_lossy(),
            "MothZip (나방집) - 초경량 무광고 에이전틱 압축 프로그램",
        ],
    )?;

    if !verify_shortcut_target(&lnk_path, exe_path)? {
        return Err(ShellError::Shortcut {
            target: lnk_path,
            message: format!(
                "Created shortcut TargetPath does not match expected executable: {:?}",
                exe_path
            ),
        });
    }

    Ok(lnk_path)
}

/// Remove Windows Start Menu application shortcut (.lnk), verifying ownership if expected target is given
pub fn remove_start_menu_shortcut(expected_target: Option<&Path>) -> Result<bool, ShellError> {
    if let Some(lnk_path) = get_start_menu_shortcut_path() {
        if lnk_path.exists() {
            if let Some(expected) = expected_target {
                if !verify_shortcut_target(&lnk_path, expected)? {
                    return Ok(false); // Does not belong to this executable
                }
            }
            fs::remove_file(&lnk_path)?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shortcut_path_resolution() {
        let path = get_start_menu_shortcut_path();
        assert!(path.is_some());
        let p = path.unwrap();
        assert!(p.ends_with("MothZip.lnk"));
    }
}
