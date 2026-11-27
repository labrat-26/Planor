//! DXGI desktop duplication and the Windows monitor/taskbar inventory the recorder writes to
//! `meta.json`. The capture code follows `tests/common/helpers.rs` (`Capture`).

use crate::util::Rect;
use serde::{Deserialize, Serialize};
use windows::{
    Win32::{
        Foundation as F,
        Graphics::{Direct3D::*, Direct3D11::*, Dxgi::Common::*, Dxgi::*},
    },
    core::Interface,
};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::{EnumWindows, GetClassNameW, IsWindowVisible, MONITORINFOF_PRIMARY},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// DXGI output index; also the `-<monitor>` file suffix.
    pub index: u32,
    /// `\\.\DISPLAYn` from DXGI.
    pub device: String,
    pub rect: Rect,
    pub work: Rect,
    pub dpi: u32,
    pub primary: bool,
}

/// Per-monitor DPI awareness for any thread that reads window rectangles.
pub fn dpi_aware() {
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

fn wide_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut dev = None;
        let mut ctx = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            F::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )?;
        Ok((dev.unwrap(), ctx.unwrap()))
    }
}

/// Enumerate DXGI outputs with their desktop rectangle, work area and DPI.
pub fn monitors() -> windows::core::Result<Vec<MonitorInfo>> {
    unsafe {
        let (dev, _ctx) = create_device()?;
        let dx: IDXGIDevice = dev.cast()?;
        let adapter = dx.GetAdapter()?;
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let output = match adapter.EnumOutputs(index) {
                Ok(o) => o,
                Err(_) => break,
            };
            let desc = output.GetDesc()?;
            let r = desc.DesktopCoordinates;
            let rect = Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top);
            let point = POINT {
                x: rect.x + rect.w / 2,
                y: rect.y + rect.h / 2,
            };
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let mon = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let mut work = rect;
            let mut primary = false;
            if !mon.is_null() && GetMonitorInfoW(mon, &mut info) != 0 {
                work = Rect::new(
                    info.rcWork.left,
                    info.rcWork.top,
                    info.rcWork.right - info.rcWork.left,
                    info.rcWork.bottom - info.rcWork.top,
                );
                primary = info.dwFlags & MONITORINFOF_PRIMARY != 0;
            }
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            if !mon.is_null() {
                let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            }
            out.push(MonitorInfo {
                index,
                device: wide_string(&desc.DeviceName),
                rect,
                work,
                dpi: dpi_x,
                primary,
            });
            index += 1;
        }
        Ok(out)
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn window_rect(hwnd: HWND) -> Option<Rect> {
    unsafe {
        let mut r = RECT::default();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        );
        if hr >= 0 {
            Some(Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top))
        } else {
            None
        }
    }
}

/// Rectangles of the primary and secondary taskbars (DWM frame bounds, physical pixels).
pub fn taskbar_rects() -> Vec<Rect> {
    struct State {
        out: Vec<Rect>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: isize) -> i32 {
        let state = unsafe { &mut *(lparam as *mut State) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let class = class_name(hwnd);
        if class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd" {
            if let Some(r) = window_rect(hwnd) {
                if !r.is_empty() {
                    state.out.push(r);
                }
            }
        }
        1
    }
    let mut state = State { out: Vec::new() };
    unsafe { EnumWindows(Some(cb), &mut state as *mut State as isize) };
    state.out
}

/// The monitor holding most of `rect`, if any.
pub fn monitor_of(monitors: &[MonitorInfo], rect: Rect) -> Option<usize> {
    let mut best: Option<(usize, i64)> = None;
    for (i, m) in monitors.iter().enumerate() {
        let o = rect.overlap_area(m.rect);
        if o > 0 && best.is_none_or(|(_, b)| o > b) {
            best = Some((i, o));
        }
    }
    best.map(|(i, _)| i)
}

/// One live desktop duplication. `acquire` returns the current frame as tightly packed BGRA with
/// the mouse pointer composited (DXGI reports the pointer separately and does not draw it into the
/// desktop image).
pub struct Duplicator {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    pointer: Option<PointerShape>,
    pub width: u32,
    pub height: u32,
    pub origin: (i32, i32),
    /// How the desktop is rotated relative to the frames duplication delivers (a rotated monitor).
    rotation: i32,
    /// Microseconds spent in the last `acquire_into`: `AcquireNextFrame`, GPU copy + `Map`, CPU copy/rotate.
    pub phases_us: [i64; 4],
    /// Reused copy of a rotated monitor's frame (rotation reads this cached copy, not the mapped GPU memory).
    native: Vec<u8>,
    /// Recorder time after `AcquireNextFrame` returned the last frame, when that frame was rotated (GPU copy,
    /// CPU copy and rotation); zero for an unrotated frame and for a call that returned no frame.
    rotate_us: i64,
}

struct PointerShape {
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hotspot: (i32, i32),
    data: Vec<u8>,
}

/// Composite the duplicated mouse pointer into a tightly packed BGRA frame. Separate from the
/// device so the blending rules are unit-testable.
fn composite_pointer(
    out: &mut [u8],
    w: usize,
    h: usize,
    origin: (i32, i32),
    pos: (i32, i32),
    visible: bool,
    pointer: Option<&PointerShape>,
) {
    if !visible {
        return;
    }
    let Some(pointer) = pointer else {
        return;
    };
    // The duplication API has used both desktop and output-relative coordinates across
    // versions; accept whichever interpretation lands on this monitor.
    let mut local_x = pos.0 - origin.0;
    let mut local_y = pos.1 - origin.1;
    if local_x < -64 || local_y < -64 || local_x > w as i32 + 64 || local_y > h as i32 + 64 {
        local_x = pos.0;
        local_y = pos.1;
    }
    if local_x < -64 || local_y < -64 || local_x > w as i32 + 64 || local_y > h as i32 + 64 {
        return;
    }
    let left = local_x - pointer.hotspot.0;
    let top = local_y - pointer.hotspot.1;
    let mono_height = if pointer.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        pointer.height / 2
    } else {
        pointer.height
    };
    for sy in 0..mono_height as i32 {
        let dy = top + sy;
        if dy < 0 || dy >= h as i32 {
            continue;
        }
        for sx in 0..pointer.width as i32 {
            let dx = left + sx;
            if dx < 0 || dx >= w as i32 {
                continue;
            }
            let di = ((dy as usize) * w + dx as usize) * 4;
            let kind = pointer.kind;
            if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32
                || kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32
            {
                let si = (sy as u32 * pointer.pitch + sx as u32 * 4) as usize;
                if si + 3 >= pointer.data.len() {
                    continue;
                }
                let b = pointer.data[si] as u32;
                let g = pointer.data[si + 1] as u32;
                let r = pointer.data[si + 2] as u32;
                let a = pointer.data[si + 3] as u32;
                if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                    // XOR blend, alpha ignored.
                    out[di] ^= b as u8;
                    out[di + 1] ^= g as u8;
                    out[di + 2] ^= r as u8;
                } else if a > 0 {
                    if a >= 255 {
                        out[di] = b as u8;
                        out[di + 1] = g as u8;
                        out[di + 2] = r as u8;
                    } else {
                        let inv = 255 - a;
                        out[di] = ((b * a + out[di] as u32 * inv) / 255) as u8;
                        out[di + 1] = ((g * a + out[di + 1] as u32 * inv) / 255) as u8;
                        out[di + 2] = ((r * a + out[di + 2] as u32 * inv) / 255) as u8;
                    }
                }
            } else {
                // 1bpp monochrome: AND mask rows followed by XOR mask rows, MSB first.
                let byte_index = (sy as u32 * pointer.pitch + (sx as u32) / 8) as usize;
                let bit = 7 - (sx as u32 % 8);
                let xor_index = byte_index + (mono_height * pointer.pitch) as usize;
                if byte_index >= pointer.data.len() {
                    continue;
                }
                let and = (pointer.data[byte_index] >> bit) & 1;
                let xor = if xor_index < pointer.data.len() {
                    (pointer.data[xor_index] >> bit) & 1
                } else {
                    0
                };
                match (and, xor) {
                    (1, 0) => {}
                    (1, 1) => {
                        out[di] = 255 - out[di];
                        out[di + 1] = 255 - out[di + 1];
                        out[di + 2] = 255 - out[di + 2];
                    }
                    (0, 0) => {
                        out[di] = 0;
                        out[di + 1] = 0;
                        out[di + 2] = 0;
                    }
                    _ => {
                        out[di] = 255;
                        out[di + 1] = 255;
                        out[di + 2] = 255;
                    }
                }
            }
        }
    }
}

impl Duplicator {
    pub fn open(index: u32) -> windows::core::Result<Self> {
        Self::open_by_device(None, index)
    }

    /// Open the duplication of the output named `device` (`\\.\DISPLAYn`); when no output has that
    /// name (or `device` is `None`) the output at `index` is used. DXGI output indices can be
    /// renumbered when a monitor drops out and returns, the device name is what stays with a panel.
    pub fn open_by_device(device: Option<&str>, index: u32) -> windows::core::Result<Self> {
        unsafe {
            let (dev, ctx) = create_device()?;
            let dx: IDXGIDevice = dev.cast()?;
            let adapter = dx.GetAdapter()?;
            let mut chosen = None;
            if let Some(name) = device {
                let mut i = 0u32;
                while let Ok(o) = adapter.EnumOutputs(i) {
                    if let Ok(desc) = o.GetDesc() {
                        if wide_string(&desc.DeviceName) == name {
                            chosen = Some(o);
                            break;
                        }
                    }
                    i += 1;
                }
            }
            let output = match chosen {
                Some(o) => o,
                None => adapter.EnumOutputs(index)?,
            };
            let out: IDXGIOutput1 = output.cast()?;
            let r = out.GetDesc()?.DesktopCoordinates;
            let dupl = out.DuplicateOutput(&dev)?;
            let desc = dupl.GetDesc();
            if desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                && desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
            {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    format!(
                        "monitor {} uses DXGI format {:?}, not 8-bit BGRA (HDR is not supported by this recorder; turn HDR off)",
                        index, desc.ModeDesc.Format
                    ),
                ));
            }
            Ok(Self {
                dev,
                ctx,
                dupl,
                pointer: None,
                width: desc.ModeDesc.Width,
                height: desc.ModeDesc.Height,
                origin: (r.left, r.top),
                rotation: desc.Rotation.0,
                phases_us: [0; 4],
                native: Vec::new(),
                rotate_us: 0,
            })
        }
    }

    fn capture_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.PointerShapeBufferSize == 0 {
            return; // shape unchanged
        }
        unsafe {
            let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut required = 0u32;
            let result = self.dupl.GetFramePointerShape(
                data.len() as u32,
                data.as_mut_ptr().cast(),
                &mut required,
                &mut shape,
            );
            if result.is_ok() {
                data.truncate((required as usize).min(data.len()));
                self.pointer = Some(PointerShape {
                    kind: shape.Type,
                    width: shape.Width,
                    height: shape.Height,
                    pitch: shape.Pitch,
                    hotspot: (shape.HotSpot.x, shape.HotSpot.y),
                    data,
                });
            }
        }
    }

    /// Wait up to `timeout_ms` for a new frame. `Ok(None)` means nothing changed in time.
    pub fn acquire(&mut self, timeout_ms: u32) -> windows::core::Result<Option<Vec<u8>>> {
        let mut out = Vec::new();
        if self.acquire_into(&mut out, timeout_ms)? {
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    /// Same as `acquire`, but reuses `out` to avoid allocating a multi-megabyte frame per tick.
    /// Returns true when a new frame was copied.
    pub fn acquire_into(
        &mut self,
        out: &mut Vec<u8>,
        timeout_ms: u32,
    ) -> windows::core::Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            let t_start = std::time::Instant::now();
            self.rotate_us = 0;
            let acquired = self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut res);
            let acquire_us = t_start.elapsed().as_micros() as i64;
            self.phases_us = [acquire_us, 0, 0, 0];
            match acquired {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) => return Err(e),
            }
            self.capture_pointer(&info);
            let mut phases = [acquire_us, 0i64, 0i64, 0i64];
            let mut rotated = false;
            let result = (|| {
                let tex: ID3D11Texture2D = res.unwrap().cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let swapped = desc.Width == self.height
                    && desc.Height == self.width
                    && self.width != self.height;
                rotated = swapped;
                if !swapped && (desc.Width != self.width || desc.Height != self.height) {
                    return Err(windows::core::Error::new(
                        windows::Win32::Foundation::E_FAIL,
                        format!(
                            "duplication frame is {}x{}, expected {}x{}",
                            desc.Width, desc.Height, self.width, self.height
                        ),
                    ));
                }
                desc.Usage = D3D11_USAGE_STAGING;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as _;
                desc.BindFlags = 0;
                desc.MiscFlags = 0;
                let t_gpu = std::time::Instant::now();
                let mut copy = None;
                self.dev.CreateTexture2D(&desc, None, Some(&mut copy))?;
                let copy = copy.unwrap();
                self.ctx.CopyResource(&copy, &tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                phases[1] = t_gpu.elapsed().as_micros() as i64;
                let t_cpu = std::time::Instant::now();
                let w = desc.Width as usize;
                let h = desc.Height as usize;
                out.resize(w * h * 4, 0);
                if swapped {
                    // A rotated monitor: duplication hands over the panel's own orientation; turn it into the
                    // orientation of the desktop (width and height swap).
                    // Read the mapped GPU memory once, row by row (a sequential copy, like the unrotated path),
                    // into a buffer that is kept between frames, and rotate from that cached copy. Rotating
                    // straight out of the mapped (uncached) memory, or allocating a fresh 15 MB buffer per
                    // frame, is what cost 30 to 105 ms per frame under recording load.
                    self.native.resize(w * h * 4, 0);
                    for y in 0..h {
                        let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                        std::ptr::copy_nonoverlapping(
                            src,
                            self.native[y * w * 4..].as_mut_ptr(),
                            w * 4,
                        );
                    }
                    phases[2] = t_cpu.elapsed().as_micros() as i64;
                    let t_rot = std::time::Instant::now();
                    rotate_strided(&self.native, w * 4, w, h, self.rotation, out);
                    phases[3] = t_rot.elapsed().as_micros() as i64;
                } else {
                    for y in 0..h {
                        let src = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
                        std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w * 4);
                    }
                }
                self.ctx.Unmap(&copy, 0);
                if !swapped {
                    phases[2] = t_cpu.elapsed().as_micros() as i64;
                }
                Ok(true)
            })();
            self.phases_us = phases;
            // Everything after AcquireNextFrame returned is the stall the frame's timestamp carries (the
            // timestamp is taken after this call). Only a rotated frame has the CPU rotation in it.
            self.rotate_us = if rotated && result.is_ok() {
                phases[1] + phases[2] + phases[3]
            } else {
                0
            };
            let _ = self.dupl.ReleaseFrame();
            if result.is_ok() {
                let position = info.PointerPosition.Position;
                let visible = info.PointerPosition.Visible.as_bool();
                composite_pointer(
                    out,
                    self.width as usize,
                    self.height as usize,
                    self.origin,
                    (position.x, position.y),
                    visible,
                    self.pointer.as_ref(),
                );
            }
            result
        }
    }
}

impl crate::timeline::FrameSource for Duplicator {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn rect(&self) -> Rect {
        Rect::new(
            self.origin.0,
            self.origin.1,
            self.width as i32,
            self.height as i32,
        )
    }
    fn phases_us(&self) -> Option<[i64; 4]> {
        Some(self.phases_us)
    }
    fn rotate_stall_us(&self) -> i64 {
        self.rotate_us
    }
    fn acquire_into(&mut self, out: &mut Vec<u8>, timeout_ms: u32) -> Result<bool, String> {
        Duplicator::acquire_into(self, out, timeout_ms).map_err(|e| {
            // `{e}` carries the HRESULT, e.g. DXGI_ERROR_ACCESS_LOST (0x887A0026) after a mode change,
            // a monitor going off and on, a UAC prompt or a driver reset.
            format!("{e} [0x{:08X}]", e.code().0 as u32)
        })
    }
}

/// Reopens one monitor's duplication, by device name first.
pub struct DxgiOpener {
    pub device: String,
    pub index: u32,
}

impl crate::timeline::Opener for DxgiOpener {
    fn open(&mut self) -> Result<Box<dyn crate::timeline::FrameSource>, String> {
        Duplicator::open_by_device(Some(&self.device), self.index)
            .map(|d| Box::new(d) as Box<dyn crate::timeline::FrameSource>)
            .map_err(|e| format!("{e} [0x{:08X}]", e.code().0 as u32))
    }
}

/// Rotate a `w` x `h` BGRA frame by a quarter turn into `out` (`h` x `w`). `rotation` is the DXGI mode
/// rotation of the desktop: 2 = 90, 4 = 270 (clockwise); 3 and 1 never swap the size.
pub fn rotate_into(src: &[u8], w: usize, h: usize, rotation: i32, out: &mut [u8]) {
    rotate_strided(src, w * 4, w, h, rotation, out);
}

/// `rotate_into` for a source whose rows are `pitch` bytes apart (a mapped GPU texture), done in 32x32 tiles
/// so both the reads and the writes stay in cache. A per-pixel loop with a strided write cost 30 to 67 ms
/// per 1440x2560 frame while three encoders loaded the CPU (it stalled the capture loop and showed up as
/// 60 to 98 ms pacing gaps on the rotated monitor); the tiled version is a few ms.
pub fn rotate_strided(src: &[u8], pitch: usize, w: usize, h: usize, rotation: i32, out: &mut [u8]) {
    // Big frames are split into bands of source rows over a few threads: even tiled, one thread needed 20 to
    // 116 ms per 1440x2560 frame while the machine was busy encoding and rendering, and the tick loop stalls
    // for as long as the rotation takes.
    let threads = if w * h >= 1 << 20 { 4 } else { 1 };
    rotate_with_threads(src, pitch, w, h, rotation, out, threads);
}

/// Raw pointer that may be shared between the rotation threads (they write disjoint pixels).
struct SharedOut(*mut u8);
unsafe impl Send for SharedOut {}
unsafe impl Sync for SharedOut {}

pub fn rotate_with_threads(
    src: &[u8],
    pitch: usize,
    w: usize,
    h: usize,
    rotation: i32,
    out: &mut [u8],
    threads: usize,
) {
    assert!(w > 0 && h > 0 && pitch >= w * 4, "bad rotation geometry");
    assert!(src.len() >= pitch * (h - 1) + w * 4, "source too small");
    assert!(out.len() >= w * h * 4, "output too small");
    let threads = threads.clamp(1, 16);
    // Bands start on tile boundaries so two threads never share a source tile.
    let band = h.div_ceil(threads).next_multiple_of(32);
    let shared = SharedOut(out.as_mut_ptr());
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (y0, y1) = (t * band, ((t + 1) * band).min(h));
            if y0 >= y1 {
                continue;
            }
            let shared = &shared;
            // SAFETY: every source pixel (x, y) maps to a distinct output pixel inside `out` (checked sizes
            // above), and each thread handles different source rows, so no two threads write the same bytes.
            scope.spawn(move || {
                crate::util::raise_current_thread_priority();
                unsafe { rotate_rows(src, pitch, w, h, rotation, shared.0, y0, y1) }
            });
        }
    });
}

/// Rotate the source rows `y0..y1` in 32x32 tiles, without bounds checks in the inner loop.
///
/// # Safety
/// `out` must have room for `w * h * 4` bytes, `src` for `pitch * (h - 1) + w * 4`, `y1 <= h`.
unsafe fn rotate_rows(
    src: &[u8],
    pitch: usize,
    w: usize,
    h: usize,
    rotation: i32,
    out: *mut u8,
    y0: usize,
    y1: usize,
) {
    const T: usize = 32;
    let src = src.as_ptr();
    for ty in (y0..y1).step_by(T) {
        for tx in (0..w).step_by(T) {
            for y in ty..(ty + T).min(y1) {
                let row = unsafe { src.add(y * pitch) };
                for x in tx..(tx + T).min(w) {
                    let (dx, dy) = if rotation == 4 {
                        (y, w - 1 - x)
                    } else {
                        (h - 1 - y, x)
                    };
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            row.add(x * 4),
                            out.add((dy * h + dx) * 4),
                            4,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize) -> Vec<u8> {
        vec![10u8; w * h * 4]
    }

    fn color_pointer(w: u32, h: u32, pixels: &[[u8; 4]]) -> PointerShape {
        PointerShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
            width: w,
            height: h,
            pitch: w * 4,
            hotspot: (0, 0),
            data: pixels.iter().flatten().copied().collect(),
        }
    }

    #[test]
    fn composites_an_opaque_colour_pointer() {
        let mut out = frame(20, 20);
        let shape = color_pointer(2, 2, &[[0, 0, 255, 255]; 4]);
        composite_pointer(&mut out, 20, 20, (0, 0), (5, 5), true, Some(&shape));
        let i = (5 * 20 + 5) * 4;
        assert_eq!(&out[i..i + 3], &[0, 0, 255]);
        // Outside the pointer nothing changed.
        assert_eq!(out[0], 10);
    }

    #[test]
    fn blends_a_translucent_pointer() {
        let mut out = frame(20, 20);
        let shape = color_pointer(1, 1, &[[0, 0, 255, 128]]);
        composite_pointer(&mut out, 20, 20, (0, 0), (3, 3), true, Some(&shape));
        let i = (3 * 20 + 3) * 4;
        assert!(out[i + 2] > 100 && out[i + 2] < 160, "{:?}", &out[i..i + 4]);
    }

    #[test]
    fn accepts_output_relative_and_desktop_pointer_coordinates() {
        // Desktop coordinates with a negative-origin monitor.
        let mut out = frame(30, 30);
        let shape = color_pointer(1, 1, &[[0, 255, 0, 255]]);
        composite_pointer(
            &mut out,
            30,
            30,
            (-1440, -1859),
            (-1435, -1855),
            true,
            Some(&shape),
        );
        let i = (4 * 30 + 5) * 4;
        assert_eq!(out[i + 1], 255);
        // Output-relative coordinates fall back to the raw value.
        let mut out = frame(30, 30);
        composite_pointer(&mut out, 30, 30, (-1440, -1859), (5, 4), true, Some(&shape));
        let i = (4 * 30 + 5) * 4;
        assert_eq!(out[i + 1], 255);
    }

    #[test]
    fn composites_a_monochrome_pointer() {
        let mut out = frame(20, 20);
        // One row: AND mask 0b1001_0000, XOR mask 0b0011_0000.
        // sx0 transparent, sx1 black, sx2 white, sx3 invert.
        let shape = PointerShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32,
            width: 8,
            height: 2,
            pitch: 1,
            hotspot: (0, 0),
            data: vec![0b1001_0000, 0b0011_0000],
        };
        composite_pointer(&mut out, 20, 20, (0, 0), (2, 2), true, Some(&shape));
        let p = |x: usize| {
            let i = (2 * 20 + x) * 4;
            out[i]
        };
        assert_eq!(p(2), 10, "transparent");
        assert_eq!(p(3), 0, "black");
        assert_eq!(p(4), 255, "white");
        assert_eq!(p(5), 245, "inverted background");
    }

    #[test]
    fn hidden_pointer_draws_nothing() {
        let mut out = frame(20, 20);
        let shape = color_pointer(1, 1, &[[0, 0, 255, 255]]);
        composite_pointer(&mut out, 20, 20, (0, 0), (5, 5), false, Some(&shape));
        assert!(out.iter().all(|&v| v == 10));
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    #[test]
    fn quarter_turns_move_corners_correctly() {
        // 3 wide x 2 high frame, pixel value = its index; BGRA with the index in the first byte.
        let (w, h) = (3usize, 2usize);
        let mut src = vec![0u8; w * h * 4];
        for i in 0..w * h {
            src[i * 4] = i as u8;
        }
        let px = |row: usize, col: usize| (row * 2 + col) * 4;
        let mut cw = vec![0u8; w * h * 4];
        rotate_into(&src, w, h, 2, &mut cw);
        // Clockwise: the top-left pixel (0) ends up top-right of the 2 wide x 3 high result; the
        // top-right pixel (2) bottom-right.
        assert_eq!(cw[px(0, 1)], 0);
        assert_eq!(cw[px(2, 1)], 2);
        let mut ccw = vec![0u8; w * h * 4];
        rotate_into(&src, w, h, 4, &mut ccw);
        // Counter-clockwise: the top-left pixel (0) ends up bottom-left; top-right (2) top-left.
        assert_eq!(ccw[px(2, 0)], 0);
        assert_eq!(ccw[px(0, 0)], 2);
    }

    /// The pre-tiling reference: one pixel at a time.
    fn rotate_naive(src: &[u8], w: usize, h: usize, rotation: i32, out: &mut [u8]) {
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = if rotation == 4 {
                    (y, w - 1 - x)
                } else {
                    (h - 1 - y, x)
                };
                let d = (dy * h + dx) * 4;
                let s = (y * w + x) * 4;
                out[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
    }

    #[test]
    fn the_tiled_rotation_equals_the_naive_one_for_sizes_that_do_not_divide_into_tiles() {
        // 75 x 41 spans partial 32 px tiles in both directions; a mapped texture has padded rows.
        let (w, h, pitch) = (75usize, 41usize, 75 * 4 + 52);
        let mut padded = vec![0xEEu8; pitch * h];
        let mut tight = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let px = [(x * 3 + y) as u8, (x + y * 5) as u8, (x ^ y) as u8, 255];
                padded[y * pitch + x * 4..y * pitch + x * 4 + 4].copy_from_slice(&px);
                tight[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
            }
        }
        for rotation in [2, 4] {
            let mut want = vec![0u8; w * h * 4];
            rotate_naive(&tight, w, h, rotation, &mut want);
            let mut got = vec![1u8; w * h * 4];
            rotate_strided(&padded, pitch, w, h, rotation, &mut got);
            assert!(
                got == want,
                "rotation {rotation}: tiled with pitch differs from the naive rotation"
            );
            let mut got2 = vec![2u8; w * h * 4];
            rotate_into(&tight, w, h, rotation, &mut got2);
            assert!(got2 == want, "rotation {rotation}: rotate_into differs");
            // Banded over several threads (bands of 32 rows: 41 rows = two bands, the rest idle).
            for threads in [2, 3, 4, 16] {
                let mut got3 = vec![3u8; w * h * 4];
                rotate_with_threads(&padded, pitch, w, h, rotation, &mut got3, threads);
                assert!(
                    got3 == want,
                    "rotation {rotation}: {threads} threads differ"
                );
            }
        }
    }

    #[test]
    fn the_threaded_rotation_equals_the_naive_one_on_a_frame_with_many_bands() {
        let (w, h) = (130usize, 200usize); // 200 rows: 4 threads get 64-row bands (the last one 8 rows)
        let mut src = vec![0u8; w * h * 4];
        for (i, b) in src.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2654435761) >> 7) as u8;
        }
        for rotation in [2, 4] {
            let mut want = vec![0u8; w * h * 4];
            rotate_naive(&src, w, h, rotation, &mut want);
            let mut got = vec![9u8; w * h * 4];
            rotate_with_threads(&src, w * 4, w, h, rotation, &mut got, 4);
            assert!(got == want, "rotation {rotation}");
        }
    }

    /// A 1440x2560 portrait frame must rotate in a few ms: the per-pixel version took 30 to 67 ms under
    /// recording load and stalled the capture loop (60 to 98 ms pacing gaps on the rotated monitor).
    #[test]
    fn rotating_a_portrait_frame_is_fast() {
        let (w, h) = (2560usize, 1440usize);
        let src = vec![7u8; w * h * 4];
        let mut out = vec![0u8; w * h * 4];
        rotate_strided(&src, w * 4, w, h, 4, &mut out); // warm up the pages
        let t = std::time::Instant::now();
        for _ in 0..5 {
            rotate_strided(std::hint::black_box(&src), w * 4, w, h, 4, &mut out);
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / 5.0;
        std::hint::black_box(&out);
        // Generous bound so a busy CI machine or a debug build does not fail it; the release figure is printed.
        println!("rotate 2560x1440: {ms:.1} ms per frame");
        if !cfg!(debug_assertions) {
            assert!(ms < 40.0, "tiled rotation took {ms:.1} ms per frame");
        }
    }
}
