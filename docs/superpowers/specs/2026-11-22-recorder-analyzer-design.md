# Planor recorder and analyzer: design

Status: built. This is the original design; `tools/planor-rec/README.md` describes what exists today (it adds
monitor-dropout handling with a `state` column in `frames-N.csv` and `capture` events, size/time limits and
`prune`, `review-pack`, test integration through `PLANOR_RECORD=1`, audio placement in `meta.json`, and the
documented limits).

## Intent and why

Planor is a tiling window manager whose bugs are visual: windows that arrive late, borders painted over the
taskbar, window interiors that never render, a monitor that goes black. Every check so far (unit tests, the
live scenario tests, log reading) trusts Planor's own state or Windows' answer to "where is this window?".
Both can be right while the screen is wrong. The user has watched broken screens while the tests passed.

The goal is for **the recorded screen to be the ground truth**. Every position Planor claims is checked against
pixels, automatically, in automated tests and in the user's own manual sessions, with no human and no LLM
watching. An LLM is a last resort for the few frames the pixel checks cannot explain, and only a Haiku
subagent is used for that (user rule, 2026-11-22).

## Goals

1. Record every monitor at full resolution, timestamped on one clock, during automated tests and manual use.
2. Record, on the same clock: mouse position, buttons and non-text keys (text keys only counted, never
   named), Planor's expected window rects, Windows' reported window rects and z-order, the user's microphone.
3. Analyze a recording offline with pixel maths only, and report numeric discrepancies:
   - **Position:** expected vs Windows-reported vs measured-in-video rect per window, difference in px.
   - **Lag:** time from a reported or expected move until the video shows it, in ms.
   - **Border placement:** Planor border colours must lie on the expected tile outline and never over the
     taskbar or outside the monitor's usable area.
   - **Interior not rendering:** a window Windows says is visible whose interior is black or flat.
   - **Monitor black or frozen:** whole-monitor luminance and frame-change checks.
   - **Frame pacing:** gaps between present times.
4. Say plainly when something cannot be measured (for example a dark window on a dark background). That is
   reported as `unmeasurable`, never as a pass.
5. Export suspicious moments as cropped PNGs plus the log lines around them, in a manifest a Haiku subagent
   can consume. The analyzer itself never calls an LLM.
6. Start and stop recording by hotkey and by command, add timeline markers by hotkey, and record the microphone
   so the user can dictate what they see, aligned to the same clock.

## Non-goals and anti-goals

- No cloud calls, no LLM inside the analyzer, no recording of typed text.
- Not a general screen recorder; it exists to verify Planor.
- Anti-goal: a pass that hides a failure. An unmeasurable window must never count as ok.
- Not replacing the existing scenario tests; they gain a recording and a final analyzer check.

## Constraints

- Windows 11, per-monitor DPI, three monitors (laptop 1920x1200, landscape 2560x1440, portrait 1440x2560),
  AMD GPU. Rust 2024, the same repo.
- FFmpeg 9.0.2 is installed (winget, Gyan build); no Python available.
- The existing screen capture in `tests/common/helpers.rs` uses DXGI desktop duplication and exposes
  `LastPresentTime` per frame. Reuse it.
- Planor already logs (diagnostics on) UTC wall-clock lines for focus, layout, events, input and placements to
  stderr. These move to a structured trace file on the QueryPerformanceCounter clock.
- The desktop background is set to solid dark blue RGB(0,0,90) (done 2026-11-22; original: solid black, saved in
  `%LOCALAPPDATA%\planor\wallpaper-backup.json`) so windows and gaps are easier to detect.

## Approaches considered

| | A. FFmpeg screen grab | B. Rust recorder on DXGI duplication | C. Windows Graphics Capture |
|---|---|---|---|
| Frame timing | approximate | exact present time per frame | exact |
| Reuse of existing code | none | the test `Capture` | none |
| Effort | lowest | moderate | higher |
| Weakness | late capture looks like lag | one more program | some windows capture black |

**Chosen by the user: B.** FFmpeg is used only to encode and decode.

## Design

### Crate layout

A new workspace member `tools/planor-rec` with a library (used by tests) and a binary:

```
planor-rec record  --out <dir>        start recording (foreground; Ctrl+C or hotkey stops)
planor-rec stop                       stop a background recording
planor-rec mark [text]                add a timeline marker
planor-rec analyze <dir>              produce report.json, report.md, suspects/
```

The recorder runs as a background process with global hotkeys (defaults, configurable):
`Ctrl+Alt+Shift+R` toggle record, `Ctrl+Alt+Shift+M` marker.

### Recording directory

```
<dir>/video-<monitor>.mp4     lossy H.264, CRF 16, constant 60 fps timeline
<dir>/frames-<monitor>.csv    per encoded frame: index, present_qpc_us, changed (0/1)
<dir>/audio.wav               microphone, 16 kHz mono, written only when mic recording is on
<dir>/events.jsonl            one JSON object per line, all on the QPC microsecond clock
<dir>/meta.json               monitors (device, rect, work area, dpi), taskbar rects, QPC frequency, git/version
```

`events.jsonl` kinds: `input` (mouse x,y, button, non-text key code), `os` (window id, class, visible rect,
z-index, cloaked, foreground, sampled at 60 Hz and on change), `expect` (Planor's target rect per window and
kind: tile, peek, fullscreen, parked, plus border colour), `planor` (focus, scene, placement), `mark`.

### Planor changes (diagnostics only)

- Replace the ad hoc `eprintln!` trace with a trace writer thread that emits `expect` and `planor` events as
  JSON lines to `%LOCALAPPDATA%\planor\trace.jsonl` using the QPC clock. Planor emits an `expect` record for
  every tile each time a scene is sent, including the animation target and duration so the analyzer knows when
  the window should have settled.
- No behavioural change when diagnostics are off.

### Analysis method (no LLM)

For each monitor, decode `video-<monitor>.mp4` through FFmpeg to raw frames and walk them with the event
stream.

1. **Measured rect.** For every expected rect that has settled (animation finished plus a margin), fit the
   window's outline in the frame: sample edge strength along the four sides of the expected rect and search a
   window of plus/minus 24 px for the offset with the best normalized edge score. The measured rect is the
   best fit. If no offset reaches a minimum contrast score, the rect is `unmeasurable`. Sentinel windows in
   automated tests have solid known colours and are measured exactly by colour boxes.
2. **Position check:** `|expected - os|` and `|os - measured|`, each side, fail above 2 px (configurable).
3. **Lag:** for each `os` or `expect` rect change, the first frame whose fitted rect is within tolerance of the
   new rect; latency is that frame's present time minus the event time. Fail above 50 ms, or never reached
   within 1 s.
4. **Border placement:** find pixels matching Planor's border colours (tolerance for lossy video), group them
   into rectangles, and require each to lie on an expected tile outline within 2 px and outside every taskbar
   rect and outside the monitor work area.
5. **Interior:** for each visible, uncloaked, un-minimized window, the interior mean luminance and standard
   deviation. Black or flat for longer than 300 ms is a fail; low contrast against the background is
   `unmeasurable`.
6. **Monitor black or frozen:** mean luminance near zero while windows should be visible, or no change between
   consecutive present times while rects or input are changing, for longer than 300 ms.
7. **Pacing:** present-time gaps above 100 ms.

### Output

- `report.json`: every check, per window and per timestamp, with expected, reported, measured, difference and
  verdict (`pass`, `fail`, `unmeasurable`).
- `report.md`: a short human summary sorted by severity, with timestamps and the nearest marker or dictated
  note.
- `suspects/`: cropped PNGs (the window rect plus margin, and the full monitor at low resolution) and
  `suspects.json` linking each to the report lines and the surrounding `events.jsonl` rows. A Haiku subagent
  reads only these, never the video.

### Test integration

In `tests/common/helpers.rs`, a `Recording` RAII guard starts the recorder (`PLANOR_RECORD=1` or per test) and
on drop stops it and runs the analyzer; a test may assert the report has no `fail`. Sentinels register their
colour so the analyzer can measure them exactly.

### Microphone

Audio is captured through WASAPI (or FFmpeg dshow as a fallback) into `audio.wav` only while mic recording is
enabled, started and stopped with the recording. A visible console line and a tray-less indicator tell the
user it is on. Timestamps use the same clock. Speech to text is not part of the first version: the audio is
kept and dictation timing is aligned to markers; transcription (whisper.cpp or Windows speech) is a later,
optional step.

## Calls made without the user (to check)

- Lossy video (CRF 16) is used for analysis, with tolerances sized for it, to keep file sizes manageable.
  If edge measurement proves unreliable, switch to lossless FFV1 for the analyzed monitor only.
- Text keys are counted, never named, so a recording cannot reveal what was typed.
- Hotkeys `Ctrl+Alt+Shift+R` and `Ctrl+Alt+Shift+M` (check against Planor's own `panic_exit`, which uses
  `Ctrl+Alt+Shift+Backspace`).
- Mic recording is opt-in per session and local only.
- Speech transcription is out of scope for version one.
- Thresholds (2 px, 50 ms, 300 ms) are defaults in a config file.

## Left to the builder

Exact edge-fit algorithm and scoring, CSV and JSON field names, FFmpeg command lines, thread layout of the
recorder, how the QPC clock is shared between the recorder process and the Planor daemon, and the report
formatting.
