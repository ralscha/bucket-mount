//! Native settings presentation. All fields remain standard, accessible Win32 controls.
use std::{
    cell::Cell,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::*,
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        Controls::{
            COMBOBOXINFO, DRAWITEMSTRUCT, EM_SETCUEBANNER, EM_SETMARGINS, GetComboBoxInfo,
            ODS_COMBOBOXEDIT, ODS_DISABLED, ODS_FOCUS, ODS_NOACCEL, ODS_NOFOCUSRECT, ODS_SELECTED,
            SetScrollInfo, WM_MOUSELEAVE,
        },
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{GetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

pub const WIDTH: i32 = 1020;
pub const HEIGHT: i32 = 660;
pub const MIN_WIDTH: i32 = 740;
pub const MIN_HEIGHT: i32 = 480;
pub const COMBO_ITEM_HEIGHT: i32 = 28;
pub const FOCUS_CHANGED: u32 = WM_APP + 52;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[derive(Clone, Copy, Default)]
pub enum Anchor {
    #[default]
    Left,
    Right,
    Stretch,
    FirstColumn,
    SecondColumn,
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub rect: (i32, i32, i32, i32),
    pub anchor: Anchor,
    pub bottom: bool,
}

impl Layout {
    pub fn new(rect: (i32, i32, i32, i32), anchor: Anchor) -> Self {
        Self {
            rect,
            anchor,
            bottom: false,
        }
    }
    pub fn bottom(mut self) -> Self {
        self.bottom = true;
        self
    }
    fn resolve(self, width: i32, height: i32, dpi: u32) -> RECT {
        let (mut x, mut y, mut w, h) = self.rect;
        let extra = width - WIDTH;
        match self.anchor {
            Anchor::Left => {}
            Anchor::Right => x += extra,
            Anchor::Stretch => w += extra,
            Anchor::FirstColumn => w += extra / 2,
            Anchor::SecondColumn => {
                x += extra / 2;
                w += extra - extra / 2;
            }
        }
        if self.bottom {
            y += height - HEIGHT;
        }
        let scale = |v| v * dpi as i32 / 96;
        RECT {
            left: scale(x),
            top: scale(y),
            right: scale(x + w),
            bottom: scale(y + h),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Font {
    Body,
    Caption,
    Strong,
    Title,
    Section,
}

#[derive(Clone, Copy)]
pub enum Button {
    Primary,
    Secondary,
    Remove,
    Navigation(usize),
    Link,
}

#[derive(Clone, Copy)]
pub enum Kind {
    Text(Font),
    Edit,
    Check,
    Combo,
    Button(Button),
}

pub struct Item {
    pub hwnd: HWND,
    pub layout: Layout,
    pub page: Option<usize>,
    pub kind: Kind,
}

struct FontHandle(HFONT);
impl Drop for FontHandle {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}
struct Brush(HBRUSH);
impl Drop for Brush {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}

pub struct Design {
    pub items: Vec<Item>,
    fonts: Vec<FontHandle>,
    background: Brush,
    surface: Brush,
    sidebar: Brush,
    pub dpi: u32,
    page: usize,
    pub scroll: i32,
}

const BG: u32 = 0xf9f9f9;
const WHITE: u32 = 0xffffff;
const SIDE: u32 = 0xf3f3f3;
const TEXT: u32 = 0x242424;
const MUTED: u32 = 0x646464;
// COLORREF stores red in the low byte.
const ACCENT: u32 = 0xb35c00;
const BORDER: u32 = 0xe4e4e4;

thread_local! { static HIGH_CONTRAST: Cell<bool> = const { Cell::new(false) }; }

fn themed(color: u32, foreground: bool) -> u32 {
    if !HIGH_CONTRAST.get() {
        return color;
    }
    let system = match (color, foreground) {
        (WHITE, true) => COLOR_HIGHLIGHTTEXT,
        (WHITE, false) => COLOR_WINDOW,
        (ACCENT, _) | (0x8b4800 | 0xa75600, false) => COLOR_HIGHLIGHT,
        (_, true) => COLOR_WINDOWTEXT,
        (BG | SIDE, false) => COLOR_BTNFACE,
        (BORDER | 0xc8c8c8 | 0x8a8a8a | 0xcfcfcf, false) => COLOR_WINDOWFRAME,
        _ => COLOR_BTNFACE,
    };
    unsafe { GetSysColor(system) }
}

impl Design {
    pub unsafe fn new(hwnd: HWND) -> Self {
        unsafe {
            let mut contrast: HIGHCONTRASTW = zeroed();
            contrast.cbSize = size_of::<HIGHCONTRASTW>() as u32;
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                contrast.cbSize,
                (&mut contrast as *mut HIGHCONTRASTW).cast(),
                0,
            );
            HIGH_CONTRAST.set(contrast.dwFlags & HCF_HIGHCONTRASTON != 0);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let fonts = Self::fonts(dpi);
            Self {
                items: Vec::new(),
                fonts,
                background: Brush(CreateSolidBrush(themed(BG, false))),
                surface: Brush(CreateSolidBrush(themed(WHITE, false))),
                sidebar: Brush(CreateSolidBrush(themed(SIDE, false))),
                dpi,
                page: 0,
                scroll: 0,
            }
        }
    }

    pub unsafe fn refresh_theme(&mut self, hwnd: HWND) {
        unsafe {
            let current = Self::new(hwnd);
            self.background = current.background;
            self.surface = current.surface;
            self.sidebar = current.sidebar;
        }
    }

    unsafe fn fonts(dpi: u32) -> Vec<FontHandle> {
        unsafe {
            [(14, 400), (12, 400), (14, 600), (28, 600), (16, 600)]
                .into_iter()
                .map(|(size, weight)| {
                    FontHandle(CreateFontW(
                        -size * dpi as i32 / 96,
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
                    ))
                })
                .collect()
        }
    }

    fn font(&self, kind: Kind) -> HFONT {
        let index = match kind {
            Kind::Text(Font::Caption) => 1,
            Kind::Text(Font::Strong) => 2,
            Kind::Text(Font::Title) => 3,
            Kind::Text(Font::Section) => 4,
            _ => 0,
        };
        self.fonts[index].0
    }

    pub unsafe fn add(&mut self, item: Item) {
        unsafe {
            SendMessageW(item.hwnd, WM_SETFONT, self.font(item.kind) as usize, 0);
            if matches!(item.kind, Kind::Button(_) | Kind::Edit | Kind::Check) {
                SetWindowSubclass(item.hwnd, Some(interaction), 1, 0);
            }
            if matches!(item.kind, Kind::Edit) {
                SendMessageW(
                    item.hwnd,
                    EM_SETMARGINS,
                    (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
                    0,
                );
            }
            self.items.push(item);
        }
    }

    pub unsafe fn layout(&mut self, hwnd: HWND, page: usize) {
        unsafe {
            self.layout_at_dpi(hwnd, page, GetDpiForWindow(hwnd).max(96));
        }
    }

    pub unsafe fn layout_at_dpi(&mut self, hwnd: HWND, page: usize, dpi: u32) {
        unsafe {
            self.page = page;
            if dpi != self.dpi {
                self.dpi = dpi;
                let old = std::mem::replace(&mut self.fonts, Self::fonts(dpi));
                for item in &self.items {
                    SendMessageW(item.hwnd, WM_SETFONT, self.font(item.kind) as usize, 0);
                }
                drop(old);
            }
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            let width = client.right * 96 / dpi as i32;
            let height = client.bottom * 96 / dpi as i32;
            self.scroll = self.scroll.clamp(0, (620 - height).max(0));
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: 437,
                nPage: (height - 182).max(1) as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(hwnd, SB_VERT, &info, 1);
            for item in &self.items {
                let mut rect = item.layout.resolve(width, height, dpi);
                let body = item.page.is_some() && item.layout.rect.1 >= 100;
                if body {
                    rect.top -= self.px(self.scroll);
                    rect.bottom -= self.px(self.scroll);
                }
                // The native text editor sits inside a padded, parent-painted field.
                if matches!(item.kind, Kind::Edit) {
                    rect.left += self.px(10);
                    rect.right -= self.px(10);
                    rect.top += self.px(7);
                    rect.bottom -= self.px(6);
                }
                MoveWindow(
                    item.hwnd,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    0,
                );
                if body {
                    let top = (self.px(100) - rect.top).max(0);
                    let bottom =
                        (client.bottom - self.px(82) - rect.top).min(rect.bottom - rect.top);
                    let region = if bottom > top {
                        CreateRectRgn(0, top, rect.right - rect.left, bottom)
                    } else {
                        CreateRectRgn(0, 0, 0, 0)
                    };
                    if SetWindowRgn(item.hwnd, region, 0) == 0 {
                        DeleteObject(region);
                    }
                }
                ShowWindow(
                    item.hwnd,
                    if item.page.is_none_or(|p| p == page) {
                        SW_SHOWNA
                    } else {
                        SW_HIDE
                    },
                );
                if matches!(item.kind, Kind::Combo) {
                    SendMessageW(
                        item.hwnd,
                        CB_SETITEMHEIGHT,
                        usize::MAX,
                        self.px(COMBO_ITEM_HEIGHT) as isize,
                    );
                    SendMessageW(
                        item.hwnd,
                        CB_SETITEMHEIGHT,
                        0,
                        self.px(COMBO_ITEM_HEIGHT) as isize,
                    );
                }
            }
            RedrawWindow(
                hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
            );
        }
    }

    fn px(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }

    pub unsafe fn reveal(&mut self, hwnd: HWND, field: HWND) {
        unsafe {
            let Some(item) = self
                .items
                .iter()
                .find(|i| i.hwnd == field && i.page == Some(self.page) && i.layout.rect.1 >= 100)
            else {
                return;
            };
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            let height = client.bottom * 96 / self.dpi as i32;
            let (_, y, _, h) = item.layout.rect;
            if y - self.scroll < 100 {
                self.scroll = y - 100;
            } else if y + h - self.scroll > height - 82 {
                self.scroll = y + h - (height - 82);
            }
        }
    }

    pub unsafe fn paint(&self, hwnd: HWND, dc: HDC) {
        unsafe {
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            FillRect(dc, &client, self.background.0);
            FillRect(
                dc,
                &RECT {
                    right: self.px(220),
                    ..client
                },
                self.sidebar.0,
            );
            let width = client.right * 96 / self.dpi as i32;
            let height = client.bottom * 96 / self.dpi as i32;
            let cards: &[(i32, i32)] = match self.page {
                0 => &[(100, 258), (374, 120)],
                1 => &[(100, 378)],
                _ => &[(100, 196), (312, 222)],
            };
            let saved = SaveDC(dc);
            IntersectClipRect(
                dc,
                self.px(220),
                self.px(100),
                client.right,
                client.bottom - self.px(82),
            );
            for &(y, h) in cards {
                let rect = Layout::new((252, y - self.scroll, 736, h), Anchor::Stretch)
                    .resolve(width, height, self.dpi);
                rounded(dc, rect, self.px(8), WHITE, BORDER);
            }
            RestoreDC(dc, saved);
            // Persistent action bar.
            let footer = RECT {
                left: self.px(220),
                top: client.bottom - self.px(82),
                ..client
            };
            FillRect(dc, &footer, self.surface.0);
            fill(
                dc,
                RECT {
                    bottom: footer.top + 1,
                    ..footer
                },
                BORDER,
            );
            let saved = SaveDC(dc);
            IntersectClipRect(
                dc,
                self.px(220),
                self.px(100),
                client.right,
                client.bottom - self.px(82),
            );
            for item in &self.items {
                if matches!(item.kind, Kind::Edit) && item.page.is_none_or(|p| p == self.page) {
                    let mut rect = item.layout.resolve(width, height, self.dpi);
                    rect.top -= self.px(self.scroll);
                    rect.bottom -= self.px(self.scroll);
                    let focused = GetFocus() == item.hwnd;
                    rounded(
                        dc,
                        rect,
                        self.px(4),
                        WHITE,
                        if focused { ACCENT } else { 0xc8c8c8 },
                    );
                    let bottom = RECT {
                        left: rect.left + self.px(3),
                        right: rect.right - self.px(3),
                        top: rect.bottom - self.px(if focused { 2 } else { 1 }),
                        bottom: rect.bottom,
                    };
                    fill(dc, bottom, if focused { ACCENT } else { 0x8a8a8a });
                }
            }
            RestoreDC(dc, saved);
        }
    }

    pub unsafe fn color(&self, hwnd: HWND, dc: HDC) -> HBRUSH {
        unsafe {
            let item = self.items.iter().find(|item| item.hwnd == hwnd);
            let (foreground, background, brush) = match item {
                Some(item) => {
                    let foreground = if matches!(item.kind, Kind::Text(Font::Caption)) {
                        MUTED
                    } else {
                        TEXT
                    };
                    if item.layout.rect.0 < 220 {
                        (foreground, SIDE, self.sidebar.0)
                    } else if item.page.is_none()
                        || item.layout.rect.1 < 100
                        || (item.page == Some(0) && item.layout.rect.1 >= 500)
                    {
                        if item.layout.bottom {
                            (foreground, WHITE, self.surface.0)
                        } else {
                            (foreground, BG, self.background.0)
                        }
                    } else {
                        (foreground, WHITE, self.surface.0)
                    }
                }
                None => (TEXT, WHITE, self.surface.0),
            };
            SetTextColor(dc, themed(foreground, true));
            SetBkColor(dc, themed(background, false));
            SetBkMode(dc, TRANSPARENT as i32);
            brush
        }
    }

    pub unsafe fn draw_combo(&self, draw: &DRAWITEMSTRUCT) -> bool {
        unsafe {
            if !self
                .items
                .iter()
                .any(|i| i.hwnd == draw.hwndItem && matches!(i.kind, Kind::Combo))
            {
                return false;
            }
            let dc = draw.hDC;
            let saved = SaveDC(dc);
            let selection_field = draw.itemState & ODS_COMBOBOXEDIT != 0;
            let selected = !selection_field && draw.itemState & ODS_SELECTED != 0;
            let background = if selected {
                COLOR_HIGHLIGHT
            } else {
                COLOR_WINDOW
            };
            FillRect(dc, &draw.rcItem, GetSysColorBrush(background));
            SetTextColor(
                dc,
                GetSysColor(if draw.itemState & ODS_DISABLED != 0 {
                    COLOR_GRAYTEXT
                } else if selected {
                    COLOR_HIGHLIGHTTEXT
                } else {
                    COLOR_WINDOWTEXT
                }),
            );
            SetBkMode(dc, TRANSPARENT as i32);
            SelectObject(dc, self.font(Kind::Combo));

            let length = SendMessageW(draw.hwndItem, CB_GETLBTEXTLEN, draw.itemID as usize, 0);
            if length >= 0 {
                let mut text = vec![0u16; length as usize + 1];
                SendMessageW(
                    draw.hwndItem,
                    CB_GETLBTEXT,
                    draw.itemID as usize,
                    text.as_mut_ptr() as isize,
                );
                // The selection field and popup have different client origins.
                // Anchor both labels to the same inset from the combo's left edge.
                let mut origin = POINT {
                    x: self.px(10),
                    y: 0,
                };
                if !selection_field {
                    let mut info: COMBOBOXINFO = zeroed();
                    info.cbSize = size_of::<COMBOBOXINFO>() as u32;
                    if GetComboBoxInfo(draw.hwndItem, &mut info) != 0 {
                        MapWindowPoints(draw.hwndItem, info.hwndList, &mut origin, 1);
                    }
                }
                let mut text_rect = draw.rcItem;
                text_rect.left = origin.x.max(text_rect.left);
                text_rect.right -= self.px(10);
                DrawTextW(
                    dc,
                    text.as_ptr(),
                    -1,
                    &mut text_rect,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
            }
            if draw.itemState & ODS_FOCUS != 0 && draw.itemState & ODS_NOFOCUSRECT == 0 {
                let mut focus = draw.rcItem;
                InflateRect(&mut focus, -1, -1);
                DrawFocusRect(dc, &focus);
            }
            RestoreDC(dc, saved);
            true
        }
    }

    pub unsafe fn draw_button(&self, draw: &DRAWITEMSTRUCT) -> bool {
        unsafe {
            let Some(item) = self.items.iter().find(|i| i.hwnd == draw.hwndItem) else {
                return false;
            };
            let Kind::Button(kind) = item.kind else {
                return false;
            };
            let dc = draw.hDC;
            let mut rect = draw.rcItem;
            let disabled = draw.itemState & ODS_DISABLED != 0;
            let pressed = draw.itemState & ODS_SELECTED != 0;
            let mut cursor: POINT = zeroed();
            GetCursorPos(&mut cursor);
            ScreenToClient(draw.hwndItem, &mut cursor);
            let hover = cursor.x >= 0
                && cursor.y >= 0
                && cursor.x < rect.right
                && cursor.y < rect.bottom
                && !disabled;
            let selected = matches!(kind, Button::Navigation(page) if page == self.page);
            let (bg, fg, border) = match kind {
                Button::Primary => (
                    if pressed {
                        0x8b4800
                    } else if hover {
                        0xa75600
                    } else {
                        ACCENT
                    },
                    WHITE,
                    ACCENT,
                ),
                Button::Navigation(_) => (
                    if selected {
                        0xe9e9e9
                    } else if hover {
                        0xececec
                    } else {
                        SIDE
                    },
                    TEXT,
                    SIDE,
                ),
                Button::Remove => (if hover { 0xe7e7e7 } else { SIDE }, 0x2424b4, SIDE),
                Button::Secondary => (
                    if pressed {
                        0xededed
                    } else if hover {
                        0xf6f6f6
                    } else {
                        WHITE
                    },
                    TEXT,
                    0xcfcfcf,
                ),
                Button::Link => (WHITE, ACCENT, WHITE),
            };
            fill(
                dc,
                rect,
                if item.layout.rect.0 < 220 {
                    SIDE
                } else {
                    WHITE
                },
            );
            rect.right -= 1;
            rect.bottom -= 1;
            rounded(dc, rect, self.px(5), bg, border);
            if selected {
                rounded(
                    dc,
                    RECT {
                        left: self.px(1),
                        top: self.px(12),
                        right: self.px(4),
                        bottom: rect.bottom - self.px(12),
                    },
                    self.px(3),
                    ACCENT,
                    ACCENT,
                );
            }
            let font = if selected || matches!(kind, Button::Primary) {
                self.fonts[2].0
            } else {
                self.fonts[0].0
            };
            let previous = SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(
                dc,
                if disabled && HIGH_CONTRAST.get() {
                    GetSysColor(COLOR_GRAYTEXT)
                } else {
                    themed(if disabled { MUTED } else { fg }, true)
                },
            );
            let mut text = vec![0u16; GetWindowTextLengthW(draw.hwndItem).max(0) as usize + 1];
            GetWindowTextW(draw.hwndItem, text.as_mut_ptr(), text.len() as i32);
            let align = if matches!(kind, Button::Navigation(_)) {
                rect.left += self.px(18);
                DT_LEFT
            } else {
                DT_CENTER
            };
            DrawTextW(
                dc,
                text.as_ptr(),
                -1,
                &mut rect,
                align
                    | DT_VCENTER
                    | DT_SINGLELINE
                    | if matches!(kind, Button::Primary) {
                        if draw.itemState & ODS_NOACCEL != 0 {
                            DT_HIDEPREFIX
                        } else {
                            0
                        }
                    } else {
                        DT_NOPREFIX
                    },
            );
            if draw.itemState & ODS_FOCUS != 0 && draw.itemState & ODS_NOFOCUSRECT == 0 {
                let mut focus = draw.rcItem;
                InflateRect(&mut focus, -self.px(4), -self.px(4));
                DrawFocusRect(dc, &focus);
            }
            SelectObject(dc, previous);
            true
        }
    }
}

unsafe fn fill(dc: HDC, rect: RECT, color: u32) {
    unsafe {
        let brush = Brush(CreateSolidBrush(themed(color, false)));
        FillRect(dc, &rect, brush.0);
    }
}

unsafe fn rounded(dc: HDC, rect: RECT, radius: i32, color: u32, border: u32) {
    unsafe {
        let brush = Brush(CreateSolidBrush(themed(color, false)));
        let pen = CreatePen(PS_SOLID, 1, themed(border, false));
        let old_brush = SelectObject(dc, brush.0);
        let old_pen = SelectObject(dc, pen);
        RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius,
            radius,
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        DeleteObject(pen);
    }
}

pub unsafe fn cue(hwnd: HWND, text: &str) {
    unsafe {
        SendMessageW(hwnd, EM_SETCUEBANNER, 0, wide(text).as_ptr() as isize);
    }
}

unsafe extern "system" fn interaction(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEMOVE => {
                let mut tracking = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tracking);
                InvalidateRect(hwnd, null(), 0);
            }
            WM_MOUSELEAVE => {
                InvalidateRect(hwnd, null(), 0);
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                InvalidateRect(GetParent(hwnd), null(), 0);
                if msg == WM_SETFOCUS {
                    PostMessageW(GetParent(hwnd), FOCUS_CHANGED, hwnd as usize, 0);
                }
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(interaction), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
