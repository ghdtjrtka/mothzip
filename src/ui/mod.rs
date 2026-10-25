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

use std::ffi::OsStr;
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
    CreateWindowExW, DefWindowProcW, DestroyAcceleratorTable, DestroyMenu, DestroyWindow,
    DispatchMessageW, DrawMenuBar, EnableMenuItem, GetClientRect, GetMenu, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, LoadCursorW, LoadIconW, MoveWindow, PeekMessageW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateAcceleratorW, TranslateMessage, UnregisterClassW, ACCEL,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, FVIRTKEY, GWLP_USERDATA, HACCEL, HMENU,
    IDC_ARROW, IDI_APPLICATION, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MSG, PM_REMOVE, SM_CYMENU, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_QUIT, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_CHILD,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
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

/// What a refused close says: the worker owns the destination until it commits or cleans up, so
/// the window must stay alive. The running label returns when the worker finishes.
const CLOSE_WHILE_BUSY_MESSAGE: &str = "작업 중… 완료 후 닫을 수 있습니다";

/// Test-only override: when set, an extraction writes here instead of opening the folder picker.
/// The shipped behaviour is always the picker; see `audit/delegation/gui-window-handoff.md`.
const TEST_EXTRACT_DIR_ENV: &str = "MOTHZIP_UI_TEST_EXTRACT_DIR";

/// Test-only harness hook: `1` makes the first command that starts a worker also request
/// `WM_CLOSE` once, right after the worker started. The request is synchronous on this thread, so
/// it cannot race the job: the production refusal path runs deterministically. The refusal text
/// then carries [`TEST_CLOSE_REFUSED_NOTE`] for a harness to read. Nothing changes unless an
/// automated check sets it.
const TEST_CLOSE_WHILE_BUSY_ENV: &str = "MOTHZIP_UI_TEST_CLOSE_WHILE_BUSY";

/// Status-line note proving a close request was refused while a worker owned the archive.
const TEST_CLOSE_REFUSED_NOTE: &str = "close-refused-while-busy";

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
    /// True once `WM_CREATE` ran; a window that reached it is destroyed by the system, menu
    /// included, when the procedure returns -1.
    created: bool,
    /// True once `read_archive_catalog` succeeded. Every action is gated on this together with
    /// [`App::busy`], so a corrupt or undecodable archive never enables extraction.
    loaded: bool,
    /// True while a worker owns the archive; close requests are refused during that time.
    busy: bool,
    /// Worker of the job in flight; joined before the state is released or the process exits.
    job: Option<job::Job>,
    /// Whether the last finished job failed; it decides the process exit code at close.
    job_failed: bool,
    /// Test-only: a close request was refused while a worker ran (see
    /// [`maybe_request_test_close`]).
    close_refused: bool,
    /// Test-only: snapshot of [`TEST_CLOSE_WHILE_BUSY_ENV`] taken when the window opens, so the
    /// message loop does not read the environment for every dispatched message.
    test_close_while_busy: bool,
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
            created: false,
            loaded: false,
            busy: false,
            job: None,
            job_failed: false,
            close_refused: false,
            test_close_while_busy: false,
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

    /// The one rule behind the menu, the accelerators and the post-job refresh: an action is
    /// available only when a catalog loaded and no worker owns the archive.
    fn actions_available(&self) -> bool {
        self.loaded && !self.busy
    }

    /// Apply [`App::actions_available`] to the menu; `닫기` stays reachable so the window can
    /// always be dismissed once the worker is done.
    fn sync_actions(&self) {
        if self.menu.is_null() {
            return;
        }
        let flag = if self.actions_available() {
            MF_ENABLED
        } else {
            MF_GRAYED
        };
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
                self.loaded = true;
                let visible = self.entries.len().min(MAX_ROWS);
                list_view::populate(self.list, &self.entries[..visible]);
                let shown = list_view::row_count(self.list);
                self.set_status(&catalog_status(self.entries.len(), shown));
            }
            Err(error) => {
                self.entries.clear();
                self.loaded = false;
                self.set_status(&format!("목록을 읽지 못했습니다: {error}"));
            }
        }
        self.sync_actions();
    }
}

/// Open the browsing window and run its message loop until the user closes it.
///
/// Nothing beside the archive is written until the user picks an action and a destination folder.
/// While a worker runs the window refuses to close; once it can close, the worker is joined so its
/// commit or staging cleanup finishes, and a failed job makes the process exit with code 1.
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
        // SAFETY: CreateWindowExW returned NULL, so no window references `state` any more.
        let app = unsafe { Box::from_raw(state) };
        // `on_create` may have made the font before a child control failed; the window and the
        // children that selected it are already gone, so the handle is released exactly once here.
        native::destroy_font(app.font);
        let window_created = app.created;
        drop(app);
        // SAFETY: a window that reached WM_CREATE was destroyed by the system together with its
        // menu, so a second DestroyMenu would fail with ERROR_INVALID_MENU_HANDLE (1401); only a
        // creation that failed before the procedure ran still owns the menu here.
        unsafe {
            if menu_owned_after_failed_create(window_created) {
                DestroyMenu(menu);
            }
            UnregisterClassW(class_name.as_ptr(), instance);
        }
        // Destroying the failed window posts a WM_QUIT; it belongs to this window, not to a later
        // message loop on this thread.
        discard_pending_quit();
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
        app.test_close_while_busy = test_close_while_busy_requested();
        app.load_catalog();
        // SAFETY: focusing a live child of a live window.
        unsafe {
            SetFocus(app.list);
        }
    }

    let accelerators = create_accelerators();
    let loop_result = pump_messages(window, accelerators);

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
    // A loop that ended in an error may have left its WM_QUIT queued; the next window on this
    // thread must start clean.
    discard_pending_quit();
    match loop_result {
        Ok(0) => Ok(()),
        Ok(_) => Err("작업이 실패한 채로 창을 닫았습니다".into()),
        Err(error) => Err(error),
    }
}

/// Drag the message loop; exits cleanly on `WM_QUIT` with the code `PostQuitMessage` was given,
/// with an error when the queue itself fails.
fn pump_messages(window: HWND, accelerators: HACCEL) -> Result<i32, Box<dyn std::error::Error>> {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is a live MSG that the call fills in.
        match unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } {
            // The wParam of WM_QUIT carries the exit code the window asked for.
            0 => return Ok(message.wParam as i32),
            -1 => {
                // The queue failed while the window is still alive. Destroy it before the caller
                // frees `App`, so a later dispatch cannot reach freed state through the window.
                // SAFETY: `window` is the live window this loop was pumping, on this thread.
                unsafe {
                    DestroyWindow(window);
                }
                return Err("메시지 루프가 실패해 창을 닫았습니다".into());
            }
            _ => {}
        }
        // Shortcuts become the same command ids the menu sends, and the command must go to the
        // window that owns the menu rather than to the focused child control.
        let translated = !accelerators.is_null()
            // SAFETY: `message` is a live MSG and `accelerators` is a live table.
            && unsafe { TranslateAcceleratorW(window, accelerators, &message) } != 0;
        if !translated {
            // SAFETY: `message` was filled by GetMessageW.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        // Runs after the dispatch returned, so no `&mut App` borrow is alive while the test hook
        // sends the close request into the procedure.
        maybe_request_test_close(window);
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
        app.created = true;
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
                on_job_done(app);
                return 0;
            }
            WM_CLOSE => {
                if app.busy {
                    // The worker owns the destination until it commits or cleans up; letting the
                    // window die here would abandon it and leave staging behind.
                    app.close_refused = true;
                    let mut text = CLOSE_WHILE_BUSY_MESSAGE.to_string();
                    if app.test_close_while_busy {
                        text.push_str(" [");
                        text.push_str(TEST_CLOSE_REFUSED_NOTE);
                        text.push(']');
                    }
                    app.set_status(&text);
                    return 0;
                }
            }
            WM_DESTROY => {
                let failed = join_worker(app);
                // A worker that finished while the window was being torn down may have posted its
                // notification after the outcome was taken; remove it instead of dispatching it.
                discard_job_messages(window);
                // SAFETY: this window is going away, so no later dispatch may reach `App` through
                // it, and the quit code is what `open_archive_window` reports.
                unsafe {
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                    PostQuitMessage(failed as i32);
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
            // SAFETY: posting to this window asks the default procedure to close it; the busy
            // check lives in the WM_CLOSE handler, so every close route meets it.
            unsafe {
                PostMessageW(app.window, WM_CLOSE, 0, 0);
            }
        }
        _ => {}
    }
}

fn begin_extract(app: &mut App, selected_only: bool) {
    if !app.actions_available() {
        // A close request, a load error and a running worker all converge here: the menu is greyed
        // for the same states, and this is the guard for the accelerator path.
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
    if !app.actions_available() {
        return;
    }
    start_job(app, JobKind::Test, None, Vec::new());
}

/// Ask the user where to unpack. `MOTHZIP_UI_TEST_EXTRACT_DIR` is a test-only hook so the audit
/// harness can drive a real extraction through this same code path; shipping use never sets it.
fn choose_destination(archive: &Path) -> Option<PathBuf> {
    if let Some(directory) = test_extract_directory() {
        return Some(directory);
    }
    let start = archive.parent().unwrap_or(archive);
    rfd::FileDialog::new()
        .set_title("압축을 풀 폴더 선택")
        .set_directory(start)
        .pick_folder()
}

/// Test-only extraction folder named by [`TEST_EXTRACT_DIR_ENV`], when it is safe to use.
///
/// The hook exists because an automated check cannot drive the folder picker. It grants only what
/// the CLI's `-o` already grants — choosing where an extraction the user started writes — so it
/// does not cross a privilege boundary, and the shipped default remains the interactive picker.
/// It is deliberately not hidden behind `cfg(debug_assertions)`: the GUI harness drives the
/// release binary, and a second binary would not be the program under test.
fn test_extract_directory() -> Option<PathBuf> {
    validated_test_directory(std::env::var_os(TEST_EXTRACT_DIR_ENV).as_deref())
}

/// The acceptance rule for [`test_extract_directory`], split from the environment read so the rule
/// itself is testable: an absolute path that names a directory and is not a reparse point
/// (symlink or junction). A missing final component is accepted only when its parent is a real
/// directory — that is the fresh folder the extractor creates for `-o`, and the shape the bundled
/// GUI harness passes; a missing parent, a relative path and a file are all rejected.
fn validated_test_directory(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value.filter(|value| !value.is_empty())?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return None;
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => (metadata.is_dir() && !native::is_reparse_point(&metadata)).then_some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent()?;
            let metadata = std::fs::symlink_metadata(parent).ok()?;
            (metadata.is_dir() && !native::is_reparse_point(&metadata)).then_some(path)
        }
        Err(_) => None,
    }
}

/// Whether the failed-creation branch still owns the menu it created.
///
/// A window that reached `WM_CREATE` is destroyed by the system when the procedure returns -1,
/// and `DestroyWindow` also destroys the window's menu; calling `DestroyMenu` again would fail with
/// ERROR_INVALID_MENU_HANDLE (1401). Only a creation that failed before the procedure ran leaves
/// the menu owned by the caller.
fn menu_owned_after_failed_create(window_created: bool) -> bool {
    !window_created
}

/// Test-only: after a command started a worker, request a close once.
///
/// `SendMessageW` enters the window procedure synchronously on this thread, so the production
/// close handler sees `busy` regardless of how fast the archive extracts. The refusal is what the
/// harness records; the window keeps running so it can finish the job and close normally.
fn maybe_request_test_close(window: HWND) {
    let state = window_state(window);
    if state.is_null() {
        return;
    }
    // SAFETY: the window is alive and this thread owns `state`; the shared borrow ends before the
    // synchronous procedure call below, which borrows the state mutably on its own.
    let should_send = {
        let app = unsafe { &*state };
        app.test_close_while_busy && app.busy && app.job.is_some() && !app.close_refused
    };
    if !should_send {
        return;
    }
    // SAFETY: sending to a window of this thread runs the procedure inline; the borrow above has
    // ended, so no two references alias.
    unsafe {
        SendMessageW(window, WM_CLOSE, 0, 0);
    }
}

fn test_close_while_busy_requested() -> bool {
    std::env::var_os(TEST_CLOSE_WHILE_BUSY_ENV).as_deref() == Some(OsStr::new("1"))
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
        Ok(worker) => {
            app.job = Some(worker);
            app.busy = true;
            app.close_refused = false;
            app.sync_actions();
            app.set_status(kind.running_label());
        }
        Err(error) => app.set_status(&format!("작업 스레드를 시작하지 못했습니다: {error}")),
    }
}

/// Take the worker's outcome, refresh the actions and report it on the status line.
fn on_job_done(app: &mut App) {
    let Some(worker) = app.job.take() else {
        return;
    };
    let outcome = worker.finish();
    app.busy = false;
    match outcome {
        Some(outcome) => {
            app.job_failed = outcome.failed();
            let mut text = describe(&outcome);
            if app.close_refused && app.test_close_while_busy {
                text.push_str(" [");
                text.push_str(TEST_CLOSE_REFUSED_NOTE);
                text.push(']');
            }
            app.set_status(&text);
        }
        None => {
            // The worker panicked instead of reporting; the job it owned still failed.
            app.job_failed = true;
            app.set_status("작업 스레드가 결과를 보고하지 못했습니다");
        }
    }
    // Recomputed, never forced on: a job that finishes after a failed load leaves the actions off.
    app.sync_actions();
}

/// Join a worker that is still running and report whether the job it was doing failed.
///
/// Closing the window must not abandon the worker mid-write: the join lets the extraction commit
/// or remove its staging directory, and the outcome decides the process exit code.
fn join_worker(app: &mut App) -> bool {
    let Some(worker) = app.job.take() else {
        return app.job_failed;
    };
    app.job_failed = match worker.finish() {
        Some(outcome) => outcome.failed(),
        None => true,
    };
    app.job_failed
}

/// Remove queued completion notifications for `window`.
fn discard_job_messages(window: HWND) {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is writable and the filter selects only this job's notification.
        let found = unsafe {
            PeekMessageW(
                &mut message,
                window,
                job::WM_APP_JOB_DONE,
                job::WM_APP_JOB_DONE,
                PM_REMOVE,
            )
        };
        if found == 0 {
            break;
        }
    }
}

/// Drop a `WM_QUIT` that is still queued, so the next message loop on this thread does not exit
/// before it starts. A normally finished loop already consumed its own quit message.
fn discard_pending_quit() {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is writable and the filter selects only thread quit messages.
        let found = unsafe {
            PeekMessageW(
                &mut message,
                std::ptr::null_mut(),
                WM_QUIT,
                WM_QUIT,
                PM_REMOVE,
            )
        };
        if found == 0 {
            break;
        }
    }
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

    /// A directory name no other test run shares, so parallel tests cannot collide.
    fn unique_temp_dir(label: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("mothzip-ui-{label}-{}-{stamp}", std::process::id()))
    }

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

    #[test]
    fn actions_are_only_available_for_a_loaded_catalog_and_no_worker() {
        let mut app = App::new(PathBuf::from("archive.zip"), None, 1 << 20);
        assert!(!app.actions_available(), "nothing is loaded yet");
        app.loaded = true;
        assert!(app.actions_available());
        app.busy = true;
        assert!(
            !app.actions_available(),
            "a running worker owns the archive"
        );
        app.busy = false;
        assert!(app.actions_available());
        app.loaded = false;
        assert!(
            !app.actions_available(),
            "a failed load must stay disabled even when no worker runs"
        );
    }

    #[test]
    fn a_job_finishing_after_a_load_failure_keeps_the_actions_disabled() {
        let mut app = App::new(PathBuf::from("이런-아카이브는-없다.zip"), None, 1 << 20);
        // The real failure path: the catalog cannot be read, so `loaded` stays false.
        app.busy = true;
        app.load_catalog();
        assert!(!app.loaded);
        // Exactly what `on_job_done` leaves behind for the refresh to recompute.
        app.busy = false;
        assert!(!app.actions_available());
    }

    #[test]
    fn the_test_extract_directory_requires_an_absolute_directory() {
        assert_eq!(validated_test_directory(None), None);
        assert_eq!(validated_test_directory(Some(OsStr::new(""))), None);
        assert_eq!(
            validated_test_directory(Some(OsStr::new("relative\\out"))),
            None
        );

        let base = unique_temp_dir("extract-rule");
        std::fs::create_dir_all(&base).expect("create the probe parent");
        let missing_parent = base.join("missing").join("out");
        assert_eq!(
            validated_test_directory(Some(missing_parent.as_os_str())),
            None,
            "a path below a missing parent is not an extraction destination"
        );

        let file = base.join("a-file.txt");
        std::fs::write(&file, b"not a directory").expect("create the probe file");
        assert_eq!(
            validated_test_directory(Some(file.as_os_str())),
            None,
            "a file is never a destination"
        );

        let directory = base.join("out");
        std::fs::create_dir(&directory).expect("create the probe directory");
        assert_eq!(
            validated_test_directory(Some(directory.as_os_str())),
            Some(directory.clone())
        );

        // The `-o` contract and the bundled GUI harness both pass a folder the extraction is
        // about to create; only the leaf may be absent, and the parent must be real.
        let fresh = base.join("fresh").join("out");
        std::fs::create_dir_all(fresh.parent().expect("fresh has a parent"))
            .expect("create the fresh parent");
        assert_eq!(
            validated_test_directory(Some(fresh.as_os_str())),
            Some(fresh.clone())
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_test_extract_directory_rejects_a_directory_symlink() {
        let base = unique_temp_dir("extract-link");
        let target = base.join("target");
        std::fs::create_dir_all(&target).expect("create the link target");
        let link = base.join("link");
        if std::os::windows::fs::symlink_dir(&target, &link).is_err() {
            // Creating a symlink needs Developer Mode or elevation, which the host running the
            // test may not allow; a junction cannot be made with `std` alone.
            std::fs::remove_dir_all(&base).ok();
            return;
        }
        assert_eq!(
            validated_test_directory(Some(link.as_os_str())),
            None,
            "a directory symlink redirects the extraction"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn failed_creation_does_not_release_a_menu_the_system_destroyed() {
        // WM_CREATE ran: the system destroyed the window and its menu before CreateWindowExW
        // returned, so the caller must not call DestroyMenu again (error 1401).
        assert!(!menu_owned_after_failed_create(true));
        // Creation failed before the procedure ran: the menu is still the caller's to release.
        assert!(menu_owned_after_failed_create(false));
    }

    use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, PM_NOREMOVE};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    #[test]
    fn a_leftover_quit_message_does_not_reach_the_next_message_loop() {
        let mut message: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: a non-removing peek creates this thread's message queue, which
        // PostThreadMessageW requires before it can deliver to the thread.
        unsafe {
            PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        }
        // SAFETY: posting to this thread's own queue, which the call above created.
        let posted = unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_QUIT, 0, 0) };
        assert_ne!(posted, 0, "PostThreadMessageW(WM_QUIT) failed");

        discard_pending_quit();

        let mut leftover: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: `leftover` is writable and the filter selects only thread quit messages.
        let remaining = unsafe {
            PeekMessageW(
                &mut leftover,
                std::ptr::null_mut(),
                WM_QUIT,
                WM_QUIT,
                PM_REMOVE,
            )
        };
        assert_eq!(remaining, 0, "the next loop must not inherit this WM_QUIT");
    }
}
