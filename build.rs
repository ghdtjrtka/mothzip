fn main() {
    #[cfg(windows)]
    {
        use std::{env, fs, path::PathBuf};
        let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let icon = root.join("assets/moth.ico");
        println!("cargo:rerun-if-changed=assets/moth.ico");
        let version = env::var("CARGO_PKG_VERSION").unwrap();
        let parts: Vec<_> = version.split('.').collect();
        let numeric = format!("{},{},{},0", parts[0], parts[1], parts[2]);
        let icon_path = icon.to_string_lossy().replace('\\', "\\\\");
        let rc = format!(
            r#"#pragma code_page(65001)
1 ICON "{icon_path}"
1 VERSIONINFO
 FILEVERSION {numeric}
 PRODUCTVERSION {numeric}
 FILEFLAGSMASK 0x3fL
 FILEFLAGS 0
 FILEOS 0x40004L
 FILETYPE 1
 FILESUBTYPE 0
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "mothzip contributors"
   VALUE "FileDescription", "MothZip (나방집)"
   VALUE "FileVersion", "{version}"
   VALUE "InternalName", "mothzip"
   VALUE "OriginalFilename", "mothzip.exe"
   VALUE "ProductName", "MothZip"
   VALUE "ProductVersion", "{version}"
   VALUE "LegalCopyright", "Copyright (c) 2026 mothzip contributors"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0409, 1200
 END
END
"#
        );
        let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("mothzip.rc");
        fs::write(&path, rc).unwrap();
        embed_resource::compile(path, embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
