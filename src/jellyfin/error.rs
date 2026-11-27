//! Typed Jellyfin failures.
//!
//! Variants never carry request URLs, headers, credentials or response bodies.
//! In particular, transport errors are reported without their source message
//! because reqwest error text can include a URL whose query string contains a
//! Quick Connect secret.

use thiserror::Error;

use crate::config::ConfigError;
use crate::storage::StorageError;

/// Failures raised by [`crate::jellyfin::JellyfinClient`].
#[derive(Debug, Error)]
pub enum JellyfinError {
    /// `ClientConfig::jellyfin_url` is absent.
    #[error("jellyfin is not configured")]
    NotConfigured,
    /// The configured URL or another client setting is not usable.
    #[error("invalid jellyfin configuration")]
    InvalidConfig(#[source] ConfigError),
    /// A supplied URL could not be parsed or does not have a supported origin.
    #[error("invalid jellyfin URL")]
    InvalidUrl,
    /// A caller supplied an empty, oversized or control-character value.
    #[error("invalid request parameter")]
    InvalidRequest,
    /// An item identifier failed the shared item-id validation.
    #[error("invalid item id")]
    InvalidItemId,
    /// The operation needs a session, but no token is installed.
    #[error("not authenticated to jellyfin")]
    NotAuthenticated,
    /// Username/password authentication was rejected.
    #[error("jellyfin rejected the supplied credentials")]
    AuthenticationRejected,
    /// Quick Connect is disabled or not reachable without a session.
    #[error("quick connect is not available")]
    QuickConnectUnavailable,
    /// A Quick Connect initiation exceeded its lifetime.
    #[error("quick connect request expired")]
    QuickConnectExpired,
    /// Quick Connect was polled before the user approved the code.
    #[error("quick connect has not been approved")]
    QuickConnectNotApproved,
    /// The server returned 401 for an authenticated request.
    #[error("jellyfin authentication is required")]
    Unauthorized,
    /// The server returned 403.
    #[error("jellyfin access is forbidden")]
    Forbidden,
    /// The server returned 429.
    #[error("jellyfin rate limited the request")]
    RateLimited {
        /// Parsed `Retry-After` seconds, when the server supplied one.
        retry_after_secs: Option<u64>,
    },
    /// The request exceeded its deadline.
    #[error("jellyfin request timed out")]
    Timeout,
    /// The connection failed before a response was received.
    #[error("jellyfin network request failed")]
    Network,
    /// The caller cancelled the request.
    #[error("jellyfin request was cancelled")]
    Cancelled,
    /// Any other non-success status.
    #[error("jellyfin returned HTTP {status}")]
    Http {
        /// Numeric status code.
        status: u16,
    },
    /// A redirect was returned. Redirects are never followed.
    #[error("jellyfin returned an unexpected redirect")]
    UnexpectedRedirect,
    /// The response was not valid UTF-8 JSON with the expected shape.
    #[error("invalid jellyfin response")]
    InvalidResponse,
    /// A response body exceeded the client's bounded read limit.
    #[error("jellyfin response body is too large")]
    ResponseTooLarge,
    /// An image's encoded size or declared length exceeded the cap.
    #[error("artwork payload is too large")]
    ArtworkTooLarge,
    /// Image dimensions exceeded the configured pixel or side limit.
    #[error("artwork dimensions {width}x{height} exceed the configured limit")]
    ImageTooLarge {
        /// Decoded width from the image header.
        width: u32,
        /// Decoded height from the image header.
        height: u32,
    },
    /// The image magic or header was not a supported format.
    #[error("artwork header could not be decoded")]
    InvalidImage,
    /// The item has no artwork of the requested kind (HTTP 404 or no tag).
    ///
    /// This is a normal browse outcome, not a page failure.
    #[error("artwork is not available for this item")]
    ArtworkMissing,
    /// Artwork storage failed.
    #[error("artwork cache failure")]
    Artwork(#[source] StorageError),
    /// Local persistence failed.
    #[error("storage failure")]
    Storage(#[source] StorageError),
}
