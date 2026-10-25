use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zip::result::ZipError;
use zip::ZipArchive;

use super::format::{archive_stem_cleaned, detect_archive_format, ArchiveFormat};
use super::guard::{check_zip_bomb, sanitize_entry_path, DEFAULT_MAX_UNCOMPRESSED_BYTES};
use super::smart::{plan_smart_unpack, plan_smart_unpack_tar};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractionSummary {
    pub extracted_files: usize,
    pub extracted_bytes: u64,
    pub target_dir: PathBuf,
    pub smart_adjusted: bool,
}

struct StagedFile {
    temp_path: PathBuf,
    final_dest: PathBuf,
    mtime: Option<SystemTime>,
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

/// Verify that no ancestor directory of `dest_path` is a symlink or junction escaping `resolved_target`.
fn verify_safe_ancestors(dest_path: &Path, resolved_target: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut curr = dest_path.parent();
    while let Some(dir) = curr {
        if dir == resolved_target {
            break;
        }
        if !dir.starts_with(resolved_target) {
            return Err(format!(
                "Security violation: path '{}' escapes target directory",
                dest_path.display()
            )
            .into());
        }
        if dir.exists() {
            if let Ok(meta) = fs::symlink_metadata(dir) {
                if meta.file_type().is_symlink() {
                    return Err(format!(
                        "Security violation: directory '{}' is a symlink or junction",
                        dir.display()
                    )
                    .into());
                }
            }
            if let Ok(canon) = dir.canonicalize() {
                if !canon.starts_with(resolved_target) {
                    return Err(format!(
                        "Security violation: directory '{}' redirects outside target directory",
                        dir.display()
                    )
                    .into());
                }
            }
        }
        curr = dir.parent();
    }
    Ok(())
}

/// Disambiguate destination path if distinct archive entries normalize/sanitize to the same path.
fn disambiguate_dest_path(
    candidate: PathBuf,
    seen_destinations: &mut HashSet<PathBuf>,
) -> PathBuf {
    if !seen_destinations.contains(&candidate) {
        seen_destinations.insert(candidate.clone());
        return candidate;
    }

    let parent = candidate
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = candidate.extension().and_then(|e| e.to_str());

    let mut counter = 1;
    loop {
        let new_name = match ext {
            Some(e) => format!("{}_{}.{}", stem, counter, e),
            None => format!("{}_{}", stem, counter),
        };
        let new_path = parent.join(new_name);
        if !seen_destinations.contains(&new_path) {
            seen_destinations.insert(new_path.clone());
            return new_path;
        }
        counter += 1;
    }
}

/// Commit staged files to their final destinations and clean up staging directory.
fn commit_staged_extraction(
    dirs_to_create: Vec<PathBuf>,
    staged_files: Vec<StagedFile>,
    staging_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for dir in dirs_to_create {
        if dir.is_file() {
            let _ = fs::remove_file(&dir);
        }
        fs::create_dir_all(&dir)?;
    }

    for staged in staged_files {
        if let Some(parent) = staged.final_dest.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        // Break hardlinks or leaf symlinks without mutating outside targets
        if staged.final_dest.exists() || fs::symlink_metadata(&staged.final_dest).is_ok() {
            if let Ok(meta) = staged.final_dest.metadata() {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = fs::set_permissions(&staged.final_dest, perms);
                }
            }
            if staged.final_dest.is_dir() {
                let _ = fs::remove_dir_all(&staged.final_dest);
            } else {
                let _ = fs::remove_file(&staged.final_dest);
            }
        }

        fs::rename(&staged.temp_path, &staged.final_dest)?;

        if let Some(mod_time) = staged.mtime {
            if let Ok(f) = fs::OpenOptions::new().write(true).open(&staged.final_dest) {
                let _ = f.set_times(fs::FileTimes::new().set_modified(mod_time));
            }
        }
    }

    let _ = fs::remove_dir_all(staging_dir);
    Ok(())
}

/// Safely extract an archive (.zip, .tar.gz, .tgz, .tar, .gz) with Zip/Tar-Slip protection,
/// smart unpack, timestamp preservation, and password support.
pub fn extract_archive(
    archive_path: &Path,
    output_base: &Path,
    smart: bool,
    force_subfolder: bool,
    password: Option<&str>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;
    match format {
        ArchiveFormat::Zip => {
            extract_zip_archive(archive_path, output_base, smart, force_subfolder, password)
        }
        ArchiveFormat::TarGz => {
            extract_tar_archive(archive_path, output_base, true, smart, force_subfolder)
        }
        ArchiveFormat::Tar => {
            extract_tar_archive(archive_path, output_base, false, smart, force_subfolder)
        }
        ArchiveFormat::Gz => {
            extract_gz_archive(archive_path, output_base, smart, force_subfolder)
        }
    }
}

/// Extract a standard .zip archive using staged atomic transactions
fn extract_zip_archive(
    archive_path: &Path,
    output_base: &Path,
    smart: bool,
    force_subfolder: bool,
    password: Option<&str>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let archive_stem = archive_stem_cleaned(archive_path);

    let (target_dir, smart_adjusted) = if force_subfolder {
        let total_uncompressed: u64 = (0..archive.len())
            .filter_map(|i| match password {
                Some(pass) => archive.by_index_decrypt(i, pass.as_bytes()).ok().map(|e| e.size()),
                None => archive.by_index(i).ok().map(|e| e.size()),
            })
            .sum();
        check_zip_bomb(total_uncompressed, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (output_base.join(&archive_stem), true)
    } else if smart {
        let plan = plan_smart_unpack(&mut archive, &archive_stem, output_base, password)?;
        check_zip_bomb(plan.total_uncompressed_bytes, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (plan.target_dir, !plan.has_single_root)
    } else {
        let total_uncompressed: u64 = (0..archive.len())
            .filter_map(|i| match password {
                Some(pass) => archive.by_index_decrypt(i, pass.as_bytes()).ok().map(|e| e.size()),
                None => archive.by_index(i).ok().map(|e| e.size()),
            })
            .sum();
        check_zip_bomb(total_uncompressed, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (output_base.to_path_buf(), false)
    };

    let target_dir_existed_before = target_dir.exists();
    fs::create_dir_all(&target_dir)?;
    let resolved_target = target_dir.canonicalize().unwrap_or_else(|_| target_dir.clone());

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(".mothzip_stage_{}_{}", std::process::id(), staging_token));
    fs::create_dir_all(&staging_dir)?;

    let mut staged_files = Vec::new();
    let mut dirs_to_create = Vec::new();
    let mut seen_destinations = HashSet::new();
    let mut extracted_bytes = 0u64;
    let mut entry_index = 0usize;

    let stage_result: Result<(), Box<dyn std::error::Error>> = (|| -> Result<(), Box<dyn std::error::Error>> {
        for i in 0..archive.len() {
            let mut entry = match password {
                Some(pass) => match archive.by_index_decrypt(i, pass.as_bytes()) {
                    Ok(e) => e,
                    Err(ZipError::UnsupportedArchive(msg)) => {
                        return Err(format!("Decryption failed: {}", msg).into());
                    }
                    Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error>),
                },
                None => match archive.by_index(i) {
                    Ok(e) => e,
                    Err(ZipError::UnsupportedArchive(msg))
                        if msg.contains("Password") || msg.contains("encrypted") =>
                    {
                        return Err(
                            "Archive entry is password-protected. Please supply --password <PASS>"
                                .into(),
                        );
                    }
                    Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error>),
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
                || normalized.contains(".mothzip_stage_")
            {
                continue;
            }

            let raw_dest = sanitize_entry_path(&resolved_target, &raw_name)?;
            verify_safe_ancestors(&raw_dest, &resolved_target)?;

            let is_dir_entry = entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');

            if is_dir_entry {
                dirs_to_create.push(raw_dest);
            } else {
                let final_dest = disambiguate_dest_path(raw_dest, &mut seen_destinations);
                verify_safe_ancestors(&final_dest, &resolved_target)?;

                let stage_file_path = staging_dir.join(format!("{}.tmp", entry_index));
                entry_index += 1;

                let mut outfile = File::create(&stage_file_path)?;

                // Defend against deceptive stream zip-bombs and clean up on mid-stream failure
                let max_entry_bytes = entry.size() + 4096;
                let mut limited = (&mut entry).take(max_entry_bytes + 1);
                let written = io::copy(&mut limited, &mut outfile).map_err(|e| {
                    format!("Extraction error on '{}': {}", raw_name, e)
                })?;

                if written > max_entry_bytes {
                    return Err(format!(
                        "Security violation: entry '{}' exceeded declared size (deceptive zip-bomb detected)",
                        raw_name
                    )
                    .into());
                }

                extracted_bytes += written;
                if extracted_bytes > DEFAULT_MAX_UNCOMPRESSED_BYTES {
                    return Err(format!(
                        "Security violation: total extracted data exceeded global safety threshold ({} GB)",
                        DEFAULT_MAX_UNCOMPRESSED_BYTES / (1024 * 1024 * 1024)
                    )
                    .into());
                }

                let mtime = entry.last_modified().and_then(zip_datetime_to_system_time);

                staged_files.push(StagedFile {
                    temp_path: stage_file_path,
                    final_dest,
                    mtime,
                });
            }
        }
        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        if !target_dir_existed_before {
            let _ = fs::remove_dir(&target_dir);
        }
        return Err(e);
    }

    let extracted_files = staged_files.len();
    commit_staged_extraction(dirs_to_create, staged_files, &staging_dir)?;

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir,
        smart_adjusted,
    })
}

/// Extract a .tar or .tar.gz / .tgz archive with adversarial security guards and atomic transactions
fn extract_tar_archive(
    archive_path: &Path,
    output_base: &Path,
    is_gz: bool,
    smart: bool,
    force_subfolder: bool,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let archive_stem = archive_stem_cleaned(archive_path);
    let archive_file_size = fs::metadata(archive_path)?.len();

    let (target_dir, smart_adjusted) = if force_subfolder {
        (output_base.join(&archive_stem), true)
    } else if smart {
        let plan = if is_gz {
            let f = File::open(archive_path)?;
            let gz = flate2::read::GzDecoder::new(f);
            let tar = tar::Archive::new(gz);
            plan_smart_unpack_tar(tar, &archive_stem, output_base, archive_file_size)?
        } else {
            let f = File::open(archive_path)?;
            let tar = tar::Archive::new(f);
            plan_smart_unpack_tar(tar, &archive_stem, output_base, archive_file_size)?
        };
        check_zip_bomb(plan.total_uncompressed_bytes, DEFAULT_MAX_UNCOMPRESSED_BYTES)?;
        (plan.target_dir, !plan.has_single_root)
    } else {
        (output_base.to_path_buf(), false)
    };

    let target_dir_existed_before = target_dir.exists();
    fs::create_dir_all(&target_dir)?;
    let resolved_target = target_dir.canonicalize().unwrap_or_else(|_| target_dir.clone());

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(".mothzip_stage_{}_{}", std::process::id(), staging_token));
    fs::create_dir_all(&staging_dir)?;

    let file = File::open(archive_path)?;
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut tar = tar::Archive::new(reader);

    let mut staged_files = Vec::new();
    let mut dirs_to_create = Vec::new();
    let mut seen_destinations = HashSet::new();
    let mut extracted_bytes = 0u64;
    let mut entry_index = 0usize;

    let stage_result: Result<(), Box<dyn std::error::Error>> = (|| {
        for entry_res in tar.entries()? {
            let mut entry = entry_res?;
            let path_cow = entry.path()?;
            let raw_path = path_cow.to_string_lossy().to_string();
            let raw_name = crate::core::encoding::compose_hangul_nfc(&raw_path);

            // Skip macOS/Windows system junk files (__MACOSX, .DS_Store, Thumbs.db, pax_global_header)
            let normalized = raw_name.replace('\\', "/");
            if normalized.starts_with("__MACOSX/")
                || normalized == "__MACOSX"
                || normalized.ends_with("/.DS_Store")
                || normalized == ".DS_Store"
                || normalized.ends_with("/Thumbs.db")
                || normalized == "Thumbs.db"
                || normalized.starts_with("._")
                || normalized.contains("/._")
                || normalized == "pax_global_header"
                || normalized.contains(".mothzip_stage_")
            {
                continue;
            }

            let raw_dest = sanitize_entry_path(&resolved_target, &raw_name)?;
            verify_safe_ancestors(&raw_dest, &resolved_target)?;

            let entry_type = entry.header().entry_type();
            let is_dir_entry = entry_type.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');

            if is_dir_entry {
                dirs_to_create.push(raw_dest);
            } else if entry_type.is_file() {
                let final_dest = disambiguate_dest_path(raw_dest, &mut seen_destinations);
                verify_safe_ancestors(&final_dest, &resolved_target)?;

                let stage_file_path = staging_dir.join(format!("{}.tmp", entry_index));
                entry_index += 1;

                let mut outfile = File::create(&stage_file_path)?;

                let entry_size = entry.header().size()?;
                let max_entry_bytes = entry_size + 4096;
                let mut limited = (&mut entry).take(max_entry_bytes + 1);
                let written = io::copy(&mut limited, &mut outfile).map_err(|e| {
                    format!("Extraction error on '{}': {}", raw_name, e)
                })?;

                if written < entry_size {
                    return Err(format!(
                        "Corrupt archive: entry '{}' was truncated (expected {} bytes, read {})",
                        raw_name, entry_size, written
                    )
                    .into());
                }

                if written > max_entry_bytes {
                    return Err(format!(
                        "Security violation: entry '{}' exceeded declared size (deceptive tar-bomb detected)",
                        raw_name
                    )
                    .into());
                }

                extracted_bytes += written;
                if extracted_bytes > DEFAULT_MAX_UNCOMPRESSED_BYTES {
                    return Err(format!(
                        "Security violation: total extracted data exceeded global safety threshold ({} GB)",
                        DEFAULT_MAX_UNCOMPRESSED_BYTES / (1024 * 1024 * 1024)
                    )
                    .into());
                }

                let mtime = if let Ok(mt) = entry.header().mtime() {
                    if mt <= 253_402_300_799 {
                        UNIX_EPOCH.checked_add(Duration::from_secs(mt))
                    } else {
                        None
                    }
                } else {
                    None
                };

                staged_files.push(StagedFile {
                    temp_path: stage_file_path,
                    final_dest,
                    mtime,
                });
            }
        }

        // Drain trailing stream to verify gzip trailer integrity (CRC32 and ISIZE)
        let mut inner = tar.into_inner();
        let mut drain = io::sink();
        io::copy(&mut inner, &mut drain)?;

        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        if !target_dir_existed_before {
            let _ = fs::remove_dir(&target_dir);
        }
        return Err(e);
    }

    let extracted_files = staged_files.len();
    commit_staged_extraction(dirs_to_create, staged_files, &staging_dir)?;

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir,
        smart_adjusted,
    })
}

/// Extract a single .gz archive (plain gzip)
fn extract_gz_archive(
    archive_path: &Path,
    output_base: &Path,
    _smart: bool,
    force_subfolder: bool,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let archive_stem = archive_stem_cleaned(archive_path);
    let target_dir = if force_subfolder {
        output_base.join(&archive_stem)
    } else {
        output_base.to_path_buf()
    };

    let target_dir_existed_before = target_dir.exists();
    fs::create_dir_all(&target_dir)?;
    let resolved_target = target_dir.canonicalize().unwrap_or_else(|_| target_dir.clone());

    let final_dest = sanitize_entry_path(&resolved_target, &archive_stem)?;
    verify_safe_ancestors(&final_dest, &resolved_target)?;

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(".mothzip_stage_{}_{}", std::process::id(), staging_token));
    fs::create_dir_all(&staging_dir)?;
    let stage_file_path = staging_dir.join("0.tmp");

    let file = File::open(archive_path)?;
    let mut gz = flate2::read::GzDecoder::new(file);

    let stage_result: Result<u64, Box<dyn std::error::Error>> = (|| {
        let mut outfile = File::create(&stage_file_path)?;
        let mut limited = (&mut gz).take(DEFAULT_MAX_UNCOMPRESSED_BYTES + 1);
        let written = io::copy(&mut limited, &mut outfile)?;
        if written > DEFAULT_MAX_UNCOMPRESSED_BYTES {
            return Err("Security violation: total extracted data exceeded global safety threshold".into());
        }
        let mut drain = io::sink();
        io::copy(&mut gz, &mut drain)?;
        Ok(written)
    })();

    let extracted_bytes = match stage_result {
        Ok(b) => b,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging_dir);
            if !target_dir_existed_before {
                let _ = fs::remove_dir(&target_dir);
            }
            return Err(e);
        }
    };

    let staged_files = vec![StagedFile {
        temp_path: stage_file_path,
        final_dest,
        mtime: None,
    }];

    commit_staged_extraction(Vec::new(), staged_files, &staging_dir)?;

    Ok(ExtractionSummary {
        extracted_files: 1,
        extracted_bytes,
        target_dir,
        smart_adjusted: force_subfolder,
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

    #[test]
    fn test_tar_gz_extraction_and_smart_unpack() {
        let mut gz_buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::default());
            let mut tar_builder = tar::Builder::new(enc);

            let data1 = b"fn main() { println!(\"Hello TarGz\"); }";
            let mut header1 = tar::Header::new_gnu();
            header1.set_size(data1.len() as u64);
            header1.set_mode(0o644);
            header1.set_mtime(1700000000);
            header1.set_cksum();
            tar_builder
                .append_data(&mut header1, "my_crate/src/main.rs", &data1[..])
                .unwrap();

            let data2 = b"[package]\nname = \"my_crate\"";
            let mut header2 = tar::Header::new_gnu();
            header2.set_size(data2.len() as u64);
            header2.set_mode(0o644);
            header2.set_mtime(1700000000);
            header2.set_cksum();
            tar_builder
                .append_data(&mut header2, "my_crate/Cargo.toml", &data2[..])
                .unwrap();

            tar_builder.finish().unwrap();
        }

        let temp_dir = PathBuf::from("target/test_tar_tmp");
        let _ = fs::create_dir_all(&temp_dir);
        let targz_path = temp_dir.join("my_crate-1.0.tar.gz");
        fs::write(&targz_path, gz_buf).unwrap();

        // 1. Test smart-unpack (single root folder 'my_crate' -> extracts directly to temp_dir without double folder)
        let res = extract_archive(&targz_path, &temp_dir, true, false, None).unwrap();
        assert_eq!(res.extracted_files, 2);
        assert!(
            !res.smart_adjusted,
            "Single root folder should not create redundant wrapper"
        );
        assert!(temp_dir
            .join("my_crate")
            .join("src")
            .join("main.rs")
            .exists());
        assert_eq!(
            fs::read_to_string(temp_dir.join("my_crate").join("src").join("main.rs")).unwrap(),
            "fn main() { println!(\"Hello TarGz\"); }"
        );

        // 2. Test cat on the tar.gz
        let mut cat_out = Vec::new();
        crate::core::inspect::cat_archive_file(
            &targz_path,
            "my_crate/Cargo.toml",
            None,
            &mut cat_out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(cat_out).unwrap(),
            "[package]\nname = \"my_crate\""
        );

        // 3. Test peek on the tar.gz
        let peek = crate::core::inspect::peek_archive(&targz_path, 2, 50).unwrap();
        assert_eq!(peek.total_entries, 2);
        assert!(peek.has_single_root);
        assert_eq!(peek.root_name.as_deref(), Some("my_crate"));

        let _ = fs::remove_file(targz_path);
        let _ = fs::remove_dir_all(temp_dir);
    }
}
