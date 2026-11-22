//! The recorder's single clocked event stream: input, OS window snapshots and markers.
//!
//! Text keys are only ever counted. `named_key` lists the non-text keys the design allows to be
//! named: modifiers, arrows, F-keys, Enter, Backspace, Esc, Tab and Win. Anything else that is not
//! listed is counted in a `text` event, without its code or the character typed.

use crate::util::{qpc_us, Rect};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS},
    UI::{
        Input::KeyboardAndMouse::GetAsyncKeyState,
        WindowsAndMessaging::{
            EnumWindows, GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowRect,
            IsIconic, IsWindowVisible,
        },
    },
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OsWindow {
    pub hwnd: i64,
    pub class: String,
    pub rect: Rect,
    /// 0 is the top of the z-order.
    pub z: i64,
    pub cloaked: bool,
    pub foreground: bool,
    pub minimized: bool,
}

/// Appends JSON lines from several threads. One file handle behind a mutex; every line is flushed so
/// a crash leaves a usable event stream.
pub struct EventWriter {
    file: Mutex<Option<BufWriter<std::fs::File>>>,
}

impl EventWriter {
    pub fn open(dir: &std::path::Path) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Ok(Arc::new(Self {
            file: Mutex::new(Some(BufWriter::new(file))),
        }))
    }

    /// Write one event. `t` is a QPC microsecond stamp; missing `t` is stamped now.
    pub fn write(&self, mut event: Value) {
        if event.get("t").is_none() {
            event["t"] = json!(qpc_us());
        }
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = serde_json::to_writer(&mut *file, &event);
                let _ = file.write_all(b"\n");
                let _ = file.flush();
            }
        }
    }

    pub fn mark(&self, text: &str) {
        self.write(json!({"kind": "mark", "text": text}));
    }

    pub fn close(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(mut file) = guard.take() {
                let _ = file.flush();
            }
        }
    }
}

// ---- Input -------------------------------------------------------------------------------------

const MOUSE_KEYS: [i32; 3] = [0x01, 0x02, 0x04];

/// A key that may be named in the recording. Text-producing keys deliberately return `None`.
pub fn named_key(vk: i32) -> Option<&'static str> {
    Some(match vk {
        0x10 => "shift",
        0xA0 => "lshift",
        0xA1 => "rshift",
        0x11 => "ctrl",
        0xA2 => "lctrl",
        0xA3 => "rctrl",
        0x12 => "alt",
        0xA4 => "lalt",
        0xA5 => "ralt",
        0x5B => "lwin",
        0x5C => "rwin",
        0x25 => "left",
        0x26 => "up",
        0x27 => "right",
        0x28 => "down",
        0x0D => "enter",
        0x08 => "backspace",
        0x1B => "esc",
        0x09 => "tab",
        0x70 => "f1",
        0x71 => "f2",
        0x72 => "f3",
        0x73 => "f4",
        0x74 => "f5",
        0x75 => "f6",
        0x76 => "f7",
        0x77 => "f8",
        0x78 => "f9",
        0x79 => "f10",
        0x7A => "f11",
        0x7B => "f12",
        0x7C => "f13",
        0x7D => "f14",
        0x7E => "f15",
        0x7F => "f16",
        0x80 => "f17",
        0x81 => "f18",
        0x82 => "f19",
        0x83 => "f20",
        0x84 => "f21",
        0x85 => "f22",
        0x86 => "f23",
        0x87 => "f24",
        _ => return None,
    })
}

/// True for a virtual-key code whose press may be counted but never named.
pub fn is_text_key(vk: i32) -> bool {
    (0x08..=0xFE).contains(&vk) && !MOUSE_KEYS.contains(&vk) && named_key(vk).is_none()
}

fn down(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

pub fn spawn_input(writer: Arc<EventWriter>, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut cursor = (i32::MIN, i32::MIN);
        let mut buttons = [false; 3];
        let mut keys = vec![false; 0x100];
        let mut text_count_total: u64 = 0;
        while !stop.load(Ordering::Relaxed) {
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point) } != 0 && (point.x, point.y) != cursor {
                cursor = (point.x, point.y);
                writer.write(json!({
                    "kind": "input", "input": "mouse", "x": point.x, "y": point.y
                }));
            }
            for (i, vk) in MOUSE_KEYS.iter().enumerate() {
                let now = down(*vk);
                if now != buttons[i] {
                    buttons[i] = now;
                    let name = ["left", "right", "middle"][i];
                    writer.write(json!({
                        "kind": "input", "input": "button", "button": name, "down": now
                    }));
                }
            }
            let mut text_down = 0u64;
            for vk in 0x08..=0xFEi32 {
                if MOUSE_KEYS.contains(&vk) {
                    continue;
                }
                let now = down(vk);
                let was = keys[vk as usize];
                if now == was {
                    continue;
                }
                keys[vk as usize] = now;
                if let Some(name) = named_key(vk) {
                    writer.write(json!({
                        "kind": "input", "input": "key", "key": name, "down": now
                    }));
                } else if now && is_text_key(vk) {
                    text_down += 1;
                }
            }
            if text_down > 0 {
                text_count_total += text_down;
                // No key code and no character: only the fact that typing happened.
                writer.write(json!({
                    "kind": "input", "input": "text", "count": text_down,
                    "total": text_count_total
                }));
            }
            thread::sleep(Duration::from_millis(16));
        }
    })
}

// ---- OS windows --------------------------------------------------------------------------------

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn extended_rect(hwnd: HWND) -> Option<Rect> {
    unsafe {
        let mut r = RECT::default();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        );
        if hr >= 0 {
            return Some(Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r) != 0 {
            Some(Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top))
        } else {
            None
        }
    }
}

fn cloaked(hwnd: HWND) -> bool {
    unsafe {
        let mut value: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut value as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        hr >= 0 && value != 0
    }
}

/// Every visible top-level window, top of the z-order first.
pub fn sample_windows() -> Vec<OsWindow> {
    struct State {
        out: Vec<OsWindow>,
        foreground: HWND,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: isize) -> i32 {
        let state = unsafe { &mut *(lparam as *mut State) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let class = class_name(hwnd);
        if class.is_empty() {
            return 1;
        }
        if let Some(rect) = extended_rect(hwnd) {
            if rect.is_empty() {
                return 1;
            }
            let z = state.out.len() as i64;
            state.out.push(OsWindow {
                hwnd: hwnd as i64,
                class,
                rect,
                z,
                cloaked: cloaked(hwnd),
                foreground: hwnd == state.foreground,
                minimized: unsafe { IsIconic(hwnd) } != 0,
            });
        }
        1
    }
    let mut state = State {
        out: Vec::new(),
        foreground: unsafe { GetForegroundWindow() },
    };
    unsafe { EnumWindows(Some(cb), &mut state as *mut State as isize) };
    state.out.sort_by_key(|w| w.z);
    state.out
}

pub fn write_os_snapshot(writer: &EventWriter, windows: &[OsWindow]) {
    writer.write(json!({
        "kind": "os",
        "windows": windows,
    }));
}

pub fn spawn_os(writer: Arc<EventWriter>, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        crate::capture::dpi_aware();
        let mut last: Vec<OsWindow> = Vec::new();
        let mut first = true;
        let mut last_emit = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let windows = sample_windows();
            // On change, and at least once a second so a long recording has a recent snapshot.
            if first || windows != last || last_emit.elapsed() >= Duration::from_secs(1) {
                write_os_snapshot(&writer, &windows);
                last = windows;
                first = false;
                last_emit = std::time::Instant::now();
            }
            thread::sleep(Duration::from_millis(33));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_keys_are_never_named() {
        for vk in [
            b'A' as i32,
            b'Z' as i32,
            b'a' as i32,
            b'0' as i32,
            b'9' as i32,
            0x20, // space types a character
            0xBA, // semicolon
            0xBD, // minus
            0xDE, // quote
        ] {
            assert_eq!(named_key(vk), None, "vk {vk:#x} must not be named");
            assert!(is_text_key(vk));
        }
        // No letter or digit is ever emitted by name.
        for vk in (0x30..=0x5A).chain(0x08..=0xFE) {
            if let Some(name) = named_key(vk) {
                assert!(
                    !name.chars().any(|c| c.is_ascii_alphanumeric() && c.is_ascii_uppercase()),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn allowed_keys_are_named() {
        for (vk, name) in [
            (0x10, "shift"),
            (0x11, "ctrl"),
            (0x12, "alt"),
            (0x25, "left"),
            (0x26, "up"),
            (0x27, "right"),
            (0x28, "down"),
            (0x0D, "enter"),
            (0x08, "backspace"),
            (0x1B, "esc"),
            (0x09, "tab"),
            (0x5B, "lwin"),
            (0x7B, "f12"),
            (0x87, "f24"),
        ] {
            assert_eq!(named_key(vk), Some(name));
        }
    }

    #[test]
    fn text_event_carries_no_key_code() {
        let event = json!({"kind": "input", "input": "text", "count": 2, "total": 2});
        let text = event.to_string();
        assert!(!text.contains("\"key\""));
        assert!(!text.contains("\"char\""));
        assert!(!text.contains("0x"));
    }
}
