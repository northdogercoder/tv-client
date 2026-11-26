# Input, provenance and HDMI-CEC

The launcher accepts physical input from two local sources:

- the USB/Bluetooth keyboard handled by the compositor and delivered to Slint;
- the TV remote delivered by the kernel HDMI-CEC adapter.

Remote network actions are a third, separate path. They are decoded from
protocol v1 and can never become a local key, a CEC event or a physical
approval.

## Input model

`src/input/` is always compiled and has no Slint or GStreamer dependency.
`InputEvent` carries the origin (`LocalKeyboard`, `LocalCec` or `Remote`), a
logical key and the source's repeat flag:

| Logical key | Keyboard source | CEC `ui_cmd` | UI effect |
|---|---|---|---|
| `Up` / `Down` / `Left` / `Right` | arrows | `UI_CMD_UP` / `DOWN` / `LEFT` / `RIGHT` | navigation |
| `Select` | Enter/Return | `UI_CMD_SELECT`, `UI_CMD_ENTER` | activate / approve a local prompt |
| `Back` | Escape | `UI_CMD_BACK`, `UI_CMD_EXIT` | back; Stop while the player owns the surface |
| `Home` | Home | `UI_CMD_DEVICE_ROOT_MENU`, `UI_CMD_MEDIA_TOP_MENU`, `UI_CMD_CONTENTS_MENU` | home; Stop while the player owns the surface |
| `PlayPause` | — (keyboard Space arrives as `Char(' ')` and is handled by the player screen) | `UI_CMD_PLAY`, `UI_CMD_PAUSE`, `UI_CMD_PLAY_FUNCTION`, `UI_CMD_PAUSE_PLAY_FUNCTION` | toggle pause |
| `VolumeUp` / `VolumeDown` / `Mute` | — | `UI_CMD_VOLUME_UP` / `DOWN` / `MUTE` | local volume group; no audio sink is wired in this build |
| `Char` / `Backspace` | printable text, Space, Backspace | never | search text; Space toggles pause on the player screen |

Unknown CEC commands are ignored. They are never guessed into a nearby key.

### Provenance rules

`LocalOrigin` has exactly two variants, `Keyboard` and `Cec`. It can only be
built from `InputOrigin::LocalKeyboard` or `InputOrigin::LocalCec` with
`TryFrom`; `InputOrigin::Remote` returns `InputReject::RemoteNotLocal` before
a key is produced. `Router::route` therefore has no remote-to-local
conversion, and `UiModel::handle_event` refuses a remote event even if a
caller constructs one directly.

Physical approval is guarded at three layers:

1. `input::PhysicalApproval::new` refuses a remote origin and a nil request id.
2. `UiModel::handle_input` refuses a remote local-key or remote action while a
   confirmation prompt is open, and the prompt remains pending.
3. `Reducer::apply` independently refuses
   `Event::PhysicalApproval { origin: Remote, .. }` with
   `StateError::RemoteApprovalDenied`.

Only local callbacks (the Slint keyboard/pointer handlers and the CEC bridge)
submit `InputEvent`s. The CEC helper runs with `--no-rc-passthrough`, so the
kernel does not also emit a duplicate keyboard event for the same remote
press; there is exactly one local path.

Remote events that arrive with no prompt open are ignored. Remote events that
arrive while a prompt is open are rejected without dismissing or approving it.

## Repeat, debounce and long press

`RepeatGate` shares a 120 ms interval and a per-key slot table between the
keyboard and CEC paths. It is the single debounce implementation used by the
`UiModel`; the CEC parser applies the same gate so a TV that repeats a press
faster than the interval cannot flood navigation.

`LongPress` is available for callers that need a hold gesture. It fires at
most once per held key and re-arms on release. This build does not bind a long
press yet.

The CEC parser treats a same-key press within two seconds (without a release)
as `repeat = true`. A `CEC_MSG_USER_CONTROL_RELEASED` message ends the hold.
Presses faster than 120 ms are dropped.

## Bounded queues

`InputQueue` has a fixed capacity (default 32, hard maximum 256). The CEC
bridge pushes without blocking; a full queue drops the event and increments a
drop counter. The Slint event loop drains the queue and feeds each event
through `UiModel::handle_event`. A stalled UI therefore cannot block the CEC
thread, and a remote that repeats faster than the UI can render cannot grow
memory.

The CEC reader caps a helper line at `MAX_CEC_LINE` (512 bytes) and discards
bytes beyond that until the newline. A truncated line is never parsed because
its `ui_cmd` field may be incomplete.

## Kernel CEC path on the Pi

The Pi exposes the TV remote through the kernel CEC adapter. Probed
2026-11-26:

```text
$ cec-ctl --version
cec-ctl 1.30.1

$ cec-ctl --list-devices
vc4_hdmi (vc4-hdmi):
	/dev/cec0

$ ls -l /dev/cec0
crw-rw---- 1 root video 239, 0 Nov 25 22:21 /dev/cec0
```

The user `anders` is in the `video` group (`id` includes `44(video)`).

Monitor mode (`cec-ctl -m`) requires root:

```text
$ cec-ctl -m -s --monitor-time 2
Selecting monitor mode failed, you may have to run this as root.
```

The bridge therefore uses follow mode, which works for a `video`-group user.
It starts this exact fixed argument list, with no shell and no appended
command-line or remote input:

```text
cec-ctl --device /dev/cec0 --playback --no-rc-passthrough --wait-for-msgs --skip-info
```

- `--device /dev/cec0` is the `vc4_hdmi` adapter.
- `--playback` claims a Playback Device logical address so the TV addresses
  user-control messages to us.
- `--no-rc-passthrough` keeps received CEC messages readable on stdout instead
  of letting the kernel translate them into duplicate keystrokes.
- `--wait-for-msgs` stays in follow mode without the root-only monitor mode.
- `--skip-info` suppresses the driver banner.

A short follow-mode run as the normal user produced the adapter's state-change
event and stayed alive until the external timeout:

```text
$ timeout 4 cec-ctl --device /dev/cec0 --playback --no-rc-passthrough --wait-for-msgs --skip-info
(warn: State Change events were lost)
19271.077598s: Event: State Change: PA: 1.0.0.0, LA mask: 0x0010, Conn Info: yes
```

`LA mask: 0x0010` is logical address 4, the Playback Device address.

### Supervision and failure behavior

`CecBridge` runs one supervisor thread:

- missing `/dev/cec0` or a missing `cec-ctl` is reported as a typed
  `CecStatus::Unavailable` reason (`DeviceMissing`, `HelperMissing`,
  `PermissionDenied`, …) and retried after the probe interval;
- a helper exit or output read failure is reported as
  `CecStatus::Disconnected` with a bounded reason and the retry delay;
- reconnect delay starts at 250 ms, doubles to a 5 s cap, and resets after a
  connection stayed up for five seconds;
- a device/helper absence is re-probed every 30 s;
- the helper's stderr is discarded and its stdout is read with a bounded line
  reader, so a noisy or malformed helper cannot exhaust memory;
- the bridge never sends a CEC command, never changes TV power state and never
  power-cycles the display.

The bridge is started only in the `native-ui` build. The headless build remains
headless and starts no CEC process.

### Teardown

`main.rs` stops the CEC bridge after the Slint loop returns and before it
cancels the playback controller:

1. set the bridge stop flag;
2. kill the helper child;
3. join the supervisor with a 2 second deadline.

The helper is killed first so a blocked stdout read returns immediately. If
the supervisor misses the deadline, main logs a warning and continues; the
playback shutdown is still bounded independently. Dropping a `CecBridge`
without `shutdown` also requests stop and kills the child.

## TV settings and keyboard fallback

Enable HDMI-CEC on the TV and point the remote at the Pi's HDMI input. Menu
names differ by vendor; look for one of:

- Samsung: *Anynet+ (HDMI-CEC)*;
- LG: *SIMPLINK (HDMI-CEC)*;
- Sony: *BRAVIA Sync* / *HDMI control*;
- Panasonic: *VIERA Link*;
- Philips: *EasyLink*;
- TCL/Hisense/other: *HDMI-CEC*, *CEC control* or *Device control*.

The tested TV is a **Fire TV (Fire OS)**. Its bundled Fire remote communicates
with the TV over Bluetooth/IR and did **not** emit CEC `User Control` messages
to the Pi; only CEC state/Active Source traffic was visible. Treat remote CEC
as best-effort on this model and use the USB keyboard fallback for reliable
navigation.

Enable "control the TV with the connected device" / "device auto power off"
only as desired. This client never asks the TV to power off, but a TV setting
that powers off inactive sources can still turn the Pi's display off. Disable
that setting if the display blanks while the client is idle.

If CEC is unavailable, the USB/Bluetooth keyboard remains a full fallback:

| Key | Action |
|---|---|
| arrows | navigate |
| Enter | select / approve a local prompt |
| Escape | back / stop playback and return home |
| Home | home / stop playback |
| Space | pause/resume on the player screen |
| printable text, `/` | library search |

labwc delivers keyboard events to the focused fullscreen Slint window; no
extra input daemon is required.

## Evidence and limits

Verified on the Pi:

- `cec-ctl` 1.30.1 and `/dev/cec0` via `vc4_hdmi`;
- follow mode starts as `anders` in the `video` group and stays running;
- monitor mode fails without root, which is why the bridge does not use it;
- the bridge's parser, queue, provenance and shutdown behavior are covered by
  feature-independent tests in `tests/input.rs`;
- the fixture-helper tests exercise reconnect/disconnect and bounded shutdown
  without touching the TV.

**Remote CEC is NOT VERIFIED on the tested Fire TV.** The operator pressed
Up/Down/Left/Right/OK/Back/Home during a 60-second follow-mode window on
2026-11-26. The capture contained only:

```text
Event: State Change: PA: 1.0.0.0, LA mask: 0x0010, Conn Info: yes
Received from TV to all (0 to 15): ACTIVE_SOURCE (0x82)
```

No `User Control Pressed`/`ui_cmd` messages arrived. The TV is a Fire TV
(Fire OS) and its Fire remote appears to use Bluetooth/IR to the TV, not CEC
user-control to the Pi. This is a hardware/model limitation, not parser
evidence. The parser follows the `cec-ctl` format and remains covered by
fixture lines; the USB keyboard fallback is the reliable input path on this
TV. Re-test CEC remote navigation only if the TV/remote setup changes.
