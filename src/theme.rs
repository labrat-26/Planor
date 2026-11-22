//! Windows theme helpers for the tray menu and the settings window: system light/dark, dark title bar and
//! Mica, dark classic controls, accent colour. Every OS call degrades silently to the classic look.
#![allow(dead_code)] // consumed by the tray and settings window (later tasks); removed in the final cleanup
use crate::wide;
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{DwmGetColorizationColor, DwmSetWindowAttribute},
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    },
    UI::Controls::SetWindowTheme,
};

const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
const BACKDROP_MAIN_WINDOW: i32 = 2; // Mica

/// `AppsUseLightTheme` registry value to "is the app theme light": missing means light.
pub fn parse_light_theme(value: Option<u32>) -> bool {
    value.is_none_or(|v| v != 0)
}
/// Whether apps should draw dark (Settings > Personalization > Colors > Choose your mode).
pub fn system_is_dark() -> bool {
    let mut data = 0u32;
    let mut len = 4u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr(),
            wide("AppsUseLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut len,
        )
    };
    !parse_light_theme((status == 0).then_some(data))
}

/// Let this process's menus and common controls follow the system theme (uxtheme ordinals 135 and 136,
/// undocumented; absent or failing means the classic light look).
pub fn enable_app_dark_mode() {
    unsafe {
        let lib = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        // 1 = AllowDark: follow the system setting rather than forcing dark.
        if let Some(set) = GetProcAddress(lib, 135usize as *const u8) {
            let set: extern "system" fn(i32) -> i32 = std::mem::transmute(set);
            set(1);
        }
        if let Some(flush) = GetProcAddress(lib, 136usize as *const u8) {
            let flush: extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

/// Dark or light title bar, plus Mica behind the title bar (the client area is painted opaque).
pub fn apply_window(hwnd: HWND, dark: bool) {
    unsafe {
        let on = dark as i32;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&on as *const i32).cast(), 4);
        DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&BACKDROP_MAIN_WINDOW as *const i32).cast(), 4);
    }
}
/// Give a classic control (button, edit, combo, list, scroll bar, tooltip) the dark or light Explorer theme.
pub fn apply_control(hwnd: HWND, dark: bool) {
    let theme = wide(if dark { "DarkMode_Explorer" } else { "Explorer" });
    unsafe { SetWindowTheme(hwnd, theme.as_ptr(), std::ptr::null()) };
}

/// DWM colorization `0xAARRGGBB` to a GDI `COLORREF` (`0x00BBGGRR`).
pub fn argb_to_colorref(argb: u32) -> u32 {
    let (r, g, b) = ((argb >> 16) & 0xFF, (argb >> 8) & 0xFF, argb & 0xFF);
    r | (g << 8) | (b << 16)
}
/// The user's accent colour as a `COLORREF` (Windows blue when unavailable).
pub fn accent_color() -> u32 {
    let (mut argb, mut opaque) = (0u32, 0i32);
    if unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) } < 0 {
        return rgb(0x00, 0x78, 0xD4);
    }
    argb_to_colorref(argb)
}

/// The app icon embedded as resource 1 (see `planor.rc`), loaded at `size` x `size` pixels.
pub fn app_icon(size: i32) -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    use windows_sys::Win32::{
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW},
    };
    unsafe { LoadImageW(GetModuleHandleW(std::ptr::null()), 1usize as *const u16, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as _ }
}

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub text: u32,
    pub subtext: u32,
    pub border: u32,
    pub error: u32,
}
/// Window colours matching Windows 11 Settings in light and dark.
pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: rgb(0x20, 0x20, 0x20),
            panel: rgb(0x2B, 0x2B, 0x2B),
            text: rgb(0xFF, 0xFF, 0xFF),
            subtext: rgb(0xA6, 0xA6, 0xA6),
            border: rgb(0x40, 0x40, 0x40),
            error: rgb(0xFF, 0x99, 0xA4),
        }
    } else {
        Palette {
            bg: rgb(0xF3, 0xF3, 0xF3),
            panel: rgb(0xFF, 0xFF, 0xFF),
            text: rgb(0x1A, 0x1A, 0x1A),
            subtext: rgb(0x5F, 0x5F, 0x5F),
            border: rgb(0xE0, 0xE0, 0xE0),
            error: rgb(0xC4, 0x2B, 0x1C),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: u32) -> f64 {
        let lin = |v: u32| {
            let s = v as f64 / 255.0;
            if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(c & 0xFF) + 0.7152 * lin((c >> 8) & 0xFF) + 0.0722 * lin((c >> 16) & 0xFF)
    }
    fn contrast(a: u32, b: u32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn parse_light_theme_defaults_to_light() {
        assert!(parse_light_theme(None));
        assert!(!parse_light_theme(Some(0)));
        assert!(parse_light_theme(Some(1)));
    }

    #[test]
    fn palette_text_contrasts_background() {
        for dark in [false, true] {
            let p = palette(dark);
            for (name, fg, bg) in [
                ("text/bg", p.text, p.bg),
                ("text/panel", p.text, p.panel),
                ("subtext/bg", p.subtext, p.bg),
                ("subtext/panel", p.subtext, p.panel),
                ("error/panel", p.error, p.panel),
            ] {
                assert!(contrast(fg, bg) >= 4.5, "dark={dark} {name}: {:.2}", contrast(fg, bg));
            }
        }
    }

    #[test]
    fn dwm_argb_becomes_colorref() {
        assert_eq!(argb_to_colorref(0xC40078D4), rgb(0x00, 0x78, 0xD4));
    }
}
