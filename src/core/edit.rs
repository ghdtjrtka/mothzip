//! One producer for archive modification: add/replace, delete, and rename members.
//!
//! ZIP edits copy the untouched members as raw stored bytes (preserving name bytes,
//! compression, timestamps, and encryption). 7z edits decompress and re-encode each
//! retained member into a fresh archive, preserving names, directory flags,
//! timestamps, and the encryption method, but not the original compression bytes.
//! RAR and TAR/TGZ/GZIP archives are rejected explicitly.
//!
//! The replacement is staged on the same filesystem as the original, verified by
//! reading every payload, and committed through the shared [`crate::core::commit::FileTransaction`].
//! On any validation/write/commit failure the original archive is retained.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::catalog::{member_form, member_subtree_suffix, CatalogEntry};
use crate::core::format::ArchiveFormat;
use crate::core::pack::{collect_source_tree, SourceTreeItem, SourceTreePolicy};

/// A single edit operation. Root maps CLI verbs (`add`, `delete`, `rename`) to these.
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Add or replace members from the given source filesystem paths.
    Add { sources: Vec<PathBuf> },
    /// Delete literal members (a directory name deletes the whole subtree at slash boundaries).
    Delete { members: Vec<String> },
    /// Rename a member (a directory prefix renames every member under it).
    Rename { from: String, to: String },
}

/// Options controlling an archive edit.
#[derive(Debug, Clone)]
pub struct EditOptions<'a> {
    /// Password for encrypted archives. Required when any retained member is encrypted.
    pub password: Option<&'a str>,
    /// Compression level for added members (0..9). `None` uses the format default.
    pub level: Option<u32>,
    /// Uncompressed byte budget selected by the caller.
    pub max_bytes: u64,
}

impl Default for EditOptions<'_> {
    fn default() -> Self {
        Self {
            password: None,
            level: None,
            max_bytes: crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        }
    }
}

/// Summary of a completed edit.
#[derive(Debug, serde::Serialize)]
pub struct EditSummary {
    pub archive_path: PathBuf,
    pub total_files: usize,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatKind {
    Zip,
    SevenZ,
}

/// Delete/rename boundary match uses the one member-selection rule in the catalog:
/// the member itself, or a member under the requested directory at a slash boundary.
fn selects(member: &str, pattern: &str) -> bool {
    member_subtree_suffix(pattern, member).is_some()
}

#[derive(Debug, Clone)]
struct OrigInfo {
    index: usize,
    is_dir: bool,
    size: u64,
}

#[derive(Debug, Clone)]
enum MemberSource {
    Original { index: usize, orig_name: String },
    Added { disk_path: PathBuf },
}

#[derive(Debug, Clone)]
struct FinalMember {
    name: String,
    is_dir: bool,
    size: u64,
    source: MemberSource,
}

struct Plan {
    finals: Vec<FinalMember>,
    added_names: Vec<String>,
    deleted_names: Vec<String>,
    renamed_pairs: Vec<(String, String)>,
    enc_required: bool,
}

fn validate_added_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty member name".to_string());
    }
    if name.starts_with('/') {
        return Err(format!("absolute member name is not allowed: {name}"));
    }
    if name.contains(':') {
        return Err(format!(
            "alternate data stream / drive colon is not allowed: {name}"
        ));
    }
    if name.chars().any(|c| (c as u32) < 0x20) {
        return Err(format!("control character in member name: {name}"));
    }
    for comp in name.split('/') {
        if comp.is_empty() {
            return Err(format!("empty path component in '{name}'"));
        }
        if comp == "." || comp == ".." {
            return Err(format!("reserved path component '{comp}' in '{name}'"));
        }
        if comp.ends_with('.') || comp.ends_with(' ') {
            return Err(format!("trailing dot/space in component '{comp}'"));
        }
        if comp
            .chars()
            .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
        {
            return Err(format!("invalid character in member name '{name}'"));
        }
        let base = comp.split('.').next().unwrap_or("");
        let stem = Path::new(comp)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if crate::core::guard::is_reserved_dos_device(base)
            || crate::core::guard::is_reserved_dos_device(stem)
        {
            return Err(format!("reserved DOS device name in '{name}'"));
        }
    }
    Ok(())
}

/// Collect the members an add source path contributes, through the shared source-tree producer.
/// A file contributes its file name; a directory contributes itself and its whole subtree with
/// `/`-separated names relative to the directory's parent, exactly as pack names them.
fn collect_add_sources(
    sources: &[PathBuf],
    archive_path: &Path,
) -> Result<Vec<SourceTreeItem>, String> {
    let excluded = [archive_path.to_path_buf()];
    collect_source_tree(sources, &edit_tree_policy(&excluded))
}

/// edit's answer to every [`SourceTreePolicy`] decision.
fn edit_tree_policy(excluded: &[PathBuf]) -> SourceTreePolicy<'_> {
    SourceTreePolicy {
        excluded,
        excluded_descendant: |path| {
            Err(format!(
                "add source would include the target archive: {}",
                path.display()
            ))
        },
        // an archive path that cannot be resolved matches nothing, so it excludes nothing
        unresolvable_path: |_, _| None,
        excluded_source: |path| {
            format!(
                "add source is the target archive itself: {}",
                path.display()
            )
        },
        symlink_source: |path| format!("add source is a symlink: {}", path.display()),
        unreadable_source: |path, error| {
            format!(
                "add source '{}' cannot be opened: {}",
                path.display(),
                error
            )
        },
        special_source: |path| Some(format!("unsupported add source object: {}", path.display())),
        file_member_name: |path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .ok_or_else(|| format!("add source has a non-UTF8 name: {}", path.display()))
        },
        validate_name: validate_added_name,
        special_descendant: |source, path| {
            format!(
                "unsupported add source object inside '{}': {}",
                source.display(),
                path.display()
            )
        },
        duplicate_name: None,
    }
}

fn build_plan(
    catalog: &[CatalogEntry],
    ops: &[EditOp],
    archive_path: &Path,
    max_bytes: u64,
) -> Result<Plan, String> {
    let mut map: BTreeMap<String, OrigInfo> = BTreeMap::new();
    for e in catalog {
        let key = member_form(&e.name);
        if map.contains_key(&key) {
            return Err(format!(
                "original archive contains duplicate member name '{key}'"
            ));
        }
        map.insert(
            key,
            OrigInfo {
                index: e.index,
                is_dir: e.is_dir,
                size: e.size,
            },
        );
    }

    let mut delete_set: HashSet<String> = HashSet::new();
    let mut deleted_names: Vec<String> = Vec::new();
    for op in ops {
        if let EditOp::Delete { members } = op {
            for pat_raw in members {
                let pat = member_form(pat_raw);
                if pat.is_empty() {
                    return Err("empty delete member name".to_string());
                }
                let matched: Vec<String> =
                    map.keys().filter(|k| selects(k, &pat)).cloned().collect();
                if matched.is_empty() {
                    return Err(format!("member not found in archive: {pat}"));
                }
                for m in matched {
                    if delete_set.insert(m.clone()) {
                        deleted_names.push(m);
                    }
                }
            }
        }
    }

    // surviving: current canonical name -> (original canonical name, info)
    let mut surviving: BTreeMap<String, (String, OrigInfo)> = BTreeMap::new();
    for (k, v) in &map {
        if !delete_set.contains(k) {
            surviving.insert(k.clone(), (k.clone(), v.clone()));
        }
    }

    let mut renamed_pairs: Vec<(String, String)> = Vec::new();
    for op in ops {
        if let EditOp::Rename { from, to } = op {
            let f = member_form(from);
            let t = member_form(to);
            if f.is_empty() {
                return Err("empty rename source".to_string());
            }
            if t.is_empty() {
                return Err("empty rename destination".to_string());
            }
            let matched: Vec<String> = surviving
                .keys()
                .filter(|k| selects(k, &f))
                .cloned()
                .collect();
            if matched.is_empty() {
                return Err(format!("rename source not found in archive: {f}"));
            }
            let moving_out: HashSet<String> = matched.iter().cloned().collect();
            let mut remap: Vec<(String, String)> = Vec::new();
            for m in &matched {
                let suffix = member_subtree_suffix(&f, m)
                    .ok_or_else(|| format!("rename source '{f}' does not select member '{m}'"))?;
                let new_name = member_form(&format!("{t}{suffix}"));
                if new_name.is_empty() {
                    return Err(format!("rename produces an empty destination for '{m}'"));
                }
                remap.push((m.clone(), new_name));
            }
            for (m, new_name) in &remap {
                if new_name == m {
                    continue;
                }
                if surviving.contains_key(new_name)
                    && !moving_out.contains(new_name)
                    && !delete_set.contains(new_name)
                {
                    return Err(format!(
                        "rename destination conflicts with existing member '{new_name}'"
                    ));
                }
            }
            let mut next: BTreeMap<String, (String, OrigInfo)> = BTreeMap::new();
            for (k, v) in &surviving {
                if !moving_out.contains(k) {
                    next.insert(k.clone(), v.clone());
                }
            }
            for (m, new_name) in &remap {
                let (orig, info) = surviving
                    .get(m)
                    .ok_or_else(|| format!("internal rename source missing: {m}"))?
                    .clone();
                if next.contains_key(new_name) {
                    return Err(format!("rename creates a duplicate member '{new_name}'"));
                }
                next.insert(new_name.clone(), (orig, info));
                if new_name != m {
                    renamed_pairs.push((m.clone(), new_name.clone()));
                }
            }
            surviving = next;
        }
    }

    let mut added_names: Vec<String> = Vec::new();
    let mut add_items: Vec<SourceTreeItem> = Vec::new();
    for op in ops {
        if let EditOp::Add { sources } = op {
            let items = collect_add_sources(sources, archive_path)?;
            for it in items {
                let key = member_form(&it.entry_name);
                if add_items.iter().any(|x| member_form(&x.entry_name) == key) {
                    return Err(format!("duplicate add member '{key}'"));
                }
                if surviving.contains_key(&key) {
                    surviving.remove(&key);
                    deleted_names.push(key.clone());
                }
                add_items.push(it);
                added_names.push(key);
            }
        }
    }

    let mut finals: Vec<FinalMember> = Vec::new();
    for (name, (orig, info)) in &surviving {
        let original_name = &catalog[info.index].name;
        finals.push(FinalMember {
            name: if name == orig {
                original_name.clone()
            } else {
                name.clone()
            },
            is_dir: info.is_dir,
            size: info.size,
            source: MemberSource::Original {
                index: info.index,
                orig_name: original_name.clone(),
            },
        });
    }
    for it in &add_items {
        finals.push(FinalMember {
            name: member_form(&it.entry_name),
            is_dir: it.is_dir,
            size: it.size,
            source: MemberSource::Added {
                disk_path: it.disk_path.clone(),
            },
        });
    }

    validate_final(&finals, max_bytes)?;

    let enc_required = finals.iter().any(|fm| match &fm.source {
        MemberSource::Original { index, .. } => catalog[*index].is_encrypted,
        MemberSource::Added { .. } => false,
    });

    Ok(Plan {
        finals,
        added_names,
        deleted_names,
        renamed_pairs,
        enc_required,
    })
}

/// Validate the planned result.
///
/// The add-name rule is a rule about names *entering* the archive, so it is applied to members
/// this edit creates: added sources and rename destinations. A retained member keeps the name it
/// already carries inside the archive; whether that name could be created today is not this
/// edit's decision to make, and re-judging it would make a delete fail because of a member it
/// does not touch (a retained `a:b.txt` must not block `delete ok.txt`).
fn validate_final(finals: &[FinalMember], max_bytes: u64) -> Result<(), String> {
    let mut seen: HashMap<String, ()> = HashMap::new();
    for fm in finals {
        let enters_archive = match &fm.source {
            MemberSource::Added { .. } => true,
            MemberSource::Original { orig_name, .. } => &fm.name != orig_name,
        };
        if enters_archive {
            validate_added_name(if fm.is_dir {
                fm.name.trim_end_matches('/')
            } else {
                &fm.name
            })?;
        }
        if seen.insert(fm.name.clone(), ()).is_some() {
            return Err(format!("result contains a duplicate member '{}'", fm.name));
        }
    }

    let files: Vec<&str> = finals
        .iter()
        .filter(|f| !f.is_dir)
        .map(|f| f.name.as_str())
        .collect();
    for f in &files {
        let prefix = format!("{f}/");
        for other in finals {
            if other.name == *f {
                continue;
            }
            if other.name.starts_with(&prefix) {
                return Err(format!(
                    "member '{f}' is both a file and a parent directory"
                ));
            }
        }
    }

    let mut total = 0u64;
    for fm in finals {
        if fm.is_dir {
            continue;
        }
        total = total
            .checked_add(fm.size)
            .ok_or("total uncompressed size overflow")?;
    }
    crate::core::guard::check_zip_bomb(total, max_bytes).map_err(|e| e.to_string())?;
    Ok(())
}

fn staging_path_for(archive_path: &Path) -> PathBuf {
    let parent = archive_path.parent().unwrap_or(Path::new("."));
    let name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    let token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".mothzip_edit_{}_{}_{}",
        std::process::id(),
        token,
        name
    ))
}

fn write_zip_replacement(
    out_file: File,
    archive_path: &Path,
    plan: &Plan,
    options: &EditOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let src_file = File::open(archive_path)?;
    let mut src = zip::ZipArchive::new(src_file)?;
    let mut writer = zip::ZipWriter::new(out_file);

    let compression = match options.level {
        Some(0) => zip::CompressionMethod::Stored,
        _ => zip::CompressionMethod::Deflated,
    };
    let mut base_options = zip::write::SimpleFileOptions::default()
        .compression_method(compression)
        .unix_permissions(0o755);
    if compression == zip::CompressionMethod::Deflated {
        base_options = base_options.compression_level(options.level.map(i64::from));
    }
    if let Some(pass) = options.password {
        base_options = base_options.with_aes_encryption(zip::AesMode::Aes256, pass);
    }

    for fm in &plan.finals {
        match &fm.source {
            MemberSource::Original { index, orig_name } => {
                let file = src.by_index_raw(*index)?;
                if orig_name == &fm.name {
                    writer.raw_copy_file(file)?;
                } else {
                    let target = if fm.is_dir {
                        format!("{}/", fm.name)
                    } else {
                        fm.name.clone()
                    };
                    writer.raw_copy_file_rename(file, target)?;
                }
            }
            MemberSource::Added { disk_path } => {
                let mut opts = base_options;
                if let Ok(meta) = fs::metadata(disk_path) {
                    if let Ok(mtime) = meta.modified() {
                        if let Some(dt) = crate::core::format::system_time_to_zip_datetime(mtime) {
                            opts = opts.last_modified_time(dt);
                        }
                    }
                }
                if fm.is_dir {
                    writer.add_directory(format!("{}/", fm.name), opts)?;
                } else {
                    writer.start_file(&fm.name, opts)?;
                    let mut f = File::open(disk_path)?;
                    io::copy(&mut f, &mut writer)?;
                }
            }
        }
    }

    writer.finish()?.sync_all()?;
    Ok(())
}

fn rebuild_entry(
    src: &sevenz_rust2::ArchiveEntry,
    name: String,
    is_dir: bool,
) -> sevenz_rust2::ArchiveEntry {
    let mut e = sevenz_rust2::ArchiveEntry {
        name,
        is_directory: is_dir,
        has_stream: !is_dir,
        ..Default::default()
    };
    e.has_last_modified_date = src.has_last_modified_date;
    e.last_modified_date = src.last_modified_date;
    e.has_creation_date = src.has_creation_date;
    e.creation_date = src.creation_date;
    e.has_access_date = src.has_access_date;
    e.access_date = src.access_date;
    e.has_windows_attributes = src.has_windows_attributes;
    e.windows_attributes = src.windows_attributes;
    e
}

fn write_7z_replacement(
    out_file: File,
    archive_path: &Path,
    plan: &Plan,
    options: &EditOptions,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let pass = sevenz_rust2::Password::from(options.password.unwrap_or(""));
    let mut reader = crate::core::format::open_7z_reader(archive_path, &pass)?;

    let mut writer = sevenz_rust2::ArchiveWriter::new(out_file)
        .map_err(|e| format!("7z writer create failed: {e}"))?;
    // One encoder chain for pack and edit: same level clamp, same AES-then-LZMA2/COPY order.
    // Edit re-encodes at the preset dictionary, so it passes no dictionary override.
    writer.set_content_methods(crate::core::pack::seven_z_encoder_chain(
        options.level.unwrap_or(5).clamp(0, 9),
        options.password,
        None,
    ));

    let mut orig_by_name: HashMap<String, usize> = HashMap::new();
    for (i, fm) in plan.finals.iter().enumerate() {
        if let MemberSource::Original { orig_name, .. } = &fm.source {
            orig_by_name.insert(member_form(orig_name), i);
        }
    }

    let mut remaining = max_bytes;
    reader
        .for_each_entries(|src_entry, rd| {
            let src_name = member_form(src_entry.name());
            if let Some(&i) = orig_by_name.get(&src_name) {
                let fm = &plan.finals[i];
                let e = rebuild_entry(src_entry, fm.name.clone(), fm.is_dir);
                if fm.is_dir {
                    writer.push_archive_entry(e, None::<&[u8]>)?;
                } else {
                    let sz = src_entry.size();
                    if sz > remaining {
                        return Err(sevenz_rust2::Error::from(std::io::Error::other(
                            "decompressed byte budget exceeded while re-encoding 7z",
                        )));
                    }
                    let mut bounded = crate::core::guard::BudgetReader::new(rd, sz);
                    writer.push_archive_entry(e, Some(&mut bounded))?;
                    if bounded.remaining() != 0 {
                        return Err(sevenz_rust2::Error::from(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "7z member is shorter than its declared size",
                        )));
                    }
                    remaining -= sz;
                }
            } else if !src_entry.is_directory() {
                let consumed =
                    crate::core::guard::drain_archive_entry(rd, Some(src_entry.size()), remaining)
                        .map_err(|error| {
                            sevenz_rust2::Error::from(io::Error::other(error.to_string()))
                        })?;
                remaining -= consumed;
            }
            Ok(true)
        })
        .map_err(|e| format!("7z re-encode failed: {e}"))?;

    for fm in &plan.finals {
        if let MemberSource::Added { disk_path } = &fm.source {
            let entry = sevenz_rust2::ArchiveEntry::from_path(disk_path, fm.name.clone());
            if fm.is_dir {
                writer
                    .push_archive_entry(entry, None::<&[u8]>)
                    .map_err(|e| format!("7z add directory failed: {e}"))?;
            } else {
                let sz = fs::metadata(disk_path).map_err(|e| e.to_string())?.len();
                if sz > remaining {
                    return Err("decompressed byte budget exceeded for an added 7z member".into());
                }
                remaining -= sz;
                let mut f = File::open(disk_path)?;
                writer
                    .push_archive_entry(entry, Some(&mut f))
                    .map_err(|e| format!("7z add file failed: {e}"))?;
            }
        }
    }

    writer
        .finish()
        .map_err(|e| format!("7z finalize failed: {e}"))?
        .sync_all()?;
    Ok(())
}

fn verify_replacement(
    archive_path: &Path,
    plan: &Plan,
    password: Option<&str>,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let catalog = crate::core::catalog::read_archive_catalog(archive_path, password, max_bytes)?;
    let mut actual: Vec<(String, bool, u64)> = catalog
        .iter()
        .map(|e| (member_form(&e.name), e.is_dir, e.size))
        .collect();
    actual.sort();
    let mut expected: Vec<_> = plan
        .finals
        .iter()
        .map(|entry| (member_form(&entry.name), entry.is_dir, entry.size))
        .collect();
    expected.sort();
    if actual != expected {
        let actual_names: Vec<String> = actual
            .iter()
            .map(|(n, d, size)| format!("{n}(dir={d},bytes={size})"))
            .collect();
        let expected_names: Vec<String> = expected
            .iter()
            .map(|(n, d, size)| format!("{n}(dir={d},bytes={size})"))
            .collect();
        return Err(format!(
            "replacement archive catalog mismatch\n  actual  : {actual_names:?}\n  expected: {expected_names:?}"
        )
        .into());
    }
    let integrity =
        crate::core::inspect::test_archive_integrity(archive_path, password, max_bytes)?;
    if !integrity.passed {
        return Err(integrity
            .error_message
            .unwrap_or_else(|| "Replacement integrity check failed".into())
            .into());
    }
    Ok(())
}

/// Edit `archive_path` by applying `ops`, producing a verified replacement that is
/// committed transactionally. On any failure the original archive is retained.
pub fn edit_archive(
    archive_path: &Path,
    ops: &[EditOp],
    options: &EditOptions,
) -> Result<EditSummary, Box<dyn std::error::Error>> {
    let max_bytes = options.max_bytes;
    let filename = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if filename.rsplit_once(".7z.").is_some_and(|(_, suffix)| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return Err(
            "Editing a split 7z set is not supported; create a replacement volume set with pack"
                .into(),
        );
    }
    if crate::core::sfx::embedded_archive(archive_path)?.is_some() {
        return Err("Edit the ZIP input before converting it to a self-extractor".into());
    }

    let format = crate::core::format::detect_archive_format(archive_path)?;
    let kind = match format {
        ArchiveFormat::Zip => FormatKind::Zip,
        ArchiveFormat::SevenZ => FormatKind::SevenZ,
        ArchiveFormat::Rar => {
            return Err("RAR archive editing is not supported".into());
        }
        _ => {
            return Err("TAR and compressed stream archive editing is not supported".into());
        }
    };

    let catalog =
        crate::core::catalog::read_archive_catalog(archive_path, options.password, max_bytes)?;
    let plan = build_plan(&catalog, ops, archive_path, max_bytes)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    if plan.enc_required && options.password.is_none() {
        return Err("archive contains encrypted members; a password is required to edit it".into());
    }

    let staging = staging_path_for(archive_path);
    let reserved = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    let result = (|| -> Result<EditSummary, Box<dyn std::error::Error>> {
        match kind {
            FormatKind::Zip => {
                write_zip_replacement(reserved, archive_path, &plan, options)?;
            }
            FormatKind::SevenZ => {
                write_7z_replacement(reserved, archive_path, &plan, options, max_bytes)?;
            }
        }
        verify_replacement(&staging, &plan, options.password, max_bytes)?;
        crate::core::commit::commit_files(&[(staging.clone(), archive_path.to_path_buf())])?;
        Ok(EditSummary {
            archive_path: archive_path.to_path_buf(),
            total_files: plan.finals.iter().filter(|f| !f.is_dir).count(),
            added: plan.added_names.clone(),
            deleted: plan.deleted_names.clone(),
            renamed: plan.renamed_pairs.clone(),
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    /// The same member name composed and in conjoining jamo, as macOS and Linux write it.
    const COMPOSED: &str = "\u{d55c}\u{ae00}.txt";
    const DECOMPOSED: &str = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}.txt";

    /// A private working directory per test, outside the archive paths under test.
    fn work_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from("target").join("edit_fixture").join(format!(
            "{}_{}_{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a stored ZIP whose member names are exactly the given bytes.
    fn zip_fixture(dir: &Path, name: &str, members: &[(&str, &[u8])]) -> PathBuf {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (member, content) in members {
            writer.start_file(*member, options).unwrap();
            io::Write::write_all(&mut writer, content).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let archive = dir.join(name);
        fs::write(&archive, bytes).unwrap();
        archive
    }

    fn member_names(archive: &Path) -> Vec<String> {
        crate::core::catalog::read_archive_catalog(
            archive,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect()
    }

    fn delete(archive: &Path, members: &[&str]) -> Result<EditSummary, Box<dyn std::error::Error>> {
        edit_archive(
            archive,
            &[EditOp::Delete {
                members: members.iter().map(|m| (*m).to_string()).collect(),
            }],
            &EditOptions::default(),
        )
    }

    /// Defect A: `delete ok.txt` used to fail on a retained `a:b.txt`, because the add-name
    /// rule was applied to every member of the result instead of only to names entering it.
    #[test]
    fn delete_keeps_retained_member_names_out_of_the_add_rule() {
        let dir = work_dir("retained_names");
        let archive = zip_fixture(
            &dir,
            "fixture.zip",
            &[("a:b.txt", b"ADS"), ("ok.txt", b"OK")],
        );

        delete(&archive, &["ok.txt"]).expect("a retained original name must not be re-validated");

        assert_eq!(member_names(&archive), vec!["a:b.txt".to_string()]);
        // The member colons cannot protect: deleting that member is the same rule.
        delete(&archive, &["a:b.txt"]).expect("deleting the member by its own name still works");
        assert!(member_names(&archive).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A rename *does* introduce a name, so the add rule still governs its destination.
    #[test]
    fn rename_rejects_a_destination_the_add_rule_forbids() {
        let dir = work_dir("rename_destination");
        let archive = zip_fixture(&dir, "fixture.zip", &[("plain.txt", b"PLAIN")]);
        let before = fs::read(&archive).unwrap();

        let error = edit_archive(
            &archive,
            &[EditOp::Rename {
                from: "plain.txt".into(),
                to: "a:b.txt".into(),
            }],
            &EditOptions::default(),
        )
        .expect_err("a colon rename destination must be refused");

        assert!(error.to_string().contains("colon"), "{error}");
        assert_eq!(fs::read(&archive).unwrap(), before);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Defect B: a member stored with decomposed Hangul used to be invisible to `delete` even
    /// though `cat` resolved the composed name. Both composition directions are measured, and a
    /// name the archive does not hold stays rejected.
    #[test]
    fn delete_resolves_both_hangul_compositions_and_rejects_an_unrelated_name() {
        let dir = work_dir("hangul_names");

        // Stored decomposed (macOS/Linux ZIP), requested composed (what a user types).
        let decomposed_archive = zip_fixture(&dir, "decomposed.zip", &[(DECOMPOSED, b"HANGUL")]);
        // Stored composed, requested decomposed, which is the same rule in the other direction.
        let composed_archive = zip_fixture(&dir, "composed.zip", &[(COMPOSED, b"HANGUL")]);

        for (archive, stored, requested) in [
            (&decomposed_archive, DECOMPOSED, COMPOSED),
            (&composed_archive, COMPOSED, DECOMPOSED),
        ] {
            let error = delete(archive, &["\u{b2e4}\u{b978}.txt"])
                .expect_err("an unrelated name must stay rejected");
            assert!(error.to_string().contains("not found"), "{error}");
            assert_eq!(member_names(archive), vec![stored.to_string()]);

            delete(archive, &[requested])
                .expect("the composed and decomposed names are one member");
            assert!(member_names(archive).is_empty());
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
