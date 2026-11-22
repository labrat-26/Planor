# Changelog

All notable changes to Planor and its developer tooling harness are documented here.

## [0.4.0] - 2026-11-22

Win+W notes N-010..N-024, each checked on landscape and portrait monitors, plus GPU profiling.

### Changed
- **Same controls on every monitor, turned with it (N-014, N-011)**: three fingers along the strip scroll it 1:1 (sideways on landscape, up/down on portrait); across the strip they roll the focused group, or with no group hop to the monitor that way, once per gesture. On portrait this used to hop focus one window at a time and a sideways swipe could never go back right.
- **Drag and drop (N-012, N-015, N-016, N-018, N-004)**: nothing dodges any more. While a window is carried, every window on the monitor under the cursor shows a small filled stack zone (`layout.stack_zone_px = 120`, at most a third of the window); the zones move with the cursor to another monitor. Drop in a zone: group. Drop elsewhere on a window of the same monitor: the two swap places (onto a sibling peek: reorder within the group). A grouped window dropped on another window goes beside it; from another monitor a drop goes beside, never swaps. Modes are positional (no speed or dwell). `stack_hover_ms`, `stack_center_px` and `animation.drag_slow_velocity_px_s` are gone; holding `drag_stack_modifier` makes the drop do nothing.
- **Groups (N-010, N-017)**: a pair no longer loops round; its other member peeks from the side it is on and glides between them. Past 50 % the next member settles in and gets focus; a member rolling out no longer vanishes at half way.

### Added
- **Win + wheel (N-023)**: scrolls the strip under the cursor along its axis; Win + Shift + wheel rolls the focused group one member per notch (`mouse.win_wheel_step_px = 240`). Start menu is suppressed like Win+W.
- **GPU profiling (`src/gpu.rs`)**: `gpu.draw` span from D3D11 timestamp queries around each overlay draw (read back without stalls); a `Planor.Gpu` thread samples this process's GPU engine utilisation (PDH), dedicated/shared video memory (DXGI) and DWM composition rate vs refresh rate with late/dropped/missed frames. In `planor perf` (`process.gpu`, summary line), the Chrome trace counters and `digest.ps1 -Perf`.

### Fixed
- **Gesture lock**: fingers lifting one by one no longer end a four-finger drag and start a three-finger scroll (the jerk at every drop); a fourth finger touching mid-scroll no longer picks a window up.
- **New windows that move themselves (N-020)**: put back in their slot during their first 2.5 s (at most 6 times).
- **Momentum (N-013, not verified live)**: stepped every 8 ms on a frame-rate timer and placed directly, instead of a 70 ms ease restarted every 32 ms.
- **Render (N-022, N-024, not verified live)**: during motion the border follows the gliding stand-in; peek thumbnails map one source pixel to one screen pixel (title bars were stretched).

### Tests
- `tests/core.rs`: 7 new (pair no-wrap, 50 % settle, sliding member drawn, swap, group reorder, drop decision, gesture lock); the drop-zone mockup disagreement became a passing owner-decision test. One stale assertion updated: `group_is_a_deck_of_equal_windows_with_peeking_neighbours_in_landscape` expected the pair's first member below the second (the loop-around N-010 removes).
- `src/sim_tests.rs`: 7 new, every one on landscape and portrait (scroll axis, monitor hop, Win+wheel, swap and zone, group reorder, cross-monitor drop, newborn window).
- `tests/live.rs::perf_spans_and_budgets` also requires `gpu.draw` and a DWM refresh rate.

## [0.3.0] - 2026-11-22

### Added
- **Fake Window Backend & Simulated Daemon Suite (`src/backend.rs`, `src/sim_tests.rs`, item `9.11`)**: Abstracted OS/Win32/DWM/VirtualDesktop operations behind `WindowBackend` (`Win32Backend` for production, `FakeBackend` / `SimDesktop` for deterministic tests) and parameterized `Engine<B>` and `Journal`. Added 9 simulated multi-monitor & scenario tests covering a 3-monitor landscape+portrait rig, cross-monitor drag with owned modal transfer, all `scn_display` scenarios (`offline_15s_round_trip`, `short_blip_3s_and_delayed_taskbar_work_area`, `pull_orphans`, `offline_then_stop`, `offline_then_kill_recover`, `topology_while_dragging`), and focus/stack/move/pin/fullscreen/rejection/holes/desktop/panic_exit checks in `< 0.5 s`.
- **Safe Parallel Test Execution (`scripts/verify.ps1 -Parallel`, `tests/common/helpers.rs`, item `5.9`)**: Added PowerShell 5.1-native `ProcessStartInfo` worker pool (`-Parallel`, `-MaxJobs`) and PID-scoped `PlanorSentinel_p<pid>` window class/rules so independent unit targets (`MaxJobs=4`) and an explicit allowlist of non-key, non-cursor, non-DXGI Win32 live tests (`MaxJobs=2`) run safely in parallel while failed first attempts retry serially with recording.
- **Synthetic Ring Capture Source (`tools/planor-rec/src/ring.rs`, `main.rs`)**: Added `--source dxgi|synthetic` (`--synthetic-size`, `--synthetic-monitors`) to `planor-rec ring` so full live FFmpeg segment-wrap encoding, pipe `ring-status`, `dump` MP4 remuxing, and `analyze` pipelines can be benchmarked & end-to-end verified even without DXGI capture access.
- **Session & Artifact Cleanup Tooling (`scripts/clean-sessions.ps1`)**: Safe pruning of stale `build\verify\20*` folders, `%LOCALAPPDATA%\planor\stalls\*` bundles, and temporary Planor test folders while preserving `latest*.{md,json}`, `latest-perf.json`, cache, and user config.

### Fixed & Optimized
- **WGC Failure Retry Storm (`src/render.rs`)**: Added `failed_sources` 5-second per-tile cooldown next to `pictures`, eliminating synchronous ~45 ms/window WGC `Source::new` stalls on un-capturable windows and stderr log spam.
- **Immediate Post-IPC Layout Dispatch (`src/daemon.rs`)**: Triggered `engine.layout()` immediately after `PipeServer::poll()` when `dirty` is set (instead of waiting up to a Windows 32–47 ms timer tick in `select!`).
- **Daemon & Render Overhead (`src/daemon.rs`, `src/render.rs`, `src/win32.rs`, `src/perf.rs`)**: Cached Direct2D solid-color brushes in `Surface`, used adaptive idle `recv_timeout` (`33 ms` / `50 ms`), throttled idle `monitors()` (`500 ms`) and `current_desktop()` (`150 ms`) polling, filtered `Engine::scan()` candidates before DWM frame sorting and batched initial `Journal` writes (`begin_batch`/`end_batch`), cached `SELF_ELEVATED` via `OnceLock`, made `SpanKind::from_u8` O(1), and resolved workspace Clippy lints (`possible_missing_else` / collapsible `if` / range checks).

### Verified Numbers (`v0.3.0`)
- **All 4 Performance Budgets Pass (`live::perf_spans_and_budgets -Perf`, `budgets.all_pass = true` in `2.31 s`)**:
  - `render_scene_warm_p99_ms`: `136.398 ms -> 0.065 ms` (`< 16.0 ms`, **-99.95%**)
  - `gesture_to_place_p99_ms`: `154.816 ms -> 2.699 ms` (`< 16.0 ms`, **-98.3%**)
  - `startup_first_place_ms`: `450.988 ms -> 320.636 ms` (`< 600.0 ms`, **-28.9%**)
  - `daemon.scan`: `36.395 ms -> 22.53 ms` (**-38.1%**)
  - `daemon.tick`: p50 `2.464 ms -> 0.02 ms`, p99 `5.481 ms -> 2.55 ms` (**123x** p50 speedup); `process.cpu_avg_pct`: `17.68% -> 11.4%`; `stalls = 0`.
- **Parallel Execution Speedups**:
  - Unit tier (`6/6` pass including `sim_tests` `9/9`): `3.62 s` serial -> `1.82 s` `-Parallel` (**-49.8%**).
  - Safe Win32 live subset (`7/7` pass): `7.24 s` serial -> `4.13 s` `-Parallel` (**-42.9%**).
- **Live 5 s Synthetic Ring Benchmark (`1280x720 @ 30 fps`)**: `126` frames written, `0` dropped, `1.4%` CPU, `63.89 MB` working set, `0.07 MB` disk; `dump` produced `video-0.mp4` (`42,669` B), `frames-0.csv` (`2,845` B, `121` rows), `events.jsonl`, `meta.json`; `planor-rec analyze` decoded `120/121` frames cleanly.
- **Release Builds**: `planor.exe` (`7.25 s`, `0` warnings) & `planor-rec.exe` (`8.52 s`, `0` warnings).

## [0.2.0] - 2026-11-22

### Added
- **Test Isolation & Coexistence (`PLANOR_TEST_MODE=1`, `planor key`)**: Test daemons skip global hooks, raw input registration, `RegisterHotKey`, and modal note UI by default so test runs leave the real desktop daemon untouched. Only exclusive hotkey/signal tests (`clicking_a_peek_reveals_that_window`, `panic_hotkey`, `ctrl_break_restores_before_exit`) briefly pause/restart the real daemon.
- **Live IPC Introspection (`planor state --json`)**: Versioned (`schema_version = 1`) machine-readable snapshot of monitors, strips, active/parked tiles, expected rectangles, animating flag, and config reload generation, used by condition-based `wait_settled` waits.
- **Full QPC Profiling (`src/perf.rs`, `planor perf`)**: Zero-allocation `ArrayQueue` QPC spans matching `planor-rec` clock (`SpanKind`), per-thread/process CPU, working-set/private memory, GDI/USER/OS handle telemetry, Chrome/Perfetto JSON (`perf --trace`), 4 performance budgets (`budgets.all_pass`), and release benchmark (`perf_span_overhead_benchmark`).
- **Freeze Watchdog (`src/watchdog.rs`)**: 50 ms watchdog detecting stalls >= 300 ms, emitting `STALL` trace markers and diagnostic stall bundles (`perf.json`, `perf.trace.json`, `trace.tail.log`, optional ring video dump).
- **10-Second Rolling Video Ring-Buffer (`planor-rec ring`, `dump`, `ring-status`, `scripts/correlate.ps1`)**: Continuous low-cost wrapped 1 s MPEG-TS capture per monitor plus bounded in-memory frame/event rings; zero-reencode MP4 `dump` correlated on the QPC clock.
- **Win+W Diagnostic Bundles (`src/note.rs`)**: Win+W notes automatically save `<LOCALAPPDATA>\planor\bundles\<stamp>-<slug>\` with desktop PNG capture, `state.json`, `perf.json`, `perf.trace.json`, trace tail, and ring video dump.
- **Core Correctness & Verification**: Runtime strip/settle invariant checking (`src/core.rs`), 1000-sequence shrinking property test (`tests/prop_core.rs`), machine-readable mockup states spec (`docs/scroll_states_spec.json`), and pure gesture replayer (`src/replay.rs` + `tests/fixtures/gestures/`).
- **Fast Verification Ladder (`scripts/verify.ps1`, `scripts/test-map.json`, `scripts/digest.ps1`, `scripts/compare-runs.ps1`, `scripts/notes.ps1`, `scripts/rebuild-daemon.ps1`, `scripts/install-hooks.ps1`, `scripts/nightly-verify.ps1`)**: Direct test `.exe` execution, `-Tier unit|fast|smoke|live|all`, `-NoDisplaySwitch`, `-Cache`, `-Changed`, `-Perf`, cheap retries (`PLANOR_RECORD=1` only on retry), tracked Win+W notes, and pre-commit guardrails.

### Fixed
- **Trailing Empty Space After Unmaximize / Shrink (`src/core.rs`)**: Clamped `Strip::offset` via `self.scroll(0)` on `resize`, `maximize`, `fullscreen` exit, before the `reveal` early-return check, and in `fill_gaps` (`unmaximizing_or_shrinking_clamps_offset_no_empty_space_on_right`; tracked note `N-006`).
- **Large IPC Replies (`src/main.rs`)**: Multi-chunk `PipeServer` writes cleanly handle >64 KB `state --json` and `perf --trace` responses.
- **Digest Parsing (`scripts/digest.ps1`)**: Aligned 6-column `results.md` parsing (`Duration` + `Detail`), surfaced `STALL` lines, added `-Perf` and `-Bundle` views (`<= 20` lines), and added fallback parsing for `--nocapture` panic blocks.
- **Chrome Trace / Bundle IPC (`src/daemon.rs`, `src/main.rs`)**: Compacted `perf --trace` JSON serialization, raised client `read_frame` limit from 4 MB to 16 MB, defaulted `perf --bundle` trace window to 15 s, and printed `perf --trace -` cleanly to stdout.

### Verified Numbers (`v0.2.0`)
- Release `perf` span overhead (`perf_span_overhead_benchmark`): `2.1 ns/span` (disabled), `74.3 ns/span` (enabled).
- Unit tier speed: `5.28 s` (`v0.1.0` baseline `20261122-082006`) -> `3.47 s` (`20261122-085623`, `-34.2%`; `0.60 s` on warm `-Cache` hit with `4 cached`); `1.67 s` (`-Changed -Tier unit`).
- Test counts: `9/9` `planor` bin unit tests, `39` pass / `1` expected-ignored (`MOCKUP_DISAGREEMENT`) in `tests/core.rs`, `2/2` in `tests/prop_core.rs` (`1000` random sequences), `103/103` lib + `2/2` integration in `planor-rec` (including synthetic FFmpeg segment-wrap + concat MP4).
- Live coexistence & non-DXGI Win32 tests (inside `exebox-...` sandbox desktop): `state_json_matches_real_windows` (`1.19–1.27 s`, `0.69 s` cached), `stop_restores_exact_rect` (`0.61 s`), `tiles_stay_inside_work_area` (`1.51 s`), `stop_with_many_windows` (`1.96 s`); real daemon PID `14700` remained undisturbed.
- Bundle & digests: `perf --bundle` wrote `perf.json` (10.6 KB), `perf.trace.json` (7.1 KB), `state.json` (1.5 KB), `trace.tail.log`; `digest.ps1 -Bundle` rendered 7 lines; `digest.ps1 -Perf` rendered 15 lines.
- Environment-blocked inside `exebox-...` (run on interactive `WinSta0\Default` desktop for desktop verification): live DXGI `DuplicateOutput` (`0x80070005`, `planor-rec ring` / `scn_overlay`), WGC `render.sources` (`0x80070057`, `perf_spans_and_budgets`), `SetForegroundWindow` (`scn_focus`), plus `scn_display` / full ~7 min suite (skipped per constraints).
