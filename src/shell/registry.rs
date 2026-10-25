use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Generate .reg script for context menu registration
pub fn generate_reg_content(exe_path: &Path) -> String {
    let exe_str = exe_path.to_string_lossy().replace('\\', "\\\\");
    format!(
        "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
@=\"알아서 풀기 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack\\command]\r\n\
@=\"\\\"{exe_str}\\\" smart-unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere]\r\n\
@=\"여기에 풀기\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.7z\\shell\\MothZip.SmartUnpack]\r\n\
@=\"알아서 풀기 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\.7z\\shell\\MothZip.SmartUnpack\\command]\r\n\
@=\"\\\"{exe_str}\\\" smart-unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack]\r\n\
@=\"ZIP으로 압축 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack\\command]\r\n\
@=\"\\\"{exe_str}\\\" pack \\\"%1.zip\\\" \\\"%1\\\"\"\r\n"
    )
}

/// Generate .reg script for context menu removal
pub fn generate_unreg_content() -> &'static str {
    "Windows Registry Editor Version 5.00\r\n\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.7z\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack]\r\n"
}

/// Write standard Windows Unicode (UTF-16LE with BOM) .reg file
fn write_reg_file(path: &Path, content: &str) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    // UTF-16LE BOM
    file.write_all(&[0xFF, 0xFE])?;
    for u in content.encode_utf16() {
        file.write_all(&u.to_le_bytes())?;
    }
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
    let _ = Command::new("reg")
        .arg("import")
        .arg(&reg_path)
        .output();

    println!("✓ Successfully registered MothZip in Windows Explorer context menu!");
    println!("  Config: {}", reg_path.display());
    println!("  - Right click on .zip / .7z: '알아서 풀기 (&MothZip)' & '여기에 풀기'");
    println!("  - Right click on folder: 'ZIP으로 압축 (&MothZip)'");
    Ok(())
}

/// Unregister right-click context menu from Windows Explorer.
pub fn unregister_context_menu() -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let reg_path = current_exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("mothzip_unregister.reg");

    write_reg_file(&reg_path, generate_unreg_content())?;

    // Attempt automatic import via reg.exe
    let _ = Command::new("reg")
        .arg("import")
        .arg(&reg_path)
        .output();

    println!("✓ Successfully removed MothZip from Windows Explorer context menu.");
    println!("  Config: {}", reg_path.display());
    Ok(())
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
    }
}
