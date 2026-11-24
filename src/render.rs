use super::*;
use planor::Rect;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation as F,
        Graphics::{
            Direct2D::{Common::*, *},
            Direct3D::*,
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};
use windows_sys::Win32::{
    Graphics::Dwm::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*,
};
#[derive(Clone)]
pub struct Tile {
    pub pinned: bool,
    /// A stack member hidden behind the visible peek: placed (parked) but never drawn.
    pub hidden: bool,
    /// Where this tile may be drawn: the strip's area. A deck member beyond the column is clipped to it,
    /// so it never paints over the taskbar.
    pub bound: Rect,
    pub fullscreen: bool,
    pub preview: bool,
    pub id: isize,
    pub rect: Rect,
    pub monitor: String,
    pub color: Option<u32>,
    pub cookie: usize,
    pub cover: Option<Instant>,
}
pub struct Scene {
    pub epoch: u64,
    pub input: Option<Instant>,
    pub input_qpc_us: Option<i64>,
    pub diagnostics: bool,
    pub monitors: std::sync::Arc<[(String, Rect, Rect)]>,
    pub tiles: Vec<Tile>,
    pub duration: u64,
    pub spring: bool,
    pub width: f32,
    pub radius: f32,
    /// Rectangles of unmanaged app windows (desktop coordinates): cut out of the overlay.
    pub holes: std::sync::Arc<[Rect]>,
    /// Filled boxes drawn under the borders: the stack drop zones while a window is carried.
    pub zones: Vec<Zone>,
    /// What caused this scene (scroll, drag_reflow, open_close, stack_switch, resize, ...): animation frame
    /// timings are reported per action.
    pub action: &'static str,
    /// Driven by the fingers (or the momentum after them): windows that move are drawn as DWM thumbnails
    /// and their real windows parked, so no app has to move or repaint 60+ times a second. A scene that is
    /// not live puts the real windows back.
    pub live: bool,
    /// When true, `apply_placements` must run OS placement even if `action == "idle"` (e.g. newborn
    /// snap-back, cancelled drag, or reload).
    pub force_place: bool,
}
#[derive(Clone, Debug)]
pub struct Zone {
    pub rect: Rect,
    pub monitor: String,
    pub color: u32,
    pub alpha: f32,
}
pub enum Message {
    Scene(Scene),
    /// Holes and tile border colour patch that must not interrupt an in-flight animation or
    /// `open_close` handoff (no new epoch, no motion reset).
    OverlayUpdate {
        #[cfg_attr(not(test), allow(dead_code))]
        update_id: u64,
        holes: std::sync::Arc<[Rect]>,
        colors: Vec<(isize, Option<u32>)>,
    },
    /// Monitors (name, bounds) to build overlay hosts for ahead of the first scene.
    Warm(Vec<(String, Rect)>),
    Clear(crossbeam_channel::Sender<()>),
    Stop(crossbeam_channel::Sender<()>),
}
/// Apply an `OverlayUpdate` patch in-place to `scene` without altering epoch, action, duration,
/// or tile geometry.
pub(crate) fn apply_overlay_update(
    scene: &mut Scene,
    holes: std::sync::Arc<[Rect]>,
    colors: &[(isize, Option<u32>)],
) {
    scene.holes = holes;
    for tile in &mut scene.tiles {
        if let Some(&(_, c)) = colors.iter().find(|&&(id, _)| id == tile.id) {
            tile.color = c;
        }
    }
}
// The overlay is WS_EX_TRANSPARENT: every click falls through to whatever is underneath (the taskbar
// included). WM_NCHITTEST/HTTRANSPARENT cannot do this: it only passes clicks to windows owned by the
// same thread, so a topmost overlay swallowed clicks meant for explorer.exe's taskbar. Peek clicks are
// caught by the low-level mouse hook in win32.rs, which reads the rectangles published in `win32::PEEKS`.
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    // DirectComposition owns the overlay surface; suppress GDI background erases so
    // ShowWindow / SetWindowRgn never flash a solid brush over the monitor.
    if m == WM_ERASEBKGND {
        return 1;
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}
use windows::{
    Graphics::{
        Capture::*,
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::System::WinRT::{Direct3D11::*, Graphics::Capture::*},
};
struct Source {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    winrt: IDirect3DDevice,
    size: windows::Graphics::SizeInt32,
    bitmap: Option<ID2D1Bitmap1>,
}
impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
impl Source {
    fn new(id: isize, gpu: &ID3D11Device) -> windows::core::Result<Self> {
        unsafe {
            let factory: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = factory.CreateForWindow(F::HWND(id as _))?;
            let size = item.Size()?;
            let dx: IDXGIDevice = gpu.cast()?;
            let winrt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dx)?.cast()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsCursorCaptureEnabled(false);
            let _ = session.SetIsBorderRequired(false);
            session.StartCapture()?;
            Ok(Self {
                pool,
                session,
                winrt,
                size,
                bitmap: None,
            })
        }
    }
    fn update(&mut self, surface: &Surface, after: i64) -> windows::core::Result<()> {
        unsafe {
            let mut newest = None;
            for _ in 0..2 {
                match self.pool.TryGetNextFrame() {
                    Ok(f) => newest = Some(f),
                    Err(_) => break,
                }
            }
            let Some(frame) = newest else { return Ok(()) };
            if frame.SystemRelativeTime()?.Duration < after {
                frame.Close()?;
                return Ok(());
            }
            let size = frame.ContentSize()?;
            if size.Width != self.size.Width || size.Height != self.size.Height {
                frame.Close()?;
                if size.Width > 0 && size.Height > 0 {
                    self.pool.Recreate(
                        &self.winrt,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        size,
                    )?;
                    self.size = size
                }
                return Ok(());
            }
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let tex: ID3D11Texture2D = access.GetInterface()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            desc.BindFlags =
                D3D11_BIND_SHADER_RESOURCE.0 as u32 | D3D11_BIND_RENDER_TARGET.0 as u32;
            let mut copy = None;
            surface.gpu.CreateTexture2D(&desc, None, Some(&mut copy))?;
            let copy = copy.unwrap();
            surface.gpu.GetImmediateContext()?.CopyResource(&copy, &tex);
            frame.Close()?;
            let dx: IDXGISurface = copy.cast()?;
            self.bitmap = Some(surface.context.CreateBitmapFromDxgiSurface(&dx, None)?);
            Ok(())
        }
    }
}
struct SharedDevices {
    generation: u64,
    gpu: ID3D11Device,
    dev: IDCompositionDevice,
    d2d: ID2D1Device,
}
thread_local! {
    /// One D3D11 device (with its DirectComposition and Direct2D devices) for the render thread's lifetime.
    /// It used to be one per overlay host, created again on every display change; the old devices were
    /// never fully released (240 AMD driver threads, +500 MB RAM, +700 MB VRAM, +10k handles per display
    /// blip in the user's daemon). Hosts now only make their own target, surface and context.
    static SHARED: std::cell::RefCell<Option<SharedDevices>> =
        const { std::cell::RefCell::new(None) };
    static SHARED_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub(crate) fn should_reset_shared_device(current_gen: Option<u64>, dead_gen: u64) -> bool {
    current_gen == Some(dead_gen)
}
/// Production invalidation decision for multi-host GPU resets: resets `shared_gen` at most once
/// for `dead_gen` and clears every host currently bound to `dead_gen` (including `D2DERR_RECREATE_TARGET`
/// where `GetDeviceRemovedReason` is still `OK`).
pub(crate) fn plan_device_invalidation(
    shared_gen: &mut Option<u64>,
    host_gens: &mut [Option<u64>],
    dead_gen: u64,
) -> bool {
    let reset = should_reset_shared_device(*shared_gen, dead_gen);
    if reset {
        *shared_gen = None;
    }
    for g in host_gens {
        if *g == Some(dead_gen) {
            *g = None;
        }
    }
    reset
}
pub(crate) fn bump_host_generation(host_generation: &mut u64, next_host_gen: &mut u64) {
    *host_generation = *next_host_gen;
    *next_host_gen += 1;
}
pub(crate) fn accept_source_delivery(host_generation: u64, job_generation: u64) -> bool {
    host_generation == job_generation
}
#[allow(clippy::too_many_arguments)]
fn record_visible_frame_state(
    last_draw: &mut DrawSig,
    want_visible: &mut bool,
    last_draw_valid: &mut bool,
    fills: &[(Rect, u32, f32)],
    cards: &[CardDraw],
    lines: &[(Rect, u32)],
    holes: &[Rect],
    width: f32,
    radius: f32,
    dpi: f32,
    has_pictures: bool,
    draw_succeeded: bool,
) {
    *want_visible = true;
    last_draw.set_from(fills, cards, lines, holes, width, radius, dpi);
    *last_draw_valid = draw_succeeded && !has_pictures;
}
fn clear_hidden_frame_state(
    last_draw: &mut DrawSig,
    want_visible: &mut bool,
    last_draw_valid: &mut bool,
    desired_rgn_holes: &mut Vec<Rect>,
) {
    *want_visible = false;
    last_draw.clear();
    *last_draw_valid = false;
    desired_rgn_holes.clear();
}
fn should_draw_on_recovery(want_visible: bool, last_draw: &DrawSig) -> bool {
    want_visible && !last_draw.is_empty()
}
/// Reset `SHARED` only if it still holds `dead_gen`, so a multi-host device loss creates at most
/// one replacement D3D11 device per dead generation instead of one per monitor.
fn reset_shared_devices_if_generation(dead_gen: u64) -> bool {
    SHARED.with(|s| {
        let mut slot = s.borrow_mut();
        let mut current = slot.as_ref().map(|d| d.generation);
        let reset = plan_device_invalidation(&mut current, &mut [], dead_gen);
        if reset {
            *slot = None;
        }
        reset
    })
}
/// True if an HRESULT indicates a D3D11/DXGI/D2D device-lost or target-recreate condition.
pub(crate) fn is_device_lost_hr(code: i32) -> bool {
    const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A_0005u32 as i32;
    const DXGI_ERROR_DEVICE_HUNG: i32 = 0x887A_0006u32 as i32;
    const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A_0007u32 as i32;
    const DXGI_ERROR_DRIVER_INTERNAL_ERROR: i32 = 0x887A_0020u32 as i32;
    const D2DERR_RECREATE_TARGET: i32 = 0x8899_000Cu32 as i32;
    matches!(
        code,
        DXGI_ERROR_DEVICE_REMOVED
            | DXGI_ERROR_DEVICE_HUNG
            | DXGI_ERROR_DEVICE_RESET
            | DXGI_ERROR_DRIVER_INTERNAL_ERROR
            | D2DERR_RECREATE_TARGET
    )
}
fn is_device_lost_error(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(we) = err.downcast_ref::<windows::core::Error>()
        && is_device_lost_hr(we.code().0)
    {
        return true;
    }
    let msg = err.to_string();
    msg.contains("0x887A0005")
        || msg.contains("0x887A0006")
        || msg.contains("0x887A0007")
        || msg.contains("0x887A0020")
        || msg.contains("0x8899000C")
}
/// Bounded backoff for recreating a lost DirectComposition/D3D11 surface without spinning or log spam.
#[derive(Default, Debug, Clone)]
pub(crate) struct SurfaceRecovery {
    pub attempts: u32,
    pub next_retry: Option<Instant>,
    pub logged: bool,
}
impl SurfaceRecovery {
    pub(crate) fn retry_delay(attempt: u32) -> Duration {
        match attempt {
            0 => Duration::ZERO,
            1 => Duration::from_millis(250),
            2 => Duration::from_millis(500),
            3 => Duration::from_millis(1000),
            _ => Duration::from_millis(2000),
        }
    }
    pub(crate) fn can_try(&self, now: Instant) -> bool {
        self.next_retry.is_none_or(|t| now >= t)
    }
    /// Returns true if this failure should be logged (first failure in an outage).
    pub(crate) fn record_failure(&mut self, now: Instant) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        self.next_retry = Some(now + Self::retry_delay(self.attempts));
        let first = !self.logged;
        self.logged = true;
        first
    }
    pub(crate) fn reset(&mut self) {
        self.attempts = 0;
        self.next_retry = None;
        self.logged = false;
    }
}
fn shared_devices() -> Result<(u64, ID3D11Device, IDCompositionDevice, ID2D1Device)> {
    SHARED.with(|s| {
        if let Some(d) = s.borrow().as_ref() {
            // A removed device (driver reset, GPU switch) is replaced instead of reused.
            if unsafe { d.gpu.GetDeviceRemovedReason() }.is_ok() {
                return Ok((d.generation, d.gpu.clone(), d.dev.clone(), d.d2d.clone()));
            }
        }
        let d = unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                F::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let gpu = device.unwrap();
            let dx: IDXGIDevice = gpu.cast()?;
            let dev: IDCompositionDevice = DCompositionCreateDevice(&dx)?;
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = factory.CreateDevice(&dx)?;
            let generation = SHARED_GEN.with(|g| {
                let next = g.get().wrapping_add(1).max(1);
                g.set(next);
                next
            });
            SharedDevices {
                generation,
                gpu,
                dev,
                d2d,
            }
        };
        let out = (d.generation, d.gpu.clone(), d.dev.clone(), d.d2d.clone());
        *s.borrow_mut() = Some(d);
        Ok(out)
    })
}
/// D3D11 devices are free-threaded; the capture worker only uses this one to create its capture source.
struct SendDevice(ID3D11Device);
unsafe impl Send for SendDevice {}
/// A capture source made on the worker (its frame pool is free-threaded, its session agile); handed over once.
struct SendSource(Source);
unsafe impl Send for SendSource {}
#[derive(Clone, Copy, Debug, PartialEq)]
struct CardDraw {
    rect: Rect,
    spinner_alpha: f32,
    spinner_angle_rad: f32,
}
#[derive(Clone, Default, PartialEq, Eq)]
struct DrawSig {
    fills: Vec<(Rect, u32, u32)>,
    cards: Vec<(Rect, u32, u32)>,
    lines: Vec<(Rect, u32)>,
    holes: Vec<Rect>,
    width_bits: u32,
    radius_bits: u32,
    dpi_bits: u32,
}
impl DrawSig {
    fn matches(
        &self,
        fills: &[(Rect, u32, f32)],
        cards: &[CardDraw],
        lines: &[(Rect, u32)],
        holes: &[Rect],
        width: f32,
        radius: f32,
        dpi: f32,
    ) -> bool {
        self.width_bits == width.to_bits()
            && self.radius_bits == radius.to_bits()
            && self.dpi_bits == dpi.to_bits()
            && self.fills.len() == fills.len()
            && self.cards.len() == cards.len()
            && self.lines.len() == lines.len()
            && self.holes.as_slice() == holes
            && self
                .fills
                .iter()
                .zip(fills)
                .all(|(&(r1, c1, a1), &(r2, c2, a2))| r1 == r2 && c1 == c2 && a1 == a2.to_bits())
            && self.cards.iter().zip(cards).all(|(&(r1, a1, ang1), c2)| {
                r1 == c2.rect
                    && a1 == c2.spinner_alpha.to_bits()
                    && ang1 == c2.spinner_angle_rad.to_bits()
            })
            && self.lines.as_slice() == lines
    }
    fn set_from(
        &mut self,
        fills: &[(Rect, u32, f32)],
        cards: &[CardDraw],
        lines: &[(Rect, u32)],
        holes: &[Rect],
        width: f32,
        radius: f32,
        dpi: f32,
    ) {
        self.fills.clear();
        self.fills
            .extend(fills.iter().map(|&(r, c, a)| (r, c, a.to_bits())));
        self.cards.clear();
        self.cards.extend(
            cards
                .iter()
                .map(|c| (c.rect, c.spinner_alpha.to_bits(), c.spinner_angle_rad.to_bits())),
        );
        self.lines.clear();
        self.lines.extend_from_slice(lines);
        self.holes.clear();
        self.holes.extend_from_slice(holes);
        self.width_bits = width.to_bits();
        self.radius_bits = radius.to_bits();
        self.dpi_bits = dpi.to_bits();
    }
    fn clear(&mut self) {
        self.fills.clear();
        self.cards.clear();
        self.lines.clear();
        self.holes.clear();
        self.width_bits = 0;
        self.radius_bits = 0;
        self.dpi_bits = 0;
    }
    fn is_empty(&self) -> bool {
        self.fills.is_empty() && self.cards.is_empty() && self.lines.is_empty()
    }
}
struct DrawFrame<'a> {
    pictures: &'a [(Rect, ID2D1Bitmap1)],
    fills: &'a [(Rect, u32, f32)],
    cards: &'a [CardDraw],
    lines: &'a [(Rect, u32)],
    holes: &'a [Rect],
    width: f32,
    radius: f32,
    dpi: f32,
}
struct Surface {
    device_gen: u64,
    gpu: ID3D11Device,
    dev: IDCompositionDevice,
    _target: IDCompositionTarget,
    surface: IDCompositionSurface,
    context: ID2D1DeviceContext,
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
    clear_brush: Option<ID2D1SolidColorBrush>,
    timer: crate::gpu::GpuTimer,
}
impl Surface {
    unsafe fn brush_for<'a>(
        brushes: &'a mut HashMap<u32, ID2D1SolidColorBrush>,
        rt: &ID2D1RenderTarget,
        c: u32,
    ) -> Result<&'a ID2D1SolidColorBrush> {
        Ok(match brushes.entry(c) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let created = unsafe {
                    rt.CreateSolidColorBrush(
                        &D2D1_COLOR_F {
                            r: ((c >> 16) & 255) as f32 / 255.,
                            g: ((c >> 8) & 255) as f32 / 255.,
                            b: (c & 255) as f32 / 255.,
                            a: 1.,
                        },
                        None,
                    )?
                };
                e.insert(created)
            }
        })
    }
    fn new(h: HWND, b: Rect) -> Result<Self> {
        unsafe {
            let (device_gen, gpu, dev, d2d) = shared_devices()?;
            let target = dev.CreateTargetForHwnd(F::HWND(h), true)?;
            let visual = dev.CreateVisual()?;
            let surface = dev.CreateSurface(
                b.w as _,
                b.h as _,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )?;
            visual.SetContent(&surface)?;
            target.SetRoot(&visual)?;
            dev.Commit()?;
            let context = d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            Ok(Self {
                device_gen,
                dev,
                _target: target,
                gpu,
                surface,
                context,
                brushes: HashMap::new(),
                clear_brush: None,
                timer: Default::default(),
            })
        }
    }
    fn draw(&mut self, frame: &DrawFrame<'_>) -> Result<()> {
        let DrawFrame {
            pictures,
            fills,
            cards,
            lines,
            holes,
            width,
            radius,
            dpi,
        } = frame;
        let (width, radius, dpi) = (*width, *radius, *dpi);
        unsafe {
            let mut off = F::POINT::default();
            let mark = self.timer.begin(&self.gpu);
            let dx: IDXGISurface = self.surface.BeginDraw(None, &mut off)?;
            let result = (|| -> Result<()> {
                let props = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.,
                    dpiY: 96.,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    ..Default::default()
                };
                let target = self
                    .context
                    .CreateBitmapFromDxgiSurface(&dx, Some(&props))?;
                self.context.SetTarget(&target);
                let rt: &ID2D1RenderTarget = &self.context;
                rt.BeginDraw();
                rt.Clear(Some(&D2D1_COLOR_F::default()));
                for (r, bitmap) in *pictures {
                    let x = (r.x + off.x) as f32;
                    let y = (r.y + off.y) as f32;
                    rt.DrawBitmap(
                        bitmap,
                        Some(&D2D_RECT_F {
                            left: x,
                            top: y,
                            right: x + r.w as f32,
                            bottom: y + r.h as f32,
                        }),
                        1.,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    )
                }
                for &(r, c, alpha) in *fills {
                    let brush = Self::brush_for(&mut self.brushes, rt, c)?;
                    let x = (r.x + off.x) as f32;
                    let y = (r.y + off.y) as f32;
                    brush.SetOpacity(alpha);
                    rt.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F { left: x, top: y, right: x + r.w as f32, bottom: y + r.h as f32 },
                            radiusX: radius,
                            radiusY: radius,
                        },
                        brush,
                    );
                }
                for card in *cards {
                    let x = (card.rect.x + off.x) as f32;
                    let y = (card.rect.y + off.y) as f32;
                    let w = card.rect.w as f32;
                    let h = card.rect.h as f32;
                    let base = Self::brush_for(&mut self.brushes, rt, 0x161B26)?;
                    base.SetOpacity(1.0);
                    rt.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                            radiusX: radius,
                            radiusY: radius,
                        },
                        base,
                    );
                    let rim_w = 1.0 * dpi;
                    let rim_d = rim_w * 0.5;
                    let rim_r = (radius - rim_d).max(0.0);
                    let rim = Self::brush_for(&mut self.brushes, rt, 0x2A3347)?;
                    rim.SetOpacity(0.85);
                    rt.DrawRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: x + rim_d,
                                top: y + rim_d,
                                right: x + w - rim_d,
                                bottom: y + h - rim_d,
                            },
                            radiusX: rim_r,
                            radiusY: rim_r,
                        },
                        rim,
                        rim_w,
                        None,
                    );
                    if card.spinner_alpha > 0.001 && w.min(h) >= 80.0 * dpi {
                        let cx = x + w * 0.5;
                        let cy = y + h * 0.5;
                        let ring_r = 14.0 * dpi;
                        let mut track = D2D1_ELLIPSE {
                            radiusX: ring_r,
                            radiusY: ring_r,
                            ..Default::default()
                        };
                        track.point.X = cx;
                        track.point.Y = cy;
                        let track_brush = Self::brush_for(&mut self.brushes, rt, 0x334155)?;
                        track_brush.SetOpacity(0.35 * card.spinner_alpha);
                        rt.DrawEllipse(&track, track_brush, 2.2 * dpi, None);
                        for i in 0..12 {
                            let frac = i as f32 / 11.0;
                            let theta = card.spinner_angle_rad
                                - (1.0 - frac) * (std::f32::consts::TAU * 2.0 / 3.0);
                            let dot_r = (1.0 + 1.3 * frac) * dpi;
                            let mut dot = D2D1_ELLIPSE {
                                radiusX: dot_r,
                                radiusY: dot_r,
                                ..Default::default()
                            };
                            dot.point.X = cx + ring_r * theta.cos();
                            dot.point.Y = cy + ring_r * theta.sin();
                            let dot_brush = Self::brush_for(&mut self.brushes, rt, 0x60A5FA)?;
                            dot_brush.SetOpacity((0.12 + 0.88 * frac * frac) * card.spinner_alpha);
                            rt.FillEllipse(&dot, dot_brush);
                        }
                    }
                }
                for &(r, c) in *lines {
                    let brush = Self::brush_for(&mut self.brushes, rt, c)?;
                    let x = (r.x + off.x) as f32;
                    let y = (r.y + off.y) as f32;
                    brush.SetOpacity(1.0);
                    let d = width / 2.;
                    rt.DrawRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F {
                                left: x + d,
                                top: y + d,
                                right: x + r.w as f32 - d,
                                bottom: y + r.h as f32 - d,
                            },
                            radiusX: radius,
                            radiusY: radius,
                        },
                        brush,
                        width,
                        None,
                    )
                }
                if !holes.is_empty() {
                    // Punch holes for unmanaged windows / system flyouts directly in Direct2D
                    // (pure transparent COPY) instead of mutating SetWindowRgn on the fullscreen
                    // overlay window, which caused DWM black flashes while moving Task Manager.
                    let clear = match self.clear_brush.as_ref() {
                        Some(b) => b,
                        None => {
                            let b = rt.CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)?;
                            self.clear_brush.insert(b)
                        }
                    };
                    self.context.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_COPY);
                    for h in *holes {
                        let x = (h.x + off.x) as f32;
                        let y = (h.y + off.y) as f32;
                        rt.FillRectangle(
                            &D2D_RECT_F {
                                left: x,
                                top: y,
                                right: x + h.w as f32,
                                bottom: y + h.h as f32,
                            },
                            clear,
                        );
                    }
                    self.context.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
                }
                let end = rt.EndDraw(None, None);
                self.context.SetTarget(None);
                end?;
                Ok(())
            })();
            self.surface.EndDraw()?;
            self.timer.end(&self.gpu, mark);
            result?;
            self.dev.Commit()?;
            Ok(())
        }
    }
}
struct Host {
    generation: u64,
    h: HWND,
    b: Rect,
    surface: Option<Surface>,
    recovery: SurfaceRecovery,
    thumbs: HashMap<isize, isize>,
    pictures: HashMap<(isize, usize), Source>,
    failed_sources: HashMap<(isize, usize), Instant>,
    pending_sources: HashSet<(isize, usize)>,
    applied_holes: Option<Vec<Rect>>,
    desired_rgn_holes: Vec<Rect>,
    visible: bool,
    want_visible: bool,
    last_draw: DrawSig,
    last_draw_valid: bool,
}
impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            for (_, t) in self.thumbs.drain() {
                DwmUnregisterThumbnail(t);
            }
            DestroyWindow(self.h);
        }
    }
}
impl Host {
    fn new(generation: u64, b: Rect) -> Result<Self> {
        unsafe {
            let class = wide("Planor.Composition");
            let inst = GetModuleHandleW(null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(proc),
                hbrBackground: null_mut(),
                lpszClassName: class.as_ptr(),
                hInstance: inst,
                ..Default::default()
            });
            let h = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                b.x,
                b.y,
                b.w,
                b.h,
                null_mut(),
                null_mut(),
                inst,
                null(),
            );
            if h.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            SetLayeredWindowAttributes(h, 0, 255, LWA_COLORKEY | LWA_ALPHA);
            DwmExtendFrameIntoClientArea(
                h,
                &windows_sys::Win32::UI::Controls::MARGINS {
                    cxLeftWidth: -1,
                    cxRightWidth: -1,
                    cyTopHeight: -1,
                    cyBottomHeight: -1,
                },
            );
            let mut surface = None;
            for attempt in 1..=3 {
                match Surface::new(h, b) {
                    Ok(s) => {
                        surface = Some(s);
                        break;
                    }
                    Err(e) if attempt == 3 => {
                        eprintln!("composition surface: {e} at {}:{}", file!(), line!())
                    }
                    _ => (),
                }
            }
            let mut recovery = SurfaceRecovery::default();
            if surface.is_none() {
                recovery.record_failure(Instant::now());
            }
            Ok(Self {
                generation,
                h,
                b,
                surface,
                recovery,
                thumbs: Default::default(),
                pictures: HashMap::new(),
                failed_sources: HashMap::new(),
                pending_sources: HashSet::new(),
                applied_holes: None,
                desired_rgn_holes: Vec::new(),
                visible: false,
                want_visible: false,
                last_draw: DrawSig::default(),
                last_draw_valid: false,
            })
        }
    }
    fn invalidate_gpu(&mut self, next_host_gen: &mut u64) {
        bump_host_generation(&mut self.generation, next_host_gen);
        self.surface = None;
        self.pictures.clear();
        self.pending_sources.clear();
        self.failed_sources.clear();
        self.last_draw_valid = false;
    }
    unsafe fn apply_rgn_holes(&mut self, rgn_holes: &[Rect]) {
        if self.applied_holes.as_deref() == Some(rgn_holes) {
            return;
        }
        let _rgn_span = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderRgnHoles);
        unsafe {
            if rgn_holes.is_empty() {
                windows_sys::Win32::Graphics::Gdi::SetWindowRgn(self.h, null_mut(), 0);
            } else {
                let whole = windows_sys::Win32::Graphics::Gdi::CreateRectRgn(0, 0, self.b.w, self.b.h);
                for h in rgn_holes {
                    let cut = windows_sys::Win32::Graphics::Gdi::CreateRectRgn(
                        h.x - self.b.x,
                        h.y - self.b.y,
                        h.x - self.b.x + h.w,
                        h.y - self.b.y + h.h,
                    );
                    windows_sys::Win32::Graphics::Gdi::CombineRgn(
                        whole,
                        whole,
                        cut,
                        windows_sys::Win32::Graphics::Gdi::RGN_DIFF,
                    );
                    windows_sys::Win32::Graphics::Gdi::DeleteObject(cut as _);
                }
                windows_sys::Win32::Graphics::Gdi::SetWindowRgn(self.h, whole, 0);
            }
        }
        let slot = self.applied_holes.get_or_insert_with(Vec::new);
        slot.clear();
        slot.extend_from_slice(rgn_holes);
    }
    fn thumbnail(&mut self, id: isize, r: Rect, clip: Rect) -> bool {
        unsafe {
            let t = *self.thumbs.entry(id).or_insert_with(|| {
                let mut t = 0;
                let hr = DwmRegisterThumbnail(self.h, id as _, &mut t);
                if hr < 0 {
                    eprintln!("DwmRegisterThumbnail: {hr:#x} at {}:{}", file!(), line!());
                }
                t
            });
            if t == 0 {
                return false;
            }
            let mut size = SIZE::default();
            if DwmQueryThumbnailSourceSize(t, &mut size) < 0 {
                return false;
            }
            // One source pixel per screen pixel, never scaled: the thumbnail source is the whole window
            // (with its invisible resize borders), and a parked group member is often not exactly the tile
            // size yet. Scaling `clip` by source/tile size stretched peeks (title bars drawn elongated).
            // When the source is smaller than the clip, the destination shrinks with it instead.
            let (ox, oy) = win32::insets(id).unwrap_or(((size.cx - r.w).max(0) / 2, 0));
            let sx = ox + clip.x - r.x;
            let sy = oy + clip.y - r.y;
            let sw = clip.w.min(size.cx - sx).max(0);
            let sh = clip.h.min(size.cy - sy).max(0);
            if sw == 0 || sh == 0 {
                return false;
            }
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_RECTSOURCE
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY,
                rcDestination: RECT {
                    left: clip.x - self.b.x,
                    top: clip.y - self.b.y,
                    right: clip.x + sw - self.b.x,
                    bottom: clip.y + sh - self.b.y,
                },
                rcSource: RECT {
                    left: sx,
                    top: sy,
                    right: sx + sw,
                    bottom: sy + sh,
                },
                opacity: 255,
                fVisible: 1,
                fSourceClientAreaOnly: 0,
            };
            let hr = DwmUpdateThumbnailProperties(t, &props);
            if hr < 0 {
                eprintln!(
                    "DwmUpdateThumbnailProperties: {hr:#x} at {}:{}",
                    file!(),
                    line!()
                );
            }
            hr >= 0
        }
    }
}
fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let w = (a.x + a.w).min(b.x + b.w) - x;
    let h = (a.y + a.h).min(b.y + b.h) - y;
    (w > 0 && h > 0).then_some(Rect { x, y, w, h })
}
/// Filter `holes` to only those that intersect at least one active DWM thumbnail rect.
/// Direct2D borders/zones/cards are masked inside `Surface::draw`, so `SetWindowRgn` is only
/// needed when a DWM thumbnail overlaps an unmanaged window.
pub(crate) fn holes_intersecting_rects(holes: &[Rect], rects: &[Rect], out: &mut Vec<Rect>) {
    out.clear();
    if rects.is_empty() || holes.is_empty() {
        return;
    }
    for &h in holes {
        if rects.iter().any(|&r| intersect(h, r).is_some()) {
            out.push(h);
        }
    }
}
/// Effective stand-in rect for a tile during motion or open/close async handoff:
/// keeps the window's original size while the stand-in glides so neither the bitmap nor the
/// focus border jumps ahead of the real window resize.
pub(crate) fn stand_in_rect(origin: Rect, current: Rect, t: f64, open_handoff: bool) -> Rect {
    if t < 1. || open_handoff {
        Rect { w: origin.w, h: origin.h, ..current }
    } else {
        current
    }
}
/// Separate the drawing stand-in rect (`rs`) and its draw clip from the parking decision, which
/// must be computed from `final_rect` against `bound`. Otherwise, a shrinking tile at `x > 0`
/// during `open_handoff` overflows `bound` at its origin width and gets parked off-screen, while
/// a growing tile whose final rect overflows `bound` would be placed unclipped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tile_stand_in_and_park(
    origin: Rect,
    current: Rect,
    final_rect: Rect,
    bound: Rect,
    t: f64,
    open_handoff: bool,
    moving: bool,
    live_proxy: bool,
    pinned: bool,
    preview: bool,
    hidden: bool,
) -> (Rect, Option<Rect>, bool, bool) {
    let final_clip = if hidden { None } else { intersect(final_rect, bound) };
    let park_window =
        !pinned && (moving || live_proxy || final_clip != Some(final_rect) || preview || hidden);
    let tile_moved = origin != final_rect;
    // When `t >= 1` and the tile is parked (e.g. clipped), draw it at its final size instead of
    // holding the old origin size (otherwise if all moved tiles end parked, the scene finishes on
    // the handoff frame with a stale stand-in size).
    let hold_origin_size = open_handoff && !park_window && tile_moved;
    let rs = stand_in_rect(origin, current, t, hold_origin_size);
    let draw_clip = if hidden { None } else { intersect(rs, bound) };
    let proxy = park_window || (!pinned && open_handoff && tile_moved);
    (rs, draw_clip, park_window, proxy)
}
/// True if a delayed post-settle verify check should report a real minimum-size constraint via
/// `Event::Rejected`: requires matching scene epoch, a responsive (non-hung) window, a size larger
/// than requested, a stable size across consecutive verify checks, AND confirmation against the
/// app's real `WM_GETMINMAXINFO` minimum (so a slow app that has not yet resized, or a timed-out
/// cover/handoff, never permanently undoes a user's resize).
#[allow(clippy::too_many_arguments)]
pub(crate) fn should_verify_reject(
    verify_epoch: u64,
    current_epoch: u64,
    want: Rect,
    origin: Option<Rect>,
    prev: Option<Rect>,
    got: Rect,
    hung: bool,
    min_track: (i32, i32),
    timed_out: bool,
) -> bool {
    if verify_epoch != current_epoch || hung {
        return false;
    }
    let w_over = got.w > want.w + 1;
    let h_over = got.h > want.h + 1;
    if !(w_over || h_over) {
        return false;
    }
    if !prev.is_some_and(|p| (p.w - got.w).abs() <= 1 && (p.h - got.h).abs() <= 1) {
        return false;
    }
    let confirmed_min = (w_over && min_track.0 > want.w + 1 && got.w <= min_track.0 + 2)
        || (h_over && min_track.1 > want.h + 1 && got.h <= min_track.1 + 2);
    if confirmed_min {
        return true;
    }
    if timed_out {
        return false;
    }
    // If the window is still at its original size, it has not started resizing yet; without a
    // matching `min_track` from WM_GETMINMAXINFO we must not treat it as constrained.
    let unchanged_from_origin =
        origin.is_some_and(|o| (o.w - got.w).abs() <= 1 && (o.h - got.h).abs() <= 1);
    if unchanged_from_origin {
        return false;
    }
    false
}
/// True if an `open_landing` item has arrived: either within 2 px of target position and at least
/// `want - 2` in size, OR positioned within 2 px with a size that has *changed from `origin`* and
/// stayed stable for 3 consecutive polls (handles character-grid terminals without prematurely
/// accepting an in-place grow that is still at its old size).
pub(crate) fn open_landing_item_arrived(
    origin: Rect,
    want: Rect,
    got: Rect,
    stable: &mut Option<(Rect, u8)>,
) -> bool {
    let pos_ok = (got.x - want.x).abs() <= 2 && (got.y - want.y).abs() <= 2;
    if !pos_ok {
        *stable = None;
        return false;
    }
    if got.w >= want.w - 2 && got.h >= want.h - 2 {
        return true;
    }
    let size_changed_from_origin =
        (got.w - origin.w).abs() > 1 || (got.h - origin.h).abs() > 1;
    if !size_changed_from_origin {
        *stable = None;
        return false;
    }
    let count = match stable {
        Some((prev, n)) if (prev.w - got.w).abs() <= 1 && (prev.h - got.h).abs() <= 1 => {
            *n = n.saturating_add(1);
            *n
        }
        _ => {
            *stable = Some((got, 1));
            1
        }
    };
    count >= 3
}
/// True when an `"idle"` scene with no forced placement or covers already matches every target's
/// real OS frame within 2 px (including parked windows at `park`), so `win32::place` can be skipped.
pub(crate) fn all_targets_already_placed(
    action: &str,
    force_place: bool,
    has_covers: bool,
    placements: &[(isize, Rect)],
    frame_of: impl Fn(isize) -> Option<Rect>,
) -> bool {
    if action != "idle" || force_place || has_covers || placements.is_empty() {
        return false;
    }
    placements.iter().all(|&(id, target)| {
        frame_of(id).is_some_and(|got| {
            (got.x - target.x).abs() <= 2
                && (got.y - target.y).abs() <= 2
                && (got.w - target.w).abs() <= 2
                && (got.h - target.h).abs() <= 2
        })
    })
}
fn interpolate(a: Rect, b: Rect, t: f64) -> Rect {
    let f = |a, b| (a as f64 + (b as f64 - a as f64) * t).round() as i32;
    Rect {
        x: f(a.x, b.x),
        y: f(a.y, b.y),
        w: f(a.w, b.w),
        h: f(a.h, b.h),
    }
}
type Placement = (isize, Rect);
struct OpenLanding {
    epoch: u64,
    started: Instant,
    items: Vec<(isize, Rect, Rect)>,
    stable: HashMap<isize, (Rect, u8)>,
}
struct VerifyState {
    epoch: u64,
    at: Instant,
    items: Vec<Placement>,
    origins: HashMap<isize, Rect>,
    timed_out_ids: HashSet<isize>,
    prev_frames: HashMap<isize, Rect>,
    left: u8,
}
type SourceKey = (isize, usize);
type SourceJob = (String, u64, SourceKey, SendDevice);
type SourceDone = (String, u64, SourceKey, std::result::Result<SendSource, String>, u128);

fn warm_hosts(monitors: Vec<(String, Rect)>, hosts: &mut BTreeMap<String, Host>, next_gen: &mut u64) {
    for (n, b) in monitors {
        if hosts.get(&n).is_some_and(|h| h.b != b) {
            hosts.remove(&n);
        }
        if let std::collections::btree_map::Entry::Vacant(e) = hosts.entry(n) {
            let generation = *next_gen;
            *next_gen += 1;
            if let Ok(h) = Host::new(generation, b) {
                e.insert(h);
            }
        }
    }
}

struct PresentInfo {
    t: f64,
    park: i32,
    any_cover_active: bool,
    time: Instant,
    render_start: Instant,
    retries: u32,
    placement_us: u64,
}

#[derive(Default)]
struct FrameScratch {
    placements: Vec<Placement>,
    cover_placements: Vec<Placement>,
    unsent_covers: Vec<Placement>,
    settled_tiles: Vec<planor::SettledTile>,
    hits: Vec<(Rect, isize)>,
    next: HashMap<isize, Rect>,
    parking: Vec<Placement>,
    landing: Vec<Placement>,
    rejected: Vec<Placement>,
    actuals: Vec<Placement>,
    verify_items: Vec<Placement>,
    lines: Vec<(Rect, u32)>,
    backing: Vec<(Rect, ID2D1Bitmap1)>,
    cards: Vec<CardDraw>,
    keep: HashSet<isize>,
    host_hits: Vec<(Rect, isize)>,
    fills: Vec<(Rect, u32, f32)>,
    mine: Vec<Rect>,
    local_holes: Vec<Rect>,
    thumb_rects: Vec<Rect>,
    rgn_holes: Vec<Rect>,
}

struct RenderState {
    hosts: BTreeMap<String, Host>,
    next_host_gen: u64,
    current: HashMap<isize, Rect>,
    origins: HashMap<isize, Rect>,
    live_ids: HashSet<isize>,
    last_live_present: Option<Instant>,
    open_landing: Option<OpenLanding>,
    open_landed: Option<u64>,
    open_timed_out: bool,
    anim_done: bool,
    cover_cfg: planor::cover::CoverConfig,
    cover_landed: HashMap<(isize, Instant), Instant>,
    cover_done: HashMap<isize, Instant>,
    cover_sent: HashSet<(u64, isize)>,
    cover_timed_out_ids: HashSet<isize>,
    live_refresh_us: f64,
    refresh_checked: Instant,
    device_health_checked: Instant,
    source_tx: crossbeam_channel::Sender<SourceJob>,
    source_done: crossbeam_channel::Receiver<SourceDone>,
    scene: Option<Scene>,
    last_settled_scene: Option<Scene>,
    begin: Instant,
    last: Instant,
    frame_of_scene: u32,
    settled: i64,
    gesture_recorded: bool,
    parked_checked_epoch: u64,
    verify: Option<VerifyState>,
    qpc_hz: i64,
    tag: Vec<u16>,
    last_peeks: Option<Vec<(Rect, isize)>>,
    scratch: FrameScratch,
}

impl RenderState {
    fn new(source_tx: crossbeam_channel::Sender<SourceJob>, source_done: crossbeam_channel::Receiver<SourceDone>) -> Self {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceFrequency(value: *mut i64) -> i32;
        }
        let mut qpc_hz = 0i64;
        unsafe { QueryPerformanceFrequency(&mut qpc_hz) };
        let begin = Instant::now();
        Self {
            hosts: BTreeMap::new(),
            next_host_gen: 1,
            current: HashMap::new(),
            origins: HashMap::new(),
            live_ids: HashSet::new(),
            last_live_present: None,
            open_landing: None,
            open_landed: None,
            open_timed_out: false,
            anim_done: false,
            cover_cfg: planor::cover::CoverConfig::default(),
            cover_landed: HashMap::new(),
            cover_done: HashMap::new(),
            cover_sent: HashSet::new(),
            cover_timed_out_ids: HashSet::new(),
            live_refresh_us: win32::dwm_refresh_period_us().unwrap_or(16_666.7),
            refresh_checked: begin,
            device_health_checked: begin,
            source_tx,
            source_done,
            scene: None,
            last_settled_scene: None,
            begin,
            last: begin,
            frame_of_scene: 0,
            settled: 0,
            gesture_recorded: false,
            parked_checked_epoch: 0,
            verify: None,
            qpc_hz,
            tag: wide("Planor.Recovery.Identity"),
            last_peeks: None,
            scratch: FrameScratch::default(),
        }
    }

    fn timestamp(&self) -> i64 {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceCounter(value: *mut i64) -> i32;
        }
        let mut now = 0i64;
        unsafe { QueryPerformanceCounter(&mut now) };
        ((now as i128 * 10_000_000) / self.qpc_hz.max(1) as i128) as i64
    }

    fn has_active_sources(&self) -> bool {
        self.hosts
            .values()
            .any(|h| !h.pictures.is_empty() || !h.pending_sources.is_empty())
    }

    fn needs_surface_recovery(&self) -> bool {
        self.hosts.values().any(|h| h.surface.is_none())
    }

    fn invalidate_device_generation(&mut self, dead_gen: u64) {
        reset_shared_devices_if_generation(dead_gen);
        for host in self.hosts.values_mut() {
            if host.surface.as_ref().is_some_and(|s| s.device_gen == dead_gen) {
                host.invalidate_gpu(&mut self.next_host_gen);
            }
        }
    }

    /// Throttled idle poll for D3D11 device loss so TDR/driver resets are detected even when no
    /// new scene arrives or when `DrawSig` matches.
    unsafe fn check_device_health(&mut self) {
        if self.device_health_checked.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.device_health_checked = Instant::now();
        let dead_gen = self.hosts.values().find_map(|h| {
            let s = h.surface.as_ref()?;
            unsafe { s.gpu.GetDeviceRemovedReason() }.is_err().then_some(s.device_gen)
        });
        if let Some(dead_gen) = dead_gen {
            diag::trace("overlay GPU device loss detected while idle; invalidating surfaces");
            self.invalidate_device_generation(dead_gen);
        }
    }

    /// Recreate any host surface that was dropped due to GPU device loss/suspension once its
    /// backoff expires, and redraw its last static frame only if the host still wants to be visible.
    unsafe fn try_recover_surfaces(&mut self) {
        let now = Instant::now();
        let mut dead_gen: Option<u64> = None;
        for host in self.hosts.values_mut() {
            if host.surface.is_some() || !host.recovery.can_try(now) {
                continue;
            }
            match Surface::new(host.h, host.b) {
                Ok(mut s) => {
                    host.last_draw_valid = false;
                    if should_draw_on_recovery(host.want_visible, &host.last_draw) {
                        let fills: Vec<(Rect, u32, f32)> = host
                            .last_draw
                            .fills
                            .iter()
                            .map(|&(r, c, a)| (r, c, f32::from_bits(a)))
                            .collect();
                        let cards: Vec<CardDraw> = host
                            .last_draw
                            .cards
                            .iter()
                            .map(|&(rect, a, ang)| CardDraw {
                                rect,
                                spinner_alpha: f32::from_bits(a),
                                spinner_angle_rad: f32::from_bits(ang),
                            })
                            .collect();
                        let frame = DrawFrame {
                            pictures: &[],
                            fills: &fills,
                            cards: &cards,
                            lines: &host.last_draw.lines,
                            holes: &host.last_draw.holes,
                            width: f32::from_bits(host.last_draw.width_bits),
                            radius: f32::from_bits(host.last_draw.radius_bits),
                            dpi: f32::from_bits(host.last_draw.dpi_bits),
                        };
                        match s.draw(&frame) {
                            Ok(()) => {
                                let was_outage = host.recovery.logged;
                                host.recovery.reset();
                                // Draw/commit and restore desired region holes BEFORE showing so
                                // active thumbnails never paint over Task Manager after recovery.
                                let rgn = host.desired_rgn_holes.clone();
                                unsafe { host.apply_rgn_holes(&rgn) };
                                if !host.visible {
                                    unsafe { ShowWindow(host.h, SW_SHOWNOACTIVATE) };
                                    host.visible = true;
                                }
                                host.last_draw_valid = true;
                                host.surface = Some(s);
                                if was_outage {
                                    diag::trace("overlay GPU surface recovered");
                                }
                            }
                            Err(e) => {
                                let removed = unsafe { s.gpu.GetDeviceRemovedReason() }.is_err();
                                if is_device_lost_error(e.as_ref()) || removed {
                                    dead_gen = Some(s.device_gen);
                                }
                                if host.recovery.record_failure(now) {
                                    diag::trace(format!("overlay GPU surface recovery draw failed: {e}"));
                                }
                            }
                        }
                    } else {
                        // Do not reset backoff until a full normal draw succeeds, and never show
                        // a hidden host (e.g. fullscreen or #48 unfocused) during idle recovery.
                        host.surface = Some(s);
                    }
                }
                Err(e) => {
                    if host.recovery.record_failure(now) {
                        diag::trace(format!("overlay GPU surface recovery pending: {e}"));
                    }
                }
            }
        }
        if let Some(dg) = dead_gen {
            self.invalidate_device_generation(dg);
        }
    }

    fn clear_state(&mut self) {
        self.verify = None;
        self.hosts.clear();
        self.scene = None;
        self.last_settled_scene = None;
        self.current.clear();
        self.open_landing = None;
        self.open_landed = None;
        self.open_timed_out = false;
        self.anim_done = false;
        self.cover_landed.clear();
        self.cover_done.clear();
        self.cover_sent.clear();
        self.cover_timed_out_ids.clear();
        win32::set_peeks(Vec::new());
        self.last_peeks = None;
    }

    unsafe fn poll_background_work(&mut self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if let Some(mut v) = self.verify.take() {
                let current_epoch = EPOCH.load(std::sync::atomic::Ordering::Acquire);
                if v.epoch != current_epoch {
                    // Scene was superseded: drop stale verify state without placing or rejecting.
                } else if Instant::now() < v.at {
                    self.verify = Some(v);
                } else if v.left > 0 {
                    let _vspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderVerify);
                    let mut constrained = Vec::new();
                    let mut drifted = false;
                    for &(id, want) in &v.items {
                        if let Some(got) = win32::frame(id)
                            && got != want
                        {
                            drifted = true;
                            let prev = v.prev_frames.insert(id, got);
                            if v.left == 1
                                && (got.w > want.w + 1 || got.h > want.h + 1)
                                && !win32::is_hung(id)
                                && prev.is_some_and(|p| (p.w - got.w).abs() <= 1 && (p.h - got.h).abs() <= 1)
                            {
                                let min_track = (win32::minimum(id, true), win32::minimum(id, false));
                                if should_verify_reject(
                                    v.epoch,
                                    current_epoch,
                                    want,
                                    v.origins.get(&id).copied(),
                                    prev,
                                    got,
                                    false,
                                    min_track,
                                    v.timed_out_ids.contains(&id),
                                ) {
                                    constrained.push((id, got));
                                }
                            }
                        }
                    }
                    if drifted {
                        win32::place_async(&v.items);
                        if !constrained.is_empty() {
                            // Only report a real, responsive, minimum-confirmed constraint tagged
                            // with the verified scene's own epoch.
                            win32::emit(win32::Event::Rejected(v.epoch, constrained));
                        }
                        if v.left > 1 {
                            v.at = Instant::now() + Duration::from_millis(150);
                            v.left -= 1;
                            self.verify = Some(v);
                        }
                    }
                }
            }
            let async_landed = self.open_landing.as_mut().and_then(|ol| {
                let current = ol.epoch == EPOCH.load(std::sync::atomic::Ordering::Acquire);
                // Position + target size (or stable positioned size that changed from origin) is
                // the handoff signal so size-limited apps do not hit the 350 ms timeout while
                // in-place grows never drop early at their unchanged old size.
                let arrived = ol.items.iter().all(|&(id, origin, want)| {
                    win32::frame(id).is_none_or(|got| {
                        let mut entry = ol.stable.get(&id).copied();
                        let ok = open_landing_item_arrived(origin, want, got, &mut entry);
                        match entry {
                            Some(v) => {
                                ol.stable.insert(id, v);
                            }
                            None => {
                                ol.stable.remove(&id);
                            }
                        }
                        ok
                    })
                });
                let timed_out = ol.started.elapsed() >= Duration::from_millis(350);
                (current && (arrived || timed_out)).then_some((ol.epoch, !arrived))
            });
            if let Some((epoch, timed_out)) = async_landed {
                self.open_landed = Some(epoch);
                self.open_timed_out = timed_out;
                self.open_landing = None;
            }
            while let Ok((mon, generation, key, made, ms)) = self.source_done.try_recv() {
                let Some(host) = self
                    .hosts
                    .get_mut(&mon)
                    .filter(|h| accept_source_delivery(h.generation, generation))
                else {
                    continue;
                };
                host.pending_sources.remove(&key);
                if ms >= 100 {
                    diag::trace(format!("capture source for {} took {ms} ms (worker thread, render not blocked)", key.0));
                }
                match made {
                    Ok(src) => {
                        host.failed_sources.remove(&key);
                        host.pictures.insert(key, src.0);
                    }
                    Err(e) => {
                        if host.failed_sources.insert(key, Instant::now()).is_none() {
                            diag::trace(format!("WGC source creation failed for {}: {e}", key.0));
                        }
                    }
                }
            }
        }
    }

    fn prepare_scene(&mut self, s: Scene) {
        let _scene_span = crate::perf::SpanGuard::with_arg(
            crate::perf::SpanKind::RenderScene,
            s.epoch,
        );
        self.gesture_recorded = false;
        crate::perf::record_instant(
            "SCENE",
            "render",
            format!("seq={} tiles={}", s.epoch, s.tiles.len()),
        );
        let handle_start = Instant::now();
        self.verify = None;
        self.origins.clear();
        for (&k, &v) in &self.current {
            self.origins.insert(k, v);
        }
        for t in &s.tiles {
            let o = *self
                .origins
                .entry(t.id)
                .or_insert_with(|| win32::frame(t.id).unwrap_or(t.rect));
            // A window already within 2px of its target is settled; animating
            // (and parking) it for a rounding discrepancy is pure noise.
            if (o.x - t.rect.x).abs() <= 2
                && (o.y - t.rect.y).abs() <= 2
                && (o.w - t.rect.w).abs() <= 2
                && (o.h - t.rect.h).abs() <= 2
            {
                self.origins.insert(t.id, t.rect);
            }
        }
        {
            let _hspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderHosts);
            // Capture sources belong to the host/D3D device for a given monitor geometry; dropping a
            // removed/resized Host automatically releases its frame pools, textures, and cooldowns.
            self.hosts
                .retain(|k, h| s.monitors.iter().any(|(n, _, b)| n == k && b == &h.b));
            for (name, host) in &mut self.hosts {
                host.pictures.retain(|&(id, c), _| {
                    s.tiles
                        .iter()
                        .any(|t| t.id == id && t.cookie == c && &t.monitor == name)
                });
                host.failed_sources.retain(|&(id, c), _| {
                    s.tiles
                        .iter()
                        .any(|t| t.id == id && t.cookie == c && &t.monitor == name)
                });
            }
            for (n, _, b) in s.monitors.iter() {
                if !self.hosts.contains_key(n) {
                    let generation = self.next_host_gen;
                    self.next_host_gen += 1;
                    if let Ok(h) = Host::new(generation, *b) {
                        self.hosts.insert(n.clone(), h);
                    }
                }
            }
        }
        let hosts_ms = handle_start.elapsed().as_millis();
        {
            let _sspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderSources);
            const WGC_RETRY_COOLDOWN: Duration = Duration::from_secs(5);
            for tile in &s.tiles {
                if tile.pinned {
                    continue;
                }
                let Some(host) = self.hosts.get_mut(&tile.monitor) else {
                    continue;
                };
                let key = (tile.id, tile.cookie);
                if host.pictures.contains_key(&key)
                    || host
                        .failed_sources
                        .get(&key)
                        .is_some_and(|at| at.elapsed() < WGC_RETRY_COOLDOWN)
                {
                    continue;
                }
                // Capture sources take 100-150 ms to create (6 render freezes in the first feel
                // run): a worker makes them; until one is ready the DWM thumbnail stands in.
                if !host.pending_sources.contains(&key)
                    && let Some(surface) = host.surface.as_ref()
                {
                    host.pending_sources.insert(key);
                    let _ = self.source_tx.send((
                        tile.monitor.clone(),
                        host.generation,
                        key,
                        SendDevice(surface.gpu.clone()),
                    ));
                }
            }
        }
        let sources_ms = handle_start.elapsed().as_millis();
        {
            let _fspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderFrames);
            let mut dead_gen: Option<u64> = None;
            for host in self.hosts.values_mut() {
                let Some(surface) = &host.surface else {
                    continue;
                };
                for (&(id, _), source) in &mut host.pictures {
                    if source.bitmap.is_none()
                        && win32::frame(id)
                            .and_then(|r| intersect(r, host.b))
                            .is_some()
                    {
                        // One attempt, no waiting (it used to sleep up to 70 ms per source for a
                        // first frame); without a bitmap the DWM thumbnail stands in.
                        if let Err(e) = source.update(surface, 0)
                            && (is_device_lost_hr(e.code().0)
                                || unsafe { surface.gpu.GetDeviceRemovedReason() }.is_err())
                        {
                            dead_gen = Some(surface.device_gen);
                            break;
                        }
                    }
                }
            }
            if let Some(dg) = dead_gen {
                self.invalidate_device_generation(dg);
            }
        }
        if handle_start.elapsed().as_millis() >= 30 {
            diag::trace(format!(
                "SLOW render scene handling: hosts {hosts_ms} ms, sources {} ms, frames {} ms",
                sources_ms - hosts_ms,
                handle_start.elapsed().as_millis() - sources_ms
            ))
        }
        if !s.live {
            self.live_ids.clear();
            self.last_live_present = None;
        }
        self.open_landing = None;
        self.open_landed = None;
        self.open_timed_out = false;
        self.anim_done = false;
        self.cover_sent.clear();
        self.cover_timed_out_ids.clear();
        self.cover_landed
            .retain(|(id, _), _| s.tiles.iter().any(|t| t.id == *id));
        self.cover_done
            .retain(|id, _| s.tiles.iter().any(|t| t.id == *id));
        self.live_refresh_us =
            planor::pacing::effective_refresh_us(win32::dwm_refresh_period_us(), None);
        self.refresh_checked = Instant::now();
        self.begin = Instant::now();
        self.last = self.begin;
        self.frame_of_scene = 0;
        self.scene = Some(s);
    }

    fn update_idle_sources(&mut self) {
        let mut dead_gen: Option<u64> = None;
        for host in self.hosts.values_mut() {
            let Some(surface) = &host.surface else {
                continue;
            };
            for (&(id, _), source) in &mut host.pictures {
                if let Some(r) = win32::frame(id)
                    && intersect(r, host.b) == Some(r)
                    && let Err(e) = source.update(surface, self.settled)
                    && (is_device_lost_hr(e.code().0)
                        || unsafe { surface.gpu.GetDeviceRemovedReason() }.is_err())
                {
                    dead_gen = Some(surface.device_gen);
                    break;
                }
            }
        }
        if let Some(dg) = dead_gen {
            self.invalidate_device_generation(dg);
        }
    }

    unsafe fn build_all_hosts(&mut self, s: &Scene, t: f64, park: i32, open_handoff: bool) -> bool {
        unsafe {
            let scratch = &mut self.scratch;
            scratch.placements.clear();
            scratch.cover_placements.clear();
            scratch.settled_tiles.clear();
            scratch.hits.clear();
            scratch.next.clear();
            let mut any_cover_active = false;
            let mut invalidated_device_gen: Option<u64> = None;
            for (name, host) in &mut self.hosts {
                if let Some(dead_gen) = invalidated_device_gen
                    && host.surface.as_ref().is_some_and(|s| s.device_gen == dead_gen)
                {
                    host.invalidate_gpu(&mut self.next_host_gen);
                }
                scratch.lines.clear();
                scratch.backing.clear();
                scratch.cards.clear();
                scratch.keep.clear();
                scratch.host_hits.clear();
                scratch.thumb_rects.clear();
                let mut full = false;
                let mut source_device_lost: Option<u64> = None;
                for tile in s.tiles.iter().filter(|tile| &tile.monitor == name) {
                    if GetPropW(tile.id as _, self.tag.as_ptr()) as usize != tile.cookie
                        || IsIconic(tile.id as _) != 0
                    {
                        continue;
                    }
                    let bound = intersect(tile.bound, host.b).unwrap_or(host.b);
                    let can_cover = !tile.pinned
                        && !tile.hidden
                        && !tile.preview
                        && !tile.fullscreen
                        && !s.live
                        && intersect(tile.rect, bound) == Some(tile.rect);
                    let mut covered_visual = false;
                    let mut skip_cover_motion = false;
                    if can_cover && let Some(started) = tile.cover {
                        if self.cover_done.get(&tile.id) == Some(&started) {
                            skip_cover_motion = true;
                        } else {
                            let frame_ready = win32::frame(tile.id).is_some_and(|got| {
                                (got.x - tile.rect.x).abs() <= 2
                                    && (got.y - tile.rect.y).abs() <= 2
                                    && got.w >= tile.rect.w - 2
                                    && got.h >= tile.rect.h - 2
                            });
                            let landed_for_ms = if frame_ready {
                                let at = *self
                                    .cover_landed
                                    .entry((tile.id, started))
                                    .or_insert_with(Instant::now);
                                Some(at.elapsed().as_millis() as u64)
                            } else {
                                None
                            };
                            let source_size_matches = host
                                .pictures
                                .get(&(tile.id, tile.cookie))
                                .is_some_and(|src| {
                                    src.bitmap.is_some()
                                        && (src.size.Width - tile.rect.w).abs() <= 24
                                        && (src.size.Height - tile.rect.h).abs() <= 24
                                });
                            let vis = planor::cover::eval_cover(
                                self.cover_cfg,
                                started.elapsed().as_millis() as u64,
                                landed_for_ms,
                                source_size_matches,
                            );
                            if vis.active {
                                covered_visual = true;
                                any_cover_active = true;
                                scratch.next.insert(tile.id, tile.rect);
                                scratch.cards.push(CardDraw {
                                    rect: Rect {
                                        x: tile.rect.x - host.b.x,
                                        y: tile.rect.y - host.b.y,
                                        ..tile.rect
                                    },
                                    spinner_alpha: vis.spinner_alpha,
                                    spinner_angle_rad: vis.spinner_angle_rad,
                                });
                                if let Some(color) = tile.color {
                                    scratch.lines.push((
                                        Rect {
                                            x: tile.rect.x - host.b.x,
                                            y: tile.rect.y - host.b.y,
                                            ..tile.rect
                                        },
                                        color,
                                    ));
                                }
                                scratch.cover_placements.push((tile.id, tile.rect));
                                if t >= 1. && !s.live {
                                    scratch.settled_tiles.push(planor::SettledTile {
                                        id: tile.id,
                                        rect: tile.rect,
                                        bound,
                                        expect_on_screen: false,
                                        focused: false,
                                    });
                                }
                            } else {
                                self.cover_done.insert(tile.id, started);
                                self.cover_landed.remove(&(tile.id, started));
                                skip_cover_motion = true;
                                if vis.timed_out {
                                    self.cover_timed_out_ids.insert(tile.id);
                                }
                            }
                        }
                    }
                    if covered_visual {
                        continue;
                    }
                    let (o, r) = if skip_cover_motion {
                        (tile.rect, tile.rect)
                    } else {
                        (self.origins[&tile.id], interpolate(self.origins[&tile.id], tile.rect, t))
                    };
                    scratch.next.insert(tile.id, r);
                    let moving = !skip_cover_motion && t < 1. && o != tile.rect;
                    let live_proxy = s.live
                        && !tile.pinned
                        && (self.live_ids.contains(&tile.id) || self.origins[&tile.id] != tile.rect);
                    if live_proxy {
                        self.live_ids.insert(tile.id);
                    }
                    if !moving && tile.fullscreen && tile.rect == host.b {
                        full = true
                    }
                    // Keep the drawing stand-in rect (`rs`) and the parking decision (`tile.rect` vs
                    // `bound`) separate, and restrict `open_handoff` stand-ins to tiles that moved.
                    let (rs, clip, park_window, proxy) = tile_stand_in_and_park(
                        o,
                        r,
                        tile.rect,
                        bound,
                        t,
                        open_handoff,
                        moving,
                        live_proxy,
                        tile.pinned,
                        tile.preview,
                        tile.hidden,
                    );
                    if let Some(c) = clip {
                        if proxy
                            && let (Some(source), Some(surface)) = (
                                host.pictures.get_mut(&(tile.id, tile.cookie)),
                                host.surface.as_ref(),
                            )
                        {
                            if source.bitmap.is_none()
                                && let Err(e) = source.update(surface, 0)
                            {
                                if is_device_lost_hr(e.code().0)
                                    || surface.gpu.GetDeviceRemovedReason().is_err()
                                {
                                    source_device_lost = Some(surface.device_gen);
                                } else {
                                    eprintln!("source frame: {e}");
                                }
                            }
                            if source_device_lost.is_none()
                                && let Some(bitmap) = &source.bitmap
                            {
                                scratch.backing.push((
                                    Rect {
                                        x: rs.x - host.b.x,
                                        y: rs.y - host.b.y,
                                        ..rs
                                    },
                                    bitmap.clone(),
                                ));
                            }
                        }
                        if proxy && host.thumbnail(tile.id, rs, c) {
                            let hidden_behind_wgc = moving
                                && host
                                    .pictures
                                    .get(&(tile.id, tile.cookie))
                                    .is_some_and(|src| src.bitmap.is_some());
                            if hidden_behind_wgc {
                                let p = DWM_THUMBNAIL_PROPERTIES {
                                    dwFlags: DWM_TNP_VISIBLE,
                                    fVisible: 0,
                                    ..Default::default()
                                };
                                DwmUpdateThumbnailProperties(host.thumbs[&tile.id], &p);
                            } else {
                                scratch.thumb_rects.push(c);
                            }
                            scratch.keep.insert(tile.id);
                            if !moving {
                                scratch.host_hits.push((c, tile.id))
                            }
                        }
                        if let Some(color) = tile.color {
                            // The border follows what is drawn: during motion that is the stand-in (`rs`,
                            // the window's real size gliding), not the in-between rectangle, which grew or
                            // shrank ahead of the picture ("border grows faster than the animation").
                            let b = if proxy { rs } else { r };
                            scratch.lines.push((
                                Rect {
                                    x: b.x - host.b.x,
                                    y: b.y - host.b.y,
                                    ..b
                                },
                                color,
                            ))
                        }
                    }
                    let target = if park_window {
                        Rect {
                            x: park,
                            ..tile.rect
                        }
                    } else {
                        tile.rect
                    };
                    if !tile.pinned {
                        scratch.placements.push((tile.id, target));
                        if t >= 1. && !s.live {
                            scratch.settled_tiles.push(planor::SettledTile {
                                id: tile.id,
                                rect: tile.rect,
                                bound,
                                expect_on_screen: !proxy,
                                focused: !proxy && tile.color.is_some(),
                            });
                        }
                    }
                }
                host.thumbs.retain(|id, h| {
                    if scratch.keep.contains(id) {
                        true
                    } else {
                        DwmUnregisterThumbnail(*h);
                        false
                    }
                });
                scratch.mine.clear();
                scratch.local_holes.clear();
                for h in s.holes.iter() {
                    if let Some(r) = intersect(*h, host.b) {
                        scratch.mine.push(r);
                        scratch.local_holes.push(Rect {
                            x: r.x - host.b.x,
                            y: r.y - host.b.y,
                            ..r
                        });
                    }
                }
                if !full {
                    if scratch.mine.is_empty() {
                        scratch.hits.append(&mut scratch.host_hits);
                    } else {
                        for (hit_rect, id) in scratch.host_hits.drain(..) {
                            for piece in crate::daemon::subtract_rects(hit_rect, &scratch.mine) {
                                scratch.hits.push((piece, id));
                            }
                        }
                    }
                }
                scratch.fills.clear();
                for z in &s.zones {
                    if &z.monitor == name
                        && let Some(r) = intersect(z.rect, host.b)
                    {
                        scratch.fills.push((
                            Rect {
                                x: r.x - host.b.x,
                                y: r.y - host.b.y,
                                ..r
                            },
                            z.color,
                            z.alpha,
                        ));
                    }
                }
                if let Some(dead_gen) = source_device_lost {
                    reset_shared_devices_if_generation(dead_gen);
                    invalidated_device_gen = Some(dead_gen);
                    host.invalidate_gpu(&mut self.next_host_gen);
                    scratch.backing.clear();
                }
                let dpi = GetDpiForWindow(host.h) as f32 / 96.;
                let width = s.width * dpi;
                let radius = s.radius * dpi;
                let has_d2d = !scratch.lines.is_empty()
                    || !scratch.fills.is_empty()
                    || !scratch.cards.is_empty()
                    || !scratch.backing.is_empty();
                let want_visible = !full && (!scratch.keep.is_empty() || has_d2d);
                if !want_visible {
                    clear_hidden_frame_state(
                        &mut host.last_draw,
                        &mut host.want_visible,
                        &mut host.last_draw_valid,
                        &mut host.desired_rgn_holes,
                    );
                    if host.visible {
                        ShowWindow(host.h, SW_HIDE);
                        host.visible = false;
                    }
                } else {
                    host.want_visible = true;
                    // Try recreating a lost GPU surface before drawing (do NOT reset recovery
                    // backoff until a full normal draw succeeds).
                    let now = Instant::now();
                    if host.surface.is_none()
                        && invalidated_device_gen.is_none()
                        && host.recovery.can_try(now)
                    {
                        match Surface::new(host.h, host.b) {
                            Ok(s) => {
                                host.surface = Some(s);
                                host.last_draw_valid = false;
                            }
                            Err(e) => {
                                if host.recovery.record_failure(now) {
                                    diag::trace(format!("overlay GPU surface create failed: {e}"));
                                }
                            }
                        }
                    }
                    // Always record the desired frame (even when `surface` is None or `backing`
                    // pictures are drawn) so idle recovery restores the latest borders/holes.
                    if host.surface.is_none() {
                        record_visible_frame_state(
                            &mut host.last_draw,
                            &mut host.want_visible,
                            &mut host.last_draw_valid,
                            &scratch.fills,
                            &scratch.cards,
                            &scratch.lines,
                            &scratch.local_holes,
                            width,
                            radius,
                            dpi,
                            !scratch.backing.is_empty(),
                            false,
                        );
                    }
                    // 1. Draw and commit BEFORE updating region or showing the overlay window.
                    if let Some(surface) = host.surface.as_mut() {
                        let sig_matches = scratch.backing.is_empty()
                            && host.last_draw_valid
                            && host.last_draw.matches(
                                &scratch.fills,
                                &scratch.cards,
                                &scratch.lines,
                                &scratch.local_holes,
                                width,
                                radius,
                                dpi,
                            );
                        let removed_before_skip =
                            sig_matches && surface.gpu.GetDeviceRemovedReason().is_err();
                        if !sig_matches || removed_before_skip {
                            let frame = DrawFrame {
                                pictures: &scratch.backing,
                                fills: &scratch.fills,
                                cards: &scratch.cards,
                                lines: &scratch.lines,
                                holes: &scratch.local_holes,
                                width,
                                radius,
                                dpi,
                            };
                            let draw_start = Instant::now();
                            let drawn = if removed_before_skip {
                                Err(windows::core::Error::from(
                                    surface.gpu.GetDeviceRemovedReason().unwrap_err(),
                                )
                                .into())
                            } else {
                                let _dspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderDraw);
                                surface.draw(&frame)
                            };
                            if draw_start.elapsed().as_millis() >= 100 {
                                diag::trace(format!("SLOW render draw: {} ms", draw_start.elapsed().as_millis()))
                            }
                            if let Err(e) = drawn {
                                let removed_reason = surface.gpu.GetDeviceRemovedReason();
                                let device_lost =
                                    is_device_lost_error(e.as_ref()) || removed_reason.is_err();
                                let dead_gen = surface.device_gen;
                                let picture_only = !device_lost && !scratch.backing.is_empty();
                                if device_lost {
                                    reset_shared_devices_if_generation(dead_gen);
                                    invalidated_device_gen = Some(dead_gen);
                                    host.invalidate_gpu(&mut self.next_host_gen);
                                } else if picture_only {
                                    // Picture-only failure: drop capture sources and bump host
                                    // generation, then fall back to border-only with backoff.
                                    bump_host_generation(&mut host.generation, &mut self.next_host_gen);
                                    host.pictures.clear();
                                    host.pending_sources.clear();
                                    host.last_draw_valid = false;
                                } else {
                                    host.invalidate_gpu(&mut self.next_host_gen);
                                }
                                let retry_frame = DrawFrame {
                                    pictures: &[],
                                    fills: &scratch.fills,
                                    cards: &scratch.cards,
                                    lines: &scratch.lines,
                                    holes: &scratch.local_holes,
                                    width,
                                    radius,
                                    dpi,
                                };
                                // Immediate single recovery attempt only on the first failure
                                // (`attempts == 0`). A picture-only fallback is degraded and must
                                // still record a failure so it backs off instead of recreating
                                // every frame.
                                let immediate_ok = if host.recovery.attempts == 0 {
                                    if let Some(existing) = host.surface.as_mut() {
                                        existing.draw(&retry_frame).is_ok()
                                    } else {
                                        match Surface::new(host.h, host.b) {
                                            Ok(mut fresh) => {
                                                if fresh.draw(&retry_frame).is_ok() {
                                                    host.surface = Some(fresh);
                                                    true
                                                } else {
                                                    if device_lost {
                                                        reset_shared_devices_if_generation(fresh.device_gen);
                                                    }
                                                    false
                                                }
                                            }
                                            Err(_) => false,
                                        }
                                    }
                                } else {
                                    false
                                };
                                record_visible_frame_state(
                                    &mut host.last_draw,
                                    &mut host.want_visible,
                                    &mut host.last_draw_valid,
                                    &scratch.fills,
                                    &scratch.cards,
                                    &scratch.lines,
                                    &scratch.local_holes,
                                    width,
                                    radius,
                                    dpi,
                                    !scratch.backing.is_empty(),
                                    immediate_ok,
                                );
                                if immediate_ok && scratch.backing.is_empty() {
                                    host.recovery.reset();
                                    diag::trace(format!(
                                        "overlay GPU surface recovered immediately after draw error: {e}"
                                    ));
                                } else {
                                    if host.recovery.record_failure(Instant::now()) {
                                        let reason_str = match removed_reason {
                                            Err(r) => format!(" (reason: {r})"),
                                            Ok(()) => String::new(),
                                        };
                                        diag::trace(format!("border draw failed: {e}{reason_str}; scheduling recovery"));
                                        eprintln!("border draw: {e}{reason_str}");
                                    }
                                }
                            } else {
                                let was_outage = host.recovery.logged;
                                host.recovery.reset();
                                if was_outage {
                                    diag::trace("overlay GPU surface recovered");
                                }
                                // Always record the desired frame even when `backing` pictures
                                // were drawn (e.g. settled peek/clipped proxies), while keeping
                                // `last_draw_valid = false` when `backing` is non-empty.
                                record_visible_frame_state(
                                    &mut host.last_draw,
                                    &mut host.want_visible,
                                    &mut host.last_draw_valid,
                                    &scratch.fills,
                                    &scratch.cards,
                                    &scratch.lines,
                                    &scratch.local_holes,
                                    width,
                                    radius,
                                    dpi,
                                    !scratch.backing.is_empty(),
                                    true,
                                );
                            }
                        }
                    }
                    // 2. Direct2D borders/zones/cards are masked inside `Surface::draw`. Only use
                    // `SetWindowRgn` (with bRedraw = 0 to prevent DWM flashing) when an active
                    // DWM thumbnail actually overlaps a hole. Store `desired_rgn_holes` on the host
                    // so idle GPU recovery restores it without clearing active thumbnail holes.
                    holes_intersecting_rects(&scratch.mine, &scratch.thumb_rects, &mut scratch.rgn_holes);
                    host.desired_rgn_holes.clear();
                    host.desired_rgn_holes.extend_from_slice(&scratch.rgn_holes);
                    host.apply_rgn_holes(&scratch.rgn_holes);
                    // 3. Show the overlay window ONLY after content is drawn/committed and region
                    // is applied, so stale borders or holes are never composed for a frame.
                    let can_show = !scratch.keep.is_empty() || host.surface.is_some();
                    if can_show && !host.visible {
                        ShowWindow(host.h, SW_SHOWNOACTIVATE);
                        host.visible = true;
                    } else if !can_show && host.visible {
                        ShowWindow(host.h, SW_HIDE);
                        host.visible = false;
                    }
                }
            }
            if let Some(dead_gen) = invalidated_device_gen {
                for host in self.hosts.values_mut() {
                    if host.surface.as_ref().is_some_and(|s| s.device_gen == dead_gen) {
                        host.invalidate_gpu(&mut self.next_host_gen);
                    }
                }
            }
            if self.last_peeks.as_deref() != Some(scratch.hits.as_slice()) {
                win32::set_peeks(scratch.hits.clone());
                let slot = self.last_peeks.get_or_insert_with(Vec::new);
                slot.clear();
                slot.extend_from_slice(&scratch.hits);
            }
            any_cover_active
        }
    }

    unsafe fn apply_placements(
        &mut self,
        s: &Scene,
        t: f64,
        park: i32,
        open_handoff: bool,
        any_cover_active: bool,
    ) -> (u32, u64) {
        unsafe {
            let scratch = &mut self.scratch;
            if !scratch.cover_placements.is_empty() {
                scratch.unsent_covers.clear();
                for &(id, rect) in &scratch.cover_placements {
                    if self.cover_sent.insert((s.epoch, id)) {
                        scratch.unsent_covers.push((id, rect));
                    }
                }
                if !scratch.unsent_covers.is_empty() {
                    win32::place_async(&scratch.unsent_covers);
                }
            }
            scratch.parking.clear();
            scratch.landing.clear();
            for &(id, rect) in &scratch.placements {
                if rect.x == park {
                    scratch.parking.push((id, rect));
                } else {
                    scratch.landing.push((id, rect));
                }
            }
            let start = Instant::now();
            // Hole-only and focus-only refreshes carry `action == "idle"` and `!force_place`:
            // skip OS window placement only when every target (including parked windows at `park`)
            // already matches its actual `win32::frame` within 2 px.
            if all_targets_already_placed(
                s.action,
                s.force_place,
                !scratch.cover_placements.is_empty(),
                &scratch.placements,
                win32::frame,
            ) {
                return (0, 0);
            }
            let mut retries = 0u32;
            let mut place_ok = false;
            scratch.rejected.clear();
            while s.epoch == EPOCH.load(std::sync::atomic::Ordering::Acquire) {
                if s.diagnostics && retries == 0 {
                    for &(id, want) in &scratch.placements {
                        if let Some(got) = win32::frame(id)
                            && got != want
                        {
                            diag::trace(format!("PLACE {id} {:?} -> {:?}", (got.x, got.y, got.w, got.h), (want.x, want.y, want.w, want.h)));
                        }
                    }
                }
                let placed_at = Instant::now();
                // Parking never waits for the app. The last on-screen move of an open/close animation is
                // asynchronous too: its thumbnail remains above it until `open_landed` confirms arrival.
                let moved_landing: Vec<(isize, Rect, Rect)> = if s.action == "open_close" && t >= 1. {
                    scratch
                        .landing
                        .iter()
                        .copied()
                        .filter_map(|(id, rect)| {
                            let origin = self.origins.get(&id).copied().unwrap_or(rect);
                            (origin != rect).then_some((id, origin, rect))
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let animated_open_landing = !moved_landing.is_empty();
                if animated_open_landing
                    && self.open_landed != Some(s.epoch)
                    && self.open_landing.as_ref().is_none_or(|ol| ol.epoch != s.epoch)
                {
                    let to_place: Vec<Placement> =
                        moved_landing.iter().map(|&(id, _, want)| (id, want)).collect();
                    win32::place_async(&to_place);
                    self.open_landing = Some(OpenLanding {
                        epoch: s.epoch,
                        started: Instant::now(),
                        items: moved_landing,
                        stable: HashMap::new(),
                    });
                }
                if retries == 0 {
                    win32::place_async(&scratch.parking);
                }
                let result = if scratch.landing.is_empty() || (animated_open_landing && retries == 0) {
                    Ok(())
                } else {
                    // Constraint/rejection handling remains synchronous, as does final placement for
                    // every non-open/close action. The existing delayed verify corrects a timed-out handoff.
                    win32::place(&scratch.landing)
                };
                if placed_at.elapsed().as_millis() >= 100 {
                    diag::trace(format!("SLOW render place: {} ms", placed_at.elapsed().as_millis()))
                }
                scratch.rejected.clear();
                if t >= 1. && !open_handoff && !self.open_timed_out && result.is_ok() {
                    for &(id, want) in &scratch.placements {
                        if want.x != park
                            && !self.cover_timed_out_ids.contains(&id)
                            && let Some(got) = win32::frame(id)
                            && (got.w > want.w + 1 || got.h > want.h + 1)
                        {
                            scratch.rejected.push((id, got))
                        }
                    }
                }
                if result.is_ok() && scratch.rejected.is_empty() {
                    place_ok = true;
                    break;
                }
                let e = result
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "application constrained placement".into());
                if retries == 2 {
                    eprintln!("DeferWindowPos: {e} at {}:{}", file!(), line!());
                    break;
                }
                std::thread::sleep(Duration::from_millis(2 << retries));
                retries += 1
            }
            if !scratch.rejected.is_empty() {
                win32::emit(win32::Event::Rejected(s.epoch, scratch.rejected.clone()));
            }
            if place_ok && (!scratch.placements.is_empty() || !scratch.cover_placements.is_empty() || t >= 1.) {
                crate::perf::mark_first_place_done();
            }
            if place_ok
                && !self.gesture_recorded
                && let Some(in_qpc) = s.input_qpc_us
            {
                self.gesture_recorded = true;
                crate::perf::record_duration_from(
                    crate::perf::SpanKind::GestureToPlace,
                    in_qpc,
                    s.epoch,
                );
            }
            let waiting_open_landing = self
                .open_landing
                .as_ref()
                .is_some_and(|ol| ol.epoch == s.epoch);
            if place_ok
                && t >= 1.
                && !waiting_open_landing
                && !any_cover_active
                && scratch.rejected.is_empty()
                && self.parked_checked_epoch != s.epoch
                && !scratch.settled_tiles.is_empty()
            {
                self.parked_checked_epoch = s.epoch;
                scratch.actuals.clear();
                for st in &scratch.settled_tiles {
                    if s.tiles.iter().any(|tile| {
                        tile.id == st.id
                            && GetPropW(tile.id as _, self.tag.as_ptr()) as usize == tile.cookie
                            && IsIconic(tile.id as _) == 0
                    }) && let Some(r) = win32::frame(st.id)
                    {
                        scratch.actuals.push((st.id, r));
                    }
                }
                let violations = planor::check_settled_parked(&scratch.settled_tiles, &scratch.actuals, park);
                for v in violations {
                    crate::perf::record_instant("INVARIANT", "invariant", v.trace_line());
                    if s.diagnostics {
                        diag::trace(v.trace_line());
                    }
                    if std::env::var_os("PLANOR_NO_INVARIANT_PANIC").is_none() {
                        debug_assert!(false, "{}", v.trace_line());
                    }
                }
            }
            (retries, start.elapsed().as_micros() as u64)
        }
    }

    unsafe fn post_present(&mut self, s: Scene, info: &PresentInfo) {
        let &PresentInfo {
            t,
            park,
            any_cover_active,
            time,
            render_start,
            retries,
            placement_us,
        } = info;
        unsafe {
            std::mem::swap(&mut self.current, &mut self.scratch.next);
            {
                let _fspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderDwmFlush);
                DwmFlush();
            }
            if s.diagnostics {
                win32::emit(win32::Event::Frame(diag::Frame {
                    input_to_present_us: s.input.map(|v| v.elapsed().as_micros() as u64),
                    interval_us: self.last.elapsed().as_micros() as _,
                    render_us: time.elapsed().as_micros() as _,
                    placement_us,
                    thumbnail_us: render_start.elapsed().as_micros() as _,
                    retries,
                }));
            }
            // Animation smoothness: the interval between two presented frames of the same animated scene. A
            // frame later than 1.5 refresh periods is a visible hitch.
            if s.duration > 0 && self.frame_of_scene > 0 && !self.anim_done {
                crate::perf::record_frame_with_refresh(
                    s.action,
                    self.last.elapsed().as_micros() as u64,
                    self.live_refresh_us,
                );
            }
            if t >= 1. {
                self.anim_done = true;
            }
            if s.live && self.frame_of_scene == 0 {
                // Under the fingers every scene is one frame: the gap between their presents is the smoothness.
                let cutoff = Duration::from_millis(planor::pacing::live_gap_cutoff_ms(self.live_refresh_us));
                if let Some(prev) = self.last_live_present.filter(|p| p.elapsed() < cutoff) {
                    crate::perf::record_frame_with_refresh(
                        "live",
                        prev.elapsed().as_micros() as u64,
                        self.live_refresh_us,
                    );
                }
                self.last_live_present = Some(Instant::now());
            }
            self.frame_of_scene += 1;
            self.last = Instant::now();
            let waiting_open_landing = self
                .open_landing
                .as_ref()
                .is_some_and(|ol| ol.epoch == s.epoch);
            if t >= 1. && !waiting_open_landing && !any_cover_active {
                self.settled =
                    self.timestamp() + planor::pacing::settled_capture_delay_100ns(self.live_refresh_us);
                let current_epoch = s.epoch == EPOCH.load(std::sync::atomic::Ordering::Acquire);
                if current_epoch {
                    self.scratch.verify_items.clear();
                    for &(id, rect) in &self.scratch.placements {
                        if rect.x != park {
                            self.scratch.verify_items.push((id, rect));
                        }
                    }
                    self.scratch
                        .verify_items
                        .extend_from_slice(&self.scratch.cover_placements);
                    let mut timed_out_ids = self.cover_timed_out_ids.clone();
                    if self.open_timed_out {
                        for &(id, _) in &self.scratch.verify_items {
                            timed_out_ids.insert(id);
                        }
                    }
                    self.verify = Some(VerifyState {
                        epoch: s.epoch,
                        at: Instant::now() + Duration::from_millis(250),
                        items: std::mem::take(&mut self.scratch.verify_items),
                        origins: self.origins.clone(),
                        timed_out_ids,
                        prev_frames: HashMap::new(),
                        left: 2,
                    });
                }
                // Keep the settled scene so future `Message::OverlayUpdate` patches can repaint
                // holes and border colours immediately without bumping epoch or placing windows.
                self.last_settled_scene = Some(s);
                self.scene = None;
            } else {
                self.scene = Some(s);
            }
        }
    }
}

pub static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn start() -> crossbeam_channel::Sender<Message> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::spawn(move || unsafe {
        crate::perf::register_thread("render");
        crate::priority::thread_highest();
        // MMCSS gives this thread's GPU submissions scheduling priority (overlay draws compete with
        // every other process's GPU work); held until the render thread exits.
        let _mmcss = crate::priority::Mmcss::acquire();
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
        let (source_tx, source_jobs) = crossbeam_channel::unbounded::<SourceJob>();
        let (source_done_tx, source_done) = crossbeam_channel::unbounded::<SourceDone>();
        std::thread::Builder::new()
            .name("Planor.Capture".into())
            .spawn(move || {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
                for (mon, generation, key, device) in source_jobs {
                    let t = Instant::now();
                    let made = Source::new(key.0, &device.0).map(SendSource).map_err(|e| e.to_string());
                    if source_done_tx
                        .send((mon, generation, key, made, t.elapsed().as_millis()))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok();
        let mut state = RenderState::new(source_tx, source_done);
        let mut hi_res: Option<win32::HighResTimer> = None;
        loop {
            crate::perf::heartbeat_render();
            state.poll_background_work();
            // 1 ms scheduler resolution while an animated scene is in flight: placement retries sleep
            // 2-8 ms, which the default ~15.6 ms granularity would round up into visible hitches.
            if state.scene.is_some() {
                if hi_res.is_none() {
                    hi_res = Some(win32::HighResTimer::acquire());
                }
            } else {
                hi_res = None;
            }
            let timeout = if state.scene.is_some() {
                Duration::ZERO
            } else if state.has_active_sources() || state.verify.is_some() {
                Duration::from_millis(33)
            } else if state.needs_surface_recovery() {
                Duration::from_millis(250)
            } else {
                Duration::from_millis(50)
            };
            match rx.recv_timeout(timeout) {
                Ok(Message::Stop(ack)) => {
                    win32::set_peeks(Vec::new());
                    state.last_peeks = None;
                    state.hosts.clear();
                    let _ = ack.send(());
                    break;
                }
                Ok(Message::Warm(monitors)) => {
                    warm_hosts(monitors, &mut state.hosts, &mut state.next_host_gen);
                    continue;
                }
                Ok(Message::Clear(ack)) => {
                    state.clear_state();
                    let _ = ack.send(());
                    continue;
                }
                Ok(Message::OverlayUpdate { holes, colors, .. }) => {
                    if let Some(active) = state.scene.as_mut() {
                        apply_overlay_update(active, holes, &colors);
                    } else if let Some(mut settled) = state.last_settled_scene.take() {
                        apply_overlay_update(&mut settled, holes, &colors);
                        settled.epoch = EPOCH.load(std::sync::atomic::Ordering::Acquire);
                        settled.action = "idle";
                        settled.duration = 0;
                        settled.live = false;
                        settled.force_place = false;
                        settled.zones.clear();
                        state.scene = Some(settled);
                    }
                }
                Ok(Message::Scene(mut s)) => {
                    // Drain any newer queued scenes so a burst of input/events skips stale frames
                    // instead of paying setup work for a scene that has already been superseded.
                    let mut stop = false;
                    let mut cleared = false;
                    while let Ok(next_msg) = rx.try_recv() {
                        match next_msg {
                            Message::Scene(newer) => s = newer,
                            Message::OverlayUpdate { holes, colors, .. } => {
                                apply_overlay_update(&mut s, holes, &colors);
                            }
                            Message::Warm(monitors) => {
                                warm_hosts(monitors, &mut state.hosts, &mut state.next_host_gen)
                            }
                            Message::Clear(ack) => {
                                state.clear_state();
                                let _ = ack.send(());
                                cleared = true;
                                break;
                            }
                            Message::Stop(ack) => {
                                win32::set_peeks(Vec::new());
                                state.last_peeks = None;
                                state.hosts.clear();
                                let _ = ack.send(());
                                stop = true;
                                break;
                            }
                        }
                    }
                    if stop {
                        break;
                    }
                    if cleared {
                        continue;
                    }
                    state.prepare_scene(s);
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                _ => (),
            }
            let Some(s) = state.scene.take() else {
                state.check_device_health();
                state.try_recover_surfaces();
                state.update_idle_sources();
                continue;
            };
            if s.epoch != EPOCH.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            if state.refresh_checked.elapsed() >= Duration::from_millis(100) {
                state.refresh_checked = Instant::now();
                state.live_refresh_us =
                    planor::pacing::effective_refresh_us(win32::dwm_refresh_period_us(), None);
            }
            let time = Instant::now();
            let t = if s.duration == 0 {
                1.
            } else {
                planor::motion(
                    state.begin.elapsed().as_secs_f64() * 1000. / s.duration as f64,
                    s.spring,
                )
            };
            let park = s.monitors.iter().map(|m| m.2.x + m.2.w).max().unwrap_or(0) + 1024;
            let open_handoff = s.action == "open_close"
                && t >= 1.
                && state.open_landed != Some(s.epoch);
            let any_cover_active = state.build_all_hosts(&s, t, park, open_handoff);
            let place_start = Instant::now();
            let (retries, placement_us) =
                state.apply_placements(&s, t, park, open_handoff, any_cover_active);
            state.post_present(
                s,
                &PresentInfo {
                    t,
                    park,
                    any_cover_active,
                    time,
                    render_start: place_start,
                    retries,
                    placement_us,
                },
            );
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_lost_hr_and_recovery_backoff() {
        assert!(is_device_lost_hr(0x887A_0005u32 as i32)); // DXGI_ERROR_DEVICE_REMOVED
        assert!(is_device_lost_hr(0x887A_0006u32 as i32)); // DXGI_ERROR_DEVICE_HUNG
        assert!(is_device_lost_hr(0x887A_0007u32 as i32)); // DXGI_ERROR_DEVICE_RESET
        assert!(is_device_lost_hr(0x887A_0020u32 as i32)); // DXGI_ERROR_DRIVER_INTERNAL_ERROR
        assert!(is_device_lost_hr(0x8899_000Cu32 as i32)); // D2DERR_RECREATE_TARGET
        assert!(!is_device_lost_hr(0));
        assert!(!is_device_lost_hr(0x8000_4005u32 as i32)); // E_FAIL

        let mut rec = SurfaceRecovery::default();
        let t0 = Instant::now();
        assert!(rec.can_try(t0));
        assert!(rec.record_failure(t0), "first failure should log");
        assert!(!rec.can_try(t0 + Duration::from_millis(100)));
        assert!(rec.can_try(t0 + Duration::from_millis(250)));
        assert!(!rec.record_failure(t0 + Duration::from_millis(250)), "subsequent failures suppress log spam");
        assert_eq!(SurfaceRecovery::retry_delay(3), Duration::from_millis(1000));
        assert_eq!(SurfaceRecovery::retry_delay(10), Duration::from_millis(2000));
        rec.reset();
        assert_eq!(rec.attempts, 0);
        assert!(rec.can_try(t0));
        assert!(!rec.logged);
    }

    #[test]
    fn holes_intersecting_rects_only_flags_overlapping_thumbnails() {
        let holes = vec![
            Rect { x: 100, y: 100, w: 300, h: 300 },
            Rect { x: 1000, y: 100, w: 200, h: 200 },
        ];
        let mut out = Vec::new();
        // No thumbnails -> no SetWindowRgn holes needed (prevents DWM black flashes).
        holes_intersecting_rects(&holes, &[], &mut out);
        assert!(out.is_empty());

        // Thumbnail away from all holes -> still no SetWindowRgn holes.
        holes_intersecting_rects(&holes, &[Rect { x: 500, y: 500, w: 100, h: 100 }], &mut out);
        assert!(out.is_empty());

        // Thumbnail overlapping only the second hole -> only that hole requires SetWindowRgn.
        holes_intersecting_rects(&holes, &[Rect { x: 1100, y: 150, w: 150, h: 150 }], &mut out);
        assert_eq!(out, vec![holes[1]]);
    }

    #[test]
    fn stand_in_rect_holds_origin_size_during_motion_and_open_handoff() {
        let origin = Rect { x: 0, y: 0, w: 1000, h: 1200 };
        let current = Rect { x: 50, y: 0, w: 2560, h: 1200 };
        // Mid-animation: keeps origin size at interpolated position.
        assert_eq!(stand_in_rect(origin, current, 0.5, false), Rect { x: 50, y: 0, w: 1000, h: 1200 });
        // At t=1.0 while open/close async handoff is still waiting for real window to arrive: keeps origin size.
        assert_eq!(stand_in_rect(origin, current, 1.0, true), Rect { x: 50, y: 0, w: 1000, h: 1200 });
        // Settled: adopts full target rect.
        assert_eq!(stand_in_rect(origin, current, 1.0, false), current);
    }

    #[test]
    fn draw_sig_detects_hole_changes() {
        let mut sig = DrawSig::default();
        let lines = vec![(Rect { x: 0, y: 0, w: 900, h: 1000 }, 0x3B82F6)];
        let holes_a = vec![Rect { x: 100, y: 100, w: 400, h: 400 }];
        let holes_b = vec![Rect { x: 150, y: 120, w: 400, h: 400 }];
        sig.set_from(&[], &[], &lines, &holes_a, 2.0, 8.0, 1.0);
        assert!(sig.matches(&[], &[], &lines, &holes_a, 2.0, 8.0, 1.0));
        assert!(!sig.matches(&[], &[], &lines, &holes_b, 2.0, 8.0, 1.0));
    }

    #[test]
    fn multi_host_gpu_reset_only_recreates_once_per_generation() {
        // Production `plan_device_invalidation`: 2 hosts sharing device generation 1; host 0 hits
        // D2DERR_RECREATE_TARGET (where GetDeviceRemovedReason may still be OK).
        let mut shared_gen = Some(1u64);
        let mut host_gens = [Some(1u64), Some(1u64)];

        assert!(plan_device_invalidation(&mut shared_gen, &mut host_gens, 1));
        assert_eq!(shared_gen, None);
        assert_eq!(host_gens, [None, None], "all hosts on dead gen 1 must be invalidated together");

        // Host 0 recreates on gen 2.
        shared_gen = Some(2);
        host_gens[0] = Some(2);

        // A second failure referencing dead gen 1 must NOT reset gen 2 or invalidate host 0.
        assert!(!plan_device_invalidation(&mut shared_gen, &mut host_gens, 1));
        assert_eq!(shared_gen, Some(2));
        assert_eq!(host_gens, [Some(2), None]);
    }

    #[test]
    fn want_visible_and_hide_recovery_behavior() {
        let mut sig = DrawSig::default();
        let mut want_visible = false;
        let mut valid = false;
        let mut rgn_holes = vec![Rect { x: 10, y: 10, w: 100, h: 100 }];
        let lines = vec![(Rect { x: 0, y: 0, w: 900, h: 1000 }, 0x3B82F6)];

        // Scene with pictures (settled peek/clipped proxy) updates `last_draw` even while keeping
        // `last_draw_valid == false`, so idle recovery restores the latest border.
        record_visible_frame_state(
            &mut sig,
            &mut want_visible,
            &mut valid,
            &[],
            &[],
            &lines,
            &[],
            2.0,
            8.0,
            1.0,
            true,
            true,
        );
        assert!(want_visible);
        assert!(!valid, "frames with WGC backing pictures keep last_draw_valid=false");
        assert!(should_draw_on_recovery(want_visible, &sig));
        assert!(sig.matches(&[], &[], &lines, &[], 2.0, 8.0, 1.0));

        // Host hides (fullscreen or #48 unfocused) -> clears want_visible, DrawSig, and rgn_holes.
        clear_hidden_frame_state(&mut sig, &mut want_visible, &mut valid, &mut rgn_holes);
        assert!(!want_visible);
        assert!(!valid);
        assert!(rgn_holes.is_empty());
        assert!(!should_draw_on_recovery(want_visible, &sig));
    }

    #[test]
    fn stale_capture_source_rejected_after_generation_bump() {
        let mut host_gen = 1u64;
        let mut next_host_gen = 2u64;
        let in_flight_job_gen = host_gen;
        assert!(accept_source_delivery(host_gen, in_flight_job_gen));

        // GPU invalidation bumps host generation via production helper.
        bump_host_generation(&mut host_gen, &mut next_host_gen);
        assert_eq!(host_gen, 2);
        assert_eq!(next_host_gen, 3);
        assert!(!accept_source_delivery(host_gen, in_flight_job_gen));
        assert!(accept_source_delivery(host_gen, host_gen));
    }

    #[test]
    fn create_ok_draw_fails_backoff_grows_and_suppresses_logs() {
        let mut rec = SurfaceRecovery::default();
        let t0 = Instant::now();

        // Attempt 1: Surface::new succeeds (does NOT reset rec), draw fails -> logs once, 250 ms.
        assert!(rec.can_try(t0));
        assert!(rec.record_failure(t0));
        assert_eq!(rec.attempts, 1);
        assert!(!rec.can_try(t0 + Duration::from_millis(200)));

        // Attempt 2 at +250 ms: create ok, draw fails again -> no duplicate log, 500 ms backoff.
        let t1 = t0 + Duration::from_millis(250);
        assert!(rec.can_try(t1));
        assert!(!rec.record_failure(t1));
        assert_eq!(rec.attempts, 2);
        assert!(!rec.can_try(t1 + Duration::from_millis(400)));

        // Attempt 3 at +500 ms: create ok, draw fails -> 1000 ms backoff.
        let t2 = t1 + Duration::from_millis(500);
        assert!(rec.can_try(t2));
        assert!(!rec.record_failure(t2));
        assert_eq!(rec.attempts, 3);
        assert!(!rec.can_try(t2 + Duration::from_millis(900)));

        // Attempt 4 at +1000 ms: create ok, draw fails -> 2000 ms cap.
        let t3 = t2 + Duration::from_millis(1000);
        assert!(rec.can_try(t3));
        assert!(!rec.record_failure(t3));
        assert_eq!(rec.attempts, 4);
        assert!(!rec.can_try(t3 + Duration::from_millis(1900)));
        assert!(rec.can_try(t3 + Duration::from_millis(2000)));

        // Only a full normal draw resets attempts and logged flag.
        rec.reset();
        assert_eq!(rec.attempts, 0);
        assert!(!rec.logged);
    }

    #[test]
    fn tile_stand_in_and_park_shrink_grow_and_portrait() {
        let bound = Rect { x: 0, y: 0, w: 2560, h: 1392 };

        // 1. Shrinking tile at x > 0 during open_handoff (1988 -> 1277 at x=1283):
        //    rs overflows bound (1283 + 1988 > 2560), but final_rect fits (1283 + 1277 == 2560).
        //    Must NOT be parked (lands on screen), while proxy stays true for the handoff stand-in.
        let origin = Rect { x: 0, y: 0, w: 1988, h: 1392 };
        let final_r = Rect { x: 1283, y: 0, w: 1277, h: 1392 };
        let (rs, draw_clip, park_window, proxy) = tile_stand_in_and_park(
            origin, final_r, final_r, bound, 1.0, true, false, false, false, false, false,
        );
        assert_eq!(rs, Rect { x: 1283, y: 0, w: 1988, h: 1392 });
        assert_eq!(draw_clip, Some(Rect { x: 1283, y: 0, w: 1277, h: 1392 }));
        assert!(!park_window, "shrinking tile whose final rect fits in bound must not be parked");
        assert!(proxy, "moved tile still uses stand-in proxy during open_handoff");

        // 2. Growing tile whose final rect overflows bound (600 -> 1400 at x=1500):
        //    final_rect overflows (1500 + 1400 > 2560), so it MUST be parked AND drawn at its
        //    final size (`rs == grow_final`), not held at the old origin size.
        let grow_origin = Rect { x: 1500, y: 0, w: 600, h: 1392 };
        let grow_final = Rect { x: 1500, y: 0, w: 1400, h: 1392 };
        let (grow_rs, grow_clip, grow_park, grow_proxy) = tile_stand_in_and_park(
            grow_origin, grow_final, grow_final, bound, 1.0, true, false, false, false, false, false,
        );
        assert_eq!(grow_rs, grow_final, "parked tile at t>=1 must draw at final rect, not old origin size");
        assert_eq!(grow_clip, Some(Rect { x: 1500, y: 0, w: 1060, h: 1392 }));
        assert!(grow_park, "growing tile whose final rect overflows bound must be parked");
        assert!(grow_proxy);

        // 3. Unmoved tile during open_handoff must NOT become a thumbnail proxy and keeps its rect.
        let still = Rect { x: 0, y: 0, w: 1277, h: 1392 };
        let (still_rs, _, still_park, still_proxy) = tile_stand_in_and_park(
            still, still, still, bound, 1.0, true, false, false, false, false, false,
        );
        assert_eq!(still_rs, still);
        assert!(!still_park);
        assert!(!still_proxy, "unmoved tile must not create thumbnail proxy during open_handoff");

        // 4. Portrait monitor: shrinking height at y > 0 lands (holds origin size while landing),
        //    growing height past bound parks and draws at final size.
        let p_bound = Rect { x: -1440, y: 0, w: 1440, h: 2512 };
        let p_shrink_o = Rect { x: -1440, y: 0, w: 1440, h: 2000 };
        let p_shrink_f = Rect { x: -1440, y: 1259, w: 1440, h: 1253 };
        let (p_shrink_rs, _, p_shrink_park, p_shrink_proxy) = tile_stand_in_and_park(
            p_shrink_o, p_shrink_f, p_shrink_f, p_bound, 1.0, true, false, false, false, false, false,
        );
        assert_eq!(p_shrink_rs, Rect { x: -1440, y: 1259, w: 1440, h: 2000 });
        assert!(!p_shrink_park, "portrait shrinking tile at y>0 fits final bound and must not park");
        assert!(p_shrink_proxy);

        let p_grow_o = Rect { x: -1440, y: 1600, w: 1440, h: 600 };
        let p_grow_f = Rect { x: -1440, y: 1600, w: 1440, h: 1253 };
        let (p_grow_rs, _, p_grow_park, _) = tile_stand_in_and_park(
            p_grow_o, p_grow_f, p_grow_f, p_bound, 1.0, true, false, false, false, false, false,
        );
        assert_eq!(p_grow_rs, p_grow_f, "portrait parked tile at t>=1 must draw at final rect");
        assert!(p_grow_park, "portrait growing tile overflowing bottom bound must park");
    }

    #[test]
    fn verify_reject_requires_matching_epoch_responsive_and_stable_size() {
        let origin = Rect { x: 0, y: 0, w: 1000, h: 1000 };
        let want = Rect { x: 0, y: 0, w: 600, h: 1000 };
        let constrained = Rect { x: 0, y: 0, w: 800, h: 1000 };
        let shrinking_prev = Rect { x: 0, y: 0, w: 950, h: 1000 };

        // Epoch mismatch -> never reject.
        assert!(!should_verify_reject(10, 11, want, Some(origin), Some(constrained), constrained, false, (800, 120), false));
        // Hung window -> never reject.
        assert!(!should_verify_reject(10, 10, want, Some(origin), Some(constrained), constrained, true, (800, 120), false));
        // Still shrinking (size changed between pass 1 and pass 2) -> never reject.
        assert!(!should_verify_reject(10, 10, want, Some(origin), Some(shrinking_prev), constrained, false, (800, 120), false));
        // Responsive but slow app with unchanged size equal to origin (no min_track constraint) -> never reject.
        assert!(!should_verify_reject(10, 10, want, Some(origin), Some(origin), origin, false, (200, 120), false));
        // Timed-out handoff/cover without matching min_track -> never reject.
        assert!(!should_verify_reject(10, 10, want, Some(origin), Some(constrained), constrained, false, (200, 120), true));
        // Stable, responsive, matching epoch, AND confirmed by real WM_GETMINMAXINFO minimum -> reject.
        assert!(should_verify_reject(10, 10, want, Some(origin), Some(constrained), constrained, false, (800, 120), false));
        // Even if timed out, a real WM_GETMINMAXINFO minimum matching `got` is rejected.
        assert!(should_verify_reject(10, 10, want, Some(origin), Some(constrained), constrained, false, (800, 120), true));
    }

    #[test]
    fn open_landing_accepts_exact_and_stable_character_grid_sizes() {
        let origin = Rect { x: 100, y: 0, w: 800, h: 1392 };
        let want = Rect { x: 100, y: 0, w: 1277, h: 1392 };

        // Exact arrival succeeds on first poll.
        let mut s = None;
        assert!(open_landing_item_arrived(origin, want, want, &mut s));

        // In-place grow with unchanged old size (`got == origin`): must NOT be accepted early!
        let mut s_inplace = None;
        for _ in 0..5 {
            assert!(
                !open_landing_item_arrived(origin, want, origin, &mut s_inplace),
                "in-place grow still at origin size must not land early"
            );
        }

        // Character-grid terminal resized from origin (800 -> 1269, 8 px narrower than target):
        // arrives after 3 stable polls.
        let grid = Rect { x: 100, y: 0, w: 1269, h: 1384 };
        let mut s_grid = None;
        assert!(!open_landing_item_arrived(origin, want, grid, &mut s_grid));
        assert!(!open_landing_item_arrived(origin, want, grid, &mut s_grid));
        assert!(open_landing_item_arrived(origin, want, grid, &mut s_grid));

        // Still actively resizing -> resets stability counter, does not prematurely arrive.
        let mut s_move = None;
        assert!(!open_landing_item_arrived(origin, want, Rect { w: 1200, ..want }, &mut s_move));
        assert!(!open_landing_item_arrived(origin, want, Rect { w: 1230, ..want }, &mut s_move));
        assert!(!open_landing_item_arrived(origin, want, Rect { w: 1260, ..want }, &mut s_move));
    }

    #[test]
    fn all_targets_already_placed_checks_real_frames_and_force_place() {
        let on_screen = Rect { x: 0, y: 0, w: 960, h: 1080 };
        let parked = Rect { x: 3584, y: 0, w: 960, h: 1080 };
        let placements = vec![(1, on_screen), (2, parked)];

        // Matches real OS frames (including parked window at park_x) -> skips placement.
        assert!(all_targets_already_placed("idle", false, false, &placements, |id| {
            Some(if id == 1 { on_screen } else { parked })
        }));
        // Drifted window -> must place.
        assert!(!all_targets_already_placed("idle", false, false, &placements, |id| {
            Some(if id == 1 { Rect { x: 20, ..on_screen } } else { parked })
        }));
        // `force_place = true` -> must place even if frames match.
        assert!(!all_targets_already_placed("idle", true, false, &placements, |id| {
            Some(if id == 1 { on_screen } else { parked })
        }));
        // Non-idle action -> must place.
        assert!(!all_targets_already_placed("scroll", false, false, &placements, |id| {
            Some(if id == 1 { on_screen } else { parked })
        }));
    }

    #[test]
    fn overlay_update_patches_active_scene_without_altering_motion() {
        let mut scene = Scene {
            epoch: 42,
            input: None,
            input_qpc_us: None,
            diagnostics: false,
            monitors: std::sync::Arc::from([]),
            tiles: vec![Tile {
                pinned: false,
                hidden: false,
                bound: Rect { x: 0, y: 0, w: 1920, h: 1080 },
                fullscreen: false,
                preview: false,
                id: 101,
                rect: Rect { x: 0, y: 0, w: 960, h: 1080 },
                monitor: "MON0".into(),
                color: Some(0x3B82F6),
                cookie: 1,
                cover: None,
            }],
            duration: 180,
            spring: false,
            width: 2.0,
            radius: 8.0,
            holes: std::sync::Arc::from([]),
            zones: vec![],
            action: "open_close",
            live: false,
            force_place: false,
        };
        let new_holes = std::sync::Arc::from([Rect { x: 200, y: 200, w: 400, h: 300 }]);
        apply_overlay_update(&mut scene, std::sync::Arc::clone(&new_holes), &[(101, None)]);
        assert_eq!(scene.epoch, 42);
        assert_eq!(scene.action, "open_close");
        assert_eq!(scene.duration, 180);
        assert_eq!(scene.holes.as_ref(), new_holes.as_ref());
        assert_eq!(scene.tiles[0].color, None);
    }
}
