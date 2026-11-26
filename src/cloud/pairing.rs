//! Pairing state, local-only decisions and HTTP submission.
//!
//! The remote phone can submit an eight-digit code, but only physical input
//! on the TV (keyboard or CEC) can accept or deny the resulting request. The
//! decision is submitted to `/v1/pair/decision`; the server pushes the final
//! status back over the device socket.

use std::time::SystemTime;

use thiserror::Error;
use uuid::Uuid;

use crate::cloud::error::CloudError;
use crate::cloud::http::{CloudHttp, PairInit};
use crate::cloud::protocol::{
    PairRequestPayload, PairStatus, PairStatusPayload, is_eight_digit_code, is_four_digit_code,
    parse_rfc3339,
};
use crate::input::{InputReject, LocalOrigin, PhysicalApproval};
use crate::model::InputOrigin;

/// Pairing admission failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PairError {
    /// A network origin tried to decide a physical confirmation.
    #[error("remote input cannot decide a local pairing request")]
    RemoteDenied,
    /// No pairing code is active.
    #[error("no pairing code is active")]
    NoPairing,
    /// No request is waiting for a decision.
    #[error("no pairing request is pending")]
    NoRequest,
    /// The request id does not match the pending request.
    #[error("unknown pairing request")]
    UnknownRequest,
    /// The request or code has expired.
    #[error("pairing request has expired")]
    Expired,
    /// Another request is still waiting for the operator.
    #[error("a pairing request is already pending")]
    Busy,
    /// The comparison code or pairing code had the wrong shape.
    #[error("invalid pairing code")]
    InvalidCode,
    /// The expiry timestamp was not valid RFC3339.
    #[error("invalid pairing expiry")]
    InvalidTimestamp,
}

/// A pairing code created by `/v1/pair/init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// Pairing session id.
    pub pair_id: Uuid,
    /// Eight digits, leading zeros preserved.
    pub code: String,
    /// Absolute expiry as reported by the server.
    pub expires_at: SystemTime,
}

/// A request waiting for a local decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPair {
    /// Server request id.
    pub request_id: Uuid,
    /// Four digits shown on both screens.
    pub comparison_code: String,
    /// Absolute expiry.
    pub expires_at: SystemTime,
}

/// A locally produced decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairDecision {
    /// Request being decided.
    pub request_id: Uuid,
    /// True for accept, false for deny.
    pub accept: bool,
}

impl PairDecision {
    /// Short label for diagnostics; no comparison code is included.
    #[must_use]
    pub const fn label(self) -> &'static str {
        if self.accept { "accept" } else { "deny" }
    }
}

/// Pure pairing state machine.
#[derive(Debug, Clone, Default)]
pub struct PairingState {
    code: Option<PairingCode>,
    pending: Option<PendingPair>,
}

impl PairingState {
    /// Create empty pairing state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a freshly issued pairing code.
    pub fn begin(&mut self, init: PairInit) -> Result<PairingCode, PairError> {
        if !is_eight_digit_code(&init.code) {
            return Err(PairError::InvalidCode);
        }
        let expires_at = parse_rfc3339(&init.expires_at).ok_or(PairError::InvalidTimestamp)?;
        let code = PairingCode {
            pair_id: init.pair_id,
            code: init.code,
            expires_at,
        };
        self.code = Some(code.clone());
        self.pending = None;
        Ok(code)
    }

    /// Current pairing code, if any.
    #[must_use]
    pub fn code(&self) -> Option<&PairingCode> {
        self.code.as_ref()
    }

    /// Pending request, if any.
    #[must_use]
    pub fn pending(&self) -> Option<&PendingPair> {
        self.pending.as_ref()
    }

    /// Record a server push request after validating its shape and expiry.
    pub fn on_request(
        &mut self,
        request: PairRequestPayload,
        now: SystemTime,
    ) -> Result<&PendingPair, PairError> {
        if !is_four_digit_code(&request.comparison_code) {
            return Err(PairError::InvalidCode);
        }
        let expires_at = parse_rfc3339(&request.expires_at).ok_or(PairError::InvalidTimestamp)?;
        if now >= expires_at {
            return Err(PairError::Expired);
        }
        if let Some(pending) = self.pending.as_ref()
            && pending.request_id != request.request_id
            && now < pending.expires_at
        {
            return Err(PairError::Busy);
        }
        self.pending = Some(PendingPair {
            request_id: request.request_id,
            comparison_code: request.comparison_code,
            expires_at,
        });
        self.pending.as_ref().ok_or(PairError::NoRequest)
    }

    /// Apply a `pair_status` push.
    pub fn on_status(&mut self, status: PairStatusPayload) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == status.request_id)
            && status.status != PairStatus::Pending
        {
            self.pending = None;
        }
    }

    /// Decide a pending request using a local physical origin.
    ///
    /// A remote origin is refused before the request id is even inspected.
    pub fn decide(
        &mut self,
        origin: InputOrigin,
        request_id: Uuid,
        accept: bool,
        now: SystemTime,
    ) -> Result<PairDecision, PairError> {
        let _local = LocalOrigin::try_from(origin).map_err(|_| PairError::RemoteDenied)?;
        let (pending_id, expires_at) = self
            .pending
            .as_ref()
            .map(|pending| (pending.request_id, pending.expires_at))
            .ok_or(PairError::NoRequest)?;
        if pending_id != request_id {
            return Err(PairError::UnknownRequest);
        }
        if now >= expires_at {
            self.pending = None;
            return Err(PairError::Expired);
        }
        if accept {
            PhysicalApproval::new(origin, request_id).map_err(|_| PairError::RemoteDenied)?;
        }
        Ok(PairDecision { request_id, accept })
    }

    /// Clear a decided request after the HTTP decision succeeded.
    pub fn finish(&mut self, request_id: Uuid) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            self.pending = None;
        }
    }

    /// Clear all pairing state (for example on device revocation).
    pub fn reset(&mut self) {
        self.code = None;
        self.pending = None;
    }
}

/// Pairing state plus the HTTPS client that submits decisions.
pub struct PairingSession {
    http: CloudHttp,
    state: PairingState,
}

impl PairingSession {
    /// Build a session around an authenticated client.
    #[must_use]
    pub fn new(http: CloudHttp) -> Self {
        Self {
            http,
            state: PairingState::new(),
        }
    }

    /// Current pairing code.
    #[must_use]
    pub fn code(&self) -> Option<&PairingCode> {
        self.state.code()
    }

    /// Pending request.
    #[must_use]
    pub fn pending(&self) -> Option<&PendingPair> {
        self.state.pending()
    }

    /// Request a new code and make it available for TV display.
    pub async fn begin(&mut self) -> Result<PairingCode, CloudError> {
        let init = self.http.pair_init().await?;
        Ok(self.state.begin(init)?)
    }

    /// Install a request received over the device socket.
    pub fn on_request(
        &mut self,
        request: PairRequestPayload,
        now: SystemTime,
    ) -> Result<&PendingPair, PairError> {
        self.state.on_request(request, now)
    }

    /// Apply a `pair_status` push.
    pub fn on_status(&mut self, status: PairStatusPayload) {
        self.state.on_status(status);
    }

    /// Decide locally and submit the decision.
    ///
    /// The HTTP call happens only after provenance, expiry and request id
    /// checks pass. A remote origin can never reach this submission.
    pub async fn decide(
        &mut self,
        origin: InputOrigin,
        request_id: Uuid,
        accept: bool,
        now: SystemTime,
    ) -> Result<PairDecision, CloudError> {
        let decision = self.state.decide(origin, request_id, accept, now)?;
        self.http.pair_decision(request_id, accept).await?;
        self.state.finish(request_id);
        Ok(decision)
    }

    /// Drop pairing state without submitting anything.
    pub fn reset(&mut self) {
        self.state.reset();
    }
}

/// Convert an input-layer rejection without leaking it as a generic error.
impl From<InputReject> for PairError {
    fn from(_error: InputReject) -> Self {
        Self::RemoteDenied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::time::UNIX_EPOCH;

    fn init() -> PairInit {
        PairInit {
            pair_id: Uuid::new_v4(),
            code: "00012345".to_owned(),
            expires_at: "2026-11-26T12:02:00Z".to_owned(),
        }
    }

    fn request() -> PairRequestPayload {
        PairRequestPayload {
            request_id: Uuid::new_v4(),
            comparison_code: "1234".to_owned(),
            expires_at: "2026-11-26T12:02:00Z".to_owned(),
        }
    }

    fn before_expiry() -> SystemTime {
        parse_rfc3339("2026-11-26T12:01:00Z").expect("timestamp")
    }

    #[test]
    fn remote_origin_cannot_decide() {
        let mut state = PairingState::new();
        state.begin(init()).expect("code");
        let request = request();
        let id = request.request_id;
        state.on_request(request, before_expiry()).expect("pending");
        assert_eq!(
            state.decide(InputOrigin::Remote, id, true, before_expiry()),
            Err(PairError::RemoteDenied)
        );
        assert_eq!(
            state.decide(InputOrigin::Remote, id, false, before_expiry()),
            Err(PairError::RemoteDenied)
        );
        assert!(state.pending().is_some());
    }

    #[test]
    fn local_keyboard_and_cec_can_accept_or_deny() {
        for origin in [InputOrigin::LocalKeyboard, InputOrigin::LocalCec] {
            let mut state = PairingState::new();
            state.begin(init()).expect("code");
            let request = request();
            let id = request.request_id;
            state.on_request(request, before_expiry()).expect("pending");
            assert_eq!(
                state.decide(origin, id, true, before_expiry()),
                Ok(PairDecision {
                    request_id: id,
                    accept: true
                })
            );
        }
    }

    #[test]
    fn expired_requests_are_not_admitted() {
        let mut state = PairingState::new();
        state.begin(init()).expect("code");
        let request = request();
        let id = request.request_id;
        state.on_request(request, before_expiry()).expect("pending");
        let after = before_expiry() + Duration::from_secs(120);
        assert_eq!(
            state.decide(InputOrigin::LocalCec, id, true, after),
            Err(PairError::Expired)
        );
        assert!(state.pending().is_none());
    }

    #[test]
    fn status_push_clears_the_pending_request() {
        let mut state = PairingState::new();
        state.begin(init()).expect("code");
        let request = request();
        let id = request.request_id;
        state.on_request(request, before_expiry()).expect("pending");
        state.on_status(PairStatusPayload {
            request_id: id,
            status: PairStatus::Rejected,
        });
        assert!(state.pending().is_none());
    }

    #[test]
    fn epoch_timestamp_round_trip_is_stable() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(UNIX_EPOCH));
    }
}
