use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use zip::result::ZipError;
use zip::ZipArchive;

use super::format::{archive_stem_cleaned, detect_archive_format, ArchiveFormat};
use super::smart::{plan_smart_unpack, plan_smart_unpack_tar};

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

/// Fast inspection of an archive (.zip, .tar.gz, .tgz, .tar) without extracting any files.
/// Tailored for AI Agents to understand archive layout within strict token budgets.
pub fn peek_archive(
    archive_path: &Path,
    depth_limit: usize,
    count_limit: usize,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;
    match format {
        ArchiveFormat::Zip => peek_zip_archive(archive_path, depth_limit, count_limit),
        ArchiveFormat::TarGz => peek_tar_archive(archive_path, true, depth_limit, count_limit),
        ArchiveFormat::Tar => peek_tar_archive(archive_path, false, depth_limit, count_limit),
    }
}

fn peek_zip_archive(
    archive_path: &Path,
    depth_limit: usize,
    count_limit: usize,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let archive_stem = archive_stem_cleaned(archive_path);

    let smart_plan = plan_smart_unpack(&mut archive, &archive_stem, Path::new("."))?;

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

fn peek_tar_archive(
    archive_path: &Path,
    is_gz: bool,
    depth_limit: usize,
    count_limit: usize,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    let archive_stem = archive_stem_cleaned(archive_path);
    let archive_file_size = fs::metadata(archive_path)?.len();

    let file = File::open(archive_path)?;
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let tar = tar::Archive::new(reader);
    let smart_plan = plan_smart_unpack_tar(tar, &archive_stem, Path::new("."), archive_file_size)?;

    // Scan for tree summary and extension distribution
    let file2 = File::open(archive_path)?;
    let reader2: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file2))
    } else {
        Box::new(file2)
    };
    let mut tar2 = tar::Archive::new(reader2);

    let mut extension_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut tree_summary = Vec::new();
    let mut truncated_items = 0;

    for entry_res in tar2.entries()? {
        let entry = entry_res?;
        let path_cow = entry.path()?;
        let raw_name = path_cow.to_string_lossy().to_string();
        let name = crate::core::encoding::compose_hangul_nfc(&raw_name).replace('\\', "/");
        let is_dir = entry.header().entry_type().is_dir() || name.ends_with('/');
        let size = entry.header().size()?;

        if !is_dir {
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_lowercase()))
                .unwrap_or_else(|| "(no ext)".to_string());
            *extension_distribution.entry(ext).or_insert(0) += 1;
        }

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
    password: Option<&str>,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;
    match format {
        ArchiveFormat::Zip => cat_zip_file(archive_path, target_entry_path, password, writer),
        ArchiveFormat::TarGz => cat_tar_file(archive_path, true, target_entry_path, writer),
        ArchiveFormat::Tar => cat_tar_file(archive_path, false, target_entry_path, writer),
    }
}

fn cat_zip_file<W: Write>(
    archive_path: &Path,
    target_entry_path: &str,
    password: Option<&str>,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    let target_normalized = target_entry_path.replace('\\', "/");

    for i in 0..archive.len() {
        let name_normalized = {
            let entry = archive.by_index(i)?;
            super::encoding::decode_entry_name(entry.name_raw()).replace('\\', "/")
        };

        if name_normalized == target_normalized
            || name_normalized.trim_start_matches('/') == target_normalized.trim_start_matches('/')
            || name_normalized.eq_ignore_ascii_case(&target_normalized)
        {
            let mut entry = match password {
                Some(pass) => match archive.by_index_decrypt(i, pass.as_bytes()) {
                    Ok(e) => e,
                    Err(ZipError::UnsupportedArchive(msg)) => {
                        return Err(format!("Decryption failed: {}", msg).into());
                    }
                    Err(e) => return Err(Box::new(e)),
                },
                None => match archive.by_index(i) {
                    Ok(e) => e,
                    Err(ZipError::UnsupportedArchive(msg))
                        if msg.contains("Password") || msg.contains("encrypted") =>
                    {
                        return Err(
                            "File is password-protected. Please supply --password <PASS>".into(),
                        );
                    }
                    Err(e) => return Err(Box::new(e)),
                },
            };

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

fn cat_tar_file<W: Write>(
    archive_path: &Path,
    is_gz: bool,
    target_entry_path: &str,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut tar = tar::Archive::new(reader);
    let target_normalized = target_entry_path.replace('\\', "/");

    for entry_res in tar.entries()? {
        let mut entry = entry_res?;
        let path_cow = entry.path()?;
        let raw_name = path_cow.to_string_lossy().to_string();
        let name_normalized =
            crate::core::encoding::compose_hangul_nfc(&raw_name).replace('\\', "/");

        if name_normalized == target_normalized
            || name_normalized.trim_start_matches('/') == target_normalized.trim_start_matches('/')
            || name_normalized.eq_ignore_ascii_case(&target_normalized)
        {
            if entry.header().entry_type().is_dir() {
                return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
            }

            io::copy(&mut entry, writer)?;
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
