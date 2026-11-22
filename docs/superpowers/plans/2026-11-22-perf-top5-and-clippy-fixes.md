# Plan: Top-5 Performance Optimizations & Audit Failure Fixes

**Date**: 2026-11-22  
**Baseline Commit**: `ccd2734` (`Add behind-overlay cover, 120Hz/VRR pacing, and full resize tests`)  
**Baseline Audit Folder**: `build/audit/20261122-151129`

---

## Baseline Metrics (`ccd2734`)

- `verify.ps1 -Tier fast -Perf -NoRecordOnRetry -Parallel`: 9 pass · 0 flaky · 1 fail (`33.77s`)
  - Passing live tests: `perf_spans_and_budgets`, `state_json_matches_real_windows`, `stop_restores_exact_rect`
  - Failing live test: `scn_focus::focus_walk_right_then_left_n3` — blocked by isolated `exebox` (`WinSta0\exebox-*`, `SetForegroundWindow` cannot take foreground on a non-input desktop, `Fg=0`). **We do NOT weaken or delete this test**; leave it intact and report the environment block honestly.
  - Budgets: `startup = 126.906 / 600 ms`, `render_p99 = 0.123 / 16 ms`, `gesture_p99 = 3.117 / 16 ms`, `stalls = 0`, `ws = 76.2 MB`, `handles = 1137`, `gdi = 0`, `user = 22`.
- `planor-rec` unit tests: `104 passed, 0 failed` (`1.11s`).
- `perf_span_overhead_benchmark`: `20.4 ns/span` (disabled), `290.3 ns/span` (enabled).
- `cargo clippy --workspace --all-targets -- -W clippy::perf`: **exit 101** (`3` `#[deny(clippy::erasing_op)]` errors in `tools/planor-rec/src/capture.rs:771, 777` + `src/`/`tests/` warnings).
- `feel.ps1 -Only open_apps,keyboard_scroll,touchpad_scroll,resize,close_apps -NoRecord` (`build/feel/20261122-151209`):
  - `open_apps`: `0/14` late, `p95=19.9ms`, `worst=23.8ms`, `eff=60.4Hz`, `stalls=0`
  - `keyboard_scroll`: `0/142` late, `p95=17.8ms`, `worst=19.8ms`, `gesture_p99=0.496ms`, `eff=59.8Hz`, `stalls=0`
  - `touchpad_scroll`: `1/645` (`0.2%`) late/dropped, `p95=17.9ms`, `worst=82.6ms`, `gesture_p99=7.995ms`, `render_p99=0.056ms`, `eff=60.0Hz`, `stalls=0` (`daemon.layout` 3,981 calls, `daemon.ipc` 3,964 calls max 13.81ms, `daemon` `cpu_sys` ~984–1,078ms)
  - `resize`: `0` anim frames (`exebox` opened 1 window), `stalls=0`
  - `close_apps`: `0/11` late, `p95=18.3ms`, `worst=19.1ms`, `eff=61.2Hz`, `stalls=0`

---

## Standing Constraints

- Preserve 100% of business rules (tiling, focus, drag, touchpad gestures, behind-overlay cover + delayed spinner, 120 Hz/VRR pacing, recovery journal).
- No GitLab / `glab`.
- No `println!` in `src/{backend,daemon,render,win32,perf,watchdog}.rs`.
- Never weaken or remove `tests/scn_focus.rs::focus_walk_right_then_left_n3`.
- Use `test-and-vision` (`Model: "flash_lite"`) ONLY with `read_file` on `build/audit/<stamp>/*` logs — explicitly forbidden from reading any source code (`*.rs`, `*.toml`, `*.ps1`, `docs/*`) or running commands.
- Log every batch via `scripts/ai-journal.ps1`.

---

## Ordered Execution Steps

### Step 1 — Commit Plan to Disk
- Write `docs/superpowers/plans/2026-11-22-perf-top5-and-clippy-fixes.md` and commit ONLY this file before modifying any `.rs` file.

### Step 2 — Fix Clippy Compile Errors (Exit 101) & Clean `src/` Warnings
- `tools/planor-rec/src/capture.rs` (lines 760–778): introduce `let px = |row: usize, col: usize| (row * 2 + col) * 4;` to fix all `3` `#[deny(clippy::erasing_op)]` compile errors and `identity_op` warnings.
- Clean `src/` & `tests/` Clippy warnings:
  - `src/perf.rs`: collapse nested `if`, use `sort_by_key(|a| std::cmp::Reverse(a.dur_us))`.
  - `src/gpu.rs`: `#[derive(Default)]` on `Sampler`, `is_multiple_of(4)`, `(0. ..1e6).contains(&mb)`.
  - `src/watchdog.rs`: collapse nested `if`, `ticks.is_multiple_of(5)`.
  - `src/config_tui.rs`: `.then_some(Color::Rgb(...))`.
  - `src/note.rs`: `as_chunks_mut::<4>().0`.
  - `src/input.rs`, `src/main.rs`: collapse nested `if` blocks.
  - `src/win32.rs`: collapse nested `if` blocks and use struct initializer syntax with `..Default::default()` for `DISPLAY_DEVICEW`, `DEVMODEW`, and `DWM_TIMING_INFO`.
  - `src/diag.rs`: move `#[cfg(test)] mod tests` to the end of the file; collapse nested `if`.
  - `src/sim_tests.rs`: add `type GestureList = ...` for `clippy::type_complexity`.
  - `tests/core.rs`, `tests/prop_core.rs`: apply `is_multiple_of(...)`, remove `- 0`, collapse nested `if`.

### Step 3 — Opt 1 & Opt 2: `perf::current_tid()` TLS Cache, Zero No-Op `daemon.layout`/`daemon.ipc` Spans, Idle Pipe Throttle
- `src/perf.rs`: cache `GetCurrentThreadId()` once per thread in `thread_local! static CACHED_TID: u32`; add multi-thread unit test verifying `current_tid() == GetCurrentThreadId()`.
- `src/main.rs` (`PipeServer::poll`): move `SpanGuard::new(SpanKind::DaemonIpc)` inside `PipeServer::poll` so it only records when a client actually exchanges/writes IPC data (not on `ERROR_PIPE_LISTENING` no-ops).
- `src/daemon.rs` (`Engine::layout` & `daemon::run`):
  - Move `SpanGuard::new(SpanKind::DaemonLayout)` and `slow("layout", t)` inside `Engine::layout()` right after `if self.blocked() || !self.dirty { return Ok(()); }`.
  - Remove both outer `DaemonLayout` and `DaemonIpc` spans in `daemon::run()`.
  - Throttle idle `server.poll` in `daemon::run()` when `!server.connected && !server.has_pending_write()` to at most once per `4 ms`, while polling immediately whenever connected or flushing a reply.

### Step 4 — Opt 3: `HotCfg` + `Arc` `displays`/`holes` + Zero-Clone `daemon::layout`
- `src/render.rs`: change `Scene::monitors` to `Arc<[(String, Rect, Rect)]>` and `Scene::holes` to `Arc<[Rect]>`.
- `src/backend.rs`: update `SimDesktop::apply_scene` for `Arc` slices.
- `src/daemon.rs`:
  - Add `pub(crate) struct HotCfg` (`Clone, Copy, Debug`) with `HotCfg::from_toml(&cfg)` initialized in `new_for_test` and `daemon::run` and refreshed in `Command::Reload`.
  - Replace hot-path `self.cfg[...]` and hex-color string parsing in `reveal`, `mark_cover`, `add_with_frame`, `home_strip`, `return_home`, `refresh`, `layout`, `contacts`, `scroll_by`, `wheel`, `pad_scroll_end`, `pad_begin`, `pad_move`, `pad_end`, `drag_update`, `event` with `self.hot`.
  - Add `displays_arc: Arc<[(String, Rect, Rect)]>` (refreshed in `topology()`) and `last_holes_arc: Arc<[Rect]>` (refreshed only when `holes()` changes).
  - In `Engine::layout()`: remove `std::mem::take(&mut self.strips)`, pre-size `tiles`, compare `d.destination` fields without cloning `String`, and clone `Arc` handles instead of deep vectors.
  - Clean `daemon.rs` Clippy warnings and move `#[cfg(test)] mod tests` to the end of `src/daemon.rs`.

### Step 5 — Opt 4: Modularize `render.rs`, Reuse `FrameScratch`, Move WGC Sources & Applied Holes into `Host`
- `src/render.rs`:
  - Introduce `DrawFrame<'a>` for `Surface::draw` and remove redundant `&*` derefs.
  - Add zero-alloc `DrawSig::matches` and `DrawSig::set_from`, reusing allocation with `last_draw_valid: bool`.
  - Move `pictures`, `failed_sources`, `pending_sources`, `applied_holes`, and monotonic `gen: u64` into `Host` (keyed by `(isize, usize)`, zero per-tile `String` clones; worker results checked against `(monitor, host_gen)`).
  - Introduce `FrameScratch` and `RenderState` with methods `clear_state`, `poll_background_work`, `prepare_scene`, `update_idle_sources`, `build_host_frame`, `apply_placements`, and `post_present`.
  - Cache `last_peeks` so `win32::set_peeks` is only called when hits change (and reset cleanly on `Clear`/`Stop`).

### Step 6 — Add Targeted Tests & Rebuild `planor-rec`
- `src/sim_tests.rs`: verify `HotCfg` stays synchronized on `Command::Reload` and `Scene` `Arc` `monitors`/`holes` reflect updates in both **landscape** (`1920x1080`) and **portrait** (`1080x1920`).
- `src/perf.rs`: unit test for TLS-cached `current_tid()`.
- Run `cargo clippy --workspace --all-targets -- -W clippy::perf` (expect exit 0, 0 warnings in `planor`).
- Run `core`, `prop_core`, `sim_tests`/unit tests, and `planor-rec` tests.
- Rebuild release `tools/planor-rec` and copy to gitignored `tools/planor-rec-bin/planor-rec.exe`.

### Step 7 — Full Audit, No-Code `flash_lite` Comparison, Commit & Report
- Run reusable `scratch/audit.ps1` producing `build/audit/<new-stamp>/*`.
- Use `test-and-vision` (`Model: "flash_lite"`, `read_file` only, no source-code reads) to extract metrics and compare against `build/audit/20261122-151129`.
- Commit implementation cleanly on `main` (excluding `build/` and `tools/planor-rec-bin/`), journal `[handoff]`, and report before/after metrics + honest `exebox` / 120 Hz / VRR boundaries.
