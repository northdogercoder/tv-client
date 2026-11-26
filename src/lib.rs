//! Headless core library for the `tv-client` package.
//!
//! Platform backends are gated behind the package feature flags declared in
//! `Cargo.toml` (`native-ui`, `gstreamer-player`, `cec`, `airplay`, `webrtc`);
//! none of them carry an implementation in this bootstrap baseline.

/// Returns the package version compiled into this build.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
