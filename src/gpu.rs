//! GPU profiling.
//!
//! - `GpuTimer`: D3D11 timestamp queries around each overlay draw (render thread). Results are read back
//!   one or more frames later without flushing or waiting, and recorded as the `gpu.draw` span (GPU
//!   execution time of the borders, zones and stand-in bitmaps).
//! - `Sampler` (watchdog thread, with the CPU/RAM counters): this process's GPU memory (DXGI
//!   `QueryVideoMemoryInfo`), its GPU engine utilisation (PDH `GPU Engine` counters, busiest engine type,
//!   like Task Manager) and DWM composition timing (refresh rate, composed frames per second, late,
//!   dropped and missed frames). A compose rate well below the refresh rate while a strip scrolls is the
//!   number behind "scroll is not smooth".
use crate::perf;
use std::collections::{HashMap, VecDeque};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::*};

struct Pending {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
    start_qpc_us: i64,
}

/// Timestamp queries for one D3D11 device. At most a few frames in flight; older ones are dropped.
#[derive(Default)]
pub struct GpuTimer {
    pending: VecDeque<Pending>,
    spare: Vec<(ID3D11Query, ID3D11Query, ID3D11Query)>,
    broken: bool,
}

pub struct Mark(Option<(ID3D11Query, ID3D11Query, ID3D11Query, i64)>);

impl GpuTimer {
    fn queries(&mut self, dev: &ID3D11Device) -> Option<(ID3D11Query, ID3D11Query, ID3D11Query)> {
        if let Some(q) = self.spare.pop() {
            return Some(q);
        }
        let make = |kind| unsafe {
            let mut q = None;
            dev.CreateQuery(&D3D11_QUERY_DESC { Query: kind, MiscFlags: 0 }, Some(&mut q)).ok()?;
            q
        };
        Some((make(D3D11_QUERY_TIMESTAMP_DISJOINT)?, make(D3D11_QUERY_TIMESTAMP)?, make(D3D11_QUERY_TIMESTAMP)?))
    }
    /// Before the draw.
    pub fn begin(&mut self, dev: &ID3D11Device) -> Mark {
        if self.broken || !perf::is_enabled() || self.pending.len() >= 6 {
            return Mark(None);
        }
        let Ok(ctx) = (unsafe { dev.GetImmediateContext() }) else { return Mark(None) };
        let Some((d, b, e)) = self.queries(dev) else {
            self.broken = true;
            return Mark(None);
        };
        unsafe {
            ctx.Begin(&d);
            ctx.End(&b);
        }
        Mark(Some((d, b, e, perf::qpc_us())))
    }
    /// After the draw was submitted (after `EndDraw`).
    pub fn end(&mut self, dev: &ID3D11Device, mark: Mark) {
        let Some((disjoint, begin, end, start_qpc_us)) = mark.0 else { return };
        let Ok(ctx) = (unsafe { dev.GetImmediateContext() }) else { return };
        unsafe {
            ctx.End(&end);
            ctx.End(&disjoint);
        }
        self.pending.push_back(Pending { disjoint, begin, end, start_qpc_us });
        self.collect(&ctx);
    }
    /// Read back every finished frame (never waits); records `gpu.draw` spans.
    fn collect(&mut self, ctx: &ID3D11DeviceContext) {
        while let Some(p) = self.pending.front() {
            let flags = D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32;
            let mut dj = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT { Frequency: 0, Disjoint: true.into() };
            let ok = unsafe {
                ctx.GetData(&p.disjoint, Some((&mut dj as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT).cast()), size_of_val(&dj) as u32, flags)
            };
            // S_FALSE (not ready yet) is a success code and leaves the data untouched.
            if ok.is_err() || dj.Frequency == 0 {
                if ok.is_err() || self.pending.len() >= 6 {
                    let p = self.pending.pop_front().unwrap();
                    self.spare.push((p.disjoint, p.begin, p.end));
                    continue;
                }
                break;
            }
            let p = self.pending.pop_front().unwrap();
            let (mut t0, mut t1) = (u64::MAX, u64::MAX);
            unsafe {
                let _ = ctx.GetData(&p.begin, Some((&mut t0 as *mut u64).cast()), 8, flags);
                let _ = ctx.GetData(&p.end, Some((&mut t1 as *mut u64).cast()), 8, flags);
            }
            if !dj.Disjoint.as_bool() && t0 != u64::MAX && t1 != u64::MAX && t1 >= t0 {
                let us = (t1 - t0) as f64 * 1_000_000. / dj.Frequency as f64;
                perf::record_span(perf::SpanKind::GpuDraw, p.start_qpc_us, us.round() as u64, 0);
            }
            self.spare.push((p.disjoint, p.begin, p.end));
        }
    }
}

/// One GPU sample for the counters ring.
#[derive(Clone, Copy, Default, Debug, serde::Serialize)]
pub struct GpuSample {
    /// Busiest engine type of this process (3D, Copy, VideoDecode ...), percent. `None`: PDH unavailable.
    pub engine_pct: Option<f64>,
    /// Dedicated (local) video memory this process uses, MB.
    pub local_mb: Option<f64>,
    /// Shared (non-local) video memory this process uses, MB.
    pub shared_mb: Option<f64>,
    pub dwm_refresh_hz: Option<f64>,
    /// Frames DWM composed per second since the previous sample.
    pub dwm_compose_fps: Option<f64>,
    pub dwm_frames_late: u64,
    pub dwm_frames_dropped: u64,
    pub dwm_frames_missed: u64,
}

/// Lives on the sampling thread.
#[derive(Default)]
pub struct Sampler {
    adapter: Option<IDXGIAdapter3>,
    adapter_tried: bool,
    pdh: Option<(*mut core::ffi::c_void, *mut core::ffi::c_void)>,
    pdh_tried: bool,
    last_dwm: Option<(i64, u64, u64, u64, u64)>,
    tick: u32,
    last: GpuSample,
}

impl Sampler {
    /// Called every counter sample (about 250 ms). PDH and DXGI are read once a second; DWM every time.
    pub fn sample(&mut self) -> GpuSample {
        let slow = self.tick.is_multiple_of(4);
        self.tick = self.tick.wrapping_add(1);
        let mut s = self.last;
        if slow {
            s.engine_pct = self.engine_pct();
            (s.local_mb, s.shared_mb) = self.memory();
        }
        self.dwm(&mut s);
        self.last = s;
        s
    }

    fn memory(&mut self) -> (Option<f64>, Option<f64>) {
        if !self.adapter_tried {
            self.adapter_tried = true;
            self.adapter = unsafe {
                CreateDXGIFactory1::<IDXGIFactory1>()
                    .and_then(|f| f.EnumAdapters1(0))
                    .and_then(|a| windows::core::Interface::cast::<IDXGIAdapter3>(&a))
                    .ok()
            };
        }
        let Some(a) = &self.adapter else { return (None, None) };
        let read = |group| unsafe {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            a.QueryVideoMemoryInfo(0, group, &mut info).ok().map(|_| info.CurrentUsage as f64 / (1024. * 1024.))
        };
        (read(DXGI_MEMORY_SEGMENT_GROUP_LOCAL), read(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL))
    }

    fn engine_pct(&mut self) -> Option<f64> {
        use windows_sys::Win32::System::Performance::*;
        unsafe {
            if !self.pdh_tried {
                self.pdh_tried = true;
                let mut q = std::ptr::null_mut();
                if PdhOpenQueryW(std::ptr::null(), 0, &mut q) != 0 {
                    return None;
                }
                let path: Vec<u16> = format!("\\GPU Engine(pid_{}_*)\\Utilization Percentage", perf::current_pid())
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let mut c = std::ptr::null_mut();
                if PdhAddEnglishCounterW(q, path.as_ptr(), 0, &mut c) != 0 {
                    PdhCloseQuery(q);
                    return None;
                }
                PdhCollectQueryData(q); // rate counter: the first value comes from the next collect
                self.pdh = Some((q, c));
                return None;
            }
            let (q, c) = self.pdh?;
            if PdhCollectQueryData(q) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let r = PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut());
            if r != PDH_MORE_DATA || size == 0 {
                return Some(0.); // no GPU engine instance for this process yet: idle
            }
            let mut buf = vec![0u8; size as usize + 16];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut count, items) != 0 {
                return None;
            }
            // Instance names end in "_engtype_<type>"; Task Manager shows the busiest type, summing the
            // engines of that type.
            let mut by_type: HashMap<String, f64> = HashMap::new();
            for i in 0..count as usize {
                let it = &*items.add(i);
                if it.FmtValue.CStatus != 0 {
                    continue;
                }
                let name = wide_to_string(it.szName);
                let kind = name.rsplit("engtype_").next().unwrap_or("").to_string();
                *by_type.entry(kind).or_default() += it.FmtValue.Anonymous.doubleValue;
            }
            Some(by_type.values().cloned().fold(0., f64::max).min(100.))
        }
    }

    fn dwm(&mut self, s: &mut GpuSample) {
        use windows_sys::Win32::Graphics::Dwm::*;
        let mut t: DWM_TIMING_INFO = unsafe { std::mem::zeroed() };
        t.cbSize = size_of::<DWM_TIMING_INFO>() as u32;
        if unsafe { DwmGetCompositionTimingInfo(std::ptr::null_mut(), &mut t) } < 0 {
            return;
        }
        let mut hz_from_qpc = None;
        if t.qpcRefreshPeriod > 0 {
            let freq = perf::qpc_frequency().max(1) as f64;
            let us = (t.qpcRefreshPeriod as f64 * 1_000_000.0) / freq;
            if us > 0.0 {
                hz_from_qpc = planor::pacing::clamp_hz(1_000_000.0 / us);
            }
        }
        if let Some(hz) = hz_from_qpc {
            s.dwm_refresh_hz = Some(hz);
        } else if t.rateRefresh.uiDenominator != 0 {
            let hz = t.rateRefresh.uiNumerator as f64 / t.rateRefresh.uiDenominator as f64;
            if let Some(hz) = planor::pacing::clamp_hz(hz) {
                s.dwm_refresh_hz = Some(hz);
            }
        }
        let now = perf::qpc_us();
        if let Some((at, frame, late, dropped, missed)) = self.last_dwm {
            let dt = (now - at).max(1) as f64 / 1e6;
            s.dwm_compose_fps = Some(t.cFrame.saturating_sub(frame) as f64 / dt);
            s.dwm_frames_late = t.cFramesLate.saturating_sub(late);
            s.dwm_frames_dropped = t.cFramesDropped.saturating_sub(dropped);
            s.dwm_frames_missed = t.cFramesMissed.saturating_sub(missed);
        }
        self.last_dwm = Some((now, t.cFrame, t.cFramesLate, t.cFramesDropped, t.cFramesMissed));
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        if let Some((q, _)) = self.pdh.take() {
            unsafe { windows_sys::Win32::System::Performance::PdhCloseQuery(q) };
        }
    }
}

unsafe fn wide_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

static LATEST: std::sync::Mutex<Option<GpuSample>> = std::sync::Mutex::new(None);
static STARTED: std::sync::Once = std::sync::Once::new();

/// Latest GPU sample. The first call starts the `Planor.Gpu` sampling thread (one sample every 250 ms,
/// PDH and DXGI once a second), so a process that never asks pays nothing.
pub fn latest() -> GpuSample {
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("Planor.Gpu".into()).spawn(|| {
            let mut sampler = Sampler::default();
            loop {
                let s = sampler.sample();
                if let Ok(mut l) = LATEST.lock() {
                    *l = Some(s);
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });
    });
    LATEST.lock().ok().and_then(|l| *l).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sampler_reads_dwm_and_gpu_counters_without_panicking() {
        let mut s = Sampler::default();
        let first = s.sample();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let second = s.sample();
        // On a desktop session DWM always reports its refresh rate; values must be sane when present.
        if let Some(hz) = second.dwm_refresh_hz.or(first.dwm_refresh_hz) {
            assert!((10. ..=1000.).contains(&hz), "refresh rate {hz}");
        }
        if let Some(fps) = second.dwm_compose_fps {
            assert!((0. ..=1000.).contains(&fps), "compose fps {fps}");
        }
        for p in [first.engine_pct, second.engine_pct].into_iter().flatten() {
            assert!((0. ..=100.).contains(&p), "engine {p}%");
        }
        for mb in [second.local_mb, second.shared_mb].into_iter().flatten() {
            assert!((0. ..1e6).contains(&mb), "video memory {mb} MB");
        }
    }
}
