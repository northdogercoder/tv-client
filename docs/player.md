# Native player (GStreamer child)

The TV client does not render media in the UI process. A supervised child
process, `tv-native-player`, owns the GStreamer pipeline, the Wayland surface
and the audio sink. The parent keeps policy and state; the child only executes
validated playback requests. This isolates decoder or driver failures from the
UI and gives the video surface an independent fullscreen window.

The child is built only with the `gstreamer-player` feature. The parent-side
protocol, state machine and supervision code in `src/player/` are always
compiled, so `--no-default-features` and the headless gate never link
GStreamer.

## Process model

```text
UI / reducer ──PlayerCommand──▶ supervisor task ──JSON line──▶ tv-native-player
     ▲                              │                              │
     └────PlayerNotification────────┘◀─────JSON line──────────────┘
```

- One child per load generation. A new `Load` (or `Stop`) tears the previous
  child down before a replacement starts.
- A `PlayerSession` is the exclusive owner of the media backend for its
  lifetime. Backends are selected by passing a `ChildFactory`
  (`GStreamerChildFactory` for `tv-native-player`, `ProgramChildFactory` for any
  other protocol-speaking child); the supervisor and state machine never name a
  backend-specific type.
- The supervisor uses bounded Tokio channels for commands, child lines and
  notifications.
- After a successful `load`, the supervisor also sends the machine's retained
  volume to the fresh child, so a later load does not silently reset to the
  child default.
- The media URL and optional headers travel only over the child's stdin pipe.
  They are never placed in argv, environment variables, filenames or logs.
- The child writes protocol events to stdout and no diagnostic data to stdout.
  Its stderr is discarded by the parent (`Stdio::null()`); GStreamer's own
  debug output therefore cannot leak a token into the parent's log.

## Wire protocol

Both directions use one UTF-8 JSON object per line, terminated by `\n`. The
maximum line length is 64 KiB excluding the newline. Malformed JSON, unknown
message types, values outside their ranges and over-long lines are rejected.
Requests from the parent and events from the child are separate tagged unions.

### Parent → child

| `type` | Fields | Meaning |
|---|---|---|
| `load` | `url`, optional `headers`, optional `start_position_ms` | Open an absolute `file:`, `http:` or `https:` URL. |
| `pause` | — | Set the pipeline to `PAUSED`. |
| `resume` | — | Set the pipeline back to `PLAYING`. |
| `seek` | `position_ms` | Flush-accurate absolute seek, at most 24 hours. |
| `set_volume` | `volume` (0–100) | Linear volume scaled to GStreamer's 0.0–1.0 property. |
| `stop` | — | Tear down the pipeline and exit the child. |

Example load:

```json
{"type":"load","url":"file:///tmp/fixture-720p30.mp4","start_position_ms":0}
```

With authentication (the value is redacted here and in all `Debug` output):

```json
{"type":"load","url":"https://jellyfin.local/Videos/1/stream?static=true","headers":{"Authorization":"Bearer <redacted>"}}
```

### Child → parent

| `type` | Fields | Meaning |
|---|---|---|
| `ready` | optional `decoder`, optional `sink` | Pipeline prerolled; identifies the selected decoder and sink. |
| `position` | `position_ms`, optional `duration_ms` | Playback clock update, emitted about every 500 ms. |
| `ended` | — | Natural end of stream. |
| `error` | `code`, optional `message` | Failure. `code` is a bounded token; `message` is redacted by both sides. |
| `tracks` | optional `video`, `audio`, `subtitle` | Selected stream descriptions, diagnostics only. |

## State machine

`PlayerMachine` is pure, synchronous and feature-independent:

```text
Idle ──load──▶ Loading ──ready──▶ Playing ⇄ pause/resume ⇄ Paused
                  │                    │
                  │ error              ├─ ended ─▶ Ended
                  └──── error ─────────┴─ error ─▶ Error
any active state ──stop──▶ Idle
```

Rules enforced by `PlayerMachine::apply_command`:

- `load` is accepted in any state and increments a generation id. Old
  generation events are rejected with `StaleGeneration`; they can never revive
  or modify a newer playback.
- `pause` is accepted only while `Playing`; pausing while already `Paused` is a
  no-op. `resume` mirrors this.
- `seek` requires `Playing` or `Paused`; a known duration bounds
  `position_ms` (0 through duration inclusive), and every position is capped
  at 24 hours so it cannot overflow the child's GStreamer clock.
- `set_volume` accepts 0 through 100 only.
- `stop` is idempotent. Stopping `Idle` produces neither I/O nor a notification.
- `error` from the child is accepted only for an active generation and moves
  the machine to `Error`; playback metadata is retained for the UI but the
  generation is not reusable.

The supervisor additionally enforces:

- **Ready deadline** (default 15 s): a child that never emits `ready` is
  reported as `child_hung`, killed and reaped.
- **Event deadline** (default 10 s): while `Playing`, silence longer than the
  deadline is treated as a hung decoder. Paused playback is exempt.
- **Spawn/write deadlines** (10 s / 5 s): a child that cannot be started or
  written to is reported as `spawn_failed` / `write_failed`.
- **Exit detection**: an unexpected exit while `Loading`, `Playing` or
  `Paused` becomes `child_exited` or `child_failed`. An exit after `Ended`,
  `Error` or `Stop` is expected and ignored.

## Teardown and reaping

Teardown sends `stop`, waits up to the reap timeout (default 5 s) for the child
to exit and then sends a kill request. The child-owning Tokio task always calls
`wait()` after a kill, so the process is reaped rather than left as a zombie.
`kill_on_drop(true)` is a final backstop if the supervisor itself is dropped.
Teardown is idempotent: repeating `Stop` produces no notification and no new
child. A subsequent `load` starts a fresh child with the next generation.

The child exits on `stop`, stdin EOF (parent died) or the normal `SIGTERM`
disposition. `Back`/`Esc` is parent-routed (C008); the child does not read the
keyboard and has no window-manager shortcuts of its own.

## Decoder and sink selection

`tv-native-player` creates `playbin3` and requests:

- video: `waylandsink` (explicit, so a missing Wayland display fails visibly
  instead of silently falling back to a CPU sink);
- audio: `pulsesink`, falling back to `autoaudiosink` only if `pulsesink` is
  unavailable.

`playbin3` selects the decoder by rank. On the Raspberry Pi 3B, the installed
`v4l2h264dec` has rank `primary + 1` (257), above software decoders, and is
selected for H.264. The child listens to `element-setup` and records the first
video and audio decoder factory names; the `ready` event reports them. The sink
name is reported as `waylandsink`.

The child does not fabricate a decoder name when none was observed. On a host
without `v4l2h264dec` the reported decoder will be whichever factory GStreamer
actually selected (for example `avdec_h264`).

## Authentication and headers

Jellyfin can require a token. Two paths are supported:

1. A URL that already carries the authenticated query parameters. It is passed
   through memory only and is never logged.
2. Optional headers attached to `load`. The child listens to `playbin3`'s
   `source-setup` signal and, when the created source exposes an
   `extra-headers` property (`souphttpsrc`), sets a `GstStructure` containing
   the header names and values. If the source does not expose that property,
   the request still plays with the URL alone; the child never falls back to
   copying the URL or headers anywhere else.

Header names are validated against the HTTP token grammar, values may not
contain control bytes, at most 32 headers and 8 KiB total are accepted, and the
whole load line stays under 64 KiB.

## Security properties

- No shell, no arbitrary command execution and no arbitrary URL fetch: the
  child only accepts the six request types above and only absolute `file:`,
  `http:` or `https:` URLs without userinfo.
- URLs sent to `Debug`, error messages, notifications and logs have query,
  fragment and userinfo removed. Header values are replaced by a count.
- Child error codes are restricted to `[A-Za-z0-9_.:-]` and lengths are
  bounded; free-form messages are URL-redacted, control-stripped and truncated
  to 512 bytes.
- The parent never prints the child's stdin, argv or URL. The child writes no
  logs to stdout.
- The protocol reader rejects a line as soon as it crosses 64 KiB, so a
  compromised child cannot make the parent allocate without bound.

## Build and run

```sh
# Parent and child with GStreamer support.
cargo build --release --features gstreamer-player

# Headless gate, no GStreamer:
cargo test --locked --no-default-features --test player_state
```

`PlayerSession` is the intended parent API: start it with a
`GStreamerChildFactory` (or a test factory), send `PlayerCommand` values and
consume `PlayerNotification` values. Unit tests in `tests/player_state.rs` use a
scripted fake child and cover normal transitions, ready-deadline expiry,
hangs, retained failure codes, late events, child errors, seek and volume
bounds, volume restoration after load, teardown/restart, bounded lines, huge
positions and redaction.

### Backend seam (C010 and future backends)

The always-compiled supervisor is backend neutral. A new playback backend only
needs to:

1. ship a child binary that speaks the protocol in this document;
2. implement `ChildFactory` (or use `ProgramChildFactory`) to start it;
3. pass the factory to `PlayerSession::start`.

`ProgramChildFactory` already handles stdin/stdout wiring, bounded line
reading, kill and reap, and `PlayerSession` enforces one active child at a time.
The C010 libmpv/yt-dlp child can therefore be added beside the GStreamer child
without editing `src/bin/tv-native-player.rs` or any
`#[cfg(feature = "gstreamer-player")]` code. A backend switch is a `Stop` (or
dropping the session) followed by a session using the other factory, so video
and audio always have exactly one owner.

## Measured on the Raspberry Pi 3B

> These measurements are short hardware smoke checks, not a soak test. The
> operator was watching the TV; each run was limited to one 20 s fixture.

Environment: Raspberry Pi 3 Model B Rev 1.2, Debian 13 (trixie), kernel
`6.18.39+rpt-rpi-v8`, GStreamer 1.26.2, Rust 1.100.0, dev profile with
`CARGO_PROFILE_DEV_DEBUG=0`, labwc 0.20.2 on tty1
(`XDG_RUNTIME_DIR=/run/user/1000`, `WAYLAND_DISPLAY=wayland-0`), HDMI audio sink
selected as in `hardware-report.md`.

Each run loaded a 20 s fixture over `file://`, played about 2 s, executed one
flush seek to 5000 ms, paused for 1 s, resumed for 2 s and sent `stop`. The
harness recorded events, `/proc/<pid>/status:VmHWM`, the peak file-descriptor
count, wall time and the process exit status.

| Run | Fixture | Observed decoder/sink | Tracks | Wall time | Peak RSS | Peak FDs | Exit |
|---|---|---|---|---|---|---|---|
| 720p | `fixture-720p30.mp4` | `v4l2h264dec` / `waylandsink` | 1280x720, 48 kHz 2 ch | 11.90 s | 63,236 KiB (61.8 MiB) | 30 | 0 |
| 1080p | `fixture-1080p30.mp4` | `v4l2h264dec` / `waylandsink` | 1920x1080, 48 kHz 2 ch | 9.52 s | 68,140 KiB (66.5 MiB) | 30 | 0 |
| 720p repeat | `fixture-720p30.mp4` | `v4l2h264dec` / `waylandsink` | 1280x720, 48 kHz 2 ch | 8.92 s | 62,308 KiB (60.8 MiB) | 30 | 0 |

Observed protocol evidence from the same runs:

- `ready` always reported the hardware decoder and the explicit sink before the
  first position event, for example
  `{"type":"ready","decoder":"v4l2h264dec","sink":"waylandsink"}`.
- `position` reported `duration_ms: 20000`; the seek produced a jump to
  `position_ms: 5000`, and the pause held the position for about one second
  before resume continued forwards.
- `tracks` reported the negotiated size and audio format, derived from the sink
  pad caps, for example `"video":"1280x720","audio":"48000 Hz, 2 ch"`.
- All three children exited with status 0 after `stop`; `ps` showed no
  `tv-native-player` process and no zombie afterwards. Temperature was
  47.8–49.4 °C after the runs, and `vcgencmd get_throttled` returned `0x0`.
- Audio was routed through the selected HDMI PulseAudio sink (`pulsesink`);
  C002 established audible HDMI output. This change did not independently
  record an audible check.

Limitations and honest non-claims:

- No 30-minute soak, no twenty-start/stop loop, no EOS-to-UI run, no CEC,
  keyboard/focus-recovery or `Back`/`Esc` end-to-end test was performed for
  this change. The three runs above are the complete C006 hardware evidence.
- Video presentation was not blocked by labwc in these runs: `playbin3`
  reached `PLAYING`, `waylandsink` was selected and no GStreamer error reached
  the bus. The executor did not take a screenshot or otherwise prove the
  operator-visible image; the operator was watching the TV during the runs.
- The first build attempt used `/tmp` on the Pi. `/tmp` is a 453 MiB tmpfs;
  compiling the GStreamer bindings there filled RAM and rebooted the Pi. The
  worktree was moved to `~/tv-client-c006` and rebuilt with one job and no
  debug info. This was a build-environment failure, not a playback result.
- The headless `--no-default-features` suite (including the 17 player tests),
  `cargo fmt --all -- --check`, `cargo clippy --all-targets
  --no-default-features -- -D warnings`, `cargo check --features
  gstreamer-player` and `cargo build --features gstreamer-player --bin
  tv-native-player` all passed on the Pi with Rust 1.100.0. The x86_64 host
  gate remains the orchestrator's run; the host did not run cargo in this
  worktree, and `Cargo.lock` in the worktree was deliberately not regenerated.
- The `source-setup` / `extra-headers` path was compiled but not exercised
  against a real authenticated Jellyfin HTTPS stream; the hardware runs used
  `file://` URLs only. Header injection is covered by protocol validation and
  unit tests, not by a live token exchange.
