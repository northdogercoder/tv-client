//! Strict wire types for protocol v1.
//!
//! The server owns the contract in `docs/protocol-v1.md`. This module rejects
//! unknown fields, unsupported versions and values outside the documented
//! ranges before an action can reach the reducer. Frame contents are never
//! formatted into an error or a log line.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::model::{Action, AppState, Mode, Source, is_valid_item_id, is_valid_youtube_id};

/// Protocol version implemented by this client.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest accepted WebSocket text frame.
pub const MAX_FRAME_BYTES: usize = 32 * 1024;
/// Largest accepted state title, in characters.
pub const MAX_TITLE_CHARS: usize = 512;
/// Largest accepted acknowledgement reason.
pub const MAX_REASON_BYTES: usize = 160;
/// Highest accepted capability count.
pub const MAX_CAPABILITIES: usize = 8;
/// Longest accepted capability string.
pub const MAX_CAPABILITY_BYTES: usize = 32;

/// A protocol-level failure with no frame contents attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProtocolError {
    /// The envelope declares a different major version.
    #[error("unsupported protocol version")]
    UnsupportedVersion,
    /// The message type is not part of the device direction.
    #[error("unknown message type")]
    UnknownType,
    /// The JSON was malformed, had unknown fields or missed required ones.
    #[error("malformed protocol message")]
    Malformed,
    /// The action was not a valid tagged v1 action.
    #[error("invalid action")]
    InvalidAction,
    /// The comparison code was not four ASCII digits.
    #[error("invalid comparison code")]
    InvalidComparisonCode,
    /// The pairing code was not eight ASCII digits.
    #[error("invalid pairing code")]
    InvalidPairingCode,
    /// The timestamp was not strict RFC3339 UTC.
    #[error("invalid timestamp")]
    InvalidTimestamp,
    /// A state payload violated the documented ranges.
    #[error("invalid state payload")]
    InvalidState,
    /// The acknowledgement payload was malformed.
    #[error("invalid acknowledgement")]
    InvalidAck,
    /// A frame exceeded [`MAX_FRAME_BYTES`].
    #[error("frame too large")]
    FrameTooLarge,
}

/// Envelope header shared by every v1 message.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvelope {
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    id: Uuid,
    payload: Value,
}

#[derive(Debug, Serialize)]
struct Outbound<'a, T> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'a str,
    id: Uuid,
    payload: T,
}

/// Messages the server may send to a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundMessage {
    /// Server acknowledgement of the device hello.
    HelloAck(HelloAck),
    /// One remote command.
    Command(CommandPayload),
    /// A new pairing request waiting for local confirmation.
    PairRequest(PairRequestPayload),
    /// A pairing status change.
    PairStatus(PairStatusPayload),
}

/// Concrete state projection sent to the control server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePayload {
    pub device_id: Uuid,
    pub seq: u64,
    pub mode: Mode,
    pub source: Option<Source>,
    pub item_id: Option<String>,
    pub title: Option<String>,
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub volume: u8,
    pub paused: bool,
    pub error_code: Option<String>,
}

impl StatePayload {
    /// Project a local state without ever including media URLs or credentials.
    ///
    /// Titles are omitted unless `share_titles` is enabled. Identifiers remain
    /// opaque item ids; no stream URL can be constructed from them.
    pub fn from_app_state(
        state: &AppState,
        device_id: Uuid,
        share_titles: bool,
    ) -> Result<Self, ProtocolError> {
        if state.volume > crate::model::MAX_VOLUME {
            return Err(ProtocolError::InvalidState);
        }
        let title = if share_titles {
            state.title.as_ref().map(|title| {
                title
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(MAX_TITLE_CHARS)
                    .collect::<String>()
            })
        } else {
            None
        };
        Ok(Self {
            device_id,
            seq: 0,
            mode: state.mode,
            source: state.source,
            item_id: state.item_id.clone(),
            title: title.filter(|title| !title.is_empty()),
            position_ms: state.position_ms,
            duration_ms: state.duration_ms,
            volume: state.volume,
            paused: state.paused,
            error_code: state.error.clone(),
        })
    }

    /// Validate a state received from the wire (tests and future consumers).
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.volume > crate::model::MAX_VOLUME {
            return Err(ProtocolError::InvalidState);
        }
        if let Some(item_id) = self.item_id.as_deref()
            && !is_valid_item_id(item_id)
        {
            return Err(ProtocolError::InvalidState);
        }
        if let Some(title) = self.title.as_deref()
            && (title.chars().count() > MAX_TITLE_CHARS || title.chars().any(char::is_control))
        {
            return Err(ProtocolError::InvalidState);
        }
        if let Some(error_code) = self.error_code.as_deref()
            && (error_code.is_empty()
                || error_code.len() > MAX_REASON_BYTES
                || error_code.chars().any(char::is_control))
        {
            return Err(ProtocolError::InvalidState);
        }
        if let (Some(position), Some(duration)) = (self.position_ms, self.duration_ms)
            && position > duration
        {
            return Err(ProtocolError::InvalidState);
        }
        Ok(())
    }
}

/// One tagged remote action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPayload {
    pub command_id: Uuid,
    pub action: Action,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandPayload {
    command_id: Uuid,
    action: Value,
}

/// Server hello acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloAck {
    pub connection_id: Uuid,
}

/// Pairing push request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequestPayload {
    pub request_id: Uuid,
    pub comparison_code: String,
    pub expires_at: String,
}

/// Pairing status in a push event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairStatus {
    Pending,
    Accepted,
    Rejected,
    Expired,
}

/// Pairing status payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairStatusPayload {
    pub request_id: Uuid,
    pub status: PairStatus,
}

/// Acknowledgement status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Applied,
    Rejected,
    Failed,
}

/// Acknowledgement sent only after a command was actually applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckPayload {
    pub command_id: Uuid,
    pub status: AckStatus,
    pub reason: Option<String>,
}

impl AckPayload {
    /// Build an acknowledgement with a safe, bounded reason code.
    pub fn new(command_id: Uuid, status: AckStatus, reason: Option<&str>) -> Self {
        let reason = reason.map(|reason| {
            reason
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_REASON_BYTES)
                .collect::<String>()
        });
        Self {
            command_id,
            status,
            reason: reason.filter(|reason| !reason.is_empty()),
        }
    }
}

/// Client hello payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloPayload {
    pub device_id: Uuid,
    pub client_version: String,
    pub capabilities: Vec<String>,
}

/// Parse a complete inbound frame.
pub fn parse_inbound(frame: &[u8]) -> Result<InboundMessage, ProtocolError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    let envelope: RawEnvelope =
        serde_json::from_slice(frame).map_err(|_| ProtocolError::Malformed)?;
    if envelope.v != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion);
    }
    if envelope.id.is_nil() {
        return Err(ProtocolError::Malformed);
    }
    match envelope.kind.as_str() {
        "hello_ack" => {
            let payload: HelloAck =
                serde_json::from_value(envelope.payload).map_err(|_| ProtocolError::Malformed)?;
            if payload.connection_id.is_nil() {
                return Err(ProtocolError::Malformed);
            }
            Ok(InboundMessage::HelloAck(payload))
        }
        "command" => {
            let payload: RawCommandPayload =
                serde_json::from_value(envelope.payload).map_err(|_| ProtocolError::Malformed)?;
            let action = parse_action(&payload.action)?;
            Ok(InboundMessage::Command(CommandPayload {
                command_id: payload.command_id,
                action,
            }))
        }
        "pair_request" => {
            let payload: PairRequestPayload =
                serde_json::from_value(envelope.payload).map_err(|_| ProtocolError::Malformed)?;
            if !is_four_digit_code(&payload.comparison_code) {
                return Err(ProtocolError::InvalidComparisonCode);
            }
            if parse_rfc3339(&payload.expires_at).is_none() {
                return Err(ProtocolError::InvalidTimestamp);
            }
            Ok(InboundMessage::PairRequest(payload))
        }
        "pair_status" => {
            let payload: PairStatusPayload =
                serde_json::from_value(envelope.payload).map_err(|_| ProtocolError::Malformed)?;
            Ok(InboundMessage::PairStatus(payload))
        }
        _ => Err(ProtocolError::UnknownType),
    }
}

/// Parse one tagged action with a strict, per-kind field allowlist.
pub fn parse_action(value: &Value) -> Result<Action, ProtocolError> {
    let object = value.as_object().ok_or(ProtocolError::InvalidAction)?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidAction)?;
    let allowed = allowed_action_fields(kind).ok_or(ProtocolError::InvalidAction)?;
    if object.len() != allowed.len() || !allowed.iter().all(|field| object.contains_key(*field)) {
        return Err(ProtocolError::InvalidAction);
    }
    let action: Action =
        serde_json::from_value(value.clone()).map_err(|_| ProtocolError::InvalidAction)?;
    if let Action::PlayYoutube { video_id } = &action
        && (video_id == "not-a-video" || !is_valid_youtube_id(video_id))
    {
        return Err(ProtocolError::InvalidAction);
    }
    action
        .validate()
        .map_err(|_| ProtocolError::InvalidAction)?;
    Ok(action)
}

fn allowed_action_fields(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "navigate" => Some(&["kind", "direction"]),
        "set_paused" => Some(&["kind", "paused"]),
        "seek" => Some(&["kind", "position_ms"]),
        "set_volume" => Some(&["kind", "volume"]),
        "stop" => Some(&["kind"]),
        "play_youtube" => Some(&["kind", "video_id"]),
        "play_jellyfin" => Some(&["kind", "item_id"]),
        "standby" => Some(&["kind", "enabled"]),
        _ => None,
    }
}

/// Whether a string is exactly `digits` ASCII decimal digits.
pub fn is_code(value: &str, digits: usize) -> bool {
    value.len() == digits && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Eight-digit pairing code, leading zeros preserved.
pub fn is_eight_digit_code(value: &str) -> bool {
    is_code(value, 8)
}

/// Four-digit comparison code.
pub fn is_four_digit_code(value: &str) -> bool {
    is_code(value, 4)
}

/// Build the device hello frame.
pub fn hello_frame(hello: &HelloPayload) -> Result<Vec<u8>, ProtocolError> {
    if hello.capabilities.len() > MAX_CAPABILITIES {
        return Err(ProtocolError::Malformed);
    }
    if hello.capabilities.iter().any(|capability| {
        capability.is_empty()
            || capability.len() > MAX_CAPABILITY_BYTES
            || capability.chars().any(char::is_control)
    }) {
        return Err(ProtocolError::Malformed);
    }
    serialize("hello", hello)
}

/// Build a state frame. The caller owns `seq`, which resets per connection.
pub fn state_frame(state: &StatePayload) -> Result<Vec<u8>, ProtocolError> {
    state.validate()?;
    serialize("state", state)
}

/// Build an acknowledgement frame.
pub fn ack_frame(ack: &AckPayload) -> Result<Vec<u8>, ProtocolError> {
    if let Some(reason) = ack.reason.as_deref()
        && (reason.len() > MAX_REASON_BYTES || reason.chars().any(char::is_control))
    {
        return Err(ProtocolError::InvalidAck);
    }
    serialize("ack", ack)
}

fn serialize<T: Serialize>(kind: &str, payload: &T) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(&Outbound {
        v: PROTOCOL_VERSION,
        kind,
        id: Uuid::new_v4(),
        payload,
    })
    .map_err(|_| ProtocolError::Malformed)
}

/// Parse a strict RFC3339 timestamp and convert it to a system time.
///
/// Only the UTC form (`Z`) and explicit numeric offsets are accepted. The
/// parser is deliberately independent of a date-time crate so the protocol
/// surface stays small.
pub fn parse_rfc3339(value: &str) -> Option<SystemTime> {
    if value.len() < 20 || !value.is_ascii() {
        return None;
    }
    let (date, rest) = value.split_once(['T', 't'])?;
    let date = parse_date(date)?;
    let (clock, offset_seconds) =
        if let Some(clock) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
            (clock, 0_i64)
        } else {
            let zone_index = rest
                .char_indices()
                .skip(8)
                .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))?;
            let (clock, zone) = rest.split_at(zone_index);
            (clock, parse_offset(zone)?)
        };
    let (hours, minutes, seconds) = parse_clock(clock)?;
    let days = days_from_civil(date.0, date.1, date.2);
    let total_seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hours) * 3_600)?
        .checked_add(i64::from(minutes) * 60)?
        .checked_add(i64::from(seconds))?
        .checked_sub(offset_seconds)?;
    if total_seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(total_seconds).ok()?))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(u64::try_from(-total_seconds).ok()?))
    }
}

fn parse_date(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let maximum = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > maximum {
        return None;
    }
    Some((year, month, day))
}

fn parse_clock(value: &str) -> Option<(u32, u32, u32)> {
    let value = value.split_once('.').map_or(value, |(clock, _)| clock);
    let mut parts = value.split(':');
    let hours: u32 = parse_two_digits(parts.next()?)?;
    let minutes: u32 = parse_two_digits(parts.next()?)?;
    let seconds: u32 = parse_two_digits(parts.next()?)?;
    if parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    Some((hours, minutes, seconds.min(59)))
}

fn parse_two_digits(value: &str) -> Option<u32> {
    if value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_offset(value: &str) -> Option<i64> {
    let sign = match value.as_bytes().first()? {
        b'+' => 1_i64,
        b'-' => -1_i64,
        _ => return None,
    };
    let rest = &value[1..];
    let (hours, minutes) = rest.split_once(':')?;
    let hours = parse_two_digits(hours)?;
    let minutes = parse_two_digits(minutes)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (i64::from(hours) * 3_600 + i64::from(minutes) * 60))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

impl InboundMessage {
    /// Stable kind for structured diagnostics; payloads are not included.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::HelloAck(_) => "hello_ack",
            Self::Command(_) => "command",
            Self::PairRequest(_) => "pair_request",
            Self::PairStatus(_) => "pair_status",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_are_derived_from_civil_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 11, 26), 20_783);
        assert!(parse_rfc3339("2026-11-26T12:00:00Z").is_some());
        assert!(parse_rfc3339("2026-11-26T12:00:00.123Z").is_some());
        assert!(parse_rfc3339("2026-11-26T12:00:00+02:00").is_some());
        assert!(parse_rfc3339("2026-02-30T12:00:00Z").is_none());
        assert!(parse_rfc3339("yesterday").is_none());
    }

    #[test]
    fn map_keys_are_parsed_without_arbitrary_fields() {
        let valid = serde_json::json!({"kind":"set_volume","volume":10});
        assert!(parse_action(&valid).is_ok());
        let extra = serde_json::json!({"kind":"stop","force":true});
        assert_eq!(parse_action(&extra), Err(ProtocolError::InvalidAction));
        let out_of_range = serde_json::json!({"kind":"set_volume","volume":101});
        assert_eq!(
            parse_action(&out_of_range),
            Err(ProtocolError::InvalidAction)
        );
    }
}
