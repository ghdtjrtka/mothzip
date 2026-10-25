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
/// Uses zero-RAM streaming (io::copy) and prevents recursive self-compression.
pub fn pack_archive(
    dest_zip: &Path,
    sources: &[PathBuf],
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    if let Some(parent) = dest_zip.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let file = File::create(dest_zip)?;
    let canonical_dest = dest_zip.canonicalize().ok();
    let mut zip = ZipWriter::new(file);

    let base_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let mut total_files = 0;
    let mut total_uncompressed_bytes = 0u64;

    for source in sources {
        if source.is_file() {
            if let Some(ref cdest) = canonical_dest {
                if let Ok(csource) = source.canonicalize() {
                    if csource == *cdest {
                        continue;
                    }
                }
            }

            let file_name = source
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");

            let mut opts = base_options;
            if let Ok(meta) = source.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if let Some(dt) = system_time_to_zip_datetime(mtime) {
                        opts = opts.last_modified_time(dt);
                    }
                }
            }

            zip.start_file(file_name, opts)?;
            let mut f = File::open(source)?;
            let written = io::copy(&mut f, &mut zip)?;

            total_uncompressed_bytes += written;
            total_files += 1;
        } else if source.is_dir() {
            let base_parent = source.parent().unwrap_or(source);

            for entry in WalkDir::new(source).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();

                // Skip the destination archive itself if created inside the source directory
                if let Some(ref cdest) = canonical_dest {
                    if let Ok(cpath) = path.canonicalize() {
                        if cpath == *cdest {
                            continue;
                        }
                    }
                }

                let relative = path.strip_prefix(base_parent)?;
                let name = relative.to_string_lossy().replace('\\', "/");

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
                    let mut f = File::open(path)?;
                    let written = io::copy(&mut f, &mut zip)?;

                    total_uncompressed_bytes += written;
                    total_files += 1;
                }
            }
        }
    }

    let finished_file = zip.finish()?;
    let compressed_bytes = finished_file.metadata()?.len();

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
}
