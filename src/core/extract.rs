use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zip::result::ZipError;
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

/// Convert Howard Hinnant civil calendar to unix epoch seconds
fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Convert zip::DateTime to std::time::SystemTime
pub fn zip_datetime_to_system_time(dt: zip::DateTime) -> Option<SystemTime> {
    let days = days_from_civil(dt.year() as i64, dt.month() as u64, dt.day() as u64);
    let secs = days * 86400
        + (dt.hour() as i64 * 3600)
        + (dt.minute() as i64 * 60)
        + (dt.second() as i64);
    if secs >= 0 {
        Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
    } else {
        None
    }
}

/// Safely extract an archive with Zip-Slip protection, smart unpack, timestamp preservation,
/// and password support.
pub fn extract_archive(
    archive_path: &Path,
    output_base: &Path,
    smart: bool,
    force_subfolder: bool,
    password: Option<&str>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let archive_stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");

    let (target_dir, smart_adjusted) = if force_subfolder {
        let total_uncompressed: u64 = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|e| e.size()))
            .sum();
        check_zip_bomb(total_uncompressed, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (output_base.join(archive_stem), true)
    } else if smart {
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
    let resolved_target = target_dir.canonicalize().unwrap_or_else(|_| target_dir.clone());

    let mut extracted_files = 0;
    let mut extracted_bytes = 0u64;

    for i in 0..archive.len() {
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
                Err(ZipError::UnsupportedArchive(msg)) if msg.contains("Password") || msg.contains("encrypted") => {
                    return Err(
                        "Archive entry is password-protected. Please supply --password <PASS>"
                            .into(),
                    );
                }
                Err(e) => return Err(Box::new(e)),
            },
        };

        let raw_name = super::encoding::decode_entry_name(entry.name_raw());

        // Skip macOS/Windows system junk files (__MACOSX, .DS_Store, Thumbs.db)
        let normalized = raw_name.replace('\\', "/");
        if normalized.starts_with("__MACOSX/")
            || normalized == "__MACOSX"
            || normalized.ends_with("/.DS_Store")
            || normalized == ".DS_Store"
            || normalized.ends_with("/Thumbs.db")
            || normalized == "Thumbs.db"
        {
            continue;
        }

        let dest_path = sanitize_entry_path(&resolved_target, &raw_name)?;

        // Defense against Symlink / Junction redirection
        if let Some(parent) = dest_path.parent() {
            if parent.exists() && parent != resolved_target {
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

        let is_dir_entry = entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');

        if is_dir_entry {
            if dest_path.is_file() {
                return Err(format!(
                    "Cannot extract directory '{}': an existing file with the same name already exists",
                    dest_path.display()
                )
                .into());
            }
            fs::create_dir_all(&dest_path)?;
        } else {
            if dest_path.is_dir() {
                return Err(format!(
                    "Cannot extract file '{}': an existing directory with the same name already exists",
                    dest_path.display()
                )
                .into());
            }

            if let Some(parent) = dest_path.parent() {
                if !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }

            // If an existing file is Read-Only, remove read-only attribute before overwriting
            if dest_path.exists() {
                if let Ok(meta) = fs::metadata(&dest_path) {
                    let mut perms = meta.permissions();
                    if perms.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        perms.set_readonly(false);
                        let _ = fs::set_permissions(&dest_path, perms);
                    }
                }
            }

            let mut outfile = File::create(&dest_path).map_err(|e| {
                format!(
                    "Failed to create destination file '{}': {}",
                    dest_path.display(),
                    e
                )
            })?;

            // Defend against deceptive stream zip-bombs and clean up on mid-stream failure
            let max_entry_bytes = entry.size() + 4096;
            let mut limited = (&mut entry).take(max_entry_bytes + 1);
            let written = match io::copy(&mut limited, &mut outfile) {
                Ok(w) => w,
                Err(e) => {
                    drop(outfile);
                    let _ = fs::remove_file(&dest_path);
                    return Err(format!("Extraction error on '{}': {}", raw_name, e).into());
                }
            };

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

            // Preserve file timestamp
            if let Some(dt) = entry.last_modified() {
                if let Some(mod_time) = zip_datetime_to_system_time(dt) {
                    let _ = outfile.set_times(fs::FileTimes::new().set_modified(mod_time));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    #[test]
    fn test_timestamp_preservation() {
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let dt = zip::DateTime::from_date_and_time(2023, 5, 17, 14, 30, 0).unwrap();
            let opts = SimpleFileOptions::default().last_modified_time(dt);
            writer.start_file("timed.txt", opts).unwrap();
            std::io::Write::write_all(&mut writer, b"timestamp check").unwrap();
            writer.finish().unwrap();
        }

        let temp_dir = PathBuf::from("target").join("test_tmp").join(format!(
            "mothzip_time_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();
        let zip_path = temp_dir.join("test_time.zip");
        fs::write(&zip_path, buf).unwrap();

        let res = extract_archive(&zip_path, &temp_dir, false, false, None).unwrap();
        assert_eq!(res.extracted_files, 1);

        let extracted_file = temp_dir.join("timed.txt");
        let meta = fs::metadata(&extracted_file).unwrap();
        let mtime = meta.modified().unwrap();
        let expected = zip_datetime_to_system_time(
            zip::DateTime::from_date_and_time(2023, 5, 17, 14, 30, 0).unwrap(),
        )
        .unwrap();

        let diff = if mtime > expected {
            mtime.duration_since(expected).unwrap()
        } else {
            expected.duration_since(mtime).unwrap()
        };
        // FAT timestamp precision is 2 seconds
        assert!(
            diff.as_secs() <= 2,
            "Timestamp must be preserved within 2s FAT precision"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
