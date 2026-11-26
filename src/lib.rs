//! Headless core library for the `tv-client` package.
//!
//! Platform backends are gated behind the package feature flags declared in
//! `Cargo.toml` (`native-ui`, `gstreamer-player`, `cec`, `airplay`, `webrtc`);
//! none of them carry an implementation in this baseline.

pub mod config;
pub mod model;
pub mod state;
pub mod storage;

pub use config::ClientConfig;
pub use model::{Action, AppState, Effect, Event, InputOrigin, Mode, Source, StateError};
pub use state::{Reducer, event_channel, run_state_loop, shutdown_channel};
pub use storage::{LastView, ProgressRecord, Storage, StorageError};

/// Returns the package version compiled into this build.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
