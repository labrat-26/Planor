#![allow(dead_code, unused_imports)]
include!("common/helpers.rs");

// Regression for the user's notes "border was above the task manager" and "focus border does not go onto
// the top window": an unmanaged window (an elevated Task Manager, a rule-ignored app) must never be painted
// over by Planor's border, whatever the z-order does, and the border must come back when it goes away.

/// Planor focus border colour (planor.example.toml [borders].focused_color = #3B82F6), as 0x00RRGGBB.
const FOCUS_RGB: u32 = 0x003b82f6;
/// A colour nothing else on the desktop uses, for the unmanaged cover window.
const COVER_RGB: u32 = 0x00123456;

/// (pixels equal to `a`, pixels equal to `b`) in one captured frame of the primary output; None when no new
/// frame arrived (desktop duplication delivers nothing while the screen does not change).
fn count_two(cap: &Capture, a: u32, b: u32) -> Option<(usize, usize)> {
    unsafe {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res = None;
        if cap.dupl.AcquireNextFrame(250, &mut info, &mut res).is_err() {
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
            let (mut na, mut nb) = (0usize, 0usize);
            for y in 0..desc.Height as usize {
                let row = std::slice::from_raw_parts(
                    (mapped.pData as *const u8)
                        .add(y * mapped.RowPitch as usize)
                        .cast::<u32>(),
                    desc.Width as usize,
                );
                na += row.iter().filter(|px| **px & 0xffffff == a).count();
                nb += row.iter().filter(|px| **px & 0xffffff == b).count();
            }
            cap.ctx.Unmap(&copy, 0);
            Ok((na, nb))
        })();
        let _ = cap.dupl.ReleaseFrame();
        result.ok()
    }
}

/// A normal (not topmost), non-tiled window covering the whole primary monitor, in COVER_RGB.
/// Dropping it closes the window.
struct Cover {
    hwnd: isize,
    join: Option<thread::JoinHandle<()>>,
}
impl Cover {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let name = wide("ExternalTestCover");
            let inst = GetModuleHandleW(null());
            let brush = CreateSolidBrush(0x0056_3412); // COLORREF is 0x00BBGGRR
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: inst,
                hbrBackground: brush,
                lpszClassName: name.as_ptr(),
                ..Default::default()
            });
            let (w, h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let hwnd = CreateWindowExW(
                0, // not topmost, like Task Manager: the topmost overlay is above it unless Planor cuts it out
                name.as_ptr(),
                name.as_ptr(),
                WS_POPUP,
                0,
                0,
                w,
                h,
                null_mut(),
                null_mut(),
                inst,
                null(),
            );
            // Shown without activation: focus stays on a managed window, so the only thing that can
            // keep the border off the screen is the cut-out under test, not the "unmanaged window has
            // focus" rule.
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            tx.send(hwnd as isize).unwrap();
            let mut m = MSG::default();
            while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            UnregisterClassW(name.as_ptr(), inst);
            DeleteObject(brush);
        });
        Self {
            hwnd: rx.recv().unwrap(),
            join: Some(join),
        }
    }
}
impl Drop for Cover {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as _, WM_CLOSE, 0, 0) };
        unsafe { PostMessageW(self.hwnd as _, WM_QUIT, 0, 0) };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[test]
#[ignore]
fn border_never_paints_over_an_unmanaged_window() {
    aware();
    let _ws: Vec<Sentinel> = (0..2).map(|i| Sentinel::new(i, 0)).collect();
    let cap = Capture::open(0).expect("desktop duplication");
    let mut d = Daemon::new("overlay-holes");
    // SAFETY: single-threaded test setup; the daemon child inherits the variable.
    unsafe { std::env::set_var("PLANOR_HOLES", "ExternalTestCover") };
    d.start();
    thread::sleep(Duration::from_millis(1500));
    // Control: with nothing covering it the focus border is on screen, so a zero below means something.
    assert!(
        eventually(4, || count_two(&cap, FOCUS_RGB, COVER_RGB).is_some_and(|c| c.0 > 0)),
        "control: expected the focus border colour #3B82F6 on screen before any cover window exists"
    );
    let cover = Cover::new();
    // Frames must show the cover (so frames are arriving) and, in the same frame, no focus border pixels.
    let mut checked = 0;
    let until = Instant::now() + Duration::from_secs(6);
    let mut worst = 0;
    while Instant::now() < until && checked < 8 {
        if let Some((border, covered)) = count_two(&cap, FOCUS_RGB, COVER_RGB) {
            if covered > 100_000 && checked == 0 && border > 0 {
                // The overlay is rebuilt on the daemon tick (200 ms); give it time before judging.
                thread::sleep(Duration::from_millis(600));
                continue;
            }
            if covered > 100_000 {
                checked += 1;
                worst = worst.max(border);
            }
        }
    }
    assert!(checked >= 3, "no captured frame showed the cover window; cannot judge the border");
    assert_eq!(
        worst, 0,
        "an unmanaged always-on-top window covers the screen: expected 0 focus-border pixels over it, observed up to {worst}"
    );
    // The border must return once the unmanaged window is gone.
    drop(cover);
    assert!(
        eventually(6, || count_two(&cap, FOCUS_RGB, COVER_RGB).is_some_and(|c| c.0 > 0 && c.1 == 0)),
        "after the unmanaged window closed the focus border did not come back"
    );
    d.stop();
}
