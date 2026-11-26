//! Authenticated Jellyfin HTTP client core.
//!
//! The client is pinned to one configured origin (optionally below a base
//! path), refuses redirects, uses rustls-backed TLS validation, and passes an
//! explicit cancellation token into every request and body read.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::time::Duration;

use reqwest::header;
use reqwest::{Client, Method, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;
use uuid::Uuid;

use crate::config::ClientConfig;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::models::{AuthenticationResult, UserDto};
use crate::storage::{Storage, StorageError};

/// Client name reported in the Jellyfin authorization header.
pub const CLIENT_NAME: &str = "tv-client";
/// Default device label reported in the authorization header.
pub const DEFAULT_DEVICE_NAME: &str = "tv-client";
/// Preference key holding the stable random device identifier.
pub const DEVICE_ID_PREFERENCE: &str = "jellyfin_device_id";
/// Credential file name holding the current access token.
pub const ACCESS_TOKEN_CREDENTIAL: &str = "jellyfin_access_token";
/// Default Quick Connect approval lifetime.
pub const QUICK_CONNECT_TTL: Duration = Duration::from_secs(600);

pub(crate) const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEVICE_NAME_BYTES: usize = 128;
const MAX_TOKEN_BYTES: usize = 4096;
const MAX_CREDENTIAL_BYTES: usize = 4096;
const MAX_ARTWORK_KEYS: usize = 256;
const STORAGE_ARTWORK_KEY_BYTES: usize = 128;

/// Connect, per-request and total deadlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// TCP/TLS connect deadline.
    pub connect: Duration,
    /// Deadline for one request, including its body read.
    pub read: Duration,
    /// Overall deadline applied by the HTTP client.
    pub total: Duration,
}

impl Timeouts {
    /// Build a set of deadlines. Zero durations are rejected.
    pub fn new(connect: Duration, read: Duration, total: Duration) -> Result<Self, JellyfinError> {
        if connect.is_zero() || read.is_zero() || total.is_zero() {
            return Err(JellyfinError::InvalidRequest);
        }
        Ok(Self {
            connect,
            read,
            total,
        })
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            read: Duration::from_secs(10),
            total: Duration::from_secs(30),
        }
    }
}

/// Active authenticated session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    user_id: String,
    user_name: Option<String>,
    server_id: Option<String>,
}

impl Session {
    /// Opaque user identifier.
    #[must_use]
    pub fn user_id(&self) -> &str {
        self.user_id.as_str()
    }

    /// Display name returned by the server.
    #[must_use]
    pub fn user_name(&self) -> Option<&str> {
        self.user_name.as_deref()
    }

    /// Server identifier returned during authentication.
    #[must_use]
    pub fn server_id(&self) -> Option<&str> {
        self.server_id.as_deref()
    }
}

/// Typed Jellyfin client for one configured server.
pub struct JellyfinClient {
    http: Client,
    base: Url,
    device_name: String,
    device_id: String,
    token: Option<String>,
    session: Option<Session>,
    timeouts: Timeouts,
    pub(crate) quick_connect_lifetime: Duration,
    artwork_keys: VecDeque<String>,
}

impl fmt::Debug for JellyfinClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JellyfinClient")
            .field("base", &self.base.as_str())
            .field("device_name", &self.device_name)
            .field("authenticated", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl JellyfinClient {
    /// Build a client from validated configuration.
    ///
    /// `storage` supplies the stable device id and any previously persisted
    /// access token. The token is not verified here; call
    /// [`Self::restore_session`] to resolve the user.
    pub fn new(
        config: &ClientConfig,
        device_name: &str,
        storage: &mut Storage,
    ) -> Result<Self, JellyfinError> {
        Self::with_timeouts(config, device_name, storage, Timeouts::default())
    }

    /// Build a client with explicit connect/read/total deadlines.
    pub fn with_timeouts(
        config: &ClientConfig,
        device_name: &str,
        storage: &mut Storage,
        timeouts: Timeouts,
    ) -> Result<Self, JellyfinError> {
        config.validate().map_err(JellyfinError::InvalidConfig)?;
        let raw = config
            .jellyfin_url
            .as_deref()
            .ok_or(JellyfinError::NotConfigured)?;
        let base = normalize_base(raw)?;
        validate_device_name(device_name)?;
        let timeouts = Timeouts::new(timeouts.connect, timeouts.read, timeouts.total)?;

        let device_id = stable_device_id(storage)?;
        let token = load_token(storage)?;
        let http = Client::builder()
            .user_agent(format!("{}/{}", CLIENT_NAME, env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.read)
            .timeout(timeouts.total)
            .build()
            .map_err(|_| JellyfinError::InvalidUrl)?;

        Ok(Self {
            http,
            base,
            device_name: device_name.to_owned(),
            device_id,
            token,
            session: None,
            timeouts,
            quick_connect_lifetime: QUICK_CONNECT_TTL,
            artwork_keys: VecDeque::new(),
        })
    }

    /// Configured base origin including an optional path prefix.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// Stable device identifier persisted in preferences.
    #[must_use]
    pub fn device_id(&self) -> &str {
        self.device_id.as_str()
    }

    /// Whether an access token is installed (it may still be expired).
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.token.is_some()
    }

    /// Active session, when the user was authenticated in this process.
    #[must_use]
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Effective deadlines.
    #[must_use]
    pub const fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// Override the Quick Connect approval lifetime.
    pub fn set_quick_connect_ttl(&mut self, ttl: Duration) -> Result<(), JellyfinError> {
        if ttl.is_zero() {
            return Err(JellyfinError::InvalidRequest);
        }
        self.quick_connect_lifetime = ttl;
        Ok(())
    }

    /// Resolve a persisted token to a user with `GET /Users/Me`.
    ///
    /// Returns `false` and removes the stored token when the server reports it
    /// as unauthorized.
    pub async fn restore_session(
        &mut self,
        storage: &mut Storage,
        cancel: &CancelToken,
    ) -> Result<bool, JellyfinError> {
        if self.token.is_none() {
            return Ok(false);
        }
        let url = self.endpoint(&["Users", "Me"])?;
        match self.get_json::<UserDto>(url, cancel).await {
            Ok(user) => {
                if !crate::model::is_valid_item_id(&user.id) {
                    return Err(JellyfinError::InvalidResponse);
                }
                self.session = Some(Session {
                    user_id: user.id,
                    user_name: user.name,
                    server_id: user.server_id,
                });
                Ok(true)
            }
            Err(JellyfinError::Unauthorized) => {
                self.token = None;
                self.session = None;
                clear_stored_token(storage)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Revoke the server session, erase the stored token and clear artwork.
    ///
    /// Local credentials and tracked artwork are removed even when revocation
    /// fails. The revocation error is returned after cleanup.
    pub async fn logout(
        &mut self,
        storage: &mut Storage,
        cancel: &CancelToken,
    ) -> Result<(), JellyfinError> {
        let revoke = if self.token.is_some() {
            let url = self.endpoint(&["Sessions", "Logout"])?;
            match self.post_no_content(url, cancel).await {
                Ok(()) | Err(JellyfinError::Unauthorized | JellyfinError::Forbidden) => Ok(()),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        self.token = None;
        self.session = None;
        let cleared = clear_stored_token(storage);
        self.erase_artwork_cache(storage);
        revoke.and(cleared)
    }

    pub(crate) fn endpoint(&self, segments: &[&str]) -> Result<Url, JellyfinError> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| JellyfinError::InvalidUrl)?;
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    pub(crate) fn require_session(&self) -> Result<&Session, JellyfinError> {
        self.session.as_ref().ok_or(JellyfinError::NotAuthenticated)
    }

    pub(crate) fn user_id(&self) -> Result<&str, JellyfinError> {
        Ok(self.require_session()?.user_id.as_str())
    }

    pub(crate) fn authorization_header(&self) -> String {
        let mut value = format!(
            "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            escape_header(CLIENT_NAME),
            escape_header(&self.device_name),
            escape_header(&self.device_id),
            escape_header(env!("CARGO_PKG_VERSION")),
        );
        if let Some(token) = &self.token {
            value.push_str(", Token=\"");
            value.push_str(&escape_header(token));
            value.push('"');
        }
        value
    }

    /// Install an authentication result and persist its token.
    pub(crate) fn install_session(
        &mut self,
        storage: &mut Storage,
        result: AuthenticationResult,
    ) -> Result<Session, JellyfinError> {
        validate_token(&result.access_token)?;
        if !crate::model::is_valid_item_id(&result.user.id) {
            return Err(JellyfinError::InvalidResponse);
        }
        let replace = self
            .session
            .as_ref()
            .is_some_and(|session| session.user_id != result.user.id);
        if replace {
            self.erase_artwork_cache(storage);
        }
        storage
            .write_credential(ACCESS_TOKEN_CREDENTIAL, result.access_token.as_bytes())
            .map_err(JellyfinError::Storage)?;
        let session = Session {
            user_id: result.user.id,
            user_name: result.user.name,
            server_id: result.server_id,
        };
        self.token = Some(result.access_token);
        self.session = Some(session.clone());
        Ok(session)
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        url: Url,
        cancel: &CancelToken,
    ) -> Result<T, JellyfinError> {
        let response = self.send_raw(Method::GET, url, None, cancel).await?;
        let body = read_body_limited(response, MAX_JSON_BYTES, cancel).await?;
        serde_json::from_slice(&body).map_err(|_| JellyfinError::InvalidResponse)
    }

    pub(crate) async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        url: Url,
        body: &B,
        cancel: &CancelToken,
    ) -> Result<T, JellyfinError> {
        let encoded = serde_json::to_vec(body).map_err(|_| JellyfinError::InvalidRequest)?;
        let response = self
            .send_raw(Method::POST, url, Some(encoded), cancel)
            .await?;
        let body = read_body_limited(response, MAX_JSON_BYTES, cancel).await?;
        serde_json::from_slice(&body).map_err(|_| JellyfinError::InvalidResponse)
    }

    pub(crate) async fn post_no_content(
        &self,
        url: Url,
        cancel: &CancelToken,
    ) -> Result<(), JellyfinError> {
        let response = self.send_raw(Method::POST, url, None, cancel).await?;
        let _ = read_body_limited(response, 64 * 1024, cancel).await?;
        Ok(())
    }

    pub(crate) async fn send_raw(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        cancel: &CancelToken,
    ) -> Result<Response, JellyfinError> {
        let authorization = self.authorization_header();
        let has_body = body.is_some();
        let mut builder = self
            .http
            .request(method, url.clone())
            .header(header::AUTHORIZATION, authorization);
        if has_body {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(body.unwrap_or_default())
            .build()
            .map_err(|_| JellyfinError::InvalidRequest)?;

        let mut token = cancel.clone();
        let response = tokio::select! {
            biased;
            () = token.cancelled() => return Err(JellyfinError::Cancelled),
            response = self.http.execute(request) => response.map_err(map_transport)?,
        };

        if self.base.origin() != response.url().origin() {
            return Err(JellyfinError::UnexpectedRedirect);
        }
        let status = response.status();
        if status.is_redirection() {
            return Err(JellyfinError::UnexpectedRedirect);
        }
        if status.is_success() {
            return Ok(response);
        }
        Err(map_status(status, response.headers()))
    }

    pub(crate) fn artwork_cache_key(
        &self,
        item_id: &str,
        image_type: crate::jellyfin::ImageType,
        tag: Option<&str>,
    ) -> Result<String, JellyfinError> {
        let user = self.user_id()?;
        let tag = tag.unwrap_or("none");
        for part in [user, item_id, image_type.as_str(), tag] {
            if part.is_empty() || part.chars().any(char::is_control) {
                return Err(JellyfinError::InvalidRequest);
            }
        }
        let readable = format!(
            "jf1|{}|{}|{}|{}|{}",
            self.base.origin().ascii_serialization(),
            user,
            item_id,
            image_type.as_str(),
            tag
        );
        if readable.len() <= STORAGE_ARTWORK_KEY_BYTES {
            return Ok(readable);
        }
        let hashed = format!(
            "jf1|{:016x}|{:016x}|{:016x}",
            fnv1a(0xcbf29ce484222325, readable.as_bytes()),
            fnv1a(0x84222325cbf29ce4, readable.as_bytes()),
            readable.len()
        );
        Ok(hashed)
    }

    pub(crate) fn remember_artwork_key(&mut self, key: String) {
        if self.artwork_keys.contains(&key) {
            return;
        }
        self.artwork_keys.push_back(key);
        while self.artwork_keys.len() > MAX_ARTWORK_KEYS {
            self.artwork_keys.pop_front();
        }
    }

    pub(crate) fn erase_artwork_cache(&mut self, storage: &mut Storage) {
        for key in self.artwork_keys.drain(..) {
            if let Ok(Some(path)) = storage.artwork_path(&key) {
                let _ = fs::remove_file(path);
                let _ = storage.artwork_path(&key);
            }
        }
        // A previous process may have cached artwork without leaving keys in
        // this session. Remove payload files; Storage reconciles stale index
        // rows lazily when an entry is next resolved or the store is reopened.
        let directory = storage.state_directory().join("artwork");
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

fn normalize_base(raw: &str) -> Result<Url, JellyfinError> {
    let mut base = Url::parse(raw).map_err(|_| JellyfinError::InvalidUrl)?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || base.cannot_be_a_base()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(JellyfinError::InvalidUrl);
    }
    if !base.path().ends_with('/') {
        base.path_segments_mut()
            .map_err(|_| JellyfinError::InvalidUrl)?
            .push("");
    }
    Ok(base)
}

fn validate_device_name(name: &str) -> Result<(), JellyfinError> {
    if name.is_empty()
        || name.len() > MAX_DEVICE_NAME_BYTES
        || !name.is_ascii()
        || name.chars().any(char::is_control)
    {
        return Err(JellyfinError::InvalidRequest);
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), JellyfinError> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(JellyfinError::InvalidResponse);
    }
    Ok(())
}

fn stable_device_id(storage: &mut Storage) -> Result<String, JellyfinError> {
    if let Some(existing) = storage
        .get_preference(DEVICE_ID_PREFERENCE)
        .map_err(JellyfinError::Storage)?
        && is_valid_device_id(&existing)
    {
        return Ok(existing);
    }
    let identifier = Uuid::new_v4().simple().to_string();
    storage
        .set_preference(DEVICE_ID_PREFERENCE, &identifier)
        .map_err(JellyfinError::Storage)?;
    Ok(identifier)
}

fn is_valid_device_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn load_token(storage: &Storage) -> Result<Option<String>, JellyfinError> {
    match storage.read_credential(ACCESS_TOKEN_CREDENTIAL) {
        Ok(bytes) => {
            if bytes.is_empty() || bytes.len() > MAX_CREDENTIAL_BYTES {
                return Err(JellyfinError::InvalidResponse);
            }
            let token = String::from_utf8(bytes).map_err(|_| JellyfinError::InvalidResponse)?;
            validate_token(&token)?;
            Ok(Some(token))
        }
        Err(StorageError::CredentialNotFound) => Ok(None),
        Err(error) => Err(JellyfinError::Storage(error)),
    }
}

pub(crate) fn clear_stored_token(storage: &Storage) -> Result<(), JellyfinError> {
    let path = storage
        .credential_path(ACCESS_TOKEN_CREDENTIAL)
        .map_err(JellyfinError::Storage)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(JellyfinError::Storage(StorageError::Io(error))),
    }
}

fn escape_header(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character == '\\' || character == '"' {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn map_transport(error: reqwest::Error) -> JellyfinError {
    if error.is_timeout() {
        JellyfinError::Timeout
    } else {
        // Do not retain the source: reqwest error text can contain a URL whose
        // query string carries a Quick Connect secret.
        JellyfinError::Network
    }
}

fn map_status(status: StatusCode, headers: &header::HeaderMap) -> JellyfinError {
    match status.as_u16() {
        401 => JellyfinError::Unauthorized,
        403 => JellyfinError::Forbidden,
        429 => JellyfinError::RateLimited {
            retry_after_secs: retry_after_seconds(headers),
        },
        other => JellyfinError::Http { status: other },
    }
}

fn retry_after_seconds(headers: &header::HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

pub(crate) async fn read_body_limited(
    mut response: Response,
    limit: usize,
    cancel: &CancelToken,
) -> Result<Vec<u8>, JellyfinError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(JellyfinError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut token = cancel.clone();
    loop {
        let chunk = tokio::select! {
            biased;
            () = token.cancelled() => return Err(JellyfinError::Cancelled),
            chunk = response.chunk() => chunk.map_err(map_transport)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(JellyfinError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
