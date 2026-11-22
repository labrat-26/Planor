# Planor: notes for agents

Planor is a Windows 11 headless scrolling tiling window manager (Rust 2024). Crate root: `src/` (daemon, core layout,
render overlay, win32), `tests/` (unit `core.rs` + live scenario tests), `tools/planor-rec/` (screen recorder and
analyzer), `docs/` (plans, results, notes).

## Run tests: always through the script

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier unit            # seconds; core, prop_core, sim_tests, lib/bin, planor-rec
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier unit -Parallel  # parallel unit targets (MaxJobs=4 default)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier fast -Perf      # unit + 4 quick live smoke checks + budgets
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier smoke -Perf     # unit + 1 test per scenario (-NoDisplaySwitch skips DisplaySwitch)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Only scn_drag        # filter by file or test name (supports comma-separated tokens)
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Changed -Tier unit   # run only tests mapped in scripts\test-map.json
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier smoke -Cache    # skip unchanged passing tests via SHA-256 cache
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\clean-sessions.ps1 -KeepVerify 3 # prune stale verify/stall/temp artifacts safely
```

Test daemons default to `PLANOR_TEST_MODE=1` (skipping global hotkeys, LL hooks reinstall, raw input, and Win+W dialogs) and use PID-scoped `PlanorSentinel_p<pid>` window rules, so normal live tests run cleanly alongside the user's real `planor.exe` and parallel-safe live batches (`-Parallel`). `verify.ps1` only stops/restarts `target\release\planor.exe` around the 3 exclusive tests (`scn_taskbar::clicking_a_peek_reveals_that_window`, `scn_exit::panic_hotkey`, `scn_exit::ctrl_break_restores_before_exit`). Note: `scripts\verify.ps1` runs serial by default as the reference; `-Parallel` parallelizes unit targets and an explicit allowlist of non-key, non-cursor, non-DXGI live tests before running remaining tests serially.

To rebuild and restart the real daemon cleanly:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\rebuild-daemon.ps1
```

Manual stop/start (if ever needed by hand): `target\release\planor.exe stop` then
`Start-Process -FilePath C:\Users\rmand\Projects\PlanorV2\target\release\planor.exe -ArgumentList "start" -WindowStyle Hidden`
and confirm one `planor.exe` runs. The release exe cannot be rebuilt while the daemon runs.

Live tests (`--ignored --test-threads=1`) use the real desktop and DXGI capture: do not use the machine while they run,
and expect other windows to interfere. Failures re-run with `PLANOR_RECORD=1` by default and save trace/digest/recording evidence under `build\verify\<stamp>\<test>\`.

## Rules

- **Every test run first rebuilds and restarts the binary** (owner rule): `scripts\verify.ps1` runs `scripts\rebuild-daemon.ps1` before
  any tier and aborts if the daemon does not come back. Use `-NoRestart` only when the owner asks. When running a test outside
  `verify.ps1`, run `scripts\rebuild-daemon.ps1` first.
- Never weaken or delete a test to make it pass. Change an assertion only when it is provably stale versus the intended
  behaviour, and say which one and why.
- Report only what you ran. "Not verified" is a valid and required answer. Give exact counts and failure messages.
- Use git: commit before risky changes, `git diff` / `git bisect` to find a regression. Do not rewrite history.
- Put debugging output behind `diag::trace` (writes `%LOCALAPPDATA%\planor\trace.log` when diagnostics are on); do not
  add `println!` to the daemon (it is headless).
- The analyzer in `tools/planor-rec` must never call an LLM. LLM review of flagged frames goes to a Haiku subagent.
- Wallpaper must stay RGB(0,0,90): the recorder classifies pixels against it.

## Known state & references

- Intended behaviour & schemas: `tests/core.rs` (`scroll_states_spec.json`), `docs/TEST_PLAN.md`, `docs/STATE_SCHEMA.md`,
  `docs/MOCKUP_DECISIONS.md`, `docs/ARCHITECTURE.md`, and `docs/TASK_BRIEF_TEMPLATE.md` (for delegated agent briefs).
- Tracked user notes live in `docs/notes.json` (regenerated into `docs/NOTES_OPEN.md` via `scripts/notes.ps1 -Generate`,
  verified via `scripts/notes.ps1 -Check`). Win+W saves a diagnostic bundle under `%LOCALAPPDATA%\planor\bundles\<stamp>-<slug>\`
  (note, screenshot PNG, `state.json`, `perf.json`, `perf.trace.json`, trace tail, plus optional ring video dump).
- Known flaky: `drag_to_portrait_monitor` (passes alone). Check `build\verify\latest.md` for the current table.
- Pre-commit hook (`scripts/install-hooks.ps1`): checks no `println!` in `src/{backend,daemon,render,win32,perf,watchdog}.rs`,
  runs `core`, `prop_core`, and simulated daemon tests (`sim_tests`), and checks `notes.ps1 -Check`.

## Profiling, state & ring capture

- `planor config`: terminal UI for `%APPDATA%\planor\planor.toml` (help text from `planor.example.toml`, same validation as the
  daemon, comment-preserving save, tells the daemon to reload). Code: `src/config_tui.rs`.
- `planor gestures record <name> [--seconds 15] [--out f]`: capture real touchpad frames into an `events.jsonl` fixture; copy it to
  `tests/fixtures/gestures/recorded/`, where `sim_tests::recorded_gestures_replay_on_landscape_and_portrait` replays it on both
  orientations.
- `planor state --json`: dump live monitors, strips, active/parked tiles, expected rects, and config version.
- `planor perf [--json | --trace <file> | --bundle <dir> | --reset]`: QPC span latency percentiles (p50/p95/p99/max),
  CPU/RAM/GDI/USER/handles, GPU (`gpu.draw` GPU time per overlay draw, engine %, VRAM, DWM compose fps vs refresh Hz), Chrome/Perfetto JSON traces, and budgets (`startup <600ms`, `render.scene <16ms`,
  `gesture_to_place <16ms`, `hook <5ms`). Watchdog logs `STALL <thread> <ms>` and saves `%LOCALAPPDATA%\planor\stalls\`.
- Trace lines: `EXPECT`, `PLACE`, `SCENE`, `FG`, `SLOW *`, `STALL *`, `INVARIANT *`, `PAD`.
- Rolling ring capture: `planor-rec ring [--seconds 10 --fps 30 --source dxgi|synthetic]`, `planor-rec ring-status`,
  `planor-rec dump --out <dir>`, `planor-rec stop --pipe ring`. Correlate video failures with QPC spans via `scripts\correlate.ps1 -Dir <dir>`.

## Read data through the digest, not raw logs

Raw traces, recorder reports and cargo logs are huge. Shrink them first (scripts run with
`powershell -NoProfile -ExecutionPolicy Bypass -File scripts\<name>.ps1`, since script execution is disabled by default):

```powershell
scripts\digest.ps1                    # latest verify results: failures, flakes, perf budgets
scripts\digest.ps1 -Perf [perf.json]  # OS counters, budget verdicts, and top spans (<= 20 lines)
scripts\digest.ps1 -Bundle <dir>      # Win+W note bundle or watchdog stall bundle (<= 20 lines)
scripts\digest.ps1 -Log <test log>    # failing assertion and context
scripts\digest.ps1 -Rec <rec dir>     # recorder findings grouped by kind with counts
scripts\digest.ps1 -Trace <trace>     # scene-to-first-PLACE latency, STALL/SLOW lines collapsed, notes
scripts\compare-runs.ps1              # delta between two results.json runs + p99 perf shifts
```

Add `-Max N` to cap the output. Open a raw file only for the one thing the digest points at.
