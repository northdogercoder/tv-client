//! Authentication, Quick Connect and session lifecycle.

use std::fmt;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::jellyfin::JellyfinClient;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::client::Session;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::models::AuthenticationResult;
use crate::storage::Storage;

const MAX_USERNAME_BYTES: usize = 256;
const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_QUICK_CONNECT_SECRET_BYTES: usize = 512;

/// A pending Quick Connect request.
#[derive(Clone)]
pub struct QuickConnectInitiation {
    secret: String,
    code: String,
    expires_at: Instant,
}

impl QuickConnectInitiation {
    /// Short code the user enters on another Jellyfin client.
    #[must_use]
    pub fn code(&self) -> &str {
        self.code.as_str()
    }

    /// Whether the approval window has elapsed.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// Instant at which this request expires.
    #[must_use]
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }
}

impl fmt::Debug for QuickConnectInitiation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QuickConnectInitiation")
            .field("code", &self.code)
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Result of polling a Quick Connect request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickConnectPoll {
    /// The user has not approved the code yet.
    Pending,
    /// The user approved the code; authenticate with the initiation.
    Approved,
}

impl JellyfinClient {
    /// Authenticate with a username and password.
    ///
    /// The password is used for this request only and is never persisted or
    /// logged. On success the access token is written to the credential store.
    pub async fn authenticate_by_name(
        &mut self,
        storage: &mut Storage,
        username: &str,
        password: &str,
        cancel: &CancelToken,
    ) -> Result<Session, JellyfinError> {
        if username.is_empty()
            || username.len() > MAX_USERNAME_BYTES
            || username.chars().any(char::is_control)
            || password.is_empty()
            || password.len() > MAX_PASSWORD_BYTES
        {
            return Err(JellyfinError::InvalidRequest);
        }
        let url = self.endpoint(&["Users", "AuthenticateByName"])?;
        let body = NamePassword {
            username,
            pw: password,
        };
        let result = self
            .post_json::<AuthenticationResult, _>(url, &body, cancel)
            .await
            .map_err(auth_error)?;
        self.install_session(storage, result)
    }

    /// Start a Quick Connect request.
    ///
    /// The server must have Quick Connect enabled.
    pub async fn quick_connect_initiate(
        &self,
        cancel: &CancelToken,
    ) -> Result<QuickConnectInitiation, JellyfinError> {
        let url = self.endpoint(&["QuickConnect", "Initiate"])?;
        let result = self
            .post_json::<QuickConnectResult, _>(url, &EmptyBody {}, cancel)
            .await
            .map_err(quick_connect_error)?;
        if result.secret.is_empty()
            || result.secret.len() > MAX_QUICK_CONNECT_SECRET_BYTES
            || result.code.is_empty()
            || result.code.len() > 32
            || result.secret.chars().any(char::is_control)
            || result.code.chars().any(char::is_control)
        {
            return Err(JellyfinError::InvalidResponse);
        }
        let expires_at = Instant::now()
            .checked_add(self.quick_connect_lifetime)
            .ok_or(JellyfinError::InvalidRequest)?;
        Ok(QuickConnectInitiation {
            secret: result.secret,
            code: result.code,
            expires_at,
        })
    }

    /// Poll a Quick Connect request once.
    pub async fn quick_connect_poll(
        &self,
        initiation: &QuickConnectInitiation,
        cancel: &CancelToken,
    ) -> Result<QuickConnectPoll, JellyfinError> {
        if initiation.is_expired() {
            return Err(JellyfinError::QuickConnectExpired);
        }
        let mut url = self.endpoint(&["QuickConnect", "Connect"])?;
        url.query_pairs_mut()
            .append_pair("secret", &initiation.secret);
        let status = self
            .get_json::<QuickConnectStatus>(url, cancel)
            .await
            .map_err(quick_connect_error)?;
        Ok(if status.authenticated {
            QuickConnectPoll::Approved
        } else {
            QuickConnectPoll::Pending
        })
    }

    /// Exchange an approved Quick Connect request for an access token.
    pub async fn quick_connect_authenticate(
        &mut self,
        storage: &mut Storage,
        initiation: &QuickConnectInitiation,
        cancel: &CancelToken,
    ) -> Result<Session, JellyfinError> {
        if initiation.is_expired() {
            return Err(JellyfinError::QuickConnectExpired);
        }
        let url = self.endpoint(&["Users", "AuthenticateWithQuickConnect"])?;
        let body = QuickConnectSecret {
            secret: &initiation.secret,
        };
        let result = self
            .post_json::<AuthenticationResult, _>(url, &body, cancel)
            .await
            .map_err(quick_connect_auth_error)?;
        self.install_session(storage, result)
    }
}

fn auth_error(error: JellyfinError) -> JellyfinError {
    match error {
        JellyfinError::Unauthorized | JellyfinError::Http { status: 400 } => {
            JellyfinError::AuthenticationRejected
        }
        other => other,
    }
}

fn quick_connect_error(error: JellyfinError) -> JellyfinError {
    match error {
        JellyfinError::Unauthorized
        | JellyfinError::Forbidden
        | JellyfinError::Http { status: 400 }
        | JellyfinError::Http { status: 404 } => JellyfinError::QuickConnectUnavailable,
        other => other,
    }
}

fn quick_connect_auth_error(error: JellyfinError) -> JellyfinError {
    match error {
        JellyfinError::Unauthorized
        | JellyfinError::Forbidden
        | JellyfinError::Http { status: 400 } => JellyfinError::QuickConnectNotApproved,
        JellyfinError::Http { status: 404 } => JellyfinError::QuickConnectUnavailable,
        other => other,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NamePassword<'a> {
    username: &'a str,
    pw: &'a str,
}

#[derive(Serialize)]
struct EmptyBody {}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct QuickConnectSecret<'a> {
    secret: &'a str,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct QuickConnectResult {
    secret: String,
    code: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct QuickConnectStatus {
    authenticated: bool,
}
