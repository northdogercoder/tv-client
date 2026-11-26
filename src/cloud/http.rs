//! HTTPS boundary for enrollment, device configuration and pairing.
//!
//! Every request is bounded by a ten-second timeout and a 64 KiB response
//! limit. Bearer credentials are attached only to the `Authorization` header;
//! they never appear in a URL, query string, debug output or error message.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::cloud::error::CloudError;
use crate::cloud::protocol::{PROTOCOL_VERSION, is_eight_digit_code, parse_rfc3339};
use crate::cloud::token::{DeviceToken, EnrollmentToken};
use crate::storage::{Storage, StorageError};

/// Largest accepted HTTP response body.
pub const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;
/// HTTP request timeout.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Credential file name for the device bearer.
pub const DEVICE_TOKEN_CREDENTIAL: &str = "device-token";
/// Preference key holding the control-server device id.
pub const DEVICE_ID_PREFERENCE: &str = "cloud.device_id";

/// One JSON request/response exchange.
///
/// The production implementation uses `reqwest` with rustls. Tests can supply
/// a recording implementation; neither form may put a token in the URL.
pub trait HttpTransport: Send + Sync {
    /// Perform one request and return the raw response body.
    ///
    /// `bearer` is the complete `Authorization` header value, including the
    /// `Bearer ` scheme. Transports must not derive or reformat a URL token.
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        bearer: Option<&'a str>,
        body: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<Vec<u8>, CloudError>>;
}

/// `reqwest`-backed transport.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build a redirect-free client with a bounded timeout.
    pub fn new() -> Result<Self, CloudError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|_| CloudError::Transport)?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        bearer: Option<&'a str>,
        body: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<Vec<u8>, CloudError>> {
        Box::pin(async move {
            let mut builder = match method {
                "GET" => self.client.get(url),
                "POST" => self.client.post(url),
                _ => return Err(CloudError::Transport),
            };
            if let Some(value) = bearer {
                builder = builder.header(AUTHORIZATION, value);
            }
            if let Some(body) = body {
                builder = builder
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.to_vec());
            }
            let response = builder.send().await.map_err(|_| CloudError::Transport)?;
            let status = response.status();
            if !status.is_success() {
                return Err(match status.as_u16() {
                    401 | 403 => CloudError::Unauthorized,
                    404 => CloudError::NotFound,
                    409 => CloudError::Conflict,
                    429 => CloudError::RateLimited,
                    503 => CloudError::Unavailable,
                    code => CloudError::Status { status: code },
                });
            }
            let bytes = response.bytes().await.map_err(|_| CloudError::Transport)?;
            if bytes.len() > MAX_HTTP_BODY_BYTES {
                return Err(CloudError::ResponseTooLarge);
            }
            Ok(bytes.to_vec())
        })
    }
}

/// Control-server HTTPS client.
pub struct CloudHttp {
    origin: Url,
    transport: Arc<dyn HttpTransport>,
    token: Option<DeviceToken>,
}

impl fmt::Debug for CloudHttp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudHttp")
            .field("origin", &self.origin.as_str())
            .field(
                "token",
                &if self.token.is_some() {
                    "redacted"
                } else {
                    "none"
                },
            )
            .finish()
    }
}

impl CloudHttp {
    /// Build a production client from a configured origin.
    pub fn new(server_url: &str) -> Result<Self, CloudError> {
        Ok(Self {
            origin: parse_origin(server_url)?,
            transport: Arc::new(ReqwestTransport::new()?),
            token: None,
        })
    }

    /// Build a client with a caller-supplied transport.
    ///
    /// The origin is still validated as HTTPS so tests cannot accidentally
    /// normalize a plaintext control plane into production code.
    pub fn with_transport(
        server_url: &str,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, CloudError> {
        Ok(Self {
            origin: parse_origin(server_url)?,
            transport,
            token: None,
        })
    }

    /// Attach the enrolled device bearer.
    #[must_use]
    pub fn with_token(mut self, token: DeviceToken) -> Self {
        self.token = Some(token);
        self
    }

    /// Whether a device token is attached.
    #[must_use]
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Borrow the device token for a WebSocket handshake header.
    pub fn token(&self) -> Result<&DeviceToken, CloudError> {
        self.token.as_ref().ok_or(CloudError::MissingToken)
    }

    /// WebSocket URL for the authenticated device socket.
    pub fn websocket_url(&self) -> Result<String, CloudError> {
        let mut url = self.origin.clone();
        url.set_scheme("wss").map_err(|()| CloudError::Endpoint)?;
        url.set_path("/v1/ws/device");
        url.set_query(None);
        url.set_fragment(None);
        Ok(url.to_string())
    }

    /// Exchange a one-use enrollment token for a device bearer.
    pub async fn enroll(
        &self,
        token: &EnrollmentToken,
        label: &str,
    ) -> Result<Enrollment, CloudError> {
        if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
            return Err(CloudError::InvalidToken);
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "token": token.expose(),
            "label": label,
        }))
        .map_err(|_| CloudError::InvalidResponse)?;
        let response = self
            .request("POST", "/v1/enroll", None, Some(body.as_slice()))
            .await?;
        let raw: EnrollmentResponse =
            serde_json::from_slice(&response).map_err(|_| CloudError::InvalidResponse)?;
        if raw.protocol != PROTOCOL_VERSION {
            return Err(CloudError::UnsupportedProtocol {
                found: raw.protocol,
            });
        }
        let token = DeviceToken::new(raw.device_token).map_err(|_| CloudError::InvalidToken)?;
        Ok(Enrollment {
            device_id: raw.device_id,
            token,
            protocol: raw.protocol,
        })
    }

    /// Fetch features and the server heartbeat preference.
    pub async fn device_config(&self) -> Result<DeviceConfig, CloudError> {
        let bearer = self.bearer()?;
        let response = self
            .request("GET", "/v1/device/config", Some(bearer), None)
            .await?;
        let raw: DeviceConfigResponse =
            serde_json::from_slice(&response).map_err(|_| CloudError::InvalidResponse)?;
        if raw.protocol != PROTOCOL_VERSION {
            return Err(CloudError::UnsupportedProtocol {
                found: raw.protocol,
            });
        }
        if !(1_000..=300_000).contains(&raw.heartbeat_ms) {
            return Err(CloudError::InvalidResponse);
        }
        Ok(DeviceConfig {
            protocol: raw.protocol,
            features: raw.features,
            heartbeat_ms: raw.heartbeat_ms,
        })
    }

    /// Create one active pairing code.
    pub async fn pair_init(&self) -> Result<PairInit, CloudError> {
        let body: &[u8] = b"{}";
        let bearer = self.bearer()?;
        let response = self
            .request("POST", "/v1/pair/init", Some(bearer), Some(body))
            .await?;
        let raw: PairInitResponse =
            serde_json::from_slice(&response).map_err(|_| CloudError::InvalidResponse)?;
        if !is_eight_digit_code(&raw.code) {
            return Err(CloudError::InvalidResponse);
        }
        if parse_rfc3339(&raw.expires_at).is_none() {
            return Err(CloudError::InvalidResponse);
        }
        Ok(PairInit {
            pair_id: raw.pair_id,
            code: raw.code,
            expires_at: raw.expires_at,
        })
    }

    /// Submit a locally decided pairing acceptance or denial.
    pub async fn pair_decision(&self, request_id: Uuid, accept: bool) -> Result<(), CloudError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "request_id": request_id,
            "accept": accept,
        }))
        .map_err(|_| CloudError::InvalidResponse)?;
        let bearer = self.bearer()?;
        let _ = self
            .request(
                "POST",
                "/v1/pair/decision",
                Some(bearer),
                Some(body.as_slice()),
            )
            .await?;
        Ok(())
    }

    fn bearer(&self) -> Result<&str, CloudError> {
        self.token
            .as_ref()
            .map(DeviceToken::expose)
            .ok_or(CloudError::MissingToken)
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, CloudError> {
        let mut url = self.origin.clone();
        url.set_path(path);
        url.set_query(None);
        url.set_fragment(None);
        let authorization = bearer.map(|token| format!("Bearer {token}"));
        self.transport
            .request(method, url.as_str(), authorization.as_deref(), body)
            .await
    }
}

fn parse_origin(value: &str) -> Result<Url, CloudError> {
    let url = Url::parse(value).map_err(|_| CloudError::Endpoint)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CloudError::Endpoint);
    }
    Ok(url)
}

/// Production WebSocket URL for a configured HTTPS origin.
pub fn device_websocket_url(server_url: &str) -> Result<String, CloudError> {
    let mut url = parse_origin(server_url)?;
    url.set_scheme("wss").map_err(|()| CloudError::Endpoint)?;
    url.set_path("/v1/ws/device");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

/// Successful enrollment result. The token is redacted in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Enrollment {
    /// Server-assigned device id.
    pub device_id: Uuid,
    /// Long-lived device bearer.
    pub token: DeviceToken,
    /// Confirmed protocol version.
    pub protocol: u32,
}

impl fmt::Debug for Enrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Enrollment")
            .field("device_id", &self.device_id)
            .field("token", &"redacted")
            .field("protocol", &self.protocol)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentResponse {
    device_id: Uuid,
    device_token: String,
    protocol: u32,
}

/// Server feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudFeatures {
    pub youtube: bool,
    pub airplay: bool,
    pub webrtc: bool,
}

/// Device configuration response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceConfig {
    /// Confirmed protocol version.
    pub protocol: u32,
    /// Server-advertised optional features.
    pub features: CloudFeatures,
    /// Heartbeat interval preference in milliseconds.
    pub heartbeat_ms: u64,
}

impl DeviceConfig {
    /// Heartbeat interval as a duration.
    #[must_use]
    pub fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_ms)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceConfigResponse {
    protocol: u32,
    features: CloudFeatures,
    heartbeat_ms: u64,
}

/// A pairing code to display on the TV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInit {
    /// Pairing session id.
    pub pair_id: Uuid,
    /// Eight decimal digits, leading zeros preserved.
    pub code: String,
    /// RFC3339 expiry.
    pub expires_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairInitResponse {
    pair_id: Uuid,
    code: String,
    expires_at: String,
}

/// Locally persisted device identity.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredDevice {
    /// Server-assigned device id.
    pub device_id: Uuid,
    /// Device bearer read from the 0600 credential file.
    pub token: DeviceToken,
}

impl fmt::Debug for StoredDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredDevice")
            .field("device_id", &self.device_id)
            .field("token", &"redacted")
            .finish()
    }
}

/// Store the device token with 0600 permissions and the id as a preference.
pub fn store_device(storage: &mut Storage, enrollment: &Enrollment) -> Result<(), CloudError> {
    storage.write_credential(
        DEVICE_TOKEN_CREDENTIAL,
        enrollment.token.expose().as_bytes(),
    )?;
    storage.set_preference(DEVICE_ID_PREFERENCE, &enrollment.device_id.to_string())?;
    Ok(())
}

/// Load a previously enrolled device, if any.
pub fn load_device(storage: &Storage) -> Result<Option<StoredDevice>, CloudError> {
    let secret = match storage.read_credential(DEVICE_TOKEN_CREDENTIAL) {
        Ok(secret) => secret,
        Err(StorageError::CredentialNotFound) => return Ok(None),
        Err(error) => return Err(CloudError::Storage(error)),
    };
    let secret = String::from_utf8(secret).map_err(|_| CloudError::InvalidToken)?;
    let token = DeviceToken::new(secret).map_err(|_| CloudError::InvalidToken)?;
    let device_id = storage
        .get_preference(DEVICE_ID_PREFERENCE)?
        .ok_or(CloudError::InvalidResponse)?;
    let device_id: Uuid = device_id.parse().map_err(|_| CloudError::InvalidResponse)?;
    Ok(Some(StoredDevice { device_id, token }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_be_https_without_credentials_or_query() {
        assert!(parse_origin("https://staging.example").is_ok());
        assert!(parse_origin("http://staging.example").is_err());
        assert!(parse_origin("https://user:pass@staging.example").is_err());
        assert!(parse_origin("https://staging.example/?token=1").is_err());
    }

    #[test]
    fn enrollment_debug_is_redacted() {
        let enrollment = Enrollment {
            device_id: Uuid::new_v4(),
            token: DeviceToken::new("device-token-value-1234567890").unwrap(),
            protocol: 1,
        };
        let rendered = format!("{enrollment:?}");
        assert!(!rendered.contains("device-token-value"));
        assert!(rendered.contains("redacted"));
    }
}
