//! Low-overhead QPC profiling spans, per-thread/process OS telemetry, and Perfetto/Chrome Trace export.
//!
//! Spans use the exact same Windows `QueryPerformanceCounter` microsecond clock as `tools/planor-rec`,
//! allowing visual frame gaps (`report.json`) to be joined directly to Planor internal spans.

use crate::diag;
use crossbeam_queue::ArrayQueue;
use serde::Serialize;
use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering},
    },
};

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}
impl FileTime {
    fn as_ms(&self) -> f64 {
        let ticks = ((self.high as u64) << 32) | (self.low as u64);
        ticks as f64 / 10_000.0
    }
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
const GR_GDIOBJECTS: u32 = 0;
const GR_USEROBJECTS: u32 = 1;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(lpPerformanceCount: *mut i64) -> i32;
    fn QueryPerformanceFrequency(lpFrequency: *mut i64) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetCurrentThread() -> *mut c_void;
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn DuplicateHandle(
        hSourceProcessHandle: *mut c_void,
        hSourceHandle: *mut c_void,
        hTargetProcessHandle: *mut c_void,
        lpTargetHandle: *mut *mut c_void,
        dwDesiredAccess: u32,
        bInheritHandle: i32,
        dwOptions: u32,
    ) -> i32;
    fn GetProcessTimes(
        hProcess: *mut c_void,
        lpCreationTime: *mut FileTime,
        lpExitTime: *mut FileTime,
        lpKernelTime: *mut FileTime,
        lpUserTime: *mut FileTime,
    ) -> i32;
    fn GetThreadTimes(
        hThread: *mut c_void,
        lpCreationTime: *mut FileTime,
        lpExitTime: *mut FileTime,
        lpKernelTime: *mut FileTime,
        lpUserTime: *mut FileTime,
    ) -> i32;
    fn GetProcessHandleCount(hProcess: *mut c_void, pdwHandleCount: *mut u32) -> i32;
    fn K32GetProcessMemoryInfo(
        Process: *mut c_void,
        ppsmemCounters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetGuiResources(hProcess: *mut c_void, uiFlags: u32) -> u32;
}

static QPC_FREQ: OnceLock<i64> = OnceLock::new();

pub fn qpc_frequency() -> i64 {
    *QPC_FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) };
        if f <= 0 { 10_000_000 } else { f }
    })
}

#[inline]
pub fn qpc_ticks() -> i64 {
    let mut c = 0i64;
    unsafe { QueryPerformanceCounter(&mut c) };
    c
}

#[inline]
pub fn qpc_us() -> i64 {
    let f = qpc_frequency();
    ((qpc_ticks() as i128 * 1_000_000i128) / f as i128) as i64
}

pub fn current_tid() -> u32 {
    unsafe { GetCurrentThreadId() }
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[repr(u8)]
pub enum SpanKind {
    Idle = 0,
    DaemonTick = 1,
    DaemonLayout = 2,
    DaemonEvent = 3,
    DaemonHoles = 4,
    DaemonScan = 5,
    DaemonIpc = 6,
    DaemonJournal = 7,
    DaemonTopology = 8,
    RenderScene = 9,
    RenderHosts = 10,
    RenderSources = 11,
    RenderFrames = 12,
    RenderDraw = 13,
    RenderDwmFlush = 14,
    RenderVerify = 15,
    RenderRgnHoles = 16,
    Win32Place = 17,
    HookMouse = 18,
    HookKeyboard = 19,
    InputContacts = 20,
    GestureToPlace = 21,
    StartupFirstPlace = 22,
}

impl SpanKind {
    pub const ALL: [SpanKind; 22] = [
        SpanKind::DaemonTick,
        SpanKind::DaemonLayout,
        SpanKind::DaemonEvent,
        SpanKind::DaemonHoles,
        SpanKind::DaemonScan,
        SpanKind::DaemonIpc,
        SpanKind::DaemonJournal,
        SpanKind::DaemonTopology,
        SpanKind::RenderScene,
        SpanKind::RenderHosts,
        SpanKind::RenderSources,
        SpanKind::RenderFrames,
        SpanKind::RenderDraw,
        SpanKind::RenderDwmFlush,
        SpanKind::RenderVerify,
        SpanKind::RenderRgnHoles,
        SpanKind::Win32Place,
        SpanKind::HookMouse,
        SpanKind::HookKeyboard,
        SpanKind::InputContacts,
        SpanKind::GestureToPlace,
        SpanKind::StartupFirstPlace,
    ];

    pub fn from_u8(v: u8) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|&k| k as u8 == v)
            .unwrap_or(SpanKind::Idle)
    }

    pub fn name(self) -> &'static str {
        match self {
            SpanKind::Idle => "idle",
            SpanKind::DaemonTick => "daemon.tick",
            SpanKind::DaemonLayout => "daemon.layout",
            SpanKind::DaemonEvent => "daemon.event",
            SpanKind::DaemonHoles => "daemon.holes",
            SpanKind::DaemonScan => "daemon.scan",
            SpanKind::DaemonIpc => "daemon.ipc",
            SpanKind::DaemonJournal => "daemon.journal",
            SpanKind::DaemonTopology => "daemon.topology",
            SpanKind::RenderScene => "render.scene",
            SpanKind::RenderHosts => "render.hosts",
            SpanKind::RenderSources => "render.sources",
            SpanKind::RenderFrames => "render.frames",
            SpanKind::RenderDraw => "render.draw",
            SpanKind::RenderDwmFlush => "render.dwm_flush",
            SpanKind::RenderVerify => "render.verify",
            SpanKind::RenderRgnHoles => "render.rgn_holes",
            SpanKind::Win32Place => "win32.place",
            SpanKind::HookMouse => "hook.mouse",
            SpanKind::HookKeyboard => "hook.keyboard",
            SpanKind::InputContacts => "input.contacts",
            SpanKind::GestureToPlace => "gesture.to_place",
            SpanKind::StartupFirstPlace => "startup.first_place",
        }
    }

    pub fn category(self) -> &'static str {
        match self {
            SpanKind::Idle => "idle",
            SpanKind::DaemonTick
            | SpanKind::DaemonLayout
            | SpanKind::DaemonEvent
            | SpanKind::DaemonHoles
            | SpanKind::DaemonScan
            | SpanKind::DaemonIpc
            | SpanKind::DaemonJournal
            | SpanKind::DaemonTopology => "daemon",
            SpanKind::RenderScene
            | SpanKind::RenderHosts
            | SpanKind::RenderSources
            | SpanKind::RenderFrames
            | SpanKind::RenderDraw
            | SpanKind::RenderDwmFlush
            | SpanKind::RenderVerify
            | SpanKind::RenderRgnHoles => "render",
            SpanKind::Win32Place => "win32",
            SpanKind::HookMouse | SpanKind::HookKeyboard | SpanKind::InputContacts => "input",
            SpanKind::GestureToPlace | SpanKind::StartupFirstPlace => "e2e",
        }
    }

    pub fn slow_threshold_us(self) -> u64 {
        match self {
            SpanKind::HookMouse | SpanKind::HookKeyboard => 5_000,
            SpanKind::GestureToPlace => 16_000,
            SpanKind::RenderScene => 30_000,
            SpanKind::StartupFirstPlace => 600_000,
            _ => 100_000,
        }
    }

    pub fn is_daemon_thread(self) -> bool {
        matches!(
            self,
            SpanKind::DaemonTick
                | SpanKind::DaemonLayout
                | SpanKind::DaemonEvent
                | SpanKind::DaemonHoles
                | SpanKind::DaemonScan
                | SpanKind::DaemonIpc
                | SpanKind::DaemonJournal
                | SpanKind::DaemonTopology
        )
    }

    pub fn is_render_thread(self) -> bool {
        matches!(
            self,
            SpanKind::RenderScene
                | SpanKind::RenderHosts
                | SpanKind::RenderSources
                | SpanKind::RenderFrames
                | SpanKind::RenderDraw
                | SpanKind::RenderDwmFlush
                | SpanKind::RenderVerify
                | SpanKind::RenderRgnHoles
        )
    }
}

#[derive(Clone, Copy)]
struct SpanRecord {
    kind: SpanKind,
    start_qpc_us: i64,
    dur_us: u64,
    tid: u32,
    arg: u64,
}

#[derive(Clone)]
struct InstantMarker {
    name: &'static str,
    cat: &'static str,
    qpc_us: i64,
    tid: u32,
    detail: String,
}

#[derive(Clone, Serialize)]
pub struct CounterSample {
    pub qpc_us: i64,
    pub uptime_ms: u64,
    pub cpu_pct: f64,
    pub working_set_mb: f64,
    pub private_mb: f64,
    pub gdi_objects: u32,
    pub user_objects: u32,
    pub os_handles: u32,
}

#[derive(Clone)]
struct ThreadEntry {
    name: &'static str,
    tid: u32,
    handle: isize,
}

static PERF_ENABLED: AtomicBool = AtomicBool::new(true);
static DIAG_TRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static START_QPC_US: AtomicI64 = AtomicI64::new(0);
static FIRST_PLACE_US: AtomicI64 = AtomicI64::new(-1);
static STALL_COUNT: AtomicU64 = AtomicU64::new(0);

pub static DAEMON_HEARTBEAT_QPC_US: AtomicI64 = AtomicI64::new(0);
pub static RENDER_HEARTBEAT_QPC_US: AtomicI64 = AtomicI64::new(0);
pub static DAEMON_SPAN_ID: AtomicU8 = AtomicU8::new(0);
pub static RENDER_SPAN_ID: AtomicU8 = AtomicU8::new(0);

const SPAN_QUEUE_CAP: usize = 16_384;
const RING_CAP: usize = 32_768;
const MARKER_RING_CAP: usize = 2_048;
const COUNTER_CAP: usize = 480;

struct Store {
    queue: ArrayQueue<SpanRecord>,
    markers_queue: ArrayQueue<InstantMarker>,
    agg: Mutex<AggState>,
    threads: Mutex<Vec<ThreadEntry>>,
}

struct AggState {
    buckets: [Vec<u64>; 23],
    max_record: [Option<(i64, u64, u32, u64)>; 23],
    ring: VecDeque<SpanRecord>,
    markers: VecDeque<InstantMarker>,
    counters: VecDeque<CounterSample>,
    last_cpu_ms: Option<(i64, f64)>,
}

impl Default for AggState {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| Vec::new()),
            max_record: [None; 23],
            ring: VecDeque::with_capacity(4096),
            markers: VecDeque::with_capacity(256),
            counters: VecDeque::with_capacity(COUNTER_CAP),
            last_cpu_ms: None,
        }
    }
}

static STORE: OnceLock<Store> = OnceLock::new();

fn store() -> &'static Store {
    STORE.get_or_init(|| Store {
        queue: ArrayQueue::new(SPAN_QUEUE_CAP),
        markers_queue: ArrayQueue::new(1024),
        agg: Mutex::new(AggState::default()),
        threads: Mutex::new(Vec::new()),
    })
}

pub fn init(diag_enabled: bool) {
    let enabled = std::env::var("PLANOR_PERF").as_deref() != Ok("0");
    PERF_ENABLED.store(enabled, Ordering::Relaxed);
    DIAG_TRACE_ENABLED.store(diag_enabled, Ordering::Relaxed);
    let now = qpc_us();
    let _ = START_QPC_US.compare_exchange(0, now, Ordering::SeqCst, Ordering::Relaxed);
    DAEMON_HEARTBEAT_QPC_US.store(now, Ordering::Relaxed);
    RENDER_HEARTBEAT_QPC_US.store(now, Ordering::Relaxed);
    let _ = store();
}

pub fn set_diag_enabled(enabled: bool) {
    DIAG_TRACE_ENABLED.store(enabled, Ordering::Relaxed);
}

#[cfg(test)]
pub fn set_enabled(enabled: bool) {
    PERF_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn is_enabled() -> bool {
    PERF_ENABLED.load(Ordering::Relaxed)
}

#[inline]
pub fn heartbeat_daemon() {
    DAEMON_HEARTBEAT_QPC_US.store(qpc_us(), Ordering::Relaxed);
}

#[inline]
pub fn heartbeat_render() {
    RENDER_HEARTBEAT_QPC_US.store(qpc_us(), Ordering::Relaxed);
}

pub fn note_stall() -> u64 {
    STALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn stall_count() -> u64 {
    STALL_COUNT.load(Ordering::Relaxed)
}

pub fn register_thread(name: &'static str) {
    unsafe {
        let proc_h = GetCurrentProcess();
        let thr_h = GetCurrentThread();
        let mut dup: *mut c_void = std::ptr::null_mut();
        let ok = DuplicateHandle(
            proc_h,
            thr_h,
            proc_h,
            &mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        let tid = GetCurrentThreadId();
        if ok != 0 {
            if let Ok(mut list) = store().threads.lock() {
                if !list.iter().any(|t| t.tid == tid) {
                    list.push(ThreadEntry {
                        name,
                        tid,
                        handle: dup as isize,
                    });
                }
            }
        }
    }
}

pub struct SpanGuard {
    kind: SpanKind,
    start_us: i64,
    prev_span: u8,
    arg: u64,
    active: bool,
}

impl SpanGuard {
    #[inline]
    pub fn new(kind: SpanKind) -> Self {
        Self::with_arg(kind, 0)
    }

    #[inline]
    pub fn with_arg(kind: SpanKind, arg: u64) -> Self {
        if !PERF_ENABLED.load(Ordering::Relaxed) {
            return Self {
                kind,
                start_us: 0,
                prev_span: 0,
                arg,
                active: false,
            };
        }
        let prev_span = if kind.is_daemon_thread() {
            DAEMON_SPAN_ID.swap(kind as u8, Ordering::Relaxed)
        } else if kind.is_render_thread() {
            RENDER_SPAN_ID.swap(kind as u8, Ordering::Relaxed)
        } else {
            0
        };
        Self {
            kind,
            start_us: qpc_us(),
            prev_span,
            arg,
            active: true,
        }
    }
}

impl Drop for SpanGuard {
    #[inline]
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self.kind.is_daemon_thread() {
            DAEMON_SPAN_ID.store(self.prev_span, Ordering::Relaxed);
        } else if self.kind.is_render_thread() {
            RENDER_SPAN_ID.store(self.prev_span, Ordering::Relaxed);
        }
        let end_us = qpc_us();
        let dur_us = (end_us - self.start_us).max(0) as u64;
        push_span(self.kind, self.start_us, dur_us, self.arg);
    }
}

#[inline]
fn push_span(kind: SpanKind, start_qpc_us: i64, dur_us: u64, arg: u64) {
    let tid = current_tid();
    let _ = store().queue.force_push(SpanRecord {
        kind,
        start_qpc_us,
        dur_us,
        tid,
        arg,
    });
    if DIAG_TRACE_ENABLED.load(Ordering::Relaxed) && dur_us >= kind.slow_threshold_us() {
        diag::trace(format!(
            "SLOW {} {:.2} ms qpc_us={}",
            kind.name(),
            dur_us as f64 / 1000.0,
            start_qpc_us
        ));
    }
}

pub fn record_duration_from(kind: SpanKind, start_qpc_us: i64, arg: u64) {
    if !PERF_ENABLED.load(Ordering::Relaxed) || start_qpc_us <= 0 {
        return;
    }
    let now = qpc_us();
    let dur_us = (now - start_qpc_us).max(0) as u64;
    push_span(kind, start_qpc_us, dur_us, arg);
}

pub fn mark_first_place_done() {
    let start = START_QPC_US.load(Ordering::Relaxed);
    if start <= 0 {
        return;
    }
    let now = qpc_us();
    let elapsed = (now - start).max(0);
    if FIRST_PLACE_US
        .compare_exchange(-1, elapsed, Ordering::SeqCst, Ordering::Relaxed)
        .is_ok()
    {
        push_span(SpanKind::StartupFirstPlace, start, elapsed as u64, 0);
    }
}

pub fn record_instant(name: &'static str, cat: &'static str, detail: impl Into<String>) {
    if !PERF_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let _ = store().markers_queue.force_push(InstantMarker {
        name,
        cat,
        qpc_us: qpc_us(),
        tid: current_tid(),
        detail: detail.into(),
    });
}

fn drain_into(agg: &mut AggState) {
    let s = store();
    while let Some(rec) = s.queue.pop() {
        let idx = rec.kind as usize;
        if idx < agg.buckets.len() {
            agg.buckets[idx].push(rec.dur_us);
            let update_max = agg.max_record[idx]
                .as_ref()
                .is_none_or(|m| rec.dur_us > m.1);
            if update_max {
                agg.max_record[idx] = Some((rec.start_qpc_us, rec.dur_us, rec.tid, rec.arg));
            }
        }
        if agg.ring.len() == RING_CAP {
            agg.ring.pop_front();
        }
        agg.ring.push_back(rec);
    }
    while let Some(m) = s.markers_queue.pop() {
        if agg.markers.len() == MARKER_RING_CAP {
            agg.markers.pop_front();
        }
        agg.markers.push_back(m);
    }
}

pub fn sample_counters() {
    let now = qpc_us();
    let start = START_QPC_US.load(Ordering::Relaxed);
    let uptime_ms = if start > 0 {
        ((now - start).max(0) / 1000) as u64
    } else {
        0
    };
    let (user_ms, kernel_ms, ws_mb, priv_mb, gdi, user_objs, handles) = read_os_counters();
    let total_cpu_ms = user_ms + kernel_ms;
    let Ok(mut agg) = store().agg.lock() else {
        return;
    };
    drain_into(&mut agg);
    let cpu_pct = if let Some((prev_us, prev_cpu_ms)) = agg.last_cpu_ms {
        let dt_ms = ((now - prev_us).max(1) as f64) / 1000.0;
        ((total_cpu_ms - prev_cpu_ms).max(0.0) / dt_ms) * 100.0
    } else {
        0.0
    };
    agg.last_cpu_ms = Some((now, total_cpu_ms));
    if agg.counters.len() == COUNTER_CAP {
        agg.counters.pop_front();
    }
    agg.counters.push_back(CounterSample {
        qpc_us: now,
        uptime_ms,
        cpu_pct,
        working_set_mb: ws_mb,
        private_mb: priv_mb,
        gdi_objects: gdi,
        user_objects: user_objs,
        os_handles: handles,
    });
}

fn read_os_counters() -> (f64, f64, f64, f64, u32, u32, u32) {
    unsafe {
        let h = GetCurrentProcess();
        let (mut cr, mut ex, mut kt, mut ut) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        GetProcessTimes(h, &mut cr, &mut ex, &mut kt, &mut ut);
        let mut mem = ProcessMemoryCountersEx {
            cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            ..Default::default()
        };
        K32GetProcessMemoryInfo(h, &mut mem, mem.cb);
        let gdi = GetGuiResources(h, GR_GDIOBJECTS);
        let user_objs = GetGuiResources(h, GR_USEROBJECTS);
        let mut handles = 0u32;
        GetProcessHandleCount(h, &mut handles);
        (
            ut.as_ms(),
            kt.as_ms(),
            mem.working_set_size as f64 / (1024.0 * 1024.0),
            mem.private_usage as f64 / (1024.0 * 1024.0),
            gdi,
            user_objs,
            handles,
        )
    }
}

fn read_thread_times(handle: isize) -> (f64, f64) {
    if handle == 0 {
        return (0.0, 0.0);
    }
    unsafe {
        let (mut cr, mut ex, mut kt, mut ut) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        if GetThreadTimes(handle as *mut c_void, &mut cr, &mut ex, &mut kt, &mut ut) != 0 {
            (ut.as_ms(), kt.as_ms())
        } else {
            (0.0, 0.0)
        }
    }
}

#[derive(Serialize)]
pub struct SpanStats {
    pub name: &'static str,
    pub category: &'static str,
    pub count: usize,
    pub total_us: u64,
    pub min_us: u64,
    pub avg_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub slow_count: usize,
    pub threshold_us: u64,
    /// Buckets: [<1ms, 1-4ms, 4-16ms, 16-50ms, 50-100ms, >=100ms]
    pub histogram: [u64; 6],
}

#[derive(Serialize)]
pub struct ProcessProfile {
    pub pid: u32,
    pub cpu_user_ms: f64,
    pub cpu_kernel_ms: f64,
    pub cpu_avg_pct: f64,
    pub working_set_mb: f64,
    pub peak_working_set_mb: f64,
    pub private_mb: f64,
    pub gdi_objects: u32,
    pub user_objects: u32,
    pub os_handles: u32,
}

#[derive(Serialize)]
pub struct ThreadProfile {
    pub name: &'static str,
    pub tid: u32,
    pub cpu_user_ms: f64,
    pub cpu_kernel_ms: f64,
    pub span_count: usize,
    pub busy_us: u64,
}

#[derive(Serialize)]
pub struct SlowestSpan {
    pub name: &'static str,
    pub category: &'static str,
    pub qpc_us: i64,
    pub dur_us: u64,
    pub tid: u32,
    pub arg: u64,
}

#[derive(Serialize)]
pub struct PerfBudgets {
    pub startup_first_place_ms: Option<f64>,
    pub startup_budget_ms: f64,
    pub startup_ok: bool,
    pub render_scene_warm_p99_ms: Option<f64>,
    pub render_scene_budget_ms: f64,
    pub render_scene_ok: bool,
    pub gesture_to_place_p99_ms: Option<f64>,
    pub gesture_budget_ms: f64,
    pub gesture_ok: bool,
    pub hook_callbacks_max_ms: Option<f64>,
    pub hook_budget_ms: f64,
    pub hook_ok: bool,
    pub all_pass: bool,
}

#[derive(Serialize)]
pub struct PerfReport {
    pub schema_version: u32,
    pub enabled: bool,
    pub qpc_frequency: i64,
    pub qpc_now_us: i64,
    pub uptime_ms: u64,
    pub first_place_ms: Option<f64>,
    pub stalls: u64,
    pub process: ProcessProfile,
    pub threads: Vec<ThreadProfile>,
    pub spans: Vec<SpanStats>,
    pub slowest_spans: Vec<SlowestSpan>,
    pub frame_telemetry: Option<serde_json::Value>,
    pub counters: Vec<CounterSample>,
    pub budgets: PerfBudgets,
}

fn percentile(sorted: &[u64], num: usize, den: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * num) / den;
    sorted[idx]
}

pub fn build_report(frame_telemetry: Option<serde_json::Value>) -> PerfReport {
    let now = qpc_us();
    let start = START_QPC_US.load(Ordering::Relaxed);
    let uptime_ms = if start > 0 {
        ((now - start).max(0) / 1000) as u64
    } else {
        0
    };
    let (user_ms, kernel_ms, ws_mb, priv_mb, gdi, user_objs, handles) = read_os_counters();
    let peak_ws_mb = unsafe {
        let mut mem = ProcessMemoryCountersEx {
            cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
            ..Default::default()
        };
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut mem, mem.cb);
        mem.peak_working_set_size as f64 / (1024.0 * 1024.0)
    };
    let cpu_avg_pct = if uptime_ms > 0 {
        ((user_ms + kernel_ms) / uptime_ms as f64) * 100.0
    } else {
        0.0
    };

    let mut spans = Vec::new();
    let mut ring_copy = Vec::new();
    let mut counters = Vec::new();
    if let Ok(mut agg) = store().agg.lock() {
        drain_into(&mut agg);
        for kind in SpanKind::ALL {
            let vals = &agg.buckets[kind as usize];
            if vals.is_empty() {
                continue;
            }
            let mut sorted = vals.clone();
            sorted.sort_unstable();
            let total_us: u64 = sorted.iter().sum();
            let min_us = sorted[0];
            let max_us = sorted[sorted.len() - 1];
            let avg_us = total_us / sorted.len() as u64;
            let p50_us = percentile(&sorted, 50, 100);
            let p95_us = percentile(&sorted, 95, 100);
            let p99_us = percentile(&sorted, 99, 100);
            let thr = kind.slow_threshold_us();
            let slow_count = sorted.iter().filter(|&&v| v >= thr).count();
            let mut histogram = [0u64; 6];
            for &v in &sorted {
                let b = if v < 1_000 {
                    0
                } else if v < 4_000 {
                    1
                } else if v < 16_000 {
                    2
                } else if v < 50_000 {
                    3
                } else if v < 100_000 {
                    4
                } else {
                    5
                };
                histogram[b] += 1;
            }
            spans.push(SpanStats {
                name: kind.name(),
                category: kind.category(),
                count: sorted.len(),
                total_us,
                min_us,
                avg_us,
                p50_us,
                p95_us,
                p99_us,
                max_us,
                slow_count,
                threshold_us: thr,
                histogram,
            });
        }
        ring_copy.extend(agg.ring.iter().copied());
        counters.extend(agg.counters.iter().cloned());
    }

    let mut slowest_spans: Vec<SlowestSpan> = ring_copy
        .iter()
        .map(|r| SlowestSpan {
            name: r.kind.name(),
            category: r.kind.category(),
            qpc_us: r.start_qpc_us,
            dur_us: r.dur_us,
            tid: r.tid,
            arg: r.arg,
        })
        .collect();
    slowest_spans.sort_by(|a, b| b.dur_us.cmp(&a.dur_us));
    slowest_spans.truncate(20);

    let threads = if let Ok(list) = store().threads.lock() {
        list.iter()
            .map(|t| {
                let (tu, tk) = read_thread_times(t.handle);
                let mut span_count = 0usize;
                let mut busy_us = 0u64;
                for r in &ring_copy {
                    if r.tid == t.tid {
                        span_count += 1;
                        busy_us += r.dur_us;
                    }
                }
                ThreadProfile {
                    name: t.name,
                    tid: t.tid,
                    cpu_user_ms: tu,
                    cpu_kernel_ms: tk,
                    span_count,
                    busy_us,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Warm scene budget: exclude startup scene (epoch 1) when calculating warm render_scene p99.
    let warm_scene_samples: Vec<u64> = ring_copy
        .iter()
        .filter(|r| r.kind == SpanKind::RenderScene && r.arg > 1)
        .map(|r| r.dur_us)
        .collect();
    let render_scene_warm_p99_ms = if !warm_scene_samples.is_empty() {
        let mut s = warm_scene_samples;
        s.sort_unstable();
        Some(percentile(&s, 99, 100) as f64 / 1000.0)
    } else {
        spans
            .iter()
            .find(|s| s.name == "render.scene")
            .map(|s| s.p99_us as f64 / 1000.0)
    };
    let gesture_to_place_p99_ms = spans
        .iter()
        .find(|s| s.name == "gesture.to_place")
        .map(|s| s.p99_us as f64 / 1000.0);
    let hook_callbacks_max_ms = spans
        .iter()
        .filter(|s| s.name == "hook.mouse" || s.name == "hook.keyboard")
        .map(|s| s.max_us as f64 / 1000.0)
        .reduce(f64::max);

    let first_place_raw = FIRST_PLACE_US.load(Ordering::Relaxed);
    let first_place_ms = (first_place_raw >= 0).then_some(first_place_raw as f64 / 1000.0);

    let startup_ok = first_place_ms.is_none_or(|v| v < 600.0);
    let render_scene_ok = render_scene_warm_p99_ms.is_none_or(|v| v < 16.0);
    let gesture_ok = gesture_to_place_p99_ms.is_none_or(|v| v < 16.0);
    let hook_ok = hook_callbacks_max_ms.is_none_or(|v| v < 5.0);

    let budgets = PerfBudgets {
        startup_first_place_ms: first_place_ms,
        startup_budget_ms: 600.0,
        startup_ok,
        render_scene_warm_p99_ms,
        render_scene_budget_ms: 16.0,
        render_scene_ok,
        gesture_to_place_p99_ms,
        gesture_budget_ms: 16.0,
        gesture_ok,
        hook_callbacks_max_ms,
        hook_budget_ms: 5.0,
        hook_ok,
        all_pass: startup_ok && render_scene_ok && gesture_ok && hook_ok,
    };

    PerfReport {
        schema_version: 1,
        enabled: is_enabled(),
        qpc_frequency: qpc_frequency(),
        qpc_now_us: now,
        uptime_ms,
        first_place_ms,
        stalls: stall_count(),
        process: ProcessProfile {
            pid: current_pid(),
            cpu_user_ms: user_ms,
            cpu_kernel_ms: kernel_ms,
            cpu_avg_pct,
            working_set_mb: ws_mb,
            peak_working_set_mb: peak_ws_mb,
            private_mb: priv_mb,
            gdi_objects: gdi,
            user_objects: user_objs,
            os_handles: handles,
        },
        threads,
        spans,
        slowest_spans,
        frame_telemetry,
        counters,
        budgets,
    }
}

pub fn format_summary(report: &PerfReport) -> String {
    let mut out = Vec::new();
    out.push(format!(
        "Planor perf (uptime {:.1}s, stalls {}, first_place {}, ws {:.1} MB, CPU {:.1}%, GDI {}, USER {}, handles {})",
        report.uptime_ms as f64 / 1000.0,
        report.stalls,
        report
            .first_place_ms
            .map(|v| format!("{v:.1}ms"))
            .unwrap_or_else(|| "n/a".into()),
        report.process.working_set_mb,
        report.process.cpu_avg_pct,
        report.process.gdi_objects,
        report.process.user_objects,
        report.process.os_handles
    ));
    out.push(format!(
        "{:<22} {:>7} {:>8} {:>8} {:>8} {:>5}",
        "span", "count", "p50(ms)", "p99(ms)", "max(ms)", "slow"
    ));
    for s in &report.spans {
        out.push(format!(
            "{:<22} {:>7} {:>8.2} {:>8.2} {:>8.2} {:>5}",
            s.name,
            s.count,
            s.p50_us as f64 / 1000.0,
            s.p99_us as f64 / 1000.0,
            s.max_us as f64 / 1000.0,
            s.slow_count
        ));
    }
    out.push(format!(
        "Budgets: startup={} (<600ms) scene_warm={} (<16ms) gesture={} (<16ms) hooks={} (<5ms) -> {}",
        if report.budgets.startup_ok { "PASS" } else { "FAIL" },
        if report.budgets.render_scene_ok { "PASS" } else { "FAIL" },
        if report.budgets.gesture_ok { "PASS" } else { "FAIL" },
        if report.budgets.hook_ok { "PASS" } else { "FAIL" },
        if report.budgets.all_pass { "ALL PASS" } else { "BREACH" },
    ));
    out.join("\n")
}

pub fn reset() {
    let s = store();
    while s.queue.pop().is_some() {}
    while s.markers_queue.pop().is_some() {}
    if let Ok(mut agg) = s.agg.lock() {
        for b in &mut agg.buckets {
            b.clear();
        }
        agg.max_record = [None; 23];
        agg.ring.clear();
        agg.markers.clear();
        agg.counters.clear();
    }
    STALL_COUNT.store(0, Ordering::Relaxed);
}

/// Export Perfetto / Chrome Trace JSON using the exact QPC microsecond clock shared with `planor-rec`.
pub fn export_chrome_trace(seconds: Option<u64>) -> serde_json::Value {
    let now = qpc_us();
    let cutoff = seconds.map(|s| now.saturating_sub(s as i64 * 1_000_000));
    let pid = current_pid();
    let mut events = Vec::<serde_json::Value>::new();

    events.push(serde_json::json!({
        "ph": "M",
        "pid": pid,
        "tid": 0,
        "name": "process_name",
        "args": { "name": "planor" }
    }));

    if let Ok(list) = store().threads.lock() {
        for t in list.iter() {
            events.push(serde_json::json!({
                "ph": "M",
                "pid": pid,
                "tid": t.tid,
                "name": "thread_name",
                "args": { "name": t.name }
            }));
        }
    }

    if let Ok(mut agg) = store().agg.lock() {
        drain_into(&mut agg);
        for rec in &agg.ring {
            if cutoff.is_some_and(|c| rec.start_qpc_us < c) {
                continue;
            }
            events.push(serde_json::json!({
                "ph": "X",
                "pid": pid,
                "tid": rec.tid,
                "cat": rec.kind.category(),
                "name": rec.kind.name(),
                "ts": rec.start_qpc_us,
                "dur": rec.dur_us,
                "args": {
                    "qpc_us": rec.start_qpc_us,
                    "arg": rec.arg,
                }
            }));
        }
        for m in &agg.markers {
            if cutoff.is_some_and(|c| m.qpc_us < c) {
                continue;
            }
            events.push(serde_json::json!({
                "ph": "i",
                "pid": pid,
                "tid": m.tid,
                "cat": m.cat,
                "name": m.name,
                "s": "p",
                "ts": m.qpc_us,
                "args": { "detail": m.detail }
            }));
        }
        for c in &agg.counters {
            if cutoff.is_some_and(|cut| c.qpc_us < cut) {
                continue;
            }
            events.push(serde_json::json!({
                "ph": "C",
                "pid": pid,
                "name": "planor.resources",
                "ts": c.qpc_us,
                "args": {
                    "cpu_pct": c.cpu_pct,
                    "working_set_mb": c.working_set_mb,
                    "private_mb": c.private_mb,
                    "gdi_objects": c.gdi_objects,
                    "user_objects": c.user_objects,
                    "os_handles": c.os_handles,
                }
            }));
        }
    }

    serde_json::json!({
        "schema_version": 1,
        "clock": "qpc_us",
        "qpc_frequency": qpc_frequency(),
        "qpc_now_us": now,
        "displayTimeUnit": "ms",
        "traceEvents": events,
    })
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_span_overhead_benchmark() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init(false);
        reset();
        const N: usize = 500_000;

        set_enabled(false);
        let t0 = std::time::Instant::now();
        for _ in 0..N {
            let _g = SpanGuard::new(SpanKind::DaemonTick);
        }
        let off_ns = t0.elapsed().as_nanos() as f64 / N as f64;

        set_enabled(true);
        let t1 = std::time::Instant::now();
        for _ in 0..N {
            let _g = SpanGuard::new(SpanKind::DaemonTick);
        }
        let on_ns = t1.elapsed().as_nanos() as f64 / N as f64;

        eprintln!(
            "PERF_SPAN_OVERHEAD: enabled=false -> {:.1} ns/span, enabled=true -> {:.1} ns/span",
            off_ns, on_ns
        );
        // Drain and verify Chrome trace JSON structure.
        let trace = export_chrome_trace(Some(60));
        assert_eq!(trace["clock"], "qpc_us");
        assert!(trace["traceEvents"].as_array().is_some_and(|a| !a.is_empty()));
        let report = build_report(None);
        assert!(report.spans.iter().any(|s| s.name == "daemon.tick"));
        reset();
    }
}
