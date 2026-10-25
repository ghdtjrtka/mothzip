use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

use super::guard::{check_zip_bomb, sanitize_entry_path, DEFAULT_MAX_UNCOMPRESSED_BYTES};
use super::smart::plan_smart_unpack;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractionSummary {
    pub extracted_files: usize,
    pub extracted_bytes: u64,
    pub target_dir: PathBuf,
    pub smart_adjusted: bool,
}

/// Safely extract an archive with Zip-Slip protection and optional Smart Unpack routing.
pub fn extract_archive(
    archive_path: &Path,
    output_base: &Path,
    smart: bool,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let archive_stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");

    let (target_dir, smart_adjusted) = if smart {
        let plan = plan_smart_unpack(&mut archive, archive_stem, output_base)?;
        check_zip_bomb(plan.total_uncompressed_bytes, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (plan.target_dir, !plan.has_single_root)
    } else {
        let total_uncompressed: u64 = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|e| e.size()))
            .sum();
        check_zip_bomb(total_uncompressed, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (output_base.to_path_buf(), false)
    };

    fs::create_dir_all(&target_dir)?;

    let mut extracted_files = 0;
    let mut extracted_bytes = 0u64;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw_name = super::encoding::decode_entry_name(entry.name_raw());

        let dest_path = sanitize_entry_path(&target_dir, &raw_name)?;

        // Defense against Symlink / Junction redirection
        if let Some(parent) = dest_path.parent() {
            if parent.exists() && parent != target_dir {
                if let Ok(meta) = fs::symlink_metadata(parent) {
                    if meta.file_type().is_symlink() {
                        return Err(format!(
                            "Security violation: parent path '{}' is a symlink",
                            parent.display()
                        )
                        .into());
                    }
                }
            }
        }

        if entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\') {
            fs::create_dir_all(&dest_path)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                if !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }

            let mut outfile = File::create(&dest_path)?;

            // Defend against deceptive stream zip-bombs:
            // Cap extraction by declared entry size + 4KB safety slack
            let max_entry_bytes = entry.size() + 4096;
            let mut limited = (&mut entry).take(max_entry_bytes + 1);
            let written = io::copy(&mut limited, &mut outfile)?;

            if written > max_entry_bytes {
                let _ = fs::remove_file(&dest_path);
                return Err(format!(
                    "Security violation: entry '{}' exceeded declared size (deceptive zip-bomb detected)",
                    raw_name
                )
                .into());
            }

            extracted_bytes += written;
            if extracted_bytes > DEFAULT_MAX_UNCOMPRESSED_BYTES {
                let _ = fs::remove_file(&dest_path);
                return Err(format!(
                    "Security violation: total extracted data exceeded global safety threshold ({} GB)",
                    DEFAULT_MAX_UNCOMPRESSED_BYTES / (1024 * 1024 * 1024)
                )
                .into());
            }

            extracted_files += 1;
        }
    }

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir,
        smart_adjusted,
    })
}
