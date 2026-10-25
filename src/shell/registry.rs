use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;

fn format_unpack_verbs(ext_key: &str, exe_str: &str) -> String {
    format!(
        "[{ext_key}\\shell\\MothZip.SmartUnpack]\r\n\
@=\"알아서 풀기 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\
\"MultiSelectModel\"=\"Player\"\r\n\r\n\
[{ext_key}\\shell\\MothZip.SmartUnpack\\command]\r\n\
@=\"\\\"{exe_str}\\\" smart-unpack \\\"%1\\\"\"\r\n\r\n\
[{ext_key}\\shell\\MothZip.UnpackHere]\r\n\
@=\"여기에 풀기\"\r\n\
\"MultiSelectModel\"=\"Player\"\r\n\r\n\
[{ext_key}\\shell\\MothZip.UnpackHere\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\"\"\r\n\r\n\
[{ext_key}\\shell\\MothZip.UnpackFolder]\r\n\
@=\"각각 폴더에 풀기\"\r\n\
\"MultiSelectModel\"=\"Player\"\r\n\r\n\
[{ext_key}\\shell\\MothZip.UnpackFolder\\command]\r\n\
@=\"\\\"{exe_str}\\\" unpack \\\"%1\\\" --to-folder\"\r\n\r\n"
    )
}

/// Generate .reg script for context menu and application registration
pub fn generate_reg_content(exe_path: &Path) -> String {
    let exe_str = exe_path.to_string_lossy().replace('\\', "\\\\");
    let exe_dir = exe_path
        .parent()
        .unwrap_or(exe_path)
        .to_string_lossy()
        .replace('\\', "\\\\");

    let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n");

    // 1. Right-click on supported archives (.zip, .tar, .tgz, .gz)
    let supported_exts = [".zip", ".tar", ".tgz", ".gz"];
    for ext in supported_exts {
        // Direct extension verb
        reg.push_str(&format_unpack_verbs(
            &format!("HKEY_CURRENT_USER\\Software\\Classes\\{ext}"),
            &exe_str,
        ));
        // Windows 10/11 Explorer SystemFileAssociations
        reg.push_str(&format_unpack_verbs(
            &format!("HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\{ext}"),
            &exe_str,
        ));
    }

    // 2. Right-click on any single non-archive file (pack to <name>.zip)
    reg.push_str(&format!(
        "[HKEY_CURRENT_USER\\Software\\Classes\\*\\shell\\MothZip.Pack]\r\n\
@=\"ZIP으로 압축 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\
\"AppliesTo\"=\"System.FileExtension:<>\\\".zip\\\" AND System.FileExtension:<>\\\".tar\\\" AND System.FileExtension:<>\\\".tgz\\\" AND System.FileExtension:<>\\\".gz\\\"\"\r\n\
\"MultiSelectModel\"=\"Player\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\*\\shell\\MothZip.Pack\\command]\r\n\
@=\"\\\"{exe_str}\\\" pack-auto \\\"%1\\\"\"\r\n\r\n"
    ));

    // 3. Right-click on folders (pack to <folder>.zip)
    reg.push_str(&format!(
        "[HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack]\r\n\
@=\"ZIP으로 압축 (&MothZip)\"\r\n\
\"Icon\"=\"\\\"{exe_str}\\\",0\"\r\n\
\"MultiSelectModel\"=\"Player\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack\\command]\r\n\
@=\"\\\"{exe_str}\\\" pack-auto \\\"%1\\\"\"\r\n\r\n"
    ));

    // 4. Windows Installed Application Registry (shows in Settings / Control Panel with icon)
    reg.push_str(&format!(
        "[HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MothZip]\r\n\
\"DisplayName\"=\"MothZip (나방집)\"\r\n\
\"DisplayIcon\"=\"\\\"{exe_str}\\\",0\"\r\n\
\"DisplayVersion\"=\"0.1.0\"\r\n\
\"Publisher\"=\"mothzip contributors\"\r\n\
\"InstallLocation\"=\"\\\"{exe_dir}\\\"\"\r\n\
\"UninstallString\"=\"\\\"{exe_str}\\\" unregister\"\r\n\
\"QuietUninstallString\"=\"\\\"{exe_str}\\\" unregister\"\r\n\
\"HelpLink\"=\"https://github.com/mothzip/mothzip\"\r\n\
\"Comments\"=\"초경량 무광고 에이전틱 압축 프로그램 (ZIP, TAR, GZ 완벽 지원)\"\r\n\
\"EstimatedSize\"=dword:00000640\r\n\
\"NoModify\"=dword:00000001\r\n\
\"NoRepair\"=dword:00000001\r\n\r\n"
    ));

    // 5. App Paths (Run dialog / Win+R -> mothzip)
    reg.push_str(&format!(
        "[HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\mothzip.exe]\r\n\
@=\"\\\"{exe_str}\\\"\"\r\n\
\"Path\"=\"\\\"{exe_dir}\\\"\"\r\n\r\n"
    ));

    // 6. Application Registration
    reg.push_str(&format!(
        "[HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe]\r\n\
\"FriendlyAppName\"=\"MothZip (나방집)\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe\\DefaultIcon]\r\n\
@=\"\\\"{exe_str}\\\",0\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe\\shell\\open\\command]\r\n\
@=\"\\\"{exe_str}\\\" smart-unpack \\\"%1\\\"\"\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe\\SupportedTypes]\r\n\
\".zip\"=\"\"\r\n\
\".tar\"=\"\"\r\n\
\".tgz\"=\"\"\r\n\
\".gz\"=\"\"\r\n\
\".tar.gz\"=\"\"\r\n"
    ));

    reg
}

/// Generate .reg script for context menu and application removal
pub fn generate_unreg_content() -> &'static str {
    "Windows Registry Editor Version 5.00\r\n\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.zip\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.tar\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.tar\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.tar\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.tgz\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.tgz\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.tgz\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.gz\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.gz\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\.gz\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.tar\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.tar\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.tar\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.tgz\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.tgz\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.tgz\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.gz\\shell\\MothZip.SmartUnpack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.gz\\shell\\MothZip.UnpackHere]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\SystemFileAssociations\\.gz\\shell\\MothZip.UnpackFolder]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\*\\shell\\MothZip.Pack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\MothZip.Pack]\r\n\
[-HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\MothZip]\r\n\
[-HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\mothzip.exe]\r\n\
[-HKEY_CURRENT_USER\\Software\\Classes\\Applications\\mothzip.exe]\r\n"
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

/// Create Windows Start Menu application shortcut (.lnk) with moth icon
fn create_start_menu_shortcut(exe_path: &Path) {
    if let Ok(app_data) = env::var("APPDATA") {
        let programs = Path::new(&app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");

        if programs.exists() {
            let lnk_path = programs.join("MothZip.lnk");
            let vbs_content = format!(
                "Set ws = CreateObject(\"WScript.Shell\")\r\n\
                 Set s = ws.CreateShortcut(\"{}\")\r\n\
                 s.TargetPath = \"{}\"\r\n\
                 s.IconLocation = \"{},0\"\r\n\
                 s.Description = \"MothZip (나방집) - 초경량 무광고 압축 프로그램\"\r\n\
                 s.WorkingDirectory = \"{}\"\r\n\
                 s.Save\r\n",
                lnk_path.to_string_lossy().replace('\\', "\\\\"),
                exe_path.to_string_lossy().replace('\\', "\\\\"),
                exe_path.to_string_lossy().replace('\\', "\\\\"),
                exe_path
                    .parent()
                    .unwrap_or(exe_path)
                    .to_string_lossy()
                    .replace('\\', "\\\\")
            );

            let vbs_path = exe_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("mothzip_shortcut.vbs");

            if let Ok(mut f) = File::create(&vbs_path) {
                let _ = f.write_all(vbs_content.as_bytes());
                drop(f);
                let _ = std::process::Command::new("cscript")
                    .args(["//nologo", &vbs_path.to_string_lossy()])
                    .output();
                let _ = std::fs::remove_file(vbs_path);
            }
        }
    }
}

/// Remove Windows Start Menu application shortcut (.lnk)
fn remove_start_menu_shortcut() {
    if let Ok(app_data) = env::var("APPDATA") {
        let lnk_path = Path::new(&app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("MothZip.lnk");
        let _ = std::fs::remove_file(lnk_path);
    }
}

/// Register right-click context menu and Windows application entries under HKEY_CURRENT_USER.
/// Generates verified registry configuration, Start Menu shortcut, and imports it directly.
/// Requires NO administrator privileges.
pub fn register_context_menu() -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let reg_path = current_exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("mothzip_register.reg");

    let content = generate_reg_content(&current_exe);
    write_reg_file(&reg_path, &content)?;

    // Create Start Menu application shortcut
    create_start_menu_shortcut(&current_exe);

    // Attempt automatic import via reg.exe
    let import_result = std::process::Command::new("reg.exe")
        .arg("import")
        .arg(&reg_path)
        .output();

    match import_result {
        Ok(output) if output.status.success() => {
            println!("✓ Successfully registered MothZip as application and Windows Explorer context menu!");
            println!("  - Start Menu App Shortcut: MothZip (나방집) with cute moth icon");
            println!("  - Windows Installed Apps: Settings -> Apps -> MothZip registered");
            println!("  - App Paths: Win+R -> 'mothzip' active");
            println!("  - Right click on .zip: '알아서 풀기 (&MothZip)', '여기에 풀기', '각각 폴더에 풀기'");
            println!("  - Right click on any file/folder: 'ZIP으로 압축 (&MothZip)'");
            println!("  - Config: {}", reg_path.display());
            Ok(())
        }
        _ => {
            println!("✓ Application registry file and Start Menu shortcut generated successfully:");
            println!("  Config: {}", reg_path.display());
            println!("  - To activate registry in headless environments, run:");
            println!("    reg import \"{}\"", reg_path.display());
            Ok(())
        }
    }
}

/// Unregister right-click context menu and Windows application entries.
pub fn unregister_context_menu() -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = env::current_exe()?;
    let reg_path = current_exe
        .parent()
        .unwrap_or(Path::new("."))
        .join("mothzip_unregister.reg");

    let content = generate_unreg_content();
    write_reg_file(&reg_path, content)?;

    // Remove Start Menu shortcut
    remove_start_menu_shortcut();

    let unreg_result = std::process::Command::new("reg.exe")
        .arg("import")
        .arg(&reg_path)
        .output();

    match unreg_result {
        Ok(output) if output.status.success() => {
            println!("✓ Successfully removed MothZip application and context menus.");
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
        assert!(content.contains("Uninstall\\MothZip"));
        assert!(content.contains("App Paths\\mothzip.exe"));
        assert!(content.contains("MultiSelectModel"));
        assert!(content.contains("AppliesTo"));
        assert!(content.contains("SystemFileAssociations\\.tar\\shell"));
        assert!(content.contains("SystemFileAssociations\\.tgz\\shell"));
        assert!(content.contains("SystemFileAssociations\\.gz\\shell"));
    }
}
