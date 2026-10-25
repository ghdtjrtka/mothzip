//! Archive browsing window.
//!
//! Double-clicking an archive opens this window instead of unpacking: it lists the members from
//! the shared catalog producer and only extracts what the user asks for. The window owns no
//! archive logic of its own — layout, members, selection, extraction and integrity checks all come
//! from the `core` producers the CLI already uses.

mod format;
mod job;
mod list_view;
mod native;

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, FALSE, HWND, LPARAM, LRESULT, TRUE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::UpdateWindow;
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WC_STATICW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, AppendMenuW, CreateAcceleratorTableW, CreateMenu, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyAcceleratorTable, DestroyMenu, DispatchMessageW,
    DrawMenuBar, EnableMenuItem, GetClientRect, GetMenu, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, LoadCursorW, LoadIconW, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    TranslateAcceleratorW, TranslateMessage, UnregisterClassW, ACCEL, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, FVIRTKEY, GWLP_USERDATA, HACCEL, HMENU, IDC_ARROW, IDI_APPLICATION,
    MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, SM_CYMENU,
    SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_SETFONT,
    WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::core::catalog::{read_archive_catalog, CatalogEntry};

use job::{JobKind, JobOutcome, JobRequest, JobResult};

/// What the window needs to show one archive.
pub struct BrowseRequest {
    /// Archive to list; the window never writes next to it until an action is chosen.
    pub archive: PathBuf,
    /// Password for encrypted archives, when one was supplied on the command line.
    pub password: Option<String>,
    /// Uncompressed byte budget shared with every extraction and preview.
    pub max_bytes: u64,
}

/// Window class name; one class serves every browsing window in this process.
const WINDOW_CLASS: &str = "MothZipBrowseWindow";

/// Client area requested at 96 DPI; scaled to the window's DPI once the window exists.
const WINDOW_WIDTH: i32 = 900;
const WINDOW_HEIGHT: i32 = 560;

/// Status line height at 96 DPI: one row of the UI font plus breathing room.
const STATUS_HEIGHT: i32 = 26;

/// Rows the list will hold. A pathological archive must not turn opening a window into a
/// multi-second stall, so only the leading members are inserted and the status line says how many
/// of the total are visible.
const MAX_ROWS: usize = 50_000;

/// Menu command ids. They travel in the low word of `WM_COMMAND`, so they stay below 0x8000.
const ID_EXTRACT_ALL: u32 = 1001;
const ID_EXTRACT_SELECTED: u32 = 1002;
const ID_TEST: u32 = 1003;
const ID_CLOSE: u32 = 1004;

/// The low word of a message parameter, where a `WM_COMMAND` command id arrives.
const LOWORD_MASK: WPARAM = 0xFFFF;

/// Test-only override: when set, an extraction writes here instead of opening the folder picker.
/// The shipped behaviour is always the picker; see `audit/delegation/gui-window-handoff.md`.
const TEST_EXTRACT_DIR_ENV: &str = "MOTHZIP_UI_TEST_EXTRACT_DIR";

/// One window's state.
///
/// The allocation is owned by [`open_archive_window`] for the whole message loop; the window
/// procedure only borrows it through `GWLP_USERDATA`, and nothing else in the process touches it.
struct App {
    window: HWND,
    list: HWND,
    status: HWND,
    menu: HMENU,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
    dpi: u32,
    archive: PathBuf,
    password: Option<String>,
    max_bytes: u64,
    entries: Vec<CatalogEntry>,
    /// True while a worker owns the archive; the action menu is refused during that time.
    busy: bool,
}

impl App {
    fn new(archive: PathBuf, password: Option<String>, max_bytes: u64) -> Self {
        Self {
            window: std::ptr::null_mut(),
            list: std::ptr::null_mut(),
            status: std::ptr::null_mut(),
            menu: std::ptr::null_mut(),
            font: std::ptr::null_mut(),
            dpi: native::BASE_DPI,
            archive,
            password,
            max_bytes,
            entries: Vec::new(),
            busy: false,
        }
    }

    fn set_status(&self, text: &str) {
        if self.status.is_null() {
            return;
        }
        let message = native::wide(text);
        // SAFETY: `message` is a live NUL-terminated string and `status` is this window's control.
        unsafe {
            SetWindowTextW(self.status, message.as_ptr());
        }
    }

    /// Grey the actions that would fight the running worker; `닫기` stays reachable so the window
    /// can always be dismissed.
    fn set_actions_enabled(&self, enabled: bool) {
        if self.menu.is_null() {
            return;
        }
        let flag = if enabled { MF_ENABLED } else { MF_GRAYED };
        // SAFETY: `menu` is the menu installed on `window` and outlives every call.
        unsafe {
            for id in [ID_EXTRACT_ALL, ID_EXTRACT_SELECTED, ID_TEST] {
                EnableMenuItem(self.menu, id, MF_BYCOMMAND | flag);
            }
            DrawMenuBar(self.window);
        }
    }

    /// Read the catalog and fill the list. A failure stays on screen: the window is already open,
    /// and closing it would throw away the only place the reason can be shown.
    fn load_catalog(&mut self) {
        match read_archive_catalog(&self.archive, self.password.as_deref(), self.max_bytes) {
            Ok(entries) => {
                self.entries = entries;
                let visible = self.entries.len().min(MAX_ROWS);
                list_view::populate(self.list, &self.entries[..visible]);
                let shown = list_view::row_count(self.list);
                self.set_status(&catalog_status(self.entries.len(), shown));
                self.set_actions_enabled(true);
            }
            Err(error) => {
                self.entries.clear();
                self.set_status(&format!("목록을 읽지 못했습니다: {error}"));
                self.set_actions_enabled(false);
            }
        }
    }
}

/// Open the browsing window and run its message loop until the user closes it.
///
/// Nothing beside the archive is written until the user picks an action and a destination folder.
pub fn open_archive_window(request: &BrowseRequest) -> Result<(), Box<dyn std::error::Error>> {
    let archive = request.archive.clone();
    if !archive.is_file() {
        return Err(format!("아카이브를 찾을 수 없습니다: {}", archive.display()).into());
    }
    // SAFETY: the module handle of the running executable is a process-wide constant.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        return Err("실행 파일 핸들을 얻지 못해 창을 만들 수 없습니다".into());
    }

    let class_name = native::wide(WINDOW_CLASS);
    let mut class: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    class.style = CS_HREDRAW | CS_VREDRAW;
    class.lpfnWndProc = Some(window_proc);
    class.hInstance = instance;
    // SAFETY: both loads use the documented predefined-resource identifiers.
    class.hIcon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
    // SAFETY: as above; the arrow cursor is a shared system resource.
    class.hCursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };
    class.hbrBackground = (COLOR_WINDOW + 1) as isize as HBRUSH;
    class.lpszClassName = class_name.as_ptr();
    // SAFETY: `class` is a fully initialised WNDCLASSEXW naming our own procedure.
    if unsafe { RegisterClassExW(&class) } == 0 {
        // SAFETY: reading the per-thread last-error code.
        let reason = unsafe { GetLastError() };
        if reason != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format!("창 클래스를 등록하지 못했습니다 (오류 {reason})").into());
        }
    }

    let menu = create_menu();
    if menu.is_null() {
        // SAFETY: no window uses the class yet and this process just registered it.
        unsafe {
            UnregisterClassW(class_name.as_ptr(), instance);
        }
        return Err("메뉴를 만들지 못했습니다".into());
    }

    let state = Box::into_raw(Box::new(App::new(
        archive.clone(),
        request.password.clone(),
        request.max_bytes,
    )));
    let title = native::wide(&format!("{} — MothZip", native::display_name_of(&archive)));
    // SAFETY: every pointer is live for the call, and `state` outlives the window: the allocation
    // is freed after the message loop in this function.
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            native::scale(WINDOW_WIDTH, native::BASE_DPI),
            native::scale(WINDOW_HEIGHT, native::BASE_DPI),
            std::ptr::null_mut(),
            menu,
            instance,
            state.cast::<core::ffi::c_void>().cast_const(),
        )
    };
    if window.is_null() {
        // SAFETY: the window was never created, so `state` is still owned here alone.
        drop(unsafe { Box::from_raw(state) });
        // SAFETY: both handles were created above and no window is left to use them.
        unsafe {
            DestroyMenu(menu);
            UnregisterClassW(class_name.as_ptr(), instance);
        }
        return Err(
            "아카이브 목록 창을 만들지 못했습니다. 창을 만들 수 있는 세션인지 확인하세요".into(),
        );
    }

    // SAFETY: `window` is a live top-level window.
    unsafe {
        ShowWindow(window, SW_SHOW);
        UpdateWindow(window);
    }
    {
        // SAFETY: `state` is the allocation this window was created with, and no other reference
        // exists while the loop has not started.
        let app = unsafe { &mut *state };
        app.load_catalog();
        // SAFETY: focusing a live child of a live window.
        unsafe {
            SetFocus(app.list);
        }
    }

    let accelerators = create_accelerators();
    let outcome = pump_messages(window, accelerators);

    if !accelerators.is_null() {
        // SAFETY: the table was created above and the loop that used it has ended.
        unsafe {
            DestroyAcceleratorTable(accelerators);
        }
    }
    {
        // SAFETY: the window is destroyed, so nothing else reads the state.
        let app = unsafe { &mut *state };
        native::destroy_font(app.font);
        app.font = std::ptr::null_mut();
    }
    // SAFETY: `state` came from `Box::into_raw` above and is dropped exactly once here.
    drop(unsafe { Box::from_raw(state) });
    // SAFETY: the class was registered above and its last window is gone.
    unsafe {
        UnregisterClassW(class_name.as_ptr(), instance);
    }
    outcome
}

/// Drag the message loop; exits cleanly on `WM_QUIT`, with an error when the queue itself fails.
fn pump_messages(window: HWND, accelerators: HACCEL) -> Result<(), Box<dyn std::error::Error>> {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is a live MSG that the call fills in.
        match unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } {
            0 => return Ok(()),
            -1 => return Err("메시지 루프가 실패해 창을 닫았습니다".into()),
            _ => {}
        }
        // Shortcuts become the same command ids the menu sends, and the command must go to the
        // window that owns the menu rather than to the focused child control.
        if !accelerators.is_null()
            // SAFETY: `message` is a live MSG and `accelerators` is a live table.
            && unsafe { TranslateAcceleratorW(window, accelerators, &message) } != 0
        {
            continue;
        }
        // SAFETY: `message` was filled by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// The window procedure; the unsafe trampoline is the only place the callback ABI appears.
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    handle_message(window, message, wparam, lparam)
}

fn handle_message(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_CREATE {
        // SAFETY: WM_CREATE carries the CREATESTRUCTW of this creation, whose lpCreateParams is
        // the `state` pointer passed to CreateWindowExW.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state = create.lpCreateParams.cast::<App>();
        if state.is_null() {
            return -1;
        }
        // SAFETY: `state` came from Box::into_raw in open_archive_window and outlives the window.
        let app = unsafe { &mut *state };
        app.window = window;
        // SAFETY: GWLP_USERDATA is unused by this class until now.
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);
        }
        return if on_create(app) { 0 } else { -1 };
    }

    let state = window_state(window);
    if !state.is_null() {
        // SAFETY: `state` is the Box<App> that open_archive_window keeps alive for the whole
        // message loop, and only this thread ever touches it.
        let app = unsafe { &mut *state };
        match message {
            WM_SIZE => {
                layout(app);
                return 0;
            }
            WM_COMMAND => {
                on_command(app, wparam);
                return 0;
            }
            job::WM_APP_JOB_DONE => {
                on_job_done(app, lparam);
                return 0;
            }
            WM_DESTROY => {
                // SAFETY: ends the loop this thread owns.
                unsafe {
                    PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
    }
    default_procedure(window, message, wparam, lparam)
}

fn default_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the default procedure accepts any message for any live window.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// The state this window was created with; null before `WM_CREATE` stored it.
fn window_state(window: HWND) -> *mut App {
    // SAFETY: GWLP_USERDATA holds either 0 or the pointer stored by `handle_message`.
    unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) as *mut App }
}

/// Build the child controls. Returning false aborts the window creation, which the caller reports
/// as an error instead of showing a window with no list.
fn on_create(app: &mut App) -> bool {
    app.dpi = native::dpi_of(app.window);
    // SAFETY: the window was created with a menu, so the handle is the one App must enable and
    // grey; the window keeps ownership of it.
    app.menu = unsafe { GetMenu(app.window) };
    app.font = native::ui_font(app.dpi);
    app.list = list_view::create(app.window);
    if app.list.is_null() {
        return false;
    }
    list_view::set_columns(app.list, app.dpi);
    app.status = create_status_line(app.window);
    if app.status.is_null() {
        return false;
    }
    if !app.font.is_null() {
        // SAFETY: both child handles and the font handle are live; the children only store the
        // font handle, which is released after the window is destroyed.
        unsafe {
            SendMessageW(app.list, WM_SETFONT, app.font as WPARAM, TRUE as LPARAM);
            SendMessageW(app.status, WM_SETFONT, app.font as WPARAM, TRUE as LPARAM);
        }
    }
    resize_to_dpi(app.window, app.dpi);
    layout(app);
    true
}

fn create_status_line(parent: HWND) -> HWND {
    // SAFETY: the module handle of the running executable is a process-wide constant.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let empty = native::wide("");
    // SAFETY: every pointer is live for the call and the control copies its own state.
    unsafe {
        CreateWindowExW(
            0,
            WC_STATICW,
            empty.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            parent,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    }
}

/// Grow the frame so the client area is the authored 900x560 at `dpi`; the frame is not part of
/// that budget.
fn resize_to_dpi(window: HWND, dpi: u32) {
    // why: AdjustWindowRectEx ignores the menu bar, so the frame would otherwise steal its height
    // from the requested client area.
    // SAFETY: a plain system-metric query with no arguments to validate.
    let menu_height = unsafe { GetSystemMetrics(SM_CYMENU) };
    let mut frame = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: native::scale(WINDOW_WIDTH, dpi),
        bottom: native::scale(WINDOW_HEIGHT, dpi) + menu_height,
    };
    // SAFETY: `frame` is a live RECT describing the wanted client area.
    if unsafe { AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, FALSE, 0) } == 0 {
        return;
    }
    // SAFETY: `window` is the window being created by this message.
    unsafe {
        SetWindowPos(
            window,
            std::ptr::null_mut(),
            0,
            0,
            frame.right - frame.left,
            frame.bottom - frame.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }
}

/// Stack the status line under the list; the list takes every remaining pixel.
fn layout(app: &mut App) {
    if app.list.is_null() || app.status.is_null() {
        return;
    }
    let mut client = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `client` is a live RECT and `window` is this window.
    if unsafe { GetClientRect(app.window, &mut client) } == 0 {
        return;
    }
    let width = client.right - client.left;
    let status_height = native::scale(STATUS_HEIGHT, app.dpi);
    let list_height = (client.bottom - client.top - status_height).max(0);
    // SAFETY: both children are live windows of this thread.
    unsafe {
        MoveWindow(app.list, 0, 0, width, list_height, TRUE);
        MoveWindow(app.status, 0, list_height, width, status_height, TRUE);
    }
}

fn create_menu() -> HMENU {
    // SAFETY: menu handles are created, filled and (on the failure path) released here.
    unsafe {
        let menu = CreateMenu();
        if menu.is_null() {
            return std::ptr::null_mut();
        }
        let popup = CreatePopupMenu();
        if popup.is_null() {
            DestroyMenu(menu);
            return std::ptr::null_mut();
        }
        for (id, label) in [
            (ID_EXTRACT_ALL, "압축 풀기"),
            (ID_EXTRACT_SELECTED, "선택 풀기"),
            (ID_TEST, "검사"),
        ] {
            let text = native::wide(label);
            AppendMenuW(popup, MF_STRING, id as usize, text.as_ptr());
        }
        let close = native::wide("닫기");
        AppendMenuW(popup, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(popup, MF_STRING, ID_CLOSE as usize, close.as_ptr());
        let root = native::wide("파일(&F)");
        AppendMenuW(menu, MF_POPUP, popup as usize, root.as_ptr());
        menu
    }
}

/// Enter extracts everything and Esc closes, both routed through the menu command ids.
fn create_accelerators() -> HACCEL {
    let accelerators = [
        ACCEL {
            fVirt: FVIRTKEY,
            key: VK_RETURN,
            cmd: ID_EXTRACT_ALL as u16,
        },
        ACCEL {
            fVirt: FVIRTKEY,
            key: VK_ESCAPE,
            cmd: ID_CLOSE as u16,
        },
    ];
    // SAFETY: the table is copied by the call; the array only has to live for this statement.
    unsafe { CreateAcceleratorTableW(accelerators.as_ptr(), accelerators.len() as i32) }
}

fn on_command(app: &mut App, wparam: WPARAM) {
    match (wparam & LOWORD_MASK) as u32 {
        ID_EXTRACT_ALL => begin_extract(app, false),
        ID_EXTRACT_SELECTED => begin_extract(app, true),
        ID_TEST => begin_test(app),
        ID_CLOSE => {
            // SAFETY: posting to this window asks the default procedure to close it.
            unsafe {
                PostMessageW(app.window, WM_CLOSE, 0, 0);
            }
        }
        _ => {}
    }
}

fn begin_extract(app: &mut App, selected_only: bool) {
    if app.busy {
        // A second action would race the worker on the same destination; the menu is greyed too,
        // and this is the guard for the accelerator path.
        return;
    }
    let members = if selected_only {
        let rows = list_view::selected_rows(app.list);
        let members: Vec<String> = rows
            .iter()
            .filter_map(|row| app.entries.get(*row))
            .map(|entry| entry.name.clone())
            .collect();
        if members.is_empty() {
            app.set_status("풀 항목을 먼저 선택하세요");
            return;
        }
        members
    } else {
        Vec::new()
    };
    let Some(destination) = choose_destination(&app.archive) else {
        // Cancelling the folder picker is not an error: nothing is written, the status stays.
        return;
    };
    let kind = if selected_only {
        JobKind::ExtractSelected
    } else {
        JobKind::ExtractAll
    };
    start_job(app, kind, Some(destination), members);
}

fn begin_test(app: &mut App) {
    if app.busy {
        return;
    }
    start_job(app, JobKind::Test, None, Vec::new());
}

/// Ask the user where to unpack. `MOTHZIP_UI_TEST_EXTRACT_DIR` is a test-only hook so the audit
/// harness can drive a real extraction through this same code path; shipping use never sets it.
fn choose_destination(archive: &Path) -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os(TEST_EXTRACT_DIR_ENV) {
        if !directory.is_empty() {
            return Some(PathBuf::from(directory));
        }
    }
    let start = archive.parent().unwrap_or(archive);
    rfd::FileDialog::new()
        .set_title("압축을 풀 폴더 선택")
        .set_directory(start)
        .pick_folder()
}

fn start_job(app: &mut App, kind: JobKind, destination: Option<PathBuf>, members: Vec<String>) {
    let request = JobRequest {
        kind,
        archive: app.archive.clone(),
        password: app.password.clone(),
        max_bytes: app.max_bytes,
        destination,
        members,
    };
    match job::spawn(app.window, request) {
        Ok(()) => {
            app.busy = true;
            app.set_actions_enabled(false);
            app.set_status(kind.running_label());
        }
        Err(error) => app.set_status(&format!("작업 스레드를 시작하지 못했습니다: {error}")),
    }
}

/// Take the worker's outcome, refresh the actions and report it on the status line.
fn on_job_done(app: &mut App, payload: LPARAM) {
    if payload == 0 {
        return;
    }
    // SAFETY: every WM_APP_JOB_DONE carries exactly one Box<JobOutcome> allocated in job::spawn;
    // taking ownership here is what frees it.
    let outcome = unsafe { Box::from_raw(payload as *mut JobOutcome) };
    app.busy = false;
    app.set_actions_enabled(true);
    app.set_status(&describe(&outcome));
}

fn describe(outcome: &JobOutcome) -> String {
    match &outcome.result {
        JobResult::Extracted {
            files,
            bytes,
            target,
        } => format!(
            "압축 풀기 완료: {files}개 파일, {} → {}",
            format::format_bytes(*bytes),
            display_target(target)
        ),
        JobResult::Tested { entries } => format!("검사 통과: {entries}개 항목"),
        JobResult::Failed(reason) => format!("{}: {reason}", outcome.kind.failure_label()),
    }
}

/// The extractor reports its canonicalised target; the `\\?\` prefix is Win32 plumbing that a
/// person reading the status line should not have to decode.
fn display_target(target: &Path) -> String {
    let text = target.display().to_string();
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string()
}

fn catalog_status(total: usize, shown: usize) -> String {
    if total > shown {
        format!("{total}개 중 {shown}개 표시")
    } else {
        format!("{total}개 항목")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_status_reports_the_truncated_total_once_the_cap_is_hit() {
        assert_eq!(catalog_status(0, 0), "0개 항목");
        assert_eq!(catalog_status(7, 7), "7개 항목");
        assert_eq!(catalog_status(70_000, MAX_ROWS), "70000개 중 50000개 표시");
    }

    #[test]
    fn status_shows_the_target_without_the_win32_prefix() {
        assert_eq!(
            display_target(Path::new(r"\\?\C:\work\out")),
            r"C:\work\out"
        );
        assert_eq!(
            display_target(Path::new(r"\\?\UNC\server\share\out")),
            r"\\server\share\out"
        );
        assert_eq!(display_target(Path::new(r"C:\work\out")), r"C:\work\out");
    }
}
