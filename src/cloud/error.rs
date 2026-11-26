//! Typed cloud failures that never include tokens, URLs or frame contents.

use thiserror::Error;

use crate::cloud::pairing::PairError;
use crate::cloud::protocol::ProtocolError;
use crate::storage::StorageError;

/// A cloud operation failed.
///
/// Messages are deliberately static or name only a sanitized HTTP status; no
/// variant retains a request URL, a bearer token or a response body.
#[derive(Debug, Error)]
pub enum CloudError {
    /// The configured control-server origin is missing or not HTTPS.
    #[error("cloud endpoint must be an HTTPS origin")]
    Endpoint,
    /// The network request failed before a response was available.
    #[error("cloud request failed")]
    Transport,
    /// The control server rejected the device credentials.
    #[error("cloud device authentication failed")]
    Unauthorized,
    /// The requested resource does not exist.
    #[error("cloud resource not found")]
    NotFound,
    /// The operation conflicts with current server state.
    #[error("cloud operation conflicts with server state")]
    Conflict,
    /// The server asked the client to slow down.
    #[error("cloud request was rate limited")]
    RateLimited,
    /// The server is temporarily unavailable.
    #[error("cloud server is unavailable")]
    Unavailable,
    /// An unclassified HTTP status.
    #[error("cloud server returned HTTP {status}")]
    Status {
        /// Numeric status code.
        status: u16,
    },
    /// The response body exceeded the protocol bound.
    #[error("cloud response is too large")]
    ResponseTooLarge,
    /// The response did not match the documented shape.
    #[error("cloud response is invalid")]
    InvalidResponse,
    /// The server speaks a different major protocol version.
    #[error("cloud server protocol version {found} is not supported")]
    UnsupportedProtocol {
        /// Version reported by the server.
        found: u32,
    },
    /// Local storage failed.
    #[error("cloud storage failed: {0}")]
    Storage(#[from] StorageError),
    /// A wire message was malformed.
    #[error("cloud protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// Pairing state or provenance rejected the operation.
    #[error("pairing rejected: {0}")]
    Pairing(#[from] PairError),
    /// No device token has been enrolled.
    #[error("cloud device is not enrolled")]
    MissingToken,
    /// A token was empty, oversized or contained separators.
    #[error("invalid cloud token")]
    InvalidToken,
    /// Cloud use was requested while disabled in configuration.
    #[error("cloud features are disabled")]
    Disabled,
    /// The server revoked this device.
    #[error("cloud device was revoked")]
    Revoked,
}
