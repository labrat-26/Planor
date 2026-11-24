use super::*;
use windows_sys::Win32::{
    Devices::HumanInterfaceDevice::*,
    UI::Input::{KeyboardAndMouse::*, *},
};
pub fn bindings(hwnd: HWND, cfg: &toml::Value) -> Vec<String> {
    let mut out = vec![];
    for (name, value) in cfg["hotkeys"].as_table().unwrap() {
        let mut mods = MOD_NOREPEAT;
        let mut vk = 0;
        for token in value.as_str().unwrap_or("").split('+') {
            match token.to_ascii_uppercase().as_str() {
                "ALT" => mods |= MOD_ALT,
                "CTRL" => mods |= MOD_CONTROL,
                "SHIFT" => mods |= MOD_SHIFT,
                "WIN" => mods |= MOD_WIN,
                "LEFT" => vk = 0x25,
                "UP" => vk = 0x26,
                "RIGHT" => vk = 0x27,
                "DOWN" => vk = 0x28,
                "[" => vk = 0xdb,
                "]" => vk = 0xdd,
                "BACKSPACE" => vk = 8,
                s if s.len() == 1 => vk = s.as_bytes()[0] as u32,
                _ => {
                    vk = 0;
                    break;
                }
            }
        }
        if vk != 0
            && mods != MOD_NOREPEAT
            && unsafe { RegisterHotKey(hwnd, out.len() as i32 + 1, mods, vk) } != 0
        {
            out.push(name.clone())
        } else {
            // A modifier-less binding would swallow that key in every app; refuse it even if it
            // reached the config through a hand edit.
            eprintln!("hotkey unavailable: {name}")
        }
    }
    out
}
/// DPI of the monitor under the cursor. This used to be `GetDpiForWindow(WindowFromPoint(..))` for every
/// contact of every report; WindowFromPoint can wait on the window under the cursor (a heavy browser
/// mid-move), which stalled this thread and, through the mouse hook, all input. A monitor lookup never
/// talks to another process.
pub(crate) fn cursor_dpi() -> f64 {
    use windows_sys::Win32::{Graphics::Gdi::MonitorFromPoint, UI::HiDpi::GetDpiForMonitor};
    unsafe {
        let mut pt = POINT::default();
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        let monitor = MonitorFromPoint(pt, 2);
        let (mut x, mut y) = (96u32, 96u32);
        GetDpiForMonitor(monitor, 0, &mut x, &mut y);
        x.max(96) as f64
    }
}
/// While a gesture recording runs: pass 1-, 2- and 5-finger frames too (normally only 3 and 4 fingers reach the daemon).
pub static RECORD_ALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Physical DPI used to scale the most recent HID contact frame (`0` when the digitizer reported
/// raw units with no physical cm/inch descriptor, so the daemon must not divide by `cursor_dpi`).
static LAST_CONTACTS_DPI: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(96);

pub(crate) fn last_contacts_dpi() -> Option<f64> {
    let v = LAST_CONTACTS_DPI.load(std::sync::atomic::Ordering::Relaxed);
    (v > 0).then_some(v as f64)
}
thread_local!{static PARTS:std::cell::RefCell<std::collections::HashMap<usize,planor::ContactFrame>>=Default::default();}
pub fn contacts(l: isize) -> Vec<Vec<(f64, f64)>> {
    let _span = crate::perf::SpanGuard::new(crate::perf::SpanKind::InputContacts);
    let mut frames = vec![];
    unsafe {
        let hdr = size_of::<RAWINPUTHEADER>() as u32;
        let mut n = 0;
        GetRawInputData(l as _, RID_INPUT, null_mut(), &mut n, hdr);
        if n < hdr + 8 || n > 1_048_576 {
            return frames;
        }
        let mut data = vec![0usize; (n as usize).div_ceil(size_of::<usize>())];
        if GetRawInputData(l as _, RID_INPUT, data.as_mut_ptr().cast(), &mut n, hdr) != n {
            return frames;
        }
        let head = data.as_ptr().cast::<RAWINPUTHEADER>().read();
        if head.dwType != RIM_TYPEHID {
            return frames;
        }
        let bytes = std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), n as usize);
        let body = &bytes[hdr as usize..];
        let size = u32::from_ne_bytes(body[..4].try_into().unwrap()) as usize;
        let count = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
        if size == 0 || count > (body.len() - 8) / size {
            return frames;
        }
        let mut len = 0;
        GetRawInputDeviceInfoW(head.hDevice, RIDI_PREPARSEDDATA, null_mut(), &mut len);
        if len == 0 || len > 1_048_576 {
            return frames;
        }
        let mut desc = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        if GetRawInputDeviceInfoW(
            head.hDevice,
            RIDI_PREPARSEDDATA,
            desc.as_mut_ptr().cast(),
            &mut len,
        ) == u32::MAX
        {
            return frames;
        }
        let prep = desc.as_ptr() as isize;
        let mut caps = HIDP_CAPS::default();
        if HidP_GetCaps(prep, &mut caps) < 0 {
            return frames;
        }
        let dpi = cursor_dpi();
        for report in body[8..].chunks_exact(size).take(count) {
            let get = |page, link, usage| {
                let mut v = 0;
                (HidP_GetUsageValue(
                    HidP_Input,
                    page,
                    link,
                    usage,
                    &mut v,
                    prep,
                    report.as_ptr().cast_mut(),
                    size as u32,
                ) >= 0)
                    .then_some(v)
            };
            let mut points = vec![];
            let mut slots = 0usize;
            let all=RECORD_ALL.load(std::sync::atomic::Ordering::Relaxed);let count=get(13,0,0x54).unwrap_or(0);if count>=3||count==0||(all&&count>=1) {
                for link in 1..caps.NumberLinkCollectionNodes {
                    let (Some(x), Some(y)) = (get(1, link, 0x30), get(1, link, 0x31)) else {
                        continue;
                    };
                    // Count every finger contact collection in this report (tip-down and tip-up)
                    // so ContactFrame knows when a parallel or hybrid multi-report frame is complete.
                    slots += 1;
                    let mut usages = [0u16; 32];
                    let mut used = 32;
                    if HidP_GetUsages(
                        HidP_Input,
                        13,
                        link,
                        usages.as_mut_ptr(),
                        &mut used,
                        prep,
                        report.as_ptr().cast_mut(),
                        size as u32,
                    ) >= 0
                        && usages[..used as usize].contains(&0x42)
                    {
                        let mut has_physical_units = false;
                        let mut scale = |usage| {
                            let mut c = HIDP_VALUE_CAPS::default();
                            let mut n = 1;
                            if HidP_GetSpecificValueCaps(HidP_Input, 1, link, usage, &mut c, &mut n, prep) < 0
                                || c.LogicalMax <= c.LogicalMin
                                || c.PhysicalMax <= c.PhysicalMin
                            {
                                return 1.;
                            }
                            let exponent = (c.UnitsExp as i32 & 15) << 28 >> 28;
                            let units = match c.Units {
                                0x11 => {
                                    has_physical_units = true;
                                    dpi / 2.54
                                }
                                0x13 => {
                                    has_physical_units = true;
                                    dpi
                                }
                                _ => return 1.,
                            };
                            (c.PhysicalMax - c.PhysicalMin) as f64
                                / (c.LogicalMax - c.LogicalMin) as f64
                                * 10f64.powi(exponent)
                                * units
                        };
                        let sx = scale(0x30);
                        let sy = scale(0x31);
                        LAST_CONTACTS_DPI.store(
                            if has_physical_units { dpi.round().max(1.0) as u32 } else { 0 },
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        points.push((get(13, link, 0x51).unwrap_or(link as u32), (x as f64 * sx, y as f64 * sy)));
                    }
                }
            }
            if let Some(frame)=PARTS.with(|v|{let mut m=v.borrow_mut();let f=m.entry(head.hDevice as usize).or_default();f.all=all;f.packet_with_slots(count,slots,points)}){frames.push(frame)}
        }
    }
    frames
}
