use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
    Tar,
    Gz,
}

/// Extract clean base stem from archive filename, stripping multi-extension formats
/// and sanitizing trailing/leading dots to prevent path traversal (e.g. "...zip" -> "archive").
pub fn archive_stem_cleaned(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("extracted");
    let lower = name.to_lowercase();
    let raw_stem = if lower.ends_with(".tar.gz") {
        &name[..name.len() - 7]
    } else if lower.ends_with(".tgz") || lower.ends_with(".tar") || lower.ends_with(".zip") {
        &name[..name.len() - 4]
    } else if lower.ends_with(".gz") {
        &name[..name.len() - 3]
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("extracted")
    };

    let trimmed = raw_stem.trim_matches(['.', ' ', '/', '\\']);
    if trimmed.is_empty() || trimmed == ".." || crate::core::guard::is_reserved_dos_device(trimmed) {
        "archive".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Check if a filename or path indicates a supported archive format
pub fn is_supported_archive(path_str: &str) -> bool {
    let lower = path_str.to_lowercase();
    lower.ends_with(".zip")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar")
        || lower.ends_with(".gz")
}

/// Detect the archive format from file extension and magic byte inspection
pub fn detect_archive_format(path: &Path) -> Result<ArchiveFormat, Box<dyn std::error::Error>> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Ok(ArchiveFormat::TarGz);
    }
    if name.ends_with(".tar") {
        return Ok(ArchiveFormat::Tar);
    }
    if name.ends_with(".zip") {
        return Ok(ArchiveFormat::Zip);
    }
    if name.ends_with(".gz") {
        // Inspect if gzipped stream is a tar archive or plain gzip file
        if let Ok(file) = File::open(path) {
            let mut gz = flate2::read::GzDecoder::new(file);
            let mut header = [0u8; 512];
            if gz.read_exact(&mut header).is_ok() && &header[257..262] == b"ustar" {
                return Ok(ArchiveFormat::TarGz);
            }
        }
        return Ok(ArchiveFormat::Gz);
    }

    // Fallback: inspect magic bytes from file header
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic)?;

    if n >= 2 && magic[0] == 0x1F && magic[1] == 0x8B {
        drop(file);
        if let Ok(f) = File::open(path) {
            let mut gz = flate2::read::GzDecoder::new(f);
            let mut header = [0u8; 512];
            if gz.read_exact(&mut header).is_ok() && &header[257..262] == b"ustar" {
                return Ok(ArchiveFormat::TarGz);
            }
        }
        return Ok(ArchiveFormat::Gz);
    }
    if n >= 4 && magic[0] == 0x50 && magic[1] == 0x4B {
        return Ok(ArchiveFormat::Zip);
    }

    // Default to Zip for standard zip archives without extension
    Ok(ArchiveFormat::Zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_stem_cleaned() {
        assert_eq!(archive_stem_cleaned(Path::new("linux-6.10.tar.gz")), "linux-6.10");
        assert_eq!(archive_stem_cleaned(Path::new("project.tgz")), "project");
        assert_eq!(archive_stem_cleaned(Path::new("archive.tar")), "archive");
        assert_eq!(archive_stem_cleaned(Path::new("backup.zip")), "backup");
        assert_eq!(archive_stem_cleaned(Path::new("C:\\data\\test.tar.gz")), "test");
        assert_eq!(archive_stem_cleaned(Path::new("...zip")), "archive");
        assert_eq!(archive_stem_cleaned(Path::new("...tar")), "archive");
    }

    #[test]
    fn test_is_supported_archive() {
        assert!(is_supported_archive("my_backup.zip"));
        assert!(is_supported_archive("source.tar.gz"));
        assert!(is_supported_archive("source.tgz"));
        assert!(is_supported_archive("archive.tar"));
        assert!(!is_supported_archive("document.docx"));
        assert!(!is_supported_archive("folder"));
    }
}
