# Building and checking tv-client

## Toolchain

The Rust toolchain is pinned in `rust-toolchain.toml` to **1.100.0** with the
`minimal` profile plus the `rustfmt` and `clippy` components. A rustup
installation honours that pin automatically, so no per-shell override is
needed. `Cargo.toml` sets `rust-version = "1.95"` as the package's minimum
supported Rust version. The bump from 1.92 is required by the optional Slint
`1.19.1` dependency; the locked headless dependency set and the optional
GStreamer bindings (`gstreamer`, `gstreamer-video`, `gstreamer-audio` 0.25.4)
are satisfied by the same minimum. The pinned 1.100.0 toolchain is the version
the project builds and checks with.

Both binaries and the library target are produced from the single package
`tv-client`:

- `src/main.rs` is the `tv-client` binary: headless by default, and the Slint
  window when built with `native-ui`.
- `src/bin/tv-native-player.rs` is the supervised GStreamer child. It declares
  `required-features = ["gstreamer-player"]`, so a default or
  `--no-default-features` build does not produce it and does not link
  GStreamer.
- `src/lib.rs` is the `tv_client` library.

## Headless gate

`scripts/check.sh` is the canonical check and runs, in order:

1. `cargo fmt --all -- --check`
2. `cargo clippy --locked --all-targets --no-default-features -- -D warnings`
3. `cargo test --locked --no-default-features`

Run it from anywhere; the script changes to the repository root first.
Because clippy and test use `--locked`, a committed `Cargo.lock` must exist and
match `Cargo.toml`.

The gate is executed on **x86_64 Linux** with no platform services present.
That host is the only architecture the headless gate validates. The C004
window adds `slint` and `slint-build` to `Cargo.toml`; after that change the
committed `Cargo.lock` must be regenerated (`cargo update` or a normal resolve)
before any `--locked` command can pass. Do not hand-edit the lockfile. The
headless gate itself does not compile any Slint crate into the library or
binary targets, and `build.rs` is a no-op without `native-ui`.

## Architecture targets

The deployment target is **aarch64 (Raspberry Pi 3B)**, while the repeatable
gate above runs on **x86_64 headless**. No aarch64 cross-compilation is part of
the gate. The native player was built and smoke-tested on the Pi itself (see
[`player.md`](player.md)); a green headless gate still says nothing about
playback behaviour on the Pi, it only establishes that the portable protocol
and state machine compile, lint and pass their tests on the CI host.

## Platform features

`Cargo.toml` declares five features:

- `gstreamer-player` enables the optional `gstreamer`, `gstreamer-video` and
  `gstreamer-audio` dependencies, the `tv-native-player` binary and the
  production child factory. The parent-side protocol and supervision code in
  `src/player/` is always compiled and does not need GStreamer.
- `native-ui` enables the optional `slint` dependency, `build.rs` compilation
  of `ui/app.slint`, and the Slint window launched by `src/main.rs`. The pure
  `src/ui.rs` view model and `tests/ui_navigation.rs` are always compiled and
  contain no Slint types.
- The supervisor, protocol and `ProgramChildFactory` are always compiled and
  backend neutral, so a future libmpv/yt-dlp child can be added without
  touching the gated GStreamer code.
- `cec` remains an empty array reserved for future feature-gated CEC work.
  The kernel CEC bridge in `src/input/cec.rs` is always compiled and is
  started by the `native-ui` entry point; it needs no Cargo feature.
  `airplay` and `webrtc` remain empty arrays that reserve future seams. No
  default feature is set, so a plain build stays headless and starts no CEC
  process.

The headless gate deliberately passes `--no-default-features`; it exercises
the protocol, the state machine, the UI view model and the scripted fake-child
tests without GStreamer or Slint. Build the native player explicitly:

```sh
cargo build --release --features gstreamer-player
```

Build the Slint launcher explicitly:

```sh
cargo build --release --features native-ui
```

## Native UI build

Slint 1.19.1 needs a C toolchain plus fontconfig and xkbcommon development
headers on Linux. The headless CI host used by this repository has no
`fontconfig.pc` and no passwordless sudo, so the feature check is run in a
container or on the Pi. A minimal container image is:

```sh
docker run --rm -it -v "$PWD:/work" -w /work rust:1.100-slim \
  sh -c 'apt-get update && apt-get install -y --no-install-recommends \
    libfontconfig1-dev libxkbcommon-dev pkg-config && \
    cargo check --locked --features native-ui'
```

Use the same two packages for a local Debian/Ubuntu host:
`libfontconfig1-dev libxkbcommon-dev`. The Pi already has
`fontconfig 2.15.0`, `xkbcommon 1.13.1`, `wayland-client 1.24.0`, EGL/GL and
`pkg-config`; no extra UI package install was needed for C004. On the Pi build
from a directory on the root filesystem with one job and no debug info:

```sh
source "$HOME/.cargo/env"
cd "$HOME/tv-client-c004"
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  cargo build --release --features native-ui
```

Run it inside the active labwc session with `WAYLAND_DISPLAY=wayland-0` and
`XDG_RUNTIME_DIR=/run/user/1000`. The chosen renderer is winit + FemtoVG
(OpenGL/EGL); see [`design.md`](design.md). `SLINT_BACKEND=winit-femtovg` may be
set explicitly to make the choice visible in diagnostics. Cage was installed on
the Pi but was not the active session at the C004/C010 baseline; the C013 kiosk
session uses Cage on the DRM backend (see [`release.md`](release.md)).

The Slint feature adds a large dependency graph. Do not build it in `/tmp` on
the Pi (453 MiB tmpfs); use `~` as above. If the Pi runs out of memory, stop
and report that instead of raising the job count.

## HDMI-CEC runtime helper

The kernel CEC bridge adds **no Cargo dependency and no build-time package**.
It runs the system `cec-ctl` tool from v4l-utils/cec-utils as a supervised
child with a fixed argument list:

```text
cec-ctl --device /dev/cec0 --playback --no-rc-passthrough --wait-for-msgs --skip-info
```

- The Pi has `cec-ctl` 1.30.1, `/dev/cec0` on the `vc4_hdmi` driver, and
  `anders` in the `video` group. The device is `crw-rw---- root:video`.
- Follow mode is used because `cec-ctl --monitor` requires root. Do **not**
  install libCEC or enable the root-only monitor mode for this client.
- The bridge starts only from the `native-ui` entry point and is stopped with a
  two-second bound during teardown; the headless build never spawns it.
- A missing helper or adapter is a typed `CecStatus::Unavailable` state logged
  at startup, not a fatal error. The USB keyboard remains usable.
- `--no-rc-passthrough` is required: it prevents the kernel from also emitting
  keyboard events for the same CEC press, which would create a duplicate input
  path.

If the helper is absent, install the distribution package that provides
`cec-ctl` (usually `v4l-utils` or `cec-utils`) or use the USB keyboard. The
bridge itself still builds and its tests run without the helper.

## Cage/GLES renderer for YouTube playback

The labwc session is **not** a supported YouTube playback renderer on the Pi
3B. In labwc, mpv 0.40 dropped about 19% of frames at 720p30 and 72% at
1080p30 with both `vo=gpu` and `vo=wlshm`; the drops are compositor/present
side, not network or decoder drops. Do not tune the client around labwc.

C013 runs the kiosk under **Cage**, and C004 UI behavior must be revalidated
there (fullscreen, focus, input, teardown). C010 hardware acceptance is local
720p30 under Cage with the GLES2 renderer; 1080p30 is explicitly unsupported on
the Pi 3B. The deployed session is defined by
`deploy/kiosk-profile.sh.example` (`WLR_RENDERER=gles2`, `unset WLR_BACKENDS`)
and `deploy/getty-override.conf.example`; `scripts/install-release.sh` installs
both and keeps `/etc/tv-client` and `/var/lib/tv-client` persistent.

Nested validation from an existing Wayland/labwc session:

```sh
WLR_BACKENDS=wayland WLR_RENDERER=gles2 \
  cage -- mpv --no-config --no-terminal --really-quiet \
    --vo=gpu --gpu-context=wayland --hwdec=v4l2m2m /path/to/local-test.mp4
```

This probes the renderer independently. The client itself starts mpv as a
child through private IPC; under a real Cage kiosk the child inherits Cage's
`WAYLAND_DISPLAY` and no manual mpv command is used.

A direct Cage kiosk session (the C013 shape) uses the DRM backend instead of
`WLR_BACKENDS=wayland`, with `WLR_RENDERER=gles2` exported in the session
environment. The nested Cage/GLES 720p30 local run measured **2 dropped frames
out of 240 at 30 fps** (PASS); the same path at 1080p dropped **151 frames**
(FAIL). Keep validation runs short, and do not power-cycle the panel.

The client policy is unchanged: 720p30 first, progressive H.264/AAC at 720p or
lower second, 1080p30 only as a last resort, and a hard 30 fps cap (no 1080p60
or 720p60). See [`youtube.md`](youtube.md) for the full matrix.

## GStreamer player build

On the Pi, install the GStreamer development headers first:
`libgstreamer1.0-dev` and `libgstreamer-plugins-base1.0-dev`. The runtime needs
`gstreamer1.0-plugins-base`, `gstreamer1.0-plugins-good`,
`gstreamer1.0-plugins-bad`/`ugly` for the chosen decoders, plus
`gstreamer1.0-libav` only if software H.264 fallback is desired.

Build on the Pi from a directory on the root filesystem (for example
`~/tv-client-c006`), not from `/tmp`: `/tmp` is a 453 MiB tmpfs on the lab
target, so a GStreamer build fills RAM, can kill the session, and is lost on
reboot. Use one compiler job (`CARGO_BUILD_JOBS=1`) and no debug info
(`CARGO_PROFILE_DEV_DEBUG=0`) on the 1 GiB Pi 3B. These C002/C006 build notes
are unchanged by the Slint window.

### Native player audio and controls

The native player opens audio through ALSA on the kiosk. The session profile
sets the defaults:

| Variable | Kiosk value | Meaning |
|---|---|---|
| `TV_AUDIO_SINK` | `alsa` | Prefer `alsasink`; unset keeps `autoaudiosink` for desktop use. |
| `TV_AUDIO_DEVICE` | `hw:0,0` | ALSA device of `card 0: vc4hdmi`; the child also tries the ALSA `default` device, `autoaudiosink`, and finally a silent `fakesink`. |
| `TV_PLAYER_OVERLAY` | unset (off) | `on`/`1` enables the optional `textoverlay` hint; the default avoids its per-frame `videoconvert` cost on the Pi. |

The overlay needs the Pango-backed `textoverlay` from
`gstreamer1.0-plugins-base`; the child probes it at startup and falls back to a
plain `waylandsink` when the probe fails. The installer adds `tvkiosk` to the
least-privilege `input` group so the child can read `/dev/input/event*` while
its own waylandsink surface has keyboard focus. The target already installs
`gstreamer1.0-plugins-base`; no extra build-time package is required.

The ALSA HDMI path, the overlay, evdev controls and the `input` group change
are **NOT RUN** on Raspberry Pi hardware in this change; only the
feature-independent headless tests cover the pure sink plan and key mapping.
See [`release.md`](release.md) for the operator steps.

## Dependencies

The direct dependencies are:

- `clap` (with the `derive` feature) for CLI parsing.
- `serde` (with the `derive` feature) and `toml` for strict TOML decoding.
- `serde_json` for the player protocol and Jellyfin responses.
- `tokio` (with `io-util`, `macros`, `net`, `process`, `rt`, `sync` and
  `time`) for the bounded async state loop, child pipes, timeouts, shutdown
  channel and the cloud WebSocket TCP stream.
- `rusqlite` with the `bundled` feature for the embedded SQLite database.
- `reqwest` for the Jellyfin HTTP client and the control-server HTTPS API.
- `futures-util` (with `std` and `sink`, no default features) for the
  WebSocket sink/stream adapters. It was already resolved in the existing
  lockfile; the cloud socket makes the direct use explicit.
- `tokio-tungstenite` 0.30 with `connect`, `handshake` and
  `rustls-tls-native-roots`, and without default features, for
  `/v1/ws/device`. Rustls uses the host trust store, so no OpenSSL
  development package is added. The device bearer travels in the
  `Authorization` header, never in the URL, and frame contents are not logged.
- `url` for strict URL parsing and redaction.
- `thiserror` for typed error enums.
- `tracing` for structured diagnostics.
- `uuid` (with `serde` and `v4`) for request ids and artwork file names.
- `gstreamer`, `gstreamer-video` and `gstreamer-audio` 0.25.4, optional and
  enabled only by `gstreamer-player`, for native playback.
- `slint` 1.19.1, optional and enabled only by `native-ui`, with only the
  `std`, `backend-winit`, `renderer-femtovg` and `compat-1-2` features. The
  default software-renderer, accessibility and system-tray features are not
  enabled, which keeps the Pi build smaller.
- `slint-build` 1.19.1, build-only, which compiles `ui/app.slint` when
  `native-ui` is enabled and is otherwise unused. It enables only `compat-1-18`
  (required by the crate's forward-compatibility check) and selects the
  `fluent` widget style explicitly; it does not pull the software-renderer
  resource pipeline because the launcher uses no images.
- `cec-ctl` is a **runtime** helper, not a Cargo dependency: the kernel CEC
  bridge spawns it with fixed arguments. It is present on the Pi (1.30.1) and
  absent on the headless CI host, where the bridge reports
  `CecStatus::Unavailable` and the feature-independent tests use fixture
  programs instead.
- `tempfile`, dev-only, for isolated test state directories and fixture CEC
  helpers.

Their transitive dependencies follow from these direct ones. The version
requirements live in `Cargo.toml`; exact resolved versions live in the
committed `Cargo.lock`. Adding the WebSocket crates changes the lockfile: run
`cargo update` (or a normal `cargo check`) once, review the diff, and commit
`Cargo.toml` and `Cargo.lock` together. Every gate command after that uses
`--locked`.

To update dependencies:

1. Adjust a version requirement in `Cargo.toml` if needed.
2. Run `cargo update` (or `cargo update -p <crate>`) to refresh `Cargo.lock`.
3. Run `bash scripts/check.sh` and confirm it exits 0.
4. Commit the `Cargo.toml` and `Cargo.lock` changes together.

Review the lockfile diff before committing; the pinned toolchain and CI both
build from `Cargo.lock` via `--locked`.

## Cloud WebSocket dependency

The cloud device socket is always compiled; it is not behind a Cargo feature.
This keeps the protocol, dedupe, pairing and reconnect code exercised by the
default headless gate. It adds `tokio-tungstenite` and an explicit
`futures-util` dependency but **no system package**:

- TLS is rustls with native roots, matching the existing `reqwest` stack. The
  host certificate store is read at runtime; there is no OpenSSL or
  `pkg-config` requirement.
- The WebSocket and HTTP client share the same HTTPS origin policy
  (`https://` only). Tests use a loopback `ws://` mock server and a recording
  HTTP transport; production code derives `wss://` from `server_url`.
- The device bearer is supplied only as an `Authorization` header. The URL,
  query string, argv, preferences and logs never contain it.
- The link uses bounded channels: 32 pending commands, 32 outbound frames and
  32 UI events. A full queue is a typed rejection, not unbounded buffering.
- The connection uses a 10-second handshake/request timeout, a 15-second ping,
  a 45-second inbound timeout and 1–60 second jittered reconnects. The pending
  command queue is per connection, so reconnect does not replay commands.

The dependency is built on x86_64 headless and aarch64. It has no GStreamer or
Slint requirement, so the native-player and native-UI feature checks are
unchanged. See [`cloud.md`](cloud.md) for enrollment and behavior.

## CI

`.github/workflows/ci.yml` runs on `ubuntu-latest` for every push and pull
request with `permissions: contents: read`. It checks out the repository with
`actions/checkout` pinned to a full commit SHA and then runs
`bash scripts/check.sh`. CI uses no secrets and no steps beyond those two, so
it exercises exactly the same gate as a local run.

## ARM64 release build

The deployable artifact is built by `.github/workflows/release.yml` on the
native GitHub-hosted **`ubuntu-24.04-arm`** runner. The workflow runs for `v*`
tags, pull requests and manual dispatch; it is not run by the C013 executor.
Every action is pinned to a full 40-character commit SHA. The top-level
permission is `contents: read`. The separate attest job is tag-only
(`startsWith(github.ref, 'refs/tags/v')`) and adds `id-token: write` and
`attestations: write` for keyless provenance. No repository secret or
production signing key is used, and pull requests cannot reach the attest job.

The build installs the pinned Rust 1.100.0 toolchain from
`rust-toolchain.toml`, plus `libfontconfig1-dev`, `libxkbcommon-dev`,
`pkg-config`, `libgstreamer1.0-dev`, `libgstreamer-plugins-base1.0-dev` and
`jq`, then builds exactly:

```sh
cargo build --release --locked --features native-ui,gstreamer-player
```

No cross-compilation from x86_64 is supported. The release artifact must be
produced on an aarch64 Debian/Ubuntu build host with the same native features;
the headless gate is not a substitute.

The job verifies both ELF files and their shared libraries before packaging:

```sh
file target/release/tv-client target/release/tv-native-player
ldd target/release/tv-client
ldd target/release/tv-native-player
```

`file` must report `ARM aarch64` (the workflow accepts the `AArch64` spelling)
and `ldd` must not report `not found`. Packaging is performed by
`scripts/package-release.sh` and normalises the archive metadata:

```sh
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) \
  scripts/package-release.sh --version "$version"
```

The tarball **`tv-client-linux-arm64-<version>.tar.gz`** contains exactly the
two binaries at its root plus `NOTICES` and `SBOM.cdx.json`.
`SHA256SUMS` is written next to it, and `NOTICES`/`SBOM.cdx.json` are also
copied to `dist/` for direct inspection. `SBOM.cdx.json` is CycloneDX 1.5
generated from
`cargo metadata --locked --format-version 1 --features native-ui,gstreamer-player`;
`NOTICES` lists every crate with its declared license and the project's
GPL-3.0-only notice. The release job also verifies that the tar root contains
exactly those four entries and that `sha256sum -c SHA256SUMS` passes for the
tarball, notices and SBOM.

Local equivalent on an ARM64 Debian 13 host (the package install is an operator
action, not part of `scripts/check.sh`):

```sh
sudo apt-get install --yes --no-install-recommends \
  libfontconfig1-dev libxkbcommon-dev pkg-config \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev jq
cargo build --release --locked --features native-ui,gstreamer-player
scripts/package-release.sh --version "$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)"
```

For an individual toolchain check, `scripts/package-release.sh --print-name
VERSION` prints the artifact name without running cargo, and `--inventory
METADATA OUTDIR` regenerates `SBOM.cdx.json` and `NOTICES` from a saved
`cargo metadata` document.

No release artifact has been produced from the C013 worktree. The workflow
exists and is policy-tested by `tests/release.rs`, but the artifact and its
attestation are **NOT BUILT**; [`release.md`](release.md) records that status
and the unrun operator matrix. Do not infer AArch64 behaviour from an x86_64
build.

## Raspberry Pi target (aarch64)

Verified on the lab target, a Raspberry Pi 3 Model B Rev 1.2 running Debian 13
(trixie) and kernel `6.18.39+rpt-rpi-v8` (aarch64). Full probe output and
playback evidence are in [`hardware-report.md`](hardware-report.md).

The target received operator-approved installs after the baseline:
`libgstreamer1.0-dev` (20:40), rustup 1.100.0 (20:43),
`libgstreamer-plugins-base1.0-dev` (20:45), `cage` 0.3.1 (20:47:58), and
`gstreamer1.0-tools` installed by the advisor. Cage was installed with operator
approval and was not started; the active session remained labwc.

- Rust 1.100.0 is installed with rustup under `~/.cargo` (the same version as
  `rust-toolchain.toml`). A non-login shell does not have it on `PATH`; run
  `source "$HOME/.cargo/env"` first, after which `cargo --version` and
  `rustc --version` report 1.100.0.
- GStreamer 1.26.2 runtime and development headers are present:
  `pkg-config --modversion gstreamer-1.0` and `gstreamer-video-1.0` both
  return 1.26.2, and `/usr/include/gstreamer-1.0/gst/gst.h` exists.
  `gst-inspect-1.0` and `gst-launch-1.0` report 1.26.2, and the
  `v4l2h264dec`, `waylandsink`, `kmssink`, `playbin3`, `alsasink` and
  `pulsesink` elements are installed.
- `gcc` 14.2.0 and `pkg-config` are available for linking.
- `cage` 0.3.1 is installed. At the C004/C010 lab baseline it was not started
  and the console session was labwc 0.20.2 on tty1, with
  `XDG_RUNTIME_DIR=/run/user/1000` and `WAYLAND_DISPLAY=wayland-0`. The C013
  kiosk session is Cage; see
  [`release.md`](release.md) for the unrun installation and reboot gates.
- `cec-ctl` 1.30.1 is installed. `cec-ctl --list-devices` reports
  `vc4_hdmi (vc4-hdmi): /dev/cec0`; the device is `crw-rw---- root:video` and
  `anders` is in the `video` group. Follow mode works without root; monitor
  mode does not. See [`input.md`](input.md) for the exact helper arguments,
  supervision behavior and the remote-input verification status.
- `bash scripts/probe-pi.sh` reruns the read-only capability probe from a
  development host; it needs only SSH access to the host alias `raspberry`
  (override with `PI_HOST=...`). It installs nothing and changes no
  configuration on the Pi; `GST_REGISTRY_UPDATE=no` is exported so
  `gst-inspect-1.0` does not rewrite
  `~/.cache/gstreamer-1.0/registry.aarch64.bin`.

## YouTube local playback

The YouTube client in `src/youtube/` is always compiled; it has **no Cargo
feature and no new build-time package**. It uses the already-required
`reqwest`, `tokio` (`process`, `net`, `time`) and `serde_json` crates, and it
talks to two runtime helpers:

- `mpv` (0.40 on the Pi) with its private JSON IPC socket. The Rust code does
  not link libmpv; `libmpv-dev` is present because the mpv binary needs it.
- A managed `yt-dlp` release. The distribution package (`/usr/bin/yt-dlp`
  2025.04.30 on the lab Pi) is **not** the supported playback dependency: it
  failed a public metadata test with `The page needs to be reloaded`. Install
  or update yt-dlp in a dedicated location on the root filesystem, for
  example:

  ```sh
  python3 -m venv "$HOME/.local/share/tv-client/yt-dlp"
  "$HOME/.local/share/tv-client/yt-dlp/bin/pip" install --upgrade yt-dlp
  "$HOME/.local/share/tv-client/yt-dlp/bin/yt-dlp" --version
  ```

  Do not install it under `/tmp` on the Pi (a 453 MiB tmpfs). Keep the
  executable path absolute and record the version when changing it. Update it
  deliberately with the same `pip install --upgrade yt-dlp` command after a
  playback failure that the client classifies as `stale_extractor`; this is
  the supported maintenance path for the unofficial extractor.

`ffmpeg` 7.1 is already present for mpv's local demuxing. There is no server
download, media relay or mux endpoint; the only muxing is local inside mpv.

### Configuration

```toml
youtube_playback_enabled = false
youtube_ytdlp_path = "/home/anders/.local/share/tv-client/yt-dlp/bin/yt-dlp"
youtube_mpv_path = "/usr/bin/mpv"
youtube_playback_timeout_secs = 60
youtube_hwdec = "v4l2m2m"          # "auto-safe" | "v4l2m2m" | "none"
```

- `youtube_playback_enabled` is the kill switch. It is off by default, and it
  is also effectively off when `youtube_ytdlp_path` is absent, not absolute,
  missing or not executable. A broken installation therefore fails safe
  instead of half-enabling the feature.
- `youtube_ytdlp_path` must be an absolute path without `..`; a relative or
  structurally invalid value fails strict configuration validation. Existence
  and executable bits are checked for readiness but are not fatal: a missing
  absolute path leaves playback disabled: a Play attempt reports
  `youtube_disabled`. `extractor_unavailable` is reserved for a configured
  extractor that passed readiness but cannot be spawned or used.
- `youtube_mpv_path` is optional; `mpv` is resolved from `PATH` when absent.
- `youtube_playback_timeout_secs` is the bounded resolve-and-start budget and
  must be `10..=300`.
- `youtube_hwdec` is a fixed allowlist mapped to mpv's `--hwdec=`; it is never
  taken from remote input.

### Verification

```sh
mpv --version
pkg-config --modversion mpv
/absolute/path/to/yt-dlp --version
ffmpeg -version | head -1
cargo test --locked --no-default-features --test youtube
```

The mock tests need no network, mpv or yt-dlp. One ignored live test exists for
an operator-run probe:

```sh
TV_CLIENT_YOUTUBE_LIVE_ID=<11-char id> \
TV_CLIENT_YOUTUBE_YTDLP=/absolute/path/to/yt-dlp \
TV_CLIENT_YOUTUBE_MPV=/usr/bin/mpv \
  cargo test --no-default-features --test youtube -- --ignored live_local_playback
```

Build the window and the media path on the Pi from a root-filesystem directory,
not `/tmp`, with one job and no debug info:

```sh
source "$HOME/.cargo/env"
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  cargo build --release --features native-ui
```

Playback requires the active Wayland session (`WAYLAND_DISPLAY=wayland-0`,
`XDG_RUNTIME_DIR=/run/user/1000`). See [`youtube.md`](youtube.md) for the
search contract, the format policy, failure classes and the measured Pi
renderer/decoder evidence.
