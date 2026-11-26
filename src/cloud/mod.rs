//! Optional OVH control plane.
//!
//! Local Jellyfin playback never depends on this module. Enrollment, pairing
//! and remote commands are additive: when the cloud is disabled, unreachable
//! or revoked the rest of the client keeps working offline.

pub mod action;
pub mod error;
pub mod http;
pub mod link;
pub mod pairing;
pub mod protocol;
pub mod token;

pub use action::{
    ApplyOutcome, ApplyRequest, COMMAND_APPLY_TIMEOUT, COMMAND_QUEUE_CAPACITY, CommandApplier,
    CommandPolicy, CommandQueue, DEDUPE_CAPACITY, DEDUPE_WINDOW, DedupeCache, QueueReject,
    RejectingApplier, Rejection,
};
pub use error::CloudError;
pub use http::{
    CloudFeatures, CloudHttp, DEVICE_ID_PREFERENCE, DEVICE_TOKEN_CREDENTIAL, DeviceConfig,
    Enrollment, HTTP_TIMEOUT, HttpTransport, MAX_HTTP_BODY_BYTES, PairInit, ReqwestTransport,
    StoredDevice, device_websocket_url, load_device, store_device,
};
pub use link::{
    CONNECT_TIMEOUT, DeviceLink, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT, LINK_EVENT_CAPACITY,
    LinkClose, LinkConfig, LinkEnd, LinkEvent, OUTBOUND_CAPACITY, PLAYBACK_STATE_INTERVAL,
    RECONNECT_MAX, RECONNECT_MIN, ReconnectPolicy,
};
pub use pairing::{
    PairDecision, PairError, PairingCode, PairingSession, PairingState, PendingPair,
};
pub use protocol::{
    AckPayload, AckStatus, CommandPayload, HelloAck, HelloPayload, InboundMessage, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, PairRequestPayload, PairStatus, PairStatusPayload, ProtocolError,
    StatePayload, is_code, is_eight_digit_code, is_four_digit_code, parse_action, parse_inbound,
    parse_rfc3339,
};
pub use token::{DeviceToken, EnrollmentToken, TokenError};

use crate::config::ClientConfig;

/// Capabilities advertised in the device hello.
///
/// YouTube is advertised only when a real playback path is wired; until C010
/// lands the client rejects `play_youtube` instead of claiming it can start.
#[must_use]
pub fn capabilities(jellyfin: bool, youtube_playback: bool) -> Vec<String> {
    let mut capabilities = Vec::new();
    if jellyfin {
        capabilities.push("jellyfin".to_owned());
    }
    if youtube_playback {
        capabilities.push("youtube".to_owned());
    }
    capabilities
}

/// Whether cloud features are enabled and a server origin is configured.
#[must_use]
pub fn enabled(config: &ClientConfig) -> bool {
    config.cloud_enabled && config.server_url.is_some()
}
