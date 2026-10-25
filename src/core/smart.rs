use std::collections::HashSet;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SmartUnpackPlan {
    /// The resolved destination directory where files will be written
    pub target_dir: PathBuf,
    /// Whether the archive cleanly wraps all files inside a single root directory
    pub has_single_root: bool,
    /// Name of the single root directory, if present
    pub root_name: Option<String>,
    /// Total number of entries in the archive
    pub total_entries: usize,
    /// Total uncompressed bytes across all entries
    pub total_uncompressed_bytes: u64,
    /// Total compressed bytes
    pub total_compressed_bytes: u64,
}

/// Analyze archive contents and produce a smart unpack plan.
/// - If all entries are contained inside a single top-level directory,
///   extracting directly to `output_base` keeps everything cleanly grouped.
/// - If multiple files or directories exist at the root, or loose files exist at root,
///   extracting to `output_base/[archive_stem]` prevents cluttering the destination directory.
/// - Common metadata entries (__MACOSX, .DS_Store, Thumbs.db) are ignored when evaluating layout.
pub fn plan_smart_unpack<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    archive_stem: &str,
    output_base: &Path,
) -> Result<SmartUnpackPlan, Box<dyn std::error::Error>> {
    let mut top_level_dirs = HashSet::new();
    let mut has_root_files = false;
    let mut total_uncompressed_bytes = 0u64;
    let mut total_compressed_bytes = 0u64;
    let total_entries = archive.len();

    for i in 0..total_entries {
        let entry = archive.by_index(i)?;
        total_uncompressed_bytes += entry.size();
        total_compressed_bytes += entry.compressed_size();

        let raw_name = crate::core::encoding::decode_entry_name(entry.name_raw()).replace('\\', "/");
        let trimmed = raw_name.trim_matches('/');
        if trimmed.is_empty() {
            continue;
        }

        // Filter out macOS/Windows system metadata from layout calculation
        if trimmed.starts_with("__MACOSX")
            || trimmed == ".DS_Store"
            || trimmed.ends_with("/.DS_Store")
            || trimmed == "Thumbs.db"
            || trimmed.ends_with("/Thumbs.db")
        {
            continue;
        }

        let is_dir = entry.is_dir() || raw_name.ends_with('/');

        if let Some((root_dir, _)) = trimmed.split_once('/') {
            top_level_dirs.insert(root_dir.to_string());
        } else if is_dir {
            top_level_dirs.insert(trimmed.to_string());
        } else {
            has_root_files = true;
        }
    }

    let has_single_root = !has_root_files && top_level_dirs.len() == 1;
    let root_name = if has_single_root {
        top_level_dirs.into_iter().next()
    } else {
        None
    };

    let target_dir = if has_single_root {
        output_base.to_path_buf()
    } else {
        output_base.join(archive_stem)
    };

    Ok(SmartUnpackPlan {
        target_dir,
        has_single_root,
        root_name,
        total_entries,
        total_uncompressed_bytes,
        total_compressed_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    #[test]
    fn test_single_folder_smart_unpack() {
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default();
            writer.start_file("my_project/src/main.rs", opts).unwrap();
            writer.start_file("my_project/Cargo.toml", opts).unwrap();
            writer.start_file("__MACOSX/._main.rs", opts).unwrap();
            writer.finish().unwrap();
        }

        let mut archive = ZipArchive::new(Cursor::new(buf)).unwrap();
        let plan = plan_smart_unpack(&mut archive, "my_archive", Path::new("C:\\out")).unwrap();
        assert!(plan.has_single_root);
        assert_eq!(plan.root_name.as_deref(), Some("my_project"));
        assert_eq!(plan.target_dir, Path::new("C:\\out"));
    }

    #[test]
    fn test_flat_files_smart_unpack() {
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default();
            writer.start_file("file1.txt", opts).unwrap();
            writer.start_file("file2.txt", opts).unwrap();
            writer.finish().unwrap();
        }

        let mut archive = ZipArchive::new(Cursor::new(buf)).unwrap();
        let plan = plan_smart_unpack(&mut archive, "my_archive", Path::new("C:\\out")).unwrap();
        assert!(!plan.has_single_root);
        assert_eq!(plan.target_dir, Path::new("C:\\out").join("my_archive"));
    }

    #[test]
    fn test_single_root_file_creates_wrapper() {
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default();
            writer.start_file("single_loose.txt", opts).unwrap();
            writer.finish().unwrap();
        }

        let mut archive = ZipArchive::new(Cursor::new(buf)).unwrap();
        let plan = plan_smart_unpack(&mut archive, "doc", Path::new("C:\\out")).unwrap();
        assert!(!plan.has_single_root);
        assert_eq!(plan.target_dir, Path::new("C:\\out").join("doc"));
    }
}
