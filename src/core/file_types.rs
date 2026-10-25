//! One catalog for supported suffixes and embedded file-icon resource identities.
//! Keep the order stable; append new formats to avoid changing existing resource IDs.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    ".zip", ".7z", ".rar", ".tar", ".gz", ".tgz", ".tar.gz", ".zst", ".tar.zst", ".xz", ".tar.xz",
    ".lz4", ".tar.lz4", ".br", ".tar.br",
];

pub const GENERIC_ARCHIVE_ICON_ID: i32 = 100;

pub const fn file_icon_resource_id(index: usize) -> i32 {
    GENERIC_ARCHIVE_ICON_ID + 1 + index as i32
}
