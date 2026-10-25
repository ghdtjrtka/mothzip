//! Row text for the archive list.
//!
//! Every cell is derived from a [`CatalogEntry`], so drawing a row never touches the archive
//! again: opening the window stays a single central-directory read.

use std::time::SystemTime;

use crate::core::catalog::CatalogEntry;

use super::native;

/// One list column: header text, width at 96 DPI and numeric alignment.
pub struct Column {
    pub title: &'static str,
    pub base_width: i32,
    pub right_aligned: bool,
}

/// Columns in display order. The widths are the 96-DPI layout, scaled to the window's DPI.
pub const COLUMNS: [Column; 5] = [
    Column {
        title: "이름",
        base_width: 330,
        right_aligned: false,
    },
    Column {
        title: "크기",
        base_width: 110,
        right_aligned: true,
    },
    Column {
        title: "압축 크기",
        base_width: 110,
        right_aligned: true,
    },
    Column {
        title: "수정일시",
        base_width: 150,
        right_aligned: false,
    },
    Column {
        title: "암호",
        base_width: 70,
        right_aligned: false,
    },
];

/// A directory is marked with a trailing separator so it cannot be confused with a file of the
/// same name; the catalog already stores the separator, this only keeps it single.
pub fn name_text(entry: &CatalogEntry) -> String {
    if entry.is_dir {
        format!("{}/", entry.display_name.trim_end_matches('/'))
    } else {
        entry.display_name.clone()
    }
}

/// Directories own no byte count here, so their size cells stay empty instead of advertising a
/// zero that would read like an empty file.
pub fn size_text(entry: &CatalogEntry) -> String {
    if entry.is_dir {
        String::new()
    } else {
        format_bytes(entry.size)
    }
}

/// Same rule as [`size_text`] for the compressed column.
pub fn compressed_text(entry: &CatalogEntry) -> String {
    if entry.is_dir {
        String::new()
    } else {
        format_bytes(entry.compressed_size)
    }
}

/// Local time, because the archive's timestamps are compared against the file manager's clock.
pub fn mtime_text(entry: &CatalogEntry) -> String {
    entry.mtime.and_then(local_timestamp).unwrap_or_default()
}

/// The password column answers one question: does opening this member need a password?
pub fn encrypted_text(entry: &CatalogEntry) -> String {
    if entry.is_encrypted {
        "예".to_string()
    } else {
        String::new()
    }
}

/// Human byte size; the unit follows the value so the column stays readable at any archive size.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    const BYTES_PER_UNIT: u64 = 1024;

    if bytes < BYTES_PER_UNIT {
        return format!("{bytes} B");
    }
    let mut scaled = bytes as f64;
    let mut unit = 0;
    while scaled >= BYTES_PER_UNIT as f64 && unit + 1 < UNITS.len() {
        scaled /= BYTES_PER_UNIT as f64;
        unit += 1;
    }
    format!("{scaled:.1} {}", UNITS[unit])
}

fn local_timestamp(time: SystemTime) -> Option<String> {
    let local = native::local_system_time(time)?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64) -> CatalogEntry {
        CatalogEntry {
            index: 0,
            name: name.to_string(),
            display_name: name.to_string(),
            is_dir,
            size,
            compressed_size: size,
            mtime: None,
            is_encrypted: false,
            crc: None,
        }
    }

    #[test]
    fn directory_rows_keep_exactly_one_separator() {
        assert_eq!(name_text(&entry("src/", true, 0)), "src/");
        assert_eq!(name_text(&entry("src", true, 0)), "src/");
        assert_eq!(name_text(&entry("src/main.rs", false, 3)), "src/main.rs");
    }

    #[test]
    fn directory_sizes_are_blank_not_zero() {
        let folder = entry("src/", true, 0);
        assert_eq!(size_text(&folder), "");
        assert_eq!(compressed_text(&folder), "");
        assert_eq!(size_text(&entry("empty.txt", false, 0)), "0 B");
    }

    #[test]
    fn byte_sizes_switch_unit_at_each_step() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn encryption_column_marks_only_encrypted_entries() {
        assert_eq!(encrypted_text(&entry("a", false, 1)), "");
        let mut locked = entry("a", false, 1);
        locked.is_encrypted = true;
        assert_eq!(encrypted_text(&locked), "예");
    }
}
