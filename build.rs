#[path = "src/core/file_types.rs"]
mod file_types;

fn main() {
    #[cfg(windows)]
    {
        use std::{env, fs, path::PathBuf};
        let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let mut resources = vec![
            (1, root.join("assets/moth.ico")),
            (
                file_types::GENERIC_ARCHIVE_ICON_ID,
                root.join("assets/file-icons/archive.ico"),
            ),
        ];
        resources.extend(file_types::SUPPORTED_EXTENSIONS.iter().enumerate().map(
            |(index, extension)| {
                (
                    file_types::file_icon_resource_id(index),
                    root.join(format!(
                        "assets/file-icons/{}.ico",
                        extension.trim_start_matches('.')
                    )),
                )
            },
        ));
        let mut icon_resources = String::new();
        for (id, path) in resources {
            println!("cargo:rerun-if-changed={}", path.display());
            icon_resources.push_str(&format!(
                "{id} ICON \"{}\"\n",
                path.to_string_lossy().replace('\\', "\\\\")
            ));
        }
        let version = env::var("CARGO_PKG_VERSION").unwrap();
        let parts: Vec<_> = version.split('.').collect();
        let numeric = format!("{},{},{},0", parts[0], parts[1], parts[2]);
        let rc = format!(
            r#"#pragma code_page(65001)
{icon_resources}
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
