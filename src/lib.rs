//! Core library for the `tv-client` package.
//!
//! The portable state, storage, Jellyfin, player-protocol, input and UI
//! view-model layers are always compiled. The GStreamer child binary and its
//! production factory are gated behind the `gstreamer-player` feature; the
//! Slint window and its bridge are gated behind `native-ui`. The `airplay` and
//! `webrtc` flags remain seams without an implementation; the kernel HDMI-CEC
//! bridge is runtime code, not a feature.

pub mod config;
pub mod input;
pub mod jellyfin;
pub mod model;
pub mod playback;
pub mod player;
pub mod state;
pub mod storage;
pub mod ui;

pub use config::ClientConfig;
pub use input::{
    CecBridge, CecConfig, CecDisconnect, CecError, CecParser, CecStatus, CecUnavailable,
    InputEvent, InputQueue, InputReject, LocalOrigin, PhysicalApproval, RepeatGate, Route, Router,
    ShutdownOutcome,
};
pub use jellyfin::{
    BaseItem, CancelHandle, CancelToken, JellyfinClient, JellyfinError, Page, QueryResult, Session,
    cancellation,
};
pub use model::{Action, AppState, Effect, Event, InputOrigin, Mode, Source, StateError};
pub use playback::{
    LibraryFilter, MAX_PROGRESS_AGE, MAX_PROGRESS_JOURNAL, MAX_PROGRESS_RETRIES, PlayRequest,
    PlaybackCommand, PlaybackConfig, PlaybackController, PlaybackError, PlaybackItem,
    PlaybackUpdate, ProgressEntry, ProgressJournal, RESUME_END_MARGIN_TICKS, RESUME_MIN_TICKS,
    TrackChoice, millis_from_ticks, ticks_from_millis,
};
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
