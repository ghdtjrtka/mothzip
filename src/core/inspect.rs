use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use zip::result::ZipError;
use zip::ZipArchive;

use super::catalog::read_archive_catalog;
use super::codec::StreamCodec;
use super::format::{archive_stem_cleaned, detect_archive_format, ArchiveFormat};

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

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct IntegritySummary {
    pub archive_path: PathBuf,
    pub total_entries: usize,
    pub tested_entries: usize,
    pub passed: bool,
    pub error_message: Option<String>,
}

/// Bound display UTF-8 bytes without slicing inside a character.
fn truncate_str_for_display(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let prefix_len = (max_len / 2).saturating_sub(2);
        let suffix_len = (max_len / 2).saturating_sub(2);
        let mut prefix_end = prefix_len;
        while !s.is_char_boundary(prefix_end) {
            prefix_end -= 1;
        }
        let mut suffix_start = s.len() - suffix_len;
        while !s.is_char_boundary(suffix_start) {
            suffix_start += 1;
        }
        format!("{}...{}", &s[..prefix_end], &s[suffix_start..])
    }
}

fn bound_extension_distribution(
    raw: BTreeMap<String, usize>,
    limit: usize,
) -> BTreeMap<String, usize> {
    if raw.len() <= limit {
        return raw;
    }
    let mut sorted: Vec<(String, usize)> = raw.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut bounded = BTreeMap::new();
    let mut other_count = 0;
    for (i, (ext, count)) in sorted.into_iter().enumerate() {
        if i < limit {
            bounded.insert(ext, count);
        } else {
            other_count += count;
        }
    }
    if other_count > 0 {
        bounded.insert("(other)".to_string(), other_count);
    }
    bounded
}

/// Fast inspection of an archive (.zip, .tar.gz, .tgz, .tar, .gz, .7z, .rar) without extracting any files.
/// Tailored for AI Agents to understand archive layout within strict token budgets.
#[cfg(test)]
pub fn peek_archive(
    archive_path: &Path,
    depth_limit: usize,
    count_limit: usize,
    max_bytes: u64,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    peek_archive_with_password(archive_path, depth_limit, count_limit, None, max_bytes)
}

pub fn peek_archive_with_password(
    archive_path: &Path,
    depth_limit: usize,
    count_limit: usize,
    password: Option<&str>,
    max_bytes: u64,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    let entries = read_archive_catalog(archive_path, password, max_bytes)?;
    peek_catalog_archive(archive_path, &entries, depth_limit, count_limit)
}

fn peek_catalog_archive(
    archive_path: &Path,
    entries: &[super::catalog::CatalogEntry],
    depth_limit: usize,
    count_limit: usize,
) -> Result<PeekSummary, Box<dyn std::error::Error>> {
    let archive_stem = archive_stem_cleaned(archive_path);
    let layout_entries: Vec<super::smart::LayoutEntryInfo> =
        entries.iter().map(|e| e.to_layout_entry()).collect();
    let smart_plan =
        super::smart::evaluate_layout_plan(&layout_entries, &archive_stem, Path::new("."))?;

    let mut extension_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut tree_summary = Vec::new();
    let mut truncated_items = 0;

    for entry in entries {
        if !entry.is_dir {
            let ext_raw = Path::new(&entry.display_name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_lowercase()))
                .unwrap_or_else(|| "(no ext)".to_string());
            let ext = truncate_str_for_display(&ext_raw, 32);
            *extension_distribution.entry(ext).or_insert(0) += 1;
        }

        let depth = entry.display_name.trim_matches('/').split('/').count();
        if depth <= depth_limit {
            if tree_summary.len() < count_limit {
                tree_summary.push(TreeSummaryItem {
                    path: truncate_str_for_display(&entry.display_name, 128),
                    is_dir: entry.is_dir,
                    size: entry.size,
                });
            } else {
                truncated_items += 1;
            }
        } else {
            truncated_items += 1;
        }
    }

    let file_size = fs::metadata(archive_path)?.len();
    let comp_bytes = if smart_plan.total_compressed_bytes > 0 {
        smart_plan.total_compressed_bytes
    } else {
        file_size
    };

    let ratio_num = if comp_bytes > 0 {
        smart_plan.total_uncompressed_bytes as f64 / comp_bytes as f64
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
        total_compressed_bytes: comp_bytes,
        total_uncompressed_bytes: smart_plan.total_uncompressed_bytes,
        compression_ratio,
        has_single_root: smart_plan.has_single_root,
        root_name: smart_plan
            .root_name
            .map(|name| truncate_str_for_display(&name, 128)),
        extension_distribution: bound_extension_distribution(extension_distribution, count_limit),
        tree_summary,
        truncated_items,
    })
}

/// Stream the contents of a specific file inside an archive (.zip, .tar.gz, .tgz, .tar, .gz, .7z, .rar) directly to stdout or a writer.
/// Designed for AI Agents to inspect specific source files within strict token/RAM budgets without disk extraction.
pub fn cat_archive_file<W: Write + 'static>(
    archive_path: &Path,
    target_entry_path: &str,
    password: Option<&str>,
    writer: &mut W,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;
    match format {
        ArchiveFormat::Zip => {
            cat_zip_file(archive_path, target_entry_path, password, writer, max_bytes)
        }
        ArchiveFormat::Tar => {
            cat_tar_file(archive_path, None, target_entry_path, writer, max_bytes)
        }
        ArchiveFormat::TarStream(codec) => cat_tar_file(
            archive_path,
            Some(codec),
            target_entry_path,
            writer,
            max_bytes,
        ),
        ArchiveFormat::Stream(codec) => cat_stream_file(archive_path, codec, writer, max_bytes),
        ArchiveFormat::SevenZ => {
            cat_7z_file(archive_path, target_entry_path, password, writer, max_bytes)
        }
        ArchiveFormat::Rar => {
            cat_rar_file(archive_path, target_entry_path, password, writer, max_bytes)
        }
    }
}

fn cat_stream_file<W: Write>(
    archive_path: &Path,
    codec: StreamCodec,
    writer: &mut W,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = super::codec::decoder(File::open(archive_path)?, codec)?;
    super::guard::copy_archive_entry(&mut reader, writer, None, 0, max_bytes)?;
    Ok(())
}

fn cat_zip_file<W: Write>(
    archive_path: &Path,
    target_entry_path: &str,
    password: Option<&str>,
    writer: &mut W,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;

    let target_raw = target_entry_path.replace('\\', "/");
    let target_normalized = crate::core::encoding::compose_hangul_nfc(&target_raw);

    // Pass 1: Find best matching entry index by metadata WITHOUT decrypting
    let mut exact_match = None;
    let mut normalized_match = None;
    let mut case_match = None;

    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i)?;
        let name_raw = super::encoding::decode_entry_name_raw(entry.name_raw()).replace('\\', "/");
        let name_normalized = crate::core::encoding::compose_hangul_nfc(&name_raw);

        if name_raw == target_raw {
            exact_match = Some(i);
            break;
        }
        if normalized_match.is_none()
            && (name_normalized == target_normalized
                || name_normalized.trim_start_matches('/')
                    == target_normalized.trim_start_matches('/'))
        {
            normalized_match = Some(i);
        }
        if case_match.is_none() && name_normalized.eq_ignore_ascii_case(&target_normalized) {
            case_match = Some(i);
        }
    }

    let chosen_idx = exact_match
        .or(normalized_match)
        .or(case_match)
        .ok_or_else(|| {
            format!(
                "File '{}' not found inside archive '{}'",
                target_entry_path,
                archive_path.display()
            )
        })?;

    // Pass 2: Decrypt and stream ONLY the chosen index
    let mut entry = match password {
        Some(pass) => match archive.by_index_decrypt(chosen_idx, pass.as_bytes()) {
            Ok(e) => e,
            Err(ZipError::UnsupportedArchive(msg)) => {
                return Err(format!("Decryption failed: {}", msg).into());
            }
            Err(e) => return Err(Box::new(e)),
        },
        None => match archive.by_index(chosen_idx) {
            Ok(e) => e,
            Err(ZipError::UnsupportedArchive(msg))
                if msg.contains("Password") || msg.contains("encrypted") =>
            {
                return Err("File is password-protected. Please supply --password <PASS>".into());
            }
            Err(e) => return Err(Box::new(e)),
        },
    };

    if entry.is_dir() {
        return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
    }

    let size = entry.size();
    super::guard::copy_archive_entry(&mut entry, writer, Some(size), 0, max_bytes)?;
    Ok(())
}

fn cat_tar_file<W: Write>(
    archive_path: &Path,
    codec: Option<StreamCodec>,
    target_entry_path: &str,
    writer: &mut W,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let target_raw = target_entry_path.replace('\\', "/");
    let target_normalized = crate::core::encoding::compose_hangul_nfc(&target_raw);

    // Pass 1: exact match
    let reader = super::format::open_tar_reader(archive_path, codec, max_bytes)?;
    let mut tar = tar::Archive::new(reader);

    for entry_res in tar.entries()? {
        let mut entry = entry_res?;
        let path_cow = entry.path()?;
        let raw_name = path_cow.to_string_lossy().to_string();
        let name_raw = raw_name.replace('\\', "/");

        if name_raw == target_raw {
            if entry.header().entry_type().is_dir() {
                return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
            }
            let size = entry.size();
            super::guard::copy_archive_entry(&mut entry, writer, Some(size), 0, max_bytes)?;
            return Ok(());
        }
    }

    // Pass 2: normalized / case-insensitive fallback match
    let reader2 = super::format::open_tar_reader(archive_path, codec, max_bytes)?;
    let mut tar2 = tar::Archive::new(reader2);

    for entry_res in tar2.entries()? {
        let mut entry = entry_res?;
        let path_cow = entry.path()?;
        let raw_name = path_cow.to_string_lossy().to_string();
        let name_raw = raw_name.replace('\\', "/");
        let name_normalized = crate::core::encoding::compose_hangul_nfc(&name_raw);

        if name_normalized == target_normalized
            || name_normalized.trim_start_matches('/') == target_normalized.trim_start_matches('/')
            || name_normalized.eq_ignore_ascii_case(&target_normalized)
        {
            if entry.header().entry_type().is_dir() {
                return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
            }
            let size = entry.size();
            super::guard::copy_archive_entry(&mut entry, writer, Some(size), 0, max_bytes)?;
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

fn cat_7z_file<W: Write>(
    archive_path: &Path,
    target_entry_path: &str,
    password: Option<&str>,
    writer: &mut W,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let catalog = read_archive_catalog(archive_path, password, max_bytes)?;
    let target_raw = target_entry_path.replace('\\', "/");
    let target_norm = crate::core::encoding::compose_hangul_nfc(&target_raw);

    let chosen = catalog
        .iter()
        .find(|e| e.name == target_raw)
        .or_else(|| {
            catalog.iter().find(|e| {
                e.name == target_norm
                    || e.name.trim_start_matches('/') == target_norm.trim_start_matches('/')
            })
        })
        .or_else(|| {
            catalog
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(&target_norm))
        })
        .ok_or_else(|| {
            format!(
                "File '{}' not found inside archive '{}'",
                target_entry_path,
                archive_path.display()
            )
        })?;

    if chosen.is_dir {
        return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
    }

    let pass = match password {
        Some(p) => sevenz_rust2::Password::from(p),
        None => sevenz_rust2::Password::empty(),
    };

    let mut reader = super::format::open_7z_reader(archive_path, &pass).map_err(|e| {
        let msg = e.to_string();
        if msg.to_lowercase().contains("password") {
            "File is password-protected. Please supply --password <PASS>".to_string()
        } else {
            format!("7z open failed: {}", msg)
        }
    })?;

    let mut found = false;
    let target_name = &chosen.name;
    let target_display = &chosen.display_name;
    reader
        .for_each_entries(|entry, read_stream| {
            let entry_name = entry.name().replace('\\', "/");
            let norm_name = super::encoding::compose_hangul_nfc(&entry_name);
            let matches = entry_name == *target_name
                || norm_name == *target_display
                || entry_name == *target_display
                || norm_name == *target_name;
            if matches {
                found = true;
                super::guard::copy_archive_entry(
                    read_stream,
                    writer,
                    Some(entry.size()),
                    0,
                    max_bytes,
                )
                .map_err(|e| sevenz_rust2::Error::from(io::Error::other(e.to_string())))?;
                return Ok(false);
            }
            if !entry.is_directory() && entry.has_stream() {
                super::guard::drain_archive_entry(read_stream, Some(entry.size()), max_bytes)
                    .map_err(|e| sevenz_rust2::Error::from(io::Error::other(e.to_string())))?;
            }
            Ok(true)
        })
        .map_err(|e| -> Box<dyn std::error::Error> {
            let msg = e.to_string();
            if msg.to_lowercase().contains("password") {
                "File is password-protected. Please supply --password <PASS>".into()
            } else {
                format!("7z stream read error: {}", msg).into()
            }
        })?;

    if !found {
        return Err(format!(
            "File '{}' not found inside archive '{}'",
            target_entry_path,
            archive_path.display()
        )
        .into());
    }

    Ok(())
}

/// rars decodes a RAR5 member whose unpacked size is at or below
/// `ArchiveReadOptions::rar50_buffered_decode_limit` completely in memory before
/// writing any of it, and streams members above that limit. The library default is
/// 512 MiB, which would let `cat` hold half a gigabyte for a single member, so the
/// limit is pinned to one unrar filter block: the streaming path has to hold a filter
/// block that size anyway, so a member at or below it costs no more when buffered
/// than when streamed. Measured peak working set stays flat across member sizes with
/// this ceiling (`audit/rar_cat_memory.py`); without it the peak tracks the member.
const RAR_STREAM_DECODE_LIMIT: u64 = 4 * 1024 * 1024;

/// Lets rars' writer-callback API write into the caller's borrowed writer.
///
/// `ExtractionDecision::Extract` carries a `Box<dyn Write>`, which is
/// `Box<dyn Write + 'static>`, so a `&mut W` cannot be boxed directly (rustc:
/// "coercion requires that `'1` must outlive `'static`"). Collecting the member into
/// a `Vec<u8>` first is the other way out, and it makes peak memory the member size.
///
/// SAFETY: `cat_rar_file` builds this from its `&mut W` and hands it to exactly one
/// extraction call. rars binds the box to the member being extracted and drops it
/// when that member ends (`rar50::extract` keeps the writer in the member's local
/// scope), so every `write`/`flush` happens while the pointer is valid and is the
/// only live access to that writer.
struct BorrowedWriter<W: Write> {
    target: *mut W,
}

impl<W: Write> BorrowedWriter<W> {
    fn new(target: &mut W) -> Self {
        Self { target }
    }
}

impl<W: Write> Write for BorrowedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: the pointer stays valid for the extraction call that owns this
        // adapter; see the struct invariant.
        unsafe { (*self.target).write(buf) }
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: as in `write`.
        unsafe { (*self.target).flush() }
    }
}

fn cat_rar_file<W: Write + 'static>(
    archive_path: &Path,
    target_entry_path: &str,
    password: Option<&str>,
    writer: &mut W,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let catalog = read_archive_catalog(archive_path, password, max_bytes)?;
    let target_raw = target_entry_path.replace('\\', "/");
    let target_norm = crate::core::encoding::compose_hangul_nfc(&target_raw);

    let chosen = catalog
        .iter()
        .find(|e| e.name == target_raw)
        .or_else(|| {
            catalog.iter().find(|e| {
                e.name == target_norm
                    || e.name.trim_start_matches('/') == target_norm.trim_start_matches('/')
            })
        })
        .or_else(|| {
            catalog
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(&target_norm))
        })
        .ok_or_else(|| {
            format!(
                "File '{}' not found inside archive '{}'",
                target_entry_path,
                archive_path.display()
            )
        })?;

    if chosen.is_dir {
        return Err(format!("'{}' is a directory, not a file", target_entry_path).into());
    }

    let target_index = chosen.index;

    let mut options = rars::ArchiveReadOptions::default();
    if let Some(p) = password {
        options.password = Some(p.as_bytes());
    }
    // rars charges both ceilings while decoding, and refuses a member whose declared
    // size is already over them before it opens the output writer, so the emitted
    // bytes are bounded without a second guard around the writer.
    options.max_member_output_bytes = Some(max_bytes);
    options.max_total_output_bytes = Some(max_bytes);
    options.rar50_buffered_decode_limit = Some(RAR_STREAM_DECODE_LIMIT);

    let volumes = super::format::find_volume_files(archive_path);
    let archives = volumes
        .iter()
        .map(|v| rars::ArchiveReader::read_path_with_options(v, options))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("password") || msg.to_lowercase().contains("encrypted") {
                "File is password-protected. Please supply --password <PASS>".to_string()
            } else {
                format!("RAR open failed: {}", msg)
            }
        })?;

    let mut emitted = false;
    let mut index = 0usize;
    let result = match archives.as_slice() {
        [] => return Err("RAR open failed: no archive to read".into()),
        // A volume set has no early stop in this API: every member is decoded and only
        // the selected one reaches the caller, exactly as the collected read did.
        [_, _, ..] => rars::extract_volumes_to_with_options(&archives, options, |member| {
            let this = index;
            index += 1;
            if this == target_index && !member.is_directory {
                emitted = true;
                Ok(Box::new(BorrowedWriter::new(writer)) as Box<dyn Write>)
            } else {
                Ok(Box::new(io::sink()) as Box<dyn Write>)
            }
        }),
        [archive] => {
            let solid = archive.is_solid();
            archive
                .extract_with_control(options, |member| {
                    let this = index;
                    index += 1;
                    if this > target_index {
                        return Ok(rars::ExtractionDecision::Stop);
                    }
                    if this == target_index && !member.meta.is_directory {
                        emitted = true;
                        return Ok(rars::ExtractionDecision::Extract(Box::new(
                            BorrowedWriter::new(writer),
                        )));
                    }
                    // A solid archive's later payloads depend on the earlier history, so
                    // skipping is not allowed; anything else can be skipped unread.
                    Ok(
                        if solid && !member.meta.is_directory && !member.meta.is_redirection {
                            rars::ExtractionDecision::Extract(Box::new(io::sink()))
                        } else {
                            rars::ExtractionDecision::Skip
                        },
                    )
                })
                .map(|_outcome| ())
        }
    };

    result.map_err(|e| {
        let msg = e.to_string();
        if msg.to_lowercase().contains("password") || msg.to_lowercase().contains("encrypted") {
            "File is password-protected. Please supply --password <PASS>".to_string()
        } else {
            format!("RAR cat failed: {}", msg)
        }
    })?;

    if !emitted {
        return Err(format!(
            "File '{}' not found inside archive '{}'",
            target_entry_path,
            archive_path.display()
        )
        .into());
    }
    Ok(())
}

/// Test the integrity of an archive by decompressing entries and verifying checksums.
/// Equivalent to `bz t <archive>` or `unzip -t`.
#[allow(dead_code)]
pub fn test_archive_integrity(
    archive_path: &Path,
    password: Option<&str>,
    max_bytes: u64,
) -> Result<IntegritySummary, Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;
    let (total_entries, catalog_err) = match read_archive_catalog(archive_path, password, max_bytes)
    {
        Ok(c) => (c.len(), None),
        Err(e) => (0, Some(e)),
    };

    if let Some(e) = catalog_err {
        return Ok(IntegritySummary {
            archive_path: archive_path.to_path_buf(),
            total_entries: 0,
            tested_entries: 0,
            passed: false,
            error_message: Some(format!("Archive header corrupt: {}", e)),
        });
    }

    let mut tested_entries = 0;
    let mut sink = super::guard::BudgetWriter::new(io::sink(), max_bytes);

    let result: Result<(), Box<dyn std::error::Error>> = match format {
        ArchiveFormat::Zip => {
            let file = File::open(archive_path)?;
            let mut archive = zip::ZipArchive::new(file)?;
            for i in 0..archive.len() {
                let mut entry = match password {
                    Some(pass) => archive.by_index_decrypt(i, pass.as_bytes())?,
                    None => archive.by_index(i)?,
                };
                if !entry.is_dir() {
                    io::copy(&mut entry, &mut sink)?;
                    tested_entries += 1;
                }
            }
            Ok(())
        }
        ArchiveFormat::Tar | ArchiveFormat::TarStream(_) => (|| {
            let reader = super::format::open_tar_reader(archive_path, format.codec(), max_bytes)?;
            let mut tar = tar::Archive::new(reader);
            for entry_res in tar.entries()? {
                let mut entry = entry_res?;
                if !entry.header().entry_type().is_dir() {
                    io::copy(&mut entry, &mut sink)?;
                    tested_entries += 1;
                }
            }
            // TAR has no trailer of its own, so the codec's checksum is only verified once the
            // decoder is read to end of stream. Without this drain a truncated or corrupted
            // TAR.GZ/TAR.ZST/TAR.XZ/TAR.LZ4 reports a clean member list while `unpack` fails.
            let mut stream = tar.into_inner();
            io::copy(&mut stream, &mut sink)?;
            Ok(())
        })(),
        ArchiveFormat::Stream(codec) => {
            let mut reader = super::codec::decoder(File::open(archive_path)?, codec)?;
            io::copy(&mut reader, &mut sink)?;
            tested_entries = 1;
            Ok(())
        }
        ArchiveFormat::SevenZ => {
            let pass = match password {
                Some(p) => sevenz_rust2::Password::from(p),
                None => sevenz_rust2::Password::empty(),
            };
            match super::format::open_7z_reader(archive_path, &pass) {
                Ok(mut reader) => reader
                    .for_each_entries(|entry, read_stream| {
                        if !entry.is_directory() {
                            io::copy(read_stream, &mut sink).map_err(|e| {
                                sevenz_rust2::Error::from(io::Error::other(e.to_string()))
                            })?;
                            tested_entries += 1;
                        }
                        Ok(true)
                    })
                    .map_err(|e| format!("7z integrity failure: {}", e).into()),
                Err(e) => Err(format!("7z open failed: {}", e).into()),
            }
        }
        ArchiveFormat::Rar => {
            let mut options = rars::ArchiveReadOptions::default();
            options.max_member_output_bytes = Some(max_bytes);
            options.max_total_output_bytes = Some(max_bytes);
            if let Some(p) = password {
                options.password = Some(p.as_bytes());
            }
            let volumes = super::format::find_volume_files(archive_path);
            if volumes.len() > 1 {
                match volumes
                    .iter()
                    .map(|v| rars::ArchiveReader::read_path_with_options(v, options))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(archives) => {
                        let members = rars::volume_members(&archives)
                            .map_err(|e| format!("RAR volume member read failed: {}", e))?;
                        tested_entries = members.iter().filter(|m| !m.meta.is_directory).count();
                        rars::test_volumes_with_options(&archives, options)
                            .map_err(|e| format!("RAR integrity failure: {}", e).into())
                    }
                    Err(e) => Err(format!("RAR volume set open failed: {}", e).into()),
                }
            } else {
                match rars::ArchiveReader::read_path_with_options(archive_path, options) {
                    Ok(archive) => archive
                        .extract_with_control(options, |member| {
                            if !member.meta.is_directory {
                                tested_entries += 1;
                                Ok(rars::ExtractionDecision::Extract(Box::new(io::sink())))
                            } else {
                                Ok(rars::ExtractionDecision::Skip)
                            }
                        })
                        .map(|_| ())
                        .map_err(|e| format!("RAR integrity failure: {}", e).into()),
                    Err(e) => Err(format!("RAR open failed: {}", e).into()),
                }
            }
        }
    };

    match result {
        Ok(()) => Ok(IntegritySummary {
            archive_path: archive_path.to_path_buf(),
            total_entries,
            tested_entries,
            passed: true,
            error_message: None,
        }),
        Err(e) => Ok(IntegritySummary {
            archive_path: archive_path.to_path_buf(),
            total_entries,
            tested_entries,
            passed: false,
            error_message: Some(e.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn test_tar_stream_trailer_is_verified() {
        use std::io::Write;

        let temp_dir = PathBuf::from("target").join(format!(
            "test_tar_trailer_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, "a.txt", &b"abc"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let mut compressed = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::new(6));
            encoder.write_all(&tar_bytes).unwrap();
            encoder.finish().unwrap();
        }

        let intact = temp_dir.join("intact.tar.gz");
        fs::write(&intact, &compressed).unwrap();
        let intact_report = test_archive_integrity(
            &intact,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("intact tar.gz test");
        assert!(intact_report.passed);

        // Dropping the gzip trailer leaves the member list readable, so a check that stops at the
        // last member would report success for a stream the decoder cannot finish.
        let truncated = temp_dir.join("truncated.tar.gz");
        fs::write(&truncated, &compressed[..compressed.len() - 8]).unwrap();
        let truncated_report = test_archive_integrity(
            &truncated,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("truncated tar.gz test");
        assert!(
            !truncated_report.passed,
            "a truncated TAR.GZ trailer must fail the integrity check"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_7z_inspect_and_cat_lifecycle() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_7z_inspect_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let sub_dir = temp_dir.join("folder");
        fs::create_dir_all(&sub_dir).unwrap();
        let file1 = sub_dir.join("hello.txt");
        fs::write(&file1, b"Hello from 7z peek test!").unwrap();
        let file2 = sub_dir.join("data.json");
        fs::write(&file2, b"{\"key\":\"value\"}").unwrap();

        let sz_path = temp_dir.join("test_inspect.7z");
        crate::core::pack::pack_archive(&sz_path, &[sub_dir]).expect("pack 7z");

        // 1. Peek archive
        let peek = peek_archive(
            &sz_path,
            5,
            10,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("peek 7z");
        assert_eq!(peek.total_entries, 3); // folder, hello.txt, data.json
        assert!(peek.total_uncompressed_bytes >= 35);
        assert!(peek.has_single_root);
        assert_eq!(peek.root_name.as_deref(), Some("folder"));
        assert_eq!(peek.extension_distribution.get(".txt"), Some(&1));
        assert_eq!(peek.extension_distribution.get(".json"), Some(&1));

        // 2. Cat archive file
        let mut cat_buf = Vec::new();
        cat_archive_file(
            &sz_path,
            "folder/hello.txt",
            None,
            &mut cat_buf,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("cat 7z exact");
        assert_eq!(cat_buf, b"Hello from 7z peek test!");

        // Case-insensitive cat
        let mut cat_buf2 = Vec::new();
        cat_archive_file(
            &sz_path,
            "FOLDER/HELLO.TXT",
            None,
            &mut cat_buf2,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("cat 7z case-insensitive");
        assert_eq!(cat_buf2, b"Hello from 7z peek test!");

        // Cat non-existent file
        let mut cat_missing = Vec::new();
        let res_missing = cat_archive_file(
            &sz_path,
            "folder/missing.txt",
            None,
            &mut cat_missing,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        );
        assert!(res_missing.is_err());

        // Cat directory fails
        let mut cat_dir = Vec::new();
        let res_dir = cat_archive_file(
            &sz_path,
            "folder",
            None,
            &mut cat_dir,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        );
        assert!(res_dir.is_err());

        // 3. Test integrity
        let integrity = test_archive_integrity(
            &sz_path,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("test integrity");
        assert!(integrity.passed);
        assert_eq!(integrity.tested_entries, 2); // 2 non-dir files

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_rar_inspect_and_cat_lifecycle() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_rar_inspect_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let rar_path = temp_dir.join("test_inspect.rar");
        let mut builder = rars::Builder::new(rars::ArchiveVersion::Rar50);
        builder
            .add_directory(b"my_root".to_vec(), None, None)
            .unwrap();
        builder
            .add_bytes(
                b"my_root/greeting.txt".to_vec(),
                b"RAR5 payload content".to_vec(),
                None,
                None,
            )
            .unwrap();
        builder.write_to_path(&rar_path, None).unwrap();

        // 1. Peek archive
        let peek = peek_archive(
            &rar_path,
            5,
            10,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("peek rar");
        assert_eq!(peek.total_entries, 2);
        assert_eq!(peek.total_uncompressed_bytes, 20);
        assert!(peek.has_single_root);
        assert_eq!(peek.root_name.as_deref(), Some("my_root"));
        assert_eq!(peek.extension_distribution.get(".txt"), Some(&1));

        // 2. Cat archive file
        let mut cat_buf = Vec::new();
        cat_archive_file(
            &rar_path,
            "my_root/greeting.txt",
            None,
            &mut cat_buf,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("cat rar");
        assert_eq!(cat_buf, b"RAR5 payload content");

        // Case-insensitive cat
        let mut cat_buf2 = Vec::new();
        cat_archive_file(
            &rar_path,
            "MY_ROOT/GREETING.TXT",
            None,
            &mut cat_buf2,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("cat rar case");
        assert_eq!(cat_buf2, b"RAR5 payload content");

        // 3. Test integrity
        let integrity = test_archive_integrity(
            &rar_path,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("test integrity rar");
        assert!(integrity.passed);
        assert_eq!(integrity.tested_entries, 1);

        // Corrupted archive test
        let mut corrupt_bytes = fs::read(&rar_path).unwrap();
        let last = corrupt_bytes.len() - 5;
        corrupt_bytes[last] ^= 0xFF;
        let corrupt_path = temp_dir.join("corrupted.rar");
        fs::write(&corrupt_path, corrupt_bytes).unwrap();

        let bad_integrity = test_archive_integrity(
            &corrupt_path,
            None,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .unwrap();
        assert!(
            !bad_integrity.passed,
            "Corrupted archive must report failure"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    /// Cat must not depend on the member fitting a decoded buffer: a member above
    /// `RAR_STREAM_DECODE_LIMIT` takes rars' streaming path, and selecting a later
    /// member must still skip the independent ones in front of it.
    #[test]
    fn test_rar_cat_member_above_stream_decode_limit() {
        let temp_dir = PathBuf::from("target").join(format!(
            "test_rar_stream_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let rar_path = temp_dir.join("stream.rar");
        let small = b"front member".to_vec();
        let large: Vec<u8> = (0..RAR_STREAM_DECODE_LIMIT as usize + 4096)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut builder = rars::Builder::new(rars::ArchiveVersion::Rar50).store(true);
        builder
            .add_bytes(b"small.bin".to_vec(), small.clone(), None, None)
            .unwrap();
        builder
            .add_bytes(b"large.bin".to_vec(), large.clone(), None, None)
            .unwrap();
        builder.write_to_path(&rar_path, None).unwrap();

        // Selecting the last member skips the independent one before it.
        let mut from_large = Vec::new();
        cat_archive_file(
            &rar_path,
            "large.bin",
            None,
            &mut from_large,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("cat above limit");
        assert_eq!(from_large.len(), large.len());
        assert_eq!(from_large, large);

        // Selecting the first member stops before the streamed one.
        let mut from_small = Vec::new();
        cat_archive_file(
            &rar_path,
            "small.bin",
            None,
            &mut from_small,
            crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .expect("cat before limit");
        assert_eq!(from_small, small);

        let _ = fs::remove_dir_all(temp_dir);
    }
}
