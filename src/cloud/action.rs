//! Remote command policy, deduplication and application boundary.
//!
//! A decoded action is not enough to change the TV. It must pass the mode and
//! permission policy, be deduplicated, and then be applied by the owning UI
//! or playback path. The acknowledgement is produced only from the result of
//! that application.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use uuid::Uuid;

use crate::cloud::protocol::AckPayload;
use crate::model::{Action, Mode};

/// Number of recently seen command ids retained in memory.
pub const DEDUPE_CAPACITY: usize = 256;
/// How long a seen command id is remembered.
pub const DEDUPE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Bound for pending remote commands per device connection.
pub const COMMAND_QUEUE_CAPACITY: usize = 32;
/// Bound for a single remote application attempt.
pub const COMMAND_APPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// A safe, typed reason attached to a rejection acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejection {
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Human-readable explanation, never derived from frame text.
    pub message: &'static str,
}

impl Rejection {
    /// Build a rejection.
    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Invalid action shape or range.
    pub const fn invalid_action() -> Self {
        Self::new("invalid_action", "action is not valid for protocol v1")
    }

    /// Action is not meaningful in the current mode.
    pub const fn invalid_mode() -> Self {
        Self::new("invalid_mode", "action is not allowed in the current mode")
    }

    /// Feature or local permission is absent.
    pub const fn not_permitted() -> Self {
        Self::new("not_permitted", "action is not permitted by local policy")
    }

    /// A source is not implemented by this build.
    pub const fn unsupported(code: &'static str, message: &'static str) -> Self {
        Self::new(code, message)
    }
}

/// Local permission snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandPolicy {
    /// A local Jellyfin origin and session are configured.
    pub jellyfin: bool,
    /// YouTube playback exists and may be started remotely.
    ///
    /// This stays false until the C010 playback path is merged. Advertising
    /// it earlier would acknowledge a command before playback can start.
    pub youtube_playback: bool,
}

impl CommandPolicy {
    /// Policy matching the current client build.
    #[must_use]
    pub const fn for_build(jellyfin: bool) -> Self {
        Self {
            jellyfin,
            youtube_playback: false,
        }
    }

    /// Check mode and permissions before any application is attempted.
    pub fn evaluate(&self, action: &Action, mode: Mode) -> Result<(), Rejection> {
        action.validate().map_err(|_| Rejection::invalid_action())?;
        match action {
            Action::Navigate { .. } if mode == Mode::Standby => Err(Rejection::invalid_mode()),
            Action::SetPaused { .. } if !matches!(mode, Mode::Playing | Mode::Paused) => {
                Err(Rejection::invalid_mode())
            }
            Action::Seek { .. } if !matches!(mode, Mode::Playing | Mode::Paused) => {
                Err(Rejection::invalid_mode())
            }
            Action::Stop if mode == Mode::Standby => Err(Rejection::invalid_mode()),
            Action::PlayJellyfin { .. } => {
                if !self.jellyfin {
                    return Err(Rejection::not_permitted());
                }
                if mode == Mode::Standby {
                    return Err(Rejection::invalid_mode());
                }
                Ok(())
            }
            Action::PlayYoutube { .. } => {
                if self.youtube_playback {
                    Ok(())
                } else {
                    Err(Rejection::unsupported(
                        "unsupported_source",
                        "YouTube playback is not available in this build",
                    ))
                }
            }
            Action::Standby { enabled: true } if mode == Mode::Standby => {
                Err(Rejection::invalid_mode())
            }
            Action::Standby { enabled: false } if mode != Mode::Standby => {
                Err(Rejection::invalid_mode())
            }
            Action::SetVolume { .. }
            | Action::Standby { .. }
            | Action::Navigate { .. }
            | Action::SetPaused { .. }
            | Action::Seek { .. }
            | Action::Stop => Ok(()),
        }
    }
}

/// Owned application request handed to the existing local paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRequest {
    /// Command identifier used for the acknowledgement.
    pub command_id: Uuid,
    /// Already validated action.
    pub action: Action,
    /// Mode observed when the command was accepted.
    pub mode: Mode,
    /// When the command was received from the socket.
    pub received_at: Instant,
    /// Last instant at which the command may still be applied.
    pub deadline: Instant,
}

impl ApplyRequest {
    /// Whether the queued command missed its application deadline.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// Result reported by the local application path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The action was applied by the UI or playback path.
    Applied,
    /// The local path refused the action without side effects.
    Rejected(Rejection),
    /// The local path attempted the action and failed.
    Failed(Rejection),
}

/// Asynchronous adapter implemented by the owner of the UI/playback paths.
///
/// The production wiring sends the action through the normal command channel
/// and completes the future only after the path reports success. The cloud
/// link never invents an `applied` acknowledgement.
pub trait CommandApplier: Send + Sync {
    /// Apply one action.
    fn apply(&self, request: ApplyRequest) -> BoxFuture<'_, ApplyOutcome>;
}

/// A command applier that always rejects.
///
/// Useful for a headless build or before the local integration is wired; it
/// keeps the link protocol-correct without claiming actions were applied.
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectingApplier;

impl CommandApplier for RejectingApplier {
    fn apply(&self, _request: ApplyRequest) -> BoxFuture<'_, ApplyOutcome> {
        Box::pin(async {
            ApplyOutcome::Rejected(Rejection::new(
                "applier_unavailable",
                "local command application is not available",
            ))
        })
    }
}

/// Result of queuing a command for application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueReject {
    /// The bounded queue is full.
    Full,
    /// The worker has stopped.
    Closed,
}

/// Bounded command queue shared by the socket reader and one worker.
///
/// The concrete queue is a Tokio `mpsc` channel in [`crate::cloud::link`].
/// This wrapper makes the capacity and the non-blocking admission rule
/// explicit and testable.
pub struct CommandQueue {
    sender: tokio::sync::mpsc::Sender<ApplyRequest>,
}

impl CommandQueue {
    /// Create a bounded queue. A capacity of zero is rejected.
    pub fn new(
        capacity: usize,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<ApplyRequest>), QueueReject> {
        if capacity == 0 {
            return Err(QueueReject::Closed);
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        Ok((Self { sender }, receiver))
    }

    /// Try to enqueue without blocking.
    pub fn try_push(&self, request: ApplyRequest) -> Result<(), QueueReject> {
        self.sender.try_send(request).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => QueueReject::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => QueueReject::Closed,
        })
    }
}

#[derive(Debug, Clone)]
struct SeenCommand {
    command_id: Uuid,
    seen_at: Instant,
    ack: AckPayload,
}

/// Last-256-commands/10-minutes deduplication cache.
///
/// The cache survives reconnects so a command is never replayed after a
/// network interruption. It is a plain in-memory ring with a linear scan over
/// at most 256 entries.
#[derive(Debug, Clone)]
pub struct DedupeCache {
    capacity: usize,
    window: Duration,
    entries: VecDeque<SeenCommand>,
}

impl Default for DedupeCache {
    fn default() -> Self {
        Self::new(DEDUPE_CAPACITY, DEDUPE_WINDOW)
    }
}

impl DedupeCache {
    /// Build a cache with an explicit bound.
    pub fn new(capacity: usize, window: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            window,
            entries: VecDeque::new(),
        }
    }

    /// Return the cached acknowledgement for a command id, if still valid.
    pub fn lookup(&mut self, command_id: Uuid, now: Instant) -> Option<AckPayload> {
        self.expire(now);
        self.entries
            .iter()
            .find(|entry| entry.command_id == command_id)
            .map(|entry| entry.ack.clone())
    }

    /// Record or replace the acknowledgement for a command id.
    pub fn record(&mut self, command_id: Uuid, now: Instant, ack: AckPayload) {
        self.expire(now);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.command_id == command_id)
        {
            entry.ack = ack;
            entry.seen_at = now;
            return;
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(SeenCommand {
            command_id,
            seen_at: now,
            ack,
        });
    }

    /// Number of live entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache has no live entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn expire(&mut self, now: Instant) {
        let window = self.window;
        self.entries
            .retain(|entry| now.saturating_duration_since(entry.seen_at) <= window);
    }
}

/// Shared applier handle used by the socket worker.
pub type SharedApplier = Arc<dyn CommandApplier>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::protocol::AckStatus;

    fn ack(command_id: Uuid) -> AckPayload {
        AckPayload::new(command_id, AckStatus::Applied, None)
    }

    #[test]
    fn duplicate_ids_return_the_recorded_ack_without_reapplying() {
        let now = Instant::now();
        let id = Uuid::new_v4();
        let mut cache = DedupeCache::default();
        cache.record(id, now, ack(id));
        assert_eq!(
            cache.lookup(id, now + Duration::from_secs(1)),
            Some(ack(id))
        );
        assert_eq!(cache.lookup(id, now + DEDUPE_WINDOW), Some(ack(id)));
        assert_eq!(
            cache.lookup(id, now + DEDUPE_WINDOW + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn cache_retains_at_most_256_ids() {
        let now = Instant::now();
        let mut cache = DedupeCache::default();
        for _ in 0..300 {
            let id = Uuid::new_v4();
            cache.record(id, now, ack(id));
        }
        assert_eq!(cache.len(), DEDUPE_CAPACITY);
    }

    #[test]
    fn queued_commands_expire_after_the_protocol_deadline() {
        let received_at = Instant::now();
        let request = ApplyRequest {
            command_id: Uuid::new_v4(),
            action: Action::Stop,
            mode: Mode::Home,
            received_at,
            deadline: received_at + COMMAND_APPLY_TIMEOUT,
        };
        assert!(!request.is_expired(received_at));
        assert!(request.is_expired(received_at + COMMAND_APPLY_TIMEOUT));
    }

    #[test]
    fn youtube_is_rejected_until_the_c010_path_exists() {
        let policy = CommandPolicy::for_build(true);
        let action = Action::PlayYoutube {
            video_id: "M7lc1UVf-VE".to_owned(),
        };
        assert_eq!(
            policy.evaluate(&action, Mode::Home),
            Err(Rejection::unsupported(
                "unsupported_source",
                "YouTube playback is not available in this build"
            ))
        );
    }
}
