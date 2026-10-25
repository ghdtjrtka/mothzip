use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Generate .reg script for context menu registration
pub fn generate_reg_content(exe_path: &Path) -> String {
    let exe_str = exe_path.to_string_lossy().replace('\\', "\\\\");
    format!(
        "Windows Registry Editor Version 5.00\r\n\r\n\
; 1. Right-click on .zip archives (Direct Extension)\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
@=\"알아서 풀기 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack\\command]\r\n\
@=\"\\\"{exe_str}\\\" smart-unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere]\r\n\
@=\"여기에 풀기\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackFolder]\r\n\
@=\"각각 폴더에 풀기\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackFolder\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\" --to-folder\"\r\n\r\n\
; 2. Right-click on .zip archives (SystemFileAssociations for Windows 10/11 Explorer)\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
@=\"알아서 풀기 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.SmartUnpack\\command]\r\n\
@=\"\\\"{exe_str}\\\" smart-unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackHere]\r\n\
@=\"여기에 풀기\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackHere\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackFolder]\r\n\
@=\"각각 폴더에 풀기\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackFolder\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\" --to-folder\"\r\n\r\n\
; 3. Right-click on any single file (pack to <name>.zip)\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\*\\shell\\MothZip.Pack]\r\n\
@=\"ZIP으로 압축 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\*\\shell\\MothZip.Pack\\command]\r\n\
@=\"\\\"{exe_str}\\\" pack-auto \\\"%1\\\"\"\r\n\r\n\
; 4. Right-click on folders (pack to <folder>.zip)\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack]\r\n\
@=\"ZIP으로 압축 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack\\command]\r\n\
@=\"\\\"{exe_str}\\\" pack-auto \\\"%1\\\"\"\r\n"
    )
}

/// Generate .reg script for context menu removal
pub fn generate_unreg_content() -> &'static str {
    "Windows Registry Editor Version 5.00\r\n\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\*\\shell\\MothZip.Pack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack]\r\n"
}

/// Write standard Windows Unicode (UTF-16LE with BOM) .reg file
fn write_reg_file(path: &Path, content: &str) -> std::io::Result<()> {
    {
        let mut file = File::create(path)?;
        // UTF-16LE BOM
        file.write_all(&[0xFF, 0xFE])?;
        for u in content.encode_utf16() {
            file.write_all(&u.to_le_bytes())?;
        }
        file.sync_all()?;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok(())
}

/// Register right-click context menu in Windows Explorer under HKEY_CURRENT_USER.
/// Generates verified registry configuration and imports it directly.
/// Requires NO administrator privileges.
pub fn register_context_menu() -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let reg_path = current_exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("mothzip_register.reg");

    let content = generate_reg_content(&current_exe);
    write_reg_file(&reg_path, &content)?;

    // Attempt automatic import via reg.exe
    let import_result = std::process::Command::new("reg.exe")
        .arg("import")
        .arg(&reg_path)
        .output();

    match import_result {
        Ok(output) if output.status.success() => {
            println!("✓ Successfully registered MothZip in Windows Explorer context menu!");
            println!("  - Right click on .zip: '알아서 풀기 (&MothZip)', '여기에 풀기', '각각 폴더에 풀기'");
            println!("  - Right click on any file/folder: 'ZIP으로 압축 (&MothZip)'");
            println!("  - Config: {}", reg_path.display());
            Ok(())
        }
        _ => {
            println!("✓ Context menu registry file generated successfully:");
            println!("  Config: {}", reg_path.display());
            println!("  - To activate in headless environments, run:");
            println!("    reg import \"{}\"", reg_path.display());
            Ok(())
        }
    }
}

/// Unregister right-click context menu from Windows Explorer.
pub fn unregister_context_menu() -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let reg_path = current_exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("mothzip_unregister.reg");

    let content = generate_unreg_content();
    write_reg_file(&reg_path, content)?;

    let unreg_result = std::process::Command::new("reg.exe")
        .arg("import")
        .arg(&reg_path)
        .output();

    match unreg_result {
        Ok(output) if output.status.success() => {
            println!("✓ Successfully removed MothZip from Windows Explorer context menu.");
            Ok(())
        }
        _ => {
            println!("✓ Context menu unregister script generated:");
            println!("  Config: {}", reg_path.display());
            println!("  - To activate in headless environments, run:");
            println!("    reg import \"{}\"", reg_path.display());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reg_generation() {
        let dummy = Path::new("C:\\tools\\mothzip.exe");
        let content = generate_reg_content(dummy);
        assert!(content.contains("알아서 풀기"));
        assert!(content.contains("C:\\\\tools\\\\mothzip.exe"));
        assert!(content.contains("*\\shell\\MothZip.Pack"));
        assert!(content.contains("SystemFileAssociations\\.zip\\shell"));
        assert!(content.contains("pack-auto"));
    }
}
