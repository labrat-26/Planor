# Tray icon, native settings window, per-user install: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Planor gets a tray icon (Pause/Resume, Configure, Exit), a native theme-following settings window that replaces the TUI, a per-user Install/Uninstall, an autostart switch (only when installed), and hot reload of settings.

**Architecture:** The daemon owns the tray (own thread, hidden window) and talks to itself through the existing named-pipe `client()`. `planor settings` is a separate process: a Win32 window whose pages are generated from the existing settings model (moved out of `config_tui.rs`). Saves are debounced, written with `toml_edit`, then a `Reload` is sent; a daemon-side directory watcher reloads on hand edits.

**Tech Stack:** Rust 2024, `windows-sys` 0.61 (already used), `toml_edit`, `embed-resource` (new build-dependency, icon only). No GUI framework.

**Spec:** `docs/superpowers/specs/2026-11-22-tray-settings-install-design.md`

## Global Constraints
- Native Win32 + comctl32 v6 only; no web view, WinUI or installer. No admin rights.
- Config stays one file: `%APPDATA%\planor\planor.toml`. Saves must keep comments and layout (`toml_edit`) and validate with `crate::valid_value`.
- Install dir `%LOCALAPPDATA%\Planor\planor.exe`. Autostart = `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Planor` = `"<installed exe>" start`; only settable when the running exe is the installed one.
- Daemon is headless: no `println!` in `src/{backend,daemon,render,win32,perf,watchdog}.rs` or the new `tray.rs`/`config_watch.rs`; use `diag::trace`.
- Tray and file watcher must NOT start when `crate::test_mode()` is true (test daemons run beside the real one).
- Never block the daemon's tiling/hook threads from tray or watcher code; they run on their own threads.
- Manifest keeps PerMonitorV2 and adds Common Controls v6.
- Run tests through `scripts\verify.ps1` (it rebuilds and restarts the daemon first); journal each batch with `scripts\ai-journal.ps1`; never weaken a test; report only what was run.
- Commit after each task. Commit messages end with `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`.

## Review Focus
- Config file missing, empty or invalid TOML when the window opens: shows a banner, never overwrites the file with defaults.
- File edited by hand while the window is open: a save re-reads the file and changes only the touched key, so hand edits are not clobbered.
- Install while the target exe is in use or half-copied: copy to a temp name then rename; if the installed daemon runs, stop it first; on failure report the error and leave the previous install intact.
- Daemon not running when a setting is saved: save succeeds silently, no error dialog.
- Exe path with spaces or non-ASCII characters in the Run value: quoted and UTF-16 correct.
- Explorer restarts or DPI/monitor changes: tray icon reappears; window re-scales.

---

### Task 1: Settings model module and rules/hotkey helpers

**Files:**
- Create: `src/settings_model.rs` (moved from `src/config_tui.rs` lines 1-210: `Entry`, `Model`, helpers)
- Modify: `src/config_tui.rs` (imports `Model`/`Entry` from the new module; behaviour unchanged), `src/main.rs` (`mod settings_model;`)
- Test: `src/settings_model.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `settings_model::{Entry, Model}` with the existing methods (`load`, `from_text`, `in_section`, `value`, `set`, `parse_input`, `step`, `reset`, `save`) plus:
  - `Model::reload_from_disk(&mut self) -> Result<()>` re-reads `path` and replaces `doc`, keeping nothing pending.
  - `Model::rules(&self) -> Vec<Rule>`, `Model::set_rules(&mut self, rules: &[Rule]) -> std::result::Result<(), String>` where `pub struct Rule { pub matcher: RuleMatch, pub value: String, pub action: RuleAction }`, `pub enum RuleMatch { Process, Class }`, `pub enum RuleAction { Ignore, Float, Tile }` (valid combos mirror `parse` in `main.rs`).
  - `pub fn hotkey_valid(s: &str) -> bool` implementing the grammar of `input::bindings` (tokens `Alt|Ctrl|Shift|Win`, one key from `Left|Up|Right|Down|[|]|Backspace|<single char>`).
  - `Model::load_error(path:&Path) -> Option<String>`: `Some(message)` when the file exists but is not valid TOML.

- [ ] **Step 1: Write failing tests** in `settings_model.rs`: `hotkey_valid_accepts_all_example_hotkeys` (every value under `[hotkeys]` in `EXAMPLE` is valid), `hotkey_valid_rejects` (`""`, `"Alt+"`, `"Foo+A"`, `"Alt+Left+Right"` (two keys), `"Alt"` (no key)), `rules_round_trip_preserves_comments` (set one rule, `doc.to_string()` still contains the original `# ` comment lines and re-parses via `crate::parse` to the same rule), `load_error_reports_invalid_toml`, `reload_from_disk_sees_external_edit` (temp file, edit, reload, `value(i)` changes). Keep the 4 existing `config_tui` model tests passing by moving them with the code.
- [ ] **Step 2:** `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier unit -Only settings_model` -> FAIL (module missing).
- [ ] **Step 3:** Move the code, add the signatures above. `set_rules` rewrites the `[[rules]]` array of tables only (leave other items untouched); `reload_from_disk` and `load_error` use `std::fs` + `str::parse::<DocumentMut>`.
- [ ] **Step 4:** Same command -> PASS; also `-Tier unit` whole -> all pass, TUI tests still green.
- [ ] **Step 5:** Journal, then `git add src/settings_model.rs src/config_tui.rs src/main.rs && git commit -m "Move settings model out of the TUI; add rules, hotkey and error helpers"`.

### Task 2: Install, uninstall and autostart logic

**Files:**
- Create: `src/install.rs`
- Modify: `src/main.rs` (`mod install;`; the old `fn autostart` and `Command::Autostart` delegate to `install::set_autostart`)
- Test: `src/install.rs`

**Interfaces:**
- Produces:
  - `pub fn install_dir() -> Result<PathBuf>` = `%LOCALAPPDATA%\Planor`; `pub fn installed_exe() -> Result<PathBuf>` = `install_dir()/planor.exe`.
  - `pub fn is_installed_exe(running: &Path, installed: &Path) -> bool` (case-insensitive, canonical-or-lexical compare). `pub fn running_is_installed() -> bool`.
  - `pub fn run_value(exe: &Path) -> String` = `"\"<exe>\" start"`.
  - `pub fn set_autostart(on: bool) -> Result<()>` writes/removes the Run value for `installed_exe()`; returns `Err("not installed")` when `on` and `!installed_exe()?.exists()`.
  - `pub fn autostart_enabled() -> bool` (Run value present and equals `run_value(installed_exe)`).
  - `pub fn install() -> Result<PathBuf>`: stop the installed daemon if running (pipe `Command::Stop`, wait up to 6 s, else error), copy `current_exe` to `planor.exe.tmp`, rename over `planor.exe`, spawn `"<installed>" start` hidden, return the path.
  - `pub fn uninstall(delete_config: bool) -> Result<()>`: `set_autostart(false)`, stop daemon, remove install dir (schedule removal via `cmd /c ping -n 3 127.0.0.1 >nul & rmdir /s /q <dir>` if the running exe is inside it), optionally delete `%APPDATA%\planor`.

- [ ] **Step 1: Write failing tests:** `is_installed_exe_ignores_case_and_slashes`, `run_value_quotes_paths_with_spaces_and_unicode` (`C:\Users\Zoë Ünï\AppData\Local\Planor\planor.exe` -> exact quoted string), `set_autostart_on_requires_install` (uses `PLANOR_INSTALL_DIR` env override pointing at an empty temp dir -> `Err`), `install_copies_via_temp_and_keeps_old_on_failure` (override dir; make the destination read-only-locked by holding it open; assert old bytes intact and error returned). Add `PLANOR_INSTALL_DIR` as a test-only override read by `install_dir()`.
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only install` -> FAIL.
- [ ] **Step 3:** Implement. Registry code reuses the existing `windows-sys` Registry calls from the old `autostart`; do not duplicate them, move them here.
- [ ] **Step 4:** Run -> PASS. Also run `target\release\planor.exe autostart off` manually afterwards to confirm the CLI still works (report as run/not run).
- [ ] **Step 5:** Journal; commit `Add per-user install, uninstall and installed-only autostart`.

### Task 3: Theme helpers (dark mode, Mica, accent)

**Files:**
- Create: `src/theme.rs`; Modify: `src/main.rs` (`mod theme;`); add `windows-sys` features `Win32_UI_Shell`, `Win32_UI_Controls` (already), `Win32_System_Registry` (already) as needed.
- Test: `src/theme.rs`

**Interfaces:**
- Produces: `pub fn system_is_dark() -> bool` (reads `HKCU\...\Themes\Personalize\AppsUseLightTheme`), `pub fn parse_light_theme(value: Option<u32>) -> bool` (pure), `pub fn enable_app_dark_mode()` (loads `uxtheme.dll` ordinals 135 `SetPreferredAppMode(AllowDark)` and 136 `FlushMenuThemes`, no-op if missing), `pub fn apply_window(hwnd: HWND, dark: bool)` (sets `DWMWA_USE_IMMERSIVE_DARK_MODE`, `DWMWA_SYSTEMBACKDROP_TYPE`=Mica (2) when the build supports it, `DWMWA_BORDER_COLOR` default), `pub fn apply_control(hwnd: HWND, dark: bool)` (`SetWindowTheme` with `DarkMode_Explorer` / `Explorer`), `pub fn accent_color() -> u32` (DWM colorization as COLORREF), `pub struct Palette { bg, panel, text, subtext, border }` and `pub fn palette(dark: bool) -> Palette` (pure).

- [ ] **Step 1: Failing tests:** `parse_light_theme_defaults_to_light` (`None` -> true, `Some(0)` -> false, `Some(1)` -> true), `palette_text_contrasts_background` (WCAG contrast ratio >= 4.5 for text/bg and text/panel in both modes).
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only theme` -> FAIL.
- [ ] **Step 3:** Implement; every Win32/ordinal call is guarded so a failure degrades to the light/classic look, never panics.
- [ ] **Step 4:** Run -> PASS. **Step 5:** Journal; commit `Add theme helpers`.

### Task 4: App icon, resources, manifest

**Files:**
- Create: `scripts/make-icon.ps1` (renders the tiled-panes glyph with System.Drawing at 16/24/32/48/256 and packs PNG entries into `assets/planor.ico`), `assets/planor.ico`, `planor.rc` (`1 ICON "assets/planor.ico"`)
- Modify: `build.rs` (call `embed_resource::compile("planor.rc", embed_resource::NONE)` guarded `#[cfg(windows)]`, keep the existing manifest link args), `Cargo.toml` (`[build-dependencies] embed-resource = "3"`), `planor.manifest` (add `<dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>`, keep dpiAwareness)
- Test: `tests/core.rs` is untouched; verification below.

**Interfaces:**
- Produces: icon resource id `1` in `planor.exe` (`LoadImageW(GetModuleHandleW(null), 1 as PCWSTR, IMAGE_ICON, ..)`), used by Tasks 6-7. Helper `pub fn app_icon(size: i32) -> HICON` in `src/theme.rs`.

- [ ] **Step 1:** Run `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-icon.ps1`; check the file exists and begins with bytes `00 00 01 00 05 00` (5 images).
- [ ] **Step 2:** Add build wiring; `cargo build --release --bin planor` is blocked while the daemon runs, so use `cargo check --release --bin planor` then `scripts\rebuild-daemon.ps1`. If `embed-resource` cannot find `rc.exe`, stop and report; do not hand-roll a fallback.
- [ ] **Step 3:** Confirm the icon is embedded: `(New-Object System.Drawing.Icon "target\release\planor.exe")`-style extraction via `[System.Drawing.Icon]::ExtractAssociatedIcon` returns a non-default icon (report the check you ran).
- [ ] **Step 4:** Journal; commit `Add app icon, resource and Common Controls v6 manifest`.

### Task 5: Config file watcher (hot reload on hand edits)

**Files:**
- Create: `src/config_watch.rs`; Modify: `src/main.rs` (`mod config_watch;`), `src/daemon.rs` (`run()` starts it unless `test_mode()`)
- Test: `src/config_watch.rs`, `src/sim_tests.rs`

**Interfaces:**
- Produces: `pub struct Debounce { quiet: Duration }` with `pub fn new(quiet: Duration) -> Self`, `pub fn event(&mut self, now: Instant)`, `pub fn due(&mut self, now: Instant) -> bool` (true once, `quiet` after the last event); `pub fn spawn(path: PathBuf, on_change: impl Fn() + Send + 'static)` runs a thread using `FindFirstChangeNotificationW` on the file's directory (last-write filter), feeding `Debounce` (300 ms) and calling `on_change`; `daemon::run` passes a closure that calls `client(&pipe_name(), &Command::Reload)`.

- [ ] **Step 1: Failing tests:** `debounce_fires_once_after_quiet_period` (three events within 50 ms, `due` false until 300 ms after the last, true exactly once), `debounce_rearms_after_firing`.
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only config_watch` -> FAIL.
- [ ] **Step 3:** Implement. The watcher thread only reads the directory notification; it never touches the engine. Register the thread with `perf::register_thread("config-watch")`.
- [ ] **Step 4:** Add `sim_tests` case `config_reload_bumps_version`: write a changed `gap_px` through the fake backend's `load_config`, send `Command::Reload`, assert `config_version` incremented and `hot.gap_px` updated (this pins the reload path the watcher and the window both use).
- [ ] **Step 5:** Run `-Tier unit` -> all pass. Journal; commit `Reload config when planor.toml changes on disk`.

### Task 6: Tray icon and menu

**Files:**
- Create: `src/tray.rs`; Modify: `src/main.rs` (`mod tray;`), `src/daemon.rs` (`run()` calls `tray::spawn()` unless `test_mode()`), `scripts/install-hooks.ps1` (add `tray.rs`, `config_watch.rs` to the `println!` check list), `Cargo.toml` (`Win32_UI_Shell` feature if absent)
- Test: `src/tray.rs`

**Interfaces:**
- Consumes: `theme::{enable_app_dark_mode, app_icon}`, `client`, `Command`, `pipe_name`.
- Produces: `pub enum TrayAction { TogglePause, Configure, Exit }`; `pub struct MenuItem { pub id: u32, pub label: &'static str, pub action: Option<TrayAction> }`; `pub fn menu_model(paused: bool) -> Vec<MenuItem>` (labels: `Pause`/`Resume` toggle, `Configure...`, separator, `Exit`); `pub fn tooltip(paused: bool) -> String` (`"Planor"` / `"Planor (paused)"`); `pub fn run_action(action: TrayAction) -> Result<()>`; `pub fn spawn()`.
- Behaviour: own thread + hidden window; `Shell_NotifyIconW` add/modify; left or right click shows the menu at the cursor (`SetForegroundWindow` + `TrackPopupMenu` + `WM_NULL`); double-click = Configure; re-adds on the registered `TaskbarCreated` message; on menu open, `Command::State{json:true}` is queried and `"paused"` read from the reply to choose the label; `TogglePause` sends `Pause` or `Resume`; `Configure` spawns `current_exe settings`; `Exit` sends `Command::Stop`. Failures go to `diag::trace`.

- [ ] **Step 1: Failing tests:** `menu_model_toggles_pause_label`, `menu_model_has_configure_and_exit_last`, `tooltip_reflects_pause`, `paused_from_state_json` (`{"paused":true,...}` -> true, malformed -> false).
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only tray` -> FAIL.
- [ ] **Step 3:** Implement. Sim: `PLANOR_TEST_MODE=1` start must not create the window (assert via a `tray::spawn` early return covered by `spawn_skipped_in_test_mode` test using the env var).
- [ ] **Step 4:** `scripts\verify.ps1 -Tier unit` -> pass. Then live, manually and reported as such: `scripts\rebuild-daemon.ps1`, confirm the icon appears, Pause/Resume changes behaviour (`planor state --json` shows `paused`), Exit stops the process, and restarting Explorer (`Stop-Process explorer`) brings the icon back.
- [ ] **Step 5:** Journal; commit `Add tray icon with pause, configure and exit`.

### Task 7: Settings window: shell, navigation, generic controls, hot save

**Files:**
- Create: `src/settings_ui.rs`; Modify: `src/main.rs` (`mod settings_ui;`, `Command::Settings` variant, `planor settings` dispatch), Cargo feature flags as needed
- Test: `src/settings_ui.rs`

**Interfaces:**
- Consumes: `settings_model::{Model, Entry}`, `theme::*`, `client`, `Command::Reload`.
- Produces: `pub fn run(path: &Path) -> Result<()>` (modal message loop); `pub enum Control { Check, Number{float:bool}, Choice(&'static [&'static str]), Color, Hotkey, Text }`; `pub fn control_for(entry: &Entry) -> Control` (bool -> Check, int/float -> Number, `choices(key)` -> Choice, key ending `_color` -> Color, section `hotkeys` -> Hotkey, else Text); `pub struct SaveScheduler` with `new(quiet: Duration)`, `touch(&mut self, now: Instant)`, `take_due(&mut self, now: Instant) -> bool` (250 ms); `pub fn apply_change(model: &mut Model, i: usize, v: toml::Value) -> std::result::Result<(), String>` which first calls `model.reload_from_disk()` then `model.set`, so hand edits survive; `pub fn single_instance() -> Option<InstanceGuard>` (named mutex `Local\Planor.Settings`; a second launch finds and focuses the first window and exits).
- Window: left list of `model.sections` plus an `Install` page entry (Task 9 fills it); right pane scrolls the section's controls with label, control, grey help text and a `Reset` link per key; invalid input shows red inline text and is not saved; a banner at the top shows `Model::load_error` (Review Focus 1) and disables editing; saves call `Model::save` then `client(Reload)` ignoring connection errors. Re-theme on `WM_SETTINGCHANGE` with `"ImmersiveColorSet"`; handle `WM_DPICHANGED`.

- [ ] **Step 1: Failing tests:** `control_for_maps_every_example_entry` (every `Model::from_text(EXAMPLE)` entry yields a non-`Text` control except free strings; hotkeys -> Hotkey, `curve` -> Choice, `focused_color` -> Color), `save_scheduler_coalesces_bursts`, `apply_change_keeps_external_edit` (temp file; change key A on disk; `apply_change` key B; both present in the file), `invalid_file_blocks_editing` (broken TOML temp file: `load_error` is Some and the file bytes unchanged after calling the guarded save).
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only settings_ui` -> FAIL.
- [ ] **Step 3:** Implement the shell and generic controls (checkbox, edit + up-down spin, drop-down list, edit). Pages are rebuilt from `Model` so new example keys appear automatically.
- [ ] **Step 4:** Unit -> pass. Manual, reported as such: `target\release\planor.exe settings` shows the window in light and dark (toggle Windows theme while it is open), a toggle takes effect in the running daemon within about a second (`planor state --json` `config_version` increases).
- [ ] **Step 5:** Journal; commit `Add native settings window`.

### Task 8: Color, hotkey and rules controls

**Files:** Modify `src/settings_ui.rs`; Test `src/settings_ui.rs`

**Interfaces:**
- Produces: `pub fn hex_to_colorref(s: &str) -> Option<u32>`, `pub fn colorref_to_hex(c: u32) -> String` (`#RRGGBB` uppercase); `pub fn hotkey_text(ctrl: bool, alt: bool, shift: bool, win: bool, vk: u32) -> Option<String>` (None for a vk outside the supported grammar; output always satisfies `settings_model::hotkey_valid`, modifiers in the order Ctrl, Alt, Shift, Win followed by the key; the key names are `Left|Up|Right|Down|[|]|Backspace` or one uppercase character); a swatch button that owner-draws the color and opens `ChooseColorW`; a hotkey box that captures the next chord (Esc cancels, Backspace alone clears to default); a rules list view with Add/Remove/Edit using `Model::rules`/`set_rules` and a dialog of process-or-class, value, action.

- [ ] **Step 1: Failing tests:** `hex_colorref_round_trip` (`#3B82F6`), `hex_to_colorref_rejects_bad_input` (`"3B82F6"`, `"#GGGGGG"`, `"#FFF"`), `hotkey_text_matches_example_style` (Alt+Left -> `"Alt+Left"`, Ctrl+Alt+Shift+Backspace -> `"Ctrl+Alt+Shift+Backspace"`), `hotkey_text_rejects_unsupported_keys` (F5 -> None), `hotkey_text_always_valid` (loop over supported vks and modifier combinations; `hotkey_valid` true).
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only settings_ui` -> FAIL.
- [ ] **Step 3:** Implement the three controls; the swatch and rules dialog are themed with `theme::apply_control`.
- [ ] **Step 4:** Unit -> pass. Manual, reported as such: change `focused_color` via the picker and see the border recolor live; record a new `focus_left` hotkey and confirm the daemon rebinds it (`input::bindings` runs on reload); add an `ignore` rule and confirm it lands as `[[rules]]` in the file with comments intact.
- [ ] **Step 5:** Journal; commit `Add color, hotkey and rules editors`.

### Task 9: Install page

**Files:** Modify `src/settings_ui.rs`; Test `src/settings_ui.rs`

**Interfaces:**
- Consumes: `install::{install, uninstall, installed_exe, running_is_installed, autostart_enabled, set_autostart}`.
- Produces: `pub struct InstallView { pub installed: bool, pub running_installed: bool, pub autostart: bool }` and `pub fn install_view() -> InstallView`; `pub fn button_states(v: &InstallView) -> (install_enabled: bool, uninstall_enabled: bool, autostart_enabled: bool, note: &'static str)` pure: autostart enabled only when `running_installed`; note explains `"Install Planor to enable autostart."` when not.
- Page: shows the install path, Install/Reinstall button (runs `install::install` on a worker thread, then offers to close this window because the installed copy took over), Uninstall (confirm; checkbox `Also delete my settings`, default off), Autostart switch bound to `set_autostart`; errors shown inline.

- [ ] **Step 1: Failing tests:** `autostart_disabled_until_running_from_install` (`running_installed:false` -> disabled and note set), `uninstall_enabled_only_when_installed`, `install_button_label_reinstalls` (installed -> "Reinstall").
- [ ] **Step 2:** `scripts\verify.ps1 -Tier unit -Only settings_ui` -> FAIL.
- [ ] **Step 3:** Implement. Long operations never run on the UI thread.
- [ ] **Step 4:** Unit -> pass. Manual, reported as such: from `target\release`, the autostart switch is greyed; click Install, the installed copy runs (`Get-Process planor` path under `%LOCALAPPDATA%\Planor`), settings opened from its tray now allow autostart, the Run value appears, Uninstall removes dir and Run value.
- [ ] **Step 5:** Journal; commit `Add install page with autostart`.

### Task 10: Remove the TUI, wire commands, docs, full verification

**Files:**
- Delete: `src/config_tui.rs`; Modify: `Cargo.toml` (remove `ratatui`), `src/main.rs` (remove `mod config_tui`; `Command::Config` -> `settings_ui::run(&folder("APPDATA")?.join("planor.toml"))`; doc comment "Open the settings window"), `CLAUDE.md` (replace the `planor config` TUI paragraph with the tray/settings/install description, new modules, hot reload), `CHANGELOG.md` (entry), `scripts/test-map.json` (add patterns: `^src/(tray|install|theme|settings_ui|settings_model|config_watch)\.rs$` -> unit targets `--bin planor (other)`, `--bin planor (sim_tests)`), `scripts/install-hooks.ps1` if the hook list needs the new files (done in Task 6; confirm)
- Test: whole suite

- [ ] **Step 1:** Delete the TUI and its dependency; move any still-useful TUI tests already ported in Task 1; confirm no remaining reference: Grep `config_tui|ratatui|crossterm` returns nothing under `src/`, `Cargo.toml`, `CLAUDE.md`.
- [ ] **Step 2:** `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify.ps1 -Tier unit` -> all green, exact counts recorded.
- [ ] **Step 3:** `scripts\verify.ps1 -Tier fast -Perf` -> green (the daemon starts in test mode, so no tray, confirming isolation). Report any flake by name (`drag_to_portrait_monitor` is known flaky).
- [ ] **Step 4:** `scripts\rebuild-daemon.ps1`; run through the manual checklist from Tasks 6-9 once end to end and note each item as run or not run.
- [ ] **Step 5:** Journal (`-Status done`); commit `Remove the TUI; settings window, tray and install complete`.
