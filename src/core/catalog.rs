use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use super::codec::StreamCodec;
use super::encoding::{compose_hangul_nfc, decode_entry_name_raw};
use super::format::{
    archive_stem_cleaned, detect_archive_format, dos_time_to_system_time, nt_time_to_system_time,
    open_stream_reader, open_tar_reader, ArchiveFormat,
};
use super::smart::LayoutEntryInfo;

/// One member name in the form every comparison uses: `/` separators, no surrounding slashes.
///
/// Archive names keep their raw bytes as identity; this is only the text two names are compared in.
pub(crate) fn member_form(name: &str) -> String {
    name.replace('\\', "/").trim_matches('/').to_string()
}

/// Rank of one member name against a request: `0` exact, `1` Hangul-composed equal, `2` ASCII
/// case-insensitive equal. `None` means the request does not name that member.
///
/// `peek` displays composed names and `cat` resolves a request by the most exact rank, so
/// `delete`, `rename` and `--member` answer the same question with the same ranks here instead
/// of comparing raw bytes on their own. There is no stage above these three: no similarity, no
/// fuzzy match, and no Unicode case folding that could change a path.
pub(crate) fn member_match_rank(requested: &str, candidate: &str) -> Option<u8> {
    let requested = member_form(requested);
    let candidate = member_form(candidate);
    if requested == candidate {
        return Some(0);
    }
    let requested = compose_hangul_nfc(&requested);
    let candidate = compose_hangul_nfc(&candidate);
    if requested == candidate {
        return Some(1);
    }
    candidate.eq_ignore_ascii_case(&requested).then_some(2)
}

/// The single member-identity test, as the yes/no answer of the rank rule.
pub(crate) fn member_matches(requested: &str, candidate: &str) -> bool {
    member_match_rank(requested, candidate).is_some()
}

/// The suffix of `candidate` that follows the selected `requested` prefix, or `None` when the
/// member is not selected. `Some("")` is the member itself; a subtree yields `Some("/rest")`.
///
/// Selection is identity plus descent at a `/` boundary: both names are split into components and
/// each component pair is decided by `member_match_rank`, so a delete, a rename or a `--member`
/// literal names one member or a whole directory without a second name rule.
pub(crate) fn member_subtree_suffix(requested: &str, candidate: &str) -> Option<String> {
    if member_matches(requested, candidate) {
        return Some(String::new());
    }
    let requested = member_form(requested);
    let candidate = member_form(candidate);
    if requested.is_empty() {
        return None;
    }
    let requested_parts: Vec<&str> = requested.split('/').collect();
    let candidate_parts: Vec<&str> = candidate.split('/').collect();
    if candidate_parts.len() < requested_parts.len()
        || !requested_parts
            .iter()
            .zip(&candidate_parts)
            .all(|(want, have)| member_match_rank(want, have).is_some())
    {
        return None;
    }
    let rest = &candidate_parts[requested_parts.len()..];
    Some(if rest.is_empty() {
        String::new()
    } else {
        format!("/{}", rest.join("/"))
    })
}

/// Uniform archive entry metadata across all supported archive formats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogEntry {
    pub index: usize,
    pub name: String,
    pub display_name: String,
    pub is_dir: bool,
    pub size: u64,
    pub compressed_size: u64,
    pub mtime: Option<SystemTime>,
    pub is_encrypted: bool,
    pub crc: Option<u32>,
}

impl CatalogEntry {
    pub fn to_layout_entry(&self) -> LayoutEntryInfo {
        LayoutEntryInfo {
            name: self.display_name.clone(),
            is_dir: self.is_dir,
            size: self.size,
            compressed_size: self.compressed_size,
        }
    }
}

/// Read the entry catalog of an archive without extracting any files.
/// Preserves entry identity, decodes filenames cleanly (UTF-8, CP949, Hangul NFC),
/// and reports accurate metadata for AI peek and extraction planners.
pub fn read_archive_catalog(
    archive_path: &Path,
    password: Option<&str>,
    max_bytes: u64,
) -> Result<Vec<CatalogEntry>, Box<dyn std::error::Error>> {
    let format = detect_archive_format(archive_path)?;
    match format {
        ArchiveFormat::Zip => read_zip_catalog(archive_path),
        ArchiveFormat::Tar => read_tar_catalog(archive_path, None, max_bytes),
        ArchiveFormat::TarStream(codec) => read_tar_catalog(archive_path, Some(codec), max_bytes),
        ArchiveFormat::Stream(codec) => read_stream_catalog(archive_path, codec, max_bytes),
        ArchiveFormat::SevenZ => read_7z_catalog(archive_path, password),
        ArchiveFormat::Rar => read_rar_catalog(archive_path, password),
    }
}

fn read_zip_catalog(archive_path: &Path) -> Result<Vec<CatalogEntry>, Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entries = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i)?;
        let raw_name = decode_entry_name_raw(entry.name_raw());
        let orig_path = raw_name.replace('\\', "/");
        let display_name = compose_hangul_nfc(&orig_path);

        let mtime = entry
            .last_modified()
            .and_then(super::extract::zip_datetime_to_system_time);

        entries.push(CatalogEntry {
            index: i,
            name: orig_path,
            display_name,
            is_dir: entry.is_dir(),
            size: entry.size(),
            compressed_size: entry.compressed_size(),
            mtime,
            is_encrypted: entry.encrypted(),
            crc: Some(entry.crc32()),
        });
    }

    Ok(entries)
}

fn read_tar_catalog(
    archive_path: &Path,
    codec: Option<StreamCodec>,
    max_bytes: u64,
) -> Result<Vec<CatalogEntry>, Box<dyn std::error::Error>> {
    let reader = open_tar_reader(archive_path, codec, max_bytes)?;
    let mut tar = tar::Archive::new(reader);
    let mut entries = Vec::new();

    for (i, entry_res) in tar.entries()?.enumerate() {
        let entry = entry_res?;
        let path_cow = entry.path()?;
        let orig_path = path_cow.to_string_lossy().replace('\\', "/");
        let display_name = compose_hangul_nfc(&orig_path);

        let mtime = entry
            .header()
            .mtime()
            .ok()
            .and_then(super::format::unix_secs_to_system_time);

        let size = entry.size();
        entries.push(CatalogEntry {
            index: i,
            name: orig_path,
            display_name,
            is_dir: entry.header().entry_type().is_dir(),
            size,
            compressed_size: size,
            mtime,
            is_encrypted: false,
            crc: entry.header().cksum().ok(),
        });
    }

    Ok(entries)
}

fn read_stream_catalog(
    archive_path: &Path,
    codec: StreamCodec,
    max_bytes: u64,
) -> Result<Vec<CatalogEntry>, Box<dyn std::error::Error>> {
    let compressed_size = std::fs::metadata(archive_path)?.len();
    let mtime = std::fs::metadata(archive_path)?.modified().ok();

    // A bare stream carries no size field, so measure it through the decoder under the budget.
    let mut reader = open_stream_reader(archive_path, codec, max_bytes)?;
    let mut buf = [0u8; 8192];
    let mut total_uncompressed = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total_uncompressed = total_uncompressed
            .checked_add(n as u64)
            .ok_or("Archive uncompressed size overflow")?;
    }

    let stem = archive_stem_cleaned(archive_path);
    Ok(vec![CatalogEntry {
        index: 0,
        name: stem.clone(),
        display_name: stem,
        is_dir: false,
        size: total_uncompressed,
        compressed_size,
        mtime,
        is_encrypted: false,
        crc: None,
    }])
}

fn read_7z_catalog(
    archive_path: &Path,
    password: Option<&str>,
) -> Result<Vec<CatalogEntry>, Box<dyn std::error::Error>> {
    let pass = match password {
        Some(p) => sevenz_rust2::Password::from(p),
        None => sevenz_rust2::Password::empty(),
    };

    let sz_reader = super::format::open_7z_reader(archive_path, &pass)?;
    let sz_archive = sz_reader.archive();

    let mut entries = Vec::with_capacity(sz_archive.files.len());
    for (i, file) in sz_archive.files.iter().enumerate() {
        let orig_path = file.name().replace('\\', "/");
        let display_name = compose_hangul_nfc(&orig_path);

        let mtime = if file.has_last_modified_date {
            nt_time_to_system_time(file.last_modified_date.into())
        } else {
            None
        };

        let is_encrypted = sz_archive
            .stream_map
            .file_block_index
            .get(i)
            .copied()
            .flatten()
            .and_then(|b_idx| sz_archive.blocks.get(b_idx))
            .map(|block| {
                block
                    .coders
                    .iter()
                    .any(|c| c.encoder_method_id() == [0x06, 0xF1, 0x07, 0x01])
            })
            .unwrap_or(false);

        entries.push(CatalogEntry {
            index: i,
            name: orig_path,
            display_name,
            is_dir: file.is_directory,
            size: file.size,
            compressed_size: file.compressed_size,
            mtime,
            is_encrypted,
            crc: if file.has_crc {
                Some(file.crc as u32)
            } else {
                None
            },
        });
    }

    Ok(entries)
}

fn read_rar_catalog(
    archive_path: &Path,
    password: Option<&str>,
) -> Result<Vec<CatalogEntry>, Box<dyn std::error::Error>> {
    let mut options = rars::ArchiveReadOptions::default();
    if let Some(p) = password {
        options.password = Some(p.as_bytes());
    }

    let volumes = super::format::find_volume_files(archive_path);
    if volumes.len() > 1 {
        let archives = volumes
            .iter()
            .map(|v| rars::ArchiveReader::read_path_with_options(v, options))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("RAR volume open failed: {}", e))?;
        let members = rars::volume_members(&archives)
            .map_err(|e| format!("RAR volume member read failed: {}", e))?;
        let mut entries = Vec::with_capacity(members.len());
        for (i, member) in members.iter().enumerate() {
            let raw_name = decode_entry_name_raw(&member.meta.name);
            let orig_path = raw_name.replace('\\', "/");
            let display_name = compose_hangul_nfc(&orig_path);

            let mtime = member
                .meta
                .file_time
                .and_then(|ft| match member.meta.family {
                    rars::ArchiveFamily::Rar50Plus => {
                        super::format::unix_secs_to_system_time(ft as u64)
                    }
                    _ => {
                        let dos_date = (ft >> 16) as u16;
                        let dos_time = (ft & 0xFFFF) as u16;
                        dos_time_to_system_time(dos_date, dos_time)
                    }
                });

            entries.push(CatalogEntry {
                index: i,
                name: orig_path,
                display_name,
                is_dir: member.meta.is_directory,
                size: member.meta.unpacked_size,
                compressed_size: member.meta.packed_size,
                mtime,
                is_encrypted: member.meta.is_encrypted,
                crc: None,
            });
        }
        return Ok(entries);
    }

    let archive = rars::ArchiveReader::read_path_with_options(archive_path, options)
        .map_err(|e| format!("RAR open failed: {}", e))?;

    let mut entries = Vec::new();
    for (i, member) in archive.members().enumerate() {
        let raw_name = decode_entry_name_raw(&member.meta.name);
        let orig_path = raw_name.replace('\\', "/");
        let display_name = compose_hangul_nfc(&orig_path);

        let mtime = member
            .meta
            .file_time
            .and_then(|ft| match member.meta.family {
                rars::ArchiveFamily::Rar50Plus => {
                    super::format::unix_secs_to_system_time(ft as u64)
                }
                _ => {
                    let dos_date = (ft >> 16) as u16;
                    let dos_time = (ft & 0xFFFF) as u16;
                    dos_time_to_system_time(dos_date, dos_time)
                }
            });

        entries.push(CatalogEntry {
            index: i,
            name: orig_path,
            display_name,
            is_dir: member.meta.is_directory,
            size: member.meta.unpacked_size,
            compressed_size: member.meta.packed_size,
            mtime,
            is_encrypted: member.meta.is_encrypted,
            crc: None,
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{member_form, member_match_rank, member_matches, member_subtree_suffix};

    /// The same name in Hangul syllables and in conjoining jamo, as macOS and Linux write it.
    const COMPOSED: &str = "\u{d55c}\u{ae00}.txt";
    const DECOMPOSED: &str = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}.txt";

    #[test]
    fn member_rank_answers_exact_then_composed_then_case() {
        assert_eq!(member_match_rank("notes.txt", "notes.txt"), Some(0));
        assert_eq!(member_match_rank(COMPOSED, DECOMPOSED), Some(1));
        assert_eq!(member_match_rank(DECOMPOSED, COMPOSED), Some(1));
        assert_eq!(member_match_rank("notes.txt", "NOTES.TXT"), Some(2));
    }

    #[test]
    fn member_identity_answers_exact_composed_and_case_in_the_cat_order() {
        assert!(member_matches("notes.txt", "notes.txt"));
        assert!(member_matches(COMPOSED, DECOMPOSED));
        assert!(member_matches(DECOMPOSED, COMPOSED));
        assert!(member_matches("notes.txt", "NOTES.TXT"));
        assert!(member_matches("dir/notes.txt", "dir\\notes.txt"));
        assert!(member_matches("/notes.txt", "notes.txt"));
    }

    #[test]
    fn member_identity_rejects_unrelated_names() {
        assert!(!member_matches(COMPOSED, "\u{b2e4}\u{b978}.txt"));
        assert!(!member_matches(COMPOSED, "notes.txt"));
        assert!(!member_matches("notes.txt", "notes.md"));
        assert!(!member_matches("notes.txt", "notes.txt.bak"));
        assert_eq!(member_match_rank(COMPOSED, "\u{b2e4}\u{b978}.txt"), None);
    }

    #[test]
    fn member_selection_descends_only_at_a_slash_boundary() {
        assert_eq!(member_subtree_suffix("dir", "dir"), Some(String::new()));
        assert_eq!(
            member_subtree_suffix("dir", "dir/sub/f.txt"),
            Some("/sub/f.txt".into())
        );
        assert_eq!(
            member_subtree_suffix("dir/", "dir/f.txt"),
            Some("/f.txt".into())
        );
        assert_eq!(member_form("dir"), "dir");
        assert_eq!(member_subtree_suffix("dir", "dirish/f.txt"), None);
        assert_eq!(
            member_subtree_suffix("dir", "DIR/f.txt"),
            Some("/f.txt".into())
        );
        // A decomposed subtree is named by its composed prefix and keeps the raw suffix.
        let composed_dir = "\u{d55c}\u{ae00}";
        let decomposed_dir = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}";
        assert_eq!(
            member_subtree_suffix(composed_dir, &format!("{decomposed_dir}/file.txt")),
            Some("/file.txt".into())
        );
        assert_eq!(
            member_subtree_suffix(COMPOSED, DECOMPOSED),
            Some(String::new())
        );
    }
}
