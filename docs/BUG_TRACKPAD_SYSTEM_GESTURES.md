# BUG: Windows' own three/four-finger touchpad gestures cannot be disabled by Planor

**Status:** open. **Workaround:** set the gestures to "Nothing" by hand in Settings (below).

## Symptom

Planor handles the precision touchpad itself (raw HID contacts): three fingers scroll/focus, four
fingers carry the focused window. Windows' built-in gestures (Task view, Show desktop, Switch apps,
Switch desktops, ...) fire at the same time, because Planor only listens (`RIDEV_INPUTSINK`) and
cannot consume the contacts.

## What was tried (machine: ELAN0676 precision touchpad, Windows 11 build 26300)

Gesture settings live in `HKCU\Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad`:

| Value | Meaning (observed) |
|---|---|
| `ThreeFingerSlideEnabled`, `FourFingerSlideEnabled` | swipe **preset**: 1 = apps, 2 = desktops, 3 = media (volume up/down, previous/next track). Unknown values behave like the default. |
| `ThreeFinger{Up,Down,Left,Right}`, `FourFinger{...}` | per-direction actions written by the Advanced gestures page (originals: 3-finger Up=2 Down=3 Left=1 Right=1; 4-finger Up=2 Down=3 Left=4 Right=4) |
| `ThreeFingerTapEnabled`, `FourFingerTapEnabled` | tap action (absent = Nothing) |

1. Writing all of the above to `0` from the daemon (registry write) did **not** stop Windows firing
   its swipes, even after re-applying the touchpad parameters (`SPI_GETTOUCHPADPARAMETERS` /
   `SPI_SETTOUCHPADPARAMETERS`, struct size 44) and broadcasting `WM_SETTINGCHANGE`.
2. Changing the preset through the Settings app writes exactly one value (`ThreeFingerSlideEnabled`
   or `FourFingerSlideEnabled`) and nothing else (full `HKCU` diff). So the registry content is not
   what differs; Windows seems to apply a change only when it arrives through the Settings path.
3. Setting both presets to `3` (media) did stop the Windows task-view/desktop gestures and let Planor
   see the contacts, **but** that preset maps swipe up/down to volume, so the system volume changed
   with every swipe (reported 2026-11-20). The per-direction values do not override the preset.
4. Driving the Settings dropdowns through UI Automation did not work (the XAML popup list is not
   exposed), so "Nothing" could not be written that way.

Planor therefore no longer touches any touchpad value (`src/win32.rs`, `SYSTEM_DWORDS` now only holds
the Snap settings). The Snap/tiling-assist settings are still disabled on start and restored on stop.

5. Observed after the user set "Nothing" in Settings by hand (2026-11-20): the registry holds
   `ThreeFingerSlideEnabled=0`, `FourFingerSlideEnabled=0` and the per-direction values **unchanged**
   (3-finger Up=2 Down=3 Left=1 Right=1, 4-finger Up=2 Down=3 Left=4 Right=4). That is the same
   `0` the daemon wrote in attempt 1, so the stored value is not the problem; what matters is that the
   change was made through Settings, which makes Windows apply it live. A direct registry write of `0`
   did not. Whatever Settings does besides the registry write is the missing piece.

## Workaround

Settings > Bluetooth & devices > Touchpad > Advanced gestures: set every three- and four-finger
Swipe (up, down, left, right) and Tap to **Nothing**.

## To resolve

- Find how the Settings app applies a gesture change at run time (probably an undocumented call into
  the input stack; trace `SystemSettings.exe`, e.g. with Process Monitor / API tracing) and do the
  same from the daemon, or find which registry value encodes "Nothing" and how to make Windows reload.
- Alternatively avoid the conflict: consume the contacts before the OS gesture recogniser (not possible
  from user mode with raw input alone), or a small filter/UMDF driver.
- Once solved, restore the originals on stop (the backup mechanism in `tweak_system` / `restore_system`
  already exists).

## Related open items

- Touchpad fling to another monitor lagged/stalled; mitigated by async window moves while dragging,
  clamping to the monitors and an instant (non-animated) cross-monitor drop. Needs re-test on hardware.
- Continuous three-finger scroll: the settle (reveal + focus) is delayed 220 ms after the fingers lift,
  and Windows foreground events are ignored while a touchpad scroll/drag is active.
