//! The Win32 pieces the window shares: UTF-16 conversion, DPI scaling, the shell UI font and the
//! time conversion the list needs.
//!
//! This module is the only place in `ui` that owns a handle whose lifetime the caller must end
//! (the font), so the acquire/release pair lives together here.

use std::ffi::c_void;
use std::fs::Metadata;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{FILETIME, HWND, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT, HGDIOBJ};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
};

/// The DPI every Win32 layout in this module is authored at.
pub const BASE_DPI: u32 = 96;

// `GetDpiForWindow`, `FileTimeToLocalFileTime` and `FileTimeToSystemTime` live in `windows-sys`
// modules whose features (`Win32_UI_HiDpi`, `Win32_Storage_FileSystem`) are not enabled in
// `Cargo.toml`, which this worker must not edit; these declarations repeat the shipped
// user32/kernel32 ABI instead of pulling a broader feature set in.
#[link(name = "user32")]
extern "system" {
    fn GetDpiForWindow(hwnd: HWND) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn FileTimeToLocalFileTime(file_time: *const FILETIME, local_file_time: *mut FILETIME) -> i32;
    fn FileTimeToSystemTime(file_time: *const FILETIME, system_time: *mut SYSTEMTIME) -> i32;
}

/// NUL-terminated UTF-16 for any Win32 text parameter.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The window's DPI, or the authored 96 when the window cannot report one; scaling by zero would
/// collapse every metric derived from it.
pub fn dpi_of(window: HWND) -> u32 {
    // SAFETY: `window` is a live window handle owned by the caller's thread.
    match unsafe { GetDpiForWindow(window) } {
        0 => BASE_DPI,
        dpi => dpi,
    }
}

/// Scale an authored-at-96-DPI pixel count to `dpi`.
pub fn scale(pixels: i32, dpi: u32) -> i32 {
    (i64::from(pixels) * i64::from(dpi) / i64::from(BASE_DPI)) as i32
}

/// The shell's message font, scaled to `dpi`, so the list matches every other Windows list; a null
/// handle means "keep the system default", which the callers already tolerate.
pub fn ui_font(dpi: u32) -> HFONT {
    let mut metrics: NONCLIENTMETRICSW = unsafe { std::mem::zeroed() };
    metrics.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
    // SAFETY: `metrics` is an initialised, correctly sized NONCLIENTMETRICSW the call may write.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&mut metrics as *mut NONCLIENTMETRICSW).cast::<c_void>(),
            0,
        )
    };
    if ok == 0 {
        return std::ptr::null_mut();
    }
    metrics.lfMessageFont.lfHeight = scale(metrics.lfMessageFont.lfHeight, dpi);
    metrics.lfMessageFont.lfWidth = scale(metrics.lfMessageFont.lfWidth, dpi);
    // SAFETY: `lfMessageFont` is an initialised LOGFONTW owned by `metrics` for the call.
    unsafe { CreateFontIndirectW(&metrics.lfMessageFont) }
}

/// Release a font returned by [`ui_font`]; a null handle is the documented "no font" case.
pub fn destroy_font(font: HFONT) {
    if font.is_null() {
        return;
    }
    // SAFETY: `font` came from CreateFontIndirectW and is deleted exactly once, after the
    // controls that used it have been destroyed.
    unsafe {
        DeleteObject(font as HGDIOBJ);
    }
}

/// `FILE_ATTRIBUTE_REPARSE_POINT` marks a symlink, a junction or another filesystem redirect.
/// `windows-sys` exposes the constant only behind the storage feature this crate does not enable,
/// so the documented value lives here next to the probe that uses it.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// Whether `metadata` describes a reparse point. Writing "into" one would put the files somewhere
/// other than the path says, so callers that trust a path must reject it before using it as a
/// destination.
pub fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Convert a timestamp to the local wall clock the user compares against Explorer.
pub fn local_system_time(time: SystemTime) -> Option<SYSTEMTIME> {
    /// 100 ns ticks, the FILETIME unit.
    const TICKS_PER_SECOND: u64 = 10_000_000;
    /// Seconds from the FILETIME epoch (1601) to the Unix epoch.
    const UNIX_EPOCH_IN_SECONDS: u64 = 11_644_473_600;

    let since_epoch = time.duration_since(UNIX_EPOCH).ok()?;
    let ticks = since_epoch
        .as_secs()
        .checked_mul(TICKS_PER_SECOND)?
        .checked_add(u64::from(since_epoch.subsec_nanos()) / (1_000_000_000 / TICKS_PER_SECOND))?
        .checked_add(UNIX_EPOCH_IN_SECONDS.checked_mul(TICKS_PER_SECOND)?)?;
    let utc = FILETIME {
        dwLowDateTime: (ticks & u64::from(u32::MAX)) as u32,
        dwHighDateTime: (ticks >> u32::BITS) as u32,
    };
    let mut local = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: both arguments point at live FILETIME values.
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0 {
        return None;
    }
    let mut system_time: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: `local` is a live FILETIME and `system_time` is writable.
    if unsafe { FileTimeToSystemTime(&local, &mut system_time) } == 0 {
        return None;
    }
    Some(system_time)
}

/// Path text for a window title, falling back to the whole path when it has no final component.
pub fn display_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wide_strings_end_with_exactly_one_nul() {
        let text = wide("가/나.txt");
        assert_eq!(text.last(), Some(&0));
        assert_eq!(text.iter().filter(|unit| **unit == 0).count(), 1);
    }

    #[test]
    fn scaling_keeps_the_authored_size_at_96_dpi() {
        assert_eq!(scale(150, BASE_DPI), 150);
        assert_eq!(scale(150, 192), 300);
        assert_eq!(scale(0, 144), 0);
    }

    #[test]
    fn local_time_round_trips_the_unix_epoch_to_a_full_year() {
        let stamp = local_system_time(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let stamp = stamp.expect("the Unix epoch is representable as local time");
        assert!(stamp.wYear >= 2023 && stamp.wYear <= 2024);
        assert!((1..=12).contains(&stamp.wMonth));
    }

    #[test]
    fn a_plain_directory_is_not_a_reparse_point() {
        let dir = std::env::temp_dir().join(format!("mothzip-native-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the probe directory");
        let metadata = std::fs::symlink_metadata(&dir).expect("read the probe directory");
        assert!(!is_reparse_point(&metadata));
        std::fs::remove_dir_all(&dir).ok();
    }
}
