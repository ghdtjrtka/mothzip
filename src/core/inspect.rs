use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use zip::ZipArchive;

use super::smart::plan_smart_unpack;

#[derive(Debug, Clone, serde::Serialize)]
pub struct TreeSummaryItem {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PeekSummary {
    pub archive_name: String,
    pub total_entries: usize,
    pub total_compressed_bytes: u64,
    pub total_uncompressed_bytes: u64,
    pub compression_ratio: String,
    pub has_single_root: bool,
    pub root_name: Option<String>,
    pub extension_distribution: BTreeMap<String, usize>,
    pub tree_summary: Vec<TreeSummaryItem>,
    pub truncated_items: usize,
}

/// Fast inspection of an archive without extracting any files.
/// Tailored for AI Agents to understand archive layout within strict token budgets.
pub fn peek_archive(
    archive_path: &Path,
    depth_limit: usize,
    count_limit: usize,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let archive_stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("archive");

    let smart_plan = plan_smart_unpack(&mut archive, archive_stem, Path::new("."))?;

    let mut extension_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut tree_summary = Vec::new();
    let mut truncated_items = 0;

    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let name = super::encoding::decode_entry_name(entry.name_raw()).replace('\\', "/");
        let is_dir = entry.is_dir() || name.ends_with('/');
        let size = entry.size();

        // Track extension distribution for files
        if !is_dir {
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_lowercase()))
                .unwrap_or_else(|| "(no ext)".to_string());
            *extension_distribution.entry(ext).or_insert(0) += 1;
        }

        // Calculate depth
        let depth = name.trim_matches('/').split('/').count();
        if depth <= depth_limit {
            if tree_summary.len() < count_limit {
                tree_summary.push(TreeSummaryItem {
                    path: name,
                    is_dir,
                    size,
                });
            } else {
                truncated_items += 1;
            }
        } else {
            truncated_items += 1;
        }
    }

    let ratio_num = if smart_plan.total_compressed_bytes > 0 {
        smart_plan.total_uncompressed_bytes as f64 / smart_plan.total_compressed_bytes as f64
    } else {
        1.0
    };
    let compression_ratio = format!("{:.1}x", ratio_num);

    Ok(PeekSummary {
        archive_name: archive_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string(),
        total_entries: smart_plan.total_entries,
        total_compressed_bytes: smart_plan.total_compressed_bytes,
        total_uncompressed_bytes: smart_plan.total_uncompressed_bytes,
        compression_ratio,
        has_single_root: smart_plan.has_single_root,
        root_name: smart_plan.root_name,
        extension_distribution,
        tree_summary,
        truncated_items,
    })
}

/// Stream the contents of a single file from within the archive directly to `writer`.
/// Allows an AI Agent to inspect a specific file without decompressing the entire archive.
pub fn cat_archive_file<W: Write>(
    archive_path: &Path,
    target_entry_path: &str,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    let target_normalized = target_entry_path.replace('\\', "/");

    // Try exact match first
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name_normalized = super::encoding::decode_entry_name(entry.name_raw()).replace('\\', "/");

        if name_normalized == target_normalized
            || name_normalized.trim_start_matches('/') == target_normalized.trim_start_matches('/')
        {
            if entry.is_dir() {
                return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
            }
            std::io::copy(&mut entry, writer)?;
            return Ok(());
        }
    }

    // Try case-insensitive fallback
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name_normalized = super::encoding::decode_entry_name(entry.name_raw()).replace('\\', "/");

        if name_normalized.eq_ignore_ascii_case(&target_normalized) {
            if entry.is_dir() {
                return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
            }
            std::io::copy(&mut entry, writer)?;
            return Ok(());
        }
    }

    Err(format!(
        "File '{}' not found inside archive '{}'",
        target_entry_path,
        archive_path.display()
    )
    .into())
}
