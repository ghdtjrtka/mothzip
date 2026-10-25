use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zip::result::ZipError;
use zip::ZipArchive;

use super::codec::StreamCodec;
use super::format::{archive_stem_cleaned, detect_archive_format, ArchiveFormat};
use super::guard::{check_zip_bomb, copy_archive_entry, sanitize_entry_path};
use super::smart::{plan_smart_unpack, plan_smart_unpack_tar};

#[path = "selection.rs"]
mod selection;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractionSummary {
    pub extracted_files: usize,
    pub extracted_bytes: u64,
    pub target_dir: PathBuf,
    pub smart_adjusted: bool,
}

/// Options controlling an extraction: layout policy, password, byte budget,
/// literal member selection, and flat (leaf-only) output.
#[derive(Debug, Clone)]
pub struct ExtractionOptions {
    pub smart: bool,
    pub to_folder: bool,
    pub password: Option<String>,
    pub max_bytes: u64,
    /// Literal member paths (raw identity or a directory prefix). Empty extracts everything.
    pub members: Vec<String>,
    /// Write only leaf filenames into the target directory, skipping directories.
    pub flat: bool,
}

impl Default for ExtractionOptions {
    fn default() -> Self {
        Self {
            smart: false,
            to_folder: false,
            password: None,
            max_bytes: super::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            members: Vec::new(),
            flat: false,
        }
    }
}

/// Last path component of a sanitized destination, used by flat extraction.
fn leaf_component(path: &Path) -> io::Result<PathBuf> {
    path.file_name().map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Archive member has no leaf filename",
        )
    })
}

struct StagedFile {
    temp_path: PathBuf,
    final_dest: PathBuf,
    mtime: Option<SystemTime>,
}

pub use super::format::zip_datetime_to_system_time;

/// Resolve and strictly verify the target extraction directory against the caller's base output directory.
/// Guards against junction/symlink redirection where target_dir points outside output_base.
fn resolve_and_verify_target_dir(
    output_base: &Path,
    target_dir: &Path,
) -> Result<(PathBuf, bool), Box<dyn std::error::Error>> {
    if !output_base.exists() {
        fs::create_dir_all(output_base)?;
    }
    let resolved_base = output_base.canonicalize()?;

    let target_dir_existed_before = target_dir.exists();
    if target_dir_existed_before {
        if let Ok(meta) = fs::symlink_metadata(target_dir) {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "Security violation: destination directory '{}' is a junction or symlink",
                    target_dir.display()
                )
                .into());
            }
        }
    }

    fs::create_dir_all(target_dir)?;

    let resolved_target = target_dir.canonicalize()?;

    if !resolved_target.starts_with(&resolved_base) {
        return Err(format!(
            "Security violation: destination directory '{}' resolves outside base directory '{}'",
            target_dir.display(),
            output_base.display()
        )
        .into());
    }

    Ok((resolved_target, target_dir_existed_before))
}

/// Verify that no ancestor directory of `dest_path` is a symlink or junction escaping `resolved_target`.
fn verify_safe_ancestors(
    dest_path: &Path,
    resolved_target: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
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

#[cfg(windows)]
fn path_collision_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

#[cfg(not(windows))]
fn path_collision_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Disambiguate destination path if distinct archive entries normalize/sanitize to the same path.
/// Respects case-insensitivity on Windows filesystem.
fn disambiguate_dest_path(candidate: PathBuf, seen_destinations: &mut HashSet<String>) -> PathBuf {
    let key = path_collision_key(&candidate);
    if !seen_destinations.contains(&key) {
        seen_destinations.insert(key);
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
        let new_key = path_collision_key(&new_path);
        if !seen_destinations.contains(&new_key) {
            seen_destinations.insert(new_key);
            return new_path;
        }
        counter += 1;
    }
}

/// Validate the entire resulting tree before committing any user-visible file.
/// Runtime commit failures restore every preceding replacement via the shared journal.
fn commit_staged_extraction(
    dirs_to_create: Vec<PathBuf>,
    staged_files: Vec<StagedFile>,
    staging_dir: &Path,
    target_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut created_dirs = Vec::new();
    let mut recovery_required = false;
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        let file_keys: HashSet<_> = staged_files
            .iter()
            .map(|file| path_collision_key(&file.final_dest))
            .collect();
        let mut directories: std::collections::BTreeSet<PathBuf> =
            dirs_to_create.into_iter().collect();
        for file in &staged_files {
            let mut current = file.final_dest.parent();
            while let Some(parent) = current {
                if parent == target_root {
                    break;
                }
                if !parent.starts_with(target_root) {
                    return Err(format!(
                        "Destination escapes extraction root: {}",
                        parent.display()
                    )
                    .into());
                }
                directories.insert(parent.to_path_buf());
                current = parent.parent();
            }
        }
        // Include the ancestors of explicit empty directories too.
        for dir in directories.clone() {
            let mut current = dir.parent();
            while let Some(parent) = current {
                if parent == target_root {
                    break;
                }
                if !parent.starts_with(target_root) {
                    return Err(
                        format!("Directory escapes extraction root: {}", parent.display()).into(),
                    );
                }
                directories.insert(parent.to_path_buf());
                current = parent.parent();
            }
        }
        for dir in &directories {
            if file_keys.contains(&path_collision_key(dir)) {
                return Err(format!(
                    "Archive uses '{}' as both a file and a directory",
                    dir.display()
                )
                .into());
            }
            verify_safe_ancestors(dir, target_root)?;
            match fs::symlink_metadata(dir) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(format!(
                        "Destination directory is a file or link: {}",
                        dir.display()
                    )
                    .into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        for file in &staged_files {
            verify_safe_ancestors(&file.final_dest, target_root)?;
            match fs::symlink_metadata(&file.final_dest) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(format!(
                        "Destination file is a directory: {}",
                        file.final_dest.display()
                    )
                    .into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            // Set times on the owned staged object, never reopen a possibly redirected destination.
            let staged = fs::OpenOptions::new().write(true).open(&file.temp_path)?;
            if let Some(mtime) = file.mtime {
                staged.set_times(fs::FileTimes::new().set_modified(mtime))?;
            }
            staged.sync_all()?;
        }
        let mut ordered: Vec<_> = directories.into_iter().collect();
        ordered.sort_by_key(|dir| dir.components().count());
        for dir in ordered {
            if !dir.exists() {
                fs::create_dir(&dir)?;
                created_dirs.push(dir);
            }
        }
        let replacements: Vec<_> = staged_files
            .iter()
            .map(|file| (file.temp_path.clone(), file.final_dest.clone()))
            .collect();
        if let Err(e) = super::commit::commit_files(&replacements) {
            recovery_required = e.recovery_required;
            return Err(e.into());
        }
        Ok(())
    })();
    if !recovery_required {
        if result.is_err() {
            for dir in created_dirs.iter().rev() {
                let _ = fs::remove_dir(dir);
            }
        }
        if let Err(e) = fs::remove_dir_all(staging_dir) {
            let state = match &result {
                Ok(()) => "Files committed".to_string(),
                Err(cause) => cause.to_string(),
            };
            return Err(format!(
                "{}; staging retained at '{}': {}",
                state,
                staging_dir.display(),
                e
            )
            .into());
        }
    }
    result
}

/// Safely extract an archive (.zip, .tar.gz, .tgz, .tar, .gz) with Zip/Tar-Slip protection,
/// smart unpack, timestamp preservation, and password support.
/// Extract an archive with the legacy flat option signature.
/// Options is a thin wrapper over [`extract_archive_with_options`] that selects
/// all members and writes them in their original layout.
pub fn extract_archive(
    archive_path: &Path,
    output_base: &Path,
    smart: bool,
    force_subfolder: bool,
    password: Option<&str>,
    max_bytes: u64,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    extract_archive_with_options(
        archive_path,
        output_base,
        &ExtractionOptions {
            smart,
            to_folder: force_subfolder,
            password: password.map(str::to_owned),
            max_bytes,
            members: Vec::new(),
            flat: false,
        },
    )
}

/// One producer for all extraction: layout, staging, guards, collision handling,
/// shared commit, literal member selection, and flat (leaf-only) output.
pub fn extract_archive_with_options(
    archive_path: &Path,
    output_base: &Path,
    opts: &ExtractionOptions,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;

    // Validate literal member selections against the raw catalog *before* any
    // staging or commit happens, so missing requested members never disturb
    // existing destination files.
    let spec = if opts.members.is_empty() {
        None
    } else {
        let catalog = super::catalog::read_archive_catalog(
            archive_path,
            opts.password.as_deref(),
            opts.max_bytes,
        )?;
        Some(selection::SelectionSpec::build(&catalog, &opts.members)?)
    };

    match format {
        ArchiveFormat::Zip => extract_zip_archive(archive_path, output_base, opts, spec.as_ref()),
        ArchiveFormat::Tar => {
            extract_tar_archive(archive_path, output_base, None, opts, spec.as_ref())
        }
        ArchiveFormat::TarStream(codec) => {
            extract_tar_archive(archive_path, output_base, Some(codec), opts, spec.as_ref())
        }
        ArchiveFormat::Stream(codec) => {
            extract_stream_archive(archive_path, output_base, codec, opts, spec.as_ref())
        }
        ArchiveFormat::SevenZ => extract_7z_archive(archive_path, output_base, opts, spec.as_ref()),
        ArchiveFormat::Rar => extract_rar_archive(archive_path, output_base, opts, spec.as_ref()),
    }
}

/// Extract a .7z archive using staged atomic transactions
fn extract_7z_archive(
    archive_path: &Path,
    output_base: &Path,
    opts: &ExtractionOptions,
    spec: Option<&selection::SelectionSpec>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let password = opts.password.as_deref();
    let max_bytes = opts.max_bytes;
    let flat = opts.flat;

    let catalog =
        super::catalog::read_archive_catalog(archive_path, password, max_bytes).map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("password") {
                "Archive is password-protected. Please supply --password <PASS>".to_string()
            } else {
                msg
            }
        })?;
    let archive_stem = archive_stem_cleaned(archive_path);
    let layout_entries: Vec<super::smart::LayoutEntryInfo> =
        catalog.iter().map(|c| c.to_layout_entry()).collect();
    let plan = super::smart::evaluate_layout_plan(&layout_entries, &archive_stem, output_base)?;

    check_zip_bomb(plan.total_uncompressed_bytes, max_bytes)?;

    let (target_dir, smart_adjusted) = if opts.to_folder {
        (output_base.join(&archive_stem), true)
    } else if opts.smart {
        (plan.target_dir, !plan.has_single_root)
    } else {
        (output_base.to_path_buf(), false)
    };

    let (resolved_target, target_dir_existed_before) =
        resolve_and_verify_target_dir(output_base, &target_dir)?;

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(
        ".mothzip_stage_{}_{}",
        std::process::id(),
        staging_token
    ));
    fs::create_dir(&staging_dir)?;

    let pass = match password {
        Some(p) => sevenz_rust2::Password::from(p),
        None => sevenz_rust2::Password::empty(),
    };

    let mut staged_files = Vec::new();
    let mut dirs_to_create = Vec::new();
    let mut seen_destinations = HashSet::new();
    let mut extracted_bytes = 0u64;
    let mut entry_index = 0usize;

    let stage_result: Result<(), Box<dyn std::error::Error>> = (|| {
        let mut reader = super::format::open_7z_reader(archive_path, &pass).map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("password") {
                "Archive is password-protected. Please supply --password <PASS>".to_string()
            } else {
                format!("7z open failed: {}", msg)
            }
        })?;

        reader
            .for_each_entries(|entry, read_stream| {
                let entry_name = entry.name().replace('\\', "/");
                let norm_name = super::encoding::compose_hangul_nfc(&entry_name);

                if entry.is_anti_item() {
                    return Err(sevenz_rust2::Error::from(io::Error::other(format!(
                        "Unsupported entry '{}': 7z anti-items cannot be extracted",
                        norm_name
                    ))));
                }

                if entry.is_directory() {
                    if !flat {
                        let sanitized =
                            sanitize_entry_path(&resolved_target, &norm_name).map_err(|e| {
                                sevenz_rust2::Error::from(io::Error::other(e.to_string()))
                            })?;
                        dirs_to_create.push(sanitized);
                    }
                    return Ok(true);
                }

                // Literal member selection; drain unselected solid streams so later
                // selected members stay readable.
                if let Some(spec) = spec {
                    if !spec.matches(&entry_name) {
                        if entry.has_stream() {
                            super::guard::drain_archive_entry(
                                read_stream,
                                Some(entry.size()),
                                max_bytes,
                            )
                            .map_err(|e| {
                                sevenz_rust2::Error::from(io::Error::other(e.to_string()))
                            })?;
                        }
                        return Ok(true);
                    }
                }

                let sanitized = sanitize_entry_path(&resolved_target, &norm_name)
                    .map_err(|e| sevenz_rust2::Error::from(io::Error::other(e.to_string())))?;
                let candidate = if flat {
                    resolved_target.join(leaf_component(&sanitized)?)
                } else {
                    sanitized
                };
                let final_dest = disambiguate_dest_path(candidate, &mut seen_destinations);

                let temp_path = staging_dir.join(format!("entry_{}", entry_index));
                entry_index += 1;

                let mut out_file = File::create(&temp_path)
                    .map_err(|e| sevenz_rust2::Error::from(io::Error::other(e.to_string())))?;

                let written = copy_archive_entry(
                    read_stream,
                    &mut out_file,
                    Some(entry.size()),
                    extracted_bytes,
                    max_bytes,
                )
                .map_err(|e| sevenz_rust2::Error::from(io::Error::other(e.to_string())))?;

                extracted_bytes += written;

                let mtime = if entry.has_last_modified_date {
                    super::format::nt_time_to_system_time(entry.last_modified_date.into())
                } else {
                    None
                };

                staged_files.push(StagedFile {
                    temp_path,
                    final_dest,
                    mtime,
                });

                Ok(true)
            })
            .map_err(|e| -> Box<dyn std::error::Error> {
                let msg = e.to_string();
                if msg.to_lowercase().contains("password") {
                    "Decryption failed: bad password or corrupted archive".into()
                } else {
                    format!("7z extraction failed: {}", msg).into()
                }
            })?;

        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        if !target_dir_existed_before {
            let _ = fs::remove_dir(&resolved_target);
        }
        return Err(e);
    }

    let extracted_files = staged_files.len();
    commit_staged_extraction(dirs_to_create, staged_files, &staging_dir, &resolved_target)?;

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir: resolved_target,
        smart_adjusted,
    })
}

/// Extract a .rar archive using staged atomic transactions
fn extract_rar_archive(
    archive_path: &Path,
    output_base: &Path,
    opts: &ExtractionOptions,
    spec: Option<&selection::SelectionSpec>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let password = opts.password.as_deref();
    let max_bytes = opts.max_bytes;
    let flat = opts.flat;

    let catalog =
        super::catalog::read_archive_catalog(archive_path, password, max_bytes).map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("password") || msg.to_lowercase().contains("encrypted") {
                "Archive is password-protected. Please supply --password <PASS>".to_string()
            } else {
                msg
            }
        })?;
    let archive_stem = archive_stem_cleaned(archive_path);
    let layout_entries: Vec<super::smart::LayoutEntryInfo> =
        catalog.iter().map(|c| c.to_layout_entry()).collect();
    let plan = super::smart::evaluate_layout_plan(&layout_entries, &archive_stem, output_base)?;

    check_zip_bomb(plan.total_uncompressed_bytes, max_bytes)?;

    let (target_dir, smart_adjusted) = if opts.to_folder {
        (output_base.join(&archive_stem), true)
    } else if opts.smart {
        (plan.target_dir, !plan.has_single_root)
    } else {
        (output_base.to_path_buf(), false)
    };

    let (resolved_target, target_dir_existed_before) =
        resolve_and_verify_target_dir(output_base, &target_dir)?;

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(
        ".mothzip_stage_{}_{}",
        std::process::id(),
        staging_token
    ));
    fs::create_dir(&staging_dir)?;

    let mut staged_files = Vec::new();
    let mut dirs_to_create = Vec::new();
    let mut seen_destinations = HashSet::new();
    let mut extracted_bytes = 0u64;
    let mut entry_index = 0usize;

    let stage_result: Result<(), Box<dyn std::error::Error>> = (|| {
        let mut options = rars::ArchiveReadOptions::default();
        if let Some(p) = password {
            options.password = Some(p.as_bytes());
        }
        options.max_total_output_bytes = Some(max_bytes);
        options.max_member_output_bytes = Some(max_bytes);

        let volumes = super::format::find_volume_files(archive_path);
        if volumes.len() > 1 {
            let archives = volumes
                .iter()
                .map(|v| rars::ArchiveReader::read_path_with_options(v, options))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.to_lowercase().contains("password")
                        || msg.to_lowercase().contains("encrypted")
                    {
                        "Archive is password-protected. Please supply --password <PASS>".to_string()
                    } else {
                        format!("Failed to open RAR volume set: {}", msg)
                    }
                })?;

            let is_rar5 = archives
                .first()
                .map(|a| a.family() == rars::ArchiveFamily::Rar50Plus)
                .unwrap_or(false);

            rars::extract_volumes_to_with_options(&archives, options, |entry_meta| {
                let raw_name =
                    super::encoding::decode_entry_name_raw(&entry_meta.name).replace('\\', "/");
                let norm_name = super::encoding::compose_hangul_nfc(&raw_name);

                if entry_meta.is_directory {
                    if !flat {
                        let sanitized =
                            sanitize_entry_path(&resolved_target, &norm_name).map_err(|_| {
                                rars::Error::UnsafePath("unsafe directory path in archive")
                            })?;
                        dirs_to_create.push(sanitized);
                    }
                    return Ok(Box::new(io::sink()));
                }

                if let Some(spec) = spec {
                    if !spec.matches(&raw_name) {
                        return Ok(Box::new(io::sink()));
                    }
                }

                let sanitized = sanitize_entry_path(&resolved_target, &norm_name)
                    .map_err(|_| rars::Error::UnsafePath("unsafe file path in archive"))?;
                let candidate = if flat {
                    resolved_target.join(leaf_component(&sanitized)?)
                } else {
                    sanitized
                };
                let final_dest = disambiguate_dest_path(candidate, &mut seen_destinations);

                let temp_path = staging_dir.join(format!("entry_{}", entry_index));
                entry_index += 1;

                let out_file = File::create(&temp_path)
                    .map_err(|e| rars::Error::from(io::Error::other(e.to_string())))?;

                let mtime = entry_meta.file_time.and_then(|ft| {
                    if is_rar5 {
                        super::format::unix_secs_to_system_time(ft as u64)
                    } else {
                        let dos_date = (ft >> 16) as u16;
                        let dos_time = (ft & 0xFFFF) as u16;
                        super::format::dos_time_to_system_time(dos_date, dos_time)
                    }
                });

                staged_files.push(StagedFile {
                    temp_path,
                    final_dest,
                    mtime,
                });

                Ok(Box::new(out_file))
            })
            .map_err(|e| -> Box<dyn std::error::Error> {
                let msg = e.to_string();
                if msg.to_lowercase().contains("password")
                    || msg.to_lowercase().contains("encrypted")
                {
                    "Decryption failed: bad password or corrupted RAR block".into()
                } else {
                    format!("RAR volume extraction failed: {}", msg).into()
                }
            })?;
        } else {
            let archive = rars::ArchiveReader::read_path_with_options(archive_path, options)
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.to_lowercase().contains("password")
                        || msg.to_lowercase().contains("encrypted")
                    {
                        "Archive is password-protected. Please supply --password <PASS>".to_string()
                    } else {
                        format!("Failed to open RAR archive: {}", msg)
                    }
                })?;

            archive
                .extract_with_control(options, |member| {
                    let raw_name =
                        super::encoding::decode_entry_name_raw(&member.meta.name).replace('\\', "/");
                    let norm_name = super::encoding::compose_hangul_nfc(&raw_name);

                    if member.meta.is_redirection {
                        return Err(rars::Error::from(io::Error::other(format!(
                            "Unsupported entry '{}': RAR links/redirections are not supported for extraction",
                            norm_name
                        ))));
                    }

                    if member.meta.is_directory {
                        if !flat {
                            let sanitized = sanitize_entry_path(&resolved_target, &norm_name)
                                .map_err(|_| {
                                    rars::Error::UnsafePath("unsafe directory path in archive")
                                })?;
                            dirs_to_create.push(sanitized);
                        }
                        return Ok(rars::ExtractionDecision::Skip);
                    }

                    if let Some(spec) = spec {
                        if !spec.matches(&raw_name) {
                            // Solid archives cannot skip member data; drain the
                            // unselected member into a null writer to advance the
                            // decoder so later selected members stay readable.
                            return Ok(rars::ExtractionDecision::Extract(Box::new(
                                io::sink(),
                            )));
                        }
                    }

                    let sanitized = sanitize_entry_path(&resolved_target, &norm_name)
                        .map_err(|_| rars::Error::UnsafePath("unsafe file path in archive"))?;
                    let candidate = if flat {
                        resolved_target.join(leaf_component(&sanitized)?)
                    } else {
                        sanitized
                    };
                    let final_dest = disambiguate_dest_path(candidate, &mut seen_destinations);

                    let temp_path = staging_dir.join(format!("entry_{}", entry_index));
                    entry_index += 1;

                    let out_file = File::create(&temp_path)
                        .map_err(|e| rars::Error::from(io::Error::other(e.to_string())))?;

                    let mtime = member.meta.file_time.and_then(|ft| match member.meta.family {
                        rars::ArchiveFamily::Rar50Plus => {
                            super::format::unix_secs_to_system_time(ft as u64)
                        }
                        _ => {
                            let dos_date = (ft >> 16) as u16;
                            let dos_time = (ft & 0xFFFF) as u16;
                            super::format::dos_time_to_system_time(dos_date, dos_time)
                        }
                    });

                    staged_files.push(StagedFile {
                        temp_path,
                        final_dest,
                        mtime,
                    });

                    extracted_bytes = extracted_bytes
                        .checked_add(member.meta.unpacked_size)
                        .ok_or_else(|| {
                            rars::Error::from(io::Error::other(
                                "Archive uncompressed size overflow",
                            ))
                        })?;

                    Ok(rars::ExtractionDecision::Extract(Box::new(out_file)))
                })
                .map_err(|e| -> Box<dyn std::error::Error> {
                    let msg = e.to_string();
                    if msg.to_lowercase().contains("password")
                        || msg.to_lowercase().contains("encrypted")
                    {
                        "Decryption failed: bad password or corrupted RAR block".into()
                    } else {
                        format!("RAR extraction failed: {}", msg).into()
                    }
                })?;
        }

        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        if !target_dir_existed_before {
            let _ = fs::remove_dir(&resolved_target);
        }
        return Err(e);
    }

    let extracted_files = staged_files.len();
    commit_staged_extraction(dirs_to_create, staged_files, &staging_dir, &resolved_target)?;

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir: resolved_target,
        smart_adjusted,
    })
}

/// Extract a standard .zip archive using staged atomic transactions
fn extract_zip_archive(
    archive_path: &Path,
    output_base: &Path,
    opts: &ExtractionOptions,
    spec: Option<&selection::SelectionSpec>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let password = opts.password.as_deref();
    let smart = opts.smart;
    let force_subfolder = opts.to_folder;
    let max_bytes = opts.max_bytes;
    let flat = opts.flat;

    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    let archive_stem = archive_stem_cleaned(archive_path);

    let plan = plan_smart_unpack(&mut archive, &archive_stem, output_base)?;
    check_zip_bomb(plan.total_uncompressed_bytes, max_bytes)?;
    let (target_dir, smart_adjusted) = if force_subfolder {
        (output_base.join(&archive_stem), true)
    } else if smart {
        (plan.target_dir, !plan.has_single_root)
    } else {
        (output_base.to_path_buf(), false)
    };

    let (resolved_target, target_dir_existed_before) =
        resolve_and_verify_target_dir(output_base, &target_dir)?;

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(
        ".mothzip_stage_{}_{}",
        std::process::id(),
        staging_token
    ));
    fs::create_dir(&staging_dir)?;

    let mut staged_files = Vec::new();
    let mut dirs_to_create = Vec::new();
    let mut seen_destinations = HashSet::new();
    let mut extracted_bytes = 0u64;
    let mut entry_index = 0usize;

    let stage_result: Result<(), Box<dyn std::error::Error>> =
        (|| -> Result<(), Box<dyn std::error::Error>> {
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

                if entry.is_symlink() {
                    return Err(
                        "Symbolic-link ZIP members are not supported; no files were committed"
                            .into(),
                    );
                }
                let identity =
                    super::encoding::decode_entry_name_raw(entry.name_raw()).replace('\\', "/");
                let raw_name = super::encoding::compose_hangul_nfc(&identity);

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

                // Literal member selection (raw identity or directory prefix).
                if let Some(spec) = spec {
                    if !spec.matches(&identity) {
                        continue;
                    }
                }

                let raw_dest = sanitize_entry_path(&resolved_target, &raw_name)?;
                verify_safe_ancestors(&raw_dest, &resolved_target)?;

                let is_dir_entry =
                    entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');

                if is_dir_entry {
                    if !flat {
                        dirs_to_create.push(raw_dest);
                    }
                } else {
                    let candidate = if flat {
                        resolved_target.join(leaf_component(&raw_dest)?)
                    } else {
                        raw_dest
                    };
                    let final_dest = disambiguate_dest_path(candidate, &mut seen_destinations);
                    verify_safe_ancestors(&final_dest, &resolved_target)?;

                    let stage_file_path = staging_dir.join(format!("{}.tmp", entry_index));
                    entry_index += 1;

                    let mut outfile = File::create(&stage_file_path)?;

                    let declared_size = entry.size();
                    let written = copy_archive_entry(
                        &mut entry,
                        &mut outfile,
                        Some(declared_size),
                        extracted_bytes,
                        max_bytes,
                    )?;
                    extracted_bytes += written;

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
            let _ = fs::remove_dir(&resolved_target);
        }
        return Err(e);
    }

    let extracted_files = staged_files.len();
    commit_staged_extraction(dirs_to_create, staged_files, &staging_dir, &resolved_target)?;

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir: resolved_target,
        smart_adjusted,
    })
}

/// Extract a .tar or .tar.gz / .tgz archive with adversarial security guards and atomic transactions
fn extract_tar_archive(
    archive_path: &Path,
    output_base: &Path,
    codec: Option<StreamCodec>,
    opts: &ExtractionOptions,
    spec: Option<&selection::SelectionSpec>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let smart = opts.smart;
    let force_subfolder = opts.to_folder;
    let max_bytes = opts.max_bytes;
    let flat = opts.flat;

    let archive_stem = archive_stem_cleaned(archive_path);
    let archive_file_size = fs::metadata(archive_path)?.len();

    let (target_dir, smart_adjusted) = if force_subfolder {
        (output_base.join(&archive_stem), true)
    } else if smart {
        let reader = super::format::open_tar_reader(archive_path, codec, max_bytes)?;
        let plan = plan_smart_unpack_tar(
            tar::Archive::new(reader),
            &archive_stem,
            output_base,
            archive_file_size,
            max_bytes,
        )?;
        check_zip_bomb(plan.total_uncompressed_bytes, max_bytes)?;
        (plan.target_dir, !plan.has_single_root)
    } else {
        (output_base.to_path_buf(), false)
    };

    let (resolved_target, target_dir_existed_before) =
        resolve_and_verify_target_dir(output_base, &target_dir)?;

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(
        ".mothzip_stage_{}_{}",
        std::process::id(),
        staging_token
    ));
    fs::create_dir(&staging_dir)?;

    let reader = super::format::open_tar_reader(archive_path, codec, max_bytes)?;
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
            let raw_path = path_cow.to_string_lossy().replace('\\', "/");
            let raw_name = crate::core::encoding::compose_hangul_nfc(&raw_path);

            // Skip macOS/Windows system junk files
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
            {
                continue;
            }

            // Literal member selection (raw identity or directory prefix).
            if let Some(spec) = spec {
                if !spec.matches(&raw_path) {
                    continue;
                }
            }

            let raw_dest = sanitize_entry_path(&resolved_target, &raw_name)?;
            verify_safe_ancestors(&raw_dest, &resolved_target)?;

            let entry_type = entry.header().entry_type();
            let is_dir_entry =
                entry_type.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\');

            if is_dir_entry {
                if !flat {
                    dirs_to_create.push(raw_dest);
                }
            } else if entry_type.is_file() {
                let candidate = if flat {
                    resolved_target.join(leaf_component(&raw_dest)?)
                } else {
                    raw_dest
                };
                let final_dest = disambiguate_dest_path(candidate, &mut seen_destinations);
                verify_safe_ancestors(&final_dest, &resolved_target)?;

                let stage_file_path = staging_dir.join(format!("{}.tmp", entry_index));
                entry_index += 1;

                let mut outfile = File::create(&stage_file_path)?;

                let declared_size = entry.header().size()?;
                let written = copy_archive_entry(
                    &mut entry,
                    &mut outfile,
                    Some(declared_size),
                    extracted_bytes,
                    max_bytes,
                )?;
                extracted_bytes += written;

                let mtime = entry
                    .header()
                    .mtime()
                    .ok()
                    .and_then(|mt| UNIX_EPOCH.checked_add(Duration::from_secs(mt)));

                staged_files.push(StagedFile {
                    temp_path: stage_file_path,
                    final_dest,
                    mtime,
                });
            } else {
                return Err(format!(
                    "Unsupported TAR member type for '{}'; no files were committed",
                    raw_name
                )
                .into());
            }
        }

        // Drain the rest of the stream so the decoder validates its trailer and checksum
        // (gzip CRC32/ISIZE, xz check, zstd content checksum when present) before we commit.
        let mut inner = tar.into_inner();
        let mut drain = io::sink();
        io::copy(&mut inner, &mut drain)?;

        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        if !target_dir_existed_before {
            let _ = fs::remove_dir(&resolved_target);
        }
        return Err(e);
    }

    let extracted_files = staged_files.len();
    commit_staged_extraction(dirs_to_create, staged_files, &staging_dir, &resolved_target)?;

    Ok(ExtractionSummary {
        extracted_files,
        extracted_bytes,
        target_dir: resolved_target,
        smart_adjusted,
    })
}

/// Extract a single stream archive (`.gz`, `.zst`, `.xz`, `.lz4`, `.br`) into its stem name.
fn extract_stream_archive(
    archive_path: &Path,
    output_base: &Path,
    codec: StreamCodec,
    opts: &ExtractionOptions,
    spec: Option<&selection::SelectionSpec>,
) -> Result<ExtractionSummary, Box<dyn std::error::Error>> {
    let max_bytes = opts.max_bytes;
    let archive_stem = archive_stem_cleaned(archive_path);

    // A bare stream's only member is its stem; selection was already validated
    // against the catalog, so this is a consistency guard for direct callers.
    if let Some(spec) = spec {
        if !spec.matches(&archive_stem) {
            return Err(format!(
                "Requested member '{}' is not present in the {} archive",
                archive_stem,
                codec.name()
            )
            .into());
        }
    }

    let target_dir = if opts.to_folder {
        output_base.join(&archive_stem)
    } else {
        output_base.to_path_buf()
    };

    let (resolved_target, target_dir_existed_before) =
        resolve_and_verify_target_dir(output_base, &target_dir)?;

    let final_dest = sanitize_entry_path(&resolved_target, &archive_stem)?;
    verify_safe_ancestors(&final_dest, &resolved_target)?;

    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = resolved_target.join(format!(
        ".mothzip_stage_{}_{}",
        std::process::id(),
        staging_token
    ));
    fs::create_dir(&staging_dir)?;
    let stage_file_path = staging_dir.join("0.tmp");

    let mut reader = super::codec::decoder(File::open(archive_path)?, codec)?;

    let stage_result: Result<u64, Box<dyn std::error::Error>> = (|| {
        let mut outfile = File::create(&stage_file_path)?;
        let written = copy_archive_entry(&mut reader, &mut outfile, None, 0, max_bytes)?;
        Ok(written)
    })();

    let extracted_bytes = match stage_result {
        Ok(b) => b,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging_dir);
            if !target_dir_existed_before {
                let _ = fs::remove_dir(&resolved_target);
            }
            return Err(e);
        }
    };

    let staged_files = vec![StagedFile {
        temp_path: stage_file_path,
        final_dest,
        mtime: None,
    }];

    commit_staged_extraction(Vec::new(), staged_files, &staging_dir, &resolved_target)?;

    Ok(ExtractionSummary {
        extracted_files: 1,
        extracted_bytes,
        target_dir: resolved_target,
        smart_adjusted: opts.to_folder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    #[test]
    fn aes_passwords_and_authenticated_corruption_preserve_existing_files() {
        for (index, mode) in [zip::AesMode::Aes128, zip::AesMode::Aes256]
            .into_iter()
            .enumerate()
        {
            let dir = PathBuf::from("target").join(format!(
                "aes_fixture_{}_{}_{}",
                std::process::id(),
                index,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .with_aes_encryption(mode, "fixture-only");
            writer.start_file("secret.txt", options).unwrap();
            std::io::Write::write_all(&mut writer, b"AUTHENTICATED CONTENT").unwrap();
            let bytes = writer.finish().unwrap().into_inner();
            let archive = dir.join("valid.zip");
            fs::write(&archive, &bytes).unwrap();
            let out = dir.join("out");
            extract_archive(
                &archive,
                &out,
                false,
                false,
                Some("fixture-only"),
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            )
            .unwrap();
            assert_eq!(
                fs::read(out.join("secret.txt")).unwrap(),
                b"AUTHENTICATED CONTENT"
            );
            let smart = extract_archive(
                &archive,
                &dir.join("smart"),
                true,
                false,
                Some("fixture-only"),
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            )
            .unwrap();
            assert_eq!(
                fs::read(smart.target_dir.join("secret.txt")).unwrap(),
                b"AUTHENTICATED CONTENT"
            );
            let mut cat = Vec::new();
            crate::core::inspect::cat_archive_file(
                &archive,
                "secret.txt",
                Some("fixture-only"),
                &mut cat,
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            )
            .unwrap();
            assert_eq!(cat, b"AUTHENTICATED CONTENT");
            fs::write(out.join("secret.txt"), b"ORIGINAL").unwrap();
            assert!(extract_archive(
                &archive,
                &out,
                false,
                false,
                Some("wrong-fixture"),
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            )
            .is_err());
            assert_eq!(fs::read(out.join("secret.txt")).unwrap(), b"ORIGINAL");
            let mut parsed = ZipArchive::new(Cursor::new(&bytes)).unwrap();
            let offset = parsed.by_index_raw(0).unwrap().data_start().unwrap() as usize;
            let mut corrupt = bytes.clone();
            let salt_length = if index == 0 { 8 } else { 16 };
            corrupt[offset + salt_length + 2 + 1] ^= 1;
            let bad = dir.join("corrupt.zip");
            fs::write(&bad, corrupt).unwrap();
            assert!(extract_archive(
                &bad,
                &out,
                false,
                false,
                Some("fixture-only"),
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            )
            .is_err());
            assert_eq!(fs::read(out.join("secret.txt")).unwrap(), b"ORIGINAL");
        }
    }

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

        let res = extract_archive(
            &zip_path,
            &temp_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
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
        let res = extract_archive(
            &targz_path,
            &temp_dir,
            true,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
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
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(cat_out).unwrap(),
            "[package]\nname = \"my_crate\""
        );

        // 3. Test peek on the tar.gz
        let peek = crate::core::inspect::peek_archive(
            &targz_path,
            2,
            50,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(peek.total_entries, 2);
        assert!(peek.has_single_root);
        assert_eq!(peek.root_name.as_deref(), Some("my_crate"));

        let _ = fs::remove_file(targz_path);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_7z_extraction_and_smart_unpack() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_7z_extract_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);

        let root_dir = temp_dir.join("single_root");
        let src_dir = root_dir.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let file1 = src_dir.join("lib.rs");
        fs::write(&file1, b"pub fn add(a: i32, b: i32) -> i32 { a + b }").unwrap();

        let sz_path = temp_dir.join("project.7z");
        crate::core::pack::pack_archive(&sz_path, &[root_dir]).unwrap();

        let out_dir = temp_dir.join("unpacked");
        let res = extract_archive(
            &sz_path,
            &out_dir,
            true,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(res.extracted_files, 1);
        assert!(
            !res.smart_adjusted,
            "Single root folder should not create wrapper"
        );
        assert!(out_dir
            .join("single_root")
            .join("src")
            .join("lib.rs")
            .exists());
        assert_eq!(
            fs::read(out_dir.join("single_root").join("src").join("lib.rs")).unwrap(),
            b"pub fn add(a: i32, b: i32) -> i32 { a + b }"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_7z_encryption_lifecycle() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_7z_enc_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);
        let secret_file = temp_dir.join("secret.txt");
        fs::write(&secret_file, b"TOP_SECRET_7Z_DATA").unwrap();

        let sz_path = temp_dir.join("secret.7z");
        let options = crate::core::pack::PackOptions {
            level: Some(5),
            password: Some("my-7z-pass"),
            format: Some(ArchiveFormat::SevenZ),
        };
        crate::core::pack::pack_archive_with_options(&sz_path, &[secret_file], options).unwrap();

        let out_dir = temp_dir.join("out");
        // Extract without password fails
        assert!(extract_archive(
            &sz_path,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .is_err());
        assert!(!out_dir.join("secret.txt").exists());

        // Extract with wrong password fails
        assert!(extract_archive(
            &sz_path,
            &out_dir,
            false,
            false,
            Some("wrong-pass"),
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .is_err());
        assert!(!out_dir.join("secret.txt").exists());

        // Extract with correct password succeeds
        let res = extract_archive(
            &sz_path,
            &out_dir,
            false,
            false,
            Some("my-7z-pass"),
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(res.extracted_files, 1);
        assert_eq!(
            fs::read(out_dir.join("secret.txt")).unwrap(),
            b"TOP_SECRET_7Z_DATA"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_rar5_extraction_and_smart_unpack() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_rar5_extract_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);

        let rar_path = temp_dir.join("test_rar5.rar");
        let mut builder = rars::Builder::new(rars::ArchiveVersion::Rar50);
        builder.add_directory(b"app".to_vec(), None, None).unwrap();
        builder
            .add_bytes(
                b"app/config.yaml".to_vec(),
                b"version: 1.0".to_vec(),
                None,
                None,
            )
            .unwrap();
        builder
            .add_bytes(b"app/run.sh".to_vec(), b"echo hello".to_vec(), None, None)
            .unwrap();
        builder.write_to_path(&rar_path, None).unwrap();

        let out_dir = temp_dir.join("unpacked");
        let res = extract_archive(
            &rar_path,
            &out_dir,
            true,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(res.extracted_files, 2);
        assert!(
            !res.smart_adjusted,
            "Single root folder app should not create redundant wrapper"
        );
        assert_eq!(
            fs::read_to_string(out_dir.join("app").join("config.yaml")).unwrap(),
            "version: 1.0"
        );
        assert_eq!(
            fs::read_to_string(out_dir.join("app").join("run.sh")).unwrap(),
            "echo hello"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_rar4_legacy_extraction() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_rar4_extract_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);

        let rar_path = temp_dir.join("legacy_v4.rar");
        let mut builder = rars::Builder::new(rars::ArchiveVersion::Rar40);
        builder
            .add_bytes(
                b"readme.txt".to_vec(),
                b"Legacy RAR4 text".to_vec(),
                None,
                None,
            )
            .unwrap();
        builder.write_to_path(&rar_path, None).unwrap();

        let out_dir = temp_dir.join("out");
        let res = extract_archive(
            &rar_path,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(res.extracted_files, 1);
        assert_eq!(
            fs::read(out_dir.join("readme.txt")).unwrap(),
            b"Legacy RAR4 text"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_rar_encryption_lifecycle() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_rar_enc_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);

        let rar_path = temp_dir.join("encrypted.rar");
        let mut builder = rars::Builder::new(rars::ArchiveVersion::Rar50);
        builder
            .add_bytes(
                b"confidential.txt".to_vec(),
                b"RESTRICTED CONTENT".to_vec(),
                None,
                None,
            )
            .unwrap();
        builder
            .set_entry_encryption(b"confidential.txt", Some(b"rar-secret".to_vec()), None)
            .unwrap();
        builder.write_to_path(&rar_path, None).unwrap();

        let out_dir = temp_dir.join("out");
        // Pre-create destination file to test rollback / preservation
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("confidential.txt"), b"ORIGINAL PRESERVED").unwrap();

        // 1. Without password fails and preserves original
        let err1 = extract_archive(
            &rar_path,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        );
        assert!(err1.is_err());
        assert_eq!(
            fs::read(out_dir.join("confidential.txt")).unwrap(),
            b"ORIGINAL PRESERVED"
        );

        // 2. With wrong password fails and preserves original
        let err2 = extract_archive(
            &rar_path,
            &out_dir,
            false,
            false,
            Some("wrong"),
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        );
        assert!(err2.is_err());
        assert_eq!(
            fs::read(out_dir.join("confidential.txt")).unwrap(),
            b"ORIGINAL PRESERVED"
        );

        // 3. With correct password succeeds and updates destination
        let ok = extract_archive(
            &rar_path,
            &out_dir,
            false,
            false,
            Some("rar-secret"),
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert_eq!(ok.extracted_files, 1);
        assert_eq!(
            fs::read(out_dir.join("confidential.txt")).unwrap(),
            b"RESTRICTED CONTENT"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_rar_adversarial_zip_slip_rejection() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_rar_slip_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);

        let rar_path = temp_dir.join("slip.rar");
        let mut builder = rars::Builder::new(rars::ArchiveVersion::Rar50);
        let add_res =
            builder.add_bytes(b"../escape.txt".to_vec(), b"malicious".to_vec(), None, None);
        if add_res.is_ok() {
            let _ = builder.write_to_path(&rar_path, None);
            let out_dir = temp_dir.join("target_dir");
            let outside_file = temp_dir.join("escape.txt");
            let _ = extract_archive(
                &rar_path,
                &out_dir,
                false,
                false,
                None,
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            );
            assert!(
                !outside_file.exists(),
                "ZipSlip traversal file must never be created outside target"
            );
        }

        let _ = fs::remove_dir_all(temp_dir);
    }
}
