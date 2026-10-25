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

#[derive(Debug, Clone)]
pub struct LayoutEntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub compressed_size: u64,
}

/// Unified layout evaluator for any archive format (Zip, Tar, TarGz).
/// - If all entries are wrapped in a single root directory, extracts directly to `output_base`.
/// - If multiple loose files or directories exist, creates `output_base/[archive_stem]`.
/// - Ignores macOS/Windows metadata (__MACOSX, .DS_Store, Thumbs.db, pax_global_header).
pub fn evaluate_layout_plan(
    entries: &[LayoutEntryInfo],
    archive_stem: &str,
    output_base: &Path,
) -> SmartUnpackPlan {
    let mut top_level_dirs = HashSet::new();
    let mut has_root_files = false;
    let mut total_uncompressed_bytes = 0u64;
    let mut total_compressed_bytes = 0u64;
    let total_entries = entries.len();

    for entry in entries {
        total_uncompressed_bytes += entry.size;
        total_compressed_bytes += entry.compressed_size;

        let raw_name = entry.name.replace('\\', "/");
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
            || trimmed.starts_with("._")
            || trimmed.ends_with("/._")
            || trimmed == "pax_global_header"
        {
            continue;
        }

        let is_dir = entry.is_dir || raw_name.ends_with('/');

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

    SmartUnpackPlan {
        target_dir,
        has_single_root,
        root_name,
        total_entries,
        total_uncompressed_bytes,
        total_compressed_bytes,
    }
}

/// Analyze ZIP archive contents and produce a smart unpack plan.
pub fn plan_smart_unpack<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    archive_stem: &str,
    output_base: &Path,
) -> Result<SmartUnpackPlan, Box<dyn std::error::Error>> {
    let total_entries = archive.len();
    let mut entries_info = Vec::with_capacity(total_entries);

    for i in 0..total_entries {
        let entry = archive.by_index(i)?;
        let raw_name = crate::core::encoding::decode_entry_name(entry.name_raw());
        let is_dir = entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');
        entries_info.push(LayoutEntryInfo {
            name: raw_name,
            is_dir,
            size: entry.size(),
            compressed_size: entry.compressed_size(),
        });
    }

    Ok(evaluate_layout_plan(&entries_info, archive_stem, output_base))
}

/// Analyze TAR archive stream contents and produce a smart unpack plan.
pub fn plan_smart_unpack_tar<R: Read>(
    mut tar: tar::Archive<R>,
    archive_stem: &str,
    output_base: &Path,
    archive_file_size: u64,
) -> Result<SmartUnpackPlan, Box<dyn std::error::Error>> {
    let mut entries_info = Vec::new();

    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let raw_name = path.to_string_lossy().to_string();
        let decoded = crate::core::encoding::compose_hangul_nfc(&raw_name);
        let is_dir = entry.header().entry_type().is_dir()
            || decoded.ends_with('/')
            || decoded.ends_with('\\');
        let size = entry.header().size()?;

        entries_info.push(LayoutEntryInfo {
            name: decoded,
            is_dir,
            size,
            compressed_size: size,
        });
    }

    let mut plan = evaluate_layout_plan(&entries_info, archive_stem, output_base);
    plan.total_compressed_bytes = archive_file_size;
    Ok(plan)
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
