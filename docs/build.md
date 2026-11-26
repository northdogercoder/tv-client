# Building and checking tv-client

## Toolchain

The Rust toolchain is pinned in `rust-toolchain.toml` to **1.100.0** with the
`minimal` profile plus the `rustfmt` and `clippy` components. A rustup
installation honours that pin automatically, so no per-shell override is
needed. `Cargo.toml` sets `rust-version = "1.92"` as the package's minimum
supported Rust version, required by the locked dependency set and by the
optional GStreamer bindings (`gstreamer`, `gstreamer-video`,
`gstreamer-audio` 0.25.4). The pinned 1.100.0 toolchain is the version the
project builds and checks with.

Both binaries and the library target are produced from the single package
`tv-client`:

- `src/main.rs` is the headless `tv-client` binary.
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
That host is the only architecture the headless gate validates.

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
- The supervisor, protocol and `ProgramChildFactory` are always compiled and
  backend neutral, so a future libmpv/yt-dlp child can be added without
  touching the gated GStreamer code.
- `native-ui`, `cec`, `airplay` and `webrtc` remain empty arrays that reserve
  future seams. No default feature is set, so a plain build stays headless.

The headless gate deliberately passes `--no-default-features`; it exercises
the protocol, the state machine and the scripted fake-child tests without
GStreamer. Build the native player explicitly:

```sh
cargo build --release --features gstreamer-player
```

On the Pi, install the GStreamer development headers first:
`libgstreamer1.0-dev` and `libgstreamer-plugins-base1.0-dev`. The runtime needs
`gstreamer1.0-plugins-base`, `gstreamer1.0-plugins-good`,
`gstreamer1.0-plugins-bad`/`ugly` for the chosen decoders, plus
`gstreamer1.0-libav` only if software H.264 fallback is desired.

Build on the Pi from a directory on the root filesystem (for example
`~/tv-client-c006`), not from `/tmp`: `/tmp` is a 453 MiB tmpfs on the lab
target, so a GStreamer build fills RAM, can kill the session, and is lost on
reboot. Use one compiler job (`CARGO_BUILD_JOBS=1`) and no debug info
(`CARGO_PROFILE_DEV_DEBUG=0`) on the 1 GiB Pi 3B.

## Dependencies

The direct dependencies are:

- `clap` (with the `derive` feature) for CLI parsing.
- `serde` (with the `derive` feature) and `toml` for strict TOML decoding.
- `serde_json` for the player protocol and Jellyfin responses.
- `tokio` (with `io-util`, `macros`, `process`, `rt`, `sync` and `time`) for
  the bounded async state loop, child pipes, timeouts and shutdown channel.
- `rusqlite` with the `bundled` feature for the embedded SQLite database.
- `reqwest` for the Jellyfin HTTP client.
- `url` for strict URL parsing and redaction.
- `thiserror` for typed error enums.
- `tracing` for structured diagnostics.
- `uuid` (with `serde` and `v4`) for request ids and artwork file names.
- `gstreamer`, `gstreamer-video` and `gstreamer-audio` 0.25.4, optional and
  enabled only by `gstreamer-player`, for native playback.
- `tempfile`, dev-only, for isolated test state directories.

Their transitive dependencies follow from these direct ones. The version
requirements live in `Cargo.toml`; exact resolved versions live in the
committed `Cargo.lock`.

To update dependencies:

1. Adjust a version requirement in `Cargo.toml` if needed.
2. Run `cargo update` (or `cargo update -p <crate>`) to refresh `Cargo.lock`.
3. Run `bash scripts/check.sh` and confirm it exits 0.
4. Commit the `Cargo.toml` and `Cargo.lock` changes together.

Review the lockfile diff before committing; the pinned toolchain and CI both
build from `Cargo.lock` via `--locked`.

## CI

`.github/workflows/ci.yml` runs on `ubuntu-latest` for every push and pull
request with `permissions: contents: read`. It checks out the repository with
`actions/checkout` pinned to a full commit SHA and then runs
`bash scripts/check.sh`. CI uses no secrets and no steps beyond those two, so
it exercises exactly the same gate as a local run.

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
- `cage` 0.3.1 is installed but is not used. The active console session is
  labwc 0.20.2 on tty1, with `XDG_RUNTIME_DIR=/run/user/1000` and
  `WAYLAND_DISPLAY=wayland-0`.
- `bash scripts/probe-pi.sh` reruns the read-only capability probe from a
  development host; it needs only SSH access to the host alias `raspberry`
  (override with `PI_HOST=...`). It installs nothing and changes no
  configuration on the Pi; `GST_REGISTRY_UPDATE=no` is exported so
  `gst-inspect-1.0` does not rewrite
  `~/.cache/gstreamer-1.0/registry.aarch64.bin`.
