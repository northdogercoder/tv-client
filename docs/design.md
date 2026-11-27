# Native launcher design (C004)

This document records the decisions for the native ten-foot launcher: licence,
renderer, layout, focus, input trust boundaries, the Rust/Slint split, and the
lab evidence collected on the Pi.

## Scope

The launcher is a full-screen Wayland client that renders the home, library,
player, settings, cast and local pairing screens. It never creates a browser,
never performs network or player I/O on the UI thread, and owns no credentials.
The pure view model in `src/ui.rs` and its navigation tests are always
compiled; the Slint window and bridge exist only with the `native-ui` feature.

The C004 window does not attach a Jellyfin loader or the player supervisor to
the window. `main` launches the window with the default reducer snapshot and
reports UI commands through a callback, so this ticket proves rendering,
navigation, focus and the input trust boundary without duplicating C003/C006
wiring. `UiHandle::apply_state` and the `UiCommand` callback are the integration
points for that follow-up wiring.

## Licence: GPL-3.0-only

The project is licensed **GPL-3.0-only** (not "or later"). This is compatible
with the GPL-3.0-only option offered by Slint 1.19.1 and keeps a single licence
for the package. Choosing Slint under its GPL option means the combined binary
is conveyed under GPL-3.0-only.

If a binary is distributed (including to a customer or as part of a product),
the distributor must:

- convey the complete corresponding source for the combined work, including
  the `ui/*.slint` sources, `build.rs`, and the exact dependency versions used
  to build it, under GPL-3.0-only;
- include the full GPL-3.0 text and keep copyright, licence and warranty
  notices intact;
- mark modified files and their dates, and not impose additional restrictions
  on the freedoms the licence grants;
- provide the Installation Information required by GPLv3 section 6 when the
  binary is conveyed in a User Product, so the recipient can install a modified
  version;
- grant no warranty beyond what the project chooses to offer, and make the
  no-warranty position explicit.

Slint's separate royalty-free and commercial licences are not used. A future
change to either option is a licensing decision, not a build flag. The private
development repository does not itself distribute the binary, so these
obligations become active at distribution time.

## Renderer: winit + FemtoVG

The window uses Slint 1.19.1 with:

```toml
slint = { version = "1.19.1", optional = true, default-features = false,
          features = ["compat-1-2", "backend-winit", "renderer-femtovg", "std"] }
```

- `backend-winit` supplies both the Wayland and X11 winit backends. The Pi runs
  labwc 0.20.2 on Wayland; X11 stays available for developer desktops.
- `renderer-femtovg` renders through OpenGL/EGL. C002 established working EGL/GL
  on the Pi, and the launcher draws only rectangles, borders and text, so the
  GPU path is cheap at 1080p. The lab Pi reports EGL 1.5 with OpenGL ES 2.0
  (VC4 V3D 2.1, Mesa 26.2.7) and Slint's FemtoVG backend requests GLES 2.0 by
  default, so the renderer and the hardware agree on the API level.
- The software renderer, accessibility, system-tray and Qt backends are not
  compiled. They are not needed for a kiosk and would enlarge the Pi build.
  If a target ever lacks usable EGL, adding `renderer-software` is a build
  decision and must be re-measured, not assumed.

The root is a `FocusScope`; all navigation is keyboard-driven. Pointer targets
exist on every row so a developer can click, but no screen requires a pointer.
No image models are used anywhere; the only model is a bounded text list.

## Layout and typography

The design base is 16:9 at 1280x720. The window keeps a `base` length equal to
`min(width * 9 / 16, height)`, so:

- at 720p, `base = 720px` and the base design is used directly;
- at 1080p, `base = 1080px`, scaling every size by 1.5;
- on a narrower window, width is the limiting dimension instead.

The header is `base/10` high and the footer `base/18`. Title text is
`base/13` (55px at 720p, 83px at 1080p), primary row text is `base/17`
(42px/64px), and secondary text is no smaller than `base/38` (19px/28px). All
screens use dark opaque backgrounds (`#0b0e13`, `#141a22`, `#151c25`) with
near-white primary text (`#f4f7fb`). Secondary text (`#a9b7c8`, `#c6d2e1`) is
kept at large sizes. No blur, translucency, video surface or animation is used.

Focus is never colour-only: the focused row changes both its background
(`#1f6feb`) and gains a 4px light border (`#a8c7ff`), and its text weight goes
to 700. Unfocused pairing buttons use a 2px border and weight 400, so their
focus is visible without relying on colour. Deny is the default pairing choice
and is the left-hand button.

## Navigation model

`src/ui.rs` owns a pure `UiModel`; the declarative files only render it.

Screens are `Home`, `Library`, `Player`, `Settings` and `Cast`. The home rows
are fixed, in this order: Resume, Jellyfin, YouTube, Cast, Settings.

Key mapping:

| Key | Result |
| --- | --- |
| Up / Down / Left / Right | `Action::Navigate` with that direction; local focus moves where the screen has a list |
| Enter | Select the focused row/item/control, or retry in a library error state |
| Escape | Close search, dismiss the prompt, or `Action::Navigate { Back }` |
| Home | `Action::Navigate { Home }` and return to the home screen |
| printable character | Start search on the library screen, or append to the query while searching |

Slint encodes named keys as private-use code points (`U+F700..=U+F7FF`). The
launcher maps the arrows, Enter, Escape, Backspace and Home, and rejects every
other named key (End, Page Up/Down, Insert, function keys) before it can become
`Key::Char`. Such keys therefore never enter the library search query.

Held arrows and Backspace pass through a 120ms per-key repeat gate
(`REPEAT_INTERVAL`). Enter, Escape and typing are never throttled. This keeps a
held remote from flooding the reducer with navigation commands.

Library focus is stored as an item identifier, not a slot. Refreshing the list
re-selects the same identifier even when its position changes; if it vanished,
the old index is clamped into the new list. This is covered by
`focus_follows_the_item_identifier_across_a_refresh`. Each row is a two-band
vertical layout (title and source, then subtitle), so the two text lines cannot
overlap. The `ListView` calls `ensure-row-visible(focus-index,
ScrollBehavior.instant)` whenever the model or focused index changes, keeping
the focused row on screen.

The library shows a source filter (All sources / Jellyfin / YouTube) and a
search field. Left/Right cycles the filter; `/` or any printable character
enters search; Backspace edits the query; Escape clears and closes it. Every
row is labelled with its source, and status text names the filter and query.

States are explicit: `Idle`, `Loading`, `Ready`, `Empty` and
`Error { message }`. Loading, empty, error and retry each have their own text
and the error row has a retry target. Library items are truncated to
`MAX_LIBRARY_ITEMS = 64`; query text is capped at 48 characters and error text
at 160 characters, with control bytes replaced.

## Input trust boundary

Local and remote input are different Rust types in the view model:

- `UiInput::LocalKey { key, origin }` accepts `LocalKeyboard` and `LocalCec`.
- `UiInput::RemoteAction(Action)` represents a decoded wire command and is never
  treated as local input.

The confirmation prompt is the only way to approve a pairing request. It
defaults to Deny, a wire pairing request may present it, and every interaction
and the approval itself require a local key. The modal consumes presses across
its whole area; only its two buttons forward a choice, so an outside press
cannot reach or mutate the hidden screen. Any remote action while the prompt
is visible is rejected with `UiReject::RemoteApprovalDenied`; a remote-origin
key event is rejected the same way. Remote input cannot dismiss the prompt
either. The prompt text is a fixed string and never interpolates a
remote-controlled value. A second pairing request while one is pending is
refused rather than replacing the operator's decision. Tests cover remote
action, remote key, LocalKeyboard and LocalCec paths.

The bridge maps physical Slint key text to the same keys. It sends
`UiCommand::ApprovePairing` only from that local path; there is deliberately no
remote command variant that can do so.

## Rust/Slint split

`src/ui.rs` always compiles the following without any Slint type:

- `UiModel`, screens, home rows, source filters and labels;
- focus, repeat throttling and key mapping;
- loading/empty/error/retry state, search and status text;
- local and remote input types, the confirmation prompt and command outcomes.

With `native-ui`, `build.rs` compiles `ui/app.slint` through `slint-build`.
The `native` module is the only Slint-aware code. It:

- owns the window and the `UiModel` behind a short-lived mutex;
- installs callbacks that upgrade a `slint::Weak<AppWindow>` and handle input
  synchronously on the Slint event loop;
- exposes `UiHandle`, which posts `AppState` snapshots with
  `upgrade_in_event_loop` for a controller on another thread;
- emits `UiCommand` values (action, approval, retry, library load) to a
  callback and never performs network, storage or player calls.

No callback blocks, spawns a runtime or waits on I/O. UI model locks are held
only while copying presentation fields. Remote state reaches the window through
`UiHandle::apply_state`; the bridge does not interpret remote actions as input.

`src/main.rs` keeps `--version`, `--help`, `--config`, `--state-dir` and
`--check-config` unchanged. Without `native-ui`, execution is exactly the
headless configuration/storage path. With `native-ui`, the window is launched
after successful configuration and storage initialization.

## Testing

`tests/ui_navigation.rs` is feature-independent and deterministic; all
timestamps are explicit `Duration`s. It covers arrows, Enter and Escape, focus
stability across refreshes, per-key repeat throttling, search and source
labels, loading/empty/error/retry, the 64-item bound, identifier validation,
remote approval rejection and local keyboard/CEC approval.

## Pi evidence

C004 was built and launched on the lab Pi 3B (aarch64, Debian 13, labwc 0.20.2
Wayland session) on 2026-11-26.

- Session: `/run/user/1000/wayland-0` present; active output `HDMI-A-1` at
  `1920x1080 px, 60.000000 Hz (preferred, current)`.
- GPU: `eglinfo -B` reports EGL 1.5, OpenGL 2.1 and OpenGL ES 2.0 on VC4 V3D
  2.1 (Mesa 26.2.7); FemtoVG requests GLES 2.0.
- Toolchain: Rust 1.100.0, `CARGO_BUILD_JOBS=1`, `CARGO_INCREMENTAL=0`,
  `CARGO_PROFILE_DEV_DEBUG=0`.
- Build: `cargo build --features native-ui` **completed** on the Pi. The first
  compile took roughly an hour; the cached final rebuild took ~5m51s and
  produced `~/tv-client-c004/target/debug/tv-client` (~70 MB debug). No OOM.
- Launch: started from the active labwc session with
  `XDG_RUNTIME_DIR=/run/user/1000` and `WAYLAND_DISPLAY=wayland-0`. The window
  mapped **fullscreen at 1920×1080**; the process stayed alive and produced no
  panic or error log. A `grim` capture was taken after 12 s.
- Fullscreen: the `.slint` window is 1920×1080 and `NativeUi::run` sets an
  explicit physical size plus a queued fullscreen request **after** `show()`.
  A pre-show fullscreen request was ignored by labwc in testing.
- Screenshot: `/tmp/c004-final.png` on the Pi (1920×1080). It is not committed;
  it contains no pairing code, token, credential or library secret.
- 720p: **NOT RUN**. The TV output is 1080p and the layout scales from the
  1280×720 design base; no 720p capture is claimed.
- Input: physical arrows/Enter/Back were not injected (`/dev/uinput` is
  root-only and `wtype`/`ydotool` are absent); the 18 deterministic navigation
  tests pass, and the operator can exercise the physical keys on the running
  window.
- Tests: `cargo test --no-default-features --test ui_navigation` passes 18/18
  on the host; Docker `cargo check`/`clippy --features native-ui` pass. The Pi
  pure view-model suite passed 15/15 before the three review regression tests;
  the native Pi build and launch were rerun after the fixes.

This is a basic launch/fullscreen proof, not a soak or a physical-input
acceptance run.

Direct Cage/GLES revalidation on the installed **0.1.0** kiosk (2026-11-27):
after lightdm was stopped and tty1 activated, `tvkiosk` ran Cage with the
1920×1080 UI and the operator confirmed it visible on the TV. The bottom status
strip is clipped by the TV's overscan and Settings is still read-only; both are
addressed by C014 (safe-area setting and editable settings/Quick Connect). No
screenshot was captured because the tvkiosk Wayland socket is not readable by
the admin SSH user.
