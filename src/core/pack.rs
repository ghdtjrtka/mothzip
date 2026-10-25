use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Debug, Clone, serde::Serialize)]
pub struct PackSummary {
    pub archive_path: PathBuf,
    pub total_files: usize,
    pub total_uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub compression_ratio: String,
}

/// Howard Hinnant algorithm to convert civil days to (year, month, day)
fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = ((5 * doy + 2) / 153) as i64;
    let d = (doy - (153 * mp as u64 + 2) / 5 + 1) as u8;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u8;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as i32, m, d)
}

/// Convert std::time::SystemTime to zip::DateTime
pub fn system_time_to_zip_datetime(st: SystemTime) -> Option<zip::DateTime> {
    let dur = st.duration_since(UNIX_EPOCH).ok()?;
    let secs = dur.as_secs();
    let days = (secs / 86400) as i64;
    let (year, month, day) = civil_from_days(days);
    if !(1980..=2099).contains(&year) {
        return None;
    }
    let day_secs = (secs % 86400) as u32;
    let hour = (day_secs / 3600) as u8;
    let min = ((day_secs % 3600) / 60) as u8;
    let sec = (day_secs % 60) as u8;
    zip::DateTime::from_date_and_time(year as u16, month, day, hour, min, sec).ok()
}

/// Automatically determine the destination archive name for a single file or directory.
/// e.g. "report.docx" -> "report.zip"
///      "MyPhotos"    -> "MyPhotos.zip"
///      "data.zip"    -> "data_packed.zip"
pub fn determine_auto_pack_dest(source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("archive");
    let parent = source.parent().unwrap_or(Path::new("."));

    let candidate = parent.join(format!("{}.zip", stem));
    if candidate == source {
        parent.join(format!("{}_packed.zip", stem))
    } else {
        candidate
    }
}

/// Compress files or directories into a `.zip` archive.
/// Uses zero-RAM streaming (io::copy), transactional cleanup on failure,
/// pre-flight source validation, and recursive self-compression protection.
pub fn pack_archive(
    dest_zip: &Path,
    sources: &[PathBuf],
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    if sources.is_empty() {
        return Err("No source files or directories specified for packing".into());
    }

    // Pre-flight check: ensure all specified sources exist before creating archive
    for source in sources {
        if !source.exists() {
            return Err(format!("Source path '{}' does not exist", source.display()).into());
        }
    }

    // Guard against self-packing collision if destination already exists
    if dest_zip.exists() {
        if let Ok(cdest) = dest_zip.canonicalize() {
            for source in sources {
                if let Ok(csource) = source.canonicalize() {
                    if csource == cdest {
                        return Err(format!(
                            "Cannot pack archive '{}' into itself",
                            dest_zip.display()
                        )
                        .into());
                    }
                }
            }
        }
    }

    let parent = dest_zip.parent().unwrap_or(Path::new("."));
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
    }

    let file_name = dest_zip
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive.zip");
    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_zip = parent.join(format!(
        ".mothzip_pack_{}_{}_{}",
        std::process::id(),
        staging_token,
        file_name
    ));

    let result = (|| -> Result<PackSummary, Box<dyn std::error::Error>> {
        let file = File::create(&staging_zip)?;
        let canonical_dest = dest_zip.canonicalize().ok();
        let canonical_staging = staging_zip.canonicalize().ok();
        let mut zip = ZipWriter::new(file);

        let base_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o755);

        let mut total_files = 0;
        let mut total_uncompressed_bytes = 0u64;

        for source in sources {
            let source_str = source.to_string_lossy();
            let trimmed_str = source_str.trim_end_matches(['/', '\\']);
            let clean_source = if trimmed_str.is_empty() {
                source.as_path()
            } else {
                Path::new(trimmed_str)
            };

            if clean_source.is_file() {
                if let Some(ref cdest) = canonical_dest {
                    if let Ok(csource) = clean_source.canonicalize() {
                        if csource == *cdest {
                            continue;
                        }
                    }
                }
                if let Some(ref cstage) = canonical_staging {
                    if let Ok(csource) = clean_source.canonicalize() {
                        if csource == *cstage {
                            continue;
                        }
                    }
                }

                let file_name = clean_source
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");

                let mut opts = base_options;
                if let Ok(meta) = clean_source.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        if let Some(dt) = system_time_to_zip_datetime(mtime) {
                            opts = opts.last_modified_time(dt);
                        }
                    }
                }

                zip.start_file(file_name, opts)?;
                let mut f = File::open(clean_source).map_err(|e| {
                    format!(
                        "Failed to open file '{}' for packing: {}",
                        clean_source.display(),
                        e
                    )
                })?;
                let written = io::copy(&mut f, &mut zip)?;

                total_uncompressed_bytes += written;
                total_files += 1;
            } else if clean_source.is_dir() {
                let base_parent = clean_source.parent().unwrap_or(clean_source);

                for entry in WalkDir::new(clean_source).into_iter().filter_map(|e| e.ok()) {
                    let path = entry.path();

                    // Skip the destination archive itself if created inside the source directory
                    if let Some(ref cdest) = canonical_dest {
                        if let Ok(cpath) = path.canonicalize() {
                            if cpath == *cdest {
                                continue;
                            }
                        }
                    }
                    if let Some(ref cstage) = canonical_staging {
                        if let Ok(cpath) = path.canonicalize() {
                            if cpath == *cstage {
                                continue;
                            }
                        }
                    }

                    let relative = path.strip_prefix(base_parent)?;
                    let raw_name = relative.to_string_lossy().replace('\\', "/");
                    let name = raw_name.trim_start_matches("./").trim_start_matches('/');

                    if name.is_empty() {
                        continue;
                    }

                    let mut opts = base_options;
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            if let Some(dt) = system_time_to_zip_datetime(mtime) {
                                opts = opts.last_modified_time(dt);
                            }
                        }
                    }

                    if entry.file_type().is_dir() {
                        zip.add_directory(format!("{}/", name), opts)?;
                    } else if entry.file_type().is_file() {
                        zip.start_file(name, opts)?;
                        let mut f = File::open(path).map_err(|e| {
                            format!("Failed to open file '{}' for packing: {}", path.display(), e)
                        })?;
                        let written = io::copy(&mut f, &mut zip)?;

                        total_uncompressed_bytes += written;
                        total_files += 1;
                    }
                }
            }
        }

        let finished_file = zip.finish()?;
        let compressed_bytes = finished_file.metadata()?.len();
        drop(finished_file);

        if dest_zip.exists() {
            let _ = fs::remove_file(dest_zip);
        }
        fs::rename(&staging_zip, dest_zip)?;

        let ratio_num = if compressed_bytes > 0 {
            total_uncompressed_bytes as f64 / compressed_bytes as f64
        } else {
            1.0
        };
        let compression_ratio = format!("{:.1}x", ratio_num);

        Ok(PackSummary {
            archive_path: dest_zip.to_path_buf(),
            total_files,
            total_uncompressed_bytes,
            compressed_bytes,
            compression_ratio,
        })
    })();

    // Transactional safety: remove incomplete staging zip if packing failed
    if result.is_err() && staging_zip.exists() {
        let _ = fs::remove_file(&staging_zip);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_auto_pack_dest() {
        assert_eq!(
            determine_auto_pack_dest(Path::new("C:\\docs\\report.docx")),
            PathBuf::from("C:\\docs\\report.zip")
        );
        assert_eq!(
            determine_auto_pack_dest(Path::new("C:\\photos\\vacation")),
            PathBuf::from("C:\\photos\\vacation.zip")
        );
        assert_eq!(
            determine_auto_pack_dest(Path::new("C:\\archive\\data.zip")),
            PathBuf::from("C:\\archive\\data_packed.zip")
        );
    }

    #[test]
    fn test_time_conversion_roundtrip() {
        let now = SystemTime::now();
        let dt = system_time_to_zip_datetime(now);
        assert!(dt.is_some());
        let dt = dt.unwrap();
        assert!(dt.year() >= 2024);
    }

    #[test]
    fn test_pack_rejects_nonexistent_source() {
        let temp_dest = PathBuf::from("target/test_missing.zip");
        let nonexistent = PathBuf::from("target/definitely_does_not_exist_12345.txt");
        let res = pack_archive(&temp_dest, &[nonexistent]);
        assert!(res.is_err());
        assert!(!temp_dest.exists(), "Target file must not be created on error");
    }

    #[test]
    fn test_pack_rejects_empty_sources() {
        let temp_dest = PathBuf::from("target/test_empty.zip");
        let res = pack_archive(&temp_dest, &[]);
        assert!(res.is_err());
        assert!(!temp_dest.exists());
    }

    #[test]
    fn test_pack_with_trailing_slash_directory() {
        let temp_dir = PathBuf::from("target/test_trailing_dir");
        let _ = fs::create_dir_all(&temp_dir);
        let sample_file = temp_dir.join("sample.txt");
        fs::write(&sample_file, b"content").unwrap();

        let zip_dest = PathBuf::from("target/test_trailing_out.zip");
        let source_with_slash = PathBuf::from("target/test_trailing_dir/");
        let res = pack_archive(&zip_dest, &[source_with_slash]).unwrap();
        assert_eq!(res.total_files, 1);
        assert!(zip_dest.exists());

        let _ = fs::remove_file(zip_dest);
        let _ = fs::remove_dir_all(temp_dir);
    }
}
