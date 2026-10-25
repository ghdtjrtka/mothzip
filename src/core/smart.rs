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
///   extracting directly to `output_base` keeps everything grouped.
/// - If multiple files or directories exist at the root, extracting to
///   `output_base/[archive_stem]` prevents cluttering the destination directory.
pub fn plan_smart_unpack<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    archive_stem: &str,
    output_base: &Path,
) -> Result<SmartUnpackPlan, Box<dyn std::error::Error>> {
    let mut top_level_roots = HashSet::new();
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

        // Get the first path component
        let first_component = match trimmed.split_once('/') {
            Some((root, _)) => root,
            None => trimmed,
        };

        top_level_roots.insert(first_component.to_string());
    }

    let has_single_root = top_level_roots.len() == 1 && total_entries > 0;
    let root_name = if has_single_root {
        top_level_roots.iter().next().cloned()
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
