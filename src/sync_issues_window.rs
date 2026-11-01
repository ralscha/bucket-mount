//! Modeless, keyboard-accessible recovery window for one saved connection.
use super::{Control, SyncState};
use crate::sync_issue::SyncIssue;
use anyhow::{Context, Result, ensure};
use std::{
    cell::{Cell, RefCell},
    mem::zeroed,
    path::Path,
    ptr::{null, null_mut},
    sync::{Arc, atomic::Ordering},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        CLEARTYPE_QUALITY, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateFontW, DEFAULT_CHARSET,
        DeleteObject, FillRect, GetSysColor, GetSysColorBrush, HFONT, SetBkMode, SetTextColor,
        TRANSPARENT,
    },
    System::{
        Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        LibraryLoader::GetModuleHandleW,
    },
    UI::{
        Controls::{EM_SETLIMITTEXT, EM_SETSEL},
        HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
        Input::KeyboardAndMouse::{EnableWindow, SetFocus},
        Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems, ShellExecuteW},
        WindowsAndMessaging::*,
    },
};

const TITLE: i32 = 301;
const ITEMS_LABEL: i32 = 302;
const ITEMS: i32 = 303;
const PATH_LABEL: i32 = 304;
const PATH: i32 = 305;
const GUIDANCE: i32 = 306;
const RETRY_STATUS: i32 = 307;
const TECH_TOGGLE: i32 = 308;
const TECH: i32 = 309;
const RETRY: i32 = 310;
const EXPLORE: i32 = 311;
const COPY: i32 = 312;
const CLOSE: i32 = 2;
const COPY_BUFFER: i32 = 313;

#[derive(Default)]
pub(super) struct Window {
    pub hwnd: Cell<HWND>,
    pub connection_id: RefCell<String>,
    control: RefCell<Option<Arc<Control>>>,
    items: RefCell<Vec<SyncIssue>>,
    selected: Cell<usize>,
    expanded: Cell<bool>,
    font: Cell<HFONT>,
    title_font: Cell<HFONT>,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

impl Window {
    // The owner keeps this object at a stable address until its windows are destroyed.
    pub unsafe fn show(&self, owner: HWND, id: &str) -> Result<()> {
        unsafe {
            if *self.connection_id.borrow() != id {
                self.items.borrow_mut().clear();
                self.selected.set(0);
                self.connection_id.replace(id.into());
                if !self.hwnd.get().is_null() {
                    SendMessageW(GetDlgItem(self.hwnd.get(), ITEMS), CB_RESETCONTENT, 0, 0);
                }
            }
            if self.hwnd.get().is_null() {
                let class_name = wide("BucketMount.SyncIssues");
                let instance = GetModuleHandleW(null());
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: instance,
                    lpszClassName: class_name.as_ptr(),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    hbrBackground: (COLOR_WINDOW + 1) as usize as _,
                    ..zeroed()
                };
                if RegisterClassW(&class) == 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(1410) {
                        return Err(error.into());
                    }
                }
                let dpi = GetDpiForWindow(owner).max(96);
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 720 * dpi as i32 / 96,
                    bottom: 430 * dpi as i32 / 96,
                };
                AdjustWindowRectExForDpi(
                    &mut rect,
                    WS_OVERLAPPEDWINDOW,
                    0,
                    WS_EX_CONTROLPARENT,
                    dpi,
                );
                let hwnd = CreateWindowExW(
                    WS_EX_CONTROLPARENT,
                    class_name.as_ptr(),
                    wide("Sync issues").as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    owner,
                    null_mut(),
                    instance,
                    (self as *const Self).cast(),
                );
                ensure!(
                    !hwnd.is_null(),
                    "Could not open Sync issues: {}",
                    std::io::Error::last_os_error()
                );
                // Reuse the owner's application icon.
                for kind in [ICON_SMALL, ICON_BIG] {
                    SendMessageW(
                        hwnd,
                        WM_SETICON,
                        kind as usize,
                        SendMessageW(owner, WM_GETICON, kind as usize, 0),
                    );
                }
            }
            ShowWindow(self.hwnd.get(), SW_RESTORE);
            SetForegroundWindow(self.hwnd.get());
            Ok(())
        }
    }

    pub unsafe fn refresh(&self, name: &str, control: Option<Arc<Control>>, interval: u64) {
        unsafe {
            let hwnd = self.hwnd.get();
            if hwnd.is_null() {
                return;
            }
            set_text(hwnd, 0, &format!("Sync issues — {name}"));
            let state = control.as_ref().map(|c| c.sync_state());
            let mut issues = control.as_ref().map(|c| c.issues()).unwrap_or_default();
            let sync_report = !issues.is_empty();
            if issues.is_empty() && state == Some(SyncState::Error) {
                issues.push(SyncIssue::from_error(
                    &anyhow::anyhow!(control.as_ref().unwrap().status_text()),
                    None,
                    "Connect to bucket",
                ));
                // Connection errors do not have a sync-pass timestamp.
                if let Some(old) = self.items.borrow().first() {
                    issues[0].first_seen = old.first_seen;
                }
            }
            if *self.items.borrow() != issues {
                let selected = self.items.borrow().get(self.selected.get()).cloned();
                self.selected.set(
                    selected
                        .and_then(|old| issues.iter().position(|i| i.same_item(&old)))
                        .unwrap_or(0),
                );
                self.items.replace(issues);
                let list = GetDlgItem(hwnd, ITEMS);
                SendMessageW(list, CB_RESETCONTENT, 0, 0);
                for issue in self.items.borrow().iter() {
                    let text = format!(
                        "{} — {}",
                        if issue.busy {
                            "In use"
                        } else {
                            "Needs attention"
                        },
                        issue
                            .path
                            .as_ref()
                            .map_or_else(|| "Connection".into(), |p| p.display().to_string())
                    );
                    SendMessageW(list, CB_ADDSTRING, 0, wide(&text).as_ptr() as isize);
                }
                SendMessageW(list, CB_SETCURSEL, self.selected.get(), 0);
            }
            let count = self.items.borrow().len();
            let retry = control
                .as_ref()
                .is_some_and(|c| c.retry_available.load(Ordering::Acquire));
            let pending = control
                .as_ref()
                .is_some_and(|c| c.sync_requested.load(Ordering::Acquire));
            set_text(
                hwnd,
                TITLE,
                match state {
                    Some(SyncState::Waiting) => "Waiting for access",
                    Some(SyncState::Error) => "Needs attention",
                    Some(SyncState::Syncing) => "Synchronizing…",
                    Some(SyncState::Paused) => "Synchronization paused",
                    Some(SyncState::Idle) if count == 0 => "No outstanding sync issues",
                    _ => "Connection is not running",
                },
            );
            set_text(hwnd, ITEMS_LABEL, &format!("Affected items ({count})"));
            set_text(
                hwnd,
                RETRY_STATUS,
                &if !retry {
                    "Automatic retries are unavailable. Enable the connection or use Save and apply in Connections.".into()
                } else if state == Some(SyncState::Paused) {
                    "Automatic retries are paused. Resume and retry when you are ready.".into()
                } else if pending {
                    "Retry requested. Waiting for the current work to finish…".into()
                } else if state == Some(SyncState::Syncing) {
                    "Synchronization is running. This window updates automatically.".into()
                } else if count > 0 {
                    if sync_report {
                        format!("Retrying automatically every {interval} seconds.")
                    } else {
                        "Retrying the connection automatically.".into()
                    }
                } else {
                    "This window updates automatically.".into()
                },
            );
            set_text(
                hwnd,
                RETRY,
                if state == Some(SyncState::Paused) {
                    "Resume and retry"
                } else {
                    "Retry now"
                },
            );
            EnableWindow(GetDlgItem(hwnd, RETRY), i32::from(retry && !pending));
            EnableWindow(GetDlgItem(hwnd, ITEMS), i32::from(count > 0));
            self.control.replace(control);
            self.render_item();
            if count == 0
                && let Some(control) = self.control.borrow().as_ref()
            {
                set_text(hwnd, GUIDANCE, &control.status_text());
            }
        }
    }

    unsafe fn render_item(&self) {
        unsafe {
            let hwnd = self.hwnd.get();
            let item = self.items.borrow().get(self.selected.get()).cloned();
            set_text(
                hwnd,
                PATH,
                &item
                    .as_ref()
                    .and_then(|i| i.path.as_ref())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
            set_text(
                hwnd,
                GUIDANCE,
                item.as_ref()
                    .map_or("There are no outstanding items to resolve.", |i| {
                        i.guidance()
                    }),
            );
            set_text(
                hwnd,
                TECH,
                &item
                    .as_ref()
                    .map(|i| format!("Operation: {}\r\n\r\n{}", i.operation, i.details))
                    .unwrap_or_default(),
            );
            EnableWindow(
                GetDlgItem(hwnd, EXPLORE),
                i32::from(item.as_ref().is_some_and(|i| i.path.is_some())),
            );
            EnableWindow(GetDlgItem(hwnd, COPY), i32::from(item.is_some()));
            EnableWindow(GetDlgItem(hwnd, TECH_TOGGLE), i32::from(item.is_some()));
        }
    }

    unsafe fn create_controls(&self) {
        unsafe {
            let hwnd = self.hwnd.get();
            for (id, class, label, style, ex) in [
                (TITLE, "STATIC", "Sync issues", 0, 0),
                (ITEMS_LABEL, "STATIC", "Affected items", 0, 0),
                (
                    ITEMS,
                    "COMBOBOX",
                    "",
                    CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                    0,
                ),
                (PATH_LABEL, "STATIC", "File or folder", 0, 0),
                (
                    PATH,
                    "EDIT",
                    "",
                    ES_READONLY as u32 | ES_MULTILINE as u32 | WS_VSCROLL | WS_TABSTOP,
                    WS_EX_CLIENTEDGE,
                ),
                (GUIDANCE, "STATIC", "", 0, 0),
                (RETRY_STATUS, "STATIC", "", 0, 0),
                (
                    TECH_TOGGLE,
                    "BUTTON",
                    "Show technical details",
                    WS_TABSTOP | BS_PUSHBUTTON as u32,
                    0,
                ),
                (
                    TECH,
                    "EDIT",
                    "",
                    ES_READONLY as u32
                        | ES_MULTILINE as u32
                        | ES_AUTOVSCROLL as u32
                        | WS_VSCROLL
                        | WS_TABSTOP,
                    WS_EX_CLIENTEDGE,
                ),
                (
                    RETRY,
                    "BUTTON",
                    "Retry now",
                    WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
                    0,
                ),
                (
                    EXPLORE,
                    "BUTTON",
                    "Show in Explorer",
                    WS_TABSTOP | BS_PUSHBUTTON as u32,
                    0,
                ),
                (
                    COPY,
                    "BUTTON",
                    "Copy details",
                    WS_TABSTOP | BS_PUSHBUTTON as u32,
                    0,
                ),
                (
                    CLOSE,
                    "BUTTON",
                    "Close",
                    WS_TABSTOP | BS_PUSHBUTTON as u32,
                    0,
                ),
            ] {
                CreateWindowExW(
                    ex,
                    wide(class).as_ptr(),
                    wide(label).as_ptr(),
                    WS_CHILD | WS_VISIBLE | style,
                    0,
                    0,
                    0,
                    0,
                    hwnd,
                    id as usize as _,
                    GetModuleHandleW(null()),
                    null(),
                );
            }
            // A hidden read-only native edit supplies Unicode clipboard ownership and cleanup.
            CreateWindowExW(
                0,
                wide("EDIT").as_ptr(),
                null(),
                WS_CHILD | ES_MULTILINE as u32 | ES_READONLY as u32,
                0,
                0,
                0,
                0,
                hwnd,
                COPY_BUFFER as usize as _,
                GetModuleHandleW(null()),
                null(),
            );
            SendMessageW(
                GetDlgItem(hwnd, COPY_BUFFER),
                EM_SETLIMITTEXT,
                0x7ffffffe,
                0,
            );
            SendMessageW(GetDlgItem(hwnd, TECH), EM_SETLIMITTEXT, 0x7ffffffe, 0);
            self.fonts();
            self.layout();
        }
    }

    unsafe fn fonts(&self) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd.get()).max(96) as i32;
            let make = |height: i32, weight: i32| {
                CreateFontW(
                    -height * dpi / 96,
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    wide("Segoe UI").as_ptr(),
                )
            };
            let old_font = self.font.replace(make(14, 400));
            let old_title = self.title_font.replace(make(20, 600));
            for id in TITLE..=COPY {
                SendMessageW(
                    GetDlgItem(self.hwnd.get(), id),
                    WM_SETFONT,
                    if id == TITLE {
                        self.title_font.get()
                    } else {
                        self.font.get()
                    } as usize,
                    1,
                );
            }
            SendMessageW(
                GetDlgItem(self.hwnd.get(), CLOSE),
                WM_SETFONT,
                self.font.get() as usize,
                1,
            );
            DeleteObject(old_font);
            DeleteObject(old_title);
        }
    }

    unsafe fn layout(&self) {
        unsafe {
            let hwnd = self.hwnd.get();
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect);
            let w = rect.right * 96 / dpi;
            let h = rect.bottom * 96 / dpi;
            let place = |id, x, y, width, height| {
                MoveWindow(
                    GetDlgItem(hwnd, id),
                    x * dpi / 96,
                    y * dpi / 96,
                    width * dpi / 96,
                    height * dpi / 96,
                    1,
                );
            };
            place(TITLE, 24, 20, w - 48, 32);
            place(ITEMS_LABEL, 24, 64, w - 48, 20);
            place(ITEMS, 24, 86, w - 48, 220);
            place(PATH_LABEL, 24, 124, w - 48, 20);
            place(PATH, 24, 146, w - 48, 52);
            place(GUIDANCE, 24, 212, w - 48, 58);
            place(RETRY_STATUS, 24, 276, w - 48, 40);
            place(TECH_TOGGLE, 24, 324, 186, 28);
            ShowWindow(
                GetDlgItem(hwnd, TECH),
                if self.expanded.get() {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
            place(TECH, 24, 364, w - 48, (h - 430).max(40));
            place(RETRY, 24, h - 52, 142, 32);
            place(EXPLORE, 176, h - 52, 140, 32);
            place(COPY, 326, h - 52, 116, 32);
            place(CLOSE, w - 116, h - 52, 92, 32);
        }
    }
}

unsafe fn set_text(hwnd: HWND, id: i32, text: &str) {
    unsafe {
        let control = if id == 0 { hwnd } else { GetDlgItem(hwnd, id) };
        let mut old = vec![0; GetWindowTextLengthW(control).max(0) as usize + 1];
        GetWindowTextW(control, old.as_mut_ptr(), old.len() as i32);
        if old != wide(text) {
            SetWindowTextW(control, wide(text).as_ptr());
        }
    }
}

fn explorer_target(path: &Path) -> Option<&Path> {
    // A remote deletion or rename may have removed the item. Reveal its nearest
    // existing parent instead. Never interpolate a path into a command line.
    path.ancestors().find(|p| p.exists())
}

unsafe fn reveal(hwnd: HWND, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    unsafe {
        let initialized = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) >= 0;
        struct ComGuard(bool);
        impl Drop for ComGuard {
            fn drop(&mut self) {
                if self.0 {
                    unsafe { CoUninitialize() };
                }
            }
        }
        let _com = ComGuard(initialized);
        let target =
            explorer_target(path).context("The file and its parent folders are unavailable")?;
        let path: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        let item = ILCreateFromPathW(path.as_ptr());
        if !item.is_null() {
            let result = SHOpenFolderAndSelectItems(item, 0, null(), 0);
            ILFree(item);
            if result >= 0 {
                return Ok(());
            }
        }
        let folder = if target.is_dir() {
            target
        } else {
            target.parent().context("No containing folder")?
        };
        let folder: Vec<u16> = folder.as_os_str().encode_wide().chain(Some(0)).collect();
        ensure!(
            ShellExecuteW(
                hwnd,
                wide("open").as_ptr(),
                folder.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL
            ) as isize
                > 32,
            "Could not open the containing folder in Explorer"
        );
        Ok(())
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let Some(window) = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Window).as_ref()
        else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match msg {
            WM_CREATE => {
                window.hwnd.set(hwnd);
                window.create_controls();
                0
            }
            WM_SIZE => {
                window.layout();
                0
            }
            WM_PRINTCLIENT => {
                let mut rect: RECT = zeroed();
                GetClientRect(hwnd, &mut rect);
                FillRect(wp as _, &rect, GetSysColorBrush(COLOR_WINDOW));
                0
            }
            WM_CTLCOLORSTATIC => {
                SetBkMode(wp as _, TRANSPARENT as i32);
                SetTextColor(wp as _, GetSysColor(COLOR_WINDOWTEXT));
                GetSysColorBrush(COLOR_WINDOW) as isize
            }
            WM_DPICHANGED => {
                let rect = &*(lp as *const RECT);
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                window.fonts();
                window.layout();
                0
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 620 * dpi as i32 / 96,
                    bottom: (if window.expanded.get() { 510 } else { 420 }) * dpi as i32 / 96,
                };
                AdjustWindowRectExForDpi(
                    &mut rect,
                    WS_OVERLAPPEDWINDOW,
                    0,
                    WS_EX_CONTROLPARENT,
                    dpi,
                );
                let limits = &mut *(lp as *mut MINMAXINFO);
                limits.ptMinTrackSize.x = rect.right - rect.left;
                limits.ptMinTrackSize.y = rect.bottom - rect.top;
                0
            }
            DM_GETDEFID => ((DC_HASDEFID as usize) << 16 | RETRY as usize) as isize,
            WM_COMMAND => {
                let id = (wp & 0xffff) as i32;
                let notification = (wp >> 16) as u32;
                match id {
                    ITEMS if notification == CBN_SELCHANGE => {
                        window.selected.set(SendMessageW(
                            GetDlgItem(hwnd, ITEMS),
                            CB_GETCURSEL,
                            0,
                            0,
                        ) as usize);
                        window.render_item();
                    }
                    RETRY => {
                        if let Some(control) = window.control.borrow().as_ref()
                            && control.retry_available.load(Ordering::Acquire)
                        {
                            control.request_sync();
                            set_text(
                                hwnd,
                                RETRY_STATUS,
                                "Retry requested. This window updates automatically.",
                            );
                            EnableWindow(GetDlgItem(hwnd, RETRY), 0);
                        }
                    }
                    EXPLORE => {
                        let path = window
                            .items
                            .borrow()
                            .get(window.selected.get())
                            .and_then(|i| i.path.clone());
                        if let Some(path) = path
                            && let Err(error) = reveal(hwnd, &path)
                        {
                            MessageBoxW(
                                hwnd,
                                wide(&format!("{error:#}")).as_ptr(),
                                wide("Show in Explorer").as_ptr(),
                                MB_OK | MB_ICONERROR,
                            );
                        }
                    }
                    COPY => {
                        if let Some(item) = window.items.borrow().get(window.selected.get()) {
                            let buffer = GetDlgItem(hwnd, COPY_BUFFER);
                            SetWindowTextW(buffer, wide(&item.copy_text()).as_ptr());
                            SendMessageW(buffer, EM_SETSEL, 0, -1);
                            SendMessageW(buffer, WM_COPY, 0, 0);
                        }
                    }
                    TECH_TOGGLE => {
                        let expanded = !window.expanded.get();
                        window.expanded.set(expanded);
                        set_text(
                            hwnd,
                            TECH_TOGGLE,
                            if expanded {
                                "Hide technical details"
                            } else {
                                "Show technical details"
                            },
                        );
                        let mut rect: RECT = zeroed();
                        GetWindowRect(hwnd, &mut rect);
                        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                        let extra = 150 * dpi / 96;
                        let mut client: RECT = zeroed();
                        GetClientRect(hwnd, &mut client);
                        let chrome = rect.bottom - rect.top - client.bottom;
                        let height = (rect.bottom - rect.top
                            + if expanded { extra } else { -extra })
                        .max((if expanded { 510 } else { 420 }) * dpi / 96 + chrome);
                        SetWindowPos(
                            hwnd,
                            null_mut(),
                            0,
                            0,
                            rect.right - rect.left,
                            height,
                            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        window.layout();
                    }
                    CLOSE => {
                        DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                0
            }
            WM_SETFOCUS => {
                SetFocus(GetDlgItem(hwnd, ITEMS));
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                window.hwnd.set(null_mut());
                window.expanded.set(false);
                window.items.borrow_mut().clear();
                window.control.borrow_mut().take();
                0
            }
            WM_NCDESTROY => {
                DeleteObject(window.font.replace(null_mut()));
                DeleteObject(window.title_font.replace(null_mut()));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_issue::WindowsFailure;
    use windows_sys::Win32::{
        Graphics::Gdi::MapWindowPoints, UI::Input::KeyboardAndMouse::IsWindowEnabled,
    };

    unsafe fn text(hwnd: HWND, id: i32) -> String {
        unsafe {
            let field = if id == 0 { hwnd } else { GetDlgItem(hwnd, id) };
            let mut buffer = vec![0; GetWindowTextLengthW(field) as usize + 1];
            let len = GetWindowTextW(field, buffer.as_mut_ptr(), buffer.len() as i32);
            String::from_utf16_lossy(&buffer[..len as usize])
        }
    }

    #[test]
    #[ignore = "requires native Windows controls"]
    fn native_issues_support_retry_selection_resolution_and_small_windows() {
        unsafe {
            let window = Box::new(Window::default());
            let class = wide("BucketMount.SyncIssues.Test");
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                hbrBackground: (COLOR_WINDOW + 1) as usize as _,
                ..zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT,
                class.as_ptr(),
                wide("Sync issues test").as_ptr(),
                WS_POPUP,
                -10000,
                -10000,
                720,
                430,
                null_mut(),
                null_mut(),
                instance,
                (&*window as *const Window).cast(),
            );
            assert!(!hwnd.is_null());
            struct Destroy(HWND);
            impl Drop for Destroy {
                fn drop(&mut self) {
                    unsafe {
                        DestroyWindow(self.0);
                    }
                }
            }
            let _destroy = Destroy(hwnd);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let control = Arc::new(Control::default());
            control.retry_available.store(true, Ordering::Release);
            let busy = SyncIssue::from_error(
                &anyhow::Error::new(WindowsFailure {
                    action: "lock cloud file".into(),
                    code: 0x80070020,
                }),
                Some(r"C:\Users\you\BucketMount\Reports\Quarterly report.docx".into()),
                "Refresh file",
            );
            let other = SyncIssue::from_error(
                &anyhow::Error::new(std::io::Error::from_raw_os_error(5)),
                Some(r"C:\Users\you\BucketMount\Restricted\notes.txt".into()),
                "Upload file",
            );
            control.finish_sync(String::new(), vec![busy.clone(), other]);
            window.refresh("Personal storage", Some(control.clone()), 15);
            assert!(text(hwnd, PATH).ends_with("Quarterly report.docx"));
            assert!(text(hwnd, GUIDANCE).contains("Save your work"));
            assert!(text(hwnd, TECH).contains("0x80070020"));
            assert_eq!(SendMessageW(GetDlgItem(hwnd, ITEMS), CB_GETCOUNT, 0, 0), 2);
            assert_ne!(IsWindowEnabled(GetDlgItem(hwnd, EXPLORE)), 0);
            SendMessageW(GetDlgItem(hwnd, ITEMS), CB_SETCURSEL, 1, 0);
            SendMessageW(
                hwnd,
                WM_COMMAND,
                ITEMS as usize | ((CBN_SELCHANGE as usize) << 16),
                0,
            );
            assert!(text(hwnd, PATH).ends_with("notes.txt"));
            control.finish_sync(String::new(), vec![busy]);
            window.refresh("Personal storage", Some(control.clone()), 15);
            assert!(text(hwnd, PATH).ends_with("Quarterly report.docx"));
            assert_eq!(text(hwnd, TITLE), "Waiting for access");
            assert_eq!(SendMessageW(GetDlgItem(hwnd, ITEMS), CB_GETCURSEL, 0, 0), 0);
            SendMessageW(hwnd, WM_COMMAND, TECH_TOGGLE as usize, 0);
            assert!(window.expanded.get());
            for (width, height, name) in [(720, 580, "normal"), (620, 510, "small")] {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                for id in (TITLE..=COPY).chain(std::iter::once(CLOSE)) {
                    if id == ITEMS {
                        continue;
                    } // Includes the drop-down popup height.
                    let mut bounds: RECT = zeroed();
                    GetWindowRect(GetDlgItem(hwnd, id), &mut bounds);
                    MapWindowPoints(null_mut(), hwnd, (&mut bounds as *mut RECT).cast(), 2);
                    assert!(
                        bounds.left >= 0
                            && bounds.top >= 0
                            && bounds.right <= width
                            && bounds.bottom <= height,
                        "control {id} is outside {name}"
                    );
                }
                if let Some(directory) = std::env::var_os("BUCKET_MOUNT_UI_PREVIEW_DIR") {
                    let directory = std::path::PathBuf::from(directory);
                    std::fs::create_dir_all(&directory).unwrap();
                    super::super::native::tests::capture_preview(
                        hwnd,
                        &directory.join(format!("sync-issues-{name}.bmp")),
                    );
                }
            }
            SendMessageW(hwnd, WM_COMMAND, TECH_TOGGLE as usize, 0);
            let mut collapsed: RECT = zeroed();
            GetClientRect(hwnd, &mut collapsed);
            assert!(
                collapsed.bottom >= 420,
                "collapsing details preserves the minimum height"
            );
            control.paused.store(true, Ordering::Release);
            window.refresh("Personal storage", Some(control.clone()), 15);
            assert_eq!(text(hwnd, RETRY), "Resume and retry");
            SendMessageW(hwnd, WM_COMMAND, RETRY as usize, 0);
            assert!(!control.paused.load(Ordering::Acquire));
            assert!(control.sync_requested.load(Ordering::Acquire));
            control.sync_requested.store(false, Ordering::Release);
            control.finish_sync("Up to date".into(), Vec::new());
            window.refresh("Personal storage", Some(control.clone()), 15);
            assert_eq!(text(hwnd, TITLE), "No outstanding sync issues");
            assert!(text(hwnd, PATH).is_empty());
            assert_eq!(IsWindowEnabled(GetDlgItem(hwnd, EXPLORE)), 0);
            assert_eq!(IsWindowEnabled(GetDlgItem(hwnd, COPY)), 0);
            control.retry_available.store(false, Ordering::Release);
            control.set_status(SyncState::Error, "Stopped: invalid configuration".into());
            window.refresh("Personal storage", Some(control), 15);
            assert!(text(hwnd, TECH).contains("invalid configuration"));
            assert!(text(hwnd, RETRY_STATUS).contains("Save and apply"));
            assert_eq!(IsWindowEnabled(GetDlgItem(hwnd, RETRY)), 0);
            window.refresh("Removed connection", None, 15);
            assert_eq!(text(hwnd, TITLE), "Connection is not running");
        }
    }

    #[test]
    fn missing_items_reveal_the_nearest_existing_parent() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("removed").join("file.txt");
        assert_eq!(explorer_target(&missing), Some(directory.path()));
    }
}
