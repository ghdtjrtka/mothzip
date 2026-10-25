#[cfg(not(test))]
use std::env;
use std::fs;
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::error::ShellError;

use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
mod win {
    use std::path::PathBuf;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemDirectoryW(lpBuffer: *mut u16, nSize: u32) -> u32;
        fn GetWindowsDirectoryW(lpBuffer: *mut u16, nSize: u32) -> u32;
    }

    fn query_dir(
        f: unsafe extern "system" fn(*mut u16, u32) -> u32,
    ) -> Result<PathBuf, std::io::Error> {
        let mut cap = 260u32;
        loop {
            let mut buf = vec![0u16; cap as usize];
            let n = unsafe { f(buf.as_mut_ptr(), cap) };
            if n == 0 {
                return Err(std::io::Error::other("Win32 directory query failed"));
            }
            if n < cap {
                buf.truncate(n as usize);
                return Ok(PathBuf::from(String::from_utf16_lossy(&buf)));
            }
            cap = n + 1;
        }
    }

    pub fn system_dir() -> Result<PathBuf, std::io::Error> {
        query_dir(GetSystemDirectoryW)
    }

    pub fn windows_dir() -> Result<PathBuf, std::io::Error> {
        query_dir(GetWindowsDirectoryW)
    }
}

#[cfg(not(windows))]
mod win {
    use std::path::PathBuf;
    pub fn system_dir() -> Result<PathBuf, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "not a Windows platform",
        ))
    }
    pub fn windows_dir() -> Result<PathBuf, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "not a Windows platform",
        ))
    }
}

/// Resolve the system directory (GetSystemDirectoryW), erroring if unavailable.
pub fn get_system_dir() -> Result<PathBuf, ShellError> {
    win::system_dir().map_err(ShellError::Io)
}

/// Resolve the Windows directory (GetWindowsDirectoryW), erroring if unavailable.
pub fn get_windows_dir() -> Result<PathBuf, ShellError> {
    win::windows_dir().map_err(ShellError::Io)
}

/// Retrieve the verified absolute path to powershell.exe via GetSystemDirectoryW.
pub fn get_powershell_exe() -> Result<PathBuf, ShellError> {
    let p = get_system_dir()?
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !p.exists() {
        return Err(ShellError::Io(IoError::new(
            ErrorKind::NotFound,
            format!("powershell.exe not found at {}", p.display()),
        )));
    }
    Ok(p)
}

/// Retrieve the verified absolute path to reg.exe via GetSystemDirectoryW.
pub fn get_reg_exe() -> Result<PathBuf, ShellError> {
    let p = get_system_dir()?.join("reg.exe");
    if !p.exists() {
        return Err(ShellError::Io(IoError::new(
            ErrorKind::NotFound,
            format!("reg.exe not found at {}", p.display()),
        )));
    }
    Ok(p)
}

/// Retrieve the verified absolute path to explorer.exe via GetWindowsDirectoryW.
pub fn get_explorer_exe() -> Result<PathBuf, ShellError> {
    let p = get_windows_dir()?.join("explorer.exe");
    if !p.exists() {
        return Err(ShellError::Io(IoError::new(
            ErrorKind::NotFound,
            format!("explorer.exe not found at {}", p.display()),
        )));
    }
    Ok(p)
}

fn inject_script_preamble(script: &str) -> String {
    let preamble = "$ErrorActionPreference = 'Stop'\r\n[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\r\n";
    if let Some(param_pos) = script.find("param") {
        if let Some(open_paren) = script[param_pos..].find('(') {
            let start = param_pos + open_paren;
            let mut depth = 0;
            let mut end_pos = None;
            for (i, c) in script[start..].char_indices() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        end_pos = Some(start + i + 1);
                        break;
                    }
                }
            }
            if let Some(end) = end_pos {
                let (param_block, rest) = script.split_at(end);
                return format!("{}\r\n{}\r\n{}", param_block, preamble, rest);
            }
        }
    }
    format!("{}\r\n{}", preamble, script)
}

/// Directory for temporary PowerShell scripts.
/// Unit tests execute from the workspace's low-integrity build directory.
/// Installed releases use the Windows temporary directory.
fn script_temp_dir() -> PathBuf {
    #[cfg(test)]
    {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/shell_tmp")
    }
    #[cfg(not(test))]
    {
        env::temp_dir()
    }
}

/// Write a temporary PowerShell script with a UTF-8 BOM, failing if the file
/// already exists (create_new) so a pre-existing/symlinked path is never reused.
fn write_temp_script(path: &Path, script: &str) -> Result<(), ShellError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut f = options.open(path).map_err(|e| {
        std::io::Error::new(e.kind(), format!("create script {}: {e}", path.display()))
    })?;
    use std::io::Write;
    f.write_all(&[0xEF, 0xBB, 0xBF])?;
    f.write_all(script.as_bytes())?;
    f.sync_all().map_err(|e| {
        std::io::Error::new(e.kind(), format!("sync script {}: {e}", path.display()))
    })?;
    Ok(())
}

fn temp_script_path(script_id: &str) -> PathBuf {
    script_temp_dir().join(format!("mothzip_ps_{script_id}.ps1"))
}

/// Execute a temporary PowerShell script file via -File with structured parameters.
/// Strictly enforces $ErrorActionPreference = 'Stop' so non-terminating errors fail the exit code.
/// Ensures Unicode, spaces, apostrophes, and ampersands are preserved byte-for-byte.
pub fn create_script(script_body: &str) -> Result<PathBuf, ShellError> {
    let script_id = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let temp_script = temp_script_path(&script_id);

    let full_script = inject_script_preamble(script_body);
    write_temp_script(&temp_script, &full_script)?;

    Ok(temp_script)
}

pub fn run_powershell_script(script_body: &str, args: &[&str]) -> Result<String, ShellError> {
    let temp_script = create_script(script_body)?;

    let ps_exe = get_powershell_exe()?;
    let mut cmd = Command::new(&ps_exe);
    cmd.creation_flags(CREATE_NO_WINDOW).args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &temp_script.to_string_lossy(),
    ]);

    for arg in args {
        cmd.arg(arg);
    }

    let output = cmd.output();
    let _ = fs::remove_file(&temp_script);

    let output = output?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ShellError::SystemCommand {
            command: format!("powershell.exe -File ... ({} args)", args.len()),
            exit_code: output.status.code(),
            stderr,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn per_test_script_dir() -> PathBuf {
        let dir = Path::new("target").join("shell_tmp");
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn test_system_paths_exist() {
        assert!(
            get_powershell_exe().unwrap().exists(),
            "PowerShell system path must exist"
        );
        assert!(
            get_reg_exe().unwrap().exists(),
            "reg.exe system path must exist"
        );
        assert!(
            get_explorer_exe().unwrap().exists(),
            "explorer.exe system path must exist"
        );
    }

    #[test]
    fn test_powershell_preserves_special_characters_and_hangul() {
        per_test_script_dir();
        let script = r#"
param([string]$target, [string]$desc)
Write-Output "TARGET:$target"
Write-Output "DESC:$desc"
"#;
        let test_target = "C:\\Program Files\\나방집 (Moth's & Zip)\\mothzip.exe";
        let test_desc = "Special 'quotes' & ampersand & 한글!";

        let out = run_powershell_script(script, &[test_target, test_desc])
            .expect("PowerShell run must succeed and preserve arguments");
        assert!(out.contains(&format!("TARGET:{}", test_target)));
        assert!(out.contains(&format!("DESC:{}", test_desc)));
    }

    #[test]
    fn test_powershell_strict_error_preference_stops_on_failure() {
        per_test_script_dir();
        let script = r#"
param([string]$nonExistent)
Get-Item -LiteralPath $nonExistent
"#;
        let res = run_powershell_script(script, &["C:\\Definitively\\Non\\Existent\\Path_12345"]);
        assert!(
            res.is_err(),
            "Non-terminating error must be promoted to failure by ErrorActionPreference = Stop"
        );
    }
}
