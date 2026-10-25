use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::codec::StreamCodec;

/// Shell and CLI supported archive extensions. Kept in sync with [`NAME_SUFFIXES`] by
/// `test_supported_extensions_match_name_suffixes`.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    ".zip", ".7z", ".rar", ".tar", ".gz", ".tgz", ".tar.gz", ".zst", ".tar.zst", ".xz", ".tar.xz",
    ".lz4", ".tar.lz4", ".br", ".tar.br",
];

/// Return the list of archive extensions supported by MothZip.
pub fn supported_extensions() -> &'static [&'static str] {
    SUPPORTED_EXTENSIONS
}

/// Container and stream codec are separate decisions: a container is TAR or a single member,
/// and the codec is what compresses the byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    SevenZ,
    Rar,
    /// Uncompressed TAR container.
    Tar,
    /// TAR container behind a stream codec (`.tar.gz`, `.tar.zst`, `.tar.xz`, ...).
    TarStream(StreamCodec),
    /// A single member behind a stream codec (`.gz`, `.zst`, `.xz`, ...).
    Stream(StreamCodec),
}

impl ArchiveFormat {
    /// Codec compressing the byte stream, or `None` for uncompressed formats.
    pub fn codec(self) -> Option<StreamCodec> {
        match self {
            Self::TarStream(codec) | Self::Stream(codec) => Some(codec),
            _ => None,
        }
    }

    /// Name used in user-facing messages, e.g. `TAR.GZ` or `Zstd`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::SevenZ => "7Z",
            Self::Rar => "RAR",
            Self::Tar => "TAR",
            Self::TarStream(StreamCodec::Gzip) => "TAR.GZ",
            Self::TarStream(StreamCodec::Zstd) => "TAR.ZST",
            Self::TarStream(StreamCodec::Xz) => "TAR.XZ",
            Self::TarStream(StreamCodec::Lz4) => "TAR.LZ4",
            Self::TarStream(StreamCodec::Brotli) => "TAR.BR",
            Self::Stream(codec) => codec.name(),
        }
    }
}

/// One table behind name classification, pack destination format, and extension listing.
/// Longest suffix first, so `.tar.gz` wins over `.gz`.
const NAME_SUFFIXES: &[(&str, ArchiveFormat)] = &[
    (".tar.gz", ArchiveFormat::TarStream(StreamCodec::Gzip)),
    (".tgz", ArchiveFormat::TarStream(StreamCodec::Gzip)),
    (".tar.zst", ArchiveFormat::TarStream(StreamCodec::Zstd)),
    (".tar.xz", ArchiveFormat::TarStream(StreamCodec::Xz)),
    (".tar.lz4", ArchiveFormat::TarStream(StreamCodec::Lz4)),
    (".tar.br", ArchiveFormat::TarStream(StreamCodec::Brotli)),
    (".tar", ArchiveFormat::Tar),
    (".zip", ArchiveFormat::Zip),
    (".7z", ArchiveFormat::SevenZ),
    (".rar", ArchiveFormat::Rar),
    (".gz", ArchiveFormat::Stream(StreamCodec::Gzip)),
    (".zst", ArchiveFormat::Stream(StreamCodec::Zstd)),
    (".xz", ArchiveFormat::Stream(StreamCodec::Xz)),
    (".lz4", ArchiveFormat::Stream(StreamCodec::Lz4)),
    (".br", ArchiveFormat::Stream(StreamCodec::Brotli)),
];

/// Format named by a file name's extension, without opening the file.
pub fn format_for_name(name: &str) -> Option<ArchiveFormat> {
    let lower = name.to_ascii_lowercase();
    NAME_SUFFIXES
        .iter()
        .find(|(suffix, _)| lower.ends_with(suffix))
        .map(|(_, format)| *format)
}

/// Format whose archive name suffix is exactly `suffix` (no leading dot), e.g. `tar.zst`.
pub fn from_suffix(suffix: &str) -> Option<ArchiveFormat> {
    let needle = format!(".{}", suffix.to_ascii_lowercase());
    NAME_SUFFIXES
        .iter()
        .find(|(candidate, _)| *candidate == needle)
        .map(|(_, format)| *format)
}

/// Name of the single file a bare stream archive yields, e.g. `data.txt` for `data.txt.zst`.
fn strip_archive_extension(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    if let Some((suffix, _)) = NAME_SUFFIXES
        .iter()
        .find(|(suffix, _)| lower.ends_with(suffix))
    {
        let without_ext = &name[..name.len() - suffix.len()];
        if *suffix == ".rar" {
            let lower_without = &lower[..lower.len() - suffix.len()];
            if let Some(pos) = lower_without.rfind(".part") {
                let part_suffix = &lower_without[pos + 5..];
                if !part_suffix.is_empty() && part_suffix.chars().all(|c| c.is_ascii_digit()) {
                    return &name[..pos];
                }
            }
        }
        return without_ext;
    }
    // Multi-volume 7z: archive.7z.001 -> archive
    if let Some(pos) = lower.rfind(".7z.") {
        let suffix = &lower[pos + 4..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return &name[..pos];
        }
    }
    // Multi-volume RAR: archive.r00, archive.r01 -> archive
    if let Some(last_4) = lower.get(lower.len().saturating_sub(4)..) {
        if last_4.starts_with(".r") && last_4[2..].chars().all(|c| c.is_ascii_digit()) {
            return &name[..name.len() - 4];
        }
    }
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

/// Open a decompressing reader over `path` for one stream codec, bounded by `max_bytes`.
pub fn open_stream_reader(
    path: &Path,
    codec: StreamCodec,
    max_bytes: u64,
) -> Result<Box<dyn Read>, Box<dyn std::error::Error>> {
    let reader = super::codec::decoder(File::open(path)?, codec)?;
    Ok(Box::new(super::guard::BudgetReader::new(reader, max_bytes)))
}

/// Shared TAR stream producer for extraction, planning, inspection, and cat.
/// `codec` is `None` for an uncompressed TAR.
pub fn open_tar_reader(
    path: &Path,
    codec: Option<StreamCodec>,
    max_bytes: u64,
) -> Result<Box<dyn Read>, Box<dyn std::error::Error>> {
    match codec {
        Some(codec) => open_stream_reader(path, codec, max_bytes),
        None => Ok(Box::new(super::guard::BudgetReader::new(
            File::open(path)?,
            max_bytes,
        ))),
    }
}

/// A stream file may still contain a TAR container (`data.zst` produced by `tar czf`-style
/// tooling). Sniff the TAR magic through the decoder so both naming habits work.
fn tar_container_behind(path: &Path, codec: StreamCodec) -> bool {
    let Ok(reader) = super::codec::decoder(
        match File::open(path) {
            Ok(file) => file,
            Err(_) => return false,
        },
        codec,
    ) else {
        return false;
    };
    let mut reader = reader.take(512);
    let mut header = [0u8; 512];
    let mut filled = 0usize;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    filled == header.len() && &header[257..262] == b"ustar"
}

/// Classify a codec-compressed file as TAR container or single member.
fn stream_format(path: &Path, codec: StreamCodec) -> ArchiveFormat {
    if tar_container_behind(path, codec) {
        ArchiveFormat::TarStream(codec)
    } else {
        ArchiveFormat::Stream(codec)
    }
}

/// Extract clean base stem from archive filename, stripping multi-extension formats
/// and sanitizing trailing/leading dots to prevent path traversal (e.g. "...zip" -> "archive").
pub fn archive_stem_cleaned(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");

    let raw_stem = strip_archive_extension(name);
    let trimmed = raw_stem.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == ".." || crate::core::guard::is_reserved_dos_device(trimmed)
    {
        "archive".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Check if a filename or path indicates a supported archive format
pub fn is_supported_archive(path_str: &str) -> bool {
    let lower = path_str.to_ascii_lowercase();
    if lower.ends_with(".exe")
        && super::sfx::embedded_archive(Path::new(path_str)).is_ok_and(|value| value.is_some())
    {
        return true;
    }
    if format_for_name(&lower).is_some() {
        return true;
    }
    if let Some(pos) = lower.rfind(".7z.") {
        let suffix = &lower[pos + 4..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    if let Some(last_4) = lower.get(lower.len().saturating_sub(4)..) {
        if last_4.starts_with(".r") && last_4[2..].chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// Detect the archive format from file extension and magic byte inspection
pub fn detect_archive_format(path: &Path) -> Result<ArchiveFormat, Box<dyn std::error::Error>> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if let Some(format) = format_for_name(&name) {
        return Ok(match format {
            // A `.zst`/`.xz`/`.lz4`/`.br`/`.gz` file may still hold a TAR container.
            ArchiveFormat::Stream(codec) => stream_format(path, codec),
            other => other,
        });
    }
    if name.contains(".7z.") {
        return Ok(ArchiveFormat::SevenZ);
    }
    if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| {
            suffix.starts_with(".r") && suffix[2..].chars().all(|c| c.is_ascii_digit())
        })
    {
        return Ok(ArchiveFormat::Rar);
    }

    // Inspect magic bytes from file header
    let mut file = File::open(path)?;
    let mut magic = [0u8; 8];
    let n = file.read(&mut magic)?;
    drop(file);

    // 7z magic: '7' 'z' 0xBC 0xAF 0x27 0x1C
    if n >= 6 && magic[0..6] == [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C] {
        return Ok(ArchiveFormat::SevenZ);
    }

    // RAR magic: 'R' 'a' 'r' '!' 0x1A 0x07 (v1.5-4.x and v5.0+)
    if n >= 6 && magic[0..6] == [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07] {
        return Ok(ArchiveFormat::Rar);
    }

    // Zip magic: 'P' 'K' 0x03 0x04 or 'P' 'K' 0x05 0x06
    if n >= 4 && magic[0] == 0x50 && magic[1] == 0x4B {
        return Ok(ArchiveFormat::Zip);
    }

    // Stream codec magics for extension-less files. Brotli defines no magic bytes, so a
    // `.br` without its name is only supported through the extension.
    let stream_codec = if n >= 2 && magic[0] == 0x1F && magic[1] == 0x8B {
        Some(StreamCodec::Gzip)
    } else if n >= 4 && magic[0..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        Some(StreamCodec::Zstd)
    } else if n >= 6 && magic[0..6] == [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00] {
        Some(StreamCodec::Xz)
    } else if n >= 4 && magic[0..4] == [0x04, 0x22, 0x4D, 0x18] {
        Some(StreamCodec::Lz4)
    } else {
        None
    };
    if let Some(codec) = stream_codec {
        return Ok(stream_format(path, codec));
    }

    // Tar header magic check at offset 257
    if n >= 8 {
        if let Ok(mut f) = File::open(path) {
            let mut header = [0u8; 512];
            if f.read_exact(&mut header).is_ok() && &header[257..262] == b"ustar" {
                return Ok(ArchiveFormat::Tar);
            }
        }
    }

    // Default to Zip for unclassified files
    Ok(ArchiveFormat::Zip)
}

/// Checked conversion from unix seconds to std::time::SystemTime without addition overflow
pub fn unix_secs_to_system_time(secs: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Convert 7z NtTime (100-nanosecond intervals since Jan 1, 1601 UTC) to std::time::SystemTime
pub fn nt_time_to_system_time(nt: u64) -> Option<SystemTime> {
    const NT_TO_UNIX_SECONDS: u64 = 11_644_473_600;
    let secs = (nt / 10_000_000).checked_sub(NT_TO_UNIX_SECONDS)?;
    let nanos = ((nt % 10_000_000) * 100) as u32;
    UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
}

/// Convert SystemTime to 7z NtTime
#[allow(dead_code)]
pub fn system_time_to_nt_time(st: SystemTime) -> Option<u64> {
    const NT_TO_UNIX_SECONDS: u64 = 11_644_473_600;
    let dur = st.duration_since(UNIX_EPOCH).ok()?;
    let total_secs = dur.as_secs().checked_add(NT_TO_UNIX_SECONDS)?;
    let nt = total_secs
        .checked_mul(10_000_000)?
        .checked_add((dur.subsec_nanos() / 100) as u64)?;
    Some(nt)
}

/// Convert DOS FAT date/time to SystemTime
pub fn dos_time_to_system_time(dos_date: u16, dos_time: u16) -> Option<SystemTime> {
    let year = ((dos_date >> 9) & 0x7F) as i32 + 1980;
    let month = ((dos_date >> 5) & 0x0F) as u8;
    let day = (dos_date & 0x1F) as u8;
    let hour = ((dos_time >> 11) & 0x1F) as u8;
    let min = ((dos_time >> 5) & 0x3F) as u8;
    let sec = ((dos_time & 0x1F) * 2) as u8;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let dt = zip::DateTime::from_date_and_time(year as u16, month, day, hour, min, sec).ok()?;
    zip_datetime_to_system_time(dt)
}

#[cfg(windows)]
#[allow(non_snake_case, dead_code, clippy::upper_case_acronyms)]
mod win_time {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Copy, Clone, Default, Debug)]
    pub struct SYSTEMTIME {
        pub wYear: u16,
        pub wMonth: u16,
        pub wDayOfWeek: u16,
        pub wDay: u16,
        pub wHour: u16,
        pub wMinute: u16,
        pub wSecond: u16,
        pub wMilliseconds: u16,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Default, Debug)]
    pub struct FILETIME {
        pub dwLowDateTime: u32,
        pub dwHighDateTime: u32,
    }

    extern "system" {
        pub fn TzSpecificLocalTimeToSystemTime(
            lpTimeZoneInformation: *const c_void,
            lpLocalTime: *const SYSTEMTIME,
            lpUniversalTime: *mut SYSTEMTIME,
        ) -> i32;

        pub fn SystemTimeToTzSpecificLocalTime(
            lpTimeZoneInformation: *const c_void,
            lpUniversalTime: *const SYSTEMTIME,
            lpLocalTime: *mut SYSTEMTIME,
        ) -> i32;

        pub fn SystemTimeToFileTime(
            lpSystemTime: *const SYSTEMTIME,
            lpFileTime: *mut FILETIME,
        ) -> i32;

        pub fn FileTimeToSystemTime(
            lpFileTime: *const FILETIME,
            lpSystemTime: *mut SYSTEMTIME,
        ) -> i32;
    }
}

/// Convert civil date (year, month, day) to days since Unix epoch (Howard Hinnant algorithm)
#[allow(dead_code)]
pub fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Convert days since Unix epoch to civil date (year, month, day) (Howard Hinnant algorithm)
#[allow(dead_code)]
pub fn civil_from_days(days: i64) -> (i32, u8, u8) {
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

/// Convert zip::DateTime (DOS local wall-clock time) to std::time::SystemTime (UTC)
#[cfg(windows)]
pub fn zip_datetime_to_system_time(dt: zip::DateTime) -> Option<SystemTime> {
    use win_time::*;
    let local = SYSTEMTIME {
        wYear: dt.year(),
        wMonth: dt.month() as u16,
        wDayOfWeek: 0,
        wDay: dt.day() as u16,
        wHour: dt.hour() as u16,
        wMinute: dt.minute() as u16,
        wSecond: dt.second() as u16,
        wMilliseconds: 0,
    };
    let mut utc = SYSTEMTIME::default();
    if unsafe { TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) } == 0 {
        return None;
    }
    let mut ft = FILETIME::default();
    if unsafe { SystemTimeToFileTime(&utc, &mut ft) } == 0 {
        return None;
    }
    let ft_u64 = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
    const WINDOWS_TICK: u64 = 10_000_000;
    const SEC_TO_UNIX_EPOCH: u64 = 11_644_473_600;
    const TICKS_TO_UNIX_EPOCH: u64 = SEC_TO_UNIX_EPOCH * WINDOWS_TICK;
    if ft_u64 >= TICKS_TO_UNIX_EPOCH {
        let dur_ticks = ft_u64 - TICKS_TO_UNIX_EPOCH;
        let secs = dur_ticks / WINDOWS_TICK;
        let nanos = (dur_ticks % WINDOWS_TICK) * 100;
        UNIX_EPOCH.checked_add(Duration::new(secs, nanos as u32))
    } else {
        let dur_ticks = TICKS_TO_UNIX_EPOCH - ft_u64;
        let secs = dur_ticks / WINDOWS_TICK;
        let nanos = (dur_ticks % WINDOWS_TICK) * 100;
        UNIX_EPOCH.checked_sub(Duration::new(secs, nanos as u32))
    }
}

#[cfg(not(windows))]
pub fn zip_datetime_to_system_time(dt: zip::DateTime) -> Option<SystemTime> {
    let days = days_from_civil(dt.year() as i64, dt.month() as u64, dt.day() as u64);
    let secs =
        days * 86400 + (dt.hour() as i64 * 3600) + (dt.minute() as i64 * 60) + (dt.second() as i64);
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        None
    }
}

/// Convert std::time::SystemTime (UTC) to zip::DateTime (DOS local wall-clock time)
#[cfg(windows)]
pub fn system_time_to_zip_datetime(st: SystemTime) -> Option<zip::DateTime> {
    use win_time::*;
    const WINDOWS_TICK: u64 = 10_000_000;
    const SEC_TO_UNIX_EPOCH: u64 = 11_644_473_600;
    const TICKS_TO_UNIX_EPOCH: u64 = SEC_TO_UNIX_EPOCH * WINDOWS_TICK;

    let ft_u64 = match st.duration_since(UNIX_EPOCH) {
        Ok(dur) => {
            let ticks = dur
                .as_secs()
                .checked_mul(WINDOWS_TICK)?
                .checked_add((dur.subsec_nanos() as u64) / 100)?;
            TICKS_TO_UNIX_EPOCH.checked_add(ticks)?
        }
        Err(err) => {
            let dur = err.duration();
            let ticks = dur
                .as_secs()
                .checked_mul(WINDOWS_TICK)?
                .checked_add((dur.subsec_nanos() as u64) / 100)?;
            TICKS_TO_UNIX_EPOCH.checked_sub(ticks)?
        }
    };

    let ft = FILETIME {
        dwLowDateTime: ft_u64 as u32,
        dwHighDateTime: (ft_u64 >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&ft, &mut utc) } == 0 {
        return None;
    }
    let mut local = SYSTEMTIME::default();
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return None;
    }
    if !(1980..=2099).contains(&local.wYear) {
        return None;
    }
    zip::DateTime::from_date_and_time(
        local.wYear,
        local.wMonth as u8,
        local.wDay as u8,
        local.wHour as u8,
        local.wMinute as u8,
        local.wSecond as u8,
    )
    .ok()
}

#[cfg(not(windows))]
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

/// Read + Seek across multiple sliced volume files (e.g. split.7z.001, split.7z.002, ...)
pub struct MultiVolumeReader {
    paths: Vec<PathBuf>,
    sizes: Vec<u64>,
    total_size: u64,
    current_pos: u64,
    current_file: Option<(usize, File)>,
}

impl MultiVolumeReader {
    pub fn new(paths: Vec<PathBuf>) -> io::Result<Self> {
        if paths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No volume files provided",
            ));
        }
        let mut sizes = Vec::with_capacity(paths.len());
        let mut total_size: u64 = 0;
        for path in &paths {
            let meta = std::fs::metadata(path)?;
            let len = meta.len();
            sizes.push(len);
            total_size = total_size.checked_add(len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Volume set total size overflow")
            })?;
        }
        Ok(Self {
            paths,
            sizes,
            total_size,
            current_pos: 0,
            current_file: None,
        })
    }

    #[allow(dead_code)]
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    #[allow(dead_code)]
    pub fn volumes(&self) -> &[PathBuf] {
        &self.paths
    }
}

impl Read for MultiVolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.current_pos >= self.total_size {
            return Ok(0);
        }

        let mut offset_in_file = self.current_pos;
        let mut target_idx = 0;
        for (i, &size) in self.sizes.iter().enumerate() {
            if offset_in_file < size {
                target_idx = i;
                break;
            }
            offset_in_file -= size;
        }

        let needs_open = match &self.current_file {
            Some((idx, _)) => *idx != target_idx,
            None => true,
        };

        if needs_open {
            let file = File::open(&self.paths[target_idx])?;
            self.current_file = Some((target_idx, file));
        }

        let (_, file) = self.current_file.as_mut().unwrap();
        file.seek(SeekFrom::Start(offset_in_file))?;

        let bytes_left_in_volume = self.sizes[target_idx] - offset_in_file;
        let max_to_read = (buf.len() as u64).min(bytes_left_in_volume) as usize;

        let n = file.read(&mut buf[..max_to_read])?;
        self.current_pos += n as u64;
        Ok(n)
    }
}

impl Seek for MultiVolumeReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(off) => off as i64,
            SeekFrom::End(off) => (self.total_size as i64) + off,
            SeekFrom::Current(off) => (self.current_pos as i64) + off,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Seek before volume start",
            ));
        }

        self.current_pos = new_pos as u64;
        Ok(self.current_pos)
    }
}

/// Discover companion volume files for multi-part archives (7z, RAR)
pub fn find_volume_files(archive_path: &Path) -> Vec<PathBuf> {
    let file_name = match archive_path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return vec![archive_path.to_path_buf()],
    };
    let parent = archive_path.parent().unwrap_or(Path::new("."));

    // Case 1: Split 7z like archive.7z.001, archive.7z.002 ...
    if let Some(pos) = file_name.to_ascii_lowercase().rfind(".7z.") {
        let suffix = &file_name[pos + 4..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            let width = suffix.len();
            let prefix = &file_name[..pos + 4]; // includes "archive.7z."
            let mut vols = Vec::new();
            let mut idx = 1;
            loop {
                let candidate_name = format!("{}{:0width$}", prefix, idx, width = width);
                let candidate_path = parent.join(&candidate_name);
                if candidate_path.is_file() {
                    vols.push(candidate_path);
                    idx += 1;
                } else {
                    break;
                }
            }
            if !vols.is_empty() {
                return vols;
            }
        }
    }

    // Case 2: Multi-part RAR like archive.part1.rar or archive.part01.rar
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".rar") {
        let stem = &file_name[..file_name.len() - 4];
        let lower_stem = &lower[..lower.len() - 4];
        if let Some(pos) = lower_stem.rfind(".part") {
            let num_str = &stem[pos + 5..];
            if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
                let width = num_str.len();
                let base = &stem[..pos + 5]; // includes "archive.part"
                let mut vols = Vec::new();
                let mut idx = 1;
                loop {
                    let candidate_name = format!("{}{:0width$}.rar", base, idx, width = width);
                    let candidate_path = parent.join(&candidate_name);
                    if candidate_path.is_file() {
                        vols.push(candidate_path);
                        idx += 1;
                    } else {
                        break;
                    }
                }
                if !vols.is_empty() {
                    return vols;
                }
            }
        }

        // Case 3: Old-style multi-volume RAR: archive.rar, archive.r00, archive.r01 ...
        let r00_candidate = parent.join(format!("{}.r00", stem));
        if r00_candidate.is_file() {
            let mut vols = vec![archive_path.to_path_buf()];
            let mut idx = 0;
            loop {
                let cand_name = format!("{}.r{:02}", stem, idx);
                let cand_path = parent.join(&cand_name);
                if cand_path.is_file() {
                    vols.push(cand_path);
                    idx += 1;
                } else {
                    break;
                }
            }
            return vols;
        }
    }

    vec![archive_path.to_path_buf()]
}

/// Open a 7z archive reader, seamlessly supporting both single .7z files and multi-volume (.001, .002...) sets.
pub fn open_7z_reader(
    archive_path: &Path,
    password: &sevenz_rust2::Password,
) -> Result<sevenz_rust2::ArchiveReader<MultiVolumeReader>, Box<dyn std::error::Error>> {
    let volumes = find_volume_files(archive_path);
    let mv_reader = MultiVolumeReader::new(volumes)
        .map_err(|e| format!("Failed to open 7z volume set: {}", e))?;
    let reader = sevenz_rust2::ArchiveReader::new(mv_reader, password.clone())
        .map_err(|e| format!("7z open failed: {}", e))?;
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_stem_cleaned() {
        assert_eq!(
            archive_stem_cleaned(Path::new("linux-6.10.tar.gz")),
            "linux-6.10"
        );
        assert_eq!(archive_stem_cleaned(Path::new("project.tgz")), "project");
        assert_eq!(archive_stem_cleaned(Path::new("archive.tar")), "archive");
        assert_eq!(archive_stem_cleaned(Path::new("backup.zip")), "backup");
        assert_eq!(archive_stem_cleaned(Path::new("bundle.7z")), "bundle");
        assert_eq!(archive_stem_cleaned(Path::new("bundle.7z.001")), "bundle");
        assert_eq!(archive_stem_cleaned(Path::new("document.rar")), "document");
        assert_eq!(
            archive_stem_cleaned(Path::new("document.part1.rar")),
            "document"
        );
        assert_eq!(
            archive_stem_cleaned(Path::new("document.part02.rar")),
            "document"
        );
        assert_eq!(archive_stem_cleaned(Path::new("document.r00")), "document");
        assert_eq!(
            archive_stem_cleaned(Path::new("C:\\data\\test.tar.gz")),
            "test"
        );
        assert_eq!(archive_stem_cleaned(Path::new("...zip")), "archive");
        assert_eq!(archive_stem_cleaned(Path::new("...7z")), "archive");
        assert_eq!(archive_stem_cleaned(Path::new("...tar")), "archive");
    }

    #[test]
    fn test_is_supported_archive() {
        assert!(is_supported_archive("my_backup.zip"));
        assert!(is_supported_archive("source.tar.gz"));
        assert!(is_supported_archive("source.tgz"));
        assert!(is_supported_archive("archive.tar"));
        assert!(is_supported_archive("archive.7z"));
        assert!(is_supported_archive("archive.7z.001"));
        assert!(is_supported_archive("archive.rar"));
        assert!(is_supported_archive("archive.part1.rar"));
        assert!(is_supported_archive("archive.r00"));
        assert!(!is_supported_archive("document.docx"));
        assert!(!is_supported_archive("folder"));
    }

    #[test]
    fn test_nt_time_roundtrip() {
        let now = SystemTime::now();
        if let Some(nt) = system_time_to_nt_time(now) {
            let recovered = nt_time_to_system_time(nt).expect("nt conversion");
            let diff = if recovered > now {
                recovered.duration_since(now).unwrap()
            } else {
                now.duration_since(recovered).unwrap()
            };
            assert!(diff.as_micros() <= 1);
        }
    }

    /// `SUPPORTED_EXTENSIONS` feeds shell registration, so it must list exactly the suffixes the
    /// classifier recognizes; this fixed test keeps the two lists from drifting apart.
    #[test]
    fn test_supported_extensions_match_name_suffixes() {
        let mut expected: Vec<&str> = NAME_SUFFIXES.iter().map(|(suffix, _)| *suffix).collect();
        let mut declared: Vec<&str> = SUPPORTED_EXTENSIONS.to_vec();
        expected.sort_unstable();
        declared.sort_unstable();
        assert_eq!(declared, expected);
        for (suffix, format) in NAME_SUFFIXES {
            assert_eq!(format_for_name(&format!("data{}", suffix)), Some(*format));
        }
    }

    #[test]
    fn test_stream_and_tar_suffix_classification() {
        assert_eq!(
            format_for_name("logs.tar.zst"),
            Some(ArchiveFormat::TarStream(StreamCodec::Zstd))
        );
        assert_eq!(
            format_for_name("data.txt.zst"),
            Some(ArchiveFormat::Stream(StreamCodec::Zstd))
        );
        assert_eq!(
            format_for_name("rootfs.tar.xz"),
            Some(ArchiveFormat::TarStream(StreamCodec::Xz))
        );
        assert_eq!(
            format_for_name("image.tar.lz4"),
            Some(ArchiveFormat::TarStream(StreamCodec::Lz4))
        );
        assert_eq!(
            format_for_name("app.js.br"),
            Some(ArchiveFormat::Stream(StreamCodec::Brotli))
        );
        assert_eq!(
            format_for_name("payload.br"),
            Some(ArchiveFormat::Stream(StreamCodec::Brotli))
        );
        assert_eq!(
            archive_stem_cleaned(Path::new("logs.tar.zst")),
            "logs",
            "tar container stem drops both suffixes"
        );
        assert_eq!(
            archive_stem_cleaned(Path::new("data.txt.zst")),
            "data.txt",
            "single member keeps its own name"
        );
        assert_eq!(archive_stem_cleaned(Path::new("app.js.br")), "app.js");
    }
}
