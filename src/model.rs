//! Typed application state, actions, events and effects.
//!
//! The types in this module mirror the state and command payloads in
//! `docs/protocol-v1.md`. Untrusted input is validated before it can change
//! state, and physical confirmation is modelled separately from wire actions.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Required length of a YouTube video identifier.
pub const YOUTUBE_ID_LEN: usize = 11;
/// Maximum byte length of an opaque Jellyfin item identifier.
pub const MAX_ITEM_ID_BYTES: usize = 128;
/// Highest accepted volume value.
pub const MAX_VOLUME: u8 = 100;
/// Initial volume used when no persisted value is available.
pub const DEFAULT_VOLUME: u8 = 50;

/// Top-level mode reported to remotes and rendered by the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Home,
    Browsing,
    Loading,
    Playing,
    Paused,
    Casting,
    Standby,
    Error,
}

impl Mode {
    /// Stable protocol spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Browsing => "browsing",
            Self::Loading => "loading",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Casting => "casting",
            Self::Standby => "standby",
            Self::Error => "error",
        }
    }

    /// Modes reachable from this mode by a single transition.
    ///
    /// A mode is always allowed to transition to itself; the reducer treats
    /// that case as a no-op instead of consulting this table.
    #[must_use]
    pub const fn allowed_transitions(self) -> &'static [Mode] {
        match self {
            Self::Home => &[Self::Browsing, Self::Loading, Self::Standby, Self::Error],
            Self::Browsing => &[Self::Home, Self::Loading, Self::Standby, Self::Error],
            Self::Loading => &[Self::Home, Self::Playing, Self::Standby, Self::Error],
            Self::Playing => &[
                Self::Home,
                Self::Loading,
                Self::Paused,
                Self::Casting,
                Self::Standby,
                Self::Error,
            ],
            Self::Paused => &[
                Self::Home,
                Self::Loading,
                Self::Playing,
                Self::Casting,
                Self::Standby,
                Self::Error,
            ],
            Self::Casting => &[
                Self::Home,
                Self::Loading,
                Self::Playing,
                Self::Paused,
                Self::Standby,
                Self::Error,
            ],
            Self::Standby => &[Self::Home],
            Self::Error => &[Self::Home, Self::Browsing, Self::Loading, Self::Standby],
        }
    }

    /// Whether a reducer transition from `self` to `next` is legal.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next || self.allowed_transitions().contains(&next)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Active media source. A local Jellyfin session never depends on the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    #[serde(rename = "jellyfin")]
    Jellyfin,
    #[serde(rename = "youtube")]
    YouTube,
    #[serde(rename = "airplay")]
    AirPlay,
    #[serde(rename = "webrtc")]
    WebRtc,
}

impl Source {
    /// Stable protocol and database spelling.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Jellyfin => "jellyfin",
            Self::YouTube => "youtube",
            Self::AirPlay => "airplay",
            Self::WebRtc => "webrtc",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl FromStr for Source {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "jellyfin" => Ok(Self::Jellyfin),
            "youtube" => Ok(Self::YouTube),
            "airplay" => Ok(Self::AirPlay),
            "webrtc" => Ok(Self::WebRtc),
            _ => Err("unknown media source"),
        }
    }
}

/// Direction of a navigation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
    Home,
}

/// Where an input or command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputOrigin {
    LocalKeyboard,
    LocalCec,
    Remote,
}

impl InputOrigin {
    /// Whether this input arrived over the network.
    #[must_use]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Remote)
    }

    /// Whether this input was produced on the physical device.
    #[must_use]
    pub const fn is_local(self) -> bool {
        !self.is_remote()
    }
}

/// A wire action, tagged by `kind` exactly as in protocol v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Navigate { direction: Direction },
    SetPaused { paused: bool },
    Seek { position_ms: u64 },
    SetVolume { volume: u8 },
    Stop,
    PlayYoutube { video_id: String },
    PlayJellyfin { item_id: String },
    Standby { enabled: bool },
}

impl Action {
    /// Reject values outside the protocol ranges.
    ///
    /// This is intentionally separate from deserialization: a decoded action
    /// still needs mode and permission checks before it changes state.
    pub fn validate(&self) -> Result<(), StateError> {
        match self {
            Self::SetVolume { volume } => {
                if *volume > MAX_VOLUME {
                    return Err(StateError::InvalidAction {
                        reason: "volume must be within 0..=100",
                    });
                }
            }
            Self::PlayYoutube { video_id } => {
                if !is_valid_youtube_id(video_id) {
                    return Err(StateError::InvalidAction {
                        reason: "youtube video id must be 11 URL-safe characters",
                    });
                }
            }
            Self::PlayJellyfin { item_id } => {
                if !is_valid_item_id(item_id) {
                    return Err(StateError::InvalidAction {
                        reason: "jellyfin item id must be opaque, 1..=128 bytes and not a URL",
                    });
                }
            }
            Self::Navigate { .. }
            | Self::SetPaused { .. }
            | Self::Seek { .. }
            | Self::Stop
            | Self::Standby { .. } => {}
        }
        Ok(())
    }
}

/// Whether a YouTube identifier matches the 11-character protocol form.
#[must_use]
pub fn is_valid_youtube_id(value: &str) -> bool {
    value.len() == YOUTUBE_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Whether an opaque item identifier is acceptable.
///
/// Identifiers are never URLs and are never interpreted as filesystem paths.
#[must_use]
pub fn is_valid_item_id(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_ITEM_ID_BYTES {
        return false;
    }
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    if value.contains("://") {
        return false;
    }
    let lowercase = value.to_ascii_lowercase();
    !lowercase.starts_with("http:")
}

/// Playback identity and generation allocated by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    /// Monotonically increasing generation for this process.
    pub generation: u64,
    /// Unique playback identifier passed to players and remotes.
    pub id: Uuid,
    /// Media source that owns the playback.
    pub source: Source,
    /// Opaque upstream item identifier, never a stream URL.
    pub item_id: String,
}

/// Complete local application state.
///
/// `playback` is reducer bookkeeping and is not part of the wire state
/// payload; the remaining fields serialize with the protocol field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    pub device_id: Option<Uuid>,
    pub mode: Mode,
    pub source: Option<Source>,
    pub item_id: Option<String>,
    pub title: Option<String>,
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub volume: u8,
    pub paused: bool,
    #[serde(rename = "error_code")]
    pub error: Option<String>,
    #[serde(skip)]
    pub playback: Option<Playback>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            device_id: None,
            mode: Mode::Home,
            source: None,
            item_id: None,
            title: None,
            position_ms: None,
            duration_ms: None,
            volume: DEFAULT_VOLUME,
            paused: false,
            error: None,
            playback: None,
        }
    }
}

/// Input or player event delivered to the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A command or local input, with the origin preserved for policy checks.
    Action { origin: InputOrigin, action: Action },
    /// A physical confirmation that must never be accepted from a remote.
    PhysicalApproval {
        origin: InputOrigin,
        request_id: Uuid,
    },
    /// The player has admitted the requested media.
    PlaybackStarted { generation: u64, playback_id: Uuid },
    /// The player reported playback progress.
    PlaybackProgress {
        generation: u64,
        playback_id: Uuid,
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    /// The player reported a pause-state change (for example from a web player).
    PlaybackPaused {
        generation: u64,
        playback_id: Uuid,
        paused: bool,
    },
    /// Playback reached its natural end.
    PlaybackEnded { generation: u64, playback_id: Uuid },
    /// Playback failed, with a bounded machine-readable code.
    PlaybackFailed {
        generation: u64,
        playback_id: Uuid,
        error_code: String,
    },
}

impl Event {
    /// Convenience constructor for command and local-input events.
    #[must_use]
    pub fn action(origin: InputOrigin, action: Action) -> Self {
        Self::Action { origin, action }
    }

    /// Convenience constructor for physical confirmation events.
    #[must_use]
    pub fn physical_approval(origin: InputOrigin, request_id: Uuid) -> Self {
        Self::PhysicalApproval { origin, request_id }
    }
}

/// Side effects emitted by the reducer.
///
/// Consumers perform the actual I/O; the reducer never blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    StartPlayback {
        generation: u64,
        playback_id: Uuid,
        source: Source,
        item_id: String,
    },
    StopPlayback {
        generation: u64,
        playback_id: Uuid,
    },
    SetPaused {
        generation: u64,
        playback_id: Uuid,
        paused: bool,
    },
    Seek {
        generation: u64,
        playback_id: Uuid,
        position_ms: u64,
    },
    SetVolume {
        volume: u8,
    },
    Navigate {
        direction: Direction,
    },
    Standby {
        enabled: bool,
    },
    ApprovePairing {
        request_id: Uuid,
    },
    Progress {
        item_id: String,
        position_ms: u64,
        duration_ms: Option<u64>,
    },
}

/// Reasons an event cannot change application state.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateError {
    #[error("illegal transition from {from} to {to}")]
    IllegalTransition { from: Mode, to: Mode },
    #[error("stale playback generation: expected {expected}, received {received}")]
    StaleGeneration { expected: u64, received: u64 },
    #[error("stale playback identifier")]
    StalePlayback { expected: Uuid, received: Uuid },
    #[error("no active playback")]
    NoActivePlayback,
    #[error("invalid action: {reason}")]
    InvalidAction { reason: &'static str },
    #[error("invalid value for {field}: {reason}")]
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
    #[error("remote input cannot approve physical confirmation")]
    RemoteApprovalDenied,
    #[error("playback generation exhausted")]
    GenerationExhausted,
    #[error("queue capacity must be greater than zero")]
    InvalidQueueCapacity,
    #[error("queue is full")]
    QueueFull,
    #[error("queue is closed")]
    QueueClosed,
}
