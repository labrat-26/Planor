//! `planor settings`: the native settings window. Pages are generated from `settings_model::Model`, so a new
//! key in `planor.example.toml` shows up without UI work. Every change is validated, saved after a short
//! quiet period (so typing is not a write per keystroke) and the running daemon is told to reload.
#![allow(dead_code)] // pages and controls are completed in the following tasks; removed in the final cleanup
use crate::{Result, settings_model::{Entry, Model}};
use std::time::{Duration, Instant};

/// Which control edits a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Check,
    Number { float: bool },
    Choice(&'static [&'static str]),
    Color,
    Hotkey,
    Text,
}
pub fn control_for(entry: &Entry) -> Control {
    match &entry.default {
        toml::Value::Boolean(_) => Control::Check,
        toml::Value::Integer(_) => Control::Number { float: false },
        toml::Value::Float(_) => Control::Number { float: true },
        _ => match crate::choices(&entry.key) {
            Some(c) => Control::Choice(c),
            None if entry.key.ends_with("_color") => Control::Color,
            None if entry.section == "hotkeys" => Control::Hotkey,
            None => Control::Text,
        },
    }
}

/// Coalesces a burst of edits into one save, `quiet` after the last of them.
pub struct SaveScheduler(crate::config_watch::Debounce);
impl SaveScheduler {
    pub fn new(quiet: Duration) -> Self {
        Self(crate::config_watch::Debounce::new(quiet))
    }
    pub fn touch(&mut self, now: Instant) {
        self.0.event(now)
    }
    pub fn take_due(&mut self, now: Instant) -> bool {
        self.0.due(now)
    }
    pub fn pending(&self) -> bool {
        self.0.pending()
    }
}

const SYNTAX_ERROR: &str = "planor.toml has a syntax error. Fix it in a text editor; the settings window will not overwrite it.";

/// Set setting `i` to `v`. Starts from what is on disk (when nothing of ours is waiting to be saved) so a
/// hand edit made while the window was open survives; refuses to touch a file that does not parse.
pub fn apply_change(model: &mut Model, i: usize, v: toml::Value) -> std::result::Result<(), String> {
    if Model::load_error(&model.path).is_some() {
        return Err(SYNTAX_ERROR.into());
    }
    if !model.dirty && model.path.exists() {
        model.reload_from_disk().map_err(|e| e.to_string())?;
    }
    model.set(i, v)
}
/// Write the file unless it is currently invalid.
pub fn save_guarded(model: &mut Model) -> std::result::Result<(), String> {
    if Model::load_error(&model.path).is_some() {
        return Err(SYNTAX_ERROR.into());
    }
    model.save().map_err(|e| e.to_string())
}

// ---- Win32 window ----------------------------------------------------------------------------------------
//
// Look: Windows 11 Settings. A navigation pane on the left (icon, label, accent pill on the selected item) and,
// on the right, the section title and one rounded card per setting: title and grey description on the left, the
// control on the right (switch, rounded field, drop-down). Shapes are anti-aliased with GDI+; text is GDI.

use crate::{Command, client, pipe_name, theme, wide};
use std::result::Result::Ok; // GdiPlus exports a constant named `Ok`
use std::{
    collections::HashMap,
    path::Path,
    sync::atomic::{AtomicPtr, Ordering},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Gdi::*, GdiPlus::*},
    System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
    UI::{
        Controls::*,
        HiDpi::*,
        Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_DOWN, VK_UP},
        Shell::{DefSubclassProc, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const BANNER_ID: usize = 101;
const ROW_BASE: usize = 1000;
const TIMER_SAVE: usize = 1;
const TIMER_REBUILD: usize = 2;
const INSTALL_PAGE: &str = "Install";

struct Fonts {
    title: HFONT,
    body: HFONT,
    small: HFONT,
    icon: HFONT,
    brand: HFONT,
}
struct Brushes {
    bg: HBRUSH,
    field: HBRUSH,
}
struct Row {
    entry: Option<usize>,
    card: RECT,
    title: String,
    title_rc: RECT,
    desc: String,
    desc_rc: RECT,
    field: Option<RECT>,
    swatch: bool,
    ctl: HWND,
    reset: HWND,
}
struct State {
    hwnd: HWND,
    nav: HWND,
    banner: HWND,
    viewport: HWND,
    page: HWND,
    model: Model,
    blocked: Option<String>,
    banner_text: String,
    pages: Vec<String>,
    selected: usize,
    dark: bool,
    pal: theme::Palette,
    accent: u32,
    brushes: Brushes,
    fonts: Fonts,
    dpi: u32,
    page_w: i32,
    content_h: i32,
    scroll: i32,
    rows: Vec<Row>,
    kids: Vec<HWND>,
    errors: HashMap<usize, String>,
    hover: HWND,
    nav_hover: Option<usize>,
    scheduler: SaveScheduler,
    stamp: Option<(std::time::SystemTime, u64)>,
    quiet: bool,
}

static STATE: AtomicPtr<State> = AtomicPtr::new(std::ptr::null_mut());
fn st() -> &'static mut State {
    unsafe { &mut *STATE.load(Ordering::Relaxed) }
}
fn lo(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}
fn hi(v: usize) -> u32 {
    ((v >> 16) & 0xFFFF) as u32
}
fn mul(px: i32, dpi: u32) -> i32 {
    px * dpi as i32 / 96
}
fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

/// "gap_px" -> "Gap (px)", "focused_color" -> "Focused color".
pub fn label_for(key: &str) -> String {
    let (base, unit) = [("_px", " (px)"), ("_ms", " (ms)"), ("_percent", " (%)"), ("_mb", " (MB)")]
        .iter()
        .find_map(|(suffix, unit)| key.strip_suffix(suffix).map(|b| (b, *unit)))
        .unwrap_or((key, ""));
    let mut text = base.replace('_', " ");
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    text + unit
}
/// Segoe Fluent Icons glyph for a navigation entry.
fn icon_for(page: &str) -> &'static str {
    match page {
        "layout" => "\u{E80A}",
        "borders" => "\u{E790}",
        "animation" => "\u{E768}",
        "touchpad" => "\u{E7C9}",
        "mouse" => "\u{E962}",
        "diagnostics" => "\u{E9D9}",
        "hotkeys" => "\u{E765}",
        INSTALL_PAGE => "\u{E896}",
        _ => "\u{E713}",
    }
}

fn make_font(face: &str, dp: i32, weight: i32, dpi: u32) -> HFONT {
    unsafe {
        CreateFontW(-mul(dp, dpi), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0, wide(face).as_ptr())
    }
}
fn make_fonts(dpi: u32) -> Fonts {
    Fonts {
        title: make_font("Segoe UI Variable Display", 28, 600, dpi),
        body: make_font("Segoe UI Variable Text", 14, 400, dpi),
        small: make_font("Segoe UI Variable Text", 12, 400, dpi),
        icon: make_font("Segoe Fluent Icons", 16, 400, dpi),
        brand: make_font("Segoe UI Variable Text", 16, 600, dpi),
    }
}
fn delete_fonts(f: &Fonts) {
    unsafe {
        for h in [f.title, f.body, f.small, f.icon, f.brand] {
            DeleteObject(h as HGDIOBJ);
        }
    }
}
fn make_brushes(pal: &theme::Palette) -> Brushes {
    unsafe { Brushes { bg: CreateSolidBrush(pal.bg), field: CreateSolidBrush(pal.field) } }
}
/// The accent colour, lightened in dark mode the way Windows does for text and switches.
fn accent_for(dark: bool) -> u32 {
    let c = theme::accent_color();
    if !dark {
        return c;
    }
    let mix = |v: u32| (v + (255 - v) * 30 / 100) & 0xFF;
    mix(c & 0xFF) | mix((c >> 8) & 0xFF) << 8 | mix((c >> 16) & 0xFF) << 16
}

// ---- drawing -------------------------------------------------------------------------------------------

fn argb(c: u32) -> u32 {
    0xFF00_0000 | ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
}
/// A drawing surface: GDI+ for shapes, GDI for text, both shifted by (-ox, -oy).
struct Canvas {
    hdc: HDC,
    g: *mut GpGraphics,
    ox: i32,
    oy: i32,
}
impl Canvas {
    fn new(hdc: HDC, ox: i32, oy: i32) -> Self {
        unsafe {
            let mut g = std::ptr::null_mut();
            GdipCreateFromHDC(hdc, &mut g);
            GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
            GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
            Self { hdc, g, ox, oy }
        }
    }
    fn off(&self, r: RECT) -> RECT {
        rect(r.left - self.ox, r.top - self.oy, r.right - self.ox, r.bottom - self.oy)
    }
    fn fill(&self, r: RECT, color: u32) {
        unsafe {
            let b = CreateSolidBrush(color);
            FillRect(self.hdc, &self.off(r), b);
            DeleteObject(b as HGDIOBJ);
        }
    }
    fn rrect(&self, r: RECT, radius: f32, fill: Option<u32>, line: Option<(u32, f32)>) {
        unsafe {
            let r = self.off(r);
            let inset = line.map_or(0.0, |(_, w)| w / 2.0);
            let (x, y) = (r.left as f32 + inset, r.top as f32 + inset);
            let (w, h) = ((r.right - r.left) as f32 - 2.0 * inset, (r.bottom - r.top) as f32 - 2.0 * inset);
            let d = (radius * 2.0).clamp(1.0, w.min(h));
            let mut path = std::ptr::null_mut();
            GdipCreatePath(FillModeAlternate, &mut path);
            GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
            GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
            GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
            GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(path);
            if let Some(color) = fill {
                let mut brush = std::ptr::null_mut();
                GdipCreateSolidFill(argb(color), &mut brush);
                GdipFillPath(self.g, brush as *mut GpBrush, path);
                GdipDeleteBrush(brush as *mut GpBrush);
            }
            if let Some((color, width)) = line {
                let mut pen = std::ptr::null_mut();
                GdipCreatePen1(argb(color), width, UnitWorld, &mut pen);
                GdipDrawPath(self.g, pen, path);
                GdipDeletePen(pen);
            }
            GdipDeletePath(path);
        }
    }
    fn circle(&self, cx: i32, cy: i32, d: i32, color: u32) {
        unsafe {
            let mut brush = std::ptr::null_mut();
            GdipCreateSolidFill(argb(color), &mut brush);
            GdipFillEllipse(self.g, brush as *mut GpBrush, (cx - self.ox) as f32 - d as f32 / 2.0, (cy - self.oy) as f32 - d as f32 / 2.0, d as f32, d as f32);
            GdipDeleteBrush(brush as *mut GpBrush);
        }
    }
    fn text(&self, s: &str, r: RECT, font: HFONT, color: u32, flags: u32) {
        unsafe {
            GdipFlush(self.g, FlushIntentionSync);
            let old = SelectObject(self.hdc, font as HGDIOBJ);
            SetTextColor(self.hdc, color);
            SetBkMode(self.hdc, TRANSPARENT as i32);
            let mut rc = self.off(r);
            let w = wide(s);
            DrawTextW(self.hdc, w.as_ptr(), -1, &mut rc, flags | DT_NOPREFIX);
            SelectObject(self.hdc, old);
        }
    }
}
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.g) };
    }
}
/// Paint `r` of `hdc` through an off-screen bitmap (no flicker); `f` draws in the original coordinates.
fn buffered(hdc: HDC, r: RECT, f: impl FnOnce(&Canvas)) {
    unsafe {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return;
        }
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp as HGDIOBJ);
        {
            let canvas = Canvas::new(mem, r.left, r.top);
            f(&canvas);
        }
        BitBlt(hdc, r.left, r.top, w, h, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mem);
    }
}

/// One instance at a time: a second launch brings the first window to the front and exits.
pub struct InstanceGuard(HANDLE);
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
pub fn single_instance() -> Option<InstanceGuard> {
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, wide("Local\\Planor.Settings").as_ptr());
        if handle.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let existing = FindWindowW(wide("PlanorSettings").as_ptr(), wide("Planor Settings").as_ptr());
            if !existing.is_null() {
                if IsIconic(existing) != 0 {
                    ShowWindow(existing, SW_RESTORE);
                }
                SetForegroundWindow(existing);
            }
            CloseHandle(handle);
            return None;
        }
        Some(InstanceGuard(handle))
    }
}

fn text_of(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}
fn set_text(hwnd: HWND, text: &str) {
    unsafe { SetWindowTextW(hwnd, wide(text).as_ptr()) };
}
/// A NUL-terminated UTF-16 string handed to a window procedure (at most 64 characters read).
unsafe fn wstr(p: *const u16) -> String {
    let mut v = vec![];
    for i in 0..64 {
        let c = unsafe { *p.add(i) };
        if c == 0 {
            break;
        }
        v.push(c);
    }
    String::from_utf16_lossy(&v)
}
fn stamp_of(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

// ---- page layout ---------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn child(st: &mut State, class: &str, text: &str, style: u32, ex: u32, r: RECT, id: usize, font: HFONT) -> HWND {
    unsafe {
        let h = CreateWindowExW(
            ex,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            style | WS_CHILD | WS_VISIBLE,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            st.page,
            id as isize as HMENU,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        SendMessageW(h, WM_SETFONT, font as usize, 1);
        if st.blocked.is_some() {
            EnableWindow(h, 0);
        }
        st.kids.push(h);
        h
    }
}

fn measure_height(st: &State, font: HFONT, text: &str, width: i32) -> i32 {
    unsafe {
        let hdc = GetDC(st.page);
        let old = SelectObject(hdc, font as HGDIOBJ);
        let mut rc = rect(0, 0, width, 0);
        let w = wide(text);
        DrawTextW(hdc, w.as_ptr(), -1, &mut rc, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
        SelectObject(hdc, old);
        ReleaseDC(st.page, hdc);
        rc.bottom
    }
}

fn clear_page(st: &mut State) {
    for h in st.kids.drain(..) {
        unsafe { DestroyWindow(h) };
    }
    st.rows.clear();
    st.hover = std::ptr::null_mut();
}

fn build_page(st: &mut State) {
    clear_page(st);
    st.quiet = true;
    let dpi = st.dpi;
    let s = |px: i32| mul(px, dpi);
    let (margin, pad, gap) = (s(36), s(16), s(6));
    let card_l = margin;
    let card_r = (st.page_w - margin).max(card_l + s(420));
    let mut y = s(24) + s(48) + s(16); // title block
    let page = st.pages[st.selected].clone();
    let body = st.fonts.body;
    if page == INSTALL_PAGE {
        let desc = "Install Planor for your user account and start it when you sign in.";
        let text_w = card_r - card_l - 2 * pad;
        let dh = measure_height(st, st.fonts.small, desc, text_w);
        let h = pad * 2 + s(20) + s(2) + dh;
        let card = rect(card_l, y, card_r, y + h);
        st.rows.push(Row {
            entry: None,
            card,
            title: "Install".into(),
            title_rc: rect(card_l + pad, y + pad, card_r - pad, y + pad + s(20)),
            desc: desc.into(),
            desc_rc: rect(card_l + pad, y + pad + s(22), card_r - pad, y + pad + s(22) + dh),
            field: None,
            swatch: false,
            ctl: std::ptr::null_mut(),
            reset: std::ptr::null_mut(),
        });
        y += h + gap;
    } else {
        for i in st.model.in_section(&page) {
            let entry = st.model.entries[i].clone();
            let base = ROW_BASE + i * 4;
            let control = control_for(&entry);
            let value = st.model.value(i);
            let editable = !matches!(control, Control::Check);
            let (ctl_w, ctl_h, reset_w) = (if editable { s(220) } else { s(92) }, s(32), s(56));
            let text_w = (card_r - card_l - 2 * pad - ctl_w - reset_w - s(24)).max(s(160));
            let title = label_for(&entry.key);
            let desc_h = if entry.help.is_empty() { 0 } else { measure_height(st, st.fonts.small, &entry.help, text_w) };
            let desc_h = if editable { desc_h.max(s(16)) } else { desc_h };
            let block = s(20) + if desc_h > 0 { s(2) + desc_h } else { 0 };
            let h = (pad * 2 + block).max(if editable { s(64) } else { s(56) });
            let block = if entry.help.is_empty() { s(20) } else { block };
            let top = y + (h - block) / 2;
            let card = rect(card_l, y, card_r, y + h);
            let cy = y + (h - ctl_h) / 2;
            let ctl_rc = rect(card_r - pad - ctl_w, cy, card_r - pad, cy + ctl_h);
            let reset_rc = rect(ctl_rc.left - s(8) - reset_w, cy, ctl_rc.left - s(8), cy + ctl_h);
            let mut field = None;
            let mut swatch = false;
            let ctl;
            match control {
                Control::Check => {
                    ctl = child(st, "BUTTON", "", BS_OWNERDRAW as u32 | WS_TABSTOP, 0, ctl_rc, base, body);
                }
                Control::Choice(_) => {
                    ctl = child(st, "BUTTON", "", BS_OWNERDRAW as u32 | WS_TABSTOP, 0, ctl_rc, base, body);
                    field = Some(ctl_rc);
                }
                other => {
                    swatch = matches!(other, Control::Color);
                    let inner_l = ctl_rc.left + s(12) + if swatch { s(28) } else { 0 };
                    let eh = s(20);
                    let shown = match &value {
                        toml::Value::String(t) => t.clone(),
                        v => v.to_string(),
                    };
                    ctl = child(st, "EDIT", &shown, ES_AUTOHSCROLL as u32 | WS_TABSTOP, 0, rect(inner_l, cy + (ctl_h - eh) / 2, ctl_rc.right - s(12), cy + (ctl_h - eh) / 2 + eh), base, body);
                    field = Some(ctl_rc);
                    if matches!(other, Control::Number { .. }) {
                        unsafe { SetWindowSubclass(ctl, Some(edit_proc), 1, i) };
                    }
                }
            }
            if matches!(control, Control::Check | Control::Choice(_)) {
                unsafe { SetWindowSubclass(ctl, Some(hover_proc), 2, 0) };
            }
            let reset = child(st, "BUTTON", "Reset", BS_OWNERDRAW as u32 | WS_TABSTOP, 0, reset_rc, base + 1, body);
            unsafe {
                SetWindowSubclass(reset, Some(hover_proc), 2, 0);
                ShowWindow(reset, if value != entry.default { SW_SHOW } else { SW_HIDE });
            }
            st.rows.push(Row {
                entry: Some(i),
                card,
                title,
                title_rc: rect(card_l + pad, top, card_l + pad + text_w, top + s(20)),
                desc: entry.help.clone(),
                desc_rc: rect(card_l + pad, top + s(22), card_l + pad + text_w, top + s(22) + desc_h),
                field,
                swatch,
                ctl,
                reset,
            });
            y += h + gap;
        }
    }
    st.content_h = y + s(24);
    st.quiet = false;
    apply_scroll(st);
    unsafe { InvalidateRect(st.page, std::ptr::null(), 1) };
}

fn apply_scroll(st: &mut State) {
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(st.viewport, &mut rc);
        let view_h = rc.bottom;
        st.scroll = st.scroll.clamp(0, (st.content_h - view_h).max(0));
        SetWindowPos(st.page, std::ptr::null_mut(), 0, -st.scroll, st.page_w, st.content_h, SWP_NOZORDER | SWP_NOACTIVATE);
        let info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: st.content_h,
            nPage: view_h.max(0) as u32,
            nPos: st.scroll,
            nTrackPos: 0,
        };
        SetScrollInfo(st.viewport, SB_VERT, &info, 1);
    }
}

fn layout(st: &mut State) {
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(st.hwnd, &mut rc);
        let dpi = st.dpi;
        let banner_h = if st.banner_text.is_empty() { 0 } else { mul(44, dpi) };
        let nav_w = mul(260, dpi);
        MoveWindow(st.banner, mul(16, dpi), mul(6, dpi), rc.right - mul(32, dpi), banner_h.max(1) - mul(8, dpi).min(banner_h.max(1) - 1), 1);
        ShowWindow(st.banner, if banner_h == 0 { SW_HIDE } else { SW_SHOW });
        MoveWindow(st.nav, 0, banner_h, nav_w, rc.bottom - banner_h, 1);
        MoveWindow(st.viewport, nav_w, banner_h, rc.right - nav_w, rc.bottom - banner_h, 1);
        let mut vr: RECT = std::mem::zeroed();
        GetClientRect(st.viewport, &mut vr);
        st.page_w = vr.right;
    }
}

fn set_banner(st: &mut State, text: &str) {
    if st.banner_text != text {
        st.banner_text = text.to_string();
        set_text(st.banner, text);
        layout(st);
        build_page(st);
    }
}

fn refresh_from_disk(st: &mut State) {
    st.blocked = Model::load_error(&st.model.path);
    if st.blocked.is_none() && !st.model.dirty {
        let _ = st.model.reload_from_disk(); // a missing file keeps the in-memory defaults
    }
    st.stamp = stamp_of(&st.model.path);
    let banner = st.blocked.as_ref().map(|e| format!("{SYNTAX_ERROR}  ({e})")).unwrap_or_default();
    st.banner_text = String::from("\u{0}"); // force the rebuild below
    set_banner(st, &banner);
}

fn start_save_timer(st: &mut State) {
    st.scheduler.touch(Instant::now());
    unsafe { SetTimer(st.hwnd, TIMER_SAVE, 50, None) };
}
fn flush_save(st: &mut State) {
    unsafe { KillTimer(st.hwnd, TIMER_SAVE) };
    if !st.model.dirty {
        return;
    }
    match save_guarded(&mut st.model) {
        Ok(()) => {
            st.stamp = stamp_of(&st.model.path);
            std::thread::spawn(|| {
                let _ = client(&pipe_name(), &Command::Reload);
            });
        }
        Err(e) => set_banner(st, &e),
    }
}

// ---- editing -------------------------------------------------------------------------------------------

fn row_of(st: &State, i: usize) -> Option<&Row> {
    st.rows.iter().find(|r| r.entry == Some(i))
}
fn repaint_row(st: &State, i: usize) {
    if let Some(r) = row_of(st, i) {
        unsafe {
            InvalidateRect(st.page, &r.card, 0);
            for h in [r.ctl, r.reset] {
                InvalidateRect(h, std::ptr::null(), 0);
            }
        }
    }
}
fn set_error(st: &mut State, i: usize, error: Option<String>) {
    match error {
        Some(e) => st.errors.insert(i, e),
        None => st.errors.remove(&i),
    };
    repaint_row(st, i);
}
fn refresh_reset(st: &State, i: usize) {
    if let Some(r) = row_of(st, i) {
        let changed = st.model.value(i) != st.model.entries[i].default;
        unsafe { ShowWindow(r.reset, if changed { SW_SHOW } else { SW_HIDE }) };
    }
}
fn change(st: &mut State, i: usize, v: toml::Value) {
    match apply_change(&mut st.model, i, v) {
        Ok(()) => {
            set_error(st, i, None);
            refresh_reset(st, i);
            start_save_timer(st);
        }
        Err(e) => set_error(st, i, Some(e)),
    }
}

fn choose(st: &mut State, i: usize, choices: &'static [&'static str]) {
    let Some(r) = row_of(st, i) else { return };
    let current = st.model.value(i);
    unsafe {
        let menu = CreatePopupMenu();
        for (n, c) in choices.iter().enumerate() {
            let flags = MF_STRING | if current.as_str() == Some(c) { MF_CHECKED } else { 0 };
            AppendMenuW(menu, flags, n + 1, wide(c).as_ptr());
        }
        let mut at = POINT { x: 0, y: (r.field.map_or(0, |f| f.bottom - f.top)) };
        ClientToScreen(r.ctl, &mut at);
        let picked = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_TOPALIGN, at.x, at.y + 2, 0, st.hwnd, std::ptr::null());
        DestroyMenu(menu);
        if picked > 0 {
            change(st, i, toml::Value::String(choices[picked as usize - 1].to_string()));
        }
    }
}

fn row_command(st: &mut State, id: usize, code: u32) {
    if st.quiet || id < ROW_BASE {
        return;
    }
    let (i, slot) = ((id - ROW_BASE) / 4, (id - ROW_BASE) % 4);
    if i >= st.model.entries.len() {
        return;
    }
    if slot == 1 && code == BN_CLICKED {
        let default = st.model.entries[i].default.clone();
        change(st, i, default);
        build_page(st);
        return;
    }
    if slot != 0 {
        return;
    }
    match control_for(&st.model.entries[i]) {
        Control::Check if code == BN_CLICKED => {
            let on = st.model.value(i).as_bool() == Some(true);
            change(st, i, toml::Value::Boolean(!on));
        }
        Control::Choice(choices) if code == BN_CLICKED => choose(st, i, choices),
        Control::Number { .. } | Control::Color | Control::Hotkey | Control::Text => {
            let Some(ctl) = row_of(st, i).map(|r| r.ctl) else { return };
            if code == EN_CHANGE {
                match st.model.parse_input(i, &text_of(ctl)) {
                    Ok(v) => change(st, i, v),
                    Err(e) => set_error(st, i, Some(e)),
                }
                repaint_row(st, i);
            } else if code == EN_SETFOCUS || code == EN_KILLFOCUS {
                repaint_row(st, i);
            }
        }
        _ => (),
    }
}

fn select_page(st: &mut State, at: usize) {
    if at < st.pages.len() && at != st.selected {
        st.selected = at;
        st.scroll = 0;
        build_page(st);
        unsafe { InvalidateRect(st.nav, std::ptr::null(), 0) };
    }
}

fn retheme(st: &mut State) {
    st.dark = theme::system_is_dark();
    st.pal = theme::palette(st.dark);
    st.accent = accent_for(st.dark);
    unsafe {
        DeleteObject(st.brushes.bg as HGDIOBJ);
        DeleteObject(st.brushes.field as HGDIOBJ);
    }
    st.brushes = make_brushes(&st.pal);
    theme::apply_window(st.hwnd, st.dark);
    theme::apply_control(st.viewport, st.dark);
    build_page(st);
    unsafe { InvalidateRect(st.hwnd, std::ptr::null(), 1) };
    unsafe { InvalidateRect(st.nav, std::ptr::null(), 1) };
}

// ---- painting ------------------------------------------------------------------------------------------

fn paint_page(st: &State, c: &Canvas, clip: RECT) {
    let (pal, dpi) = (st.pal, st.dpi);
    let s = |px: i32| mul(px, dpi);
    c.fill(clip, pal.bg);
    let page = &st.pages[st.selected];
    c.text(&label_for(page), rect(s(36), s(24), st.page_w - s(36), s(24) + s(48)), st.fonts.title, pal.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    for row in &st.rows {
        if row.card.bottom < clip.top || row.card.top > clip.bottom {
            continue;
        }
        c.rrect(row.card, s(4) as f32, Some(pal.card), Some((pal.border, 1.0)));
        let error = row.entry.and_then(|i| st.errors.get(&i));
        // a card with no description makes room for the error line by lifting its title
        let lift = if error.is_some() && row.desc.is_empty() { s(10) } else { 0 };
        let up = |r: RECT| rect(r.left, r.top - lift, r.right, r.bottom - lift);
        c.text(&row.title, up(row.title_rc), st.fonts.body, pal.text, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
        match error {
            Some(e) => c.text(e, up(row.desc_rc), st.fonts.small, pal.error, DT_LEFT | DT_WORDBREAK),
            None if !row.desc.is_empty() => c.text(&row.desc, row.desc_rc, st.fonts.small, pal.subtext, DT_LEFT | DT_WORDBREAK),
            None => (),
        }
        let (Some(f), Some(i)) = (row.field, row.entry) else { continue };
        if matches!(control_for(&st.model.entries[i]), Control::Choice(_)) {
            continue; // the drop-down paints itself
        }
        let focused = unsafe { GetFocus() } == row.ctl;
        c.rrect(f, s(4) as f32, Some(pal.field), Some((if error.is_some() { pal.error } else { pal.border }, 1.0)));
        let line = if error.is_some() { pal.error } else if focused { st.accent } else { pal.field_line };
        c.fill(rect(f.left + s(3), f.bottom - if focused { s(2) } else { 1 }, f.right - s(3), f.bottom), line);
        if row.swatch {
            let hex = st.model.value(i);
            let hex = hex.as_str().unwrap_or("#000000");
            if let Some(color) = theme::hex_to_colorref(hex) {
                c.rrect(rect(f.left + s(10), f.top + (f.bottom - f.top - s(18)) / 2, f.left + s(28), f.top + (f.bottom - f.top - s(18)) / 2 + s(18)), s(3) as f32, Some(color), Some((pal.field_line, 1.0)));
            }
        }
    }
}

fn draw_item(st: &State, d: &DRAWITEMSTRUCT) {
    let id = d.CtlID as usize;
    if id < ROW_BASE {
        return;
    }
    let (i, slot) = ((id - ROW_BASE) / 4, (id - ROW_BASE) % 4);
    let (pal, dpi) = (st.pal, st.dpi);
    let s = |px: i32| mul(px, dpi);
    let rc = d.rcItem;
    let hover = st.hover == d.hwndItem;
    let pressed = d.itemState & ODS_SELECTED != 0;
    let focus = d.itemState & ODS_FOCUS != 0 && d.itemState & ODS_NOFOCUSRECT == 0;
    buffered(d.hDC, rc, |c| {
        c.fill(rc, pal.card);
        if slot == 1 {
            if hover || pressed {
                c.rrect(rc, s(4) as f32, Some(pal.hover), None);
            }
            c.text("Reset", rc, st.fonts.body, st.accent, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        } else {
            match control_for(&st.model.entries[i]) {
                Control::Check => {
                    let on = st.model.value(i).as_bool() == Some(true);
                    let (w, h) = (s(40), s(20));
                    let sw = rect(rc.right - w, rc.top + (rc.bottom - rc.top - h) / 2, rc.right, rc.top + (rc.bottom - rc.top - h) / 2 + h);
                    c.text(if on { "On" } else { "Off" }, rect(rc.left, rc.top, sw.left - s(10), rc.bottom), st.fonts.body, pal.text, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
                    let cy = sw.top + h / 2;
                    let knob = if hover || pressed { s(14) } else { s(12) };
                    if on {
                        c.rrect(sw, (h / 2) as f32, Some(st.accent), None);
                        c.circle(sw.right - s(10), cy, knob, if st.dark { 0 } else { 0xFFFFFF });
                    } else {
                        c.rrect(sw, (h / 2) as f32, if hover { Some(pal.hover) } else { None }, Some((pal.track, 1.0)));
                        c.circle(sw.left + s(10), cy, knob, pal.track);
                    }
                }
                Control::Choice(_) => {
                    c.rrect(rc, s(4) as f32, Some(if hover || pressed { pal.hover } else { pal.field }), Some((pal.border, 1.0)));
                    c.fill(rect(rc.left + s(3), rc.bottom - 1, rc.right - s(3), rc.bottom), pal.field_line);
                    let v = st.model.value(i);
                    c.text(v.as_str().unwrap_or(""), rect(rc.left + s(12), rc.top, rc.right - s(36), rc.bottom), st.fonts.body, pal.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                    c.text("\u{E70D}", rect(rc.right - s(32), rc.top, rc.right - s(10), rc.bottom), st.fonts.icon, pal.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                }
                _ => (),
            }
        }
        if focus {
            c.rrect(rect(rc.left + 1, rc.top + 1, rc.right - 1, rc.bottom - 1), s(4) as f32, None, Some((pal.text, 2.0)));
        }
    });
}

fn paint_nav(st: &State, hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        let (pal, dpi) = (st.pal, st.dpi);
        let s = |px: i32| mul(px, dpi);
        buffered(hdc, rc, |c| {
            c.fill(rc, pal.bg);
            DrawIconEx(c.hdc, s(18), s(18), theme::app_icon(s(28)), s(28), s(28), 0, std::ptr::null_mut(), DI_NORMAL);
            c.text("Planor", rect(s(56), s(14), rc.right - s(8), s(14) + s(36)), st.fonts.brand, pal.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            for (n, page) in st.pages.iter().enumerate() {
                let top = s(72) + n as i32 * s(44);
                let item = rect(s(8), top, rc.right - s(8), top + s(40));
                let selected = n == st.selected;
                if selected {
                    c.rrect(item, s(4) as f32, Some(pal.nav_sel), None);
                    c.rrect(rect(item.left, top + s(12), item.left + s(3), top + s(28)), 1.5, Some(st.accent), None);
                } else if st.nav_hover == Some(n) {
                    c.rrect(item, s(4) as f32, Some(pal.hover), None);
                }
                c.text(icon_for(page), rect(item.left + s(12), top, item.left + s(36), top + s(40)), st.fonts.icon, pal.text, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                c.text(&label_for(page), rect(item.left + s(46), top, item.right - s(8), top + s(40)), st.fonts.body, pal.text, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
            }
        });
        EndPaint(hwnd, &ps);
    }
}

fn erase(hwnd: HWND, hdc: HDC, brush: HBRUSH) -> LRESULT {
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        FillRect(hdc, &rc, brush);
    }
    1
}

// ---- window procedures ---------------------------------------------------------------------------------

fn track_leave(hwnd: HWND) {
    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
    unsafe { TrackMouseEvent(&mut t) };
}

/// Hover highlight for the owner-drawn buttons.
unsafe extern "system" fn hover_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        let p = STATE.load(Ordering::Relaxed);
        if !p.is_null() {
            let s = &mut *p;
            match msg {
                WM_MOUSEMOVE if s.hover != hwnd => {
                    s.hover = hwnd;
                    track_leave(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                WM_MOUSELEAVE => {
                    s.hover = std::ptr::null_mut();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                _ => (),
            }
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// Up and down arrows in a number box step the value.
unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, entry: usize) -> LRESULT {
    unsafe {
        if msg == WM_KEYDOWN && (wp as u16 == VK_UP || wp as u16 == VK_DOWN) {
            let p = STATE.load(Ordering::Relaxed);
            if !p.is_null() {
                let s = &mut *p;
                let dir = if wp as u16 == VK_UP { 1 } else { -1 };
                if let Ok(v) = s.model.stepped(entry, dir) {
                    set_text(hwnd, &v.to_string()); // EN_CHANGE then validates and saves it
                }
            }
            return 0;
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

unsafe extern "system" fn page_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let p = STATE.load(Ordering::Relaxed);
        if p.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let s = &mut *p;
        match msg {
            WM_COMMAND => {
                row_command(s, lo(wp) as usize, hi(wp));
                0
            }
            WM_DRAWITEM => {
                draw_item(s, &*(lp as *const DRAWITEMSTRUCT));
                1
            }
            WM_CTLCOLOREDIT => {
                SetTextColor(wp as HDC, s.pal.text);
                SetBkColor(wp as HDC, s.pal.field);
                s.brushes.field as LRESULT
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                buffered(hdc, ps.rcPaint, |c| paint_page(s, c, ps.rcPaint));
                EndPaint(hwnd, &ps);
                0
            }
            WM_MOUSEWHEEL => SendMessageW(s.viewport, msg, wp, lp),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn viewport_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let p = STATE.load(Ordering::Relaxed);
        if p.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let s = &mut *p;
        match msg {
            WM_ERASEBKGND => erase(hwnd, wp as HDC, s.brushes.bg),
            WM_VSCROLL | WM_MOUSEWHEEL => {
                let line = mul(40, s.dpi);
                let mut view: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut view);
                let delta = ((wp >> 16) as u16 as i16) as i32;
                s.scroll = if msg == WM_MOUSEWHEEL {
                    s.scroll - delta * line * 3 / 120
                } else {
                    match lo(wp) as i32 {
                        SB_LINEUP => s.scroll - line,
                        SB_LINEDOWN => s.scroll + line,
                        SB_PAGEUP => s.scroll - view.bottom,
                        SB_PAGEDOWN => s.scroll + view.bottom,
                        SB_THUMBTRACK | SB_THUMBPOSITION => hi(wp) as i32,
                        _ => s.scroll,
                    }
                };
                apply_scroll(s);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn nav_index(st: &State, y: i32) -> Option<usize> {
    let s = |px: i32| mul(px, st.dpi);
    let rel = y - s(72);
    (rel >= 0 && rel % s(44) < s(40) && ((rel / s(44)) as usize) < st.pages.len()).then_some((rel / s(44)) as usize)
}

unsafe extern "system" fn nav_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let p = STATE.load(Ordering::Relaxed);
        if p.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let s = &mut *p;
        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint_nav(s, hwnd);
                0
            }
            WM_GETDLGCODE => DLGC_WANTARROWS as LRESULT,
            WM_LBUTTONDOWN => {
                SetFocus(hwnd);
                if let Some(n) = nav_index(s, (lp >> 16) as i16 as i32) {
                    select_page(s, n);
                }
                0
            }
            WM_MOUSEMOVE => {
                let over = nav_index(s, (lp >> 16) as i16 as i32);
                if over != s.nav_hover {
                    s.nav_hover = over;
                    track_leave(hwnd);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_MOUSELEAVE => {
                s.nav_hover = None;
                InvalidateRect(hwnd, std::ptr::null(), 0);
                0
            }
            WM_KEYDOWN => {
                match wp as u16 {
                    VK_UP if s.selected > 0 => select_page(s, s.selected - 1),
                    VK_DOWN => select_page(s, s.selected + 1),
                    _ => (),
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let p = STATE.load(Ordering::Relaxed);
        if p.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let s = &mut *p;
        match msg {
            WM_SIZE => {
                if wp as u32 != SIZE_MINIMIZED {
                    layout(s);
                    SetTimer(hwnd, TIMER_REBUILD, 120, None);
                }
                0
            }
            WM_TIMER => {
                match wp {
                    TIMER_SAVE if s.scheduler.take_due(Instant::now()) => flush_save(s),
                    TIMER_REBUILD => {
                        KillTimer(hwnd, TIMER_REBUILD);
                        build_page(s);
                    }
                    _ => (),
                }
                0
            }
            WM_CTLCOLORSTATIC => {
                SetTextColor(wp as HDC, s.pal.error);
                SetBkColor(wp as HDC, s.pal.bg);
                s.brushes.bg as LRESULT
            }
            WM_ERASEBKGND => erase(hwnd, wp as HDC, s.brushes.bg),
            WM_SETTINGCHANGE => {
                if lp != 0 && wstr(lp as *const u16) == "ImmersiveColorSet" {
                    retheme(s);
                }
                0
            }
            WM_DPICHANGED => {
                s.dpi = hi(wp);
                let r = &*(lp as *const RECT);
                SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                delete_fonts(&s.fonts);
                s.fonts = make_fonts(s.dpi);
                SendMessageW(s.banner, WM_SETFONT, s.fonts.body as usize, 1);
                layout(s);
                build_page(s);
                InvalidateRect(s.nav, std::ptr::null(), 1);
                0
            }
            WM_ACTIVATE => {
                if lo(wp) != 0 && !s.scheduler.pending() && stamp_of(&s.model.path) != s.stamp {
                    refresh_from_disk(s);
                }
                0
            }
            WM_CLOSE => {
                flush_save(s);
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn register(name: &str, proc: WNDPROC, style: u32) {
    unsafe {
        let class = wide(name);
        let wc = WNDCLASSW {
            style,
            lpfnWndProc: proc,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: theme::app_icon(32),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
    }
}

/// Open the settings window and run until it is closed.
pub fn run(path: &Path) -> Result<()> {
    let Some(_guard) = single_instance() else { return Ok(()) };
    theme::enable_app_dark_mode();
    let mut gdiplus_token = 0usize;
    unsafe {
        let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 };
        GdiplusStartup(&mut gdiplus_token, &input, std::ptr::null_mut());
    }
    let (model, load_problem) = match Model::load(path) {
        Ok(m) => (m, None),
        Err(e) => (Model::from_text(path, crate::EXAMPLE)?, Some(e.to_string())),
    };
    let blocked = Model::load_error(path).or(load_problem);
    let dark = theme::system_is_dark();
    let pal = theme::palette(dark);
    let mut pages: Vec<String> = model.sections.clone();
    pages.push(INSTALL_PAGE.into());
    register("PlanorSettings", Some(main_proc), CS_HREDRAW | CS_VREDRAW);
    register("PlanorViewport", Some(viewport_proc), 0);
    register("PlanorPage", Some(page_proc), 0);
    register("PlanorNav", Some(nav_proc), 0);
    unsafe {
        let dpi = GetDpiForSystem();
        let instance = GetModuleHandleW(std::ptr::null());
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            wide("PlanorSettings").as_ptr(),
            wide("Planor Settings").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            mul(1100, dpi),
            mul(760, dpi),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err("cannot create the settings window".into());
        }
        let mk = |class: &str, style: u32, ex: u32, id: usize| {
            CreateWindowExW(ex, wide(class).as_ptr(), wide("").as_ptr(), style | WS_CHILD | WS_VISIBLE, 0, 0, 0, 0, hwnd, id as isize as HMENU, instance, std::ptr::null())
        };
        let nav = mk("PlanorNav", WS_TABSTOP, 0, 100);
        let banner = mk("STATIC", 0, 0, BANNER_ID);
        let viewport = mk("PlanorViewport", WS_VSCROLL | WS_CLIPCHILDREN, WS_EX_CONTROLPARENT, 0);
        let page = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            wide("PlanorPage").as_ptr(),
            wide("").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
            0,
            0,
            0,
            0,
            viewport,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        let fonts = make_fonts(dpi);
        let state = Box::new(State {
            hwnd,
            nav,
            banner,
            viewport,
            page,
            model,
            blocked: blocked.clone(),
            banner_text: String::new(),
            pages,
            selected: 0,
            dark,
            pal,
            accent: accent_for(dark),
            brushes: make_brushes(&pal),
            fonts,
            dpi,
            page_w: 0,
            content_h: 0,
            scroll: 0,
            rows: vec![],
            kids: vec![],
            errors: HashMap::new(),
            hover: std::ptr::null_mut(),
            nav_hover: None,
            scheduler: SaveScheduler::new(Duration::from_millis(250)),
            stamp: stamp_of(path),
            quiet: false,
        });
        STATE.store(Box::into_raw(state), Ordering::Relaxed);
        let s = st();
        SendMessageW(banner, WM_SETFONT, s.fonts.body as usize, 1);
        theme::apply_window(hwnd, dark);
        theme::apply_control(viewport, dark);
        if let Some(e) = blocked {
            s.banner_text = format!("{SYNTAX_ERROR}  ({e})");
            set_text(banner, &s.banner_text.clone());
        }
        layout(s);
        build_page(s);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetFocus(nav);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        GdiplusShutdown(gdiplus_token);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp(name: &str, text: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("planor-ui-{name}-{}.toml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        p
    }
    fn idx(m: &Model, s: &str, k: &str) -> usize {
        m.entries.iter().position(|e| e.section == s && e.key == k).unwrap()
    }

    #[test]
    fn control_for_maps_every_example_entry() {
        let m = Model::from_text(Path::new("planor.toml"), crate::EXAMPLE).unwrap();
        for e in &m.entries {
            let c = control_for(e);
            match (&e.default, e.section.as_str()) {
                (toml::Value::Boolean(_), _) => assert_eq!(c, Control::Check, "{}", e.key),
                (toml::Value::Integer(_), _) => assert_eq!(c, Control::Number { float: false }, "{}", e.key),
                (toml::Value::Float(_), _) => assert_eq!(c, Control::Number { float: true }, "{}", e.key),
                (_, "hotkeys") => assert_eq!(c, Control::Hotkey, "{}", e.key),
                _ => assert_ne!(c, Control::Text, "{} should have a dedicated control", e.key),
            }
        }
        assert!(matches!(control_for(&m.entries[idx(&m, "animation", "curve")]), Control::Choice(_)));
        assert_eq!(control_for(&m.entries[idx(&m, "borders", "focused_color")]), Control::Color);
        assert_eq!(control_for(&m.entries[idx(&m, "hotkeys", "focus_left")]), Control::Hotkey);
    }

    #[test]
    fn save_scheduler_coalesces_bursts() {
        let t0 = Instant::now();
        let mut s = SaveScheduler::new(Duration::from_millis(250));
        for ms in [0, 40, 90, 200] {
            s.touch(t0 + Duration::from_millis(ms));
        }
        assert!(!s.take_due(t0 + Duration::from_millis(449)));
        assert!(s.take_due(t0 + Duration::from_millis(450)));
        assert!(!s.take_due(t0 + Duration::from_millis(2000)), "one save per burst");
    }

    #[test]
    fn apply_change_keeps_external_edit() {
        let p = temp("external", "[layout]\ngap_px = 6\n");
        let mut m = Model::load(&p).unwrap();
        std::fs::write(&p, "[layout]\ngap_px = 9\n").unwrap(); // a hand edit while the window is open
        let width = idx(&m, "borders", "width_px");
        apply_change(&mut m, width, toml::Value::Integer(4)).unwrap();
        save_guarded(&mut m).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("gap_px = 9"), "hand edit kept: {text}");
        assert!(text.contains("width_px = 4"), "window edit saved: {text}");
    }

    #[test]
    fn two_quick_edits_both_survive_until_the_save() {
        let p = temp("burst", "[layout]\ngap_px = 6\n");
        let mut m = Model::load(&p).unwrap();
        let (gap, width) = (idx(&m, "layout", "gap_px"), idx(&m, "borders", "width_px"));
        apply_change(&mut m, gap, toml::Value::Integer(8)).unwrap();
        apply_change(&mut m, width, toml::Value::Integer(5)).unwrap(); // must not reload over the unsaved first edit
        save_guarded(&mut m).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("gap_px = 8") && text.contains("width_px = 5"), "{text}");
    }

    #[test]
    fn invalid_file_blocks_editing() {
        let broken = "[layout\ngap_px = ";
        let p = temp("broken", broken);
        let mut m = Model::from_text(&p, crate::EXAMPLE).unwrap();
        let gap = idx(&m, "layout", "gap_px");
        assert!(apply_change(&mut m, gap, toml::Value::Integer(3)).is_err());
        assert!(save_guarded(&mut m).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken, "the file is never overwritten");
    }

    #[test]
    fn invalid_value_is_rejected_and_not_saved() {
        let p = temp("invalid", "[layout]\ngap_px = 6\n");
        let mut m = Model::load(&p).unwrap();
        let gap = idx(&m, "layout", "gap_px");
        assert!(apply_change(&mut m, gap, toml::Value::Integer(-1)).is_err());
        assert!(!m.dirty);
    }
}
