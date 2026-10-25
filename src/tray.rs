//! Small native Windows notification-area UI. No browser or background UI runtime.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

/// Visual state is explicit so localized status text never controls the icon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SyncState {
    Idle,
    Syncing,
    Paused,
    Error,
}

/// Shared commands and human-readable status between the tray and sync worker.
pub struct Control {
    pub paused: AtomicBool,
    pub stopping: AtomicBool,
    pub sync_requested: AtomicBool,
    status: Mutex<String>,
    state: AtomicU8,
}

impl Control {
    pub fn set_status(&self, state: SyncState, status: String) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
        self.state.store(state as u8, Ordering::Release);
    }

    pub fn status_text(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn sync_state(&self) -> SyncState {
        if self.paused.load(Ordering::Acquire) {
            return SyncState::Paused;
        }
        match self.state.load(Ordering::Acquire) {
            0 => SyncState::Idle,
            1 => SyncState::Syncing,
            2 => SyncState::Paused,
            _ => SyncState::Error,
        }
    }
}

impl Default for Control {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            sync_requested: AtomicBool::new(false),
            status: Mutex::new("Starting…".into()),
            state: AtomicU8::new(SyncState::Syncing as u8),
        }
    }
}

#[cfg(windows)]
pub use native::run;

#[cfg(windows)]
#[path = "tray_design.rs"]
mod design;

#[cfg(windows)]
mod native {
    use super::design::{self, Anchor, Button, Design, Font, Item, Kind, Layout};
    use super::{Control, SyncState};
    use crate::{
        config::{ConditionalEtagFormat, Config},
        credentials::Secrets,
        manager::Manager,
        profiles::{Connection, Profiles},
    };
    use anyhow::{Context, Result, bail};
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        ffi::OsStr,
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        ptr::{null, null_mut},
        sync::{Arc, atomic::Ordering},
    };
    use windows_sys::Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{BeginPaint, COLOR_WINDOW, EndPaint, InvalidateRect, PAINTSTRUCT},
        System::{
            LibraryLoader::GetModuleHandleW,
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
            SystemServices::{SS_ENDELLIPSIS, SS_NOPREFIX},
        },
        UI::{
            Controls::{
                BST_CHECKED, DRAWITEMSTRUCT, EM_SETLIMITTEXT, EM_SETPASSWORDCHAR,
                ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
                MEASUREITEMSTRUCT, ODT_COMBOBOX, ShowScrollBar,
            },
            HiDpi::{
                AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi,
                SetThreadDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::{EnableWindow, SetFocus},
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
                Shell_NotifyIconW, ShellExecuteW,
            },
            WindowsAndMessaging::*,
        },
    };

    const TRAY_MESSAGE: u32 = WM_APP + 1;
    const TIMER: usize = 1;
    const CONFIGURE: usize = 102;
    const SYNC_NOW: usize = 103;
    const PAUSE: usize = 104;
    const QUIT: usize = 106;
    const SAVE: usize = 1;
    const CANCEL: usize = 2;
    const ENDPOINT: i32 = 201;
    const BUCKET: i32 = 202;
    const REGION: i32 = 203;
    const PREFIX: i32 = 204;
    const MOUNT_DIR: i32 = 205;
    const DRIVE: i32 = 206;
    const CACHE_BYTES: i32 = 207;
    const CACHE_FILES: i32 = 208;
    const POLL_SECS: i32 = 209;
    const ACCESS_ENV: i32 = 210;
    const SECRET_ENV: i32 = 211;
    const SESSION_ENV: i32 = 212;
    const ALLOW_HTTP: i32 = 213;
    const OVH_CONDITIONALS: i32 = 214;
    const PROFILE: i32 = 215;
    const ADD_PROFILE: usize = 216;
    const REMOVE_PROFILE: usize = 217;
    const NAME: i32 = 218;
    const ENABLED: i32 = 219;
    const STATE_DIR: i32 = 220;
    const SETTLE_SECS: i32 = 221;
    const UPLOADS: i32 = 222;
    const SECURE: i32 = 223;
    const CREDENTIAL_HELP: i32 = 224;
    const PROFILE_STATUS: i32 = 225;
    const ACCESS_LABEL: i32 = 226;
    const SECRET_LABEL: i32 = 227;
    const SESSION_LABEL: i32 = 228;
    const NAV_CONNECTION: usize = 230;
    const NAV_CREDENTIALS: usize = 231;
    const NAV_SYNC: usize = 232;
    const STATUS_TITLE: i32 = 233;
    const STATUS_DETAILS: usize = 234;
    const PROFILE_ACTION_BASE: usize = 1000;
    const NEW_PROFILE: usize = usize::MAX;

    const APP_ICON: u16 = 101;
    const TRAY_ON_DARK: u16 = 201;
    const TRAY_ON_LIGHT: u16 = 211;

    struct OwnedIcon(HICON);

    impl Drop for OwnedIcon {
        fn drop(&mut self) {
            // LoadImageW below deliberately omits LR_SHARED. Shared images can
            // return the first cached size for a resource, even after DPI changes.
            unsafe { DestroyIcon(self.0) };
        }
    }

    #[derive(Default)]
    struct IconCache(BTreeMap<(u16, i32), OwnedIcon>);

    impl IconCache {
        unsafe fn get(&mut self, resource: u16, pixels: i32) -> Result<HICON> {
            unsafe {
                if let Some(icon) = self.0.get(&(resource, pixels)) {
                    return Ok(icon.0);
                }
                let handle = LoadImageW(
                    GetModuleHandleW(null()),
                    resource as usize as *const u16,
                    IMAGE_ICON,
                    pixels,
                    pixels,
                    LR_DEFAULTCOLOR,
                ) as HICON;
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!("Load application icon {resource} at {pixels}px")
                    });
                }
                self.0.insert((resource, pixels), OwnedIcon(handle));
                Ok(handle)
            }
        }
    }

    struct UiState {
        config_path: PathBuf,
        config: RefCell<Config>,
        profiles: RefCell<Profiles>,
        workers: RefCell<Manager>,
        editing: Cell<usize>,
        draft_name: RefCell<String>,
        draft_enabled: Cell<bool>,
        dirty: Cell<bool>,
        control: Arc<Control>,
        form: Cell<HWND>,
        taskbar_created: u32,
        icons: RefCell<IconCache>,
        light_taskbar: Cell<bool>,
        design: RefCell<Option<Design>>,
        section: Cell<usize>,
        laying_out: Cell<bool>,
    }

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain(Some(0)).collect()
    }

    /// Run on a dedicated UI thread or the process main thread until Quit is chosen.
    /// An invalid or incomplete connection does not prevent the tray from opening.
    pub fn run(config_path: PathBuf, profiles: Profiles) -> Result<()> {
        // The boxed state remains at a stable address until all windows are destroyed.
        unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_STANDARD_CLASSES,
            });
            let instance = GetModuleHandleW(null());
            let main_class = wide("BucketMount.NotificationWindow");
            let form_class = wide("BucketMount.ConfigurationWindow");
            register_class(instance, &main_class, Some(tray_proc))?;
            register_class(instance, &form_class, Some(form_proc))?;
            let needs_setup = profiles.connections.is_empty()
                || profiles
                    .connections
                    .iter()
                    .any(|c| c.config.validate().is_err());
            let initial = profiles
                .connections
                .first()
                .cloned()
                .unwrap_or_else(|| Connection::new("New connection".into()));
            let mut workers = Manager::new()?;
            workers.apply(&profiles)?;
            let state = Box::new(UiState {
                editing: Cell::new(if profiles.connections.is_empty() {
                    NEW_PROFILE
                } else {
                    0
                }),
                draft_name: RefCell::new(initial.name),
                draft_enabled: Cell::new(initial.enabled),
                dirty: Cell::new(false),
                config: RefCell::new(initial.config),
                profiles: RefCell::new(profiles),
                workers: RefCell::new(workers),
                config_path,
                control: Arc::new(Control::default()),
                form: Cell::new(null_mut()),
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                icons: RefCell::new(IconCache::default()),
                light_taskbar: Cell::new(taskbar_is_light()),
                design: RefCell::new(None),
                section: Cell::new(0),
                laying_out: Cell::new(false),
            });
            // Fail clearly if a build omitted any icon, before opening the UI.
            let tray_pixels = taskbar_icon_pixels();
            for resource in [201, 202, 203, 204, 211, 212, 213, 214] {
                state.icons.borrow_mut().get(resource, tray_pixels)?;
            }
            let hwnd = CreateWindowExW(
                0,
                main_class.as_ptr(),
                wide("Bucket Mount").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                (&*state as *const UiState).cast(),
            );
            if hwnd.is_null() {
                return Err(std::io::Error::last_os_error()).context("Create tray message window");
            }
            set_window_icons(hwnd, &state);
            update_icon(hwnd, &state, NIM_ADD);
            if SetTimer(hwnd, TIMER, 2_000, None) == 0 {
                let error = std::io::Error::last_os_error();
                DestroyWindow(hwnd);
                return Err(error).context("Create tray status timer");
            }
            if needs_setup {
                show_config(hwnd, &state);
            }
            let mut message: MSG = zeroed();
            let mut result = Ok(());
            loop {
                let received = GetMessageW(&mut message, null_mut(), 0, 0);
                if received == 0 {
                    break;
                }
                if received == -1 {
                    result =
                        Err(std::io::Error::last_os_error()).context("Read Windows UI message");
                    break;
                }
                let form = state.form.get();
                if form.is_null() || IsDialogMessageW(form, &message) == 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            state.control.stopping.store(true, Ordering::Release);
            if IsWindow(hwnd) != 0 {
                DestroyWindow(hwnd);
            }
            result
        }
    }

    unsafe fn register_class(instance: HINSTANCE, name: &[u16], procedure: WNDPROC) -> Result<()> {
        unsafe {
            let mut class: WNDCLASSW = zeroed();
            class.lpfnWndProc = procedure;
            class.hInstance = instance;
            class.lpszClassName = name.as_ptr();
            class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
            // LoadIconW returns a shared handle; the window class can retain it
            // after a UI run. Per-window and tray images below have owned handles.
            class.hIcon = LoadIconW(instance, APP_ICON as usize as *const u16);
            if class.hIcon.is_null() {
                return Err(std::io::Error::last_os_error()).context("Load window class icon");
            }
            class.hbrBackground = (COLOR_WINDOW + 1) as usize as _;
            if RegisterClassW(&class) == 0 {
                let error = std::io::Error::last_os_error();
                // ERROR_CLASS_ALREADY_EXISTS allows a later UI run in the same process.
                if error.raw_os_error() != Some(1410) {
                    return Err(error).context("Register native window class");
                }
            }
            Ok(())
        }
    }

    unsafe fn set_state(hwnd: HWND, lparam: LPARAM) {
        unsafe {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }

    unsafe fn state<'a>(hwnd: HWND) -> Option<&'a UiState> {
        unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const UiState).as_ref() }
    }

    unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if msg == WM_NCCREATE {
                set_state(hwnd, lp);
            }
            let Some(ui) = state(hwnd) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            if ui.taskbar_created != 0 && msg == ui.taskbar_created {
                ui.light_taskbar.set(taskbar_is_light());
                update_icon(hwnd, ui, NIM_ADD);
                return 0;
            }
            match msg {
                WM_SETTINGCHANGE | WM_THEMECHANGED | WM_DISPLAYCHANGE | WM_DPICHANGED => {
                    ui.light_taskbar.set(taskbar_is_light());
                    set_window_icons(hwnd, ui);
                    if !ui.form.get().is_null() {
                        set_window_icons(ui.form.get(), ui);
                    }
                    update_icon(hwnd, ui, NIM_MODIFY);
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
                TRAY_MESSAGE => {
                    match lp as u32 {
                        WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd, ui),
                        WM_LBUTTONDBLCLK => show_config(hwnd, ui),
                        WM_LBUTTONUP => show_menu(hwnd, ui),
                        _ => {}
                    }
                    0
                }
                WM_TIMER => {
                    refresh_status(ui);
                    if ui.control.stopping.load(Ordering::Acquire) {
                        DestroyWindow(hwnd);
                    } else if !update_icon(hwnd, ui, NIM_MODIFY) {
                        update_icon(hwnd, ui, NIM_ADD);
                    }
                    0
                }
                WM_COMMAND => {
                    command(hwnd, ui, wp & 0xffff);
                    0
                }
                WM_CLOSE => {
                    ui.control.stopping.store(true, Ordering::Release);
                    DestroyWindow(hwnd);
                    0
                }
                WM_DESTROY => {
                    KillTimer(hwnd, TIMER);
                    update_icon(hwnd, ui, NIM_DELETE);
                    ui.control.stopping.store(true, Ordering::Release);
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
    }

    fn status(ui: &UiState) -> String {
        ui.control.status_text()
    }

    fn refresh_status(ui: &UiState) {
        let profiles = ui.profiles.borrow();
        let workers = ui.workers.borrow();
        let enabled: Vec<_> = profiles
            .connections
            .iter()
            .filter(|c| c.enabled)
            .filter_map(|c| workers.control(&c.config.device_id))
            .collect();
        let errors = enabled
            .iter()
            .filter(|c| c.sync_state() == SyncState::Error)
            .count();
        let all_paused =
            !enabled.is_empty() && enabled.iter().all(|c| c.sync_state() == SyncState::Paused);
        let state = if errors > 0 {
            SyncState::Error
        } else if all_paused {
            SyncState::Paused
        } else if enabled.iter().any(|c| c.sync_state() == SyncState::Syncing) {
            SyncState::Syncing
        } else {
            SyncState::Idle
        };
        ui.control.paused.store(all_paused, Ordering::Release);
        ui.control.set_status(
            state,
            if errors > 0 {
                format!(
                    "{} connections enabled; {errors} need attention",
                    enabled.len()
                )
            } else {
                format!("{} connections enabled", enabled.len())
            },
        );
        if !ui.form.get().is_null() {
            let new_connection = ui.editing.get() == NEW_PROFILE;
            let selected = profiles
                .connections
                .get(ui.editing.get())
                .and_then(|c| workers.control(&c.config.device_id));
            let text = selected
                .as_ref()
                .map(|control| control.status_text())
                .unwrap_or_else(|| {
                    if new_connection {
                        "New connection — save to start".into()
                    } else {
                        "Not running — save to start this connection".into()
                    }
                });
            let title = if ui.dirty.get() {
                "Unsaved changes"
            } else {
                match selected.as_ref().map(|c| c.sync_state()) {
                    Some(SyncState::Idle) => "Up to date",
                    Some(SyncState::Syncing) => "Synchronizing…",
                    Some(SyncState::Paused) => "Paused",
                    Some(SyncState::Error) => "Needs attention",
                    None if new_connection => "Ready to set up",
                    None => "Not running",
                }
            };
            unsafe {
                SetWindowTextW(
                    GetDlgItem(ui.form.get(), STATUS_TITLE),
                    wide(title).as_ptr(),
                );
                SetWindowTextW(
                    GetDlgItem(ui.form.get(), PROFILE_STATUS),
                    wide(text).as_ptr(),
                );
            }
        }
    }

    unsafe fn taskbar_is_light() -> bool {
        unsafe {
            let mut value: u32 = 0;
            let mut size = size_of::<u32>() as u32;
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize").as_ptr(),
                wide("SystemUsesLightTheme").as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            ) == 0
                && value != 0
        }
    }

    unsafe fn taskbar_icon_pixels() -> i32 {
        unsafe {
            // Explorer is DPI aware, even if this fixed-layout settings window
            // uses Windows' automatic scaling. Query its primary taskbar DPI.
            let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
            let dpi = GetDpiForWindow(taskbar);
            let dpi = if dpi == 0 { GetDpiForSystem() } else { dpi };
            GetSystemMetricsForDpi(SM_CXSMICON, dpi.max(96)).max(16)
        }
    }

    unsafe fn set_window_icons(hwnd: HWND, ui: &UiState) {
        unsafe {
            let dpi = GetDpiForWindow(hwnd).max(96);
            for (kind, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
                let pixels = GetSystemMetricsForDpi(metric, dpi).max(16);
                match ui.icons.borrow_mut().get(APP_ICON, pixels) {
                    Ok(icon) => {
                        SendMessageW(hwnd, WM_SETICON, kind as usize, icon as LPARAM);
                    }
                    Err(error) => tracing::warn!(%error, "Could not resize window icon"),
                }
            }
        }
    }

    fn tray_resource(light: bool, state: SyncState) -> u16 {
        let base = if light { TRAY_ON_LIGHT } else { TRAY_ON_DARK };
        base + match state {
            SyncState::Idle => 0,
            SyncState::Syncing => 1,
            SyncState::Paused => 2,
            SyncState::Error => 3,
        }
    }

    unsafe fn update_icon(hwnd: HWND, ui: &UiState, operation: u32) -> bool {
        unsafe {
            let mut icon: NOTIFYICONDATAW = zeroed();
            icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = hwnd;
            icon.uID = 1;
            icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            icon.uCallbackMessage = TRAY_MESSAGE;
            if operation != NIM_DELETE {
                let resource = tray_resource(ui.light_taskbar.get(), ui.control.sync_state());
                match ui.icons.borrow_mut().get(resource, taskbar_icon_pixels()) {
                    Ok(handle) => icon.hIcon = handle,
                    Err(error) => {
                        tracing::warn!(%error, "Could not load notification icon");
                        return false;
                    }
                }
            }
            let tooltip = format!("Bucket Mount — {}", status(ui));
            // Leave a terminator, and do not split a UTF-16 surrogate pair.
            let encoded: Vec<u16> = tooltip.encode_utf16().collect();
            let mut count = encoded.len().min(icon.szTip.len() - 1);
            if count > 0 && (0xd800..=0xdbff).contains(&encoded[count - 1]) {
                count -= 1;
            }
            icon.szTip[..count].copy_from_slice(&encoded[..count]);
            Shell_NotifyIconW(operation, &icon) != 0
        }
    }

    unsafe fn show_menu(hwnd: HWND, ui: &UiState) {
        unsafe {
            refresh_status(ui);
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(status(ui)).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(
                menu,
                MF_STRING,
                CONFIGURE,
                wide("Connections\u{2026}").as_ptr(),
            );
            {
                let profiles = ui.profiles.borrow();
                let workers = ui.workers.borrow();
                for (index, connection) in profiles.connections.iter().enumerate() {
                    let submenu = CreatePopupMenu();
                    if submenu.is_null() {
                        continue;
                    }
                    let control = workers.control(&connection.config.device_id);
                    let text = control
                        .as_ref()
                        .map(|control| control.status_text())
                        .unwrap_or_else(|| "Stopped".into());
                    AppendMenuW(
                        submenu,
                        MF_STRING | MF_GRAYED,
                        0,
                        wide(
                            text.chars()
                                .take(160)
                                .collect::<String>()
                                .replace('&', "&&"),
                        )
                        .as_ptr(),
                    );
                    AppendMenuW(
                        submenu,
                        MF_STRING,
                        PROFILE_ACTION_BASE + index * 3,
                        wide("Open mounted folder").as_ptr(),
                    );
                    let flags = MF_STRING | if connection.enabled { 0 } else { MF_GRAYED };
                    AppendMenuW(
                        submenu,
                        flags,
                        PROFILE_ACTION_BASE + index * 3 + 1,
                        wide("Sync now").as_ptr(),
                    );
                    let paused = control.is_some_and(|c| c.paused.load(Ordering::Acquire));
                    AppendMenuW(
                        submenu,
                        flags,
                        PROFILE_ACTION_BASE + index * 3 + 2,
                        wide(if paused {
                            "Resume synchronization"
                        } else {
                            "Pause synchronization"
                        })
                        .as_ptr(),
                    );
                    AppendMenuW(
                        menu,
                        MF_STRING | MF_POPUP,
                        submenu as usize,
                        wide(connection.name.replace('&', "&&")).as_ptr(),
                    );
                }
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(
                menu,
                MF_STRING,
                SYNC_NOW,
                wide("Sync all connections now").as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                PAUSE,
                wide(if ui.control.paused.load(Ordering::Acquire) {
                    "Resume all connections"
                } else {
                    "Pause all connections"
                })
                .as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, QUIT, wide("Quit Bucket Mount").as_ptr());
            let mut point: POINT = zeroed();
            GetCursorPos(&mut point);
            SetForegroundWindow(hwnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                hwnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW(hwnd, WM_NULL, 0, 0);
            if selected != 0 {
                command(hwnd, ui, selected as usize);
            }
        }
    }

    unsafe fn command(hwnd: HWND, ui: &UiState, id: usize) {
        unsafe {
            if id >= PROFILE_ACTION_BASE {
                let index = (id - PROFILE_ACTION_BASE) / 3;
                let action = (id - PROFILE_ACTION_BASE) % 3;
                let connection = ui.profiles.borrow().connections.get(index).cloned();
                if let Some(connection) = connection {
                    if action == 0 {
                        open_path(hwnd, &connection.config.mount_dir);
                    } else if let Some(control) =
                        ui.workers.borrow().control(&connection.config.device_id)
                    {
                        if action == 1 {
                            control.sync_requested.store(true, Ordering::Release);
                        } else {
                            control.paused.fetch_xor(true, Ordering::AcqRel);
                        }
                    }
                }
                refresh_status(ui);
                return;
            }
            match id {
                CONFIGURE => show_config(hwnd, ui),
                SYNC_NOW => {
                    for connection in &ui.profiles.borrow().connections {
                        if let Some(control) =
                            ui.workers.borrow().control(&connection.config.device_id)
                        {
                            control.sync_requested.store(true, Ordering::Release);
                        }
                    }
                }
                PAUSE => {
                    let paused = !ui.control.paused.load(Ordering::Acquire);
                    for connection in &ui.profiles.borrow().connections {
                        if let Some(control) =
                            ui.workers.borrow().control(&connection.config.device_id)
                        {
                            control.paused.store(paused, Ordering::Release);
                        }
                    }
                    refresh_status(ui);
                    update_icon(hwnd, ui, NIM_MODIFY);
                }
                QUIT => {
                    PostMessageW(hwnd, WM_CLOSE, 0, 0);
                }
                _ => {}
            }
        }
    }

    unsafe fn open_path(hwnd: HWND, path: &Path) {
        unsafe {
            if !path.is_dir() {
                alert(
                    hwnd,
                    "The mounted folder is not available yet. Check the connection status and configuration.",
                    true,
                );
                return;
            }
            let outcome = ShellExecuteW(
                hwnd,
                wide("open").as_ptr(),
                wide(path).as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            );
            if outcome as isize <= 32 {
                alert(
                    hwnd,
                    "Could not open the mounted folder. Check the connection status and configuration.",
                    true,
                );
            }
        }
    }

    unsafe fn alert(hwnd: HWND, message: &str, error: bool) {
        unsafe {
            MessageBoxW(
                hwnd,
                wide(message).as_ptr(),
                wide("Bucket Mount").as_ptr(),
                MB_OK
                    | if error {
                        MB_ICONERROR
                    } else {
                        MB_ICONINFORMATION
                    },
            );
        }
    }

    unsafe fn show_config(owner: HWND, ui: &UiState) {
        unsafe {
            if !ui.form.get().is_null() {
                ShowWindow(ui.form.get(), SW_RESTORE);
                SetForegroundWindow(ui.form.get());
                return;
            }
            let dpi = GetDpiForWindow(owner).max(96);
            let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
            let mut bounds = RECT {
                left: 0,
                top: 0,
                right: design::WIDTH * dpi as i32 / 96,
                bottom: design::HEIGHT * dpi as i32 / 96,
            };
            AdjustWindowRectExForDpi(
                &mut bounds,
                style,
                0,
                WS_EX_APPWINDOW | WS_EX_CONTROLPARENT,
                dpi,
            );
            let form = CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_CONTROLPARENT,
                wide("BucketMount.ConfigurationWindow").as_ptr(),
                wide("Bucket Mount — Connections").as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                owner,
                null_mut(),
                GetModuleHandleW(null()),
                (ui as *const UiState).cast(),
            );
            if form.is_null() {
                alert(owner, "Could not open the configuration window.", true);
                return;
            }
            ui.form.set(form);
            refresh_status(ui);
            ShowWindow(form, SW_SHOW);
            SetForegroundWindow(form);
            SetFocus(GetDlgItem(form, PROFILE));
        }
    }

    unsafe fn control(
        hwnd: HWND,
        class: &str,
        text: &str,
        id: i32,
        style: u32,
        rect: (i32, i32, i32, i32),
    ) -> HWND {
        unsafe {
            let (x, y, width, height) = rect;
            let child = CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                width,
                height,
                hwnd,
                id as usize as HMENU,
                GetModuleHandleW(null()),
                null(),
            );
            if class == "EDIT" {
                SendMessageW(child, EM_SETLIMITTEXT, 4096, 0);
            }
            child
        }
    }

    unsafe fn populate_form(hwnd: HWND, ui: &UiState) {
        unsafe {
            *ui.design.borrow_mut() = Some(Design::new(hwnd));
            let config = ui.config.borrow().clone();
            let add = |class: &str, text: &str, id, style, layout: Layout, page, kind| {
                let child = control(hwnd, class, text, id, style, layout.rect);
                ui.design.borrow_mut().as_mut().unwrap().add(Item {
                    hwnd: child,
                    layout,
                    page,
                    kind,
                });
                child
            };
            let label = |text: &str, id, rect, anchor, page, font| {
                add(
                    "STATIC",
                    text,
                    id,
                    SS_NOPREFIX,
                    Layout::new(rect, anchor),
                    page,
                    Kind::Text(font),
                )
            };
            let field = |text: &str, id, value: &str, x, y, width, anchor, page| {
                label(text, 0, (x, y, width, 20), anchor, Some(page), Font::Body);
                add(
                    "EDIT",
                    value,
                    id,
                    WS_TABSTOP | ES_AUTOHSCROLL as u32,
                    Layout::new((x, y + 22, width, 34), anchor),
                    Some(page),
                    Kind::Edit,
                )
            };
            let button = |text: &str, id, layout, kind| {
                add(
                    "BUTTON",
                    text,
                    id,
                    WS_TABSTOP | BS_OWNERDRAW as u32,
                    layout,
                    None,
                    Kind::Button(kind),
                )
            };
            label(
                "Bucket Mount",
                0,
                (24, 28, 180, 28),
                Anchor::Left,
                None,
                Font::Section,
            );
            label(
                "CONNECTIONS",
                0,
                (24, 88, 172, 18),
                Anchor::Left,
                None,
                Font::Caption,
            );
            let selector = add(
                "COMBOBOX",
                "",
                PROFILE,
                WS_TABSTOP
                    | CBS_DROPDOWNLIST as u32
                    | CBS_OWNERDRAWFIXED as u32
                    | CBS_HASSTRINGS as u32
                    | WS_VSCROLL,
                Layout::new((20, 112, 180, 300), Anchor::Left),
                None,
                Kind::Combo,
            );
            for connection in &ui.profiles.borrow().connections {
                SendMessageW(
                    selector,
                    CB_ADDSTRING,
                    0,
                    wide(&connection.name).as_ptr() as LPARAM,
                );
            }
            let selected = if ui.editing.get() == NEW_PROFILE {
                SendMessageW(
                    selector,
                    CB_ADDSTRING,
                    0,
                    wide("New connection").as_ptr() as LPARAM,
                );
                ui.profiles.borrow().connections.len()
            } else {
                ui.editing.get()
            };
            SendMessageW(selector, CB_SETCURSEL, selected, 0);
            button(
                "+  Add connection",
                ADD_PROFILE as i32,
                Layout::new((20, 158, 180, 34), Anchor::Left),
                Button::Secondary,
            );
            for (page, (id, title)) in [
                (NAV_CONNECTION, "Connection"),
                (NAV_CREDENTIALS, "Credentials"),
                (NAV_SYNC, "Sync & cache"),
            ]
            .iter()
            .enumerate()
            {
                button(
                    title,
                    *id as i32,
                    Layout::new((12, 224 + page as i32 * 48, 196, 42), Anchor::Left),
                    Button::Navigation(page),
                );
            }
            let remove = button(
                "Remove connection",
                REMOVE_PROFILE as i32,
                Layout::new((20, 596, 180, 36), Anchor::Left).bottom(),
                Button::Remove,
            );
            EnableWindow(remove, i32::from(ui.editing.get() != NEW_PROFILE));
            for (page, (title, subtitle)) in [
                (
                    "Connection",
                    "Choose your bucket and where to access its files.",
                ),
                ("Credentials", "Manage how this connection signs in."),
                (
                    "Sync & cache",
                    "Control local storage and background synchronization.",
                ),
            ]
            .iter()
            .enumerate()
            {
                label(
                    title,
                    0,
                    (252, 24, 736, 38),
                    Anchor::Stretch,
                    Some(page),
                    Font::Title,
                );
                label(
                    subtitle,
                    0,
                    (254, 66, 730, 20),
                    Anchor::Stretch,
                    Some(page),
                    Font::Body,
                );
            }
            // Connection: familiar names and folders first, tuning on its own page.
            label(
                "Bucket details",
                0,
                (276, 120, 688, 24),
                Anchor::Stretch,
                Some(0),
                Font::Section,
            );
            field(
                "Connection name",
                NAME,
                &ui.draft_name.borrow(),
                276,
                150,
                332,
                Anchor::FirstColumn,
                0,
            );
            field(
                "Region",
                REGION,
                &config.region,
                632,
                150,
                332,
                Anchor::SecondColumn,
                0,
            );
            field(
                "Endpoint URL",
                ENDPOINT,
                &config.endpoint,
                276,
                220,
                688,
                Anchor::Stretch,
                0,
            );
            field(
                "Bucket",
                BUCKET,
                &config.bucket,
                276,
                290,
                332,
                Anchor::FirstColumn,
                0,
            );
            let prefix = field(
                "Key prefix (optional)",
                PREFIX,
                &config.prefix,
                632,
                290,
                332,
                Anchor::SecondColumn,
                0,
            );
            design::cue(prefix, "Entire bucket");
            label(
                "Files on this PC",
                0,
                (276, 394, 688, 24),
                Anchor::Stretch,
                Some(0),
                Font::Section,
            );
            field(
                "Mounted folder",
                MOUNT_DIR,
                &config.mount_dir.display().to_string(),
                276,
                426,
                552,
                Anchor::Stretch,
                0,
            );
            let drive = field(
                "Drive letter",
                DRIVE,
                &config
                    .drive_letter
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                852,
                426,
                112,
                Anchor::Right,
                0,
            );
            design::cue(drive, "Optional");
            // All pages stay alive so switching sections never loses unsaved edits.
            label(
                "Sign-in details",
                0,
                (276, 120, 688, 24),
                Anchor::Stretch,
                Some(1),
                Font::Section,
            );
            let secure = add(
                "BUTTON",
                "Protect saved credentials with my Windows account",
                SECURE,
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                Layout::new((276, 158, 688, 28), Anchor::Stretch),
                Some(1),
                Kind::Check,
            );
            SendMessageW(
                secure,
                BM_SETCHECK,
                usize::from(config.protected_credentials.is_some() || config.bucket.is_empty()),
                0,
            );
            for (label_id, field_id, x, y, width, anchor) in [
                (ACCESS_LABEL, ACCESS_ENV, 276, 220, 332, Anchor::FirstColumn),
                (
                    SECRET_LABEL,
                    SECRET_ENV,
                    632,
                    220,
                    332,
                    Anchor::SecondColumn,
                ),
                (SESSION_LABEL, SESSION_ENV, 276, 290, 688, Anchor::Stretch),
            ] {
                label("", label_id, (x, y, width, 20), anchor, Some(1), Font::Body);
                add(
                    "EDIT",
                    "",
                    field_id,
                    WS_TABSTOP | ES_AUTOHSCROLL as u32,
                    Layout::new((x, y + 22, width, 34), anchor),
                    Some(1),
                    Kind::Edit,
                );
            }
            label(
                "",
                CREDENTIAL_HELP,
                (276, 370, 688, 82),
                Anchor::Stretch,
                Some(1),
                Font::Body,
            );
            // Storage uses GiB in the editor; the configuration stays byte-precise.
            label(
                "Storage & synchronization",
                0,
                (276, 120, 688, 24),
                Anchor::Stretch,
                Some(2),
                Font::Section,
            );
            field(
                "Cache limit (GiB)",
                CACHE_BYTES,
                &cache_gib_text(config.cache_max_bytes),
                276,
                152,
                332,
                Anchor::FirstColumn,
                2,
            );
            field(
                "Cached file limit",
                CACHE_FILES,
                &config.cache_max_files.to_string(),
                632,
                152,
                332,
                Anchor::SecondColumn,
                2,
            );
            field(
                "Check for changes (seconds)",
                POLL_SECS,
                &config.poll_interval_secs.to_string(),
                276,
                222,
                332,
                Anchor::FirstColumn,
                2,
            );
            field(
                "Wait after edits (seconds)",
                SETTLE_SECS,
                &config.settle_delay_secs.to_string(),
                632,
                222,
                332,
                Anchor::SecondColumn,
                2,
            );
            label(
                "Advanced",
                0,
                (276, 332, 688, 24),
                Anchor::Stretch,
                Some(2),
                Font::Section,
            );
            field(
                "State folder",
                STATE_DIR,
                &config.state_dir.display().to_string(),
                276,
                366,
                500,
                Anchor::Stretch,
                2,
            );
            field(
                "Parallel uploads",
                UPLOADS,
                &config.upload_concurrency.to_string(),
                800,
                366,
                164,
                Anchor::Right,
                2,
            );
            for (text, id, checked, rect, page) in [
                (
                    "Enable this connection when the app starts",
                    ENABLED,
                    ui.draft_enabled.get(),
                    (254, 510, 734, 28),
                    0,
                ),
                (
                    "OVHcloud conditional requests",
                    OVH_CONDITIONALS,
                    config.conditional_etag_format == ConditionalEtagFormat::Bare,
                    (276, 438, 688, 28),
                    2,
                ),
                (
                    "Allow HTTP for trusted local services",
                    ALLOW_HTTP,
                    config.allow_http,
                    (276, 478, 688, 28),
                    2,
                ),
            ] {
                let checkbox = add(
                    "BUTTON",
                    text,
                    id,
                    WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                    Layout::new(rect, Anchor::Stretch),
                    Some(page),
                    Kind::Check,
                );
                SendMessageW(checkbox, BM_SETCHECK, usize::from(checked), 0);
            }
            add(
                "STATIC",
                "Connection status",
                STATUS_TITLE,
                SS_NOPREFIX | SS_ENDELLIPSIS,
                Layout::new((252, 594, 340, 20), Anchor::Stretch).bottom(),
                None,
                Kind::Text(Font::Strong),
            );
            button(
                "Details",
                STATUS_DETAILS as i32,
                Layout::new((610, 590, 82, 28), Anchor::Right).bottom(),
                Button::Link,
            );
            add(
                "STATIC",
                "",
                PROFILE_STATUS,
                SS_NOPREFIX,
                Layout::new((252, 618, 440, 32), Anchor::Stretch).bottom(),
                None,
                Kind::Text(Font::Caption),
            );
            button(
                "&Save and apply",
                SAVE as i32,
                Layout::new((718, 600, 152, 36), Anchor::Right).bottom(),
                Button::Primary,
            );
            button(
                "Close",
                CANCEL as i32,
                Layout::new((882, 600, 106, 36), Anchor::Right).bottom(),
                Button::Secondary,
            );
            credential_mode(hwnd, ui);
            ui.dirty.set(false);
            layout_form(hwnd, ui);
            refresh_status(ui);
        }
    }

    unsafe fn layout_form(hwnd: HWND, ui: &UiState) {
        unsafe {
            if ui.laying_out.replace(true) {
                return;
            }
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            ShowScrollBar(
                hwnd,
                SB_VERT,
                i32::from(client.bottom * 96 / (GetDpiForWindow(hwnd).max(96) as i32) < 620),
            );
            if let Some(design) = ui.design.borrow_mut().as_mut() {
                design.layout(hwnd, ui.section.get());
            }
            ui.laying_out.set(false);
        }
    }

    unsafe fn show_section(hwnd: HWND, ui: &UiState, section: usize) {
        unsafe {
            ui.section.set(section);
            if let Some(design) = ui.design.borrow_mut().as_mut() {
                design.scroll = 0;
            }
            layout_form(hwnd, ui);
            SetFocus(GetDlgItem(hwnd, (NAV_CONNECTION + section) as i32));
        }
    }

    fn cache_gib_text(bytes: u64) -> String {
        (bytes as f64 / 1_073_741_824.0).to_string()
    }

    fn parse_cache_gib(text: &str, previous: u64) -> Result<u64> {
        if text == cache_gib_text(previous) {
            return Ok(previous);
        }
        let gib: f64 = text
            .parse()
            .context("Cache limit must be a number in GiB, such as 10 or 0.5")?;
        let bytes = gib * 1_073_741_824.0;
        anyhow::ensure!(
            bytes.is_finite() && bytes >= 4096.0 && bytes < u64::MAX as f64,
            "Cache limit must be at least 4096 bytes and fit in a 64-bit byte count"
        );
        Ok(bytes.round() as u64)
    }

    unsafe fn credential_mode(hwnd: HWND, ui: &UiState) {
        unsafe {
            let secure =
                SendMessageW(GetDlgItem(hwnd, SECURE), BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            let config = ui.config.borrow();
            for (label_id, field_id, label, value) in [
                (
                    ACCESS_LABEL,
                    ACCESS_ENV,
                    if secure {
                        "Access key"
                    } else {
                        "Access key variable"
                    },
                    &config.access_key_env,
                ),
                (
                    SECRET_LABEL,
                    SECRET_ENV,
                    if secure {
                        "Secret key"
                    } else {
                        "Secret key variable"
                    },
                    &config.secret_key_env,
                ),
                (
                    SESSION_LABEL,
                    SESSION_ENV,
                    if secure {
                        "Session token (optional)"
                    } else {
                        "Session token variable"
                    },
                    &config.session_token_env.clone().unwrap_or_default(),
                ),
            ] {
                SetWindowTextW(GetDlgItem(hwnd, label_id), wide(label).as_ptr());
                SendMessageW(
                    GetDlgItem(hwnd, field_id),
                    EM_SETPASSWORDCHAR,
                    if secure { 0x25cf } else { 0 },
                    0,
                );
                SendMessageW(
                    GetDlgItem(hwnd, field_id),
                    EM_SETLIMITTEXT,
                    if secure { 32768 } else { 255 },
                    0,
                );
                SetWindowTextW(
                    GetDlgItem(hwnd, field_id),
                    wide(if secure { "" } else { value }).as_ptr(),
                );
                if secure {
                    design::cue(
                        GetDlgItem(hwnd, field_id),
                        if config.protected_credentials.is_some() {
                            "Saved — leave blank to keep"
                        } else if field_id == SESSION_ENV {
                            "Optional"
                        } else {
                            "Enter your key"
                        },
                    );
                } else {
                    design::cue(GetDlgItem(hwnd, field_id), "Environment variable name");
                }
            }
            let help = if secure && config.protected_credentials.is_some() {
                "Credentials are saved and encrypted. Leave all three fields blank to keep them. To replace them, enter both keys; a blank session token removes the old token."
            } else if secure {
                "Enter your keys here. Windows encrypts them for your account before saving. Leave the session token blank unless you use temporary credentials."
            } else {
                "Enter environment variable names, not secret values. Set those variables before starting the app. Leave the session token variable blank when unused."
            };
            SetWindowTextW(GetDlgItem(hwnd, CREDENTIAL_HELP), wide(help).as_ptr());
        }
    }

    unsafe fn discard_edits(hwnd: HWND, ui: &UiState) -> bool {
        !ui.dirty.get()
            || unsafe {
                MessageBoxW(
                    hwnd,
                    wide("Discard unsaved changes to this connection?").as_ptr(),
                    wide("Bucket Mount").as_ptr(),
                    MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
                ) == IDYES
            }
    }

    unsafe fn select_profile(hwnd: HWND, ui: &UiState, index: usize) {
        unsafe {
            let connection = ui
                .profiles
                .borrow()
                .connections
                .get(index)
                .cloned()
                .unwrap_or_else(|| Connection::new("New connection".into()));
            ui.editing
                .set(if index < ui.profiles.borrow().connections.len() {
                    index
                } else {
                    NEW_PROFILE
                });
            *ui.config.borrow_mut() = connection.config;
            *ui.draft_name.borrow_mut() = connection.name;
            ui.draft_enabled.set(connection.enabled);
            loop {
                let child = GetWindow(hwnd, GW_CHILD);
                if child.is_null() {
                    break;
                }
                DestroyWindow(child);
            }
            populate_form(hwnd, ui);
            SetFocus(GetDlgItem(hwnd, (NAV_CONNECTION + ui.section.get()) as i32));
        }
    }

    unsafe fn remove_profile(hwnd: HWND, ui: &UiState) -> Result<()> {
        unsafe {
            let index = ui.editing.get();
            if index == NEW_PROFILE {
                return Ok(());
            }
            if MessageBoxW(hwnd, wide("Remove this connection and its saved credentials?\n\nLocal files, sync state, and files in the bucket will be kept. Any current synchronization finishes before the connection stops.").as_ptr(), wide("Remove connection").as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) != IDYES { return Ok(()); }
            let mut profiles = ui.profiles.borrow().clone();
            profiles.connections.remove(index);
            profiles.save(&ui.config_path)?;
            *ui.profiles.borrow_mut() = profiles.clone();
            ui.workers.borrow_mut().apply(&profiles)?;
            select_profile(
                hwnd,
                ui,
                index.min(profiles.connections.len().saturating_sub(1)),
            );
            Ok(())
        }
    }

    unsafe fn secret_field(hwnd: HWND, id: i32) -> String {
        unsafe {
            let child = GetDlgItem(hwnd, id);
            let length = GetWindowTextLengthW(child).max(0) as usize;
            let mut text = vec![0u16; length + 1];
            let count = GetWindowTextW(child, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
            let result = String::from_utf16_lossy(&text[..count]);
            for unit in &mut text {
                std::ptr::write_volatile(unit, 0);
            }
            result
        }
    }

    unsafe fn field(hwnd: HWND, id: i32) -> String {
        unsafe {
            let child = GetDlgItem(hwnd, id);
            let length = GetWindowTextLengthW(child).max(0) as usize;
            let mut text = vec![0u16; length + 1];
            let count = GetWindowTextW(child, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
            String::from_utf16_lossy(&text[..count]).trim().to_owned()
        }
    }

    unsafe fn save_form(hwnd: HWND, ui: &UiState) -> Result<()> {
        unsafe {
            let mut config = ui.config.borrow().clone();
            config.endpoint = field(hwnd, ENDPOINT);
            config.bucket = field(hwnd, BUCKET);
            config.region = field(hwnd, REGION);
            config.prefix = crate::config::canonical_prefix(&field(hwnd, PREFIX))?;
            config.mount_dir = PathBuf::from(field(hwnd, MOUNT_DIR));
            config.state_dir = PathBuf::from(field(hwnd, STATE_DIR));
            let drive = field(hwnd, DRIVE).trim_end_matches(':').to_uppercase();
            config.drive_letter = if drive.is_empty() {
                None
            } else {
                if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
                    bail!("Drive letter must be one letter, such as S, or left blank");
                }
                drive.chars().next()
            };
            config.cache_max_bytes =
                parse_cache_gib(&field(hwnd, CACHE_BYTES), config.cache_max_bytes)?;
            config.cache_max_files = field(hwnd, CACHE_FILES)
                .parse()
                .context("Cached file limit must be a whole number")?;
            config.poll_interval_secs = field(hwnd, POLL_SECS)
                .parse()
                .context("Poll interval must be a whole number of seconds")?;
            config.settle_delay_secs = field(hwnd, SETTLE_SECS)
                .parse()
                .context("Settle delay must be a whole number of seconds")?;
            config.upload_concurrency = field(hwnd, UPLOADS)
                .parse()
                .context("Parallel uploads must be a whole number")?;
            let secure =
                SendMessageW(GetDlgItem(hwnd, SECURE), BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
            if secure {
                let secrets = Secrets {
                    access_key: secret_field(hwnd, ACCESS_ENV),
                    secret_key: secret_field(hwnd, SECRET_ENV),
                    session_token: Some(secret_field(hwnd, SESSION_ENV)).filter(|s| !s.is_empty()),
                };
                if !secrets.access_key.is_empty()
                    || !secrets.secret_key.is_empty()
                    || secrets.session_token.is_some()
                {
                    config.protected_credentials = Some(secrets.protect(&config.device_id)?);
                } else if config.protected_credentials.is_none() {
                    bail!("Enter your access key and secret key before saving");
                }
            } else {
                config.protected_credentials = None;
                config.access_key_env = field(hwnd, ACCESS_ENV);
                config.secret_key_env = field(hwnd, SECRET_ENV);
                config.session_token_env = Some(field(hwnd, SESSION_ENV)).filter(|s| !s.is_empty());
            }
            config.allow_http = SendMessageW(GetDlgItem(hwnd, ALLOW_HTTP), BM_GETCHECK, 0, 0)
                == BST_CHECKED as isize;
            config.conditional_etag_format =
                if SendMessageW(GetDlgItem(hwnd, OVH_CONDITIONALS), BM_GETCHECK, 0, 0)
                    == BST_CHECKED as isize
                {
                    ConditionalEtagFormat::Bare
                } else {
                    ConditionalEtagFormat::Quoted
                };
            let previous = ui.config.borrow();
            if previous.state_dir.join("state.sqlite3").exists()
                && previous.state_dir == config.state_dir
                && (previous.endpoint != config.endpoint
                    || previous.bucket != config.bucket
                    || previous.prefix != config.prefix
                    || previous.mount_dir != config.mount_dir)
            {
                bail!(
                    "This state folder belongs to the original connection. Use a new connection with a separate mounted folder and state folder when changing the bucket, endpoint, prefix, or mounted folder."
                );
            }
            drop(previous);
            let connection = Connection {
                name: field(hwnd, NAME),
                enabled: SendMessageW(GetDlgItem(hwnd, ENABLED), BM_GETCHECK, 0, 0)
                    == BST_CHECKED as isize,
                config,
            };
            let mut profiles = ui.profiles.borrow().clone();
            let index = if ui.editing.get() == NEW_PROFILE {
                profiles.connections.push(connection);
                profiles.connections.len() - 1
            } else {
                profiles.connections[ui.editing.get()] = connection;
                ui.editing.get()
            };
            profiles.validate_structure()?;
            profiles.validate_connection(index)?;
            ui.workers.borrow().validate_resources(&profiles)?;
            profiles.save(&ui.config_path)?;
            *ui.profiles.borrow_mut() = profiles.clone();
            ui.workers.borrow_mut().apply(&profiles)?;
            select_profile(hwnd, ui, index);
            Ok(())
        }
    }

    unsafe extern "system" fn form_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if msg == WM_NCCREATE {
                set_state(hwnd, lp);
            }
            let Some(ui) = state(hwnd) else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            match msg {
                WM_CREATE => {
                    set_window_icons(hwnd, ui);
                    populate_form(hwnd, ui);
                    0
                }
                WM_DPICHANGED => {
                    let bounds = &*(lp as *const RECT);
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        bounds.left,
                        bounds.top,
                        bounds.right - bounds.left,
                        bounds.bottom - bounds.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    layout_form(hwnd, ui);
                    set_window_icons(hwnd, ui);
                    0
                }
                WM_SIZE => {
                    layout_form(hwnd, ui);
                    0
                }
                WM_VSCROLL | WM_MOUSEWHEEL => {
                    let mut info: SCROLLINFO = zeroed();
                    info.cbSize = size_of::<SCROLLINFO>() as u32;
                    info.fMask = SIF_ALL;
                    GetScrollInfo(hwnd, SB_VERT, &mut info);
                    if let Some(design) = ui.design.borrow_mut().as_mut() {
                        if msg == WM_MOUSEWHEEL {
                            design.scroll -= ((wp >> 16) as i16 as i32) * 40 / 120;
                        } else {
                            match (wp & 0xffff) as i32 {
                                SB_LINEUP => design.scroll -= 32,
                                SB_LINEDOWN => design.scroll += 32,
                                SB_PAGEUP => design.scroll -= info.nPage as i32,
                                SB_PAGEDOWN => design.scroll += info.nPage as i32,
                                SB_THUMBTRACK | SB_THUMBPOSITION => design.scroll = info.nTrackPos,
                                SB_TOP => design.scroll = 0,
                                SB_BOTTOM => design.scroll = 438,
                                _ => {}
                            }
                        }
                    }
                    layout_form(hwnd, ui);
                    0
                }
                design::FOCUS_CHANGED => {
                    if let Some(design) = ui.design.borrow_mut().as_mut() {
                        design.reveal(hwnd, wp as HWND);
                    }
                    layout_form(hwnd, ui);
                    0
                }
                WM_GETMINMAXINFO => {
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    let mut bounds = RECT {
                        left: 0,
                        top: 0,
                        right: design::MIN_WIDTH * dpi as i32 / 96,
                        bottom: design::MIN_HEIGHT * dpi as i32 / 96,
                    };
                    AdjustWindowRectExForDpi(
                        &mut bounds,
                        GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
                        0,
                        GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32,
                        dpi,
                    );
                    let limits = &mut *(lp as *mut MINMAXINFO);
                    limits.ptMinTrackSize = POINT {
                        x: bounds.right - bounds.left,
                        y: bounds.bottom - bounds.top,
                    };
                    0
                }
                WM_ERASEBKGND => {
                    if let Ok(design) = ui.design.try_borrow()
                        && let Some(design) = design.as_ref()
                    {
                        design.paint(hwnd, wp as _);
                    }
                    1
                }
                WM_PAINT => {
                    let mut paint: PAINTSTRUCT = zeroed();
                    let dc = BeginPaint(hwnd, &mut paint);
                    if let Ok(design) = ui.design.try_borrow()
                        && let Some(design) = design.as_ref()
                    {
                        design.paint(hwnd, dc);
                    }
                    EndPaint(hwnd, &paint);
                    0
                }
                WM_PRINTCLIENT => {
                    if let Ok(design) = ui.design.try_borrow()
                        && let Some(design) = design.as_ref()
                    {
                        design.paint(hwnd, wp as _);
                    }
                    0
                }
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN => {
                    // Native controls can paint synchronously while layout holds
                    // the mutable design. A full repaint is queued after layout.
                    if let Ok(design) = ui.design.try_borrow()
                        && let Some(design) = design.as_ref()
                    {
                        design.color(lp as HWND, wp as _) as isize
                    } else {
                        DefWindowProcW(hwnd, msg, wp, lp)
                    }
                }
                WM_MEASUREITEM => {
                    let measure = &mut *(lp as *mut MEASUREITEMSTRUCT);
                    if measure.CtlType == ODT_COMBOBOX && measure.CtlID == PROFILE as u32 {
                        measure.itemHeight =
                            design::COMBO_ITEM_HEIGHT as u32 * GetDpiForWindow(hwnd).max(96) / 96;
                        1
                    } else {
                        0
                    }
                }
                WM_DRAWITEM => {
                    let draw = &*(lp as *const DRAWITEMSTRUCT);
                    if let Ok(design) = ui.design.try_borrow()
                        && let Some(design) = design.as_ref()
                    {
                        isize::from(design.draw_combo(draw) || design.draw_button(draw))
                    } else {
                        0
                    }
                }
                DM_GETDEFID => ((DC_HASDEFID as usize) << 16 | SAVE) as isize,
                WM_SETTINGCHANGE | WM_THEMECHANGED => {
                    if let Some(design) = ui.design.borrow_mut().as_mut() {
                        design.refresh_theme(hwnd);
                    }
                    InvalidateRect(hwnd, null(), 1);
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
                WM_COMMAND => {
                    let id = wp & 0xffff;
                    let notification = (wp >> 16) & 0xffff;
                    if (NAV_CONNECTION..=NAV_SYNC).contains(&id)
                        && notification == BN_CLICKED as usize
                    {
                        show_section(hwnd, ui, id - NAV_CONNECTION);
                    } else if id == PROFILE as usize && notification == CBN_SELCHANGE as usize {
                        let selected =
                            SendMessageW(GetDlgItem(hwnd, PROFILE), CB_GETCURSEL, 0, 0) as usize;
                        if discard_edits(hwnd, ui) {
                            select_profile(hwnd, ui, selected);
                        } else {
                            let previous = if ui.editing.get() == NEW_PROFILE {
                                ui.profiles.borrow().connections.len()
                            } else {
                                ui.editing.get()
                            };
                            SendMessageW(GetDlgItem(hwnd, PROFILE), CB_SETCURSEL, previous, 0);
                        }
                    } else if id == SECURE as usize && notification == BN_CLICKED as usize {
                        credential_mode(hwnd, ui);
                        ui.dirty.set(true);
                    } else if notification == EN_CHANGE as usize
                        || [
                            ENABLED as usize,
                            ALLOW_HTTP as usize,
                            OVH_CONDITIONALS as usize,
                        ]
                        .contains(&id)
                    {
                        ui.dirty.set(true);
                    } else {
                        match id {
                            STATUS_DETAILS => alert(hwnd, &field(hwnd, PROFILE_STATUS), false),
                            SAVE => {
                                if let Err(error) = save_form(hwnd, ui) {
                                    alert(
                                        hwnd,
                                        &format!("Could not save connection:\n\n{error:#}"),
                                        true,
                                    );
                                }
                            }
                            CANCEL => {
                                if discard_edits(hwnd, ui) {
                                    DestroyWindow(hwnd);
                                }
                            }
                            ADD_PROFILE => {
                                if discard_edits(hwnd, ui) {
                                    select_profile(hwnd, ui, NEW_PROFILE);
                                }
                            }
                            REMOVE_PROFILE => {
                                if let Err(error) = remove_profile(hwnd, ui) {
                                    alert(
                                        hwnd,
                                        &format!("Could not remove connection:\n\n{error:#}"),
                                        true,
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    0
                }
                WM_CLOSE => {
                    if discard_edits(hwnd, ui) {
                        DestroyWindow(hwnd);
                    }
                    0
                }
                WM_DESTROY => {
                    ui.form.set(null_mut());
                    0
                }
                WM_NCDESTROY => {
                    ui.design.borrow_mut().take();
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::BTreeSet;
        use windows_sys::Win32::Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
            GetDIBits, GetObjectW, ReleaseDC,
        };
        use windows_sys::Win32::UI::Controls::EM_GETPASSWORDCHAR;

        #[test]
        fn cache_limits_accept_gib_and_preserve_existing_byte_counts() {
            assert_eq!(
                parse_cache_gib("10", 4096).unwrap(),
                10 * 1024 * 1024 * 1024
            );
            assert_eq!(parse_cache_gib("0.5", 4096).unwrap(), 512 * 1024 * 1024);
            for bytes in [4096, 123456789, 10737418241, u64::MAX - 1] {
                assert_eq!(
                    parse_cache_gib(&cache_gib_text(bytes), bytes).unwrap(),
                    bytes
                );
            }
            for invalid in ["", "abc", "NaN", "inf", "-1", "0", "1e40"] {
                assert!(parse_cache_gib(invalid, 4096).is_err(), "{invalid}");
            }
        }

        unsafe fn capture_preview(hwnd: HWND, path: &Path) {
            use windows_sys::Win32::Graphics::Gdi::*;
            unsafe {
                RedrawWindow(
                    hwnd,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
                let mut client: RECT = zeroed();
                GetClientRect(hwnd, &mut client);
                let dc = CreateCompatibleDC(null_mut());
                let mut layout: BITMAPINFO = zeroed();
                layout.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                layout.bmiHeader.biWidth = client.right;
                layout.bmiHeader.biHeight = -client.bottom;
                layout.bmiHeader.biPlanes = 1;
                layout.bmiHeader.biBitCount = 32;
                layout.bmiHeader.biCompression = BI_RGB;
                let mut pixels = null_mut();
                let bitmap =
                    CreateDIBSection(dc, &layout, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
                assert!(!bitmap.is_null());
                let previous = SelectObject(dc, bitmap);
                SendMessageW(hwnd, WM_PRINTCLIENT, dc as usize, PRF_CLIENT as isize);
                let mut child = GetWindow(hwnd, GW_CHILD);
                while !child.is_null() {
                    if GetWindowLongPtrW(child, GWL_STYLE) & WS_VISIBLE as isize != 0 {
                        let mut rect: RECT = zeroed();
                        GetWindowRect(child, &mut rect);
                        MapWindowPoints(null_mut(), hwnd, (&mut rect as *mut RECT).cast(), 2);
                        let saved = SaveDC(dc);
                        SetViewportOrgEx(dc, rect.left, rect.top, null_mut());
                        IntersectClipRect(dc, 0, 0, rect.right - rect.left, rect.bottom - rect.top);
                        let region = CreateRectRgn(0, 0, 0, 0);
                        if GetWindowRgn(child, region) != ERROR {
                            // Clip regions use device coordinates, unlike the
                            // viewport-relative coordinates used to print controls.
                            OffsetRgn(region, rect.left, rect.top);
                            ExtSelectClipRgn(dc, region, RGN_AND);
                        }
                        DeleteObject(region);
                        SendMessageW(
                            child,
                            WM_PRINT,
                            dc as usize,
                            (PRF_CLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize,
                        );
                        RestoreDC(dc, saved);
                    }
                    child = GetWindow(child, GW_HWNDNEXT);
                }
                GdiFlush();
                let bytes = std::slice::from_raw_parts(
                    pixels.cast::<u8>(),
                    (client.right * client.bottom * 4) as usize,
                );
                let mut file = Vec::new();
                file.extend_from_slice(b"BM");
                file.extend_from_slice(&(54 + bytes.len() as u32).to_le_bytes());
                file.extend_from_slice(&[0; 4]);
                file.extend_from_slice(&54u32.to_le_bytes());
                file.extend_from_slice(&40u32.to_le_bytes());
                file.extend_from_slice(&client.right.to_le_bytes());
                file.extend_from_slice(&(-client.bottom).to_le_bytes());
                file.extend_from_slice(&1u16.to_le_bytes());
                file.extend_from_slice(&32u16.to_le_bytes());
                file.extend_from_slice(&[0; 24]);
                file.extend_from_slice(bytes);
                std::fs::write(path, file).unwrap();
                SelectObject(dc, previous);
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
        }

        unsafe fn check_layout_and_preview(form: HWND, ui: &UiState) {
            unsafe {
                let output = std::env::var_os("BUCKET_MOUNT_UI_PREVIEW_DIR").map(PathBuf::from);
                let mut restore = None;
                if let Some(output) = &output {
                    std::fs::create_dir_all(output).unwrap();
                    // Documentation captures show a representative running
                    // connection with ordinary paths; this test keeps its
                    // worker disabled and its fixture in a temporary folder.
                    for connection in &ui.profiles.borrow().connections {
                        if let Some(control) =
                            ui.workers.borrow().control(&connection.config.device_id)
                        {
                            control.set_status(
                                SyncState::Idle,
                                "1,284 cached files, 3.4 GiB; 0 pending, 0 conflicts, 0 errors"
                                    .into(),
                            );
                        }
                    }
                    let fields = [
                        (MOUNT_DIR, r"C:\Users\you\BucketMount"),
                        (
                            STATE_DIR,
                            r"C:\Users\you\AppData\Local\BucketMountProfiles\desktop-1a2b3c4d",
                        ),
                    ]
                    .map(|(id, text)| {
                        let before = field(form, id);
                        SetWindowTextW(GetDlgItem(form, id), wide(text).as_ptr());
                        (id, before)
                    });
                    let dirty = ui.dirty.replace(false);
                    refresh_status(ui);
                    restore = Some((dirty, fields));
                }
                for (width, height, dpi, name) in [
                    (1020, 660, 96, "normal"),
                    (940, 620, 96, "compact"),
                    (740, 480, 96, "small"),
                    (1530, 990, 144, "150-percent"),
                    (2040, 1320, 192, "200-percent"),
                ] {
                    SetWindowPos(
                        form,
                        null_mut(),
                        0,
                        0,
                        width,
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                    );
                    for page in 0..3 {
                        ui.section.set(page);
                        ui.laying_out.set(true);
                        ui.design.borrow_mut().as_mut().unwrap().scroll = 0;
                        ui.design
                            .borrow_mut()
                            .as_mut()
                            .unwrap()
                            .layout_at_dpi(form, page, dpi);
                        ui.laying_out.set(false);
                        let mut client: RECT = zeroed();
                        GetClientRect(form, &mut client);
                        let design = ui.design.borrow();
                        for item in &design.as_ref().unwrap().items {
                            let visible =
                                GetWindowLongPtrW(item.hwnd, GWL_STYLE) & WS_VISIBLE as isize != 0;
                            assert_eq!(visible, item.page.is_none_or(|p| p == page));
                            if !visible || matches!(item.kind, Kind::Combo) {
                                continue;
                            }
                            let mut rect: RECT = zeroed();
                            GetWindowRect(item.hwnd, &mut rect);
                            windows_sys::Win32::Graphics::Gdi::MapWindowPoints(
                                null_mut(),
                                form,
                                (&mut rect as *mut RECT).cast(),
                                2,
                            );
                            if item.page.is_some() && item.layout.rect.1 >= 100 {
                                let mut clip: RECT = zeroed();
                                if windows_sys::Win32::Graphics::Gdi::GetWindowRgnBox(
                                    item.hwnd, &mut clip,
                                ) == windows_sys::Win32::Graphics::Gdi::NULLREGION
                                {
                                    continue;
                                }
                                let (x, y) = (rect.left, rect.top);
                                rect = RECT {
                                    left: x + clip.left,
                                    top: y + clip.top,
                                    right: x + clip.right,
                                    bottom: y + clip.bottom,
                                };
                                assert!(
                                    rect.top >= 100 * dpi as i32 / 96
                                        && rect.bottom <= client.bottom - 82 * dpi as i32 / 96
                                );
                            }
                            assert!(
                                rect.left >= 0
                                    && rect.top >= 0
                                    && rect.right <= client.right
                                    && rect.bottom <= client.bottom,
                                "control {} outside {name} page {page}: {},{},{},{}",
                                GetDlgCtrlID(item.hwnd),
                                rect.left,
                                rect.top,
                                rect.right,
                                rect.bottom
                            );
                        }
                        drop(design);
                        if let Some(output) = &output {
                            capture_preview(
                                form,
                                &output.join(format!("settings-{page}-{name}.bmp")),
                            );
                            if page == 0 {
                                use windows_sys::Win32::UI::Controls::{
                                    COMBOBOXINFO, GetComboBoxInfo,
                                };
                                let selector = GetDlgItem(form, PROFILE);
                                let mut info: COMBOBOXINFO = zeroed();
                                info.cbSize = size_of::<COMBOBOXINFO>() as u32;
                                GetComboBoxInfo(selector, &mut info);
                                let mut bounds: RECT = zeroed();
                                GetWindowRect(selector, &mut bounds);
                                // Render the hidden native popup alongside the
                                // form preview without opening it on the desktop.
                                SetWindowPos(
                                    info.hwndList,
                                    null_mut(),
                                    bounds.left,
                                    bounds.bottom,
                                    bounds.right - bounds.left,
                                    design::COMBO_ITEM_HEIGHT * dpi as i32 / 96 + 2,
                                    SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                                capture_preview(
                                    info.hwndList,
                                    &output.join(format!("connection-options-{name}.bmp")),
                                );
                            }
                        }
                        if name == "small" && page != 1 {
                            let last =
                                GetDlgItem(form, if page == 0 { ENABLED } else { ALLOW_HTTP });
                            SendMessageW(form, design::FOCUS_CHANGED, last as usize, 0);
                            assert!(ui.design.borrow().as_ref().unwrap().scroll > 0);
                            let mut client: RECT = zeroed();
                            GetClientRect(last, &mut client);
                            let mut visible: RECT = zeroed();
                            windows_sys::Win32::Graphics::Gdi::GetWindowRgnBox(last, &mut visible);
                            assert_eq!(visible.top, 0, "keyboard focus reveals the whole control");
                            assert_eq!(visible.bottom, client.bottom);
                            if let Some(output) = &output {
                                capture_preview(
                                    form,
                                    &output.join(format!("settings-{page}-small-scrolled.bmp")),
                                );
                            }
                        }
                    }
                }
                SetWindowPos(
                    form,
                    null_mut(),
                    0,
                    0,
                    1020,
                    660,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                );
                if let Some((dirty, fields)) = restore {
                    for (id, text) in fields {
                        SetWindowTextW(GetDlgItem(form, id), wide(&text).as_ptr());
                    }
                    ui.dirty.set(dirty);
                }
                show_section(form, ui, 0);
            }
        }

        unsafe fn pixels(icon: HICON, size: i32) -> Vec<u8> {
            unsafe {
                let mut info: ICONINFO = zeroed();
                assert_ne!(GetIconInfo(icon, &mut info), 0);
                let mut bitmap: BITMAP = zeroed();
                assert_ne!(
                    GetObjectW(
                        info.hbmColor,
                        size_of::<BITMAP>() as i32,
                        (&mut bitmap as *mut BITMAP).cast(),
                    ),
                    0
                );
                assert_eq!((bitmap.bmWidth, bitmap.bmHeight), (size, size));
                let mut layout: BITMAPINFO = zeroed();
                layout.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                layout.bmiHeader.biWidth = size;
                layout.bmiHeader.biHeight = -size;
                layout.bmiHeader.biPlanes = 1;
                layout.bmiHeader.biBitCount = 32;
                layout.bmiHeader.biCompression = BI_RGB;
                let mut bytes = vec![0; (size * size * 4) as usize];
                let dc = GetDC(null_mut());
                let copied = GetDIBits(
                    dc,
                    info.hbmColor,
                    0,
                    size as u32,
                    bytes.as_mut_ptr().cast(),
                    &mut layout,
                    DIB_RGB_COLORS,
                );
                ReleaseDC(null_mut(), dc);
                DeleteObject(info.hbmColor);
                DeleteObject(info.hbmMask);
                assert_eq!(copied, size);
                bytes
            }
        }

        struct TestWindow(HWND);

        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe {
                    let mut icon: NOTIFYICONDATAW = zeroed();
                    icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
                    icon.hWnd = self.0;
                    icon.uID = 1;
                    Shell_NotifyIconW(NIM_DELETE, &icon);
                    DestroyWindow(self.0);
                }
            }
        }

        #[test]
        #[ignore = "requires native Windows controls and embedded application resources"]
        fn native_editor_manages_profiles_and_encrypts_credentials() {
            let directory = tempfile::tempdir().unwrap();
            let config_path = directory.path().join("config.toml");
            let mut first = Connection::new("Personal storage".into());
            first.enabled = false; // Exercise the real editor without contacting any bucket.
            first.config.bucket = "first-bucket".into();
            first.config.mount_dir = directory.path().join("mount-first");
            first.config.state_dir = directory.path().join("state-first");
            let profiles = Profiles {
                format_version: 1,
                connections: vec![first.clone()],
            };
            let ui = Box::new(UiState {
                config_path: config_path.clone(),
                config: RefCell::new(first.config),
                profiles: RefCell::new(profiles.clone()),
                workers: RefCell::new(Manager::new().unwrap()),
                editing: Cell::new(0),
                draft_name: RefCell::new(first.name),
                draft_enabled: Cell::new(false),
                dirty: Cell::new(false),
                control: Arc::new(Control::default()),
                form: Cell::new(null_mut()),
                taskbar_created: 0,
                icons: RefCell::new(IconCache::default()),
                light_taskbar: Cell::new(false),
                design: RefCell::new(None),
                section: Cell::new(0),
                laying_out: Cell::new(false),
            });
            // Match the application, which starts a worker for every saved connection.
            ui.workers.borrow_mut().apply(&profiles).unwrap();
            unsafe {
                let instance = GetModuleHandleW(null());
                let class = wide("BucketMount.ConfigurationWindow");
                register_class(instance, &class, Some(form_proc)).unwrap();
                InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                    dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
                    dwICC: ICC_STANDARD_CLASSES,
                });
                // An offscreen tool window renders native controls without
                // opening a visible application or adding a taskbar entry.
                let form = CreateWindowExW(
                    WS_EX_CONTROLPARENT | WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    wide("Editor test").as_ptr(),
                    WS_POPUP,
                    -10000,
                    -10000,
                    980,
                    694,
                    null_mut(),
                    null_mut(),
                    instance,
                    (&*ui as *const UiState).cast(),
                );
                assert!(!form.is_null());
                let _window = TestWindow(form);
                ui.form.set(form);
                ShowWindow(form, SW_SHOWNOACTIVATE);
                refresh_status(&ui);
                assert_eq!(
                    SendMessageW(GetDlgItem(form, PROFILE), CB_GETCURSEL, 0, 0),
                    0
                );
                assert_eq!(SendMessageW(GetDlgItem(form, SECURE), BM_GETCHECK, 0, 0), 0);
                SendMessageW(GetDlgItem(form, SECURE), BM_SETCHECK, 1, 0);
                credential_mode(form, &ui);
                assert_eq!(field(form, ACCESS_LABEL), "Access key");
                assert_eq!(field(form, SECRET_LABEL), "Secret key");
                check_layout_and_preview(form, &ui);
                assert_ne!(
                    SendMessageW(GetDlgItem(form, SECRET_ENV), EM_GETPASSWORDCHAR, 0, 0),
                    0
                );
                SetWindowTextW(
                    GetDlgItem(form, ACCESS_ENV),
                    wide("editor-test-access").as_ptr(),
                );
                SetWindowTextW(
                    GetDlgItem(form, SECRET_ENV),
                    wide("editor-test-secret").as_ptr(),
                );
                SetWindowTextW(
                    GetDlgItem(form, SESSION_ENV),
                    wide("editor-test-token").as_ptr(),
                );
                for section in [2, 0, 1] {
                    show_section(form, &ui, section);
                }
                assert_eq!(secret_field(form, SECRET_ENV), "editor-test-secret");
                assert!(ui.dirty.get(), "changing pages must preserve unsaved edits");
                save_form(form, &ui).unwrap();
                let saved = Profiles::load(&config_path).unwrap();
                let protected = saved.connections[0]
                    .config
                    .protected_credentials
                    .clone()
                    .unwrap();
                let loaded = Secrets::load(&saved.connections[0].config).unwrap();
                assert_eq!(loaded.secret_key, "editor-test-secret");
                let contents = std::fs::read_to_string(&config_path).unwrap();
                for secret in [
                    "editor-test-access",
                    "editor-test-secret",
                    "editor-test-token",
                ] {
                    assert!(!contents.contains(secret));
                }
                assert!(secret_field(form, SECRET_ENV).is_empty());
                SetWindowTextW(GetDlgItem(form, NAME), wide("Renamed").as_ptr());
                save_form(form, &ui).unwrap();
                let saved = Profiles::load(&config_path).unwrap();
                assert_eq!(saved.connections[0].name, "Renamed");
                assert_eq!(
                    saved.connections[0].config.protected_credentials.as_ref(),
                    Some(&protected)
                );
                SetWindowTextW(
                    GetDlgItem(form, ACCESS_ENV),
                    wide("replacement-access").as_ptr(),
                );
                SetWindowTextW(
                    GetDlgItem(form, SECRET_ENV),
                    wide("replacement-secret").as_ptr(),
                );
                save_form(form, &ui).unwrap();
                assert!(
                    Secrets::load(&Profiles::load(&config_path).unwrap().connections[0].config)
                        .unwrap()
                        .session_token
                        .is_none()
                );
                select_profile(form, &ui, NEW_PROFILE);
                SetWindowTextW(GetDlgItem(form, NAME), wide("Second").as_ptr());
                SetWindowTextW(GetDlgItem(form, BUCKET), wide("second-bucket").as_ptr());
                SetWindowTextW(
                    GetDlgItem(form, MOUNT_DIR),
                    wide(directory.path().join("mount-second")).as_ptr(),
                );
                SetWindowTextW(
                    GetDlgItem(form, STATE_DIR),
                    wide(directory.path().join("state-second")).as_ptr(),
                );
                SetWindowTextW(GetDlgItem(form, ACCESS_ENV), wide("second-access").as_ptr());
                SetWindowTextW(GetDlgItem(form, SECRET_ENV), wide("second-secret").as_ptr());
                SendMessageW(GetDlgItem(form, ENABLED), BM_SETCHECK, 0, 0);
                save_form(form, &ui).unwrap();
                let saved = Profiles::load(&config_path).unwrap();
                assert_eq!(saved.connections.len(), 2);
                assert_ne!(
                    saved.connections[0].config.device_id,
                    saved.connections[1].config.device_id
                );
                assert_eq!(
                    Secrets::load(&saved.connections[1].config)
                        .unwrap()
                        .secret_key,
                    "second-secret"
                );
                let before = std::fs::read(&config_path).unwrap();
                SetWindowTextW(GetDlgItem(form, CACHE_FILES), wide("invalid").as_ptr());
                assert!(save_form(form, &ui).is_err());
                assert_eq!(std::fs::read(&config_path).unwrap(), before);
            }
        }

        #[test]
        #[ignore = "requires an interactive Windows desktop and embedded application resources"]
        fn live_native_icons_and_tray_states() {
            unsafe {
                let instance = GetModuleHandleW(null());
                let ui = Box::new(UiState {
                    config_path: PathBuf::new(),
                    config: RefCell::new(Config::default()),
                    profiles: RefCell::new(Profiles::default()),
                    workers: RefCell::new(Manager::new().unwrap()),
                    editing: Cell::new(0),
                    draft_name: RefCell::new("My bucket".into()),
                    draft_enabled: Cell::new(true),
                    dirty: Cell::new(false),
                    control: Arc::new(Control::default()),
                    form: Cell::new(null_mut()),
                    taskbar_created: 0,
                    icons: RefCell::new(IconCache::default()),
                    light_taskbar: Cell::new(false),
                    design: RefCell::new(None),
                    section: Cell::new(0),
                    laying_out: Cell::new(false),
                });
                for size in [16, 20, 24, 32, 40, 48] {
                    let mut rendered = BTreeSet::new();
                    for resource in [201, 202, 203, 204, 211, 212, 213, 214] {
                        let handle = ui.icons.borrow_mut().get(resource, size).unwrap();
                        assert!(
                            rendered.insert(pixels(handle, size)),
                            "distinct tray artwork"
                        );
                        assert_eq!(ui.icons.borrow_mut().get(resource, size).unwrap(), handle);
                    }
                }
                for size in [16, 32, 48, 256] {
                    let icon = ui.icons.borrow_mut().get(APP_ICON, size).unwrap();
                    assert_ne!(icon, LoadIconW(null_mut(), IDI_APPLICATION));
                    pixels(icon, size);
                }
                let class = wide("BucketMount.IconResourceSmokeTest");
                register_class(instance, &class, Some(DefWindowProcW)).unwrap();
                register_class(
                    instance,
                    &wide("BucketMount.ConfigurationWindow"),
                    Some(form_proc),
                )
                .unwrap();
                let hwnd = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    wide("Bucket Mount icon smoke test").as_ptr(),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                );
                assert!(!hwnd.is_null());
                let window = TestWindow(hwnd);
                set_window_icons(window.0, &ui);
                assert_ne!(SendMessageW(hwnd, WM_GETICON, ICON_SMALL as usize, 0), 0);
                assert_ne!(SendMessageW(hwnd, WM_GETICON, ICON_BIG as usize, 0), 0);
                assert!(update_icon(hwnd, &ui, NIM_ADD));
                for light in [false, true] {
                    ui.light_taskbar.set(light);
                    for (state, expected) in [
                        (SyncState::Idle, 201),
                        (SyncState::Syncing, 202),
                        (SyncState::Paused, 203),
                        (SyncState::Error, 204),
                    ] {
                        ui.control.set_status(state, format!("{state:?}"));
                        assert_eq!(
                            tray_resource(light, ui.control.sync_state()),
                            expected + if light { 10 } else { 0 }
                        );
                        assert!(update_icon(hwnd, &ui, NIM_MODIFY));
                    }
                }
                ui.control.set_status(SyncState::Error, "Offline".into());
                ui.control.paused.store(true, Ordering::Release);
                assert_eq!(ui.control.sync_state(), SyncState::Paused);
                ui.control.paused.store(false, Ordering::Release);
                assert_eq!(ui.control.sync_state(), SyncState::Error);
                show_config(hwnd, &ui);
                let form = ui.form.get();
                assert!(!form.is_null());
                assert_ne!(SendMessageW(form, WM_GETICON, ICON_SMALL as usize, 0), 0);
                assert_ne!(SendMessageW(form, WM_GETICON, ICON_BIG as usize, 0), 0);
                assert_ne!(
                    GetWindowLongPtrW(form, GWL_EXSTYLE) & WS_EX_APPWINDOW as isize,
                    0
                );
                DestroyWindow(form);
            }
        }
    }
}
