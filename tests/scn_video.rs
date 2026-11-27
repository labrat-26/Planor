#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

// #106: Live real-window tests for scrolling D3D11 flip-model video windows (normal and
// borderless fullscreen) and Picture-in-Picture (WS_EX_TOPMOST | WS_EX_TOOLWINDOW) windows.
//
// Flake note: these tests read the primary output through DXGI desktop duplication and judge exact pixel colours
// (FOCUS_RGB, PIP_RGB, the video's two clear colours). Any other window, toast or cursor over the sampled rects
// breaks them, so they run serial (not in verify.ps1's parallel allowlist) and only under the desktop lock.
// Duplication delivers no frame while nothing changes, so every check repaints/presents and counts frames before
// it judges. The one recorded FAIL (verify 20261126-215337) was the intended negative control (PiP class not
// named in PLANOR_HOLES), not a flake.

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use windows::{
    Win32::{
        Foundation as WF,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
                DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice,
                IDXGIFactory2, IDXGISwapChain1,
            },
        },
    },
};

struct VideoStats {
    wm_sizes: Mutex<Vec<(u32, u32)>>,
    resize_buffers: AtomicUsize,
    presents: AtomicUsize,
    present_errors: AtomicUsize,
}

static VIDEO_REGISTRY: OnceLock<Mutex<std::collections::HashMap<isize, Arc<VideoStats>>>> =
    OnceLock::new();

fn video_registry() -> &'static Mutex<std::collections::HashMap<isize, Arc<VideoStats>>> {
    VIDEO_REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// The video window's own class. It must not be the sentinel class: `RegisterClassW` keeps whichever
/// registration came first, so sharing it made the wndproc (and the sentinels' painting) depend on
/// creation order. The `PlanorSentinel` prefix keeps the user's real daemon off it
/// (`never_tiled_class`); the isolated test daemon tiles it through `tile_video_class`.
fn video_class() -> String {
    format!("PlanorSentinelVideo_p{}", std::process::id())
}

/// Add a tile rule for `video_class()` to the isolated daemon's config (before its catch-all ignore
/// rule). Call before `Daemon::start`.
fn tile_video_class(d: &Daemon) {
    let path = d.dir.join("planor/planor.toml");
    let cfg = fs::read_to_string(&path).unwrap();
    let catch_all = "[[rules]]\nclass=\"*\"\naction=\"ignore\"\n";
    assert!(cfg.contains(catch_all), "test config has no catch-all ignore rule");
    let rule = format!("[[rules]]\nclass={:?}\naction=\"tile\"\n", video_class());
    fs::write(&path, cfg.replacen(catch_all, &(rule + catch_all), 1)).unwrap();
}

/// Set an environment variable for the rest of the scope, removed again on drop (also when the test
/// panics), so a failing test cannot leak it into the next one in the same process.
struct EnvGuard(&'static str);
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        // SAFETY: live tests run with --test-threads=1; nothing else reads the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self(key)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

/// Join a test window's thread and re-raise a panic from it (a failed `CreateWindowExW`, D3D11 device or
/// swapchain), unless the test is already unwinding.
fn join_window_thread(join: Option<thread::JoinHandle<()>>) {
    if let Some(j) = join
        && let Err(payload) = j.join()
        && !thread::panicking()
    {
        std::panic::resume_unwind(payload);
    }
}

unsafe extern "system" fn video_wndproc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    if m == WM_SIZE {
        let cw = (l & 0xFFFF) as u32;
        let ch = ((l >> 16) & 0xFFFF) as u32;
        if let Some(stats) = video_registry().lock().unwrap().get(&(h as isize)).cloned() {
            stats.wm_sizes.lock().unwrap().push((cw, ch));
        }
    } else if m == WM_CLOSE {
        // The render thread owns teardown: the swapchain must be released before its window is
        // destroyed, so a close never runs DefWindowProc's DestroyWindow here.
        return 0;
    } else if m == WM_DESTROY {
        video_registry().lock().unwrap().remove(&(h as isize));
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

/// Real Win32 window owning a D3D11 flip-model swapchain presenting a changing frame every 16 ms.
struct VideoWindow {
    hwnd: isize,
    stats: Arc<VideoStats>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl VideoWindow {
    fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let stats = Arc::new(VideoStats {
            wm_sizes: Mutex::new(Vec::new()),
            resize_buffers: AtomicUsize::new(0),
            presents: AtomicUsize::new(0),
            present_errors: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let stats_t = stats.clone();
        let stop_t = stop.clone();

        let join = thread::spawn(move || unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let class_name = wide(&video_class());
            let inst = GetModuleHandleW(null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(video_wndproc),
                hInstance: inst,
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                wide("PlanorVideoTest").as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                x,
                y,
                w,
                h,
                null_mut(),
                null_mut(),
                inst,
                null(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW failed for VideoWindow");
            video_registry()
                .lock()
                .unwrap()
                .insert(hwnd as isize, stats_t.clone());
            UpdateWindow(hwnd);

            // Create real D3D11 device + DXGI flip-model swapchain bound to `hwnd`.
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                WF::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .expect("D3D11CreateDevice");
            let device = device.unwrap();
            let ctx = ctx.unwrap();
            let dxgi_dev: IDXGIDevice = device.cast().expect("cast IDXGIDevice");
            let adapter = dxgi_dev.GetAdapter().expect("GetAdapter");
            let factory: IDXGIFactory2 = adapter.GetParent().expect("GetParent IDXGIFactory2");

            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client);
            let mut sc_w = (client.right - client.left).max(1) as u32;
            let mut sc_h = (client.bottom - client.top).max(1) as u32;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: sc_w,
                Height: sc_h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swapchain: IDXGISwapChain1 = factory
                .CreateSwapChainForHwnd(&device, WF::HWND(hwnd as _), &desc, None, None)
                .expect("CreateSwapChainForHwnd");

            let make_rtv = |sc: &IDXGISwapChain1| -> ID3D11RenderTargetView {
                let back: ID3D11Texture2D = sc.GetBuffer(0).expect("GetBuffer(0)");
                let mut rtv = None;
                device
                    .CreateRenderTargetView(&back, None, Some(&mut rtv))
                    .expect("CreateRenderTargetView");
                rtv.unwrap()
            };
            let mut rtv = Some(make_rtv(&swapchain));

            tx.send(hwnd as isize).unwrap();

            let mut frame_idx: u32 = 0;
            let mut msg = MSG::default();
            while !stop_t.load(Ordering::Acquire) {
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        stop_t.store(true, Ordering::Release);
                        break;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if stop_t.load(Ordering::Acquire) {
                    break;
                }
                // Check if client size changed; resize swapchain buffers just like a real video player.
                let mut cur = RECT::default();
                GetClientRect(hwnd, &mut cur);
                let cw = (cur.right - cur.left).max(1) as u32;
                let ch = (cur.bottom - cur.top).max(1) as u32;
                if cw != sc_w || ch != sc_h {
                    rtv = None;
                    if swapchain
                        .ResizeBuffers(0, cw, ch, DXGI_FORMAT_B8G8R8A8_UNORM, Default::default())
                        .is_ok()
                    {
                        sc_w = cw;
                        sc_h = ch;
                        stats_t.resize_buffers.fetch_add(1, Ordering::AcqRel);
                        rtv = Some(make_rtv(&swapchain));
                    }
                }
                if let Some(ref view) = rtv {
                    // A steady red, so the window does not strobe on screen. Its red channel drifts 205..225
                    // and back over about 0.6 s, a change too small to see. That keeps every present changing
                    // the pixels (desktop duplication delivers no frame for an unchanged screen), and the test
                    // still finds the video's pixels with `is_video_px`: RGB (205..=225, 64, 89).
                    let t = frame_idx % 40;
                    let red = 205 + if t < 20 { t } else { 40 - t };
                    let color = [red as f32 / 255.0, 64.0 / 255.0, 89.0 / 255.0, 1.0];
                    ctx.ClearRenderTargetView(view, &color);
                    let hr = swapchain.Present(0, DXGI_PRESENT(0));
                    if hr.is_ok() {
                        stats_t.presents.fetch_add(1, Ordering::AcqRel);
                    } else {
                        stats_t.present_errors.fetch_add(1, Ordering::AcqRel);
                    }
                    frame_idx = frame_idx.wrapping_add(1);
                }
                thread::sleep(Duration::from_millis(16));
            }
            // Teardown in dependency order: unbind and flush the device context, release the render
            // target and swapchain, then destroy the window they were bound to, then its class.
            ctx.ClearState();
            ctx.Flush();
            drop(rtv);
            drop(swapchain);
            drop(ctx);
            drop(device);
            let _ = DestroyWindow(hwnd);
            UnregisterClassW(class_name.as_ptr(), inst);
        });

        let Ok(hwnd) = rx.recv() else {
            // The thread died before sending its window: surface its panic, not a RecvError.
            join_window_thread(Some(join));
            unreachable!("VideoWindow thread exited without a window or a panic");
        };
        Self {
            hwnd,
            stats,
            stop,
            join: Some(join),
        }
    }

    fn rect(&self) -> RECT {
        let mut r = RECT::default();
        unsafe { GetWindowRect(self.hwnd as _, &mut r) };
        r
    }

    fn make_borderless_fullscreen(&self, mon: RECT) {
        unsafe {
            SetWindowLongW(self.hwnd as _, GWL_STYLE, (WS_POPUP | WS_VISIBLE) as i32);
            SetWindowPos(
                self.hwnd as _,
                null_mut(),
                mon.left,
                mon.top,
                mon.right - mon.left,
                mon.bottom - mon.top,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn wm_size_count(&self) -> usize {
        self.stats.wm_sizes.lock().unwrap().len()
    }

    fn resize_buffers_count(&self) -> usize {
        self.stats.resize_buffers.load(Ordering::Acquire)
    }

    fn presents(&self) -> usize {
        self.stats.presents.load(Ordering::Acquire)
    }

    fn present_errors(&self) -> usize {
        self.stats.present_errors.load(Ordering::Acquire)
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the render loop; it releases the swapchain and destroys the window itself.
        unsafe { PostMessageW(self.hwnd as _, WM_NULL, 0, 0) };
        join_window_thread(self.join.take());
    }
}
// ---- Pixel checks -------------------------------------------------------------------------------------------

/// Planor focus border colour (planor.example.toml [borders].focused_color = #3B82F6), as 0x00RRGGBB.
const FOCUS_RGB: u32 = 0x003b82f6;
/// The PiP window's colour on screen (0x00RRGGBB); its brush is the COLORREF 0x0034AB12.
const PIP_RGB: u32 = 0x0012ab34;

/// The video window clears to a steady RGB (205..=225, 64, 89) (see `VideoWindow`); the range below also takes
/// small colour-conversion error.
fn is_video_px(px: u32) -> bool {
    let (r, g, b) = ((px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff);
    (54..=74).contains(&g) && r.max(b) >= 200 && (70..=110).contains(&r.min(b))
}

/// One desktop-duplication frame of the primary output: (pixels inside `r` matching `want`, pixels of `r` on the
/// output). None when no new frame arrived within 100 ms (duplication delivers nothing while nothing changes).
fn count_in(cap: &Capture, r: RECT, want: impl Fn(u32) -> bool) -> Option<(usize, usize)> {
    unsafe {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res = None;
        if cap.dupl.AcquireNextFrame(100, &mut info, &mut res).is_err() {
            return None;
        }
        let result = (|| -> windows::core::Result<(usize, usize)> {
            let tex: ID3D11Texture2D = res.unwrap().cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as _;
            desc.BindFlags = 0;
            desc.MiscFlags = 0;
            let mut copy = None;
            cap.dev.CreateTexture2D(&desc, None, Some(&mut copy))?;
            let copy = copy.unwrap();
            cap.ctx.CopyResource(&copy, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            cap.ctx.Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let (x0, y0) = ((r.left - cap.origin.0).max(0), (r.top - cap.origin.1).max(0));
            let x1 = (r.right - cap.origin.0).min(desc.Width as i32).max(x0);
            let y1 = (r.bottom - cap.origin.1).min(desc.Height as i32).max(y0);
            let (mut hit, mut total) = (0usize, 0usize);
            for y in y0..y1 {
                let row = std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y as usize * mapped.RowPitch as usize).cast::<u32>(),
                    desc.Width as usize,
                );
                for px in &row[x0 as usize..x1 as usize] {
                    total += 1;
                    hit += usize::from(want(px & 0xffffff));
                }
            }
            cap.ctx.Unmap(&copy, 0);
            Ok((hit, total))
        })();
        let _ = cap.dupl.ReleaseFrame();
        result.ok()
    }
}

fn shrink(r: RECT, by_w: i32, by_h: i32) -> RECT {
    RECT { left: r.left + by_w, top: r.top + by_h, right: r.right - by_w, bottom: r.bottom - by_h }
}

fn client_screen_rect(hwnd: isize) -> RECT {
    let mut c = RECT::default();
    let mut p = POINT { x: 0, y: 0 };
    unsafe {
        GetClientRect(hwnd as _, &mut c);
        ClientToScreen(hwnd as _, &mut p);
    }
    RECT { left: p.x, top: p.y, right: p.x + c.right, bottom: p.y + c.bottom }
}

/// Whether a frame showed at least `frac` of `r` in the video colours within 3 s (the video presents
/// continuously, so frames keep arriving), and the best fraction seen.
fn video_visible_in(cap: &Capture, r: RECT, frac: f64) -> (bool, f64) {
    let until = Instant::now() + Duration::from_secs(3);
    let mut best = 0.0f64;
    while Instant::now() < until {
        if let Some((hit, total)) = count_in(cap, r, is_video_px) {
            let f = hit as f64 / total.max(1) as f64;
            best = best.max(f);
            if f >= frac {
                return (true, f);
            }
        }
    }
    (false, best)
}

// ---- Real Picture-in-Picture window -------------------------------------------------------------------------

/// Real Picture-in-Picture style floating window: small, unowned, WS_EX_TOPMOST | WS_EX_TOOLWINDOW, solid PIP_RGB,
/// shown without activation. Its class must not start with "Planor" (Planor's own windows never punch holes).
struct PipWindow {
    hwnd: isize,
    join: Option<thread::JoinHandle<()>>,
}

fn pip_class() -> String {
    format!("VideoPiPTest_p{}", std::process::id())
}

unsafe extern "system" fn pip_wndproc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    if m == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

impl PipWindow {
    fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let class_name = wide(&pip_class());
            let inst = GetModuleHandleW(null());
            let brush = CreateSolidBrush(0x0034AB12); // COLORREF 0x00BBGGRR -> RGB 0x12AB34
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(pip_wndproc),
                hInstance: inst,
                hbrBackground: brush,
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                wide("Picture in picture").as_ptr(),
                WS_POPUP,
                x,
                y,
                w,
                h,
                null_mut(),
                null_mut(),
                inst,
                null(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW failed for PipWindow");
            // Like a browser PiP: shown without taking the focus from the page.
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(hwnd);
            tx.send(hwnd as isize).unwrap();
            let mut m = MSG::default();
            while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            UnregisterClassW(class_name.as_ptr(), inst);
            DeleteObject(brush as _);
        });
        let Ok(hwnd) = rx.recv() else {
            join_window_thread(Some(join));
            unreachable!("PipWindow thread exited without a window or a panic");
        };
        Self { hwnd, join: Some(join) }
    }

    fn rect(&self) -> RECT {
        let mut r = RECT::default();
        unsafe { GetWindowRect(self.hwnd as _, &mut r) };
        r
    }

    fn is_topmost(&self) -> bool {
        unsafe { (GetWindowLongW(self.hwnd as _, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST != 0 }
    }

    fn repaint(&self) {
        unsafe { RedrawWindow(self.hwnd as _, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW) };
    }
}

impl Drop for PipWindow {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as _, WM_CLOSE, 0, 0) };
        join_window_thread(self.join.take());
    }
}

/// The test daemon's own overlay host windows (class "Planor.Composition", owned by process `pid`).
fn overlay_hosts(pid: u32) -> Vec<isize> {
    unsafe extern "system" fn cb(h: HWND, l: isize) -> i32 {
        let (pid, out) = unsafe { &mut *(l as *mut (u32, Vec<isize>)) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut owner) };
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 64) }.max(0) as usize;
        if owner == *pid && String::from_utf16_lossy(&buf[..n]) == "Planor.Composition" {
            out.push(h as isize);
        }
        1
    }
    let mut acc: (u32, Vec<isize>) = (pid, vec![]);
    unsafe { EnumWindows(Some(cb), &mut acc as *mut _ as isize) };
    acc.1
}

/// True if `below` comes after `above` in the top-to-bottom z-order.
fn z_below(below: isize, above: isize) -> bool {
    let mut h = unsafe { GetWindow(above as _, GW_HWNDNEXT) };
    while !h.is_null() {
        if h as isize == below {
            return true;
        }
        h = unsafe { GetWindow(h, GW_HWNDNEXT) };
    }
    false
}

// ---- Daemon helpers -----------------------------------------------------------------------------------------

fn daemon_state(d: &Daemon) -> serde_json::Value {
    let out = d.call_output(&["state", "--json"]);
    assert!(out.status.success(), "planor state --json failed");
    serde_json::from_slice(&out.stdout).expect("parse state --json")
}

fn tile_of(st: &serde_json::Value, id: isize) -> Option<&serde_json::Value> {
    st["tiles"].as_array()?.iter().find(|t| t["id"].as_i64() == Some(id as i64))
}

/// Offset of the strip holding `id`.
fn strip_offset(st: &serde_json::Value, id: isize) -> Option<i64> {
    st["strips"].as_array()?.iter().find_map(|s| {
        let holds = s["columns"].as_array()?.iter().any(|c| {
            c["members"].as_array().is_some_and(|m| m.iter().any(|v| v.as_i64() == Some(id as i64)))
        });
        if holds { s["offset"].as_i64() } else { None }
    })
}

fn primary_monitor_bounds() -> RECT {
    mons()[0].0
}

fn claim_fg(hwnd: isize) {
    for _ in 0..4 {
        if take_foreground(hwnd) && fg() == hwnd {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(fg(), hwnd, "failed to claim the foreground for 0x{hwnd:x}");
}

/// Write a touchpad recording of two three-finger swipes along the strip (`dx` finger units, then back) and play
/// it into the daemon through the same event bus as the real touchpad (`planor gestures replay`). Each swipe ends
/// with the fingers held still for 120 ms, so no momentum is left. Returns how long the replay takes.
fn replay_scroll(d: &Daemon, dx: f64) -> Duration {
    let path = d.dir.join("scroll.jsonl");
    let mut lines = vec![r#"{"kind":"meta","schema":1,"name":"scn_video-scroll","synthetic":true}"#.to_string()];
    let mut t: i64 = 0;
    let (steps, hold) = (45, 15);
    for (from, to) in [(600.0, 600.0 + dx), (600.0 + dx, 600.0)] {
        for i in 0..=steps + hold {
            let x: f64 = from + (to - from) * (i.min(steps) as f64 / steps as f64);
            lines.push(format!(
                r#"{{"t":{t},"kind":"contacts","pts":[[{:.1},300],[{:.1},312],[{:.1},300]]}}"#,
                x,
                x + 40.,
                x + 80.
            ));
            t += 8000;
        }
        lines.push(format!(r#"{{"t":{t},"kind":"contacts","pts":[]}}"#));
        t += 8000;
    }
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    let out = d.call_output(&["gestures", "replay", path.to_str().unwrap(), "--gap-ms", "500"]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success() && said.contains("replaying 2 gestures"), "gesture replay refused: {said:?}");
    Duration::from_micros(t as u64) + Duration::from_millis(2 * 500)
}

// ---- Tests --------------------------------------------------------------------------------------------------

#[test]
#[ignore]
fn d3d11_flip_video_window_scroll_no_resize_or_cover() {
    // (#106a) A tiled window owning a D3D11 flip-model swapchain presenting every 16 ms. The strip scrolls it off
    // the screen and back, by focus walk and by a live three-finger scroll: its client (so its swapchain) never
    // changes size, it keeps presenting, and whenever it is focused and settled it is on screen, not under a
    // placeholder card or anything else of Planor's.
    aware();
    let s0 = Sentinel::at(0, 0, 40, 40);
    let video = VideoWindow::new(120, 80, 640, 480);
    let s2 = Sentinel::at(2, 0, 200, 120);
    let s3 = Sentinel::at(3, 0, 280, 120);
    let s4 = Sentinel::at(4, 0, 360, 120);

    let mut d = Daemon::new_animated("video-scroll");
    tile_video_class(&d);
    d.start();
    assert!(wait_settled(&d, 5, Duration::from_secs(6)), "the five windows did not settle");
    let st = daemon_state(&d);
    let cols: Vec<i64> = st["strips"][0]["columns"]
        .as_array()
        .map(|c| c.iter().filter_map(|c| c["active_id"].as_i64()).collect())
        .unwrap_or_default();
    let want: Vec<i64> = [s0.hwnd, video.hwnd, s2.hwnd, s3.hwnd, s4.hwnd].iter().map(|&h| h as i64).collect();
    assert_eq!(cols, want, "setup: the strip holds the five windows left to right in creation order");
    claim_fg(s0.hwnd);
    assert!(wait_settled(&d, 5, Duration::from_secs(4)), "layout did not settle after focusing the first window");
    thread::sleep(Duration::from_millis(400));
    let mon = primary_monitor_bounds();
    let cap = Capture::open(0).expect("DXGI primary output duplication");
    assert_eq!(cap.origin, (mon.left, mon.top), "DXGI output 0 is not the primary monitor");

    let base_sizes = video.wm_size_count();
    let base_client = client_screen_rect(video.hwnd);
    let base_size = ((base_client.right - base_client.left) as u32, (base_client.bottom - base_client.top) as u32);
    let base_rb = video.resize_buffers_count();
    let mut presents = video.presents();

    // Focus walk s0 -> s4 and back. The video is the second column, so it is parked while s3/s4 are focused.
    let mut parked_seen = false;
    let mut visible_checks = 0;
    for (i, &step) in [RIGHT, RIGHT, RIGHT, RIGHT, LEFT, LEFT, LEFT, LEFT].iter().enumerate() {
        press(&[ALT], step);
        assert!(wait_settled(&d, 5, Duration::from_secs(4)), "step {i}: layout did not settle");
        let st = daemon_state(&d);
        let tile = tile_of(&st, video.hwnd).expect("video tile in state");
        parked_seen |= tile["parked"].as_bool() == Some(true);
        if fg() == video.hwnd {
            let r = shrink(client_screen_rect(video.hwnd), 40, 40);
            let (ok, got) = video_visible_in(&cap, r, 0.9);
            assert!(
                ok,
                "step {i}: the focused, settled video must be on screen; at best {:.0}% of its client showed video pixels",
                got * 100.
            );
            visible_checks += 1;
        }
    }
    assert_eq!(fg(), s0.hwnd, "the focus walk must end back on the first window");
    assert!(parked_seen, "the focus walk never parked the video window: the test did not scroll it off screen");
    assert_eq!(visible_checks, 2, "the video window must have been focused twice during the walk");
    let walk_presents = video.presents() - presents;
    assert!(walk_presents >= 60, "the video must keep presenting during the walk (presents={walk_presents})");

    // Live three-finger scroll one way and back (whichever way the strip can go from the first column).
    presents = video.presents();
    let off0 = strip_offset(&daemon_state(&d), video.hwnd).unwrap();
    let play = replay_scroll(&d, 400.0);
    let until = Instant::now() + play + Duration::from_millis(300);
    let mut moved = 0i64;
    while Instant::now() < until {
        if let Some(o) = strip_offset(&daemon_state(&d), video.hwnd) {
            moved = moved.max((o - off0).abs());
        }
        thread::sleep(Duration::from_millis(30));
    }
    assert!(wait_settled(&d, 5, Duration::from_secs(4)), "layout did not settle after the live scroll");
    assert!(moved >= 300, "the replayed three-finger swipe must scroll the strip (max offset change {moved} px)");
    let scroll_presents = video.presents() - presents;
    assert!(scroll_presents >= 30, "the video must keep presenting during the live scroll (presents={scroll_presents})");

    let sizes: Vec<(u32, u32)> = video.stats.wm_sizes.lock().unwrap()[base_sizes..].to_vec();
    assert!(
        sizes.iter().all(|&s| s == base_size),
        "the video's client size must never change while scrolling: baseline {base_size:?}, WM_SIZE {sizes:?}"
    );
    assert_eq!(video.resize_buffers_count(), base_rb, "scrolling must never make the video resize its swapchain");
    assert_eq!(video.present_errors(), 0, "the video must present without errors");
    d.stop();
}

#[test]
#[ignore]
fn fullscreen_borderless_d3d11_video_window_scroll_preserves_frame() {
    // (#106b, #77, #78) A borderless fullscreen D3D11 flip-model video, one pixel short of the monitor height as
    // Chromium leaves it, is scrolled off the screen and back (focus walk and live three-finger scroll). Its frame
    // stays within 2 px of the monitor size at every sample, its client size never changes (no swapchain resize on
    // park/unpark), and it is on screen again once focused.
    use std::cell::Cell;
    aware();
    let mon = primary_monitor_bounds();
    let (mon_w, mon_h) = (mon.right - mon.left, mon.bottom - mon.top);
    let s0 = Sentinel::at(0, 0, 40, 40);
    let video = VideoWindow::new(120, 80, 640, 480);
    let s2 = Sentinel::at(2, 0, 200, 120);

    let mut d = Daemon::new_animated("video-fs");
    tile_video_class(&d);
    d.start();
    assert!(wait_settled(&d, 3, Duration::from_secs(6)), "the three windows did not settle");
    claim_fg(video.hwnd);
    assert!(wait_settled(&d, 3, Duration::from_secs(4)), "layout did not settle after focusing the video");

    video.make_borderless_fullscreen(mon);
    let entered = wait_until(Duration::from_secs(6), || {
        tile_of(&daemon_state(&d), video.hwnd).is_some_and(|t| t["fullscreen"].as_bool() == Some(true))
    });
    assert!(entered, "the daemon did not recognise the monitor-sized borderless video as fullscreen");
    thread::sleep(Duration::from_millis(400));
    // Chromium's borderless fullscreen often ends up at h - 1; Planor must keep that size, not force h (#77).
    unsafe {
        SetWindowPos(video.hwnd as _, null_mut(), mon.left, mon.top, mon_w, mon_h - 1, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    thread::sleep(Duration::from_millis(500));
    assert!(
        tile_of(&daemon_state(&d), video.hwnd).is_some_and(|t| t["fullscreen"].as_bool() == Some(true)),
        "an h - 1 frame must keep the video fullscreen"
    );
    let cap = Capture::open(0).expect("DXGI primary output duplication");
    assert_eq!(cap.origin, (mon.left, mon.top), "DXGI output 0 is not the primary monitor");

    let base_sizes = video.wm_size_count();
    let base_rb = video.resize_buffers_count();
    let base_client = client_screen_rect(video.hwnd);
    let base_size = ((base_client.right - base_client.left) as u32, (base_client.bottom - base_client.top) as u32);
    assert_eq!(base_size, (mon_w as u32, (mon_h - 1) as u32), "setup: the borderless video's client is the h - 1 frame");
    let presents = video.presents();

    let parked_seen = Cell::new(false);
    let samples = Cell::new(0usize);
    let check_frame = |when: &str| {
        let r = video.rect();
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        assert!(
            (w - mon_w).abs() <= 2 && (h - mon_h).abs() <= 2,
            "{when}: the fullscreen video frame {w}x{h} must stay within 2 px of the monitor {mon_w}x{mon_h}"
        );
        if r.left >= mon.right || r.right <= mon.left {
            parked_seen.set(true);
        }
        samples.set(samples.get() + 1);
    };
    // Entering fullscreen moved the video's column to the end: [s0, s2, video]. Left twice parks it, right twice
    // brings it back.
    for (i, &step) in [LEFT, LEFT, RIGHT, RIGHT].iter().enumerate() {
        send_chord_raw(&[ALT], step);
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            check_frame(&format!("focus step {i}"));
            thread::sleep(Duration::from_millis(15));
        }
        assert!(wait_settled(&d, 3, Duration::from_secs(4)), "step {i}: layout did not settle");
        check_frame(&format!("focus step {i}, settled"));
    }
    assert!(parked_seen.get(), "the focus walk never parked the fullscreen video: it did not scroll off screen");
    assert_eq!(fg(), video.hwnd, "the focus walk must end back on the video");
    let r = video.rect();
    assert!(
        (r.left - mon.left).abs() <= 2 && (r.top - mon.top).abs() <= 2,
        "back in focus, the fullscreen video must cover the monitor again: {:?} vs monitor {:?}",
        (r.left, r.top, r.right, r.bottom),
        (mon.left, mon.top, mon.right, mon.bottom)
    );
    let centre = RECT {
        left: mon.left + mon_w / 4,
        top: mon.top + mon_h / 4,
        right: mon.right - mon_w / 4,
        bottom: mon.bottom - mon_h / 4,
    };
    let (ok, got) = video_visible_in(&cap, centre, 0.95);
    assert!(ok, "the fullscreen video must be on screen after unparking; at best {:.0}% video pixels", got * 100.);

    // Live three-finger scroll one way and back (whichever way the strip can go from the last column).
    parked_seen.set(false);
    let play = replay_scroll(&d, -400.0);
    let until = Instant::now() + play + Duration::from_millis(300);
    while Instant::now() < until {
        check_frame("live scroll");
        thread::sleep(Duration::from_millis(15));
    }
    assert!(wait_settled(&d, 3, Duration::from_secs(4)), "layout did not settle after the live scroll");
    check_frame("after the live scroll");
    assert!(parked_seen.get(), "the live scroll never moved the fullscreen video off screen");
    assert!(samples.get() > 100, "too few frame samples ({})", samples.get());

    let sizes: Vec<(u32, u32)> = video.stats.wm_sizes.lock().unwrap()[base_sizes..].to_vec();
    assert!(
        sizes.iter().all(|&s| s == base_size),
        "the fullscreen video's client size must never change (#77): baseline {base_size:?}, WM_SIZE {sizes:?}"
    );
    assert_eq!(video.resize_buffers_count(), base_rb, "park/unpark must never make the video resize its swapchain");
    let delta = video.presents() - presents;
    assert!(delta >= 60, "the fullscreen video must keep presenting (presents={delta})");
    assert_eq!(video.present_errors(), 0, "the fullscreen video must present without errors");
    drop(s0);
    drop(s2);
    d.stop();
}

#[test]
#[ignore]
fn pip_topmost_toolwindow_stays_untiled_unparked_and_topmost_during_scroll() {
    // (#106c) A Picture-in-Picture window (small, unowned, WS_EX_TOPMOST | WS_EX_TOOLWINDOW) over the focused
    // tile's border and below Planor's own topmost overlay in the z-order (as after the overlay is shown again):
    // the overlay never paints over it, neither the border nor the moving pictures of a scroll; it stays topmost
    // with its rect, is never tiled or parked, and the strip still scrolls under it.
    use std::cell::Cell;
    aware();
    let ws: Vec<Sentinel> = (0..3).map(|i| Sentinel::at(i, 0, 40 + 80 * i as i32, 60)).collect();
    let mut d = Daemon::new_animated("video-pip");
    // Isolated daemons carve overlay holes only for the class a test names (they share the desktop with the
    // developer's own windows); the real daemon carves them for every occluder. The daemon child inherits the
    // variable at spawn; the guard removes it from this process again even if the test panics.
    let holes = EnvGuard::set("PLANOR_HOLES", &pip_class());
    d.start();
    drop(holes);
    assert!(wait_settled(&d, 3, Duration::from_secs(6)), "the three windows did not settle");
    claim_fg(ws[0].hwnd);
    assert!(wait_settled(&d, 3, Duration::from_secs(4)), "layout did not settle after focusing the first window");
    thread::sleep(Duration::from_millis(300));
    let mon = primary_monitor_bounds();
    let cap = Capture::open(0).expect("DXGI primary output duplication");
    assert_eq!(cap.origin, (mon.left, mon.top), "DXGI output 0 is not the primary monitor");

    // The PiP straddles the focused window's right edge, where its focus border runs.
    let v = ws[0].visible();
    let (pw, ph) = (320, 180);
    let cy = (v.top + v.bottom) / 2;
    let at = RECT { left: v.right - pw / 2, top: cy - ph / 2, right: v.right + pw / 2, bottom: cy + ph / 2 };
    // Control: with nothing there, the focus border shows in that rectangle, so a zero later means something.
    let control = eventually(4, || {
        unsafe { RedrawWindow(ws[1].hwnd as _, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW) };
        count_in(&cap, at, |px| px == FOCUS_RGB).is_some_and(|c| c.0 > 0)
    });
    assert!(control, "control: expected the focus border #3B82F6 inside {:?} before the PiP window exists", (at.left, at.top, at.right, at.bottom));

    let pip = PipWindow::new(at.left, at.top, pw, ph);
    let pip_rect0 = pip.rect();
    assert_eq!((pip_rect0.left, pip_rect0.top), (at.left, at.top), "setup: the PiP window opened where asked");
    assert_eq!(fg(), ws[0].hwnd, "opening the PiP window must not take the focus");
    let pid = d.last_pid.expect("daemon pid");
    let hosts = overlay_hosts(pid);
    let host = *hosts
        .iter()
        .find(|&&h| {
            let mut r = RECT::default();
            unsafe { GetWindowRect(h as _, &mut r) };
            r.left <= at.left && r.top <= at.top && r.right >= at.right && r.bottom >= at.bottom
        })
        .unwrap_or_else(|| panic!("no overlay host of the test daemon covers {:?} (hosts {hosts:?})", (at.left, at.top, at.right, at.bottom)));
    unsafe { SetWindowPos(pip.hwnd as _, host as _, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
    assert!(z_below(pip.hwnd, host), "setup: the PiP window must sit below the overlay host in the z-order");
    assert!(pip.is_topmost(), "setup: the PiP window is still topmost");
    // Holes are rebuilt on the daemon tick (200 ms).
    thread::sleep(Duration::from_millis(600));

    let pip_area = (pw * ph) as f64;
    let frames = Cell::new(0usize);
    let worst_border = Cell::new(0usize);
    let worst_cover = Cell::new(1.0f64);
    let sample = || {
        let r = pip.rect();
        if let Some((pip_px, _)) = count_in(&cap, r, |px| px == PIP_RGB) {
            frames.set(frames.get() + 1);
            worst_cover.set(worst_cover.get().min(pip_px as f64 / pip_area));
        }
        if let Some((border, _)) = count_in(&cap, r, |px| px == FOCUS_RGB) {
            frames.set(frames.get() + 1);
            worst_border.set(worst_border.get().max(border));
        }
    };
    // Settled, with the border running under the PiP window. Repaint so frames keep arriving.
    for _ in 0..8 {
        pip.repaint();
        sample();
        thread::sleep(Duration::from_millis(40));
    }
    assert!(frames.get() >= 4, "no captured frame while settled ({}); cannot judge the PiP window", frames.get());
    assert_eq!(
        worst_border.get(),
        0,
        "settled: the focus border painted over the PiP window ({} px)",
        worst_border.get()
    );

    let check_untouched = |when: &str| {
        let st = daemon_state(&d);
        assert!(tile_of(&st, pip.hwnd).is_none(), "{when}: the PiP window must never be tiled");
        assert!(
            !st["parked"].as_array().unwrap().iter().any(|p| p["id"].as_i64() == Some(pip.hwnd as i64)),
            "{when}: the PiP window must never be parked"
        );
        assert!(pip.is_topmost(), "{when}: the PiP window must stay topmost");
        let r = pip.rect();
        assert_eq!(
            (r.left, r.top, r.right, r.bottom),
            (pip_rect0.left, pip_rect0.top, pip_rect0.right, pip_rect0.bottom),
            "{when}: the PiP window must keep its rect"
        );
    };

    // Focus walk 0 -> 2 -> 0, sampling every frame of every animation.
    let mut visited = vec![fg()];
    for (i, &step) in [RIGHT, RIGHT, LEFT, LEFT].iter().enumerate() {
        send_chord_raw(&[ALT], step);
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            sample();
        }
        assert!(wait_settled(&d, 3, Duration::from_secs(4)), "step {i}: layout did not settle");
        visited.push(fg());
        check_untouched(&format!("focus step {i}"));
    }
    let expect: Vec<isize> = [0, 1, 2, 1, 0].iter().map(|&i| ws[i].hwnd).collect();
    assert_eq!(visited, expect, "the focus walk must work with the PiP window on top");

    // Live three-finger scroll under the PiP window (the overlay draws the moving windows' pictures).
    let off0 = strip_offset(&daemon_state(&d), ws[0].hwnd).unwrap();
    let play = replay_scroll(&d, 400.0);
    let until = Instant::now() + play + Duration::from_millis(300);
    let mut moved = 0i64;
    let mut polled = Instant::now();
    while Instant::now() < until {
        sample();
        if polled.elapsed() > Duration::from_millis(60) {
            polled = Instant::now();
            if let Some(o) = strip_offset(&daemon_state(&d), ws[0].hwnd) {
                moved = moved.max((o - off0).abs());
            }
        }
    }
    assert!(wait_settled(&d, 3, Duration::from_secs(4)), "layout did not settle after the live scroll");
    assert!(
        moved >= 300,
        "the replayed three-finger swipe must scroll the strip under the PiP window (max offset change {moved} px)"
    );
    check_untouched("after the live scroll");

    assert!(frames.get() >= 20, "too few captured frames during the scrolls ({})", frames.get());
    assert_eq!(
        worst_border.get(),
        0,
        "the focus border painted over the PiP window while scrolling ({} px)",
        worst_border.get()
    );
    assert!(
        worst_cover.get() >= 0.98,
        "something painted over the PiP window while scrolling: as little as {:.1}% of it showed its colour",
        worst_cover.get() * 100.
    );
    d.stop();
}
