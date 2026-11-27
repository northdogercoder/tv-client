//! Native player protocol, state machine and supervised child process.
//!
//! The parent UI never renders media itself. It starts the `tv-native-player`
//! child, speaks a bounded JSON-lines protocol over the child's stdin/stdout,
//! and owns all policy: URLs and authentication headers are sent over the pipe,
//! never through argv or environment variables.
//!
//! The always-compiled part of this module is feature-independent so the
//! headless gate (`--no-default-features`) exercises the protocol, the state
//! machine and the fake-child supervision path without linking GStreamer.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use url::Url;

use crate::state::{ShutdownHandle, ShutdownSignal, shutdown_channel};

/// Largest accepted protocol line, excluding the trailing newline.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Largest accepted number of HTTP headers in a load request.
pub const MAX_HEADERS: usize = 32;
/// Largest accepted total header name and value bytes.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Largest accepted media URL.
pub const MAX_URL_BYTES: usize = 2048;
/// Largest accepted child error code.
pub const MAX_ERROR_CODE_BYTES: usize = 128;
/// Largest accepted child error message after redaction.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 512;
/// Largest accepted decoder, sink or track description.
const MAX_NAME_BYTES: usize = 128;
/// Highest accepted volume value.
pub const MAX_VOLUME: u8 = 100;
/// Highest accepted absolute playback position (24 hours).
///
/// This keeps positions well inside GStreamer's `ClockTime` range and rejects
/// obviously hostile values before they reach a child.
pub const MAX_POSITION_MS: u64 = 24 * 60 * 60 * 1_000;
/// Initial volume when the parent has not set one yet.
pub const DEFAULT_VOLUME: u8 = 50;
/// Default time allowed between starting the child and its `ready` event.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Default time allowed between playback events while playing.
pub const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default time allowed for a stopped child to exit before it is killed.
pub const DEFAULT_REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// Default time allowed for the child process to start.
pub const DEFAULT_SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Default time allowed to hand one line to the child.
pub const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A load request as sent over the child pipe.
///
/// `Debug` never prints the URL query, fragment, userinfo or header values, so
/// a Jellyfin token cannot end up in a log line or `{:?}` assertion output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadRequest {
    /// Absolute `file:`, `http:` or `https:` URL.
    pub url: String,
    /// Optional HTTP headers, usually a Jellyfin token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Optional initial position in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_position_ms: Option<u64>,
}

impl LoadRequest {
    /// Build a load request without headers.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: None,
            start_position_ms: None,
        }
    }

    /// Validate the URL and headers.
    pub fn validate(&self) -> Result<(), PlayerError> {
        validate_media_url(&self.url)?;
        if self
            .start_position_ms
            .is_some_and(|position| position > MAX_POSITION_MS)
        {
            return Err(PlayerError::InvalidRequest {
                field: "start_position_ms exceeds 24 hours",
            });
        }
        validate_headers(self.headers.as_ref())
    }
}

impl fmt::Debug for LoadRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadRequest")
            .field("url", &redact_url(&self.url))
            .field(
                "headers",
                &format_args!(
                    "{} redacted header(s)",
                    self.headers.as_ref().map_or(0, BTreeMap::len)
                ),
            )
            .field("start_position_ms", &self.start_position_ms)
            .finish()
    }
}

/// Requests sent from the parent to the native-player child.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlayerRequest {
    /// Start playback of an absolute URL.
    Load(LoadRequest),
    /// Pause playback.
    Pause,
    /// Resume playback.
    Resume,
    /// Seek to an absolute position.
    Seek { position_ms: u64 },
    /// Set the linear volume, 0 through 100.
    SetVolume { volume: u8 },
    /// Stop playback and exit the child.
    Stop,
}

impl PlayerRequest {
    /// Reject malformed or out-of-range requests.
    pub fn validate(&self) -> Result<(), PlayerError> {
        match self {
            Self::Load(load) => load.validate(),
            Self::Seek { position_ms } => {
                if *position_ms > MAX_POSITION_MS {
                    return Err(PlayerError::InvalidRequest {
                        field: "position_ms exceeds 24 hours",
                    });
                }
                Ok(())
            }
            Self::Pause | Self::Resume | Self::Stop => Ok(()),
            Self::SetVolume { volume } => {
                if *volume > MAX_VOLUME {
                    return Err(PlayerError::InvalidRequest {
                        field: "volume must be within 0..=100",
                    });
                }
                Ok(())
            }
        }
    }
}

impl fmt::Debug for PlayerRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(load) => fmt::Debug::fmt(load, formatter),
            Self::Pause => formatter.write_str("Pause"),
            Self::Resume => formatter.write_str("Resume"),
            Self::Seek { position_ms } => formatter
                .debug_struct("Seek")
                .field("position_ms", position_ms)
                .finish(),
            Self::SetVolume { volume } => formatter
                .debug_struct("SetVolume")
                .field("volume", volume)
                .finish(),
            Self::Stop => formatter.write_str("Stop"),
        }
    }
}

/// Events emitted by the native-player child.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlayerEvent {
    /// The child admitted the media. Optional names identify the decoder and sink.
    Ready {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decoder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sink: Option<String>,
    },
    /// Current playback position and, when known, duration.
    Position {
        position_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Playback reached its natural end.
    Ended,
    /// Playback failed with a bounded machine-readable code and optional detail.
    Error {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Streams selected by the child, for diagnostics only.
    Tracks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        video: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
    },
    /// The user stopped playback from the native surface (`Back`/`Esc`).
    ///
    /// The child exits after this event, so the parent returns to the launcher
    /// instead of reporting an intentional exit as a playback failure.
    Stopped,
    /// A capability is missing or degraded but playback continues.
    Degraded {
        /// Bounded machine-readable reason, for example `input_unavailable`.
        code: String,
        /// Optional bounded detail; the child never sends secrets here.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl PlayerEvent {
    /// Redact and truncate free-form fields so the event is safe to log.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        match &mut self {
            Self::Ready { decoder, sink } => {
                *decoder = decoder.take().map(|value| sanitize_name(&value));
                *sink = sink.take().map(|value| sanitize_name(&value));
            }
            Self::Error { message, .. } => {
                *message = message
                    .take()
                    .map(|value| redact_text(&value))
                    .filter(|value| !value.is_empty());
            }
            Self::Tracks {
                video,
                audio,
                subtitle,
            } => {
                *video = video.take().map(|value| sanitize_name(&value));
                *audio = audio.take().map(|value| sanitize_name(&value));
                *subtitle = subtitle.take().map(|value| sanitize_name(&value));
            }
            Self::Degraded { code, message } => {
                if sanitize_code(code).is_none() {
                    *code = "degraded".to_string();
                }
                *message = message
                    .take()
                    .map(|value| redact_text(&value))
                    .filter(|value| !value.is_empty());
            }
            Self::Position { .. } | Self::Ended | Self::Stopped => {}
        }
        self
    }
}

impl fmt::Debug for PlayerEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready { decoder, sink } => formatter
                .debug_struct("Ready")
                .field("decoder", decoder)
                .field("sink", sink)
                .finish(),
            Self::Position {
                position_ms,
                duration_ms,
            } => formatter
                .debug_struct("Position")
                .field("position_ms", position_ms)
                .field("duration_ms", duration_ms)
                .finish(),
            Self::Ended => formatter.write_str("Ended"),
            Self::Error { code, message } => formatter
                .debug_struct("Error")
                .field("code", code)
                .field("message", &message.as_deref().map(redact_text))
                .finish(),
            Self::Tracks {
                video,
                audio,
                subtitle,
            } => formatter
                .debug_struct("Tracks")
                .field("video", video)
                .field("audio", audio)
                .field("subtitle", subtitle)
                .finish(),
            Self::Stopped => formatter.write_str("Stopped"),
            Self::Degraded { code, message } => formatter
                .debug_struct("Degraded")
                .field("code", code)
                .field("message", &message.as_deref().map(redact_text))
                .finish(),
        }
    }
}

/// Playback state owned by the parent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerState {
    /// No media is loaded.
    #[default]
    Idle,
    /// A child was started and the media is being opened.
    Loading,
    /// Media is playing.
    Playing,
    /// Media is paused.
    Paused,
    /// Media reached its natural end.
    Ended,
    /// Playback failed.
    Error,
}

impl PlayerState {
    /// Stable lowercase spelling used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for PlayerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Bounded failure information retained for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerFailure {
    /// Machine-readable code such as `gst_error` or `child_hung`.
    pub code: String,
    /// Redacted, bounded human-readable detail.
    pub message: Option<String>,
}

/// Stream descriptions reported by the child.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracks {
    /// Selected video stream description.
    pub video: Option<String>,
    /// Selected audio stream description.
    pub audio: Option<String>,
    /// Selected subtitle stream description.
    pub subtitle: Option<String>,
}

/// Commands accepted from the UI or remote-control layer.
#[derive(Clone, PartialEq, Eq)]
pub enum PlayerCommand {
    /// Load an authenticated URL and start playback.
    Load(LoadRequest),
    /// Pause the current playback.
    Pause,
    /// Resume paused playback.
    Resume,
    /// Seek to an absolute position.
    Seek { position_ms: u64 },
    /// Set the volume from 0 through 100.
    SetVolume { volume: u8 },
    /// Stop playback and tear the child down.
    Stop,
}

impl fmt::Debug for PlayerCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(load) => formatter.debug_tuple("Load").field(load).finish(),
            Self::Pause => formatter.write_str("Pause"),
            Self::Resume => formatter.write_str("Resume"),
            Self::Seek { position_ms } => formatter
                .debug_struct("Seek")
                .field("position_ms", position_ms)
                .finish(),
            Self::SetVolume { volume } => formatter
                .debug_struct("SetVolume")
                .field("volume", volume)
                .finish(),
            Self::Stop => formatter.write_str("Stop"),
        }
    }
}

/// Notifications published to the parent UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerNotification {
    /// The state machine moved to a new state.
    State { generation: u64, state: PlayerState },
    /// The child admitted the media.
    Ready {
        generation: u64,
        decoder: Option<String>,
        sink: Option<String>,
    },
    /// A fresh playback position.
    Position {
        generation: u64,
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    /// Streams selected by the child.
    Tracks {
        generation: u64,
        video: Option<String>,
        audio: Option<String>,
        subtitle: Option<String>,
    },
    /// Playback reached its natural end.
    Ended { generation: u64 },
    /// Playback failed; `code` is bounded and `message` is redacted.
    Failed {
        generation: u64,
        code: String,
        message: Option<String>,
    },
    /// A capability is degraded but playback continues.
    Degraded {
        generation: u64,
        code: String,
        message: Option<String>,
    },
}

/// Result of one accepted command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    /// Generation the command applies to.
    pub generation: u64,
    /// Request to send to the child, if any.
    pub request: Option<PlayerRequest>,
    /// State notifications produced by the command.
    pub notifications: Vec<PlayerNotification>,
}

/// Errors produced by the protocol, state machine or supervision layer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlayerError {
    /// A protocol line exceeded [`MAX_LINE_BYTES`].
    #[error("player protocol line exceeds {MAX_LINE_BYTES} bytes")]
    LineTooLong,
    /// A protocol line was not valid UTF-8.
    #[error("player protocol line is not valid UTF-8")]
    InvalidUtf8,
    /// A protocol line was not valid JSON.
    #[error("player protocol line is not valid JSON")]
    InvalidJson,
    /// A request field failed validation; the message never contains the value.
    #[error("invalid player request field: {field}")]
    InvalidRequest { field: &'static str },
    /// An event field failed validation; the message never contains the value.
    #[error("invalid player event field: {field}")]
    InvalidEvent { field: &'static str },
    /// A command is not legal in the current state.
    #[error("player command is not valid in state {state}")]
    InvalidState { state: PlayerState },
    /// An event belongs to an older load generation.
    #[error("stale player generation {received}, expected {expected}")]
    StaleGeneration { expected: u64, received: u64 },
    /// The child pipe or notification queue is closed.
    #[error("player channel is closed")]
    Closed,
    /// The child process failed with a bounded code.
    #[error("player child failed: {code}")]
    ChildFailed { code: String },
    /// A supervisor or session parameter is invalid.
    #[error("invalid player configuration: {field}")]
    InvalidConfig { field: &'static str },
}

impl PlayerError {
    /// Bounded code used for UI failure events.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::LineTooLong | Self::InvalidUtf8 | Self::InvalidJson => "protocol",
            Self::InvalidRequest { .. } | Self::InvalidEvent { .. } => "invalid_request",
            Self::InvalidState { .. } | Self::StaleGeneration { .. } => "invalid_state",
            Self::Closed => "channel_closed",
            Self::ChildFailed { code } => code,
            Self::InvalidConfig { .. } => "invalid_config",
        }
    }
}

/// Validate that a URL is absolute and uses an allowed scheme.
pub fn validate_media_url(value: &str) -> Result<(), PlayerError> {
    if value.is_empty() || value.len() > MAX_URL_BYTES {
        return Err(PlayerError::InvalidRequest { field: "url" });
    }
    if value.chars().any(char::is_control) {
        return Err(PlayerError::InvalidRequest { field: "url" });
    }
    let url = Url::parse(value).map_err(|_| PlayerError::InvalidRequest { field: "url" })?;
    if !matches!(url.scheme(), "file" | "http" | "https") {
        return Err(PlayerError::InvalidRequest { field: "url" });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PlayerError::InvalidRequest { field: "url" });
    }
    Ok(())
}

fn validate_headers(headers: Option<&BTreeMap<String, String>>) -> Result<(), PlayerError> {
    let Some(headers) = headers else {
        return Ok(());
    };
    if headers.len() > MAX_HEADERS {
        return Err(PlayerError::InvalidRequest {
            field: "headers count",
        });
    }
    let mut total = 0_usize;
    for (name, value) in headers {
        if !is_valid_header_name(name) {
            return Err(PlayerError::InvalidRequest {
                field: "header name",
            });
        }
        if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(PlayerError::InvalidRequest {
                field: "header value",
            });
        }
        total = total.saturating_add(name.len()).saturating_add(value.len());
    }
    if total > MAX_HEADER_BYTES {
        return Err(PlayerError::InvalidRequest {
            field: "header bytes",
        });
    }
    Ok(())
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Encode a request as one JSON line without the trailing newline.
pub fn encode_request(request: &PlayerRequest) -> Result<String, PlayerError> {
    request.validate()?;
    let line = serde_json::to_string(request).map_err(|_| PlayerError::InvalidJson)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(PlayerError::LineTooLong);
    }
    Ok(line)
}

/// Decode one request line and validate its fields.
pub fn decode_request_line(line: &str) -> Result<PlayerRequest, PlayerError> {
    let line = strip_line_ending(line)?;
    let request: PlayerRequest =
        serde_json::from_str(line).map_err(|_| PlayerError::InvalidJson)?;
    request.validate()?;
    Ok(request)
}

/// Encode an event as one JSON line without the trailing newline.
pub fn encode_event(event: &PlayerEvent) -> Result<String, PlayerError> {
    let line = serde_json::to_string(event).map_err(|_| PlayerError::InvalidJson)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(PlayerError::LineTooLong);
    }
    Ok(line)
}

/// Decode one event line, sanitizing free-form fields.
pub fn decode_event_line(line: &str) -> Result<PlayerEvent, PlayerError> {
    let line = strip_line_ending(line)?;
    let event: PlayerEvent = serde_json::from_str(line).map_err(|_| PlayerError::InvalidJson)?;
    match &event {
        PlayerEvent::Error { code, .. } => {
            if sanitize_code(code).is_none() {
                return Err(PlayerError::InvalidEvent {
                    field: "error code",
                });
            }
        }
        PlayerEvent::Degraded { code, .. } => {
            if sanitize_code(code).is_none() {
                return Err(PlayerError::InvalidEvent {
                    field: "degraded code",
                });
            }
        }
        PlayerEvent::Position {
            position_ms,
            duration_ms,
        } => {
            if let Some(duration) = *duration_ms
                && *position_ms > duration
                && position_ms.saturating_sub(duration) > 60_000
            {
                // The child may overshoot by a frame; the machine clamps. Only
                // reject obviously absurd values that could overflow UI math.
                return Err(PlayerError::InvalidEvent {
                    field: "position_ms",
                });
            }
        }
        PlayerEvent::Ready { .. }
        | PlayerEvent::Ended
        | PlayerEvent::Stopped
        | PlayerEvent::Tracks { .. } => {}
    }
    Ok(event.sanitized())
}

fn strip_line_ending(line: &str) -> Result<&str, PlayerError> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.len() > MAX_LINE_BYTES {
        return Err(PlayerError::LineTooLong);
    }
    if line.is_empty() {
        return Err(PlayerError::InvalidJson);
    }
    Ok(line)
}

/// Read one newline-terminated UTF-8 line without growing past the protocol bound.
///
/// The returned string does not include the newline. An over-long line is
/// rejected as soon as the bound is crossed, so a hostile child cannot make the
/// parent allocate without limit.
pub async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<String>, PlayerError>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| PlayerError::Closed)?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                finish_bounded_line(bytes)
            };
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if bytes.len().saturating_add(position) > MAX_LINE_BYTES {
                return Err(PlayerError::LineTooLong);
            }
            bytes.extend_from_slice(&available[..position]);
            reader.consume(position + 1);
            return finish_bounded_line(bytes);
        }
        bytes.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if bytes.len() > MAX_LINE_BYTES {
            return Err(PlayerError::LineTooLong);
        }
    }
}

fn finish_bounded_line(bytes: Vec<u8>) -> Result<Option<String>, PlayerError> {
    let mut bytes = bytes;
    if bytes.last() == Some(&b'\r') {
        let _ = bytes.pop();
    }
    if bytes.len() > MAX_LINE_BYTES {
        return Err(PlayerError::LineTooLong);
    }
    if bytes.is_empty() {
        return Ok(Some(String::new()));
    }
    let line = String::from_utf8(bytes).map_err(|_| PlayerError::InvalidUtf8)?;
    Ok(Some(line))
}

/// Remove credentials, query and fragment from a URL for logging.
#[must_use]
pub fn redact_url(value: &str) -> String {
    match Url::parse(value) {
        Ok(mut url) => {
            let _ = url.set_password(None);
            if !url.username().is_empty() {
                let _ = url.set_username("redacted");
            }
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "<invalid-url>".to_string(),
    }
}

/// Redact URLs and control bytes from untrusted diagnostic text.
#[must_use]
pub fn redact_text(input: &str) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(start) = find_url_start(rest) {
        output.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|character: char| {
                character.is_whitespace() || character == '"' || character == '\''
            })
            .unwrap_or(tail.len());
        output.push_str(&redact_url(&tail[..end]));
        rest = &tail[end..];
    }
    output.push_str(rest);
    let cleaned: String = output
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    truncate_bytes(&cleaned, MAX_ERROR_MESSAGE_BYTES)
}

fn find_url_start(value: &str) -> Option<usize> {
    let http = value.find("http://");
    let https = value.find("https://");
    match (http, https) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn truncate_bytes(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn sanitize_name(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    truncate_bytes(cleaned.trim(), MAX_NAME_BYTES)
}

fn sanitize_code(code: &str) -> Option<String> {
    if code.is_empty() || code.len() > MAX_ERROR_CODE_BYTES {
        return None;
    }
    if !code
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return None;
    }
    Some(code.to_string())
}

/// Pure playback state machine.
///
/// It performs no I/O and never blocks, so tests can drive every transition and
/// every stale-event path without a real decoder.
#[derive(Debug, Clone)]
pub struct PlayerMachine {
    generation: u64,
    state: PlayerState,
    position_ms: Option<u64>,
    duration_ms: Option<u64>,
    volume: u8,
    tracks: Tracks,
    decoder: Option<String>,
    sink: Option<String>,
    failure: Option<PlayerFailure>,
}

impl Default for PlayerMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerMachine {
    /// Create an idle machine with no generation used.
    #[must_use]
    pub fn new() -> Self {
        Self {
            generation: 0,
            state: PlayerState::Idle,
            position_ms: None,
            duration_ms: None,
            volume: DEFAULT_VOLUME,
            tracks: Tracks::default(),
            decoder: None,
            sink: None,
            failure: None,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> PlayerState {
        self.state
    }

    /// Generation of the current or most recent load.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Last known position.
    #[must_use]
    pub fn position_ms(&self) -> Option<u64> {
        self.position_ms
    }

    /// Last known duration.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// Current volume.
    #[must_use]
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Selected streams.
    #[must_use]
    pub fn tracks(&self) -> &Tracks {
        &self.tracks
    }

    /// Decoder reported by the child.
    #[must_use]
    pub fn decoder(&self) -> Option<&str> {
        self.decoder.as_deref()
    }

    /// Video sink reported by the child.
    #[must_use]
    pub fn sink(&self) -> Option<&str> {
        self.sink.as_deref()
    }

    /// Last failure, if any.
    #[must_use]
    pub fn failure(&self) -> Option<&PlayerFailure> {
        self.failure.as_ref()
    }

    /// Apply a command, returning the request to forward and state changes.
    ///
    /// Idempotent commands (`Pause` while paused, `Resume` while playing,
    /// `Stop` while idle) succeed without producing work.
    pub fn apply_command(&mut self, command: PlayerCommand) -> Result<CommandOutcome, PlayerError> {
        let generation = self.generation;
        match command {
            PlayerCommand::Load(load) => {
                load.validate()?;
                self.generation = self.generation.saturating_add(1);
                self.state = PlayerState::Loading;
                self.position_ms = Some(load.start_position_ms.unwrap_or(0));
                self.duration_ms = None;
                self.tracks = Tracks::default();
                self.decoder = None;
                self.sink = None;
                self.failure = None;
                Ok(CommandOutcome {
                    generation: self.generation,
                    request: Some(PlayerRequest::Load(load)),
                    notifications: vec![PlayerNotification::State {
                        generation: self.generation,
                        state: PlayerState::Loading,
                    }],
                })
            }
            PlayerCommand::Pause => match self.state {
                PlayerState::Playing => {
                    self.state = PlayerState::Paused;
                    Ok(self.outcome_with(
                        Some(PlayerRequest::Pause),
                        vec![PlayerNotification::State {
                            generation,
                            state: PlayerState::Paused,
                        }],
                    ))
                }
                PlayerState::Paused => Ok(self.empty_outcome()),
                _ => Err(PlayerError::InvalidState { state: self.state }),
            },
            PlayerCommand::Resume => match self.state {
                PlayerState::Paused => {
                    self.state = PlayerState::Playing;
                    Ok(self.outcome_with(
                        Some(PlayerRequest::Resume),
                        vec![PlayerNotification::State {
                            generation,
                            state: PlayerState::Playing,
                        }],
                    ))
                }
                PlayerState::Playing => Ok(self.empty_outcome()),
                _ => Err(PlayerError::InvalidState { state: self.state }),
            },
            PlayerCommand::Seek { position_ms } => match self.state {
                PlayerState::Playing | PlayerState::Paused => {
                    if position_ms > MAX_POSITION_MS {
                        return Err(PlayerError::InvalidRequest {
                            field: "position_ms exceeds 24 hours",
                        });
                    }
                    if self
                        .duration_ms
                        .is_some_and(|duration| position_ms > duration)
                    {
                        return Err(PlayerError::InvalidRequest {
                            field: "position_ms exceeds duration",
                        });
                    }
                    self.position_ms = Some(position_ms);
                    Ok(self.outcome_with(Some(PlayerRequest::Seek { position_ms }), Vec::new()))
                }
                _ => Err(PlayerError::InvalidState { state: self.state }),
            },
            PlayerCommand::SetVolume { volume } => {
                if volume > MAX_VOLUME {
                    return Err(PlayerError::InvalidRequest {
                        field: "volume must be within 0..=100",
                    });
                }
                self.volume = volume;
                Ok(self.outcome_with(Some(PlayerRequest::SetVolume { volume }), Vec::new()))
            }
            PlayerCommand::Stop => {
                if self.state == PlayerState::Idle {
                    return Ok(self.empty_outcome());
                }
                self.state = PlayerState::Idle;
                self.position_ms = None;
                self.duration_ms = None;
                self.tracks = Tracks::default();
                self.decoder = None;
                self.sink = None;
                self.failure = None;
                Ok(self.outcome_with(
                    None,
                    vec![PlayerNotification::State {
                        generation,
                        state: PlayerState::Idle,
                    }],
                ))
            }
        }
    }

    /// Apply one child event for a specific load generation.
    pub fn apply_event(
        &mut self,
        generation: u64,
        event: PlayerEvent,
    ) -> Result<Vec<PlayerNotification>, PlayerError> {
        if generation != self.generation {
            return Err(PlayerError::StaleGeneration {
                expected: self.generation,
                received: generation,
            });
        }
        let event = event.sanitized();
        match event {
            PlayerEvent::Ready { decoder, sink } => {
                if !matches!(
                    self.state,
                    PlayerState::Loading | PlayerState::Playing | PlayerState::Paused
                ) {
                    return Err(PlayerError::InvalidState { state: self.state });
                }
                self.decoder = decoder;
                self.sink = sink;
                let mut notifications = vec![PlayerNotification::Ready {
                    generation,
                    decoder: self.decoder.clone(),
                    sink: self.sink.clone(),
                }];
                if self.state == PlayerState::Loading {
                    self.state = PlayerState::Playing;
                    notifications.push(PlayerNotification::State {
                        generation,
                        state: PlayerState::Playing,
                    });
                }
                Ok(notifications)
            }
            PlayerEvent::Position {
                position_ms,
                duration_ms,
            } => {
                if !matches!(
                    self.state,
                    PlayerState::Loading | PlayerState::Playing | PlayerState::Paused
                ) {
                    return Err(PlayerError::InvalidState { state: self.state });
                }
                if let Some(duration) = duration_ms {
                    self.duration_ms = Some(duration);
                }
                let position = self
                    .duration_ms
                    .map_or(position_ms, |duration| position_ms.min(duration));
                self.position_ms = Some(position);
                Ok(vec![PlayerNotification::Position {
                    generation,
                    position_ms: position,
                    duration_ms: self.duration_ms,
                }])
            }
            PlayerEvent::Ended => {
                if !matches!(
                    self.state,
                    PlayerState::Loading | PlayerState::Playing | PlayerState::Paused
                ) {
                    return Err(PlayerError::InvalidState { state: self.state });
                }
                if let Some(duration) = self.duration_ms {
                    self.position_ms = Some(duration);
                }
                self.state = PlayerState::Ended;
                Ok(vec![
                    PlayerNotification::Ended { generation },
                    PlayerNotification::State {
                        generation,
                        state: PlayerState::Ended,
                    },
                ])
            }
            PlayerEvent::Error { code, message } => {
                if self.state == PlayerState::Idle {
                    return Err(PlayerError::InvalidState { state: self.state });
                }
                let code = sanitize_code(&code).unwrap_or_else(|| "child_error".to_string());
                self.failure = Some(PlayerFailure {
                    code: code.clone(),
                    message: message.clone(),
                });
                self.state = PlayerState::Error;
                Ok(vec![
                    PlayerNotification::Failed {
                        generation,
                        code,
                        message,
                    },
                    PlayerNotification::State {
                        generation,
                        state: PlayerState::Error,
                    },
                ])
            }
            PlayerEvent::Tracks {
                video,
                audio,
                subtitle,
            } => {
                if !matches!(
                    self.state,
                    PlayerState::Loading | PlayerState::Playing | PlayerState::Paused
                ) {
                    return Err(PlayerError::InvalidState { state: self.state });
                }
                self.tracks = Tracks {
                    video: video.clone(),
                    audio: audio.clone(),
                    subtitle: subtitle.clone(),
                };
                Ok(vec![PlayerNotification::Tracks {
                    generation,
                    video,
                    audio,
                    subtitle,
                }])
            }
            PlayerEvent::Stopped => {
                // The child exits after this event. An idempotent stop (for
                // example the parent already sent `Stop`) changes nothing.
                if self.state == PlayerState::Idle {
                    return Ok(Vec::new());
                }
                self.state = PlayerState::Idle;
                self.position_ms = None;
                self.duration_ms = None;
                self.tracks = Tracks::default();
                self.decoder = None;
                self.sink = None;
                self.failure = None;
                Ok(vec![PlayerNotification::State {
                    generation,
                    state: PlayerState::Idle,
                }])
            }
            PlayerEvent::Degraded { code, message } => {
                if self.state == PlayerState::Idle {
                    return Ok(Vec::new());
                }
                Ok(vec![PlayerNotification::Degraded {
                    generation,
                    code: sanitize_code(&code).unwrap_or_else(|| "degraded".to_string()),
                    message,
                }])
            }
        }
    }

    /// Force a failure for the current generation, for example a hung child.
    pub fn fail(
        &mut self,
        generation: u64,
        code: &str,
        message: Option<String>,
    ) -> Result<Vec<PlayerNotification>, PlayerError> {
        self.apply_event(
            generation,
            PlayerEvent::Error {
                code: code.to_string(),
                message,
            },
        )
    }

    fn empty_outcome(&self) -> CommandOutcome {
        CommandOutcome {
            generation: self.generation,
            request: None,
            notifications: Vec::new(),
        }
    }

    fn outcome_with(
        &self,
        request: Option<PlayerRequest>,
        notifications: Vec<PlayerNotification>,
    ) -> CommandOutcome {
        CommandOutcome {
            generation: self.generation,
            request,
            notifications,
        }
    }
}

/// Configuration for the supervising task.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Maximum time from child start to the `ready` event.
    pub ready_timeout: Duration,
    /// Maximum silence between events while playing.
    pub event_timeout: Duration,
    /// Time allowed for a stopped child to exit before it is killed.
    pub reap_timeout: Duration,
    /// Time allowed for the child process to start.
    pub spawn_timeout: Duration,
    /// Time allowed to write one line to the child.
    pub send_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            ready_timeout: DEFAULT_READY_TIMEOUT,
            event_timeout: DEFAULT_EVENT_TIMEOUT,
            reap_timeout: DEFAULT_REAP_TIMEOUT,
            spawn_timeout: DEFAULT_SPAWN_TIMEOUT,
            send_timeout: DEFAULT_SEND_TIMEOUT,
        }
    }
}

impl SupervisorConfig {
    fn validate(&self) -> Result<(), PlayerError> {
        for (field, value) in [
            ("ready_timeout", self.ready_timeout),
            ("event_timeout", self.event_timeout),
            ("reap_timeout", self.reap_timeout),
            ("spawn_timeout", self.spawn_timeout),
            ("send_timeout", self.send_timeout),
        ] {
            if value.is_zero() {
                return Err(PlayerError::InvalidConfig { field });
            }
        }
        Ok(())
    }
}

/// Command sent to the child-owning actor.
#[derive(Debug)]
pub enum ChildCommand {
    /// Write one JSON line to the child's stdin.
    Line(String),
    /// Terminate the child immediately.
    Kill,
}

/// Message produced by the child-owning actor.
#[derive(Debug)]
pub enum ChildMessage {
    /// One JSON line from the child's stdout.
    Line(String),
    /// The child process exited and was reaped.
    Exited(ChildExit),
    /// The child pipe failed.
    Io { code: &'static str },
}

/// Exit status of a reaped child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildExit {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Process exit code, when the platform provides one.
    pub code: Option<i32>,
}

impl ChildExit {
    /// Stable, secret-free description for failure notifications.
    #[must_use]
    pub fn description(self) -> String {
        match (self.success, self.code) {
            (_, Some(code)) => format!("exited with status {code}"),
            (true, None) => "exited successfully".to_string(),
            (false, None) => "was terminated by a signal".to_string(),
        }
    }
}

impl From<Result<std::process::ExitStatus, std::io::Error>> for ChildExit {
    fn from(status: Result<std::process::ExitStatus, std::io::Error>) -> Self {
        match status {
            Ok(status) => Self {
                success: status.success(),
                code: status.code(),
            },
            Err(_) => Self {
                success: false,
                code: None,
            },
        }
    }
}

/// Channels to one running child.
///
/// Tests construct this directly with a fake child actor; production uses
/// [`SupervisedChild::spawn_process`].
pub struct SupervisedChild {
    control: mpsc::Sender<ChildCommand>,
    messages: mpsc::Receiver<ChildMessage>,
}

impl SupervisedChild {
    /// Assemble a child handle from channels owned by a fake or external actor.
    #[must_use]
    pub fn from_channels(
        control: mpsc::Sender<ChildCommand>,
        messages: mpsc::Receiver<ChildMessage>,
    ) -> Self {
        Self { control, messages }
    }

    /// Spawn the child process and start its stdout reader and reaper.
    ///
    /// The actor always waits on the child after a kill request, so a
    /// terminated child is reaped rather than left as a zombie.
    pub async fn spawn_process(mut command: tokio::process::Command) -> Result<Self, PlayerError> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| PlayerError::ChildFailed {
            code: "spawn_failed".to_string(),
        })?;
        let mut stdin = child.stdin.take().ok_or(PlayerError::ChildFailed {
            code: "spawn_failed".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or(PlayerError::ChildFailed {
            code: "spawn_failed".to_string(),
        })?;
        let (control, mut control_rx) = mpsc::channel::<ChildCommand>(8);
        let (message_tx, messages) = mpsc::channel::<ChildMessage>(16);
        let reader_tx = message_tx.clone();
        let reader = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            loop {
                match read_bounded_line(&mut reader).await {
                    Ok(Some(line)) => {
                        if reader_tx.send(ChildMessage::Line(line)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let _ = reader_tx
                            .send(ChildMessage::Io {
                                code: "child_output",
                            })
                            .await;
                        break;
                    }
                }
            }
        });
        tokio::spawn(async move {
            let mut killed = false;
            loop {
                tokio::select! {
                    command = control_rx.recv() => match command {
                        Some(ChildCommand::Line(line)) => {
                            if stdin.write_all(line.as_bytes()).await.is_err()
                                || stdin.write_all(b"\n").await.is_err()
                                || stdin.flush().await.is_err()
                            {
                                break;
                            }
                        }
                        Some(ChildCommand::Kill) | None => {
                            killed = true;
                            break;
                        }
                    },
                    status = child.wait() => {
                        // The stdout reader must finish first: a child that
                        // wrote `stopped` just before exiting may still have
                        // that line buffered, and the supervisor must not see
                        // the exit before the intentional-stop event.
                        let _ = reader.await;
                        let _ = message_tx.send(ChildMessage::Exited(status.into())).await;
                        return;
                    }
                }
            }
            if killed {
                let _ = child.start_kill();
            }
            let status = child.wait().await;
            let _ = reader.await;
            let _ = message_tx.send(ChildMessage::Exited(status.into())).await;
        });
        Ok(Self { control, messages })
    }
}

/// Factory for supervised children.
///
/// This is the backend-neutral seam: a GStreamer child, a libmpv/yt-dlp child
/// or a test fake all implement the same trait and speak the same JSON-lines
/// protocol. The supervisor never sees backend-specific types.
pub trait ChildFactory: Send {
    /// Start one child and return its handle.
    fn spawn(&mut self) -> impl Future<Output = Result<SupervisedChild, PlayerError>> + Send;
}

/// Runs a supervised child program that speaks the player protocol.
///
/// The factory is backend-agnostic: it only starts the program, wires its
/// stdin/stdout pipes and reaps it. The C010 libmpv backend can add its own
/// binary and use this factory without touching the gated GStreamer module.
#[derive(Debug, Clone)]
pub struct ProgramChildFactory {
    program: PathBuf,
}

impl ProgramChildFactory {
    /// Use an explicit player binary path.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Locate a player binary next to the current executable.
    pub fn sibling(binary: &str) -> Result<Self, PlayerError> {
        if binary.is_empty() || binary.contains('/') {
            return Err(PlayerError::InvalidConfig {
                field: "child binary name",
            });
        }
        let current = std::env::current_exe().map_err(|_| PlayerError::ChildFailed {
            code: "spawn_failed".to_string(),
        })?;
        let directory = current.parent().ok_or(PlayerError::ChildFailed {
            code: "spawn_failed".to_string(),
        })?;
        Ok(Self::new(directory.join(binary)))
    }
}

impl ChildFactory for ProgramChildFactory {
    fn spawn(&mut self) -> impl Future<Output = Result<SupervisedChild, PlayerError>> + Send {
        let program = self.program.clone();
        async move {
            let command = tokio::process::Command::new(program);
            SupervisedChild::spawn_process(command).await
        }
    }
}

/// Convenience factory for the GStreamer `tv-native-player` child.
#[cfg(feature = "gstreamer-player")]
#[derive(Debug, Clone)]
pub struct GStreamerChildFactory {
    inner: ProgramChildFactory,
}

#[cfg(feature = "gstreamer-player")]
impl GStreamerChildFactory {
    /// Use an explicit GStreamer player binary path.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            inner: ProgramChildFactory::new(program),
        }
    }

    /// Locate `tv-native-player` next to the current executable.
    pub fn sibling() -> Result<Self, PlayerError> {
        Ok(Self {
            inner: ProgramChildFactory::sibling("tv-native-player")?,
        })
    }
}

#[cfg(feature = "gstreamer-player")]
impl ChildFactory for GStreamerChildFactory {
    fn spawn(&mut self) -> impl Future<Output = Result<SupervisedChild, PlayerError>> + Send {
        self.inner.spawn()
    }
}

struct ActiveChild {
    control: mpsc::Sender<ChildCommand>,
    messages: mpsc::Receiver<ChildMessage>,
}

impl ActiveChild {
    fn new(child: SupervisedChild) -> Self {
        Self {
            control: child.control,
            messages: child.messages,
        }
    }
}

/// Run the supervising task until shutdown, queue closure or child teardown.
///
/// Every load gets a fresh generation and a fresh child. Events are accepted
/// only for the active generation; stale events are rejected. A child that
/// does not become ready or stops producing progress is killed and reaped, and
/// a hung child is reported as `child_hung` instead of blocking the UI.
pub async fn run_supervisor<F>(
    mut factory: F,
    config: SupervisorConfig,
    mut commands: mpsc::Receiver<PlayerCommand>,
    notifications: mpsc::Sender<PlayerNotification>,
    mut shutdown: ShutdownSignal,
) -> Result<PlayerMachine, PlayerError>
where
    F: ChildFactory,
{
    config.validate()?;
    let mut machine = PlayerMachine::new();
    let mut active: Option<ActiveChild> = None;
    let mut ready_deadline: Option<Instant> = None;
    let mut event_deadline: Option<Instant> = None;

    loop {
        let deadline = match (ready_deadline, event_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };

        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                teardown(&mut active, &config).await;
                return Ok(machine);
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    teardown(&mut active, &config).await;
                    return Ok(machine);
                };
                let is_load = matches!(command, PlayerCommand::Load(_));
                let is_stop = matches!(command, PlayerCommand::Stop);
                let outcome = match machine.apply_command(command) {
                    Ok(outcome) => outcome,
                    Err(_) => continue,
                };
                emit(&notifications, outcome.notifications).await?;
                if is_load {
                    teardown(&mut active, &config).await;
                    ready_deadline = None;
                    event_deadline = None;
                    if let Some(request) = outcome.request {
                        match start_child(&mut factory, &config).await {
                            Ok(child) => {
                                let mut child = ActiveChild::new(child);
                                match send_request(&mut child, &config, &request).await {
                                    Ok(()) => {
                                        let volume = PlayerRequest::SetVolume {
                                            volume: machine.volume(),
                                        };
                                        match send_request(&mut child, &config, &volume).await {
                                            Ok(()) => {
                                                ready_deadline =
                                                    Some(Instant::now() + config.ready_timeout);
                                                active = Some(child);
                                            }
                                            Err(error) => {
                                                ready_deadline = None;
                                                event_deadline = None;
                                                fail_with(
                                                    &mut machine,
                                                    &notifications,
                                                    error.code(),
                                                )
                                                .await?;
                                                teardown_child(child, &config).await;
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        ready_deadline = None;
                                        event_deadline = None;
                                        fail_with(&mut machine, &notifications, error.code()).await?;
                                        teardown_child(child, &config).await;
                                    }
                                }
                            }
                            Err(error) => {
                                ready_deadline = None;
                                event_deadline = None;
                                fail_with(&mut machine, &notifications, error.code()).await?;
                            }
                        }
                    }
                } else {
                    if let Some(request) = outcome.request
                        && let Some(child) = active.as_mut()
                        && let Err(error) = send_request(child, &config, &request).await
                    {
                        ready_deadline = None;
                        event_deadline = None;
                        fail_with(&mut machine, &notifications, error.code()).await?;
                        teardown(&mut active, &config).await;
                    }
                    match machine.state() {
                        PlayerState::Playing => {
                            ready_deadline = None;
                            event_deadline = Some(Instant::now() + config.event_timeout);
                        }
                        PlayerState::Paused => {
                            ready_deadline = None;
                            event_deadline = None;
                        }
                        PlayerState::Idle
                        | PlayerState::Ended
                        | PlayerState::Error => event_deadline = None,
                        PlayerState::Loading => {}
                    }
                }
                if is_stop {
                    teardown(&mut active, &config).await;
                    ready_deadline = None;
                    event_deadline = None;
                }
            }
            message = next_child_message(&mut active) => {
                match message {
                    Some(ChildMessage::Line(line)) => {
                        match decode_event_line(&line) {
                            Ok(event) => {
                                let generation = machine.generation();
                                match machine.apply_event(generation, event) {
                                    Ok(notes) => {
                                        emit(&notifications, notes).await?;
                                        match machine.state() {
                                            PlayerState::Playing => {
                                                ready_deadline = None;
                                                event_deadline =
                                                    Some(Instant::now() + config.event_timeout);
                                            }
                                            PlayerState::Paused => {
                                                ready_deadline = None;
                                                event_deadline = None;
                                            }
                                            PlayerState::Ended | PlayerState::Error => {
                                                ready_deadline = None;
                                                event_deadline = None;
                                                teardown(&mut active, &config).await;
                                            }
                                            PlayerState::Idle | PlayerState::Loading => {}
                                        }
                                    }
                                    Err(_) => {
                                        ready_deadline = None;
                                        event_deadline = None;
                                        fail_with(&mut machine, &notifications, "protocol").await?;
                                        teardown(&mut active, &config).await;
                                    }
                                }
                            }
                            Err(_) => {
                                ready_deadline = None;
                                event_deadline = None;
                                fail_with(&mut machine, &notifications, "protocol").await?;
                                teardown(&mut active, &config).await;
                            }
                        }
                    }
                    Some(ChildMessage::Exited(exit)) => {
                        let state = machine.state();
                        active = None;
                        ready_deadline = None;
                        event_deadline = None;
                        if matches!(
                            state,
                            PlayerState::Loading | PlayerState::Playing | PlayerState::Paused
                        ) {
                            let code = if exit.success {
                                "child_exited"
                            } else {
                                "child_failed"
                            };
                            fail_with(
                                &mut machine,
                                &notifications,
                                code,
                            )
                            .await?;
                        }
                    }
                    Some(ChildMessage::Io { code }) => {
                        ready_deadline = None;
                        event_deadline = None;
                        fail_with(&mut machine, &notifications, code).await?;
                        teardown(&mut active, &config).await;
                    }
                    None => {
                        active = None;
                        ready_deadline = None;
                        event_deadline = None;
                        fail_with(&mut machine, &notifications, "child_exited").await?;
                    }
                }
            }
            () = wait_deadline(deadline) => {
                ready_deadline = None;
                event_deadline = None;
                fail_with(&mut machine, &notifications, "child_hung").await?;
                teardown(&mut active, &config).await;
            }
        }
    }
}

async fn next_child_message(active: &mut Option<ActiveChild>) -> Option<ChildMessage> {
    match active.as_mut() {
        Some(child) => child.messages.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn emit(
    notifications: &mpsc::Sender<PlayerNotification>,
    values: Vec<PlayerNotification>,
) -> Result<(), PlayerError> {
    for value in values {
        notifications
            .send(value)
            .await
            .map_err(|_| PlayerError::Closed)?;
    }
    Ok(())
}

async fn fail_with(
    machine: &mut PlayerMachine,
    notifications: &mpsc::Sender<PlayerNotification>,
    code: &str,
) -> Result<(), PlayerError> {
    let generation = machine.generation();
    match machine.fail(generation, code, None) {
        Ok(notes) => emit(notifications, notes).await,
        Err(_) => Ok(()),
    }
}

async fn start_child<F>(
    factory: &mut F,
    config: &SupervisorConfig,
) -> Result<SupervisedChild, PlayerError>
where
    F: ChildFactory,
{
    match tokio::time::timeout(config.spawn_timeout, factory.spawn()).await {
        Ok(result) => result,
        Err(_) => Err(PlayerError::ChildFailed {
            code: "spawn_timeout".to_string(),
        }),
    }
}

async fn send_request(
    child: &mut ActiveChild,
    config: &SupervisorConfig,
    request: &PlayerRequest,
) -> Result<(), PlayerError> {
    let line = encode_request(request)?;
    match tokio::time::timeout(
        config.send_timeout,
        child.control.send(ChildCommand::Line(line)),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(PlayerError::ChildFailed {
            code: "write_failed".to_string(),
        }),
    }
}

async fn teardown(active: &mut Option<ActiveChild>, config: &SupervisorConfig) {
    if let Some(child) = active.take() {
        teardown_child(child, config).await;
    }
}

async fn teardown_child(mut child: ActiveChild, config: &SupervisorConfig) {
    if let Ok(line) = encode_request(&PlayerRequest::Stop) {
        let _ = tokio::time::timeout(
            config.send_timeout,
            child.control.send(ChildCommand::Line(line)),
        )
        .await;
    }
    if wait_for_exit(&mut child.messages, config.reap_timeout).await {
        return;
    }
    let _ = tokio::time::timeout(config.send_timeout, child.control.send(ChildCommand::Kill)).await;
    let _ = wait_for_exit(&mut child.messages, config.reap_timeout).await;
}

async fn wait_for_exit(messages: &mut mpsc::Receiver<ChildMessage>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, messages.recv()).await {
            Ok(Some(ChildMessage::Exited(_))) => return true,
            Ok(Some(_)) => {}
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

/// Owned player session: command sender, notification receiver and join handle.
///
/// A session is the exclusive owner of one media backend for its lifetime: it
/// spawns at most one child, replaces it on `Load`, and reaps it on `Stop` or
/// shutdown. Switching backends means stopping one session and starting another
/// with a different [`ChildFactory`]; no backend-specific code lives here.
pub struct PlayerSession {
    commands: mpsc::Sender<PlayerCommand>,
    notifications: mpsc::Receiver<PlayerNotification>,
    shutdown: ShutdownHandle,
    task: JoinHandle<Result<PlayerMachine, PlayerError>>,
}

impl PlayerSession {
    /// Start a supervising task with bounded command and notification queues.
    pub fn start<F>(
        factory: F,
        config: SupervisorConfig,
        command_capacity: usize,
        notification_capacity: usize,
    ) -> Result<Self, PlayerError>
    where
        F: ChildFactory + 'static,
    {
        if command_capacity == 0 || notification_capacity == 0 {
            return Err(PlayerError::InvalidConfig {
                field: "queue capacity",
            });
        }
        config.validate()?;
        let (commands, command_rx) = mpsc::channel(command_capacity);
        let (notification_tx, notifications) = mpsc::channel(notification_capacity);
        let (shutdown, signal) = shutdown_channel();
        let task = tokio::spawn(run_supervisor(
            factory,
            config,
            command_rx,
            notification_tx,
            signal,
        ));
        Ok(Self {
            commands,
            notifications,
            shutdown,
            task,
        })
    }

    /// Send one command, waiting for queue capacity.
    pub async fn send(&self, command: PlayerCommand) -> Result<(), PlayerError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| PlayerError::Closed)
    }

    /// Receive the next notification.
    pub async fn next(&mut self) -> Option<PlayerNotification> {
        self.notifications.recv().await
    }

    /// Request shutdown, drop the command channel and wait for the task.
    pub async fn shutdown(self) -> Result<PlayerMachine, PlayerError> {
        let Self {
            commands,
            notifications,
            shutdown,
            task,
        } = self;
        shutdown.cancel();
        drop(commands);
        drop(notifications);
        match task.await {
            Ok(result) => result,
            Err(_) => Err(PlayerError::Closed),
        }
    }
}

/// Planning for the native player's audio output.
///
/// The GStreamer child is a separate process. On the kiosk it runs as
/// `tvkiosk`, which has no PulseAudio or PipeWire session, so a `pulsesink`
/// builds successfully and then fails (or stays silent) when it opens. The
/// kiosk profile therefore selects the direct ALSA path with
/// `TV_AUDIO_SINK=alsa` and `TV_AUDIO_DEVICE=hw:0,0`; a generic desktop keeps
/// the auto-detected sink. The ordered candidate list is pure so every choice
/// is testable without GStreamer or an audio device.
pub mod audio {
    /// Environment variable selecting the preferred audio sink.
    ///
    /// Accepted values are `alsa`, `pulse` and `auto` (the default when unset).
    pub const SINK_ENV: &str = "TV_AUDIO_SINK";
    /// Environment variable selecting the ALSA device for `TV_AUDIO_SINK=alsa`.
    pub const DEVICE_ENV: &str = "TV_AUDIO_DEVICE";
    /// ALSA device of the Raspberry Pi `vc4hdmi` card (`card 0: vc4hdmi`).
    pub const DEFAULT_HDMI_DEVICE: &str = "hw:0,0";
    /// Fallback ALSA device resolved by the host's ALSA configuration.
    pub const DEFAULT_ALSA_DEVICE: &str = "default";
    /// Last-resort sink that keeps video playing when no audio device opens.
    pub const SILENT_FACTORY: &str = "fakesink";
    /// Largest accepted ALSA device string.
    pub const MAX_DEVICE_BYTES: usize = 64;

    /// One GStreamer audio sink and its optional ALSA device.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AudioSinkSpec {
        factory: &'static str,
        device: Option<String>,
    }

    impl AudioSinkSpec {
        /// GStreamer factory name, for example `alsasink`.
        #[must_use]
        pub fn factory(&self) -> &'static str {
            self.factory
        }

        /// ALSA device when the factory takes one.
        #[must_use]
        pub fn device(&self) -> Option<&str> {
            self.device.as_deref()
        }

        /// Bounded, secret-free description for logs and the `ready` event.
        #[must_use]
        pub fn label(&self) -> String {
            match self.device.as_deref() {
                Some(device) => format!("{} device={device}", self.factory),
                None => self.factory.to_string(),
            }
        }
    }

    /// Ordered audio-sink candidates and the configuration warning, if any.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AudioSinkPlan {
        candidates: Vec<AudioSinkSpec>,
        warning: Option<&'static str>,
    }

    impl AudioSinkPlan {
        /// Read the plan from `TV_AUDIO_SINK` and `TV_AUDIO_DEVICE`.
        #[must_use]
        pub fn from_env() -> Self {
            Self::from_values(
                env_value(SINK_ENV).as_deref(),
                env_value(DEVICE_ENV).as_deref(),
            )
        }

        /// Build a plan from explicit values; `None` means unset.
        #[must_use]
        pub fn from_values(sink: Option<&str>, device: Option<&str>) -> Self {
            let requested = sink
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_ascii_lowercase);
            let device = device.and_then(sanitize_device);
            match requested.as_deref() {
                None | Some("auto") => Self {
                    candidates: vec![auto_sink()],
                    warning: None,
                },
                Some("alsa") => {
                    let mut candidates = Vec::with_capacity(3);
                    candidates.push(AudioSinkSpec {
                        factory: "alsasink",
                        device: Some(device.unwrap_or_else(|| DEFAULT_HDMI_DEVICE.to_string())),
                    });
                    if candidates[0].device.as_deref() != Some(DEFAULT_ALSA_DEVICE) {
                        candidates.push(AudioSinkSpec {
                            factory: "alsasink",
                            device: Some(DEFAULT_ALSA_DEVICE.to_string()),
                        });
                    }
                    candidates.push(auto_sink());
                    Self {
                        candidates,
                        warning: None,
                    }
                }
                Some("pulse") | Some("pulseaudio") => Self {
                    candidates: vec![
                        AudioSinkSpec {
                            factory: "pulsesink",
                            device: None,
                        },
                        auto_sink(),
                    ],
                    warning: None,
                },
                Some(_) => Self {
                    candidates: vec![auto_sink()],
                    warning: Some("unknown_audio_sink"),
                },
            }
        }

        /// Candidates in preference order.
        #[must_use]
        pub fn candidates(&self) -> &[AudioSinkSpec] {
            &self.candidates
        }

        /// Bounded warning code when the environment value was not recognized.
        #[must_use]
        pub fn warning(&self) -> Option<&'static str> {
            self.warning
        }

        /// One-line, secret-free summary for diagnostics.
        #[must_use]
        pub fn describe(&self) -> String {
            let mut text = String::new();
            for candidate in &self.candidates {
                if !text.is_empty() {
                    text.push_str(" > ");
                }
                text.push_str(&candidate.label());
            }
            text
        }
    }

    /// Final fallback used by the child after every real audio sink failed.
    #[must_use]
    pub fn silent_fallback() -> AudioSinkSpec {
        AudioSinkSpec {
            factory: SILENT_FACTORY,
            device: None,
        }
    }

    /// Reject newlines, whitespace and control bytes in a device string.
    #[must_use]
    pub fn sanitize_device(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_DEVICE_BYTES {
            return None;
        }
        let valid = trimmed.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b':' | b',' | b'-' | b'_' | b'=' | b'+' | b'.' | b'/')
        });
        valid.then(|| trimmed.to_string())
    }

    /// ALSA card number of `hw:N,D` or `plughw:N,D`.
    #[must_use]
    pub fn alsa_card(device: &str) -> Option<u32> {
        let device = device.trim();
        let rest = device
            .strip_prefix("plughw:")
            .or_else(|| device.strip_prefix("hw:"))?;
        rest.split(',').next()?.parse::<u32>().ok()
    }

    fn auto_sink() -> AudioSinkSpec {
        AudioSinkSpec {
            factory: "autoaudiosink",
            device: None,
        }
    }

    fn env_value(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// Remote/keyboard mapping for the fullscreen native-player surface.
///
/// `waylandsink` owns a separate Wayland surface, so cage delivers keyboard
/// events to that surface and the UI process cannot see them. The child reads
/// Linux evdev key events directly instead of inventing a second window. This
/// module is the pure, bounded mapping from evdev key codes to player actions,
/// plus the status text of the on-screen `textoverlay` hint.
pub mod controls {
    use super::truncate_bytes;

    /// EV_KEY events are the only ones mapped.
    pub const EVDEV_KEY: u16 = 1;
    /// Size of one Linux `input_event` structure on this pointer width.
    #[cfg(target_pointer_width = "64")]
    pub const EVDEV_EVENT_BYTES: usize = 24;
    /// Size of one Linux `input_event` structure on 32-bit targets.
    #[cfg(target_pointer_width = "32")]
    pub const EVDEV_EVENT_BYTES: usize = 16;
    /// evdev `KEY_ESC`.
    pub const KEY_ESC: u16 = 1;
    /// evdev `KEY_SPACE`.
    pub const KEY_SPACE: u16 = 57;
    /// evdev `KEY_UP`.
    pub const KEY_UP: u16 = 103;
    /// evdev `KEY_LEFT`.
    pub const KEY_LEFT: u16 = 105;
    /// evdev `KEY_RIGHT`.
    pub const KEY_RIGHT: u16 = 106;
    /// evdev `KEY_DOWN`.
    pub const KEY_DOWN: u16 = 108;
    /// evdev `KEY_VOLUMEDOWN`.
    pub const KEY_VOLUMEDOWN: u16 = 114;
    /// evdev `KEY_VOLUMEUP`.
    pub const KEY_VOLUMEUP: u16 = 115;
    /// evdev `KEY_STOP`.
    pub const KEY_STOP: u16 = 128;
    /// evdev `KEY_BACK` (remote Back).
    pub const KEY_BACK: u16 = 158;
    /// evdev `KEY_PLAYPAUSE`.
    pub const KEY_PLAYPAUSE: u16 = 164;
    /// evdev `KEY_REWIND`.
    pub const KEY_REWIND: u16 = 168;
    /// evdev `KEY_EXIT`.
    pub const KEY_EXIT: u16 = 174;
    /// evdev `KEY_FASTFORWARD`.
    pub const KEY_FASTFORWARD: u16 = 208;
    /// Seek distance for one Left/Right press.
    pub const SEEK_STEP_MS: u64 = 10_000;
    /// Volume change for one Up/Down press, in protocol units.
    pub const VOLUME_STEP: u8 = 5;
    /// Longest hint text handed to `textoverlay`.
    pub const MAX_HINT_BYTES: usize = 240;
    /// Key legend shown while evdev input is available.
    pub const LEGEND: &str = "ESC stop   SPACE pause   LEFT/RIGHT seek 10s   UP/DOWN volume";
    /// Legend shown when no input device could be opened.
    pub const NO_INPUT_LEGEND: &str = "TV controls unavailable - check the input group";

    /// One bounded action derived from a key press.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum KeyAction {
        /// Stop playback and exit the child.
        Stop,
        /// Toggle between playing and paused.
        TogglePause,
        /// Seek backwards by [`SEEK_STEP_MS`].
        SeekBackward,
        /// Seek forwards by [`SEEK_STEP_MS`].
        SeekForward,
        /// Raise the volume by [`VOLUME_STEP`].
        VolumeUp,
        /// Lower the volume by [`VOLUME_STEP`].
        VolumeDown,
    }

    /// Map one evdev key press to an action.
    ///
    /// Only `value == 1` (press) is accepted: releases and kernel repeats are
    /// ignored so a held or noisy device cannot create an unbounded action
    /// stream. Unknown codes are ignored, never executed.
    #[must_use]
    pub fn map_key(code: u16, value: i32) -> Option<KeyAction> {
        if value != 1 {
            return None;
        }
        Some(match code {
            KEY_ESC | KEY_BACK | KEY_STOP | KEY_EXIT => KeyAction::Stop,
            KEY_SPACE | KEY_PLAYPAUSE => KeyAction::TogglePause,
            KEY_LEFT | KEY_REWIND => KeyAction::SeekBackward,
            KEY_RIGHT | KEY_FASTFORWARD => KeyAction::SeekForward,
            KEY_UP | KEY_VOLUMEUP => KeyAction::VolumeUp,
            KEY_DOWN | KEY_VOLUMEDOWN => KeyAction::VolumeDown,
            _ => return None,
        })
    }

    /// Parse one Linux `input_event` buffer and map a key press.
    #[must_use]
    pub fn parse_evdev(bytes: &[u8]) -> Option<KeyAction> {
        if bytes.len() != EVDEV_EVENT_BYTES {
            return None;
        }
        let tail = &bytes[EVDEV_EVENT_BYTES - 8..];
        let event_type = u16::from_ne_bytes([tail[0], tail[1]]);
        if event_type != EVDEV_KEY {
            return None;
        }
        let code = u16::from_ne_bytes([tail[2], tail[3]]);
        let value = i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]);
        map_key(code, value)
    }

    /// New absolute position for a seek action, clamped to the known duration.
    #[must_use]
    pub fn seek_target(position_ms: u64, duration_ms: Option<u64>, forward: bool) -> u64 {
        let target = if forward {
            position_ms.saturating_add(SEEK_STEP_MS)
        } else {
            position_ms.saturating_sub(SEEK_STEP_MS)
        };
        let target = duration_ms.map_or(target, |duration| target.min(duration));
        target.min(super::MAX_POSITION_MS)
    }

    /// Volume after one step, clamped to the protocol range.
    #[must_use]
    pub fn volume_step(volume: u8, up: bool) -> u8 {
        if up {
            volume.saturating_add(VOLUME_STEP).min(super::MAX_VOLUME)
        } else {
            volume.saturating_sub(VOLUME_STEP)
        }
    }

    /// Compose the `textoverlay` hint, bounded and free of control bytes.
    #[must_use]
    pub fn hint_text(controls_available: bool, status: &str) -> String {
        let legend = if controls_available {
            LEGEND
        } else {
            NO_INPUT_LEGEND
        };
        let status: String = status
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        let status = status.trim();
        let text = if status.is_empty() {
            legend.to_string()
        } else {
            format!("{legend}\n{status}")
        };
        truncate_bytes(&text, MAX_HINT_BYTES)
    }
}

#[cfg(all(test, unix))]
mod child_process_tests {
    use super::*;

    /// Regression guard for the intentional-stop path: a child that writes one
    /// final line and exits must have that line delivered before `Exited`, or
    /// the supervisor reports a clean local stop as `child_exited`.
    #[tokio::test]
    async fn final_stdout_line_is_delivered_before_the_exit_notification() {
        let mut command = tokio::process::Command::new("/bin/echo");
        command.arg("{\"type\":\"stopped\"}");
        let mut child = SupervisedChild::spawn_process(command)
            .await
            .expect("echo child starts");
        let first = child.messages.recv().await.expect("first message");
        match first {
            ChildMessage::Line(line) => assert!(line.contains("stopped"), "line was {line:?}"),
            other => panic!("expected the child line first, got {other:?}"),
        }
        let second = child.messages.recv().await.expect("second message");
        assert!(matches!(second, ChildMessage::Exited(_)));
    }
}
