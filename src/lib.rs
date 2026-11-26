//! Core library for the `tv-client` package.
//!
//! The portable state, storage, Jellyfin and player-protocol layers are always
//! compiled. The GStreamer child binary and its production factory are gated
//! behind the `gstreamer-player` feature; the other platform feature flags
//! (`native-ui`, `cec`, `airplay`, `webrtc`) remain seams without an
//! implementation.

pub mod config;
pub mod jellyfin;
pub mod model;
pub mod player;
pub mod state;
pub mod storage;

pub use config::ClientConfig;
pub use jellyfin::{
    BaseItem, CancelHandle, CancelToken, JellyfinClient, JellyfinError, Page, QueryResult, Session,
    cancellation,
};
pub use model::{Action, AppState, Effect, Event, InputOrigin, Mode, Source, StateError};
pub use player::{
    ChildFactory, CommandOutcome, LoadRequest, PlayerCommand, PlayerError, PlayerEvent,
    PlayerFailure, PlayerMachine, PlayerNotification, PlayerRequest, PlayerSession, PlayerState,
    ProgramChildFactory, SupervisorConfig, Tracks,
};
pub use state::{Reducer, event_channel, run_state_loop, shutdown_channel};
pub use storage::{LastView, ProgressRecord, Storage, StorageError};

/// Returns the package version compiled into this build.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
