use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::format::civil_from_days;
use super::format::ArchiveFormat;

/// RAR creation is out of scope for every writer; one message keeps the entry points honest.
const RAR_NOT_IMPLEMENTED: &str = "RAR creation is not implemented; use ZIP or 7z for compression";

#[derive(Debug, Clone, serde::Serialize)]
pub struct PackSummary {
    pub archive_path: PathBuf,
    pub total_files: usize,
    pub total_uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub compression_ratio: String,
    /// Volume file paths for a split 7z set (empty for a single-file archive).
    pub volumes: Vec<PathBuf>,
    /// Byte size of each volume, parallel to `volumes`.
    pub volume_sizes: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct PackOptions<'a> {
    pub level: Option<u32>, // 0..9 (0: store, 5/6: default, 9: maximum)
    pub password: Option<&'a str>,
    pub format: Option<ArchiveFormat>,
}

/// One pack request: what to compress and how the archive must be laid out.
///
/// The reproducible flag lives here rather than in [`PackOptions`] because it changes the member
/// order and every timestamp — a different archive, not a different compression setting — and
/// because `PackOptions` is a public struct literal that other producers already build.
#[derive(Debug, Clone, Default)]
pub struct PackRequest<'a> {
    pub options: PackOptions<'a>,
    /// Sort entries by member name and pin every timestamp, so the same tree always yields the
    /// same bytes (`--reproducible`).
    pub reproducible: bool,
    /// LZMA2 dictionary size in bytes for 7z output (`--7z-dict`). The dictionary is what the
    /// match finder's memory is proportional to, so this is the memory lever for 7z creation.
    pub seven_z_dictionary_bytes: Option<u32>,
}

pub use super::format::system_time_to_zip_datetime;

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

/// Resolve target archive format from options or destination file extension.
pub fn resolve_pack_format(
    dest_path: &Path,
    explicit_format: Option<ArchiveFormat>,
) -> Result<ArchiveFormat, Box<dyn std::error::Error>> {
    if let Some(fmt) = explicit_format {
        return Ok(fmt);
    }
    let lower = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    match super::format::format_for_name(&lower) {
        Some(ArchiveFormat::Rar) => Err(RAR_NOT_IMPLEMENTED.into()),
        Some(format) => Ok(format),
        None => Ok(ArchiveFormat::Zip),
    }
}

/// Reject an unsupported level before any bytes exist, using the same producer that validates it
/// inside the codec factory.
fn validate_level(
    format: ArchiveFormat,
    level: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(codec) = format.codec() {
        super::codec::validated_level(codec, level)
            .map_err(|error| format!("{}: {}", format.name(), error))?;
        return Ok(());
    }
    // ZIP and 7z take a 0..=9 level. 7z used to clamp anything above 9 without saying so, and ZIP
    // only failed after the staging file existed, with the encoder's own message.
    if matches!(format, ArchiveFormat::Zip | ArchiveFormat::SevenZ) {
        if let Some(level) = level {
            if level > 9 {
                return Err(format!("{} compression level must be 0..=9", format.name()).into());
            }
        }
    }
    Ok(())
}

/// `SOURCE_DATE_EPOCH` as whole seconds, the value `--reproducible` pins every entry timestamp to.
///
/// An unparsable value is refused instead of falling back to zero: a silently different timestamp
/// is exactly the non-reproducible result the flag exists to prevent.
fn reproducible_epoch_seconds() -> Result<u64, String> {
    match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(text) => parse_epoch_seconds(&text),
        Err(std::env::VarError::NotPresent) => Ok(0),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("SOURCE_DATE_EPOCH is not valid Unicode".to_string())
        }
    }
}

fn parse_epoch_seconds(text: &str) -> Result<u64, String> {
    text.trim().parse::<u64>().map_err(|_| {
        format!("SOURCE_DATE_EPOCH must be whole seconds since 1970-01-01, got '{text}'")
    })
}

/// The DOS date/time a reproducible ZIP records for the fixed instant.
///
/// ZIP stores a local wall-clock DOS time, and `system_time_to_zip_datetime` converts through the
/// process time zone on Windows, so one `SOURCE_DATE_EPOCH` would produce a different archive on a
/// machine in another zone. The conversion here is plain UTC. The format cannot express a date
/// before 1980, so anything earlier clamps to the DOS floor `zip::DateTime::DEFAULT` (1980-01-01).
fn reproducible_zip_time(epoch_seconds: u64) -> zip::DateTime {
    let (year, month, day) = civil_from_days((epoch_seconds / 86_400) as i64);
    let second_of_day = epoch_seconds % 86_400;
    // 2107 is the last year a seven-bit DOS year field reaches.
    if !(1980..=2107).contains(&year) {
        return zip::DateTime::DEFAULT;
    }
    zip::DateTime::from_date_and_time(
        year as u16,
        month,
        day,
        (second_of_day / 3_600) as u8,
        ((second_of_day % 3_600) / 60) as u8,
        (second_of_day % 60) as u8,
    )
    .unwrap_or(zip::DateTime::DEFAULT)
}

/// A 7z timestamp for a Unix instant (100 ns ticks since 1601-01-01, the NT epoch).
fn seven_z_time(epoch_seconds: u64) -> sevenz_rust2::NtTime {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    let unix_epoch: u64 = sevenz_rust2::NtTime::UNIX_EPOCH.into();
    sevenz_rust2::NtTime::from(unix_epoch + epoch_seconds.saturating_mul(TICKS_PER_SECOND))
}

/// Pin one 7z entry to the reproducible instant.
///
/// `ArchiveEntry::from_path` copies the source mtime, ctime and atime, so an archive of a touched
/// tree would differ on every run; a fixed timestamp must replace all three, not just the mtime.
fn pin_seven_z_entry_time(entry: &mut sevenz_rust2::ArchiveEntry, epoch_seconds: u64) {
    entry.has_creation_date = false;
    entry.has_access_date = false;
    entry.last_modified_date = seven_z_time(epoch_seconds);
    entry.has_last_modified_date = true;
}

/// One entry of a source tree as it will enter an archive: the on-disk path, the member name it
/// maps to, and the metadata captured during the single survey.
///
/// Merged identity of the former pack inventory (`SourceItem`) and the editor's add inventory
/// (`InvItem`), so the member name, the symlink/special-object refusal and the exclusion decision
/// cannot drift between creating an archive and adding to one. `disk_path` is unconditional because
/// every former `InvItem` was built with `Some(path)`; the branches that guarded it were unreachable.
#[derive(Debug, Clone)]
pub(crate) struct SourceTreeItem {
    pub(crate) disk_path: PathBuf,
    pub(crate) entry_name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    /// Captured once for the ZIP/TAR writers; the editor re-stats an added member when it writes.
    pub(crate) mtime: Option<SystemTime>,
}

/// The survey decisions that genuinely differ between pack and edit. The walk, the entry naming,
/// the exclusion comparison and the refusal detection belong to [`collect_source_tree`]; every
/// field below exists because the two former collectors decided that point differently, and no field
/// carries state.
pub(crate) struct SourceTreePolicy<'a> {
    /// Paths that must never enter the inventory: the archive output and its stale volumes, or the
    /// archive being edited. Resolved once, before any source is read.
    pub(crate) excluded: &'a [PathBuf],
    /// A walked descendant that resolves onto an excluded path: `Ok` leaves it out (pack), `Err`
    /// refuses the tree (edit refuses a tree that would contain the archive being edited).
    pub(crate) excluded_descendant: fn(&Path) -> Result<(), String>,
    /// An excluded path that cannot be canonicalized: `Some` aborts (pack could then pack its own
    /// output), `None` treats it as not excluded (edit).
    pub(crate) unresolvable_path: fn(&Path, &io::Error) -> Option<String>,
    /// A top-level source that resolves onto an excluded path.
    pub(crate) excluded_source: fn(&Path) -> String,
    /// A top-level source that is a symbolic link.
    pub(crate) symlink_source: fn(&Path) -> String,
    /// A top-level source whose metadata cannot be read.
    pub(crate) unreadable_source: fn(&Path, &io::Error) -> String,
    /// A source that is neither a regular file nor a directory (`None` skips the source).
    pub(crate) special_source: fn(&Path) -> Option<String>,
    /// Member name of a top-level file source; the only place a non-UTF-8 name can be refused,
    /// because walked descendants are decoded lossily by both callers.
    pub(crate) file_member_name: fn(&Path) -> Result<String, String>,
    /// Archive member-name rules applied while collecting (`Ok` copies the filesystem name).
    pub(crate) validate_name: fn(&str) -> Result<(), String>,
    /// A walked descendant that is neither a regular file nor a directory; the first argument is
    /// the source root, which edit names in its message.
    pub(crate) special_descendant: fn(&Path, &Path) -> String,
    /// `Some` rejects a repeated entry name during the survey; `None` leaves member-name
    /// deduplication to the caller, whose plan also sees retained and renamed members.
    pub(crate) duplicate_name: Option<fn(&str) -> String>,
}

/// Compare one candidate path against the resolved exclusion set. An unresolvable path is never
/// silently treated as excluded: the caller decides whether it is fatal (pack) or irrelevant (edit).
fn resolves_into_excluded(
    path: &Path,
    excluded: &HashSet<PathBuf>,
    policy: &SourceTreePolicy<'_>,
) -> Result<bool, String> {
    match path.canonicalize() {
        Ok(canonical) => Ok(excluded.contains(&canonical)),
        Err(error) => match (policy.unresolvable_path)(path, &error) {
            Some(message) => Err(message),
            None => Ok(false),
        },
    }
}

/// Drop trailing separators without changing what the path refers to.
///
/// `C:\` has to stay `C:\`: trimming it produces `C:`, which Windows reads as "the current
/// directory on drive C" — a different tree that would be packed silently instead of the drive.
fn without_trailing_separators(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        return path.to_path_buf();
    }
    PathBuf::from(trimmed)
}

/// The single producer of an archive member inventory from source filesystem paths.
///
/// A file source contributes its file name; a directory source contributes itself and its whole
/// subtree with `/`-separated names relative to the directory's parent. For every caller it refuses
/// symbolic links and special objects, keeps the caller's excluded paths out of the archive, and
/// (when the caller asks) produces each member name exactly once.
pub(crate) fn collect_source_tree(
    sources: &[PathBuf],
    policy: &SourceTreePolicy<'_>,
) -> Result<Vec<SourceTreeItem>, String> {
    let mut excluded = HashSet::new();
    for path in policy.excluded {
        match path.canonicalize() {
            Ok(canonical) => {
                excluded.insert(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                if let Some(message) = (policy.unresolvable_path)(path, &error) {
                    return Err(message);
                }
            }
        }
    }

    let mut items = Vec::new();
    let mut seen_entries = HashSet::new();

    for source in sources {
        let clean_source = without_trailing_separators(source);
        let clean_source = clean_source.as_path();

        let meta = clean_source
            .symlink_metadata()
            .map_err(|error| (policy.unreadable_source)(clean_source, &error))?;
        if meta.file_type().is_symlink() {
            return Err((policy.symlink_source)(clean_source));
        }
        if !meta.is_file() && !meta.is_dir() {
            match (policy.special_source)(clean_source) {
                Some(message) => return Err(message),
                None => continue,
            }
        }
        if resolves_into_excluded(clean_source, &excluded, policy)? {
            return Err((policy.excluded_source)(clean_source));
        }

        if meta.is_file() {
            let entry_name = (policy.file_member_name)(clean_source)?;
            (policy.validate_name)(&entry_name)?;
            if let Some(duplicate) = policy.duplicate_name {
                if !seen_entries.insert(entry_name.clone()) {
                    return Err(duplicate(&entry_name));
                }
            }
            items.push(SourceTreeItem {
                disk_path: clean_source.to_path_buf(),
                entry_name,
                is_dir: false,
                size: meta.len(),
                mtime: meta.modified().ok(),
            });
            continue;
        }

        let base_parent = clean_source.parent().unwrap_or(clean_source);
        for entry in WalkDir::new(clean_source) {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if resolves_into_excluded(path, &excluded, policy)? {
                (policy.excluded_descendant)(path)?;
                continue;
            }

            let relative = path
                .strip_prefix(base_parent)
                .map_err(|_| format!("walk escaped source root: {}", path.display()))?;
            let raw_name = relative.to_string_lossy().replace('\\', "/");
            let name = raw_name.strip_prefix("./").unwrap_or(&raw_name);
            if name.is_empty() {
                continue;
            }

            let is_dir = entry.file_type().is_dir();
            let is_file = entry.file_type().is_file();
            (policy.validate_name)(name)?;
            if !is_dir && !is_file {
                return Err((policy.special_descendant)(clean_source, path));
            }
            if let Some(duplicate) = policy.duplicate_name {
                if !seen_entries.insert(name.to_string()) {
                    return Err(duplicate(name));
                }
            }
            let metadata = entry.metadata().map_err(|error| error.to_string())?;
            items.push(SourceTreeItem {
                disk_path: path.to_path_buf(),
                entry_name: name.to_string(),
                is_dir,
                size: if is_dir { 0 } else { metadata.len() },
                mtime: metadata.modified().ok(),
            });
        }
    }

    Ok(items)
}

/// pack's answer to every [`SourceTreePolicy`] decision.
fn pack_tree_policy(excluded: &[PathBuf]) -> SourceTreePolicy<'_> {
    SourceTreePolicy {
        excluded,
        excluded_descendant: |_| Ok(()),
        // pack cannot verify that an unreadable path is not its own output, so it aborts.
        unresolvable_path: |_, error| Some(error.to_string()),
        excluded_source: |_| {
            "Cannot pack an archive output or one of its volumes into itself".to_string()
        },
        symlink_source: |path| format!("Unsupported source object: {}", path.display()),
        unreadable_source: |_, error| error.to_string(),
        special_source: |_| None,
        file_member_name: |path| {
            Ok(path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file")
                .to_string())
        },
        validate_name: |_| Ok(()),
        special_descendant: |_, path| format!("Unsupported source object: {}", path.display()),
        duplicate_name: Some(|name| {
            format!(
                "Duplicate archive entry name '{}' from multiple sources",
                name
            )
        }),
    }
}

/// The archive body as every writer receives it: the surveyed inventory plus the single timestamp
/// decision `--reproducible` made, so no two writers can disagree about order or time.
struct ArchivePlan {
    items: Vec<SourceTreeItem>,
    total_files: usize,
    total_uncompressed_bytes: u64,
    /// `Some(seconds)` when `--reproducible` pinned every entry to one instant.
    fixed_epoch_seconds: Option<u64>,
    /// LZMA2 dictionary override for 7z output, carried with the plan because it is a property
    /// of how this archive is written rather than of the source tree.
    seven_z_dictionary_bytes: Option<u32>,
}

/// Survey the sources and decide the member order and timestamps, before anything is written.
///
/// Every pack destination shares this: a file the tool replaces atomically, and stdout where the
/// bytes that already left are the only copy, must see the same inventory in the same order.
fn plan_archive(
    sources: &[PathBuf],
    request: &PackRequest<'_>,
    excluded: &[PathBuf],
) -> Result<ArchivePlan, Box<dyn std::error::Error>> {
    if sources.is_empty() {
        return Err("No source files or directories specified for packing".into());
    }

    for source in sources {
        if !source.exists() {
            return Err(format!("Source path '{}' does not exist", source.display()).into());
        }
    }

    let mut items = collect_source_tree(sources, &pack_tree_policy(excluded))?;
    let fixed_epoch_seconds = if request.reproducible {
        let seconds = reproducible_epoch_seconds()?;
        // Byte order, not the locale's: the archive must not depend on which machine sorted it.
        items.sort_by(|left, right| left.entry_name.as_bytes().cmp(right.entry_name.as_bytes()));
        Some(seconds)
    } else {
        None
    };
    let total_files = items.iter().filter(|item| !item.is_dir).count();
    let total_uncompressed_bytes: u64 = items
        .iter()
        .filter(|item| !item.is_dir)
        .map(|item| item.size)
        .sum();

    Ok(ArchivePlan {
        items,
        total_files,
        total_uncompressed_bytes,
        fixed_epoch_seconds,
        seven_z_dictionary_bytes: request.seven_z_dictionary_bytes,
    })
}

/// Compress files or directories into an archive with default options.
#[cfg(test)]
pub fn pack_archive(
    dest_path: &Path,
    sources: &[PathBuf],
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    pack_archive_with_options(dest_path, sources, PackOptions::default())
}

/// Compress files or directories with explicit options (compression level, format, password).
/// Defends against self-packing collision, missing sources, and provides transactional rollback.
pub fn pack_archive_with_options(
    dest_path: &Path,
    sources: &[PathBuf],
    options: PackOptions,
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    pack_archive_request(
        dest_path,
        sources,
        PackRequest {
            options,
            reproducible: false,
            seven_z_dictionary_bytes: None,
        },
        None,
    )
}

/// Split-volume form used by the volume examples and the split fixtures; a CLI caller reaches the
/// same body through [`pack_archive_request`], which is why the binary build has no caller of it.
#[allow(dead_code)] // examples/split_probe.rs and `extract`'s split tests build through this shape
pub fn pack_archive_split_with_options(
    dest_path: &Path,
    sources: &[PathBuf],
    options: PackOptions,
    volume_bytes: u64,
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    pack_archive_request(
        dest_path,
        sources,
        PackRequest {
            options,
            reproducible: false,
            seven_z_dictionary_bytes: None,
        },
        Some(volume_bytes),
    )
}

/// Write the archive to a sink that is not a file: the `pack --to-stdout` body.
///
/// The contract of this destination, which a file destination does not have:
///
/// * no staging and no commit — there is no destination to replace, rename, or delete, and no
///   rollback is possible;
/// * a failure can only be reported after part of the archive may already have reached the
///   consumer, so the error is the report and the emitted prefix is unrecoverable;
/// * it is a real stream — nothing spools the whole archive to make this call easier, so the
///   formats that have to seek are refused up front with a message instead of being buffered.
///
/// The sink is any forward-only `Write` with a `'static` life, which includes the process stdout
/// lock a pipe gives us and a test buffer.
pub fn pack_archive_to_sink<W: Write + 'static>(
    sink: W,
    sources: &[PathBuf],
    request: PackRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    // A destination without a name has no extension to read, so ZIP is the default it inherits
    // from `resolve_pack_format`.
    let format = request.options.format.unwrap_or(ArchiveFormat::Zip);
    validate_level(format, request.options.level)?;
    let plan = plan_archive(sources, &request, &[])?;
    write_forward_archive(sink, format, &plan, &request.options)
}

/// The directory the output archive and its volumes are written into.
///
/// `Path::parent` reports an empty path for a bare file name (`rel.7z`), and an empty path cannot be
/// opened as a directory (`fs::read_dir("")` fails with the OS "path not found"), even though the
/// current directory is exactly what that name denotes. Normalising here — once, in the single place
/// that answers "which directory holds this output" — keeps every caller off that empty path.
fn destination_dir(dest_path: &Path) -> &Path {
    match dest_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Compress a source tree into `dest_path` (and, for 7z, an optional numbered volume set).
pub fn pack_archive_request(
    dest_path: &Path,
    sources: &[PathBuf],
    request: PackRequest,
    volume_bytes: Option<u64>,
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    if sources.is_empty() {
        return Err("No source files or directories specified for packing".into());
    }

    for source in sources {
        if !source.exists() {
            return Err(format!("Source path '{}' does not exist", source.display()).into());
        }
    }

    let options = &request.options;
    let format = resolve_pack_format(dest_path, options.format)?;

    // Direct collision guard
    if dest_path.exists() {
        if let Ok(cdest) = dest_path.canonicalize() {
            for source in sources {
                if let Ok(csource) = source.canonicalize() {
                    if csource == cdest {
                        return Err(format!(
                            "Cannot pack archive '{}' into itself",
                            dest_path.display()
                        )
                        .into());
                    }
                }
            }
        }
    }

    // Reject an unsupported level before any staging file exists, using the same producer that
    // validates it inside the codec factory.
    validate_level(format, options.level)?;

    if let Some(vb) = volume_bytes {
        if format != ArchiveFormat::SevenZ {
            return Err("Volume splitting is only supported for 7z output".into());
        }
        if vb == 0 {
            return Err("Volume size must be greater than zero".into());
        }
    }

    let parent = destination_dir(dest_path);
    fs::create_dir_all(parent)?;

    let file_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_path = parent.join(format!(
        ".mothzip_pack_{}_{}_{}",
        std::process::id(),
        staging_token,
        file_name
    ));

    // 1. Collect inventory BEFORE creating staging file on disk
    let existing_volumes = if volume_bytes.is_some() {
        existing_volume_set(dest_path)?
    } else {
        Vec::new()
    };
    let mut excluded_paths = existing_volumes.clone();
    excluded_paths.push(dest_path.to_path_buf());
    let plan = plan_archive(sources, &request, &excluded_paths)?;
    let total_files = plan.total_files;
    let total_uncompressed_bytes = plan.total_uncompressed_bytes;

    let mut created_staged: Vec<PathBuf> = Vec::new();
    let staged_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging_path)?;

    // 2. Write complete archive to staging path (single writer pass over source bytes).
    let write_res = (|| -> Result<PackSummary, Box<dyn std::error::Error>> {
        match format {
            ArchiveFormat::SevenZ => write_7z_archive(staged_file, &plan, options)?,
            forward_only => write_forward_archive(staged_file, forward_only, &plan, options)?,
        }
        // The writers own and finish their sink, so the finished staging file is reopened once to
        // make its bytes durable before the commit renames it onto the destination.
        fs::OpenOptions::new()
            .write(true)
            .open(&staging_path)?
            .sync_all()?;

        if let Some(vb) = volume_bytes {
            let staged_volumes = split_7z_volumes(&staging_path, dest_path, vb)?;
            created_staged.extend(staged_volumes.iter().map(|(staged, _)| staged.clone()));

            let mut stale = existing_volumes
                .iter()
                .skip(staged_volumes.len())
                .cloned()
                .collect::<Vec<_>>();
            if dest_path.exists() {
                stale.push(dest_path.to_path_buf());
            }
            let volume_sizes: Vec<u64> = staged_volumes
                .iter()
                .map(|(staged, _)| fs::metadata(staged).map(|metadata| metadata.len()))
                .collect::<io::Result<_>>()?;
            super::commit::commit_files_with_removals(&staged_volumes, &stale)?;
            let _ = fs::remove_file(&staging_path);

            let volumes: Vec<PathBuf> = staged_volumes
                .iter()
                .map(|(_, dest)| dest.clone())
                .collect();
            let compressed_bytes = volume_sizes.iter().sum();
            let ratio_num = if compressed_bytes > 0 {
                total_uncompressed_bytes as f64 / compressed_bytes as f64
            } else {
                1.0
            };
            let compression_ratio = format!("{:.1}x", ratio_num);
            let first = volumes
                .first()
                .ok_or("7z encoder produced no volumes")?
                .clone();

            Ok(PackSummary {
                archive_path: first,
                total_files,
                total_uncompressed_bytes,
                compressed_bytes,
                compression_ratio,
                volumes,
                volume_sizes,
            })
        } else {
            super::commit::commit_files(&[(staging_path.clone(), dest_path.to_path_buf())])?;
            let compressed_bytes = fs::metadata(dest_path)?.len();
            let ratio_num = if compressed_bytes > 0 {
                total_uncompressed_bytes as f64 / compressed_bytes as f64
            } else {
                1.0
            };
            let compression_ratio = format!("{:.1}x", ratio_num);

            Ok(PackSummary {
                archive_path: dest_path.to_path_buf(),
                total_files,
                total_uncompressed_bytes,
                compressed_bytes,
                compression_ratio,
                volumes: Vec::new(),
                volume_sizes: Vec::new(),
            })
        }
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&staging_path);
        for staged in &created_staged {
            let _ = fs::remove_file(staged);
        }
        return Err(e);
    }

    write_res
}

/// Split a complete staged archive into numbered volume chunks. Each volume is written to a
/// staging path in the same directory as `dest_path` (same filesystem, so the follow-up commit
/// can atomically rename it onto the final volume path). Returns `(staged, final)` pairs.
fn split_7z_volumes(
    staging_path: &Path,
    dest_path: &Path,
    volume_bytes: u64,
) -> Result<Vec<(PathBuf, PathBuf)>, Box<dyn std::error::Error>> {
    let parent = destination_dir(dest_path);
    let base_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive")
        .to_string();
    let staging_token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let len = fs::metadata(staging_path)?.len();
    let mut src = File::open(staging_path)?;
    let mut written_total = 0u64;
    let mut pairs = Vec::new();
    let split: Result<(), Box<dyn std::error::Error>> = (|| {
        while written_total < len {
            let index = pairs.len() + 1;
            let staged_vol = parent.join(format!(
                ".mothzip_pack_{}_{}_vol_{}",
                std::process::id(),
                staging_token,
                index
            ));
            let final_vol = parent.join(format!("{}.{:03}", base_name, index));
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged_vol)?;
            pairs.push((staged_vol, final_vol));
            let expected = volume_bytes.min(len - written_total);
            let copied = io::copy(&mut Read::by_ref(&mut src).take(expected), &mut out)?;
            if copied != expected {
                return Err("Completed 7z archive changed while splitting".into());
            }
            written_total += copied;
            out.sync_all()?;
        }
        Ok(())
    })();
    if let Err(error) = split {
        for (staged, _) in &pairs {
            let _ = fs::remove_file(staged);
        }
        return Err(error);
    }
    Ok(pairs)
}

/// Enumerate existing tail volumes beyond `new_count` in the exact numbered set of `dest_path`
/// (e.g. `NAME.7z.003` when the new set has 2 volumes). Only regular files are collected; a
/// directory, symlink, or reparse point at a tail index is a hard error rather than a deletion.
fn existing_volume_set(dest_path: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let parent = destination_dir(dest_path);
    let base_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    let mut parts = Vec::new();
    for entry in fs::read_dir(parent)? {
        let path = entry?.path();
        let Some(suffix) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(index) = suffix.parse::<usize>() else {
            continue;
        };
        if index == 0 || format!("{index:03}") != suffix {
            continue;
        }
        let candidate = parent.join(format!("{base_name}.{suffix}"));
        let expected = match candidate.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if path.canonicalize()? != expected {
            continue;
        }
        if !is_regular_file_for_removal(&fs::symlink_metadata(&path)?) {
            return Err(
                format!("Refusing a non-regular archive volume: {}", path.display()).into(),
            );
        }
        parts.push((index, path));
    }
    parts.sort_by_key(|(index, _)| *index);
    for (position, (index, _)) in parts.iter().enumerate() {
        if *index != position + 1 {
            return Err("Existing volume set is incomplete; original files retained".into());
        }
    }
    Ok(parts.into_iter().map(|(_, path)| path).collect())
}

/// A tail volume / removal target must be a regular file (not directory, symlink, reparse point).
fn is_regular_file_for_removal(meta: &fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

/// Write one archive body into a sink that only has to accept bytes.
///
/// Everything but 7z is forward-only: ZIP in stream mode (member sizes go into a data descriptor
/// after the data instead of a seek back into the local header), plain TAR, and the single-stream
/// codecs. That is what lets a file and a pipe receive the same bytes from the same writer, and it
/// is also why 7z — which rewrites its signature header at position 0 when it finishes — is refused
/// here rather than buffered.
fn write_forward_archive<W: Write + 'static>(
    sink: W,
    format: ArchiveFormat,
    plan: &ArchivePlan,
    options: &PackOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        ArchiveFormat::Zip => write_zip_archive(sink, plan, options),
        ArchiveFormat::Tar => write_tar_archive(sink, plan, None, options),
        ArchiveFormat::TarStream(codec) => write_tar_archive(sink, plan, Some(codec), options),
        ArchiveFormat::Stream(codec) => write_stream_archive(sink, plan, codec, options),
        ArchiveFormat::SevenZ => Err(
            "7Z rewrites its signature header when it finishes, so it needs a seekable output; \
             write it to an archive path instead of stdout"
                .into(),
        ),
        ArchiveFormat::Rar => Err(RAR_NOT_IMPLEMENTED.into()),
    }
}

fn write_zip_archive<W: Write>(
    sink: W,
    plan: &ArchivePlan,
    options: &PackOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    // Stream mode is the single ZIP writer for both destinations: it never seeks, so the bytes do
    // not depend on whether they land in a file or a pipe.
    let mut zip = ZipWriter::new_stream(sink);

    let compression = match options.level {
        Some(0) => CompressionMethod::Stored,
        _ => CompressionMethod::Deflated,
    };

    let mut base_options = SimpleFileOptions::default()
        .compression_method(compression)
        .unix_permissions(0o755);
    if compression == CompressionMethod::Deflated {
        base_options = base_options.compression_level(options.level.map(i64::from));
    }

    if let Some(pass) = options.password {
        base_options = base_options.with_aes_encryption(zip::AesMode::Aes256, pass);
    }

    for item in &plan.items {
        let mut opts = base_options;
        if let Some(seconds) = plan.fixed_epoch_seconds {
            opts = opts.last_modified_time(reproducible_zip_time(seconds));
        } else if let Some(mtime) = item.mtime {
            if let Some(dt) = system_time_to_zip_datetime(mtime) {
                opts = opts.last_modified_time(dt);
            }
        }

        if item.is_dir {
            zip.add_directory(format!("{}/", item.entry_name), opts)?;
        } else {
            zip.start_file(&item.entry_name, opts)?;
            let mut f = File::open(&item.disk_path).map_err(|e| {
                format!(
                    "Failed to open file '{}' for packing: {}",
                    item.disk_path.display(),
                    e
                )
            })?;
            io::copy(&mut f, &mut zip)?;
        }
    }

    zip.finish()?;
    Ok(())
}

/// Encoder chain for the 7z blocks: optional AES, then LZMA2 at `level` or COPY at level 0.
///
/// One chain applies to every block. The dictionary is the preset's, because it is the archive's
/// compression/ratio parameter: `Lzma2Options::from_level` resolves it through `lzma_rust2`'s
/// `PRESET_TO_DICT_SIZE` (8 MiB at the default level 5), and every dictionary byte costs the match
/// finder about eleven bytes of state (see the 7z row of the pack-output handoff).
pub(crate) fn seven_z_encoder_chain(
    level: u32,
    password: Option<&str>,
    dictionary_bytes: Option<u32>,
) -> Vec<sevenz_rust2::EncoderConfiguration> {
    let mut methods = Vec::new();
    if let Some(pass) = password {
        methods.push(sevenz_rust2::EncoderConfiguration::from(
            sevenz_rust2::encoder_options::AesEncoderOptions::new(sevenz_rust2::Password::from(
                pass,
            )),
        ));
    }
    if level == 0 {
        methods.push(sevenz_rust2::EncoderConfiguration::new(
            sevenz_rust2::EncoderMethod::COPY,
        ));
    } else {
        let mut lzma2 = sevenz_rust2::encoder_options::Lzma2Options::from_level(level);
        if let Some(dictionary) = dictionary_bytes {
            lzma2.set_dictionary_size(dictionary);
        }
        methods.push(
            sevenz_rust2::EncoderConfiguration::new(sevenz_rust2::EncoderMethod::LZMA2)
                .with_options(sevenz_rust2::encoder_options::EncoderOptions::Lzma2(lzma2)),
        );
    }
    methods
}

fn write_7z_archive(
    file: File,
    plan: &ArchivePlan,
    options: &PackOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = sevenz_rust2::ArchiveWriter::new(file)
        .map_err(|e| format!("Failed to create 7z writer: {}", e))?;

    let level = options.level.unwrap_or(5).clamp(0, 9);
    // Set once, before the first entry: the same chain also decides whether the 7z header itself
    // is encrypted, so it must be in place before `finish` encodes that header.
    writer.set_content_methods(seven_z_encoder_chain(
        level,
        options.password,
        plan.seven_z_dictionary_bytes,
    ));

    for item in &plan.items {
        let mut sz_entry =
            sevenz_rust2::ArchiveEntry::from_path(&item.disk_path, item.entry_name.clone());
        if let Some(seconds) = plan.fixed_epoch_seconds {
            pin_seven_z_entry_time(&mut sz_entry, seconds);
        }
        if item.is_dir {
            writer
                .push_archive_entry(sz_entry, None::<&mut File>)
                .map_err(|e| format!("Failed to push 7z directory: {}", e))?;
        } else {
            let mut f = File::open(&item.disk_path)?;
            writer
                .push_archive_entry(sz_entry, Some(&mut f))
                .map_err(|e| format!("Failed to push 7z file: {}", e))?;
        }
    }

    writer
        .finish()
        .map_err(|e| format!("Failed to finalize 7z archive: {}", e))?;
    Ok(())
}

fn write_tar_archive<W: Write + 'static>(
    sink: W,
    plan: &ArchivePlan,
    codec: Option<super::codec::StreamCodec>,
    options: &PackOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    if options.password.is_some() {
        let name = match codec {
            Some(codec) => format!("TAR.{}", codec.suffix().to_ascii_uppercase()),
            None => "TAR".to_string(),
        };
        return Err(format!("{} format does not support password encryption", name).into());
    }

    match codec {
        Some(codec) => {
            let mut encoder = super::codec::encoder(sink, codec, options.level)?;
            {
                let mut builder = tar::Builder::new(&mut *encoder);
                write_tar_entries(&mut builder, &plan.items, plan.fixed_epoch_seconds)?;
            }
            encoder.finish()?;
        }
        None => {
            let mut builder = tar::Builder::new(sink);
            write_tar_entries(&mut builder, &plan.items, plan.fixed_epoch_seconds)?;
            builder.into_inner()?;
        }
    }
    Ok(())
}

fn write_tar_entries<W: io::Write>(
    builder: &mut tar::Builder<W>,
    items: &[SourceTreeItem],
    fixed_epoch_seconds: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    for item in items {
        let mut header = tar::Header::new_gnu();
        if let Some(seconds) = fixed_epoch_seconds {
            header.set_mtime(seconds);
        } else if let Some(mtime) = item.mtime {
            if let Ok(dur) = mtime.duration_since(UNIX_EPOCH) {
                header.set_mtime(dur.as_secs());
            }
        }
        if item.is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            let mut dir_name = item.entry_name.clone();
            if !dir_name.ends_with('/') {
                dir_name.push('/');
            }
            builder.append_data(&mut header, dir_name, io::empty())?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(item.size);
            header.set_mode(0o644);
            header.set_cksum();
            let mut f = File::open(&item.disk_path)?;
            builder.append_data(&mut header, &item.entry_name, &mut f)?;
        }
    }
    builder.finish()?;
    Ok(())
}

/// Compress one already-open byte stream (stdin) into a single-stream archive.
///
/// Containers are refused: ZIP, 7z and TAR need per-entry names, sizes and timestamps that a pipe
/// does not carry, so a container built from stdin would have to invent them. A single stream
/// needs none of that — it stores exactly the bytes the caller pipes in.
pub fn pack_stream_from_reader<R: Read, W: Write + 'static>(
    reader: R,
    sink: W,
    codec: super::codec::StreamCodec,
    level: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = reader;
    let mut encoder = super::codec::encoder(sink, codec, level)?;
    io::copy(&mut reader, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

/// Write one member as a bare compressed stream (`.gz`, `.zst`, `.xz`, `.lz4`, `.br`).
fn write_stream_archive<W: Write + 'static>(
    sink: W,
    plan: &ArchivePlan,
    codec: super::codec::StreamCodec,
    options: &PackOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = codec.name();
    if options.password.is_some() {
        return Err(format!("{} format does not support password encryption", name).into());
    }
    let file_items: Vec<_> = plan.items.iter().filter(|item| !item.is_dir).collect();
    if file_items.len() != 1 {
        return Err(format!(
            "{} format only supports compressing a single file (use .tar.{} for multiple files or directories)",
            name,
            codec.suffix()
        )
        .into());
    }
    let item = file_items[0];
    let mut encoder = super::codec::encoder(sink, codec, options.level)?;
    let mut source = File::open(&item.disk_path)?;
    io::copy(&mut source, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 7z dictionary is the memory lever, so it must actually reach the encoder: a smaller
    /// dictionary changes the output bytes while both archives stay readable.
    #[test]
    fn test_seven_z_dictionary_changes_output_and_stays_readable() {
        let dir = PathBuf::from("target").join(format!(
            "test_7z_dict_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Repetitive text so a larger dictionary has something to win back.
        let body: String = (0..4000)
            .map(|index| format!("record {index} mothzip dictionary probe\n"))
            .collect();
        let source = dir.join("sample.txt");
        fs::write(&source, body.as_bytes()).unwrap();

        let wide = dir.join("wide.7z");
        let narrow = dir.join("narrow.7z");
        pack_archive_request(
            &wide,
            std::slice::from_ref(&source),
            PackRequest {
                options: PackOptions {
                    format: Some(ArchiveFormat::SevenZ),
                    ..Default::default()
                },
                reproducible: false,
                seven_z_dictionary_bytes: Some(16 * 1024 * 1024),
            },
            None,
        )
        .expect("wide dictionary pack");
        pack_archive_request(
            &narrow,
            std::slice::from_ref(&source),
            PackRequest {
                options: PackOptions {
                    format: Some(ArchiveFormat::SevenZ),
                    ..PackOptions::default()
                },
                reproducible: false,
                seven_z_dictionary_bytes: Some(1024 * 1024),
            },
            None,
        )
        .expect("narrow dictionary pack");

        let wide_bytes = fs::read(&wide).unwrap();
        let narrow_bytes = fs::read(&narrow).unwrap();
        // The dictionary size is part of the encoded stream properties, so the two archives must
        // differ; identical bytes would mean the option never reached the encoder. The memory and
        // ratio effect of the option is measured end to end in the release evidence, not here.
        assert_ne!(
            wide_bytes, narrow_bytes,
            "the dictionary size did not reach the 7z encoder"
        );
        for archive in [&wide, &narrow] {
            let report = crate::core::inspect::test_archive_integrity(
                archive,
                None,
                crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
            )
            .expect("7z integrity");
            assert!(report.passed, "{} must stay readable", archive.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pipe has no member metadata, so `--from-stdin` must compress exactly the bytes it reads
    /// and hand them back through the same single-stream decoder.
    /// `C:\` must not lose its separator: `C:` is the current directory on that drive, so the
    /// pack would silently archive a different tree than the one the caller named.
    #[test]
    fn test_trailing_separator_keeps_a_drive_root_absolute() {
        let drive_root = without_trailing_separators(Path::new("C:\\"));
        assert_eq!(drive_root, Path::new("C:\\"));
        assert!(
            drive_root.is_absolute(),
            "a drive root must stay absolute, got {drive_root:?}"
        );
        assert_eq!(
            without_trailing_separators(Path::new("C:\\Users\\me\\")),
            Path::new("C:\\Users\\me")
        );
        assert_eq!(
            without_trailing_separators(Path::new("/")),
            Path::new("/"),
            "a bare separator is still the root"
        );
    }

    /// ZIP and 7z accept 0..=9 only. 7z used to clamp anything higher without saying so, and ZIP
    /// failed after its staging file already existed.
    #[test]
    fn test_archive_level_range_is_checked_before_staging() {
        for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZ] {
            assert!(validate_level(format, Some(0)).is_ok());
            assert!(validate_level(format, Some(9)).is_ok());
            for level in [10, 15, 22] {
                let error = validate_level(format, Some(level))
                    .expect_err("out-of-range archive level must be refused");
                assert!(
                    error.to_string().contains("0..=9"),
                    "{} level {level} message was {error}",
                    format.name()
                );
            }
        }
        // TAR carries no level of its own; a codec container is validated against its codec.
        assert!(validate_level(ArchiveFormat::Tar, Some(5)).is_ok());
        assert!(validate_level(
            ArchiveFormat::TarStream(super::super::codec::StreamCodec::Gzip),
            Some(9)
        )
        .is_ok());
        assert!(validate_level(
            ArchiveFormat::TarStream(super::super::codec::StreamCodec::Gzip),
            Some(10)
        )
        .is_err());
    }

    #[test]
    fn test_stream_from_reader_roundtrips_through_the_codec() {
        let payload = b"stdin payload with no member name\n".repeat(64);
        for codec in [
            crate::core::codec::StreamCodec::Gzip,
            crate::core::codec::StreamCodec::Zstd,
            crate::core::codec::StreamCodec::Xz,
            crate::core::codec::StreamCodec::Lz4,
            crate::core::codec::StreamCodec::Brotli,
        ] {
            let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = SharedSink(buffer.clone());
            pack_stream_from_reader(&payload[..], sink, codec, None)
                .unwrap_or_else(|error| panic!("{} pack failed: {error}", codec.name()));
            let compressed = buffer.lock().unwrap().clone();
            let file = {
                let path = std::env::temp_dir().join(format!(
                    "mothzip_stream_{}_{}.{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    codec.suffix()
                ));
                fs::write(&path, &compressed).unwrap();
                path
            };
            let mut decoder = crate::core::codec::decoder(File::open(&file).unwrap(), codec)
                .unwrap_or_else(|error| panic!("{} decoder: {error}", codec.name()));
            let mut restored = Vec::new();
            io::copy(&mut decoder, &mut restored)
                .unwrap_or_else(|error| panic!("{} decode failed: {error}", codec.name()));
            let _ = fs::remove_file(&file);
            assert_eq!(restored, payload, "{} roundtrip mismatch", codec.name());
        }
    }

    /// Collects a writer into memory so a codec can be exercised without a real file.
    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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
        assert!(
            !temp_dest.exists(),
            "Target file must not be created on error"
        );
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

    #[test]
    fn test_pack_7z_roundtrip() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_7z_roundtrip_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);
        let src_file = temp_dir.join("hello.txt");
        fs::write(&src_file, b"Hello from 7z pack engine!").unwrap();

        let sz_dest = temp_dir.join("test.7z");
        let summary =
            pack_archive(&sz_dest, std::slice::from_ref(&src_file)).expect("7z pack success");
        assert_eq!(summary.total_files, 1);
        assert!(sz_dest.exists());

        let out_dir = temp_dir.join("out");
        let ext_res = crate::core::extract::extract_archive(
            &sz_dest,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("7z extract success");
        assert_eq!(ext_res.extracted_files, 1);
        let extracted_file = out_dir.join("hello.txt");
        assert_eq!(
            fs::read(extracted_file).unwrap(),
            b"Hello from 7z pack engine!"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_pack_tar_gz_roundtrip() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_targz_roundtrip_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&temp_dir);
        let src_file = temp_dir.join("sample.txt");
        fs::write(&src_file, b"Sample TAR.GZ content").unwrap();

        let tar_dest = temp_dir.join("archive.tar.gz");
        let summary =
            pack_archive(&tar_dest, std::slice::from_ref(&src_file)).expect("tar.gz pack success");
        assert_eq!(summary.total_files, 1);
        assert!(tar_dest.exists());

        // Verify magic bytes: must start with 1F 8B (GZIP)
        let magic = fs::read(&tar_dest).unwrap();
        assert!(magic.len() >= 2);
        assert_eq!(magic[0], 0x1f);
        assert_eq!(magic[1], 0x8b);

        let out_dir = temp_dir.join("out");
        let ext_res = crate::core::extract::extract_archive(
            &tar_dest,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("tar.gz extract success");
        assert_eq!(ext_res.extracted_files, 1);
        assert_eq!(
            fs::read(out_dir.join("sample.txt")).unwrap(),
            b"Sample TAR.GZ content"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_pack_rejects_duplicate_entry_name_across_sources() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_duplicate_names_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = temp_dir.join("first");
        let second = temp_dir.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("same.txt"), b"FIRST").unwrap();
        fs::write(second.join("same.txt"), b"SECOND").unwrap();

        let dest = temp_dir.join("out.zip");
        let error = pack_archive(&dest, &[first.join("same.txt"), second.join("same.txt")])
            .expect_err("a repeated member name must be refused");
        assert_eq!(
            error.to_string(),
            "Duplicate archive entry name 'same.txt' from multiple sources"
        );
        assert!(!dest.exists(), "the destination must not be created");

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_destination_dir_normalizes_a_bare_file_name() {
        assert_eq!(destination_dir(Path::new("rel.7z")), Path::new("."));
        assert_eq!(destination_dir(Path::new("./rel.7z")), Path::new("."));
        assert_eq!(destination_dir(Path::new("sub/rel.7z")), Path::new("sub"));
    }

    #[test]
    fn test_pack_inside_self_exclusion() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_inside_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src_dir = temp_dir.join("source");
        let _ = fs::create_dir_all(&src_dir);
        fs::write(src_dir.join("hello.txt"), b"only one file").unwrap();

        // Pack destination is inside the source directory: source/inside.7z
        let dest = src_dir.join("inside.7z");
        let summary =
            pack_archive(&dest, std::slice::from_ref(&src_dir)).expect("pack inside success");

        // Must only pack hello.txt, NOT inside.7z or any staging file!
        assert_eq!(summary.total_files, 1);

        let out_dir = temp_dir.join("out");
        let ext_res = crate::core::extract::extract_archive(
            &dest,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("extract inside success");
        assert_eq!(ext_res.extracted_files, 1);
        assert_eq!(
            fs::read(out_dir.join("source").join("hello.txt")).unwrap(),
            b"only one file"
        );
        assert!(!out_dir.join("source").join("inside.7z").exists());

        let _ = fs::remove_dir_all(temp_dir);
    }

    /// Scratch directory under `target/`, unique per test process and call.
    fn scratch_dir(name: &str) -> PathBuf {
        let path = PathBuf::from("target").join(format!(
            "{}_{}_{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn reproducible_request(format: ArchiveFormat) -> PackRequest<'static> {
        PackRequest {
            seven_z_dictionary_bytes: None,
            options: PackOptions {
                format: Some(format),
                ..Default::default()
            },
            reproducible: true,
        }
    }

    fn member_names(archive: &Path) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
        (0..zip.len())
            .map(|index| zip.by_index(index).unwrap().name().unwrap().to_string())
            .collect()
    }

    #[test]
    fn test_reproducible_zip_time_uses_utc_and_clamps_to_the_dos_floor() {
        // ZIP cannot express a date before 1980, so epoch 0 is the format floor, not 1970.
        assert_eq!(reproducible_zip_time(0), zip::DateTime::DEFAULT);
        assert_eq!(
            reproducible_zip_time(1_700_000_000),
            zip::DateTime::from_date_and_time(2023, 11, 14, 22, 13, 20).unwrap()
        );
        // The instant, not a wall clock: the same seconds give the same field everywhere.
        assert_eq!(
            reproducible_zip_time(1_700_000_000),
            reproducible_zip_time(1_700_000_000)
        );
    }

    #[test]
    fn test_parse_epoch_seconds_rejects_non_seconds() {
        assert_eq!(parse_epoch_seconds("1700000000"), Ok(1_700_000_000));
        assert_eq!(parse_epoch_seconds(" 42\n"), Ok(42));
        assert!(parse_epoch_seconds("2023-11-14T22:13:20Z").is_err());
        assert!(parse_epoch_seconds("-5").is_err());
    }

    #[test]
    fn test_reproducible_sorts_entries_by_member_name() {
        let temp_dir = scratch_dir("test_reproducible_order");
        let tree = temp_dir.join("tree");
        fs::create_dir_all(tree.join("dir")).unwrap();
        for name in ["zeta.txt", "alpha.txt", "dir/beta.txt", "dir/aaa.txt"] {
            fs::write(tree.join(name), name.as_bytes()).unwrap();
        }

        let dest = temp_dir.join("out.zip");
        pack_archive_request(
            &dest,
            std::slice::from_ref(&tree),
            reproducible_request(ArchiveFormat::Zip),
            None,
        )
        .expect("reproducible pack success");

        let names = member_names(&dest);
        let mut sorted = names.clone();
        sorted.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        assert_eq!(names, sorted, "entries must be written in byte order");

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_reproducible_is_byte_identical_across_runs_and_mtimes() {
        let temp_dir = scratch_dir("test_reproducible_bytes");
        let tree = temp_dir.join("tree");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::write(tree.join("b.txt"), b"SECOND").unwrap();
        fs::write(tree.join("a.txt"), b"FIRST").unwrap();
        fs::write(tree.join("nested/c.bin"), vec![7u8; 4096]).unwrap();

        let formats = [
            (ArchiveFormat::Zip, "out.zip"),
            (ArchiveFormat::SevenZ, "out.7z"),
            (ArchiveFormat::Tar, "out.tar"),
            (
                ArchiveFormat::TarStream(super::super::codec::StreamCodec::Gzip),
                "out.tar.gz",
            ),
            (
                ArchiveFormat::TarStream(super::super::codec::StreamCodec::Zstd),
                "out.tar.zst",
            ),
        ];

        for (format, name) in formats {
            let first = temp_dir.join(format!("first-{name}"));
            let second = temp_dir.join(format!("second-{name}"));
            pack_archive_request(
                &first,
                std::slice::from_ref(&tree),
                reproducible_request(format),
                None,
            )
            .expect("first reproducible pack");
            pack_archive_request(
                &second,
                std::slice::from_ref(&tree),
                reproducible_request(format),
                None,
            )
            .expect("second reproducible pack");
            assert_eq!(
                fs::read(&first).unwrap(),
                fs::read(&second).unwrap(),
                "{name}: two runs over the same tree must be byte identical"
            );

            // Touching every source must not change a single byte.
            let later = SystemTime::now() + std::time::Duration::from_secs(9_999);
            for entry in WalkDir::new(&tree).into_iter().skip(1) {
                let entry = entry.unwrap();
                if entry.file_type().is_file() {
                    File::options()
                        .write(true)
                        .open(entry.path())
                        .unwrap()
                        .set_modified(later)
                        .unwrap();
                }
            }
            let third = temp_dir.join(format!("third-{name}"));
            pack_archive_request(
                &third,
                std::slice::from_ref(&tree),
                reproducible_request(format),
                None,
            )
            .expect("third reproducible pack");
            assert_eq!(
                fs::read(&first).unwrap(),
                fs::read(&third).unwrap(),
                "{name}: a source mtime change must not change the archive"
            );
        }

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_pack_to_sink_matches_the_file_destination_byte_for_byte() {
        let temp_dir = scratch_dir("test_sink_matches_file");
        let tree = temp_dir.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("one.txt"), b"ONE").unwrap();
        fs::write(tree.join("two.bin"), vec![3u8; 2048]).unwrap();

        for (format, name) in [
            (ArchiveFormat::Zip, "out.zip"),
            (ArchiveFormat::Tar, "out.tar"),
            (
                ArchiveFormat::TarStream(super::super::codec::StreamCodec::Zstd),
                "out.tar.zst",
            ),
        ] {
            let dest = temp_dir.join(name);
            pack_archive_request(
                &dest,
                std::slice::from_ref(&tree),
                reproducible_request(format),
                None,
            )
            .expect("file pack success");

            // Same options, forward-only sink: the destination must not change the bytes.
            let sink_path = temp_dir.join(format!("sink-{name}"));
            let sink = File::create(&sink_path).unwrap();
            pack_archive_to_sink(
                sink,
                std::slice::from_ref(&tree),
                reproducible_request(format),
            )
            .expect("sink pack success");
            assert_eq!(
                fs::read(&dest).unwrap(),
                fs::read(&sink_path).unwrap(),
                "{name}: a pipe and a file must receive the same archive"
            );
        }

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_pack_to_sink_refuses_formats_that_must_seek() {
        let temp_dir = scratch_dir("test_sink_refuses");
        let source = temp_dir.join("one.txt");
        fs::write(&source, b"ONE").unwrap();

        for format in [ArchiveFormat::SevenZ, ArchiveFormat::Rar] {
            let sink_path = temp_dir.join("refused.bin");
            let sink = File::create(&sink_path).unwrap();
            let error = pack_archive_to_sink(
                sink,
                std::slice::from_ref(&source),
                reproducible_request(format),
            )
            .expect_err("a seek-dependent format must be refused");
            assert!(
                error.to_string().contains("7Z") || error.to_string().contains("RAR"),
                "unexpected message: {error}"
            );
            assert_eq!(
                fs::metadata(&sink_path).unwrap().len(),
                0,
                "the refusal must happen before any byte is emitted"
            );
        }

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_pack_to_sink_rejects_an_out_of_range_level_before_writing() {
        let temp_dir = scratch_dir("test_sink_level");
        let source = temp_dir.join("one.txt");
        fs::write(&source, b"ONE").unwrap();
        let sink_path = temp_dir.join("refused.bin");
        let sink = File::create(&sink_path).unwrap();
        let options = PackOptions {
            level: Some(10),
            format: Some(ArchiveFormat::Stream(
                super::super::codec::StreamCodec::Gzip,
            )),
            ..Default::default()
        };
        let request = PackRequest {
            options,
            reproducible: false,
            seven_z_dictionary_bytes: None,
        };
        let error = pack_archive_to_sink(sink, std::slice::from_ref(&source), request)
            .expect_err("an out-of-range gzip level must be refused");
        assert!(
            error.to_string().contains("GZIP compression level"),
            "{error}"
        );
        assert_eq!(fs::metadata(&sink_path).unwrap().len(), 0);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_forward_zip_is_readable_by_its_own_reader() {
        let temp_dir = scratch_dir("test_forward_zip_readable");
        let tree = temp_dir.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("member.txt"), b"FORWARD ONLY").unwrap();

        let dest = temp_dir.join("out.zip");
        pack_archive_request(
            &dest,
            std::slice::from_ref(&tree),
            reproducible_request(ArchiveFormat::Zip),
            None,
        )
        .expect("pack success");

        let out_dir = temp_dir.join("out");
        let extracted = crate::core::extract::extract_archive(
            &dest,
            &out_dir,
            false,
            false,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("stream-mode zip must extract");
        assert_eq!(extracted.extracted_files, 1);
        assert_eq!(
            fs::read(out_dir.join("tree").join("member.txt")).unwrap(),
            b"FORWARD ONLY"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }
}
