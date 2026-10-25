//! The report-mode list that shows the catalog.
//!
//! Rows are appended in catalog order and never sorted or removed, so a row index *is* the catalog
//! index; selection is reported as indices and the caller resolves the member without reading the
//! control's text back.

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    LVCFMT_LEFT, LVCFMT_RIGHT, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVITEMW,
    LVM_GETITEMCOUNT, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVNI_SELECTED, LVS_EX_DOUBLEBUFFER,
    LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_REPORT, LVS_SHOWSELALWAYS, WC_LISTVIEWW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, SendMessageW, WS_CHILD, WS_EX_CLIENTEDGE, WS_TABSTOP, WS_VISIBLE,
};

use crate::core::catalog::CatalogEntry;

use super::{format, native};

/// Create the list as a child of `parent`. Returns a null handle when the control class is
/// unavailable, which the caller reports as a creation failure instead of panicking.
pub fn create(parent: HWND) -> HWND {
    // SAFETY: the module handle of the running executable is a process-wide constant.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: every pointer is either static control-class data or a live parent handle; the
    // control copies its own state, so no Rust value has to outlive the call.
    let list = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            WC_LISTVIEWW,
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | LVS_REPORT | LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            parent,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if !list.is_null() {
        let extended = LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER;
        // SAFETY: `list` is the control created above.
        unsafe {
            SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, extended as LPARAM);
        }
    }
    list
}

/// Install the five columns at the window's DPI.
pub fn set_columns(list: HWND, dpi: u32) {
    for (index, column) in format::COLUMNS.iter().enumerate() {
        let mut title = native::wide(column.title);
        let mut layout = LVCOLUMNW {
            mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT,
            fmt: if column.right_aligned {
                LVCFMT_RIGHT
            } else {
                LVCFMT_LEFT
            },
            cx: native::scale(column.base_width, dpi),
            pszText: title.as_mut_ptr(),
            cchTextMax: 0,
            iSubItem: 0,
            iImage: 0,
            iOrder: 0,
            cxMin: 0,
            cxDefault: 0,
            cxIdeal: 0,
        };
        // SAFETY: `layout` and `title` are live for the call, and the control copies the header
        // text into its own storage.
        unsafe {
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                index as WPARAM,
                (&mut layout as *mut LVCOLUMNW).cast::<core::ffi::c_void>() as LPARAM,
            );
        }
    }
}

/// Append `entries` in order. A refused row ends the fill, because the row-index-to-entry mapping
/// is what the rest of the window relies on; [`row_count`] reports how many rows survived.
pub fn populate(list: HWND, entries: &[CatalogEntry]) {
    for (row, entry) in entries.iter().enumerate() {
        if !insert_row(list, row, &format::name_text(entry)) {
            break;
        }
        let cells = [
            format::size_text(entry),
            format::compressed_text(entry),
            format::mtime_text(entry),
            format::encrypted_text(entry),
        ];
        for (offset, text) in cells.iter().enumerate() {
            set_cell(list, row, offset + 1, text);
        }
    }
}

/// Selected row indices in ascending order, as reported by the control itself.
pub fn selected_rows(list: HWND) -> Vec<usize> {
    let mut rows = Vec::new();
    let mut from = FIRST_ROW;
    loop {
        // SAFETY: `list` is a live control; the message only reads its selection state.
        let next = unsafe {
            SendMessageW(
                list,
                LVM_GETNEXTITEM,
                from as WPARAM,
                LVNI_SELECTED as LPARAM,
            )
        };
        if next < 0 {
            break;
        }
        rows.push(next as usize);
        from = next as isize;
    }
    rows
}

/// Rows the control actually holds; the status line reports this rather than the catalog length.
pub fn row_count(list: HWND) -> usize {
    // SAFETY: `list` is a live control; the message only returns its item count.
    unsafe { SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as usize }
}

/// `LVM_GETNEXTITEM` starts from -1 to ask for the first matching row.
const FIRST_ROW: isize = -1;

fn insert_row(list: HWND, row: usize, name: &str) -> bool {
    let mut text = native::wide(name);
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.mask = LVIF_TEXT;
    item.iItem = row as i32;
    item.iSubItem = 0;
    item.pszText = text.as_mut_ptr();
    // SAFETY: `item` and `text` are live for the call; the control copies the text it needs.
    let inserted = unsafe {
        SendMessageW(
            list,
            LVM_INSERTITEMW,
            0,
            (&mut item as *mut LVITEMW).cast::<core::ffi::c_void>() as LPARAM,
        )
    };
    inserted == row as isize
}

fn set_cell(list: HWND, row: usize, column: usize, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut buffer = native::wide(text);
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.iSubItem = column as i32;
    item.pszText = buffer.as_mut_ptr();
    // SAFETY: `item` and `buffer` are live for the call; LVM_SETITEMTEXTW copies the text.
    unsafe {
        SendMessageW(
            list,
            LVM_SETITEMTEXTW,
            row as WPARAM,
            (&mut item as *mut LVITEMW).cast::<core::ffi::c_void>() as LPARAM,
        );
    }
}
