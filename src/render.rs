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
    /// Monitors (name, bounds) to build overlay hosts for ahead of the first scene.
    Warm(Vec<(String, Rect)>),
    Clear(crossbeam_channel::Sender<()>),
    Stop(crossbeam_channel::Sender<()>),
}
// The overlay is WS_EX_TRANSPARENT: every click falls through to whatever is underneath (the taskbar
// included). WM_NCHITTEST/HTTRANSPARENT cannot do this: it only passes clicks to windows owned by the
// same thread, so a topmost overlay swallowed clicks meant for explorer.exe's taskbar. Peek clicks are
// caught by the low-level mouse hook in win32.rs, which reads the rectangles published in `win32::PEEKS`.
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
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
thread_local! {
    /// One D3D11 device (with its DirectComposition and Direct2D devices) for the render thread's lifetime.
    /// It used to be one per overlay host, created again on every display change; the old devices were
    /// never fully released (240 AMD driver threads, +500 MB RAM, +700 MB VRAM, +10k handles per display
    /// blip in the user's daemon). Hosts now only make their own target, surface and context.
    static SHARED: std::cell::RefCell<Option<(ID3D11Device, IDCompositionDevice, ID2D1Device)>> =
        const { std::cell::RefCell::new(None) };
}
fn shared_devices() -> Result<(ID3D11Device, IDCompositionDevice, ID2D1Device)> {
    SHARED.with(|s| {
        if let Some(d) = s.borrow().as_ref() {
            // A removed device (driver reset, GPU switch) is replaced instead of reused.
            if unsafe { d.0.GetDeviceRemovedReason() }.is_ok() {
                return Ok(d.clone());
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
            (gpu, dev, d2d)
        };
        *s.borrow_mut() = Some(d.clone());
        Ok(d)
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
        self.width_bits = width.to_bits();
        self.radius_bits = radius.to_bits();
        self.dpi_bits = dpi.to_bits();
    }
}
struct DrawFrame<'a> {
    pictures: &'a [(Rect, Rect, ID2D1Bitmap1)],
    fills: &'a [(Rect, u32, f32)],
    cards: &'a [CardDraw],
    lines: &'a [(Rect, u32)],
    width: f32,
    radius: f32,
    dpi: f32,
}
struct Surface {
    gpu: ID3D11Device,
    dev: IDCompositionDevice,
    _target: IDCompositionTarget,
    surface: IDCompositionSurface,
    context: ID2D1DeviceContext,
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
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
            let (gpu, dev, d2d) = shared_devices()?;
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
                dev,
                _target: target,
                gpu,
                surface,
                context,
                brushes: HashMap::new(),
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
                for (dst, src, bitmap) in *pictures {
                    let x = (dst.x + off.x) as f32;
                    let y = (dst.y + off.y) as f32;
                    rt.DrawBitmap(
                        bitmap,
                        Some(&D2D_RECT_F {
                            left: x,
                            top: y,
                            right: x + dst.w as f32,
                            bottom: y + dst.h as f32,
                        }),
                        1.,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&D2D_RECT_F {
                            left: src.x as f32,
                            top: src.y as f32,
                            right: (src.x + src.w) as f32,
                            bottom: (src.y + src.h) as f32,
                        }),
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
struct ThumbState {
    handle: isize,
    tile: Rect,
    clip: Rect,
    source_size: (i32, i32),
    visible: bool,
}
struct Host {
    generation: u64,
    h: HWND,
    b: Rect,
    surface: Option<Surface>,
    thumbs: HashMap<isize, ThumbState>,
    pictures: HashMap<(isize, usize), Source>,
    failed_sources: HashMap<(isize, usize), Instant>,
    pending_sources: HashSet<(isize, usize)>,
    applied_holes: Option<Vec<Rect>>,
    visible: bool,
    last_draw: DrawSig,
    last_draw_valid: bool,
}
impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            for (_, t) in self.thumbs.drain() {
                DwmUnregisterThumbnail(t.handle);
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
                hbrBackground: windows_sys::Win32::Graphics::Gdi::GetStockObject(
                    windows_sys::Win32::Graphics::Gdi::BLACK_BRUSH,
                )
                .cast(),
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
            Ok(Self {
                generation,
                h,
                b,
                surface,
                thumbs: Default::default(),
                pictures: HashMap::new(),
                failed_sources: HashMap::new(),
                pending_sources: HashSet::new(),
                applied_holes: None,
                visible: false,
                last_draw: DrawSig::default(),
                last_draw_valid: false,
            })
        }
    }
    fn thumbnail(&mut self, id: isize, r: Rect, clip: Rect) -> bool {
        unsafe {
            let state = self.thumbs.entry(id).or_insert_with(|| {
                let mut t = 0;
                let hr = DwmRegisterThumbnail(self.h, id as _, &mut t);
                if hr < 0 {
                    eprintln!("DwmRegisterThumbnail: {hr:#x} at {}:{}", file!(), line!());
                }
                ThumbState {
                    handle: t,
                    tile: r,
                    clip,
                    source_size: (0, 0),
                    visible: false,
                }
            });
            let t = state.handle;
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
            let insets = win32::insets(id).unwrap_or(((size.cx - r.w).max(0) / 2, 0));
            let Some((src, dst)) =
                proxy_blit_rects(r, clip, (size.cx, size.cy), insets, (self.b.x, self.b.y))
            else {
                return false;
            };
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_RECTSOURCE
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY,
                rcDestination: RECT {
                    left: dst.x,
                    top: dst.y,
                    right: dst.x + dst.w,
                    bottom: dst.y + dst.h,
                },
                rcSource: RECT {
                    left: src.x,
                    top: src.y,
                    right: src.x + src.w,
                    bottom: src.y + src.h,
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
                return false;
            }
            state.tile = r;
            state.clip = clip;
            state.source_size = (size.cx, size.cy);
            state.visible = true;
            true
        }
    }
    fn hide_thumbnail(&mut self, id: isize) {
        if let Some(state) = self.thumbs.get_mut(&id) {
            state.visible = false;
            let p = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_VISIBLE,
                fVisible: 0,
                ..Default::default()
            };
            unsafe {
                DwmUpdateThumbnailProperties(state.handle, &p);
            }
        }
    }
    fn refresh_resized_thumbnails(&mut self) {
        let host_origin = (self.b.x, self.b.y);
        for (&id, state) in &mut self.thumbs {
            if !state.visible || state.handle == 0 {
                continue;
            }
            let mut size = SIZE::default();
            if unsafe { DwmQueryThumbnailSourceSize(state.handle, &mut size) } < 0 {
                continue;
            }
            let new_size = (size.cx, size.cy);
            let insets = win32::insets(id).unwrap_or(((size.cx - state.tile.w).max(0) / 2, 0));
            if let Some((src, dst)) = refresh_thumb_props_if_resized(
                state.tile,
                state.clip,
                state.source_size,
                new_size,
                insets,
                host_origin,
            ) {
                let props = DWM_THUMBNAIL_PROPERTIES {
                    dwFlags: DWM_TNP_RECTDESTINATION
                        | DWM_TNP_RECTSOURCE
                        | DWM_TNP_VISIBLE
                        | DWM_TNP_OPACITY,
                    rcDestination: RECT {
                        left: dst.x,
                        top: dst.y,
                        right: dst.x + dst.w,
                        bottom: dst.y + dst.h,
                    },
                    rcSource: RECT {
                        left: src.x,
                        top: src.y,
                        right: src.x + src.w,
                        bottom: src.y + src.h,
                    },
                    opacity: 255,
                    fVisible: 1,
                    fSourceClientAreaOnly: 0,
                };
                if unsafe { DwmUpdateThumbnailProperties(state.handle, &props) } >= 0 {
                    state.source_size = new_size;
                }
            }
        }
    }
}
/// Compute strictly 1:1 (unscaled) `(source_rect, dest_rect)` for blitting a proxy window
/// (either a DWM thumbnail or a WGC backing bitmap) clipped to `clip` in monitor-local coords.
/// Never stretches a stale/smaller capture across a widened tile.
pub(crate) fn proxy_blit_rects(
    tile: Rect,
    clip: Rect,
    source_size: (i32, i32),
    insets: (i32, i32),
    host_origin: (i32, i32),
) -> Option<(Rect, Rect)> {
    let (src_w, src_h) = source_size;
    let (ox, oy) = insets;
    let sx = ox + clip.x - tile.x;
    let sy = oy + clip.y - tile.y;
    if sx >= src_w || sy >= src_h {
        return None;
    }
    let (sx, dx_pad) = if sx < 0 { (0, -sx) } else { (sx, 0) };
    let (sy, dy_pad) = if sy < 0 { (0, -sy) } else { (sy, 0) };
    let sw = (clip.w - dx_pad).min(src_w - sx).max(0);
    let sh = (clip.h - dy_pad).min(src_h - sy).max(0);
    if sw == 0 || sh == 0 {
        return None;
    }
    Some((
        Rect { x: sx, y: sy, w: sw, h: sh },
        Rect {
            x: clip.x + dx_pad - host_origin.0,
            y: clip.y + dy_pad - host_origin.1,
            w: sw,
            h: sh,
        },
    ))
}

/// Only paint a WGC backing bitmap on top of the host when actively gliding/scrolling/handing off,
/// or as a fallback if the live DWM thumbnail is unavailable. Settled grouped/side peeks show the
/// live 1:1 DWM thumbnail instead of a frozen pre-resize WGC frame.
pub(crate) fn should_draw_backing(
    moving: bool,
    live_proxy: bool,
    open_handoff: bool,
    thumb_ok: bool,
) -> bool {
    moving || live_proxy || open_handoff || !thumb_ok
}

/// WGC window captures normally start at the visible frame (`(0, 0)`), whereas DWM thumbnails
/// capture the full `GetWindowRect` (requiring `win32::insets`). Only apply `dwm_insets` to a
/// WGC capture when the capture actually matches the larger `window_rect` rather than `frame_rect`.
pub(crate) fn wgc_capture_insets(
    capture_size: (i32, i32),
    frame_rect: Rect,
    window_rect: Option<Rect>,
    dwm_insets: (i32, i32),
) -> (i32, i32) {
    if let Some(wr) = window_rect
        && (wr.w > frame_rect.w + 2 || wr.h > frame_rect.h + 2)
        && (capture_size.0 - wr.w).abs() <= 2
        && (capture_size.1 - wr.h).abs() <= 2
    {
        dwm_insets
    } else {
        (0, 0)
    }
}

/// Decide whether to draw a WGC backing bitmap for a proxy tile and compute its 1:1 `(src, dst)`.
/// When `thumb_ok` is true, WGC backing is only used if it covers the entire `clip` rectangle;
/// partial coverage keeps the live DWM thumbnail visible instead of leaving blank regions.
/// When `thumb_ok` is false, partial backing is allowed as a fallback.
#[allow(clippy::too_many_arguments)]
pub(crate) fn choose_proxy_backing(
    tile: Rect,
    clip: Rect,
    source_size: (i32, i32),
    insets: (i32, i32),
    host_origin: (i32, i32),
    moving: bool,
    live_proxy: bool,
    open_handoff: bool,
    thumb_ok: bool,
) -> Option<(Rect, Rect)> {
    if !should_draw_backing(moving, live_proxy, open_handoff, thumb_ok) {
        return None;
    }
    let (src_r, dst_r) = proxy_blit_rects(tile, clip, source_size, insets, host_origin)?;
    let covers_clip = dst_r.w == clip.w && dst_r.h == clip.h;
    if thumb_ok && !covers_clip {
        return None;
    }
    Some((src_r, dst_r))
}

/// True when a cached WGC capture bitmap should be dropped as stale.
/// While `in_motion` (`moving || open_handoff`), the stand-in `rs` keeps the window's original
/// size while the real OS window may already have been resized at `park`: judge the capture
/// against `drawn_size` (`rs`) so a shrinking window keeps its full-coverage origin-size bitmap
/// during the animation instead of dropping it and leaving part of the tile blank.
/// When settled (`!in_motion`), judge against the OS frame (`os_frame`) so resized windows refresh.
pub(crate) fn is_capture_stale(
    capture_size: (i32, i32),
    drawn_size: (i32, i32),
    os_frame: Option<Rect>,
    in_motion: bool,
) -> bool {
    let target = if in_motion {
        drawn_size
    } else if let Some(fr) = os_frame {
        (fr.w, fr.h)
    } else {
        drawn_size
    };
    (capture_size.0 - target.0).abs() > 24 || (capture_size.1 - target.1).abs() > 24
}

/// Compute the off-screen parked placement rectangle for a proxy window.
/// While `in_live_motion` (`s.live || moving`), anchor `y` to `bound.y` so rolling a group
/// Anchor parked tiles at `(park, bound.y)` during both live motion and settle so rolling a group
/// vertically never triggers per-frame `DeferWindowPos` / `EVENT_OBJECT_LOCATIONCHANGE` storms
/// (#46) and never emits an extra settle-time move that could drift `y` into another monitor's DPI
/// band in mixed-DPI layouts.
pub(crate) fn parked_target_rect(
    park: i32,
    tile_rect: Rect,
    bound: Rect,
    _in_live_motion: bool,
) -> Rect {
    Rect { x: park, y: bound.y, ..tile_rect }
}

/// Recompute 1:1 thumbnail `(src, dst)` when a parked window's DWM thumbnail source size changes
/// asynchronously after settling. Returns `None` when unchanged.
pub(crate) fn refresh_thumb_props_if_resized(
    tile: Rect,
    clip: Rect,
    prev_source_size: (i32, i32),
    new_source_size: (i32, i32),
    insets: (i32, i32),
    host_origin: (i32, i32),
) -> Option<(Rect, Rect)> {
    if new_source_size == prev_source_size {
        return None;
    }
    proxy_blit_rects(tile, clip, new_source_size, insets, host_origin)
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let w = (a.x + a.w).min(b.x + b.w) - x;
    let h = (a.y + a.h).min(b.y + b.h) - y;
    (w > 0 && h > 0).then_some(Rect { x, y, w, h })
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
type OpenLanding = (u64, Instant, Vec<Placement>);
type VerifyState = (Instant, Vec<Placement>, u8);
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
    backing: Vec<(Rect, Rect, ID2D1Bitmap1)>,
    cards: Vec<CardDraw>,
    keep: HashSet<isize>,
    host_hits: Vec<(Rect, isize)>,
    fills: Vec<(Rect, u32, f32)>,
    mine: Vec<Rect>,
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
    last_thumb_refresh: Instant,
    source_tx: crossbeam_channel::Sender<SourceJob>,
    source_done: crossbeam_channel::Receiver<SourceDone>,
    scene: Option<Scene>,
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
            last_thumb_refresh: begin,
            source_tx,
            source_done,
            scene: None,
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
            .any(|h| !h.pictures.is_empty() || !h.pending_sources.is_empty() || !h.thumbs.is_empty())
    }

    fn clear_state(&mut self) {
        self.verify = None;
        self.hosts.clear();
        self.scene = None;
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
            if let Some((at, items, left)) = self.verify.take() {
                if Instant::now() < at {
                    self.verify = Some((at, items, left))
                } else if left > 0 {
                    let _vspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderVerify);
                    if items
                        .iter()
                        .any(|&(id, want)| win32::frame(id).is_some_and(|got| got != want))
                    {
                        win32::place_async(&items);
                        self.verify = Some((Instant::now() + Duration::from_millis(150), items, left - 1))
                    }
                }
            }
            let async_landed = self.open_landing.as_ref().and_then(|(epoch, started, items)| {
                let current = *epoch == EPOCH.load(std::sync::atomic::Ordering::Acquire);
                // Position is the handoff signal. A minimum-size application may legitimately be wider or
                // taller than requested; the normal rejection path handles that after the handoff.
                let arrived = items.iter().all(|(id, want)| {
                    win32::frame(*id).is_none_or(|got| {
                        (got.x - want.x).abs() <= 2 && (got.y - want.y).abs() <= 2
                    })
                });
                let timed_out = started.elapsed() >= Duration::from_millis(350);
                (current && (arrived || timed_out)).then_some((*epoch, !arrived))
            });
            if let Some((epoch, timed_out)) = async_landed {
                self.open_landed = Some(epoch);
                self.open_timed_out = timed_out;
                self.open_landing = None;
            }
            while let Ok((mon, generation, key, made, ms)) = self.source_done.try_recv() {
                let Some(host) = self.hosts.get_mut(&mon).filter(|h| h.generation == generation) else {
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
                        let _ = source.update(surface, 0);
                    }
                }
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
        let refresh_thumbs = self.last_thumb_refresh.elapsed() >= Duration::from_millis(100);
        if refresh_thumbs {
            self.last_thumb_refresh = Instant::now();
        }
        for host in self.hosts.values_mut() {
            if refresh_thumbs {
                host.refresh_resized_thumbnails();
            }
            let Some(surface) = &host.surface else {
                continue;
            };
            for (&(id, _), source) in &mut host.pictures {
                if let Some(r) = win32::frame(id) {
                    let stale = source.bitmap.as_ref().is_some_and(|bm| {
                        let px = unsafe { bm.GetPixelSize() };
                        is_capture_stale((px.width as i32, px.height as i32), (r.w, r.h), Some(r), false)
                    });
                    if stale {
                        source.bitmap = None;
                    }
                    if intersect(r, host.b) == Some(r) {
                        let _ = source.update(surface, if stale { 0 } else { self.settled });
                    }
                }
            }
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
            for (name, host) in &mut self.hosts {
                scratch.lines.clear();
                scratch.backing.clear();
                scratch.cards.clear();
                scratch.keep.clear();
                scratch.host_hits.clear();
                let mut full = false;
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
                    // While moving, the stand-in keeps the window's real size and only its position
                    // glides; scaling it to the in-between rectangle stretched the contents.
                    let rs = if t < 1. { Rect { w: o.w, h: o.h, ..r } } else { r };
                    let clip = if tile.hidden { None } else { intersect(rs, bound) };
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
                    let park_window = !tile.pinned
                        && (moving || live_proxy || clip != Some(rs) || tile.preview || tile.hidden);
                    // Once an open/close animation reaches its last frame, hold the stand-in on screen
                    // while the real window's final asynchronous move catches up. The OS target below is
                    // still the visible tile rectangle (not the park column).
                    let proxy = park_window || (!tile.pinned && open_handoff);
                    if let Some(c) = clip {
                        let thumb_ok = proxy && host.thumbnail(tile.id, rs, c);
                        let mut drew_backing = false;
                        if proxy
                            && should_draw_backing(moving, live_proxy, open_handoff, thumb_ok)
                            && let (Some(source), Some(surface)) = (
                                host.pictures.get_mut(&(tile.id, tile.cookie)),
                                host.surface.as_ref(),
                            )
                        {
                            let cur_frame = win32::frame(tile.id);
                            let in_motion = moving || open_handoff;
                            if let Some(bm) = &source.bitmap {
                                let px = bm.GetPixelSize();
                                if is_capture_stale(
                                    (px.width as i32, px.height as i32),
                                    (rs.w, rs.h),
                                    cur_frame,
                                    in_motion,
                                ) {
                                    source.bitmap = None;
                                }
                            }
                            if source.bitmap.is_none()
                                && let Err(e) = source.update(surface, 0)
                            {
                                eprintln!("source frame: {e}")
                            }
                            if let Some(bitmap) = &source.bitmap {
                                let px = bitmap.GetPixelSize();
                                let src_size = (px.width as i32, px.height as i32);
                                let frame_rect = cur_frame.unwrap_or(rs);
                                let mut wr = RECT::default();
                                let window_rect = (GetWindowRect(tile.id as _, &mut wr) != 0)
                                    .then(|| win32::rect(wr));
                                let dwm_insets = win32::insets(tile.id).unwrap_or((0, 0));
                                let wgc_insets =
                                    wgc_capture_insets(src_size, frame_rect, window_rect, dwm_insets);
                                if let Some((src_r, dst_r)) = choose_proxy_backing(
                                    rs,
                                    c,
                                    src_size,
                                    wgc_insets,
                                    (host.b.x, host.b.y),
                                    moving,
                                    live_proxy,
                                    open_handoff,
                                    thumb_ok,
                                ) {
                                    scratch.backing.push((dst_r, src_r, bitmap.clone()));
                                    drew_backing = true;
                                }
                            }
                        }
                        if thumb_ok {
                            if drew_backing {
                                host.hide_thumbnail(tile.id);
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
                        parked_target_rect(park, tile.rect, bound, s.live || moving)
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
                host.thumbs.retain(|id, t| {
                    if scratch.keep.contains(id) {
                        true
                    } else {
                        DwmUnregisterThumbnail(t.handle);
                        false
                    }
                });
                if !full {
                    scratch.hits.append(&mut scratch.host_hits);
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
                if full
                    || (scratch.keep.is_empty()
                        && scratch.lines.is_empty()
                        && scratch.fills.is_empty()
                        && scratch.cards.is_empty())
                {
                    if host.visible {
                        ShowWindow(host.h, SW_HIDE);
                        host.visible = false;
                        host.last_draw_valid = false;
                    }
                } else {
                    if !host.visible {
                        ShowWindow(host.h, SW_SHOWNOACTIVATE);
                        host.visible = true;
                        host.last_draw_valid = false;
                    }
                    // Cut unmanaged windows out of the overlay (SetWindowRgn); only when they changed.
                    scratch.mine.clear();
                    for h in s.holes.iter() {
                        if let Some(r) = intersect(*h, host.b) {
                            scratch.mine.push(r);
                        }
                    }
                    if host.applied_holes.as_ref() != Some(&scratch.mine) {
                        let _rgn_span = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderRgnHoles);
                        if scratch.mine.is_empty() {
                            windows_sys::Win32::Graphics::Gdi::SetWindowRgn(host.h, null_mut(), 1);
                        } else {
                            let whole = windows_sys::Win32::Graphics::Gdi::CreateRectRgn(0, 0, host.b.w, host.b.h);
                            for h in &scratch.mine {
                                let cut = windows_sys::Win32::Graphics::Gdi::CreateRectRgn(
                                    h.x - host.b.x,
                                    h.y - host.b.y,
                                    h.x - host.b.x + h.w,
                                    h.y - host.b.y + h.h,
                                );
                                windows_sys::Win32::Graphics::Gdi::CombineRgn(
                                    whole,
                                    whole,
                                    cut,
                                    windows_sys::Win32::Graphics::Gdi::RGN_DIFF,
                                );
                                windows_sys::Win32::Graphics::Gdi::DeleteObject(cut as _);
                            }
                            windows_sys::Win32::Graphics::Gdi::SetWindowRgn(host.h, whole, 1);
                        }
                        let slot = host.applied_holes.get_or_insert_with(Vec::new);
                        slot.clear();
                        slot.extend_from_slice(&scratch.mine);
                    }
                    if let Some(surface) = host.surface.as_mut() {
                        let dpi = GetDpiForWindow(host.h) as f32 / 96.;
                        let width = s.width * dpi;
                        let radius = s.radius * dpi;
                        if !scratch.backing.is_empty()
                            || !host.last_draw_valid
                            || !host.last_draw.matches(
                                &scratch.fills,
                                &scratch.cards,
                                &scratch.lines,
                                width,
                                radius,
                                dpi,
                            )
                        {
                            let frame = DrawFrame {
                                pictures: &scratch.backing,
                                fills: &scratch.fills,
                                cards: &scratch.cards,
                                lines: &scratch.lines,
                                width,
                                radius,
                                dpi,
                            };
                            let draw_start = Instant::now();
                            let drawn = {
                                let _dspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderDraw);
                                surface.draw(&frame)
                            };
                            if draw_start.elapsed().as_millis() >= 100 {
                                diag::trace(format!("SLOW render draw: {} ms", draw_start.elapsed().as_millis()))
                            }
                            if let Err(e) = drawn {
                                eprintln!("border draw: {e}");
                                host.surface = None;
                                host.last_draw_valid = false;
                            } else if scratch.backing.is_empty() {
                                host.last_draw.set_from(
                                    &scratch.fills,
                                    &scratch.cards,
                                    &scratch.lines,
                                    width,
                                    radius,
                                    dpi,
                                );
                                host.last_draw_valid = true;
                            } else {
                                host.last_draw_valid = false;
                            }
                        }
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
                let animated_open_landing = s.action == "open_close"
                    && t >= 1.
                    && !scratch.landing.is_empty();
                if animated_open_landing
                    && self.open_landed != Some(s.epoch)
                    && self.open_landing.as_ref().is_none_or(|(epoch, _, _)| *epoch != s.epoch)
                {
                    win32::place_async(&scratch.landing);
                    self.open_landing = Some((s.epoch, Instant::now(), scratch.landing.clone()));
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
                .is_some_and(|(epoch, _, _)| *epoch == s.epoch);
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
                .is_some_and(|(epoch, _, _)| *epoch == s.epoch);
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
                    self.verify = Some((
                        Instant::now() + Duration::from_millis(250),
                        std::mem::take(&mut self.scratch.verify_items),
                        2,
                    ));
                }
                // `self.scene` stays cleared after the final settled frame.
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
        loop {
            crate::perf::heartbeat_render();
            state.poll_background_work();
            let timeout = if state.scene.is_some() {
                Duration::ZERO
            } else if state.has_active_sources() || state.verify.is_some() {
                Duration::from_millis(33)
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
                Ok(Message::Scene(mut s)) => {
                    // Drain any newer queued scenes so a burst of input/events skips stale frames
                    // instead of paying setup work for a scene that has already been superseded.
                    let mut stop = false;
                    let mut cleared = false;
                    while let Ok(next_msg) = rx.try_recv() {
                        match next_msg {
                            Message::Scene(newer) => s = newer,
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
    use super::{
        choose_proxy_backing, is_capture_stale, parked_target_rect, proxy_blit_rects,
        refresh_thumb_props_if_resized, should_draw_backing, wgc_capture_insets,
    };
    use planor::Rect;

    #[test]
    fn wgc_vs_dwm_origins_are_distinguished() {
        let frame = Rect { x: 0, y: 0, w: 1280, h: 1364 };
        let window = Rect { x: -8, y: 0, w: 1296, h: 1372 };
        let dwm_insets = (8, 0);

        // Normal WGC capture matches the visible frame -> origin is (0, 0), NOT (8, 0).
        assert_eq!(wgc_capture_insets((1280, 1364), frame, Some(window), dwm_insets), (0, 0));
        // If WGC actually captured the full GetWindowRect with invisible borders -> use dwm_insets.
        assert_eq!(wgc_capture_insets((1296, 1372), frame, Some(window), dwm_insets), (8, 0));
        // Without a window rect -> default to (0, 0).
        assert_eq!(wgc_capture_insets((1280, 1364), frame, None, dwm_insets), (0, 0));

        // Verify blitting with WGC (0, 0) vs DWM (8, 0) produces exact unshifted rects.
        let clip = Rect { x: 0, y: 0, w: 1280, h: 1364 };
        let (wgc_src, wgc_dst) = proxy_blit_rects(frame, clip, (1280, 1364), (0, 0), (0, 0)).unwrap();
        let (dwm_src, dwm_dst) = proxy_blit_rects(frame, clip, (1296, 1372), (8, 0), (0, 0)).unwrap();
        assert_eq!(wgc_src, Rect { x: 0, y: 0, w: 1280, h: 1364 });
        assert_eq!(dwm_src, Rect { x: 8, y: 0, w: 1280, h: 1364 });
        assert_eq!(wgc_dst, dwm_dst);
    }

    #[test]
    fn choose_proxy_backing_requires_full_coverage_when_thumb_is_live() {
        // Repro of #38 (bundle 000812248): group widened from 1157 -> 2560; bottom peek is 28px tall.
        let tile = Rect { x: 0, y: 1364, w: 2560, h: 1364 };
        let clip = Rect { x: 0, y: 1364, w: 2560, h: 28 };

        // Partial capture (1157 < 2560) with live DWM thumbnail: must NOT draw backing or hide thumbnail.
        assert_eq!(
            choose_proxy_backing(tile, clip, (1157, 1364), (0, 0), (0, 0), true, false, false, true),
            None,
            "partial WGC capture must keep live DWM thumbnail visible during motion"
        );
        // Same partial capture when DWM thumbnail failed (thumb_ok == false): allow partial fallback, 1:1 unscaled.
        let (src, dst) =
            choose_proxy_backing(tile, clip, (1157, 1364), (0, 0), (0, 0), false, false, false, false)
                .expect("partial fallback allowed when thumb_ok == false");
        assert_eq!(src, Rect { x: 0, y: 0, w: 1157, h: 28 });
        assert_eq!(dst, Rect { x: 0, y: 1364, w: 1157, h: 28 });

        // Full-coverage capture (2560 >= 2560) during motion: allowed and 1:1.
        let (full_src, full_dst) =
            choose_proxy_backing(tile, clip, (2560, 1364), (0, 0), (0, 0), true, false, false, true)
                .expect("full coverage during motion uses WGC backing");
        assert_eq!(full_src, Rect { x: 0, y: 0, w: 2560, h: 28 });
        assert_eq!(full_dst, Rect { x: 0, y: 1364, w: 2560, h: 28 });

        // Settled with live DWM thumbnail: never draw WGC backing over it.
        assert_eq!(
            choose_proxy_backing(tile, clip, (2560, 1364), (0, 0), (0, 0), false, false, false, true),
            None
        );
    }

    #[test]
    fn proxy_blit_rects_keeps_top_and_bottom_peeks_unscaled_landscape_and_portrait() {
        // Repro of #18 (bundle 144350042): 3-window group widened 1277 -> 2560 on monitor at y=-1440.
        let host = (0, -1440);
        let top_tile = Rect { x: 0, y: -2748, w: 2560, h: 1336 };
        let top_clip = Rect { x: 0, y: -1440, w: 2560, h: 28 };
        let (top_src, top_dst) =
            proxy_blit_rects(top_tile, top_clip, (1293, 1344), (8, 0), host).unwrap();
        assert_eq!(top_src.w, top_dst.w);
        assert_eq!(top_src.h, top_dst.h);
        assert_eq!(top_dst.y, 0);
        assert_eq!(top_dst.h, 28);

        let bot_tile = Rect { x: 0, y: -76, w: 2560, h: 1336 };
        let bot_clip = Rect { x: 0, y: -76, w: 2560, h: 28 };
        let (bot_src, bot_dst) =
            proxy_blit_rects(bot_tile, bot_clip, (2576, 1344), (8, 0), host).unwrap();
        assert_eq!(bot_src, Rect { x: 8, y: 0, w: 2560, h: 28 });
        assert_eq!(bot_dst, Rect { x: 0, y: 1364, w: 2560, h: 28 });

        // Portrait monitor (-1440, 0, 1440, 2512): grouped peek on right edge (28px wide), negative host origin.
        let p_host = (-1440, 0);
        let p_tile = Rect { x: -28, y: 0, w: 1412, h: 1200 };
        let p_clip = Rect { x: -28, y: 0, w: 28, h: 1200 };
        let (p_src, p_dst) =
            proxy_blit_rects(p_tile, p_clip, (1428, 1208), (8, 0), p_host).unwrap();
        assert_eq!(p_src.w, p_dst.w);
        assert_eq!(p_src.h, p_dst.h);
        assert_eq!(p_dst, Rect { x: 1412, y: 0, w: 28, h: 1200 });
    }

    #[test]
    fn proxy_blit_rects_handles_negative_offsets_and_left_scrolled_windows() {
        // Repro of #55 (bundle 225940320): left window at x=-1455, w=1920, visible 0..465.
        let tile = Rect { x: -1455, y: 0, w: 1920, h: 1140 };
        let clip = Rect { x: 0, y: 0, w: 465, h: 1140 };
        let (src, dst) = proxy_blit_rects(tile, clip, (1936, 1148), (8, 0), (0, 0)).unwrap();
        assert_eq!(src, Rect { x: 1463, y: 0, w: 465, h: 1140 });
        assert_eq!(dst, Rect { x: 0, y: 0, w: 465, h: 1140 });

        // Negative sx (clip.x < tile.x) pads dst.x and clamps src.x to 0.
        let (neg_src, neg_dst) = proxy_blit_rects(
            Rect { x: 10, y: 0, w: 100, h: 100 },
            Rect { x: 0, y: 0, w: 50, h: 100 },
            (100, 100),
            (0, 0),
            (0, 0),
        )
        .unwrap();
        assert_eq!(neg_src, Rect { x: 0, y: 0, w: 40, h: 100 });
        assert_eq!(neg_dst, Rect { x: 10, y: 0, w: 40, h: 100 });

        // Stale narrower capture returns None so caller keeps live DWM thumbnail.
        assert_eq!(proxy_blit_rects(tile, clip, (957, 1140), (0, 0), (0, 0)), None);
    }

    #[test]
    fn idle_thumbnail_refresh_updates_props_when_source_grows_after_settling() {
        // Repro of #18 (144350042): parked peek 3016780 resized asynchronously 1277 -> 2560 after settle.
        let tile = Rect { x: 0, y: -76, w: 2560, h: 1336 };
        let clip = Rect { x: 0, y: -76, w: 2560, h: 28 };
        let host = (0, -1440);

        // Unchanged size -> no update needed.
        assert_eq!(
            refresh_thumb_props_if_resized(tile, clip, (1293, 1344), (1293, 1344), (8, 0), host),
            None
        );
        // Once the app finishes resizing to 2576x1344 -> thumbnail props expand to full 2560x28.
        let (src, dst) =
            refresh_thumb_props_if_resized(tile, clip, (1293, 1344), (2576, 1344), (8, 0), host)
                .expect("grown source must refresh thumbnail properties");
        assert_eq!(src, Rect { x: 8, y: 0, w: 2560, h: 28 });
        assert_eq!(dst, Rect { x: 0, y: 1364, w: 2560, h: 28 });
    }

    #[test]
    fn should_draw_backing_only_during_motion_or_thumb_failure() {
        assert!(!should_draw_backing(false, false, false, true));
        assert!(should_draw_backing(true, false, false, true));
        assert!(should_draw_backing(false, true, false, true));
        assert!(should_draw_backing(false, false, true, true));
        assert!(should_draw_backing(false, false, false, false));
    }

    #[test]
    fn stale_capture_decision_keeps_origin_bitmap_in_motion_and_drops_settled_mismatch() {
        // (a) Shrinking window in motion: stand-in `rs` keeps original 2560x1364 while OS window
        //     at `park` already shrunk to 1157x1364. Must KEEP the origin-size capture so the
        //     stand-in covers the whole clip without blanking out during the 180 ms animation.
        let shrunk_os = Some(Rect { x: 3584, y: 0, w: 1157, h: 1364 });
        assert!(
            !is_capture_stale((2560, 1364), (2560, 1364), shrunk_os, true),
            "origin-size bitmap must be kept during motion even after OS window shrunk"
        );
        let tile = Rect { x: 0, y: 0, w: 2560, h: 1364 };
        let clip = Rect { x: 0, y: 0, w: 2560, h: 1364 };
        assert_eq!(
            choose_proxy_backing(tile, clip, (2560, 1364), (0, 0), (0, 0), true, false, false, true),
            Some((clip, clip)),
            "kept origin-size bitmap covers full clip during shrinking animation"
        );

        // (b) Growing tile: during motion `rs` is still 1157x1364 so 1157 bitmap matches `rs`;
        //     once settled (`in_motion == false`), 1157 bitmap vs 2560 OS frame is stale.
        let grown_os = Some(Rect { x: 3584, y: 0, w: 2560, h: 1364 });
        assert!(!is_capture_stale((1157, 1364), (1157, 1364), grown_os, true));
        assert!(is_capture_stale((1157, 1364), (2560, 1364), grown_os, false));

        // (c) Settled proxy / idle refresh with mismatched OS frame is stale and dropped.
        assert!(is_capture_stale((1277, 1364), (1277, 1364), grown_os, false));
        assert!(!is_capture_stale((2560, 1364), (2560, 1364), grown_os, false));
    }

    #[test]
    fn parked_target_rect_holds_constant_y_during_live_group_roll_and_restores_on_settle() {
        // Repro of #46 (bundle 001446456): rolling a 3-window group changed tile.rect.y every frame.
        let bound = Rect { x: 0, y: -1440, w: 2560, h: 1392 };
        let frame1 = Rect { x: 0, y: -301, w: 1277, h: 1336 };
        let frame2 = Rect { x: 0, y: -291, w: 1277, h: 1336 };
        let p1 = parked_target_rect(3584, frame1, bound, true);
        let p2 = parked_target_rect(3584, frame2, bound, true);
        assert_eq!(p1, p2, "parked target stays constant during live roll (no per-frame DeferWindowPos storm)");
        assert_eq!(p1, Rect { x: 3584, y: -1440, w: 1277, h: 1336 });
        // At settle, parked target keeps bound.y as well (zero extra settle move, no mixed-DPI y drift).
        let settled = parked_target_rect(3584, frame2, bound, false);
        assert_eq!(settled, Rect { x: 3584, y: -1440, w: 1277, h: 1336 });
    }
}
