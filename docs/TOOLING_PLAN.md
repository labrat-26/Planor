# Testing, debugging and profiling tooling: plan

Goal: make it fast and cheap for agents (and the user) to test Planor, find out why something failed, and measure
performance, without stopping the user's daemon, without raw-log dumps, and without trusting an agent's word.

Status legend: DONE, NEXT (queued for the Sonnet agent), LATER. Owners: the user decides; Sonnet implements; every item is
verified by the lead (re-run the tests, read the diff) before it counts as done.

## 0. Already in place (DONE)

| What | Where |
|---|---|
| Git repository, baseline commit, `.gitignore` | repo root |
| `scripts/verify.ps1`: safe daemon stop/restart, one test at a time, failures re-run (PASS / FLAKY / FAIL), table in `build/verify/latest.md`, logs per run | `scripts/` |
| `scripts/digest.ps1`: shrinks verify results, test logs, recorder reports and traces to a few lines | `scripts/` |
| `CLAUDE.md`: how to run tests, the rules, where to look | repo root |
| Recorder and analyzer (screen video, input events, position checks, no LLM in the analyzer) | `tools/planor-rec/` |
| Memory notes (Haiku for media analysis, cheapest agent first) | user memory |

## 1. Measure first (NEXT, small, do before anything else)

1. `verify.ps1` records the duration of every test run and prints the ten slowest.
2. `results.json` keeps duration, verdict, runs and the first failure line, so a later script can compare runs.
3. Acceptance: one full run produces a table with durations; the slowest tests are named. Without this nothing in
   section 5 is allowed to claim a speed-up.

## 2. Test environment (NEXT)

| # | Item | Acceptance |
|---|---|---|
| 2.1 | Test mode for the daemon (`PLANOR_TEST_MODE=1`, or implied by `PLANOR_INSTANCE`): no global hotkeys, no low-level input hooks reinstall thread, no Win+W box. | A test daemon starts while the user's real `planor.exe` is running; `Daemon::start` no longer refuses. The real daemon is unaffected (verify with its pid and a hotkey). |
| 2.2 | Failed runs keep evidence: when `PLANOR_KEEP_ARTIFACTS=<dir>` is set (verify.ps1 already sets it), `tests/common/helpers.rs` copies the daemon trace, the recorder report and suspect images of a failed test into `<dir>/<test>/`. | A deliberately failing test leaves its trace in the verify folder; passing tests leave nothing. |
| 2.3 | Clean surface for screen tests: the recorder backdrop stays (done for recorded tests); document that Windows only duplicates the interactive desktop, so a second desktop is not possible and a VM is the only full isolation. Decision on a VM stays with the user. | Short section in `docs/TEST_PLAN.md`; no code. |

## 3. Observability: state dump, debug checks, function-level tests (NEXT)

| # | Item | Acceptance |
|---|---|---|
| 3.1 | `planor state --json`: monitors, strips (columns, members, active, roll fraction), tiles with expected rects, parked windows, pinned, orphans/homes, config version, uptime. | Command works on a running daemon; a test compares it with the real window rects; schema documented in `docs/`. |
| 3.2 | Invariant checks in the daemon (`debug_assert!` in debug builds, a `INVARIANT <name> <details>` trace line in release when diagnostics are on): no gap on a monitor with windows, no overlap between settled tiles, every tile inside its monitor area, one active member per group, no window left parked after a settle. | A unit test breaks each invariant on purpose and sees the line; a normal live run emits none. |
| 3.3 | Function-level tests for the pure parts of `src/core.rs`: layout for 0..6 windows and groups of 2..4 in landscape and portrait, roll fraction, drop-zone classification (outer 20% beside, middle 60% join), insert position after the dropped-on window, pull-out, close, fill_gaps, cross-monitor insert rule. Each state in the mockup canvas "Planor scroll states" gets at least one test. | New tests in `tests/core.rs`, all pass; each mockup state is named in a test comment. Where the code disagrees with the mockup the test is written for the mockup and listed as a failing test with the owner's decision pending. |
| 3.4 | Replay of recorded input (`events.jsonl` and `PAD` trace lines) into the gesture code without hardware. | A recorded three-finger scroll replayed in a unit test gives the same scroll offsets as the live run within a stated tolerance. |

## 4. Profiling (NEXT, after 3.1 because `state` and `perf` share the command plumbing)

1. Timing spans around: daemon tick, `layout`, renderer scene handling (hosts, sources, frames), `win32::place`, `draw`, input
   hook callbacks, and the gesture-to-`SetWindowPos` path.
2. Spans live in an in-memory ring; a `SLOW <span> <ms>` trace line is written only above a threshold (as today).
3. `planor perf`: p50, p99, max and count per span since start; `planor perf --reset`.
4. `planor perf --trace <file>`: Perfetto/Chrome trace JSON of the last N seconds (opt in).
5. Join with the recorder: spans use the same QPC clock as the recorder events, so a frame gap can be matched to the span.
6. Budgets checked by `verify.ps1 -Perf`: first placement after startup under 600 ms, scene handling under 16 ms once
   warm, gesture path under 16 ms, hook callbacks under 5 ms. Numbers kept per run in `results.json`.
7. Cost: measure the overhead of spans on and off before enabling them by default; report the number.
8. Not covered: how much touchpad lag is the pad driver. Spans show Planor's share only.
9. `digest.ps1 -Perf` prints the top spans and any budget breach in under 20 lines.

## 5. Faster testing (NEXT, each item must show a measured gain from section 1)

| # | Item | Idea |
|---|---|---|
| 5.1 | Replace fixed sleeps with waits | Convert `thread::sleep` in the live tests to `wait_until(condition, timeout)`; keep a sleep only where time is the thing under test. |
| 5.2 | Run test programs directly | Build once, then run each test binary with `--exact`; no `cargo` start per test. |
| 5.3 | Animations off for logic tests | Test config without animation; animation and timing tests keep it. |
| 5.4 | Three tiers | `-Tier unit` (seconds), `-Tier smoke` (about a dozen key live tests, target about 1 minute), `-Tier all`. |
| 5.5 | Select by change | A map of source file to tests; `-Changed` runs only the tests that map to files changed since the last passing run (uses git). |
| 5.6 | Skip unchanged | `verify.ps1 -Cache`: remember pass per test with a fingerprint of the relevant sources; skip when nothing changed. A failing or flaky test is never skipped. |
| 5.7 | Cheap retries | Re-runs of a failure turn recording on; first runs do not record. |
| 5.8 | Shorten long tests | Split `scn_display` into a 3 s smoke version and the 15 s full version. |
| 5.9 | Parallel runs | Last, and only for tests on different monitors that send no keys and share no capture; needs 2.1. Decide after 5.1 to 5.8 whether it is worth the risk. |

Targets (to be confirmed or revised by section 1): smoke tier under about 1 minute; full run clearly under the current
about 7 minutes. These are targets, not measurements.

## 6. Order of work and agents

1. Wait for the Haiku agent (compute_visual regression, display and drag flakes) to finish; the lead verifies and
   commits its result.
2. Sonnet agent A: sections 1, 2 and 5 (they all touch `scripts/verify.ps1` and `tests/common/helpers.rs`).
3. Sonnet agent B: sections 3 and 4 after agent A is merged (they touch `src/`). Splitting avoids two agents editing the
   same files.
4. Each agent: small commits with the task in the message; run `scripts\verify.ps1 -Tier unit` before and after; report
   exact numbers; list what was not verified.
5. The lead re-runs `verify.ps1` (smoke and full), reads the diff, and only then marks an item DONE here.
6. Escalation: Haiku first for mechanical fixes; after two failed attempts move up a tier (user rule).

## 7. Risks and open decisions

- A test mode that skips hooks means those code paths are not covered by live tests; keep one live test of the hooks
  that runs only when the real daemon is stopped.
- Replay and state dump freeze formats; version them in the output.
- Parallel tests can hide interference as flakes; keep the serial run as the reference.
- Decision for the user: a virtual machine for fully clean screen tests (heavier setup, true isolation).
- Decision for the user: which mockup disagreements with the code become code changes (listed by 3.3).

## 8. Definition of done for this whole plan

- `verify.ps1 -Tier smoke` passes in about a minute with the user's daemon running.
- A failed test always leaves a trace, a digest and (when recorded) frames, and `digest.ps1` explains it in under 20 lines.
- `planor state --json` and `planor perf` exist and are used by at least one test each.
- Every state in the mockup canvas has a core test, and the pass/fail list is committed.

## 9. Further improvements (added after the first plan)

Priority: A = cheap and immediate, B = valuable, needs design, C = large, needs the owner's go-ahead.

| # | Item | Pri | Acceptance |
|---|---|---|---|
| 9.1 | `docs/ARCHITECTURE.md` for agents: threads (OS events, daemon 32 ms tick, render), tile rect versus OS rect, parked windows at the rightmost monitor edge + 1024 px, DWM stand-ins, overlay holes (SetWindowRgn, 200 ms poll), hook reinstall every 4 s, gesture pipeline, config merge (embedded example + `%APPDATA%\planor\planor.toml`), trace line glossary, how recovery works. | A | A new agent can answer five questions about the design from the doc alone; every claim is checked against the code. |
| 9.2 | Freeze watchdog: a thread that notices when the daemon tick or the renderer makes no progress for 300 ms and writes a `STALL <thread> <ms>` trace line with the last span and lock/wait it was in. | A | A test that blocks the tick on purpose produces the line; no line in a normal run. Overhead measured. |
| 9.3 | Randomised layout tests over `src/core.rs`: random sequences of open, close, move, resize, scroll, group, pull-out and monitor changes (landscape and portrait), invariants from 3.2 checked after every step, failing sequences shrunk and printed as a replayable list. | A | Runs in the unit tier in seconds; a deliberately broken layout rule is caught within 1000 sequences. |
| 9.4 | Brief template for delegated tasks (files, acceptance test, exact run command, escalation rule, "report what you did not verify"). Stored in `docs/` and referenced from `CLAUDE.md`. | A | Template exists; the next delegated task uses it. |
| 9.5 | Win+W attaches context: with each saved note, write the last N seconds of trace, a `planor state --json` dump and a screenshot into a folder next to the note. | B | A note produces the bundle; the digest script can print a bundle in under 20 lines. Needs 3.1. |
| 9.6 | Notes become tracked items: ID, status, linked test; a fixed note without a test is not done. `docs/NOTES_OPEN.md` is generated from that list. GitLab issues only if the owner adds a remote (global rule: use `glab`). | B | Every open note has an ID; each closed note names its test. |
| 9.7 | Golden gestures: the owner records a few real touchpad gestures; the streams are kept and replayed as tests (uses 3.4). | B | Replays reproduce the recorded scroll and move results within the stated tolerance. |
| 9.8 | Mockups as the spec: export the state-atlas coordinates to a data file read by the core tests (percent based, not pixels). | B | A mockup state and its core test cannot disagree silently; a mismatch fails a test. |
| 9.9 | Nightly full `verify.ps1` run on a schedule with the results table kept per night; pre-commit hook runs the unit tier. | B | A table appears each morning; a failing unit test blocks a commit. |
| 9.10 | Release hygiene: versions and tags per milestone, changelog (global rule: versioned milestones). | B | First tag and changelog exist. |
| 9.11 | Fake window backend: put window operations (list, move, focus, monitors, z-order) behind an interface; tests plug in a simulated desktop so the daemon logic (cross-monitor moves, unplug, orientation change, groups) runs fast and without the real desktop. | C | The `scn_display` scenarios and the three-monitor rig states run against the simulation in seconds; the live tests remain as a thin end-to-end layer. Owner must approve the refactor first. |
| 9.12 | Ring-buffer screen capture that always keeps the last 10 s, saved on Win+W or on a stall. | C | Cost (CPU, memory, disk) measured and acceptable before it is on by default; owner decides. |

Order: 9.1, 9.4, 9.2, 9.3 go to Sonnet agent A with section 1; 9.5 to 9.10 after section 3; 9.11 and 9.12 only after the
owner approves them. The same verification rule applies: the lead re-runs and reads the diff before an item is marked done.
