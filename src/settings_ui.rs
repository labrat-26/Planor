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

use crate::{Command, client, pipe_name, theme, wide};
use std::{
    path::Path,
    sync::atomic::{AtomicPtr, Ordering},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
    UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::{EnableWindow, SetFocus}, WindowsAndMessaging::*},
};

const NAV_ID: usize = 100;
const BANNER_ID: usize = 101;
const ROW_BASE: usize = 1000;
const TIMER_SAVE: usize = 1;
const TIMER_REBUILD: usize = 2;
const KIND_LABEL: isize = 1;
const KIND_HELP: isize = 2;
const KIND_ERROR: isize = 3;
const SS_LEFT: u32 = 0;
const SS_NOTIFY: u32 = 0x100;
const INSTALL_PAGE: &str = "Install";

struct Brushes {
    bg: HBRUSH,
    panel: HBRUSH,
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
    brushes: Brushes,
    font: HFONT,
    dpi: u32,
    page_w: i32,
    content_h: i32,
    scroll: i32,
    kids: Vec<HWND>,
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

fn make_font(dpi: u32) -> HFONT {
    unsafe {
        let mut m: ICONMETRICSW = std::mem::zeroed();
        m.cbSize = std::mem::size_of::<ICONMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETICONMETRICS, m.cbSize, (&mut m as *mut ICONMETRICSW).cast(), 0, dpi);
        CreateFontIndirectW(&m.lfFont)
    }
}
fn make_brushes(pal: &theme::Palette) -> Brushes {
    unsafe { Brushes { bg: CreateSolidBrush(pal.bg), panel: CreateSolidBrush(pal.panel) } }
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

#[allow(clippy::too_many_arguments)]
fn child(st: &mut State, class: &str, text: &str, style: u32, ex: u32, r: (i32, i32, i32, i32), id: usize, kind: isize) -> HWND {
    unsafe {
        let h = CreateWindowExW(
            ex,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            style | WS_CHILD | WS_VISIBLE,
            r.0,
            r.1,
            r.2,
            r.3,
            st.page,
            id as isize as HMENU,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        SendMessageW(h, WM_SETFONT, st.font as usize, 1);
        if kind != 0 {
            SetWindowLongPtrW(h, GWLP_USERDATA, kind);
        }
        match class {
            "EDIT" | "COMBOBOX" => theme::apply_input(h, st.dark),
            "BUTTON" | "msctls_updown32" => theme::apply_control(h, st.dark),
            _ => (),
        }
        if st.blocked.is_some() && kind == 0 {
            EnableWindow(h, 0);
        }
        st.kids.push(h);
        h
    }
}

fn measure_height(st: &State, text: &str, width: i32) -> i32 {
    unsafe {
        let hdc = GetDC(st.page);
        let old = SelectObject(hdc, st.font as HGDIOBJ);
        let mut rc = RECT { left: 0, top: 0, right: width, bottom: 0 };
        let w = wide(text);
        DrawTextW(hdc, w.as_ptr(), -1, &mut rc, DT_CALCRECT | DT_WORDBREAK);
        SelectObject(hdc, old);
        ReleaseDC(st.page, hdc);
        rc.bottom
    }
}

fn clear_page(st: &mut State) {
    for h in st.kids.drain(..) {
        unsafe { DestroyWindow(h) };
    }
}

fn build_page(st: &mut State) {
    clear_page(st);
    st.quiet = true;
    let dpi = st.dpi;
    let s = |px: i32| mul(px, dpi);
    let (pad, label_w, ctl_w, reset_w) = (s(24), s(215), s(200), s(72));
    let row_h = s(28);
    let full_w = (st.page_w - 2 * pad).max(s(300));
    let ctl_x = pad + label_w;
    let mut y = s(16);
    let page = st.pages[st.selected].clone();
    if page == INSTALL_PAGE {
        child(st, "STATIC", "Install options appear here.", SS_LEFT, 0, (pad, y, full_w, s(24)), 0, KIND_LABEL);
        y += s(40);
    } else {
        for i in st.model.in_section(&page) {
            let entry = st.model.entries[i].clone();
            let base = ROW_BASE + i * 4;
            let control = control_for(&entry);
            let value = st.model.value(i);
            let label = label_for(&entry.key);
            match control {
                Control::Check => {
                    let h = child(st, "BUTTON", "", BS_AUTOCHECKBOX as u32 | WS_TABSTOP, 0, (pad, y + s(2), s(24), s(24)), base, 0);
                    let on = value.as_bool() == Some(true);
                    unsafe { SendMessageW(h, BM_SETCHECK, if on { BST_CHECKED } else { BST_UNCHECKED } as usize, 0) };
                    child(st, "STATIC", &label, SS_LEFT | SS_NOTIFY, 0, (pad + s(30), y + s(4), label_w + ctl_w, s(22)), base + 3, KIND_LABEL);
                }
                other => {
                    child(st, "STATIC", &label, SS_LEFT, 0, (pad, y + s(4), label_w - s(8), s(22)), base + 3, KIND_LABEL);
                    match other {
                        Control::Choice(choices) => {
                            let h = child(st, "COMBOBOX", "", CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL, 0, (ctl_x, y, ctl_w, s(240)), base, 0);
                            for c in choices {
                                unsafe { SendMessageW(h, CB_ADDSTRING, 0, wide(c).as_ptr() as isize) };
                            }
                            let at = choices.iter().position(|c| Some(*c) == value.as_str()).unwrap_or(0);
                            unsafe { SendMessageW(h, CB_SETCURSEL, at, 0) };
                        }
                        _ => {
                            let shown = match &value {
                                toml::Value::String(t) => t.clone(),
                                v => v.to_string(),
                            };
                            let edit = child(st, "EDIT", &shown, ES_AUTOHSCROLL as u32 | WS_TABSTOP, WS_EX_CLIENTEDGE, (ctl_x, y, ctl_w, row_h), base, 0);
                            if matches!(other, Control::Number { float: false }) {
                                unsafe {
                                    let up = child(st, "msctls_updown32", "", UDS_ALIGNRIGHT as u32 | UDS_SETBUDDYINT as u32 | UDS_ARROWKEYS as u32, 0, (0, 0, 0, 0), base + 4000, 0);
                                    SendMessageW(up, UDM_SETBUDDY, edit as usize, 0);
                                    SendMessageW(up, UDM_SETRANGE32, 0, 100000);
                                    SendMessageW(up, UDM_SETPOS32, 0, value.as_integer().unwrap_or(0) as isize);
                                }
                            }
                        }
                    }
                }
            }
            let reset = child(st, "BUTTON", "Reset", BS_PUSHBUTTON as u32 | WS_TABSTOP, 0, (ctl_x + ctl_w + s(8), y, reset_w, row_h), base + 1, 0);
            unsafe { EnableWindow(reset, (value != entry.default && st.blocked.is_none()) as i32) };
            let help_h = if entry.help.is_empty() { 0 } else { measure_height(st, &entry.help, full_w) };
            let info_h = help_h.max(s(16));
            child(st, "STATIC", &entry.help, SS_LEFT, 0, (pad, y + row_h + s(2), full_w, info_h), base + 2, KIND_HELP);
            y += row_h + s(2) + info_h + s(10);
        }
    }
    st.content_h = y + s(8);
    st.quiet = false;
    apply_scroll(st);
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
        let nav_w = mul(210, dpi);
        MoveWindow(st.banner, 0, 0, rc.right, banner_h, 1);
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

fn show_info(st: &State, i: usize, error: Option<&str>) {
    unsafe {
        let h = GetDlgItem(st.page, (ROW_BASE + i * 4 + 2) as i32);
        if h.is_null() {
            return;
        }
        match error {
            Some(e) => {
                SetWindowLongPtrW(h, GWLP_USERDATA, KIND_ERROR);
                set_text(h, e);
            }
            None => {
                SetWindowLongPtrW(h, GWLP_USERDATA, KIND_HELP);
                set_text(h, &st.model.entries[i].help);
            }
        }
        InvalidateRect(h, std::ptr::null(), 1);
    }
}
fn refresh_reset(st: &State, i: usize) {
    unsafe {
        let h = GetDlgItem(st.page, (ROW_BASE + i * 4 + 1) as i32);
        if !h.is_null() {
            EnableWindow(h, (st.model.value(i) != st.model.entries[i].default) as i32);
        }
    }
}
fn change(st: &mut State, i: usize, v: toml::Value) {
    match apply_change(&mut st.model, i, v) {
        Ok(()) => {
            show_info(st, i, None);
            refresh_reset(st, i);
            start_save_timer(st);
        }
        Err(e) => show_info(st, i, Some(&e)),
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
    let ctl = unsafe { GetDlgItem(st.page, (ROW_BASE + i * 4) as i32) };
    match slot {
        1 if code == BN_CLICKED => {
            let default = st.model.entries[i].default.clone();
            change(st, i, default);
            build_page(st);
        }
        3 if code == STN_CLICKED => {
            // clicking a checkbox's label toggles it
            let on = unsafe { SendMessageW(ctl, BM_GETCHECK, 0, 0) } as u32 == BST_CHECKED;
            unsafe { SendMessageW(ctl, BM_SETCHECK, if on { BST_UNCHECKED } else { BST_CHECKED } as usize, 0) };
            change(st, i, toml::Value::Boolean(!on));
        }
        0 => match control_for(&st.model.entries[i]) {
            Control::Check if code == BN_CLICKED => {
                let on = unsafe { SendMessageW(ctl, BM_GETCHECK, 0, 0) } as u32 == BST_CHECKED;
                change(st, i, toml::Value::Boolean(on));
            }
            Control::Choice(choices) if code == CBN_SELCHANGE => {
                let at = unsafe { SendMessageW(ctl, CB_GETCURSEL, 0, 0) };
                if let Some(word) = choices.get(at.max(0) as usize) {
                    change(st, i, toml::Value::String(word.to_string()));
                }
            }
            Control::Number { .. } | Control::Color | Control::Hotkey | Control::Text if code == EN_CHANGE => {
                match st.model.parse_input(i, &text_of(ctl)) {
                    Ok(v) => change(st, i, v),
                    Err(e) => show_info(st, i, Some(&e)),
                }
            }
            _ => (),
        },
        _ => (),
    }
}

fn select_page(st: &mut State, at: usize) {
    if at < st.pages.len() {
        st.selected = at;
        st.scroll = 0;
        build_page(st);
    }
}

fn retheme(st: &mut State) {
    st.dark = theme::system_is_dark();
    st.pal = theme::palette(st.dark);
    unsafe {
        DeleteObject(st.brushes.bg as HGDIOBJ);
        DeleteObject(st.brushes.panel as HGDIOBJ);
    }
    st.brushes = make_brushes(&st.pal);
    theme::apply_window(st.hwnd, st.dark);
    theme::apply_control(st.nav, st.dark);
    theme::apply_control(st.viewport, st.dark);
    build_page(st);
    unsafe { InvalidateRect(st.hwnd, std::ptr::null(), 1) };
}

fn paint_colors(st: &State, msg: u32, hdc: HDC, ctl: HWND) -> LRESULT {
    unsafe {
        if msg == WM_CTLCOLORSTATIC {
            let color = match GetWindowLongPtrW(ctl, GWLP_USERDATA) {
                KIND_HELP => st.pal.subtext,
                KIND_ERROR => st.pal.error,
                _ => st.pal.text,
            };
            SetTextColor(hdc, color);
            SetBkColor(hdc, st.pal.bg);
            st.brushes.bg as LRESULT
        } else {
            SetTextColor(hdc, st.pal.text);
            SetBkColor(hdc, st.pal.panel);
            st.brushes.panel as LRESULT
        }
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
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => paint_colors(s, msg, wp as HDC, lp as HWND),
            WM_ERASEBKGND => erase(hwnd, wp as HDC, s.brushes.bg),
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
            WM_COMMAND => {
                if lo(wp) as usize == NAV_ID && hi(wp) == LBN_SELCHANGE {
                    let at = SendMessageW(s.nav, LB_GETCURSEL, 0, 0);
                    select_page(s, at.max(0) as usize);
                }
                0
            }
            WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                if lp as HWND == s.banner {
                    SetTextColor(wp as HDC, s.pal.error);
                    SetBkColor(wp as HDC, s.pal.bg);
                    return s.brushes.bg as LRESULT;
                }
                paint_colors(s, if msg == WM_CTLCOLORLISTBOX { WM_CTLCOLOREDIT } else { msg }, wp as HDC, lp as HWND)
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
                DeleteObject(s.font as HGDIOBJ);
                s.font = make_font(s.dpi);
                SendMessageW(s.nav, WM_SETFONT, s.font as usize, 1);
                SendMessageW(s.nav, LB_SETITEMHEIGHT, 0, mul(34, s.dpi) as isize);
                layout(s);
                build_page(s);
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
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_UPDOWN_CLASS | ICC_LISTVIEW_CLASSES,
        };
        InitCommonControlsEx(&icc);
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
            mul(1040, dpi),
            mul(700, dpi),
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
        let nav = mk("LISTBOX", LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_TABSTOP | WS_VSCROLL, 0, NAV_ID);
        let banner = mk("STATIC", SS_LEFT, 0, BANNER_ID);
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
        let font = make_font(dpi);
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
            brushes: make_brushes(&pal),
            font,
            dpi,
            page_w: 0,
            content_h: 0,
            scroll: 0,
            kids: vec![],
            scheduler: SaveScheduler::new(Duration::from_millis(250)),
            stamp: stamp_of(path),
            quiet: false,
        });
        STATE.store(Box::into_raw(state), Ordering::Relaxed);
        let s = st();
        for p in &s.pages {
            SendMessageW(nav, LB_ADDSTRING, 0, wide(&format!("  {}", label_for(p))).as_ptr() as isize);
        }
        SendMessageW(nav, LB_SETCURSEL, 0, 0);
        SendMessageW(nav, WM_SETFONT, font as usize, 1);
        SendMessageW(banner, WM_SETFONT, font as usize, 1);
        SendMessageW(nav, LB_SETITEMHEIGHT, 0, mul(34, dpi) as isize);
        theme::apply_window(hwnd, dark);
        theme::apply_control(nav, dark);
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
