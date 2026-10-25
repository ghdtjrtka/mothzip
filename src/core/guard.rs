use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("Zip-Slip detected: entry '{0}' attempts path traversal outside target directory")]
    ZipSlip(String),
    #[error("Archive data size ({size_gb:.2} GiB) exceeds configured byte budget ({limit_gb:.2} GiB); use --max-bytes to select a larger budget")]
    ZipBomb { size_gb: f64, limit_gb: f64 },
    #[error("Reserved DOS device name detected in entry: '{0}' (CON, PRN, AUX, NUL, COM1-9, LPT1-9 are prohibited)")]
    ReservedDeviceName(String),
    #[error(
        "NTFS Alternate Data Stream detected in entry: '{0}' (colon ':' character is prohibited)"
    )]
    AlternateDataStream(String),
    #[error("Illegal path component in entry: '{0}' (trailing dots or spaces are prohibited on Windows)")]
    IllegalPathCharacters(String),
}

/// Default maximum allowed uncompressed size in bytes (10 GB)
pub const DEFAULT_MAX_UNCOMPRESSED_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Budget the entire decompressed TAR stream, including padding and trailing data.
pub struct BudgetReader<R> {
    inner: R,
    remaining: u64,
}
impl<R: Read> BudgetReader<R> {
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    pub fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}
impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return if self.inner.read(&mut probe)? == 0 {
                Ok(0)
            } else {
                Err(io::Error::other(
                    "Decompressed archive stream exceeds byte budget",
                ))
            };
        }
        let length = output
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut output[..length])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// Enforces uncompressed byte bounds on writes, defending against decompressed stream bombs.
pub struct BudgetWriter<W: Write> {
    inner: W,
    remaining: u64,
    written: u64,
}

impl<W: Write> BudgetWriter<W> {
    pub fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            written: 0,
        }
    }

    #[allow(dead_code)]
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    #[allow(dead_code)]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for BudgetWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::other(
                "Decompressed archive stream exceeds byte budget",
            ));
        }
        let to_write = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.write(&buf[..to_write])?;
        self.remaining -= n as u64;
        self.written += n as u64;
        if n < buf.len() && self.remaining == 0 {
            return Err(io::Error::other(
                "Decompressed archive stream exceeds byte budget",
            ));
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

const DOS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$", "CLOCK$",
];

/// Check whether a filename or stem matches Windows reserved DOS device names.
/// Ceiling on a RAR5 compressed member's declared dictionary size.
///
/// The decoder allocates a window of the declared size, so a hostile archive could ask for
/// gigabytes before writing a byte. Real archives stay far below this: RAR5 tools default to
/// 32-64 MiB, and the largest dictionary this product writes (7z at level 9) is 64 MiB. The
/// limit is a policy ceiling, not a measurement of any particular file.
pub const RAR_DICTIONARY_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

pub fn is_reserved_dos_device(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    DOS_RESERVED_NAMES.contains(&upper.as_str())
}

/// Sanitize an internal archive entry path and resolve it safely inside `base_dir`.
/// Defends against:
/// 1. Zip-Slip (`..`, root dir, prefix)
/// 2. NTFS Alternate Data Streams (`:`)
/// 3. Windows DOS device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`, `CLOCK$`)
/// 4. Trailing dots and spaces which alter Windows path resolution
/// 5. Cross-platform invalid characters (`<`, `>`, `"`, `|`, `?`, `*`) sanitized to `_`
pub fn sanitize_entry_path(base_dir: &Path, raw_path: &str) -> Result<PathBuf, GuardError> {
    // 1. Check for Alternate Data Stream (ADS) and ASCII control characters
    if raw_path.contains(':') {
        return Err(GuardError::AlternateDataStream(raw_path.to_string()));
    }
    if raw_path.chars().any(|c| (c as u32) < 0x20) {
        return Err(GuardError::IllegalPathCharacters(raw_path.to_string()));
    }

    // Normalize: strip leading slashes or dots that indicate relative/root positioning
    let trimmed = raw_path.trim_start_matches(['/', '\\']);
    let p = Path::new(trimmed);

    // 2. Check each component for traversal, illegal chars, and DOS devices
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                return Err(GuardError::ZipSlip(raw_path.to_string()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(GuardError::ZipSlip(raw_path.to_string()));
            }
            Component::Normal(comp_os) => {
                let comp_raw = comp_os.to_string_lossy();

                // Sanitize invalid cross-platform filename characters: < > " | ? * -> _
                let comp_str: String = comp_raw
                    .chars()
                    .map(|c| match c {
                        '<' | '>' | '"' | '|' | '?' | '*' => '_',
                        _ => c,
                    })
                    .collect();

                // 3. Prohibit trailing dots or spaces in Windows paths
                if comp_str.ends_with('.') || comp_str.ends_with(' ') {
                    return Err(GuardError::IllegalPathCharacters(raw_path.to_string()));
                }

                // 4. Prohibit DOS device names (e.g. con, con.txt, aux.tar.gz, clock$)
                let base_name = comp_str.split('.').next().unwrap_or("");
                let file_stem = Path::new(&comp_str)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if is_reserved_dos_device(base_name) || is_reserved_dos_device(file_stem) {
                    return Err(GuardError::ReservedDeviceName(raw_path.to_string()));
                }
            }
            Component::CurDir => {}
        }
    }

    let mut dest = base_dir.to_path_buf();
    for comp in p.components() {
        if let Component::Normal(c) = comp {
            let sanitized: String = c
                .to_string_lossy()
                .chars()
                .map(|ch| match ch {
                    '<' | '>' | '"' | '|' | '?' | '*' => '_',
                    _ => ch,
                })
                .collect();
            dest.push(sanitized);
        }
    }

    // Double check directory containment and non-empty target
    if !dest.starts_with(base_dir) || dest == base_dir {
        return Err(GuardError::ZipSlip(raw_path.to_string()));
    }

    Ok(dest)
}

/// Check whether the total uncompressed size exceeds safe bounds.
pub fn check_zip_bomb(total_uncompressed_bytes: u64, max_allowed: u64) -> Result<(), GuardError> {
    if total_uncompressed_bytes > max_allowed {
        let size_gb = total_uncompressed_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let limit_gb = max_allowed as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(GuardError::ZipBomb { size_gb, limit_gb });
    }
    Ok(())
}

/// Copy only the permitted bytes, then probe EOF without writing an extra byte.
/// A declared size is an exact integrity constraint, not a tolerance or an allocation hint.
pub fn copy_archive_entry<R: Read + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    expected: Option<u64>,
    already_written: u64,
    limit: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let remaining = limit
        .checked_sub(already_written)
        .ok_or("Archive byte budget exhausted")?;
    let bound = match expected {
        Some(size) => {
            let total = already_written
                .checked_add(size)
                .ok_or("Archive size overflow")?;
            check_zip_bomb(total, limit)?;
            size
        }
        None => remaining,
    };
    let written = io::copy(&mut reader.take(bound), writer)?;
    if expected.is_some() && written != bound {
        return Err(format!(
            "Corrupt archive: declared {} bytes, read {}",
            bound, written
        )
        .into());
    }
    let mut probe = [0u8; 1];
    if reader.read(&mut probe)? != 0 {
        return Err("Archive entry exceeds its declared size or remaining byte budget".into());
    }
    Ok(written)
}

/// Safely drain an unselected archive entry into a null sink while verifying bounds and advancing solid decoders.
pub fn drain_archive_entry<R: Read + ?Sized>(
    reader: &mut R,
    expected: Option<u64>,
    limit: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut sink = io::sink();
    copy_archive_entry(reader, &mut sink, expected, 0, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_stream_budget_covers_trailing_data() {
        let mut reader = BudgetReader::new(&b"123"[..], 2);
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).is_err());
        assert_eq!(output, b"12");
        let mut reader = BudgetReader::new(&b"12"[..], 2);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"12");
    }

    #[test]
    fn copy_budget_rejects_large_headers_before_writing() {
        let mut input = &b"payload"[..];
        let mut output = Vec::new();
        assert!(copy_archive_entry(&mut input, &mut output, Some(7), 2, 8).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn copy_budget_never_writes_probe_byte_and_detects_truncation() {
        let mut output = Vec::new();
        assert!(copy_archive_entry(&mut &b"12345"[..], &mut output, None, 2, 6).is_err());
        assert_eq!(output, b"1234");
        let mut output = Vec::new();
        assert!(copy_archive_entry(&mut &b"12"[..], &mut output, Some(3), 0, 6).is_err());
        assert_eq!(output, b"12");
        let mut output = Vec::new();
        assert_eq!(
            copy_archive_entry(&mut &b"1234"[..], &mut output, Some(4), 2, 6).unwrap(),
            4
        );
    }

    #[test]
    fn test_zip_slip_rejection() {
        let base = Path::new("C:\\safe\\extract");
        assert!(sanitize_entry_path(base, "../evil.exe").is_err());
        assert!(sanitize_entry_path(base, "sub/../../evil.exe").is_err());
        assert!(sanitize_entry_path(base, "C:\\Windows\\system32").is_err());
        assert!(sanitize_entry_path(base, "/").is_err());
        assert!(sanitize_entry_path(base, "///").is_err());
    }

    #[test]
    fn test_ads_rejection() {
        let base = Path::new("C:\\safe\\extract");
        assert!(sanitize_entry_path(base, "file.txt:hidden.exe").is_err());
        assert!(sanitize_entry_path(base, "folder/sub:stream").is_err());
        assert!(sanitize_entry_path(base, "folder/evil\0file.txt").is_err());
    }

    #[test]
    fn test_dos_device_rejection() {
        let base = Path::new("C:\\safe\\extract");
        assert!(sanitize_entry_path(base, "con").is_err());
        assert!(sanitize_entry_path(base, "CON.txt").is_err());
        assert!(sanitize_entry_path(base, "con.tar.gz").is_err());
        assert!(sanitize_entry_path(base, "folder/aux").is_err());
        assert!(sanitize_entry_path(base, "folder/NUL.dat").is_err());
        assert!(sanitize_entry_path(base, "com1/file.txt").is_err());
        assert!(sanitize_entry_path(base, "clock$").is_err());
        assert!(sanitize_entry_path(base, "sub/CLOCK$.txt").is_err());
    }

    #[test]
    fn test_trailing_dot_space_rejection() {
        let base = Path::new("C:\\safe\\extract");
        assert!(sanitize_entry_path(base, "folder./file.txt").is_err());
        assert!(sanitize_entry_path(base, "folder /file.txt").is_err());
        assert!(sanitize_entry_path(base, "file.txt.").is_err());
        assert!(sanitize_entry_path(base, "folder/file\x1f.txt").is_err());
    }

    #[test]
    fn test_valid_paths() {
        let base = Path::new("C:\\safe\\extract");
        let safe = sanitize_entry_path(base, "folder/sub/file.txt").unwrap();
        assert_eq!(safe, base.join("folder").join("sub").join("file.txt"));

        // Leading slashes should be normalized safely
        let safe_leading = sanitize_entry_path(base, "/folder/sub/file.txt").unwrap();
        assert_eq!(
            safe_leading,
            base.join("folder").join("sub").join("file.txt")
        );

        // Cross-platform illegal characters should be sanitized to '_'
        let cross_platform = sanitize_entry_path(base, "folder/report<draft>?*|.pdf").unwrap();
        assert_eq!(
            cross_platform,
            base.join("folder").join("report_draft____.pdf")
        );
    }
}
