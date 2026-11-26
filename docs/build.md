# Building and checking tv-client

## Toolchain

The Rust toolchain is pinned in `rust-toolchain.toml` to **1.100.0** with the
`minimal` profile plus the `rustfmt` and `clippy` components. A rustup
installation honours that pin automatically, so no per-shell override is
needed. `Cargo.toml` sets `rust-version = "1.89"` as the package's minimum
supported Rust version, required by the locked dependency set. The pinned
1.100.0 toolchain is the version the project builds and checks with.

Both the binary and the library target are produced from the single package
`tv-client`: `src/main.rs` is the `tv-client` binary and `src/lib.rs` is the
`tv_client` library.

## Headless gate

`scripts/check.sh` is the canonical check and runs, in order:

1. `cargo fmt --all -- --check`
2. `cargo clippy --locked --all-targets --no-default-features -- -D warnings`
3. `cargo test --locked --no-default-features`

Run it from anywhere; the script changes to the repository root first.
Because clippy and test use `--locked`, a committed `Cargo.lock` must exist and
match `Cargo.toml`.

The gate is executed on **x86_64 Linux** with no platform services present.
That host is the only architecture this baseline validates.

## Architecture targets

The deployment target is **aarch64 (Raspberry Pi 3B)**, while the repeatable
gate above runs on **x86_64 headless**. No aarch64 cross-compilation, device
testing, or hardware validation is part of this baseline. A green headless
gate therefore says nothing about behaviour on the Pi; it only establishes
that the package compiles, lints, and passes its tests on the host that runs
CI.

## Platform features

`Cargo.toml` declares five features, each currently an empty array:

- `native-ui`
- `gstreamer-player`
- `cec`
- `airplay`
- `webrtc`

No feature enables any code yet, no default feature is set, and none of the
platform integrations has been implemented or validated. The features exist
only to reserve the intended seams; enabling one changes nothing observable.
The headless gate deliberately passes `--no-default-features`, and the
bootstrap tests exercise only the headless library and CLI.

## Dependencies

The direct dependencies are:

- `clap` (with the `derive` feature) for CLI parsing.
- `serde` (with the `derive` feature) and `toml` for strict TOML decoding.
- `tokio` (with `macros`, `rt`, `sync` and `time`) for the bounded async state
  loop and shutdown channel.
- `rusqlite` with the `bundled` feature for the embedded SQLite database.
- `thiserror` for typed error enums.
- `tracing` for structured diagnostics.
- `uuid` (with `serde` and `v4`) for request ids and artwork file names.
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
