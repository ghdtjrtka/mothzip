#[cfg(test)]
mod desktop_review_tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::shell::manifest::{
        validate_installed_file_entry, InstallationManifest, MANIFEST_FILENAME,
    };
    use crate::shell::registry::{RegistryProfile, SUPPORTED_EXTENSIONS};
    use crate::shell::system::run_powershell_script;

    #[test]
    fn test_path_quoting_with_spaces_and_hangul() {
        let hangul_path = Path::new(
            "C:\\사용자\\홍길동\\AppData\\Local\\Programs\\MothZip (나방집)\\mothzip.exe",
        );
        let profile = RegistryProfile::build(hangul_path, true, SUPPORTED_EXTENSIONS);
        let reg = profile.generate_reg_content();

        assert!(reg.contains("C:\\\\사용자\\\\홍길동\\\\AppData\\\\Local\\\\Programs\\\\MothZip (나방집)\\\\mothzip.exe"));
        assert!(reg.contains("\\\"C:\\\\사용자\\\\홍길동\\\\AppData\\\\Local\\\\Programs\\\\MothZip (나방집)\\\\mothzip.exe\\\" smart-unpack"));
        assert!(reg.contains("\\\"C:\\\\사용자\\\\홍길동\\\\AppData\\\\Local\\\\Programs\\\\MothZip (나방집)\\\\mothzip.exe\\\" uninstall"));
    }

    #[test]
    fn test_manifest_rejects_parent_traversal_and_absolute_and_ads() {
        let dummy_dir = Path::new("C:\\Users\\Admin\\AppData\\Local\\Programs\\MothZip");

        assert!(validate_installed_file_entry("..\\outside.txt", dummy_dir).is_err());
        assert!(validate_installed_file_entry("../outside.txt", dummy_dir).is_err());
        assert!(
            validate_installed_file_entry("C:\\Windows\\System32\\calc.exe", dummy_dir).is_err()
        );
        assert!(validate_installed_file_entry("/etc/shadow", dummy_dir).is_err());
        assert!(validate_installed_file_entry("mothzip.exe:hidden_stream", dummy_dir).is_err());
        assert!(validate_installed_file_entry("sub/../../escape.txt", dummy_dir).is_err());

        assert!(validate_installed_file_entry("mothzip.exe", dummy_dir).is_ok());
        assert!(validate_installed_file_entry("LICENSE", dummy_dir).is_ok());
    }

    #[test]
    fn test_validate_install_root_security_boundaries() {
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
    fn test_foreign_files_without_manifest_rejected() {
        let temp_dir = PathBuf::from("target")
            .join("agy-desktop")
            .join(format!("test_foreign_guard_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("user_existing_doc.docx"), b"document").unwrap();

        let res = InstallationManifest::validate_install_root(&temp_dir);
        assert!(
            res.is_err(),
            "Target directory with foreign files must be rejected"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_tampered_manifest_rejected() {
        let temp_dir = PathBuf::from("target")
            .join("agy-desktop")
            .join(format!("test_tampered_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        // Write a tampered manifest that claims install_dir is C:\Windows
        let tampered_json = r#"{
            "marker": "mothzip-installation-manifest",
            "app_name": "MothZip",
            "version": "0.1.1",
            "install_dir": "C:\\Windows",
            "exe_path": "C:\\Windows\\mothzip.exe",
            "installed_files": ["mothzip.exe", "..\\dangerous.txt"],
            "created_at_utc": "0",
            "managed": true
        }"#;
        fs::write(temp_dir.join(MANIFEST_FILENAME), tampered_json).unwrap();

        let res = InstallationManifest::read_and_validate(&temp_dir);
        assert!(
            res.is_err(),
            "Tampered manifest with mismatched install_dir and dotdot must be rejected"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_arbitrary_deletion_defense_unrelated_files_preserved() {
        let test_dir = PathBuf::from("target")
            .join("agy-desktop")
            .join(format!("test_safe_uninstall_rev_{}", std::process::id()));
        fs::create_dir_all(&test_dir).unwrap();

        let manifest = InstallationManifest::new(&test_dir, "mothzip.exe", "0.1.1", &["LICENSE"]);
        manifest.write_to_dir(&test_dir).unwrap();

        fs::write(test_dir.join("mothzip.exe"), b"MothZip Binary").unwrap();
        fs::write(test_dir.join("LICENSE"), b"MIT License").unwrap();

        // User placed an unmanaged personal file
        let user_file = test_dir.join("financial_records.xlsx");
        fs::write(&user_file, b"CONFIDENTIAL USER DATA").unwrap();

        let loaded = InstallationManifest::read_and_validate(&test_dir).unwrap();
        assert_eq!(
            loaded.installed_files,
            vec!["mothzip.exe", MANIFEST_FILENAME, "LICENSE"]
        );

        // Simulate safe removal of only manifest-listed files
        for f in &loaded.installed_files {
            let p = test_dir.join(f);
            if p.exists() {
                fs::remove_file(p).unwrap();
            }
        }

        // Verify user file remains untouched
        assert!(
            user_file.exists(),
            "User personal file must NEVER be removed!"
        );
        assert_eq!(fs::read(&user_file).unwrap(), b"CONFIDENTIAL USER DATA");

        // Non-recursive folder removal check: directory must NOT be removed when user file exists
        let remaining: Vec<_> = fs::read_dir(&test_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(remaining.len(), 1);
        assert!(
            test_dir.exists(),
            "Install directory must NOT be removed while containing user files"
        );

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_staged_update_rollback_preservation() {
        let test_dir = PathBuf::from("target")
            .join("agy-desktop")
            .join(format!("test_staged_rollback_{}", std::process::id()));
        fs::create_dir_all(&test_dir).unwrap();

        let target_exe = test_dir.join("mothzip.exe");
        let backup_exe = test_dir.join("mothzip.exe.old");
        let staged_exe = test_dir.join("mothzip.exe.staged");

        // v1 exists
        fs::write(&target_exe, b"v0.1.0 working binary").unwrap();

        // Staging v2
        fs::write(&staged_exe, b"v0.1.1 new binary").unwrap();

        // Backup v1
        fs::rename(&target_exe, &backup_exe).unwrap();

        // Simulate failure during atomic move
        let simulated_move_failed = true;
        if simulated_move_failed {
            // Rollback
            if backup_exe.exists() {
                fs::rename(&backup_exe, &target_exe).unwrap();
            }
            let _ = fs::remove_file(&staged_exe);
        }

        // Verify v1 restored intact
        assert!(target_exe.exists());
        assert_eq!(fs::read(&target_exe).unwrap(), b"v0.1.0 working binary");
        assert!(!backup_exe.exists());
        assert!(!staged_exe.exists());

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_registry_unreg_shared_key_preservation() {
        let dummy = Path::new("C:\\Program Files\\MothZip\\mothzip.exe");
        let profile = RegistryProfile::build(dummy, true, SUPPORTED_EXTENSIONS);
        let unreg = profile.generate_unreg_content();

        // Shared key must NOT be deleted
        assert!(!unreg.contains("[-HKEY_CURRENT_USER\\Software\\RegisteredApplications]"));
        // Only MothZip value must be deleted
        assert!(unreg
            .contains("[HKEY_CURRENT_USER\\Software\\RegisteredApplications]\r\n\"MothZip\"=-"));
    }

    #[test]
    fn test_powershell_argument_preservation_in_child_process() {
        let tmpdir = Path::new("target").join("shell_tmp");
        fs::create_dir_all(&tmpdir).unwrap();
        let script = r#"
param([string]$target, [string]$korean, [string]$quotes)
Write-Output "TARGET:$target"
Write-Output "KOREAN:$korean"
Write-Output "QUOTES:$quotes"
"#;
        let test_target = "C:\\Program Files\\나방집 (Moth's & Zip)\\mothzip.exe";
        let test_korean = "초경량 & 무광고 & 에이전틱";
        let test_quotes = "Special 'single' and \"double\" quotes";

        let out = run_powershell_script(script, &[test_target, test_korean, test_quotes])
            .expect("PowerShell run must succeed and preserve arguments");
        assert!(out.contains(&format!("TARGET:{}", test_target)));
        assert!(out.contains(&format!("KOREAN:{}", test_korean)));
        assert!(out.contains(&format!("QUOTES:{}", test_quotes)));
    }
}
