use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation as F,
        Graphics::{Direct3D::*, Direct3D11::*, Dxgi::*},
    },
    core::Interface,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::LibraryLoader::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
struct Sentinel {
    hwnd: isize,
    join: Option<thread::JoinHandle<()>>,
}
unsafe extern "system" fn proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    if m == WM_PAINT || m == WM_PRINTCLIENT {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = if m == WM_PAINT {
                BeginPaint(h, &mut ps)
            } else {
                w as HDC
            };
            let mut rect = RECT::default();
            GetClientRect(h, &mut rect);
            FillRect(dc, &rect, GetClassLongPtrW(h, GCLP_HBRBACKGROUND) as HBRUSH);
            if m == WM_PAINT {
                EndPaint(h, &ps);
            }
            return 0;
        }
    }
    if m == WM_WINDOWPOSCHANGING
        && unsafe { GetPropW(h, wide("Planor.Test.Minimum").as_ptr()) } as usize != 0
    {
        let pos = unsafe { &mut *(l as *mut WINDOWPOS) };
        if pos.flags & SWP_NOSIZE == 0 && pos.cx < 1300 {
            pos.cx = 1300;
        }
    }
    if m == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}
impl Sentinel {
    fn new(index: usize, owner: isize) -> Self {
        Self::at(index, owner, 50 + index as i32 * 60, 70)
    }
    /// Create a sentinel with its top-left at physical virtual-desktop coordinates (x, y).
    fn at(index: usize, owner: isize, x: i32, y: i32) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let name = wide(&format!("PlanorSentinel{index}"));
            let inst = GetModuleHandleW(null());
            let brush = CreateSolidBrush([0xff00ff, 0xffff00, 0x00ffff][index % 3]);
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: inst,
                hbrBackground: brush,
                lpszClassName: name.as_ptr(),
                ..Default::default()
            });
            let h = CreateWindowExW(
                0,
                name.as_ptr(),
                name.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                x,
                y,
                360,
                240,
                owner as _,
                null_mut(),
                inst,
                null(),
            );
            UpdateWindow(h);
            tx.send(h as isize).unwrap();
            let mut m = MSG::default();
            while !h.is_null() && GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            UnregisterClassW(name.as_ptr(), inst);
            DeleteObject(brush);
        });
        let hwnd = rx.recv().unwrap();
        assert_ne!(hwnd, 0);
        Self {
            hwnd,
            join: Some(join),
        }
    }
    fn rect(&self) -> [i32; 4] {
        let mut r = RECT::default();
        assert_ne!(unsafe { GetWindowRect(self.hwnd as _, &mut r) }, 0);
        [r.left, r.top, r.right, r.bottom]
    }
    fn placement(&self)->[i32;10]{let mut p=WINDOWPLACEMENT{length:size_of::<WINDOWPLACEMENT>() as _,..Default::default()};assert_ne!(unsafe{GetWindowPlacement(self.hwnd as _,&mut p)},0);[p.flags as _,p.showCmd as _,p.ptMinPosition.x,p.ptMinPosition.y,p.ptMaxPosition.x,p.ptMaxPosition.y,p.rcNormalPosition.left,p.rcNormalPosition.top,p.rcNormalPosition.right,p.rcNormalPosition.bottom]}
    fn visible(&self) -> RECT {
        let mut r = RECT::default();
        assert_eq!(
            unsafe {
                DwmGetWindowAttribute(
                    self.hwnd as _,
                    DWMWA_EXTENDED_FRAME_BOUNDS as _,
                    (&mut r as *mut RECT).cast(),
                    size_of::<RECT>() as _,
                )
            },
            0
        );
        r
    }
}
impl Drop for Sentinel {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as _, WM_CLOSE, 0, 0) };
        self.join.take().unwrap().join().unwrap();
    }
}
const EXE: &str = env!("CARGO_BIN_EXE_planor");
struct Daemon {
    p: Option<Child>,
    dir: PathBuf,
    inst: String,
}
impl Daemon {
    fn new(label: &str) -> Self {
        let dir = daemon_dir(label);
        fs::create_dir_all(dir.join("planor")).unwrap();
        let config = format!(
            "{}\n[[rules]]\nprocess={:?}\naction=\"tile\"\n[[rules]]\nclass=\"*\"\naction=\"ignore\"\n",
            // With PLANOR_RECORD=1 the daemon's diagnostics trace (EXPECT lines) is switched on so the
            // recording can be checked against what Planor claims; otherwise the config is unchanged.
            config_for_recording(include_str!("../../planor.example.toml")),
            std::env::current_exe()
                .unwrap()
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap()
        );
        fs::write(dir.join("planor/planor.toml"), config).unwrap();
        Self {
            dir,
            p: None,
            inst: format!("t{}-{label}", std::process::id()),
        }
    }
    fn start(&mut self) {
        let running = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq planor.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("planor.exe"))
            .unwrap_or(false);
        assert!(
            !running,
            "a real planor.exe is running; stop it first (global hotkeys cannot be isolated)"
        );
        self.p = Some(
            Command::new(EXE)
                .env("PLANOR_INSTANCE", &self.inst)
                .arg("start")
                .env("APPDATA", &self.dir)
                .env("LOCALAPPDATA", &self.dir)
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(8);
        while !self.call("reload") {
            assert!(
                self.p.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            assert!(Instant::now() < until, "pipe startup timeout");
            thread::sleep(Duration::from_millis(80))
        }
    }
    fn call(&self, cmd: &str) -> bool {
        Command::new(EXE)
            .env("PLANOR_INSTANCE", &self.inst)
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }
    fn stop(&mut self) {
        // The analyzer expects nothing from Planor once it has stopped (its windows go back where they were).
        recording_mark_global("daemon-stopped");
        assert!(self.call("stop"));
        assert!(self.p.take().unwrap().wait().unwrap().success())
    }
    fn kill(&mut self) {
        recording_mark_global("daemon-stopped");
        let mut p = self.p.take().unwrap();
        p.kill().unwrap();
        p.wait().unwrap();
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(p) = self.p.as_mut() {
            if p.try_wait().unwrap().is_none() {
                recording_mark_global("daemon-stopped");
                let _ = self.call("stop");
                let _ = self.p.as_mut().unwrap().wait();
            }
        }
        if !self.dir.join("planor/recovery.json").exists() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
fn wait(mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(6);
    while !f() {
        assert!(Instant::now() < until, "window state timeout");
        thread::sleep(Duration::from_millis(30))
    }
}
fn focus_key(vk: u8) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        keybd_event(VK_MENU as u8, 0, 0, 0);
        keybd_event(vk, 0, 0, 0);
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        keybd_event(VK_MENU as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}
struct Capture {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    origin: (i32, i32),
    secondary: bool,
}
struct Pixels {
    mask: Vec<u8>,
    time: i64,
    edge: Option<i32>,
    border: Option<i32>,
    sentinel: bool,
}
impl Capture {
    fn open(index: u32) -> windows::core::Result<Self> {
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
            let dev = dev.unwrap();
            let dx: IDXGIDevice = dev.cast()?;
            let out: IDXGIOutput1 = dx.GetAdapter()?.EnumOutputs(index)?.cast()?;
            let r = out.GetDesc()?.DesktopCoordinates;
            Ok(Self {
                dupl: out.DuplicateOutput(&dev)?,
                dev,
                ctx: ctx.unwrap(),
                origin: (r.left, r.top),
                secondary: index != 0,
            })
        }
    }
    fn frame(&self, border: RECT) -> windows::core::Result<Pixels> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            self.dupl.AcquireNextFrame(250, &mut info, &mut res)?;
            let result = (|| {
                let tex: ID3D11Texture2D = res.unwrap().cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as _;
                desc.BindFlags = 0;
                desc.MiscFlags = 0;
                let mut copy = None;
                self.dev.CreateTexture2D(&desc, None, Some(&mut copy))?;
                let copy = copy.unwrap();
                self.ctx.CopyResource(&copy, &tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let mut p = Pixels {
                    mask: Vec::new(),
                    time: info.LastPresentTime,
                    edge: None,
                    border: None,
                    sentinel: false,
                };
                if !self.secondary && std::env::var_os("PLANOR_FILMSTRIP").is_some() {
                    for y in 0..120 {
                        let row = (mapped.pData as *const u8)
                            .add((y * desc.Height as usize / 120) * mapped.RowPitch as usize)
                            .cast::<u32>();
                        for x in 0..240 {
                            let c = *row.add(x * desc.Width as usize / 240) & 0xffffff;
                            let rgb = match c {
                                0xff00ff => [255, 0, 255],
                                0x00ffff => [0, 255, 255],
                                0xffff00 => [255, 255, 0],
                                0x3b82f6 => [59, 130, 246],
                                _ => [28, 28, 28],
                            };
                            p.mask.extend(rgb)
                        }
                    }
                }
                let y = (border.top + border.bottom) / 2 - self.origin.1;
                for row in 0..desc.Height {
                    if !self.secondary && row as i32 != y {
                        continue;
                    }
                    let line = std::slice::from_raw_parts(
                        (mapped.pData as *const u8)
                            .add(row as usize * mapped.RowPitch as usize)
                            .cast::<u32>(),
                        desc.Width as usize,
                    );
                    if row as i32 == y {
                        p.border = line
                            .iter()
                            .enumerate()
                            .filter(|(_, v)| **v & 0xffffff == 0x3b82f6)
                            .map(|(x, _)| (x as i32 + self.origin.0 - border.left).abs())
                            .min();
                        p.edge = line
                            .iter()
                            .rposition(|v| *v & 0xffffff == 0x00ffff)
                            .map(|x| x as i32)
                    }
                    if self.secondary && !p.sentinel {
                        p.sentinel = line
                            .iter()
                            .any(|px| matches!(px & 0xffffff, 0xff00ff | 0x00ffff | 0xffff00))
                    }
                }
                self.ctx.Unmap(&copy, 0);
                Ok(p)
            })();
            self.dupl.ReleaseFrame()?;
            result
        }
    }
}

// ---- Scenario helpers (shared by every live scenario file) -------------------------------------
/// Make the calling test thread per-monitor DPI aware so every rect below is physical pixels.
fn aware() {
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}
/// Press a chord: hold `mods` (VK codes, e.g. VK_MENU=0x12, VK_SHIFT=0x10, VK_CONTROL=0x11), tap `vk`.
fn press(mods: &[u8], vk: u8) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        for m in mods {
            keybd_event(*m, 0, 0, 0);
        }
        keybd_event(vk, 0, 0, 0);
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        for m in mods.iter().rev() {
            keybd_event(*m, 0, KEYEVENTF_KEYUP, 0);
        }
    }
    thread::sleep(Duration::from_millis(700)); // 180ms animation + 250ms frame verification + slack
}
const ALT: u8 = 0x12;
const SHIFT: u8 = 0x10;
const CTRL: u8 = 0x11;
const LEFT: u8 = 0x25;
const UP: u8 = 0x26;
const RIGHT: u8 = 0x27;
const DOWN: u8 = 0x28;
/// Foreground window handle.
fn fg() -> isize {
    unsafe { GetForegroundWindow() as isize }
}
/// Index of the foreground window inside `ws`, or None if some other window has focus.
fn fg_index(ws: &[Sentinel]) -> Option<usize> {
    let f = fg();
    ws.iter().position(|w| w.hwnd == f)
}
/// Physical monitor list: (monitor rect, work rect), primary first. Orientation: w>=h landscape.
fn mons() -> Vec<(RECT, RECT)> {
    aware();
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, l: isize) -> i32 {
        let mut i = MONITORINFO { cbSize: size_of::<MONITORINFO>() as _, ..Default::default() };
        unsafe { GetMonitorInfoW(m, &mut i) };
        let v = unsafe { &mut *(l as *mut Vec<(RECT, RECT, bool)>) };
        v.push((i.rcMonitor, i.rcWork, i.dwFlags & 1 != 0));
        1
    }
    let mut v: Vec<(RECT, RECT, bool)> = vec![];
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut v as *mut _ as isize) };
    v.sort_by_key(|m| !m.2);
    v.into_iter().map(|m| (m.0, m.1)).collect()
}
fn is_portrait(r: &RECT) -> bool {
    r.bottom - r.top > r.right - r.left
}
fn area(r: &RECT) -> i64 {
    ((r.right - r.left).max(0) as i64) * ((r.bottom - r.top).max(0) as i64)
}
fn overlap(a: &RECT, b: &RECT) -> i64 {
    area(&RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    })
}
/// Fraction (0..=1) of the window's visible frame that lies on any attached monitor.
fn on_screen_fraction(w: &Sentinel) -> f64 {
    let v = w.visible();
    let total = area(&v).max(1);
    let seen: i64 = mons().iter().map(|m| overlap(&v, &m.0)).sum();
    seen as f64 / total as f64
}
/// True if the window is reachable by the user: at least 90% of it is on a monitor.
fn reachable(w: &Sentinel) -> bool {
    on_screen_fraction(w) >= 0.9
}
/// Index of the monitor that contains most of the window, or None when it is off every monitor.
fn monitor_of(w: &Sentinel) -> Option<usize> {
    let v = w.visible();
    mons().iter().enumerate().max_by_key(|(_, m)| overlap(&v, &m.0)).filter(|(_, m)| overlap(&v, &m.0) > 0).map(|(i, _)| i)
}
/// Restores the extended multi-display layout when dropped (use around any DisplaySwitch test).
struct DisplayGuard;
impl DisplayGuard {
    fn internal_only() -> Self {
        let _ = Command::new("DisplaySwitch.exe").arg("/internal").status();
        Self
    }
}
impl Drop for DisplayGuard {
    fn drop(&mut self) {
        let _ = Command::new("DisplaySwitch.exe").arg("/extend").status();
        thread::sleep(Duration::from_secs(4));
    }
}
/// Poll until `f` is true or `secs` elapse; returns the final value (never panics).
fn eventually(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(50))
    }
}

/// RGB (0xRRGGBB) of the solid client colour of sentinel `index % 3` as seen in captured pixels.
/// NOTE: sentinels 0 and 3 share a colour, so pixel-based tests must use at most 3 sentinels.
const SENTINEL_RGB: [u32; 3] = [0xff00ff, 0x00ffff, 0xffff00];
/// Bounding box (left, top, right, bottom) in *output* pixels, or None if the colour is absent.
type BBox = Option<(i32, i32, i32, i32)>;
impl Capture {
    /// Grab one desktop-duplication frame and return (LastPresentTime, output width, output height,
    /// bounding box of each sentinel colour). Pure pixel math; no images are produced.
    fn boxes(&self) -> windows::core::Result<(i64, i32, i32, [BBox; 3])> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            self.dupl.AcquireNextFrame(250, &mut info, &mut res)?;
            let result = (|| {
                let tex: ID3D11Texture2D = res.unwrap().cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as _;
                desc.BindFlags = 0;
                desc.MiscFlags = 0;
                let mut copy = None;
                self.dev.CreateTexture2D(&desc, None, Some(&mut copy))?;
                let copy = copy.unwrap();
                self.ctx.CopyResource(&copy, &tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx.Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let mut out: [BBox; 3] = [None; 3];
                for y in 0..desc.Height as i32 {
                    let row = std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(y as usize * mapped.RowPitch as usize).cast::<u32>(),
                        desc.Width as usize,
                    );
                    for (x, px) in row.iter().enumerate() {
                        let c = px & 0xffffff;
                        // Match the sentinel colour or a darkened shade of it (the focused neighbour's shadow); never
                        // brighter or hue-shifted, so taskbar/app pixels cannot widen the box.
                        let near = |s: u32| (0..3).all(|i| { let (p, t) = (((c >> (8 * i)) & 0xff) as i32, ((s >> (8 * i)) & 0xff) as i32); if t == 0 { p <= 14 } else { p <= t && p * 100 >= t * 78 } });
                        if let Some(k) = SENTINEL_RGB.iter().position(|&s| near(s)) {
                            let x = x as i32;
                            out[k] = Some(match out[k] {
                                None => (x, y, x + 1, y + 1),
                                Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(x + 1), b.max(y + 1)),
                            });
                        }
                    }
                }
                self.ctx.Unmap(&copy, 0);
                Ok((info.LastPresentTime, desc.Width as i32, desc.Height as i32, out))
            })();
            let _ = self.dupl.ReleaseFrame();
            result
        }
    }
}

// ---- Optional screen recording of a scenario (PLANOR_RECORD=1) ---------------------------------------------
//
// `Recording::start(test, daemon_label)` starts `planor-rec record` as a child process for the duration of a
// scenario and, on drop, stops it and runs `planor-rec analyze`, printing the report summary. Without
// PLANOR_RECORD=1 every call is a no-op, so normal runs are unchanged. `assert_recording_clean` fails the test
// on any `fail` finding (never on `unmeasurable`). See docs/TEST_PLAN.md ("Recording a scenario").

/// The directory the test daemon uses as APPDATA/LOCALAPPDATA (and `Daemon::new` creates).
fn daemon_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("planor-live-{label}-{}", std::process::id()))
}

/// Where a test daemon started with `Daemon::new(label)` writes its diagnostics trace.
fn daemon_trace_path(label: &str) -> PathBuf {
    daemon_dir(label).join("planor").join("trace.log")
}

fn recording_enabled() -> bool {
    std::env::var("PLANOR_RECORD").as_deref() == Ok("1")
}

/// The Planor test config with diagnostics switched on when recording is requested.
fn config_for_recording(base: &str) -> String {
    if !recording_enabled() {
        return base.to_string();
    }
    let Some(section) = base.find("[diagnostics]") else {
        return format!("{base}\n[diagnostics]\nenabled = true\n");
    };
    let tail = &base[section..];
    let end = tail[1..].find("\n[").map(|i| i + 1).unwrap_or(tail.len());
    let body = tail[..end].replacen("enabled = false", "enabled = true", 1);
    format!("{}{}{}", &base[..section], body, &tail[end..])
}

/// `planor-rec.exe`: `PLANOR_REC_BIN`, else the safe copy in `tools/planor-rec-bin`, else the release build.
fn planor_rec_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANOR_REC_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    [
        root.join("tools/planor-rec-bin/planor-rec.exe"),
        root.join("target/release/planor-rec.exe"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

struct RecCheck {
    kind: String,
    verdict: String,
    monitor: i64,
    time_ms: i64,
    detail: String,
}

struct RecSummary {
    dir: PathBuf,
    checks: usize,
    fail: usize,
    unmeasurable: usize,
    pass: usize,
    failures: Vec<RecCheck>,
}

struct RecInner {
    child: Option<Child>,
    exe: PathBuf,
    dir: PathBuf,
    pipe: String,
    trace: PathBuf,
    name: String,
    summary: Option<Result<RecSummary, String>>,
    /// Solid wallpaper-coloured windows over every monitor's work area, below the test's windows (see `Backdrop`).
    backdrop: Option<Backdrop>,
}

/// Where `recording_mark_global` sends a marker: the active recording's recorder binary and pipe name.
static REC_MARK_TARGET: std::sync::Mutex<Option<(PathBuf, String)>> = std::sync::Mutex::new(None);

/// Add a marker to the active recording from anywhere (a no-op when nothing is being recorded). The daemon
/// helper uses it to mark `daemon-stopped`, after which the analyzer expects no tiles.
fn recording_mark_global(text: &str) {
    let target = REC_MARK_TARGET.lock().ok().and_then(|g| g.clone());
    if let Some((exe, pipe)) = target {
        let _ = Command::new(exe)
            .args(["mark", "--pipe", &pipe, text])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The recorder's wallpaper colour, RGB(0,0,90), as a COLORREF (0x00BBGGRR).
const BACKDROP_COLORREF: u32 = 0x005A_0000;
static BACKDROP_ALIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "system" fn backdrop_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    if m == WM_DESTROY {
        // The last backdrop to go ends the thread's message loop.
        if BACKDROP_ALIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            unsafe { PostQuitMessage(0) };
        }
        return 0;
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

/// With PLANOR_RECORD=1, one plain window per monitor covering its work area in the recorder's wallpaper colour
/// (solid RGB(0,0,90)). The real desktop has the developer's own windows (a full-screen app, a browser) behind
/// the test's tiles; the pixel analyzer takes any non-wallpaper pixel for the tile, so without this it measures
/// those windows and reports bogus 9 px mismatches and missing borders. The backdrops are created BEFORE the
/// sentinels, so the sentinels (and Planor's tiles and overlay) sit above them and every other window is hidden
/// below them. Planor ignores them: no WS_THICKFRAME (never eligible to be tiled), WS_EX_NOACTIVATE (never
/// "external"/foreground, never carved out), not topmost, shown without activation. Dropped with the recording.
struct Backdrop {
    hwnds: Vec<isize>,
    join: Option<thread::JoinHandle<()>>,
}

impl Backdrop {
    fn cover_work_areas(areas: Vec<RECT>) -> Backdrop {
        let (tx, rx) = std::sync::mpsc::channel();
        BACKDROP_ALIVE.store(areas.len(), std::sync::atomic::Ordering::SeqCst);
        let join = thread::spawn(move || unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let name = wide("ExternalTestBackdrop");
            let inst = GetModuleHandleW(null());
            let brush = CreateSolidBrush(BACKDROP_COLORREF);
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(backdrop_proc),
                hInstance: inst,
                hbrBackground: brush,
                lpszClassName: name.as_ptr(),
                ..Default::default()
            });
            let mut hwnds = vec![];
            for r in &areas {
                let h = CreateWindowExW(
                    WS_EX_NOACTIVATE,
                    name.as_ptr(),
                    name.as_ptr(),
                    WS_POPUP,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    null_mut(),
                    null_mut(),
                    inst,
                    null(),
                );
                if !h.is_null() {
                    ShowWindow(h, SW_SHOWNOACTIVATE);
                    UpdateWindow(h);
                    hwnds.push(h as isize);
                } else {
                    BACKDROP_ALIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            tx.send(hwnds).unwrap();
            let mut m = MSG::default();
            while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            UnregisterClassW(name.as_ptr(), inst);
            DeleteObject(brush);
        });
        Backdrop { hwnds: rx.recv().unwrap(), join: Some(join) }
    }
}

impl Drop for Backdrop {
    fn drop(&mut self) {
        for h in &self.hwnds {
            unsafe { PostMessageW(*h as _, WM_CLOSE, 0, 0) };
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// RAII recording of one scenario. Inert unless PLANOR_RECORD=1.
struct Recording {
    inner: std::cell::RefCell<Option<RecInner>>,
}

impl Recording {
    /// Start recording all monitors. `daemon_label` is the label the scenario passes to `Daemon::new`
    /// (its trace file is what the analyzer compares the pixels with). The recording folder is
    /// `<temp>/planor-rec-<test>-<pid>` and is kept.
    fn start(test: &str, daemon_label: &str) -> Recording {
        if !recording_enabled() {
            return Recording { inner: std::cell::RefCell::new(None) };
        }
        let exe = planor_rec_exe().unwrap_or_else(|| {
            panic!(
                "PLANOR_RECORD=1 but planor-rec.exe was not found; build it (`cargo build --release -p planor-rec`) \
                 or set PLANOR_REC_BIN"
            )
        });
        let safe: String = test.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let dir = std::env::temp_dir().join(format!("planor-rec-{safe}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pipe = format!("t{}-{safe}", std::process::id());
        let trace = daemon_trace_path(daemon_label);
        let log = fs::File::create(dir.join("recorder.log")).unwrap();
        let child = Command::new(&exe)
            .args(["record", "--out"])
            .arg(&dir)
            .args(["--pipe", &pipe, "--no-hotkeys", "--mark", &safe, "--max-minutes", "10", "--trace"])
            .arg(&trace)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start planor-rec");
        // The pipe answers once the recorder is up; the monitors start capturing about a second later.
        let up = eventually(20, || {
            Command::new(&exe)
                .args(["mark", "--pipe", &pipe, "scenario-start"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
        assert!(up, "planor-rec did not come up within 20 s; see {}", dir.join("recorder.log").display());
        thread::sleep(Duration::from_millis(1500));
        // Hide every other window of the real desktop below solid wallpaper-coloured backdrops. This must come
        // before the sentinels exist (the callers create the guard first), so the sentinels sit above.
        let backdrop = Backdrop::cover_work_areas(mons().into_iter().map(|(_, work)| work).collect());
        thread::sleep(Duration::from_millis(400)); // let the backdrops paint and the recorder see them
        if let Ok(mut g) = REC_MARK_TARGET.lock() {
            *g = Some((exe.clone(), pipe.clone()));
        }
        println!("[recording] {safe}: recording to {} (wallpaper-coloured backdrop over every monitor)", dir.display());
        Recording {
            inner: std::cell::RefCell::new(Some(RecInner {
                child: Some(child),
                exe,
                dir,
                pipe,
                trace,
                name: safe,
                summary: None,
                backdrop: Some(backdrop),
            })),
        }
    }

    fn enabled(&self) -> bool {
        self.inner.borrow().is_some()
    }

    /// Add a marker (a step name) to the recording's timeline. Best effort.
    fn mark(&self, text: &str) {
        if let Some(r) = self.inner.borrow().as_ref() {
            if r.child.is_some() {
                let text: String = text.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
                let _ = Command::new(&r.exe)
                    .args(["mark", "--pipe", &r.pipe, &text])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    /// Stop the recorder, analyze the recording, print the summary. Runs once; later calls return the
    /// same result. `None` when recording is not enabled.
    fn finish(&self) -> Option<Result<RecSummary, String>> {
        let mut guard = self.inner.borrow_mut();
        let r = guard.as_mut()?;
        if r.summary.is_none() {
            r.summary = Some(Self::stop_and_analyze(r));
            match r.summary.as_ref().unwrap() {
                Ok(s) => {
                    println!(
                        "[recording] {}: report {} checks ({} fail, {} unmeasurable, {} pass); {}",
                        r.name,
                        s.checks,
                        s.fail,
                        s.unmeasurable,
                        s.pass,
                        s.dir.join("report.md").display()
                    );
                    for c in &s.failures {
                        println!(
                            "[recording]   FAIL {} monitor {} at {} ms: {}",
                            c.kind, c.monitor, c.time_ms, c.detail
                        );
                    }
                }
                Err(e) => println!("[recording] {}: analysis not available: {e}", r.name),
            }
        }
        r.summary.as_ref().map(|s| match s {
            Ok(s) => Ok(RecSummary {
                dir: s.dir.clone(),
                checks: s.checks,
                fail: s.fail,
                unmeasurable: s.unmeasurable,
                pass: s.pass,
                failures: s
                    .failures
                    .iter()
                    .map(|c| RecCheck {
                        kind: c.kind.clone(),
                        verdict: c.verdict.clone(),
                        monitor: c.monitor,
                        time_ms: c.time_ms,
                        detail: c.detail.clone(),
                    })
                    .collect(),
            }),
            Err(e) => Err(e.clone()),
        })
    }

    fn stop_and_analyze(r: &mut RecInner) -> Result<RecSummary, String> {
        if let Ok(mut g) = REC_MARK_TARGET.lock() {
            *g = None;
        }
        if let Some(mut child) = r.child.take() {
            let _ = Command::new(&r.exe)
                .args(["stop", "--pipe", &r.pipe])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let until = Instant::now() + Duration::from_secs(60);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(100)),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        // The recorder has stopped: the backdrops stayed up to the last frame; now they can go.
        r.backdrop = None;
        let out = Command::new(&r.exe)
            .arg("analyze")
            .arg(&r.dir)
            .arg("--trace")
            .arg(&r.trace)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("could not run planor-rec analyze: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "planor-rec analyze failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = fs::read_to_string(r.dir.join("report.json")).map_err(|e| format!("report.json: {e}"))?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("report.json: {e}"))?;
        let s = &v["summary"];
        let count = |k: &str| s[k].as_u64().unwrap_or(0) as usize;
        let failures = v["checks"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|c| c["verdict"] == "fail")
                    .map(|c| RecCheck {
                        kind: c["kind"].as_str().unwrap_or("?").to_string(),
                        verdict: "fail".into(),
                        monitor: c["monitor"].as_i64().unwrap_or(-1),
                        time_ms: c["time_ms"].as_i64().unwrap_or(0),
                        detail: c["detail"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(RecSummary {
            dir: r.dir.clone(),
            checks: count("checks"),
            fail: count("fail"),
            unmeasurable: count("unmeasurable"),
            pass: count("pass"),
            failures,
        })
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Never panic here (a test may already be unwinding); just stop, analyze and print.
        let _ = self.finish();
    }
}

/// Fail the test if the recording has any `fail` finding. `unmeasurable` findings never fail it. A no-op
/// when recording is not enabled. Stops and analyzes the recording at this point.
fn assert_recording_clean(rec: &Recording) {
    if !rec.enabled() {
        return;
    }
    match rec.finish().expect("recording enabled") {
        Err(e) => panic!("recording analysis failed: {e}"),
        Ok(s) => {
            if s.fail > 0 {
                let lines: Vec<String> = s
                    .failures
                    .iter()
                    .map(|c| format!("  {} monitor {} at {} ms: {}", c.kind, c.monitor, c.time_ms, c.detail))
                    .collect();
                panic!(
                    "the recording has {} failing finding(s) (report: {}):\n{}",
                    s.fail,
                    s.dir.join("report.md").display(),
                    lines.join("\n")
                );
            }
        }
    }
}