# Tray icon, native settings window, per-user install

Date: 2026-11-22. Status: awaiting owner approval.

## Intent
Planor is headless. The owner wants it to look and behave like a normal Windows app: an icon in the notification
area (Pause / Configure / Exit), a native-looking, theme-following settings window that replaces the terminal UI, a
per-user install, and autostart. Changes made in the window apply to the running daemon immediately.

## Goals
- Tray icon owned by the daemon: left/right click opens a menu with Pause/Resume (label toggles), Configure, Exit.
  Double-click = Configure. Tray re-adds itself when Explorer restarts (`TaskbarCreated`). Tooltip shows paused state.
- App icon (tiled-panes glyph) as a multi-size `.ico`, embedded in the exe (build.rs resource) and used by the tray
  and window.
- `planor settings` (separate process, launched by the tray) opens a native Win32 window: left nav of sections
  (from `planor.example.toml`), right pane of controls per key, help text under each control, Reset-to-default per
  key. Controls: checkbox (bool), spin/edit (numbers), combobox (known choices), color swatch + hex edit (colors,
  swatch opens the system color chooser), hotkey edit (hotkeys), list editor for `[[rules]]`.
- Follows the system theme (light/dark) live, including title bar, Mica backdrop where available (Win11), system
  accent color and per-monitor DPI.
- Install page/button: copies the running exe to `%LOCALAPPDATA%\Planor\planor.exe`, starts the installed copy,
  stops the source daemon. Uninstall button reverses it (stops daemon, removes Run entry and install dir; asks
  whether to also delete `planor.toml`).
- Autostart toggle writes/removes `HKCU\...\Run\Planor` pointing at the installed exe. Disabled (greyed, with an
  explanation) unless the running exe is the installed one. Uninstall removes it.
- Config stays in `%APPDATA%\planor\planor.toml`, the single file. Saves go through `toml_edit` so comments and
  layout survive, validated with the daemon's own `valid_value`.
- Hot reload: the window saves each change (debounced ~250 ms) and sends `Reload`. The daemon also watches the file
  (directory change notification, debounced) so hand edits reload too.
- Delete the TUI: `src/config_tui.rs`, `ratatui` (and crossterm) dependency. `planor config` now launches the
  settings window. The reusable model (defaults, help, sections, validation, comment-preserving save) moves to a
  new module shared by the window.

## Non-goals
- No web view, WinUI/XAML, or new GUI framework. No installer (MSI/MSIX), no auto-update, no admin rights.
- No change to tiling behaviour, gestures or the overlay renderer.
- No cloud/sync of settings.

## Anti-goals (failure even if it "works")
- The settings window freezing or crashing the daemon (hence separate process).
- Unreadable dark mode (black-on-dark text, white flashes, light edit boxes).
- Autostart pointing at a `target\release` build that gets deleted/rebuilt.
- A save that drops the user's comments or reorders their file.
- The tray adding a `println!` or any blocking work on the daemon's tiling/hook threads.

## Constraints
- Rust 2024, existing crates (`windows-sys` / `windows`). Add only needed feature flags (`Win32_UI_Shell`,
  `Win32_UI_Controls`, `Win32_System_Registry` already there, etc.) and a `windows-sys`/resource approach for the icon.
- Manifest (`planor.manifest`) must add the Common Controls v6 dependency and keep PerMonitorV2 DPI.
- Daemon is headless: debug output only via `diag::trace`.
- Project rules: tests via `scripts\verify.ps1`; every test run rebuilds/restarts the daemon; journal each batch via
  `scripts\ai-journal.ps1`; never weaken tests. No git remote exists, so GitLab ticketing does not apply here.
- The release exe cannot be rebuilt while the daemon runs; the installed copy is separate, so installing does not
  block development builds.

## Approaches considered
1. **Native Win32 + comctl32 v6 (chosen by owner).** Real Windows controls, Mica, dark title bar, accent color; no
   new runtime. Cost: dark mode on classic controls needs the undocumented uxtheme dark-mode calls
   (`SetPreferredAppMode`, `AllowDarkModeForWindow`, `DarkMode_Explorer` theme) and a few owner-drawn parts; looks
   Windows-native, not pixel-exact WinUI.
2. WebView2 page styled like Fluent. Pretty but only imitates native, adds a dependency and a second UI stack.
3. WinUI 3 / XAML. Truest Fluent look, but needs packaging or the Windows App SDK runtime and is awkward from Rust.

## Design
- **Modules:** `settings_model.rs` (moved from config_tui: defaults+help from embedded example, validation, save),
  `settings_ui.rs` (window, pages, theming), `tray.rs` (daemon-side icon + menu on its own message-only thread
  that posts commands into the existing command path), `install.rs` (install/uninstall/autostart/installed-detect).
- **Tray -> daemon:** tray thread sends the same `Command::{Pause,Resume,Stop}` the pipe client already uses;
  Configure spawns `planor.exe settings`. Tray state (paused) read from the engine for label/tooltip.
- **Settings -> daemon:** after each debounced save, call the existing pipe client with `Command::Reload`; if the
  daemon is not running, just save. The daemon file-watcher makes the explicit call a redundancy, not a requirement.
- **Theme:** read `AppsUseLightTheme`, listen for `WM_SETTINGCHANGE "ImmersiveColorSet"`, re-apply dark mode attribute
  (`DWMWA_USE_IMMERSIVE_DARK_MODE`), backdrop (`DWMWA_SYSTEMBACKDROP_TYPE`), and control themes.
- **Install detection:** running exe path equals `%LOCALAPPDATA%\Planor\planor.exe` (case-insensitive, canonical).
- **Single instance:** one settings window (named mutex; second launch focuses the first).

## Builder's calls (owner may override)
- Install dir `%LOCALAPPDATA%\Planor\` (not Programs).
- Uninstall offered, config deletion is a prompt, default keep.
- Debounce 250 ms; invalid input shows an inline error and is not saved.
- `[[rules]]` edited as a simple add/remove/edit list, not free-form TOML.
- Icon drawn as a vector-ish pixel set at 16/24/32/48/256 and committed as `assets/planor.ico`.

## Testing
- Unit: model load/validate/save-preserves-comments; install-path detection; Run-entry value; tray command mapping;
  file-watcher debounce logic (pure parts).
- Existing `sim_tests` continue to pass; add one asserting Reload via file change updates `config_version`.
- Manual/live: tray icon appears, menu actions work, Explorer-restart re-adds icon, settings window in light and
  dark, install/uninstall round trip. Anything not run is reported as not verified.
