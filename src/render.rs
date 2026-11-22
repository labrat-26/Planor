use super::*;
use planor::Rect;
use std::{
    collections::{BTreeMap, HashMap},
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
}
pub struct Scene {
    pub epoch: u64,
    pub input: Option<Instant>,
    pub input_qpc_us: Option<i64>,
    pub diagnostics: bool,
    pub monitors: Vec<(String, Rect, Rect)>,
    pub tiles: Vec<Tile>,
    pub duration: u64,
    pub spring: bool,
    pub width: f32,
    pub radius: f32,
    /// Rectangles of unmanaged app windows (desktop coordinates): cut out of the overlay.
    pub holes: Vec<Rect>,
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
struct Surface {
    gpu: ID3D11Device,
    dev: IDCompositionDevice,
    _target: IDCompositionTarget,
    surface: IDCompositionSurface,
    context: ID2D1DeviceContext,
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
}
impl Surface {
    fn new(h: HWND, b: Rect) -> Result<Self> {
        unsafe {
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
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let context = factory
                .CreateDevice(&dx)?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            Ok(Self {
                dev,
                _target: target,
                gpu,
                surface,
                context,
                brushes: HashMap::new(),
            })
        }
    }
    fn draw(
        &mut self,
        pictures: &[(Rect, ID2D1Bitmap1)],
        lines: &[(Rect, u32)],
        width: f32,
        radius: f32,
    ) -> Result<()> {
        unsafe {
            let mut off = F::POINT::default();
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
                for (r, bitmap) in pictures {
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
                for &(r, c) in lines {
                    let brush = match self.brushes.entry(c) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            let created = rt.CreateSolidColorBrush(
                                &D2D1_COLOR_F {
                                    r: ((c >> 16) & 255) as f32 / 255.,
                                    g: ((c >> 8) & 255) as f32 / 255.,
                                    b: (c & 255) as f32 / 255.,
                                    a: 1.,
                                },
                                None,
                            )?;
                            e.insert(created)
                        }
                    };
                    let x = (r.x + off.x) as f32;
                    let y = (r.y + off.y) as f32;
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
                        &*brush,
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
            result?;
            self.dev.Commit()?;
            Ok(())
        }
    }
}
struct Host {
    h: HWND,
    b: Rect,
    surface: Option<Surface>,
    thumbs: HashMap<isize, isize>,
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
    fn new(b: Rect) -> Result<Self> {
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
                h,
                b,
                surface,
                thumbs: Default::default(),
            })
        }
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
            let x = clip.x - r.x;
            let y = clip.y - r.y;
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_RECTSOURCE
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY,
                rcDestination: RECT {
                    left: clip.x - self.b.x,
                    top: clip.y - self.b.y,
                    right: clip.x + clip.w - self.b.x,
                    bottom: clip.y + clip.h - self.b.y,
                },
                rcSource: RECT {
                    left: x * size.cx / r.w,
                    top: y * size.cy / r.h,
                    right: (x + clip.w) * size.cx / r.w,
                    bottom: (y + clip.h) * size.cy / r.h,
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
fn interpolate(a: Rect, b: Rect, t: f64) -> Rect {
    let f = |a, b| (a as f64 + (b as f64 - a as f64) * t).round() as i32;
    Rect {
        x: f(a.x, b.x),
        y: f(a.y, b.y),
        w: f(a.w, b.w),
        h: f(a.h, b.h),
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
        let mut hosts = BTreeMap::<String, Host>::new();
        let mut applied = HashMap::<String, Vec<Rect>>::new();
        let mut current = HashMap::new();
        let mut origins = HashMap::new();
        let mut pictures = HashMap::<(isize, usize, String), Source>::new();
        // Cooldown cache so WGC Source::new failures (e.g. elevated/protected windows or
        // non-input desktops) do not block every subsequent warm scene for ~45 ms/window.
        let mut failed_sources = HashMap::<(isize, usize, String), Instant>::new();
        let mut scene: Option<Scene> = None;
        let mut begin = Instant::now();
        let mut last = begin;
        let mut settled = 0;
        let mut gesture_recorded = false;
        let mut parked_checked_epoch = 0u64;
        // Windows that were just uncloaked (desktop switch) can report a transiently inset frame,
        // which skews the border delta used for placement. After settling, re-check the frames
        // and re-place anything that is not exactly on target.
        let mut verify: Option<(Instant, Vec<(isize, Rect)>, u8)> = None;
        let mut hz = 0;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceFrequency(value: *mut i64) -> i32;
            fn QueryPerformanceCounter(value: *mut i64) -> i32;
        }
        QueryPerformanceFrequency(&mut hz);
        let timestamp = || {
            let mut now = 0;
            QueryPerformanceCounter(&mut now);
            ((now as i128 * 10_000_000) / hz as i128) as i64
        };
        let tag = wide("Planor.Recovery.Identity");
        loop {
            crate::perf::heartbeat_render();
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if let Some((at, items, left)) = verify.take() {
                if Instant::now() < at {
                    verify = Some((at, items, left))
                } else if left > 0 {
                    let _vspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderVerify);
                    if items
                        .iter()
                        .any(|&(id, want)| win32::frame(id).is_some_and(|got| got != want))
                    {
                        let _ = win32::place(&items);
                        verify = Some((Instant::now() + Duration::from_millis(150), items, left - 1))
                    }
                }
            }
            match rx.recv_timeout(if scene.is_some() {
                Duration::ZERO
            } else if !pictures.is_empty() || verify.is_some() {
                Duration::from_millis(33)
            } else {
                Duration::from_millis(50)
            }) {
                Ok(Message::Stop(ack)) => {
                    win32::set_peeks(Vec::new());
                    hosts.clear();
                    let _ = ack.send(());
                    break;
                }
                Ok(Message::Warm(monitors)) => {
                    for (n, b) in monitors {
                        if hosts.get(&n).is_some_and(|h| h.b != b) {
                            hosts.remove(&n);
                        }
                        if !hosts.contains_key(&n) {
                            if let Ok(h) = Host::new(b) {
                                hosts.insert(n, h);
                            }
                        }
                    }
                    continue;
                }
                Ok(Message::Clear(ack)) => {
                    verify = None;
                    hosts.clear();
                    scene = None;
                    current.clear();
                    pictures.clear();
                    failed_sources.clear();
                    win32::set_peeks(Vec::new());
                    let _ = ack.send(());
                    continue;
                }
                Ok(Message::Scene(s)) => {
                    let _scene_span = crate::perf::SpanGuard::with_arg(
                        crate::perf::SpanKind::RenderScene,
                        s.epoch,
                    );
                    gesture_recorded = false;
                    crate::perf::record_instant(
                        "SCENE",
                        "render",
                        format!("seq={} tiles={}", s.epoch, s.tiles.len()),
                    );
                    let handle_start = Instant::now();
                    verify = None;
                    origins = current.clone();
                    for t in &s.tiles {
                        origins
                            .entry(t.id)
                            .or_insert_with(|| win32::frame(t.id).unwrap_or(t.rect));
                        // A window already within 2px of its target is settled; animating
                        // (and parking) it for a rounding discrepancy is pure noise.
                        let o = origins[&t.id];
                        if (o.x - t.rect.x).abs() <= 2
                            && (o.y - t.rect.y).abs() <= 2
                            && (o.w - t.rect.w).abs() <= 2
                            && (o.h - t.rect.h).abs() <= 2
                        {
                            origins.insert(t.id, t.rect);
                        }
                    }
                    pictures.retain(|(id, c, name), _| {
                        s.tiles
                            .iter()
                            .any(|t| t.id == *id && t.cookie == *c && &t.monitor == name)
                    });
                    failed_sources.retain(|(id, c, name), _| {
                        s.tiles
                            .iter()
                            .any(|t| t.id == *id && t.cookie == *c && &t.monitor == name)
                    });
                    {
                        let _hspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderHosts);
                        hosts.retain(|k, h| s.monitors.iter().any(|(n, _, b)| n == k && b == &h.b));
                        applied.retain(|k, _| hosts.contains_key(k));
                        for (n, _, b) in &s.monitors {
                            if !hosts.contains_key(n) {
                                if let Ok(h) = Host::new(*b) {
                                    hosts.insert(n.clone(), h);
                                }
                            }
                        }
                    }
                    let hosts_ms = handle_start.elapsed().as_millis();
                    {
                        let _sspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderSources);
                        const WGC_RETRY_COOLDOWN: Duration = Duration::from_secs(5);
                        for tile in &s.tiles {
                            let key = (tile.id, tile.cookie, tile.monitor.clone());
                            if tile.pinned || pictures.contains_key(&key) {
                                continue;
                            }
                            if failed_sources
                                .get(&key)
                                .is_some_and(|at| at.elapsed() < WGC_RETRY_COOLDOWN)
                            {
                                continue;
                            }
                            if let Some(surface) =
                                hosts.get(&tile.monitor).and_then(|h| h.surface.as_ref())
                            {
                                let t = Instant::now();
                                match Source::new(tile.id, &surface.gpu) {
                                    Ok(source) => {
                                        failed_sources.remove(&key);
                                        pictures.insert(key, source);
                                    }
                                    Err(e) => {
                                        let first = failed_sources.insert(key, Instant::now()).is_none();
                                        if first {
                                            diag::trace(format!("WGC source creation failed for {}: {e}", tile.id));
                                            eprintln!("source creation: {e}");
                                        }
                                    }
                                }
                                if t.elapsed().as_millis() >= 100 {
                                    diag::trace(format!("SLOW render Source::new: {} ms", t.elapsed().as_millis()))
                                }
                            }
                        }
                    }
                    let sources_ms = handle_start.elapsed().as_millis();
                    {
                        let _fspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderFrames);
                        for ((id, _, name), source) in &mut pictures {
                            if let Some(host) = hosts.get(name) {
                                if win32::frame(*id)
                                    .and_then(|r| intersect(r, host.b))
                                    .is_some()
                                {
                                    if let Some(surface) = &host.surface {
                                        let deadline = Instant::now() + Duration::from_millis(70);
                                        loop {
                                            if source.bitmap.is_some()
                                                || source.update(surface, 0).is_err()
                                                || source.bitmap.is_some()
                                                || Instant::now() >= deadline
                                            {
                                                break;
                                            }
                                            std::thread::sleep(Duration::from_millis(2))
                                        }
                                    }
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
                    begin = Instant::now();
                    last = begin;
                    scene = Some(s)
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                _ => (),
            }
            let Some(s) = scene.as_ref() else {
                for ((id, _, name), source) in &mut pictures {
                    if let Some(host) = hosts.get(name) {
                        if let (Some(surface), Some(r)) = (&host.surface, win32::frame(*id)) {
                            if intersect(r, host.b) == Some(r) {
                                let _ = source.update(surface, settled);
                            }
                        }
                    }
                }
                continue;
            };
            if s.epoch != EPOCH.load(std::sync::atomic::Ordering::Acquire) {
                scene = None;
                continue;
            }
            let time = Instant::now();
            let t = if s.duration == 0 {
                1.
            } else {
                planor::motion(
                    begin.elapsed().as_secs_f64() * 1000. / s.duration as f64,
                    s.spring,
                )
            };
            let mut placements = vec![];
            let mut settled_tiles = vec![];
            let park = s.monitors.iter().map(|m| m.2.x + m.2.w).max().unwrap_or(0) + 1024;
            let mut hits = vec![];
            let mut next = HashMap::new();
            for (name, host) in &mut hosts {
                let mut lines = vec![];
                let mut backing = vec![];
                let mut full = false;
                let mut keep = std::collections::HashSet::new();
                let mut host_hits = vec![];
                for tile in s.tiles.iter().filter(|tile| &tile.monitor == name) {
                    if GetPropW(tile.id as _, tag.as_ptr()) as usize != tile.cookie
                        || IsIconic(tile.id as _) != 0
                    {
                        continue;
                    }
                    let r = interpolate(origins[&tile.id], tile.rect, t);
                    next.insert(tile.id, r);
                    // While moving, the stand-in keeps the window's real size and only its position
                    // glides; scaling it to the in-between rectangle stretched the contents.
                    let o = origins[&tile.id];
                    let rs = if t < 1. { Rect { w: o.w, h: o.h, ..r } } else { r };
                    let bound = intersect(tile.bound, host.b).unwrap_or(host.b);
                    let clip = if tile.hidden { None } else { intersect(rs, bound) };
                    let moving = t < 1. && origins[&tile.id] != tile.rect;
                    if !moving && tile.fullscreen && tile.rect == host.b {
                        full = true
                    }
                    let proxy = !tile.pinned && (moving || clip != Some(rs) || tile.preview || tile.hidden);
                    if let Some(c) = clip {
                        if proxy {
                            if let (Some(source), Some(surface)) = (
                                pictures.get_mut(&(tile.id, tile.cookie, name.clone())),
                                host.surface.as_ref(),
                            ) {
                                if source.bitmap.is_none() {
                                    if let Err(e) = source.update(surface, 0) {
                                        eprintln!("source frame: {e}")
                                    }
                                }
                                if let Some(bitmap) = &source.bitmap {
                                    backing.push((
                                        Rect {
                                            x: rs.x - host.b.x,
                                            y: rs.y - host.b.y,
                                            ..rs
                                        },
                                        bitmap.clone(),
                                    ));
                                }
                            }
                        }
                        if proxy && host.thumbnail(tile.id, rs, c) {
                            if moving
                                && pictures
                                    .get(&(tile.id, tile.cookie, name.clone()))
                                    .is_some_and(|src| src.bitmap.is_some())
                            {
                                let p = DWM_THUMBNAIL_PROPERTIES {
                                    dwFlags: DWM_TNP_VISIBLE,
                                    fVisible: 0,
                                    ..Default::default()
                                };
                                DwmUpdateThumbnailProperties(host.thumbs[&tile.id], &p);
                            }
                            keep.insert(tile.id);
                            if !moving {
                                host_hits.push((c, tile.id))
                            }
                        }
                        if let Some(color) = tile.color {
                            lines.push((
                                Rect {
                                    x: r.x - host.b.x,
                                    y: r.y - host.b.y,
                                    ..r
                                },
                                color,
                            ))
                        }
                    }
                    let target = if proxy {
                        Rect {
                            x: park,
                            ..tile.rect
                        }
                    } else {
                        tile.rect
                    };
                    if !tile.pinned {
                        placements.push((tile.id, target));
                        if t >= 1. {
                            settled_tiles.push(planor::SettledTile {
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
                    if keep.contains(id) {
                        true
                    } else {
                        DwmUnregisterThumbnail(*h);
                        false
                    }
                });
                if !full {
                    hits.append(&mut host_hits)
                }
                if full || (keep.is_empty() && lines.is_empty()) {
                    ShowWindow(host.h, SW_HIDE);
                } else {
                    ShowWindow(host.h, SW_SHOWNOACTIVATE);
                    // Cut unmanaged windows out of the overlay (SetWindowRgn); only when they changed.
                    let mine: Vec<Rect> = s
                        .holes
                        .iter()
                        .filter_map(|h| intersect(*h, host.b))
                        .collect();
                    if applied.get(name) != Some(&mine) {
                        let _rgn_span = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderRgnHoles);
                        if mine.is_empty() {
                            windows_sys::Win32::Graphics::Gdi::SetWindowRgn(host.h, null_mut(), 1);
                        } else {
                            let whole = windows_sys::Win32::Graphics::Gdi::CreateRectRgn(0, 0, host.b.w, host.b.h);
                            for h in &mine {
                                let cut = windows_sys::Win32::Graphics::Gdi::CreateRectRgn(
                                    h.x - host.b.x,
                                    h.y - host.b.y,
                                    h.x - host.b.x + h.w,
                                    h.y - host.b.y + h.h,
                                );
                                windows_sys::Win32::Graphics::Gdi::CombineRgn(whole, whole, cut, windows_sys::Win32::Graphics::Gdi::RGN_DIFF);
                                windows_sys::Win32::Graphics::Gdi::DeleteObject(cut as _);
                            }
                            windows_sys::Win32::Graphics::Gdi::SetWindowRgn(host.h, whole, 1);
                        }
                        applied.insert(name.clone(), mine);
                    }
                    if let Some(surface) = host.surface.as_mut() {
                        let dpi = GetDpiForWindow(host.h) as f32 / 96.;
                        let t = Instant::now();
                        let drawn = {
                            let _dspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderDraw);
                            surface.draw(&backing, &lines, s.width * dpi, s.radius * dpi)
                        };
                        if t.elapsed().as_millis() >= 100 {
                            diag::trace(format!("SLOW render draw: {} ms", t.elapsed().as_millis()))
                        }
                        if let Err(e) = drawn {
                            eprintln!("border draw: {e}");
                            host.surface = None;
                        }
                    }
                }
            }
            win32::set_peeks(hits);
            let start = Instant::now();
            let mut retries = 0;
            let mut place_ok = false;
            let mut rejected = Vec::new();
            while s.epoch == EPOCH.load(std::sync::atomic::Ordering::Acquire) {
                if s.diagnostics && retries == 0 {
                    for (id, want) in &placements {
                        if let Some(got) = win32::frame(*id) {
                            if got != *want {
                                diag::trace(format!("PLACE {id} {:?} -> {:?}", (got.x, got.y, got.w, got.h), (want.x, want.y, want.w, want.h)));
                            }
                        }
                    }
                }
                let placed_at = Instant::now();
                let result = win32::place(&placements);
                if placed_at.elapsed().as_millis() >= 100 {
                    diag::trace(format!("SLOW render place: {} ms", placed_at.elapsed().as_millis()))
                }
                rejected.clear();
                if t >= 1. && result.is_ok() {
                    for &(id, want) in &placements {
                        if want.x != park {
                            if let Some(got) = win32::frame(id) {
                                if got.w > want.w + 1 || got.h > want.h + 1 {
                                    rejected.push((id, got))
                                }
                            }
                        }
                    }
                }
                if result.is_ok() && rejected.is_empty() {
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
            if !rejected.is_empty() {
                win32::emit(win32::Event::Rejected(s.epoch, rejected.clone()));
            }
            if place_ok && (!placements.is_empty() || t >= 1.) {
                crate::perf::mark_first_place_done();
            }
            if place_ok && !gesture_recorded {
                if let Some(in_qpc) = s.input_qpc_us {
                    gesture_recorded = true;
                    crate::perf::record_duration_from(
                        crate::perf::SpanKind::GestureToPlace,
                        in_qpc,
                        s.epoch,
                    );
                }
            }
            if place_ok && t >= 1. && rejected.is_empty() && parked_checked_epoch != s.epoch && !settled_tiles.is_empty() {
                parked_checked_epoch = s.epoch;
                let actuals: Vec<(isize, Rect)> = settled_tiles
                    .iter()
                    .filter(|st| {
                        s.tiles.iter().any(|t| {
                            t.id == st.id
                                && GetPropW(t.id as _, tag.as_ptr()) as usize == t.cookie
                                && IsIconic(t.id as _) == 0
                        })
                    })
                    .filter_map(|st| win32::frame(st.id).map(|r| (st.id, r)))
                    .collect();
                let violations = planor::check_settled_parked(&settled_tiles, &actuals, park);
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
            let placement_us = start.elapsed().as_micros() as u64;
            current = next;
            {
                let _fspan = crate::perf::SpanGuard::new(crate::perf::SpanKind::RenderDwmFlush);
                DwmFlush();
            }
            if s.diagnostics {
                win32::emit(win32::Event::Frame(diag::Frame {
                    input_to_present_us: s.input.map(|v| v.elapsed().as_micros() as u64),
                    interval_us: last.elapsed().as_micros() as _,
                    render_us: time.elapsed().as_micros() as _,
                    placement_us,
                    thumbnail_us: start.duration_since(time).as_micros() as _,
                    retries,
                }));
            }
            last = Instant::now();
            if t >= 1. {
                settled = timestamp() + 330_000;
                let current_epoch = s.epoch == EPOCH.load(std::sync::atomic::Ordering::Acquire);
                scene = None;
                if current_epoch {
                    verify = Some((
                        Instant::now() + Duration::from_millis(250),
                        placements.iter().filter(|p| p.1.x != park).copied().collect(),
                        2,
                    ))
                }
            }
        }
    });
    tx
}
