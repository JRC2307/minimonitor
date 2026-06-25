# Display Resolution Presets — Design

**Date:** 2026-06-24
**Status:** Approved
**Crate:** `crates/menubar`

## Problem

caguabot remotes into the Mac mini via **RustDesk** from several devices: a MacBook
Air, the mini's own connected ViewSonic VX2370 display, and an iPhone held vertically.
RustDesk mirrors whatever resolution and orientation the mini is currently running, so a
landscape 16:9 desktop is awkward on a portrait iPhone, and the mini's native resolution
doesn't necessarily match the MacBook Air. Changing the mini's resolution today means
opening System Settings over a clumsy remote session.

## Goal

Add a **"Display"** submenu to the MiniMonitor tray menu (the macOS top nav bar) with
three one-click presets that switch the mini's display to a resolution/orientation matched
to the device being used to remote in. Tray-only — no inspector-window UI.

## The Three Presets

| Preset | Menu label | Target mode | Rotation |
|--------|------------|-------------|----------|
| Mini native screen | `Mini screen (1920×1080)` | 1920×1080 | 0° (landscape) |
| MacBook Air | `MacBook Air (1440×900)` | 1440×900 (16:10) | 0° (landscape) |
| iPhone (vertical) | `iPhone portrait (1080×1920)` | 1920×1080 @ degree 90 | 90° (portrait) |

Rationale:
- **Mini native** — full native panel resolution for working at the desk.
- **MacBook Air** — 16:10 to match the Air's aspect so RustDesk fills the laptop screen.
- **iPhone portrait** — true portrait so RustDesk fills the phone held vertically. While
  active, the physical ViewSonic shows a sideways image; acceptable since nobody is at the
  desk when remoting from the phone.

### Mode-list reconciliation (first implementation step)

The VX2370 is a 16:9 panel, so it may not expose a 16:10 `1440×900` mode. **Before
wiring the presets**, run `brew install displayplacer` then `displayplacer list` on the
mini and record the actual available modes for this display. Pin each preset to the
closest available mode. If `1440×900` is not offered, fall back to `1600×900` for the
MacBook Air preset and note it in the menu label. The numbers in the table are the intent;
they are reconciled against the real mode list during implementation.

## Mechanism

Shell out to **`displayplacer`** (installed via Homebrew), consistent with how the app
already shells out to `caffeinate` and `osascript`. displayplacer handles both resolution
and rotation cleanly; native CoreGraphics would require private APIs for rotation.

A displayplacer apply command looks like:

```
displayplacer "id:<persistent-screen-id> res:1920x1080 hz:60 color_depth:8 scaling:on degree:90"
```

The persistent screen id, refresh rate, and color depth are read from `displayplacer list`
(single display) and reused; only `res` and `degree` change per preset.

## Architecture

Mirrors the existing action pattern (`actions.rs` helper → `app.rs` menu handler →
`tray.rs` menu item + status message).

### `crates/menubar/src/actions.rs`

- `enum DisplayPreset { MiniNative, MacBookAir, IPhonePortrait }`
  - maps to a target `(width: u32, height: u32, degree: u32)`.
- `struct DisplayMode { screen_id: String, width: u32, height: u32, hz: String, color_depth: String, degree: u32 }`
- `fn parse_displayplacer_list(output: &str) -> Result<DisplayMode, String>`
  - parses `displayplacer list` text to extract the (single) display's persistent
    screen id and current res/hz/color_depth/degree.
- `fn build_apply_command(mode: &DisplayMode, preset: DisplayPreset) -> String`
  - builds the displayplacer argument string for the target preset, reusing the
    detected screen id / hz / color_depth.
- `fn current_mode() -> Result<DisplayMode, String>`
  - runs `displayplacer list`, returns parsed `DisplayMode`, or an error if the binary
    is missing (`displayplacer not installed — run: brew install displayplacer`).
- `fn apply_preset(preset: DisplayPreset) -> Result<DisplayMode, String>`
  - `current_mode()` → `build_apply_command()` → run displayplacer → return the new mode.

### `crates/menubar/src/tray.rs`

- `fn build_display_submenu(current: Option<&DisplayMode>) -> Submenu`
  - a "Display" submenu with three items: `display:native`, `display:mba`,
    `display:iphone`.
  - marks the active preset with a trailing `•`, determined by matching `current`
    (res + degree) against each preset's target.
  - placed adjacent to the existing "Quick actions" submenu in `build_menu`.

### `crates/menubar/src/app.rs`

- `handle_menu` gains arms for `display:native` / `display:mba` / `display:iphone`:
  - call `actions::apply_preset(...)`,
  - set `status_message` to a human result, e.g.
    `Display → iPhone portrait (1080×1920)` or the error string,
  - existing `refresh_tray_menu()` re-renders the menu so the `•` moves to the new preset.
- `AppState` may cache the last-known `DisplayMode` so the submenu marks the active
  preset without re-shelling-out on every menu open (best-effort; a missing/failed read
  just shows no `•`).

## Data Flow

```
click "iPhone portrait" (display:iphone)
  → MenuEvent → UserEvent::Menu
  → AppState::handle_menu("display:iphone")
  → actions::apply_preset(IPhonePortrait)
      → current_mode()  (displayplacer list → parse_displayplacer_list)
      → build_apply_command(mode, IPhonePortrait)
      → run displayplacer
  → status_message = "Display → iPhone portrait (1080×1920)"
  → refresh_tray_menu()  (• now on the iPhone item)
```

## Error Handling

- `displayplacer` binary missing → status message
  `displayplacer not installed — run: brew install displayplacer`. No crash.
- `displayplacer list` unparseable / no display → status message with the error; presets
  remain clickable but report failure.
- A failed apply (non-zero exit) → status message includes the exit/error; the menu does
  not falsely mark the preset active (the `•` is driven by the actual detected mode).

## Testing (TDD)

Pure functions are unit-tested in `actions.rs`; the thin shell-out is left untested,
matching `flush_dns` / `caffeinate`.

1. `parse_displayplacer_list` — feed a captured sample of real `displayplacer list`
   output, assert it extracts screen id, width, height, hz, color_depth, degree.
2. `build_apply_command` — for each `DisplayPreset`, assert the exact displayplacer
   argument string (correct res, degree, reused id/hz/color_depth).
3. Active-preset matching — given a parsed `DisplayMode`, assert the correct preset is
   identified (including the portrait 90° case) and that an off-list mode matches none.

## Out of Scope (YAGNI)

- Multi-display handling (single screen on the mini).
- Arbitrary / custom user-entered resolutions.
- Persisting a preset across reboots.
- Inspector-window UI for resolution (tray-only by request).
- Auto-detecting the connecting RustDesk client and switching automatically.
