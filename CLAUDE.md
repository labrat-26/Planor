# Planor: notes for agents

Planor is a Windows 11 headless scrolling tiling window manager (Rust 2024). Crate root: `src/` (daemon, core layout,
render overlay, win32), `tests/` (unit `core.rs` + live scenario tests), `tools/planor-rec/` (screen recorder and
analyzer), `docs/` (plans, results, notes).

## Run tests: always through the script

```powershell
.\scripts\verify.ps1                  # unit + every live test, failures re-run (FLAKY vs FAIL), table in build\verify\latest.md
.\scripts\verify.ps1 -Tier unit       # fast, does not touch the user's daemon
.\scripts\verify.ps1 -Only scn_drag   # one live test file or test name
.\scripts\verify.ps1 -Record          # PLANOR_RECORD=1: screen-record and analyze the wired tests
```

The script stops the user's real `planor.exe` for live tests and restarts it at the end, even on failure. If you must do
it by hand: `target\release\planor.exe stop` ... then restart with exactly
`Start-Process -FilePath C:\Users\rmand\Projects\PlanorV2\target\release\planor.exe -ArgumentList "start" -WindowStyle Hidden`
and confirm exactly one `planor.exe` runs. The release exe cannot be rebuilt while the daemon runs.

Live tests (`--ignored --test-threads=1`) use the real desktop and DXGI capture: do not use the machine while they run,
and expect other windows to interfere. A test that passes alone and fails in the suite is a flake or leftover state: say so.

## Rules

- Never weaken or delete a test to make it pass. Change an assertion only when it is provably stale versus the intended
  behaviour, and say which one and why.
- Report only what you ran. "Not verified" is a valid and required answer. Give exact counts and failure messages.
- Use git: commit before risky changes, `git diff` / `git bisect` to find a regression. Do not rewrite history.
- Put debugging output behind `diag::trace` (writes `%LOCALAPPDATA%\planor\trace.log` when diagnostics are on); do not
  add `println!` to the daemon (it is headless).
- The analyzer in `tools/planor-rec` must never call an LLM. LLM review of flagged frames goes to a Haiku subagent.
- Wallpaper must stay RGB(0,0,90): the recorder classifies pixels against it.

## Known state

- Intended behaviour is described in `tests/core.rs` (unit tests), `docs/TEST_PLAN.md`, and the mockup canvas
  "Planor scroll states" (landscape/portrait animated states).
- Known flaky: `drag_to_portrait_monitor` (passes alone). Check `build\verify\latest.md` for the current table.
- Open user notes: `docs/NOTES_OPEN.md`. The user dictates new ones with Win+W; they land as `NOTE` lines in
  `%LOCALAPPDATA%\planor\trace.log`.

## Where to look when something is wrong

- Trace lines: `EXPECT` (what Planor claims a tile rect should be), `PLACE` (actual window moves), `SCENE`, `FG`, `SLOW *`
  (anything over 100 ms), `PAD` (touchpad). `planor state` (when available) dumps strips and tiles.
- Recorder: `tools/planor-rec/README.md`. Reports in `%TEMP%\planor-rec-*\report.md`.
