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
use windows_sys::Win32::Graphics::Gdi::{InvalidateRect, COLOR_WINDOW, HBRUSH};
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
    IDC_ARROW, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MSG, PM_REMOVE, SM_CYMENU, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_QUIT, WM_SETFONT, WM_SETREDRAW, WM_SIZE, WNDCLASSEXW, WS_CHILD,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::core::catalog::CatalogEntry;

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
const ID_CANCEL: u32 = 1005;

/// The low word of a message parameter, where a `WM_COMMAND` command id arrives.
const LOWORD_MASK: WPARAM = 0xFFFF;

/// Status line from the moment the window opens until the catalog arrives: the read runs on a
/// worker thread, so this is what the user sees instead of a frozen window.
const LOADING_STATUS: &str = "Loading…";

/// Status line after the user stopped a job.
const CANCELLED_STATUS: &str = "Cancelled";

/// What a refused close says: the integrity check owns the archive until it reports, so the window
/// must stay alive; its result returns to the status line.
const CLOSE_WHILE_BUSY_MESSAGE: &str = "Busy: close this window when the job finishes";

/// Test-only override: when set, an extraction writes here instead of opening the folder picker.
/// The shipped behaviour is always the picker; see `audit/delegation/gui-window-handoff.md`.
const TEST_EXTRACT_DIR_ENV: &str = "MOTHZIP_UI_TEST_EXTRACT_DIR";

/// Test-only harness hook: `1` makes the first command that starts a worker also request
/// `WM_CLOSE` once, right after the worker started. The request is synchronous on this thread, so
/// it cannot race the job: the production path runs deterministically. While an extraction runs
/// that request is the cancellation path, and a running integrity check refuses it and carries
/// [`TEST_CLOSE_REFUSED_NOTE`] on the status line for a harness to read. Nothing changes unless an
/// automated check sets it.
const TEST_CLOSE_WHILE_BUSY_ENV: &str = "MOTHZIP_UI_TEST_CLOSE_WHILE_BUSY";

/// Status-line note proving a close request was refused while an integrity check owned the
/// archive.
const TEST_CLOSE_REFUSED_NOTE: &str = "close-refused-while-busy";

/// Status line when the catalog worker ended without reporting; the window stays open with the
/// same "could not read" contract as any other load failure.
const LOAD_WORKER_SILENT: &str = "The worker did not report a result";

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
    /// True once a catalog arrived. Every action is gated on this together with [`App::busy`],
    /// so a corrupt or undecodable archive never enables extraction.
    loaded: bool,
    /// True while the catalog worker reads the archive: the list is still empty and every action
    /// is off, which is the same state a failed load leaves behind.
    loading: bool,
    /// Worker of the catalog read in flight; joined when the window closes after it finished, or
    /// detached when it is still reading.
    load_job: Option<job::LoadJob>,
    /// True while a worker owns the archive; before it can close, a running extraction is
    /// cancelled and a running integrity check is refused.
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
            loading: false,
            load_job: None,
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
    /// available only when a catalog arrived and no worker owns the archive.
    fn actions_available(&self) -> bool {
        self.loaded && !self.busy && !self.loading
    }

    /// Whether the running job can be stopped; see [`JobKind::cancellable`].
    fn cancellable(&self) -> bool {
        self.busy && self.job.as_ref().is_some_and(job::Job::cancellable)
    }

    /// Apply [`App::actions_available`] and [`App::cancellable`] to the menu; `닫기`/`Close` stays
    /// reachable so the window can always be dismissed once the worker is done.
    fn sync_actions(&self) {
        if self.menu.is_null() {
            return;
        }
        let action_flag = if self.actions_available() {
            MF_ENABLED
        } else {
            MF_GRAYED
        };
        let cancel_flag = if self.cancellable() {
            MF_ENABLED
        } else {
            MF_GRAYED
        };
        // SAFETY: `menu` is the menu installed on `window` and outlives every call.
        unsafe {
            for id in [ID_EXTRACT_ALL, ID_EXTRACT_SELECTED, ID_TEST] {
                EnableMenuItem(self.menu, id, MF_BYCOMMAND | action_flag);
            }
            EnableMenuItem(self.menu, ID_CANCEL, MF_BYCOMMAND | cancel_flag);
            DrawMenuBar(self.window);
        }
    }

    /// Start reading the catalog on a worker thread.
    ///
    /// The window is already on screen and its message loop starts right after this, so the read
    /// never runs on the thread that pumps messages: the status line says `Loading…` and the
    /// posted notification brings the entries back here, where the list is filled.
    fn begin_load(&mut self) {
        let request = job::LoadRequest {
            archive: self.archive.clone(),
            password: self.password.clone(),
            max_bytes: self.max_bytes,
        };
        match job::spawn_load(self.window, request) {
            Ok(loader) => {
                self.load_job = Some(loader);
                self.loading = true;
                self.set_status(LOADING_STATUS);
                self.sync_actions();
            }
            Err(error) => {
                // No thread means no catalog: keep the failed-load contract instead of a window
                // that never finishes loading.
                self.apply_catalog(Err(error.to_string()));
            }
        }
    }

    /// Take the entries the catalog worker read and hand them to the list.
    fn on_load_done(&mut self, notification: WPARAM) {
        let Some(loader) = self.load_job.as_ref() else {
            return;
        };
        if loader.id() != notification as u64 {
            // A notification from an earlier load (its window is gone): not our result.
            return;
        }
        let Some(loader) = self.load_job.take() else {
            return;
        };
        match loader.finish() {
            Some(outcome) => self.apply_catalog(outcome),
            None => self.apply_catalog(Err(LOAD_WORKER_SILENT.to_string())),
        }
    }

    /// Apply one load outcome and end the load. The list is filled here, on the thread that owns
    /// the control; a failure stays on screen, because the window is already open and closing it
    /// would throw away the only place the reason can be shown.
    fn apply_catalog(&mut self, outcome: Result<Vec<CatalogEntry>, String>) {
        self.loading = false;
        match outcome {
            Ok(entries) => {
                self.entries = entries;
                self.loaded = true;
                let visible = self.entries.len().min(MAX_ROWS);
                // A repaint per inserted row is what turns a large fill into a tens-of-seconds
                // stall; suppressing redraw for the fill and painting once at the end measured
                // 5.3 s against 41 s for the same 30 000-row catalog.
                // SAFETY: `list` is this window's live child control and the message only changes
                // its redraw state.
                unsafe {
                    SendMessageW(self.list, WM_SETREDRAW, FALSE as WPARAM, 0);
                }
                list_view::populate(self.list, &self.entries[..visible]);
                let shown = list_view::row_count(self.list);
                // SAFETY: as above; one invalidate repaints the control after the fill.
                unsafe {
                    SendMessageW(self.list, WM_SETREDRAW, TRUE as WPARAM, 0);
                    InvalidateRect(self.list, std::ptr::null(), TRUE);
                }
                self.set_status(&catalog_status(self.entries.len(), shown));
                // Focus moves to the list once it holds rows: keyboard selection needs a row to
                // land on, and focusing an empty list would only draw a focus rectangle.
                // SAFETY: `list` is this window's live child control, on this thread.
                unsafe {
                    SetFocus(self.list);
                }
            }
            Err(reason) => {
                self.entries.clear();
                self.loaded = false;
                self.set_status(&load_failure_status(&reason));
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
        return Err(format!("Archive not found: {}", archive.display()).into());
    }
    // SAFETY: the module handle of the running executable is a process-wide constant.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        return Err("Cannot create the window: no module handle".into());
    }

    let class_name = native::wide(WINDOW_CLASS);
    let mut class: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    class.style = CS_HREDRAW | CS_VREDRAW;
    class.lpfnWndProc = Some(window_proc);
    class.hInstance = instance;
    // SAFETY: resource 1 is the app icon embedded by build.rs in this module.
    class.hIcon = unsafe { LoadIconW(instance, 1usize as *const u16) };
    class.hIconSm = class.hIcon;
    // SAFETY: as above; the arrow cursor is a shared system resource.
    class.hCursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };
    class.hbrBackground = (COLOR_WINDOW + 1) as isize as HBRUSH;
    class.lpszClassName = class_name.as_ptr();
    // SAFETY: `class` is a fully initialised WNDCLASSEXW naming our own procedure.
    if unsafe { RegisterClassExW(&class) } == 0 {
        // SAFETY: reading the per-thread last-error code.
        let reason = unsafe { GetLastError() };
        if reason != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format!("Could not register the window class (error {reason})").into());
        }
    }

    let menu = create_menu();
    if menu.is_null() {
        // SAFETY: no window uses the class yet and this process just registered it.
        unsafe {
            UnregisterClassW(class_name.as_ptr(), instance);
        }
        return Err("Could not create the menu".into());
    }

    let state = Box::into_raw(Box::new(App::new(
        archive.clone(),
        request.password.clone(),
        request.max_bytes,
    )));
    let title = native::wide(&format!("{} — Mothzip", native::display_name_of(&archive)));
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
            "Could not create the archive window; check that this session may create windows"
                .into(),
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
        app.begin_load();
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
        Ok(_) => Err("Window closed while a job was still failing".into()),
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
                return Err("Window closed after the message loop failed".into());
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
            job::WM_APP_LOAD_DONE => {
                app.on_load_done(wparam);
                return 0;
            }
            WM_CLOSE => {
                if app.cancellable() {
                    // A running extraction stages its output and stops at the next chunk, so a
                    // close request is the user's stop: it cancels and the window stays usable.
                    cancel_job(app);
                    return 0;
                }
                if app.busy {
                    // An integrity check cannot be preempted; letting the window die here would
                    // throw its verdict away, so the request is refused and reported.
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
                discard_notifications(window, job::WM_APP_JOB_DONE);
                discard_notifications(window, job::WM_APP_LOAD_DONE);
                release_load_worker(app);
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
            (ID_EXTRACT_ALL, "Extract all"),
            (ID_EXTRACT_SELECTED, "Extract selected"),
            (ID_TEST, "Test integrity"),
            (ID_CANCEL, "Cancel"),
        ] {
            let text = native::wide(label);
            AppendMenuW(popup, MF_STRING, id as usize, text.as_ptr());
        }
        let close = native::wide("Close");
        AppendMenuW(popup, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(popup, MF_STRING, ID_CLOSE as usize, close.as_ptr());
        let root = native::wide("&File");
        AppendMenuW(menu, MF_POPUP, popup as usize, root.as_ptr());
        menu
    }
}

/// Enter extracts everything, Esc is the close request, and both travel as menu command ids so a
/// keyboard press and a menu pick take the same path — while an extraction runs, both become the
/// cancellation.
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
        ID_CANCEL => cancel_job(app),
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
            app.set_status("Select an item first");
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
        .set_title("Choose a folder to extract to")
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
        Err(error) => app.set_status(&format!("Could not start the worker: {error}")),
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
            app.set_status("The worker did not report a result");
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

/// Stop the running extraction, wait for it to discard its staging, and report the cancellation.
///
/// The wait is short by design: the producer checks the token before every member and on every
/// copied chunk, so the join costs at most one chunk. Nothing is committed after the staging
/// phase, which is why a cancelled job leaves the destination exactly as it found it. A job that
/// finished before the request reached it is reported as what it actually did, never as cancelled.
fn cancel_job(app: &mut App) {
    let Some(worker) = app.job.as_ref() else {
        return;
    };
    if !worker.cancellable() {
        return;
    }
    worker.request_cancel();
    let Some(worker) = app.job.take() else {
        return;
    };
    let outcome = worker.finish();
    app.busy = false;
    app.close_refused = false;
    match outcome {
        Some(outcome) if outcome.cancelled() => {
            // The user asked for this stop, so it is not a failure and the process still exits 0.
            app.job_failed = false;
            app.set_status(CANCELLED_STATUS);
        }
        Some(outcome) => {
            app.job_failed = outcome.failed();
            app.set_status(&describe(&outcome));
        }
        None => {
            app.job_failed = true;
            app.set_status(LOAD_WORKER_SILENT);
        }
    }
    app.sync_actions();
}

/// Release the catalog worker when the window is being destroyed.
///
/// A load that already finished is joined and its entries dropped; one that is still reading is
/// detached, because a catalog read has no cancellation seam and a window that the user closed
/// must not wait for it. The worker owns only the archive path and its channel after this returns,
/// and the notification it posts carries its generation id, which the window compares first.
fn release_load_worker(app: &mut App) {
    let Some(loader) = app.load_job.take() else {
        return;
    };
    if loader.is_finished() {
        let _ = loader.finish();
    } else {
        loader.detach();
    }
}

/// Remove queued notifications of one kind for `window`.
fn discard_notifications(window: HWND, notification: u32) {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `message` is writable and the filter selects only this job's notification.
        let found =
            unsafe { PeekMessageW(&mut message, window, notification, notification, PM_REMOVE) };
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
            "Extracted {files} files, {} -> {}",
            format::format_bytes(*bytes),
            display_target(target)
        ),
        JobResult::Tested { entries } => format!("Test passed: {entries} entries"),
        JobResult::Cancelled => CANCELLED_STATUS.to_string(),
        JobResult::Failed(reason) => format!("{}: {reason}", outcome.kind.failure_label()),
    }
}

/// The status line a load failure leaves behind: the window stays open and every action stays off.
fn load_failure_status(reason: &str) -> String {
    format!("Could not read the archive: {reason}")
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
        format!("showing {shown} of {total}")
    } else {
        format!("{total} entries")
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
        assert_eq!(catalog_status(0, 0), "0 entries");
        assert_eq!(catalog_status(7, 7), "7 entries");
        assert_eq!(catalog_status(70_000, MAX_ROWS), "showing 50000 of 70000");
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
        app.loading = true;
        assert!(
            !app.actions_available(),
            "the catalog worker still owns the load"
        );
        app.loading = false;
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
        app.loading = true;
        app.apply_catalog(Err("the archive is not a zip".to_string()));
        assert!(!app.loaded);
        assert!(!app.loading, "a reported failure ends the load");
        // Exactly what `on_job_done` leaves behind for the refresh to recompute.
        app.busy = false;
        assert!(!app.actions_available());
    }

    #[test]
    fn a_failed_load_reports_the_contract_text_and_keeps_the_window_usable() {
        assert_eq!(
            load_failure_status("not a zip"),
            "Could not read the archive: not a zip"
        );
        assert!(
            load_failure_status("x").starts_with("Could not read the archive: "),
            "the failure contract keeps its prefix"
        );
        assert!(!CANCELLED_STATUS.is_empty());
        assert_eq!(LOADING_STATUS, "Loading…");
    }

    /// The window thread's cancel path, end to end: a real extraction worker, the token, the join,
    /// the discarded staging and the state the menu refresh reads afterwards.
    #[test]
    fn cancelling_an_extraction_frees_the_window_and_is_not_a_failure() {
        use std::io::Write;

        let dir = unique_temp_dir("cancel-path");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let archive = dir.join("fixture.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).expect("create zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let block = vec![0xA5u8; 1024 * 1024];
        for index in 0..8 {
            writer
                .start_file(format!("chunk{index:02}.bin"), options)
                .expect("start member");
            writer.write_all(&block).expect("write member");
        }
        writer.finish().expect("finish zip");
        let destination = dir.join("out");

        let mut app = App::new(archive, None, 1 << 30);
        app.loaded = true;
        let request = JobRequest {
            kind: JobKind::ExtractAll,
            archive: app.archive.clone(),
            password: None,
            max_bytes: app.max_bytes,
            destination: Some(destination.clone()),
            members: Vec::new(),
        };
        app.job = Some(job::spawn(app.window, request).expect("spawn the worker"));
        app.busy = true;
        assert!(app.cancellable(), "an extraction can be stopped");
        assert!(!app.actions_available());

        cancel_job(&mut app);

        assert!(!app.busy, "the window is usable again");
        assert!(app.job.is_none(), "the worker was joined and released");
        assert!(
            !app.job_failed,
            "the user's stop must not fail the process exit"
        );
        assert!(app.actions_available(), "the actions come back");
        assert!(
            !destination.exists() || std::fs::read_dir(&destination).unwrap().next().is_none(),
            "nothing is committed after a cancel"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_job_that_cannot_be_cancelled_is_left_alone() {
        let mut app = App::new(PathBuf::from("archive.zip"), None, 1 << 20);
        app.loaded = true;
        app.busy = true;
        assert!(!app.cancellable(), "no job means nothing to cancel");
        app.job = Some(
            job::spawn(
                app.window,
                JobRequest {
                    kind: JobKind::Test,
                    archive: PathBuf::from("이런-아카이브는-없다.zip"),
                    password: None,
                    max_bytes: 1 << 20,
                    destination: None,
                    members: Vec::new(),
                },
            )
            .expect("spawn the worker"),
        );
        assert!(
            !app.cancellable(),
            "an integrity check has no cancellation seam"
        );
        // The refusal path keeps the job; the caller reports it instead of stopping it.
        cancel_job(&mut app);
        assert!(
            app.job.is_some(),
            "a refused cancel leaves the worker alone"
        );
        app.job.take().expect("the job is still there").finish();
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
