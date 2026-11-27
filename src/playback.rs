//! Typed orchestration of Jellyfin playback.
//!
//! The controller owns one [`JellyfinClient`] and one [`PlayerSession`]. It
//! negotiates PlaybackInfo, selects only a proven stream, loads the native
//! player, reports `/Sessions/Playing`, throttled progress and one final
//! `/Sessions/Playing/Stopped`, and performs typed teardown on back, stop or
//! failure. Every network error that is safe to retry lands in a bounded
//! progress journal keyed by user and play session; a newer position always
//! supersedes an older retry.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::mpsc;
use url::Url;

use crate::jellyfin::{
    ArtworkClass, BrowseItem, CachedArtwork, CancelToken, ImageRequest, ImageType, ItemQuery,
    JellyfinClient, JellyfinError, LibraryEntry, MAX_PLAYBACK_START_TICKS, MediaSelection,
    MediaStreamChoice, MediaStreamInfo, Page, PlayMethod, PlaybackInfoOptions, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, SelectionError, SelectionOptions, TICKS_PER_MILLISECOND,
    UserItemData, select_media_source,
};
use crate::player::{
    LoadRequest, MAX_POSITION_MS, PlayerCommand, PlayerError, PlayerNotification, PlayerSession,
    PlayerState,
};
use crate::state::ShutdownSignal;
use crate::storage::Storage;

/// Progress is reported every ten seconds while playing.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
/// First retry delay for a failed progress report.
pub const PROGRESS_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Maximum retry attempts for one progress snapshot.
pub const MAX_PROGRESS_RETRIES: u8 = 5;
/// Maximum progress snapshots retained in the retry journal.
pub const MAX_PROGRESS_JOURNAL: usize = 16;
/// A failed progress report older than this is never retried.
pub const MAX_PROGRESS_AGE: Duration = Duration::from_secs(120);
/// Do not resume before this many ticks (30 seconds).
pub const RESUME_MIN_TICKS: u64 = 30 * TICKS_PER_MILLISECOND * 1_000;
/// Do not resume within this many ticks of the end (30 seconds).
pub const RESUME_END_MARGIN_TICKS: u64 = 30 * TICKS_PER_MILLISECOND * 1_000;

/// Convert milliseconds to Jellyfin's 100 ns ticks with a 24 hour bound.
pub fn ticks_from_millis(milliseconds: u64) -> Result<u64, PlaybackError> {
    if milliseconds > MAX_POSITION_MS {
        return Err(PlaybackError::PositionOutOfRange);
    }
    Ok(milliseconds.saturating_mul(TICKS_PER_MILLISECOND))
}

/// Convert 100 ns ticks to milliseconds, rounded down and bounded to 24 hours.
#[must_use]
pub fn millis_from_ticks(ticks: u64) -> u64 {
    (ticks / TICKS_PER_MILLISECOND).min(MAX_POSITION_MS)
}

/// Controller tuning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackConfig {
    /// Interval between position reports while playing.
    pub progress_interval: Duration,
    /// Delay before the first retry of a failed progress report.
    pub progress_retry_delay: Duration,
    /// Maximum retries for one snapshot.
    pub max_progress_retries: u8,
    /// Requested streaming bitrate; clamped to ten megabits.
    pub max_streaming_bitrate: u64,
    /// Resume positions below this are ignored.
    pub resume_min_ticks: u64,
    /// Resume positions this close to the end restart from zero.
    pub resume_end_margin_ticks: u64,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            progress_interval: PROGRESS_INTERVAL,
            progress_retry_delay: PROGRESS_RETRY_DELAY,
            max_progress_retries: MAX_PROGRESS_RETRIES,
            max_streaming_bitrate: crate::jellyfin::START_STREAMING_BITRATE,
            resume_min_ticks: RESUME_MIN_TICKS,
            resume_end_margin_ticks: RESUME_END_MARGIN_TICKS,
        }
    }
}

impl PlaybackConfig {
    /// Validate intervals, retry count and bitrate.
    pub fn validate(&self) -> Result<(), PlaybackError> {
        if self.progress_interval.is_zero() || self.progress_retry_delay.is_zero() {
            return Err(PlaybackError::InvalidConfig {
                field: "progress interval",
            });
        }
        if self.max_progress_retries == 0 {
            return Err(PlaybackError::InvalidConfig {
                field: "progress retries",
            });
        }
        if self.max_streaming_bitrate == 0
            || self.max_streaming_bitrate > crate::jellyfin::MAX_STREAMING_BITRATE
        {
            return Err(PlaybackError::BitrateTooHigh);
        }
        Ok(())
    }
}

/// A request to start one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRequest {
    /// Opaque Jellyfin item identifier.
    pub item_id: String,
    /// Optional display title from the UI.
    pub title: Option<String>,
    /// Resume from Jellyfin user data.
    pub resume: bool,
    /// Explicit start position, overriding resume.
    pub start_ticks: Option<u64>,
    /// Explicit audio stream index.
    pub audio_stream_index: Option<i32>,
    /// Explicit subtitle stream index; any selection forces burn-in.
    pub subtitle_stream_index: Option<i32>,
    /// Request subtitle burn-in even without an explicit index.
    pub burn_in_subtitles: bool,
    /// Explicit media source identifier.
    pub media_source_id: Option<String>,
}

impl PlayRequest {
    /// Start `item_id`, resuming from server user data.
    #[must_use]
    pub fn new(item_id: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            title: None,
            resume: true,
            start_ticks: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            burn_in_subtitles: false,
            media_source_id: None,
        }
    }

    /// Attach a display title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Start from the beginning instead of resuming.
    #[must_use]
    pub const fn from_start(mut self) -> Self {
        self.resume = false;
        self.start_ticks = Some(0);
        self
    }
}

/// Commands accepted by the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackCommand {
    /// Negotiate and start an item.
    Play(PlayRequest),
    /// Pause the active playback.
    Pause,
    /// Resume the active playback.
    Resume,
    /// Toggle pause.
    TogglePause,
    /// Seek to an absolute position.
    Seek {
        /// Target position in milliseconds.
        position_ms: u64,
    },
    /// Stop and release the active playback.
    Stop,
    /// Retry the last failed start.
    Retry,
    /// Play the next episode in the current series.
    NextEpisode,
    /// Switch the audio stream, renegotiating at the current position.
    SelectAudio {
        /// Stream index, or `None` for the server default.
        index: Option<i32>,
    },
    /// Switch or disable subtitles, renegotiating at the current position.
    SelectSubtitle {
        /// Stream index, or `None` to disable subtitles.
        index: Option<i32>,
    },
    /// Fetch a library list and publish it to the UI.
    LoadLibrary {
        /// Which list to fetch.
        filter: LibraryFilter,
    },
    /// Retry the last library fetch.
    RetryLibrary,
    /// Fetch the user's top-level Jellyfin library views.
    LoadJellyfinLibraries,
    /// Fetch one bounded page of playable items below a library view.
    LoadJellyfinItems {
        /// Library (view) identifier used as `ParentId`.
        library_id: String,
        /// Zero-based first item index.
        start_index: u32,
    },
    /// Fetch one cached poster or preview image.
    LoadArtwork {
        /// Opaque item identifier.
        item_id: String,
        /// Server primary image tag, when the browse item supplied one.
        tag: Option<String>,
        /// Requested size class.
        class: ArtworkClass,
    },
    /// Report a source this build cannot play.
    Unsupported {
        /// Bounded machine-readable code.
        code: String,
        /// Bounded user-facing message.
        message: String,
    },
}

/// Library list requested by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryFilter {
    /// All playable video items.
    Jellyfin,
    /// The user's resumable items.
    ContinueWatching,
}

/// One selectable audio or subtitle stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackChoice {
    /// Jellyfin stream index.
    pub index: i32,
    /// Human-readable label.
    pub label: String,
}

/// An item the UI can offer as the next episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackItem {
    /// Opaque item identifier.
    pub id: String,
    /// Display title.
    pub title: String,
}

/// Browse and artwork updates published to the UI.
///
/// These are grouped under [`PlaybackUpdate::Browse`] so browse results never
/// disturb the player screen. Values contain opaque IDs, bounded metadata and
/// local cache paths only; no server URL or token is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseUpdate {
    /// Top-level library views are ready.
    LibrariesReady {
        /// Bounded library list.
        libraries: Vec<LibraryEntry>,
    },
    /// Library listing failed with a redacted message.
    LibrariesFailed {
        /// Redacted message.
        message: String,
    },
    /// One bounded item page is ready.
    ItemsReady {
        /// Library the page belongs to.
        library_id: String,
        /// Bounded item list.
        items: Vec<BrowseItem>,
        /// Total matching items reported by the server.
        total: u64,
        /// Next page start index, when more items exist.
        next_start: Option<u32>,
    },
    /// Item listing failed with a redacted message.
    ItemsFailed {
        /// Library the page belongs to.
        library_id: String,
        /// Redacted message.
        message: String,
    },
    /// One artwork image is cached and validated.
    ArtworkReady {
        /// Item the artwork belongs to.
        item_id: String,
        /// Requested size class.
        class: ArtworkClass,
        /// Local cached file and validated metadata.
        artwork: CachedArtwork,
    },
    /// The item has no artwork for the requested class.
    ArtworkMissing {
        /// Item the artwork belongs to.
        item_id: String,
        /// Requested size class.
        class: ArtworkClass,
    },
    /// Artwork fetching failed for a reason other than absence.
    ArtworkFailed {
        /// Item the artwork belongs to.
        item_id: String,
        /// Requested size class.
        class: ArtworkClass,
        /// Redacted message.
        message: String,
    },
    /// The Jellyfin token was rejected during a browse request.
    SessionExpired,
}

/// Updates published to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackUpdate {
    /// A browse or artwork request completed.
    Browse(BrowseUpdate),
    /// Negotiation succeeded and the child was asked to load.
    Loading {
        /// Item being opened.
        item_id: String,
        /// Display title, when known.
        title: Option<String>,
    },
    /// Position or pause state changed.
    Position {
        /// Item identifier.
        item_id: String,
        /// Position in milliseconds.
        position_ms: u64,
        /// Duration in milliseconds, when known.
        duration_ms: Option<u64>,
        /// Whether playback is paused.
        paused: bool,
    },
    /// The negotiated play method.
    Method {
        /// Direct play, remux or transcode.
        method: PlayMethod,
    },
    /// Available and selected tracks.
    Tracks {
        /// Audio tracks.
        audio: Vec<TrackChoice>,
        /// Subtitle tracks.
        subtitles: Vec<TrackChoice>,
        /// Selected audio index.
        selected_audio: Option<i32>,
        /// Selected subtitle index.
        selected_subtitle: Option<i32>,
    },
    /// A library fetch completed.
    LibraryReady {
        /// Requested list.
        filter: LibraryFilter,
        /// Bounded item list.
        items: Vec<PlaybackItem>,
    },
    /// A library fetch failed with a bounded message.
    LibraryFailed {
        /// Requested list.
        filter: LibraryFilter,
        /// Redacted message.
        message: String,
    },
    /// Playback reached its natural end.
    Ended {
        /// Finished item.
        item_id: String,
        /// Final position in milliseconds.
        position_ms: u64,
        /// Duration in milliseconds, when known.
        duration_ms: Option<u64>,
        /// Next episode, when one exists.
        next_episode: Option<PlaybackItem>,
    },
    /// Playback failed and may be retried.
    Failed {
        /// Item that failed, when known.
        item_id: Option<String>,
        /// Bounded machine-readable code.
        code: String,
        /// Redacted optional detail.
        message: Option<String>,
        /// Whether a retry request is available.
        retry_available: bool,
    },
    /// Playback was stopped by the user.
    Stopped {
        /// Item that was stopped.
        item_id: String,
    },
    /// The Jellyfin token was rejected; retries must not continue.
    SessionExpired,
    /// Informational text for the status line.
    Message {
        /// Bounded message.
        text: String,
    },
}

/// Typed controller failures.
#[derive(Debug, Error)]
pub enum PlaybackError {
    /// Jellyfin request failed.
    #[error("jellyfin playback request failed: {0}")]
    Jellyfin(#[from] JellyfinError),
    /// Native player failed.
    #[error("native player failed: {0}")]
    Player(#[from] PlayerError),
    /// No compatible media source was returned.
    #[error("media selection failed: {0}")]
    Selection(#[from] SelectionError),
    /// A position exceeded the 24 hour player bound.
    #[error("playback position exceeds the 24 hour bound")]
    PositionOutOfRange,
    /// No playback is active.
    #[error("no active playback")]
    NoActivePlayback,
    /// The current item has no next episode.
    #[error("no next episode")]
    NoNextEpisode,
    /// Controller tuning is invalid.
    #[error("invalid playback configuration: {field}")]
    InvalidConfig {
        /// Offending field.
        field: &'static str,
    },
    /// The requested bitrate exceeds the ten megabit cap.
    #[error("requested bitrate exceeds the {max} bit/s cap", max = crate::jellyfin::MAX_STREAMING_BITRATE)]
    BitrateTooHigh,
    /// The player task closed.
    #[error("player session is closed")]
    Closed,
}

fn retryable(error: &JellyfinError) -> bool {
    matches!(
        error,
        JellyfinError::Timeout
            | JellyfinError::Network
            | JellyfinError::RateLimited { .. }
            | JellyfinError::Http { status: 500..=599 }
            | JellyfinError::ResponseTooLarge
    )
}

fn retry_delay(error: &JellyfinError, fallback: Duration) -> Duration {
    match error {
        JellyfinError::RateLimited {
            retry_after_secs: Some(seconds),
        } => Duration::from_secs((*seconds).min(60)),
        _ => fallback,
    }
}

/// One snapshot waiting to be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEntry {
    /// User that produced the snapshot.
    pub user_id: String,
    /// Item identifier.
    pub item_id: String,
    /// Media source identifier.
    pub media_source_id: Option<String>,
    /// Play session identifier.
    pub play_session_id: Option<String>,
    /// Position in 100 ns ticks.
    pub position_ticks: u64,
    /// Whether the snapshot was paused.
    pub is_paused: bool,
    /// Negotiated play method.
    pub play_method: PlayMethod,
    /// Selected audio index.
    pub audio_stream_index: Option<i32>,
    /// Selected subtitle index.
    pub subtitle_stream_index: Option<i32>,
    /// Monotonic sequence within this controller.
    pub sequence: u64,
    /// When the snapshot was recorded.
    pub recorded_at: Instant,
    /// Attempts already made.
    pub attempts: u8,
    /// Earliest next attempt.
    pub next_attempt: Instant,
}

/// Bounded progress retry journal with freshness and user isolation.
#[derive(Debug, Clone)]
pub struct ProgressJournal {
    capacity: usize,
    entries: VecDeque<ProgressEntry>,
}

impl ProgressJournal {
    /// Create a journal bounded to `capacity` snapshots.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
        }
    }

    /// Number of retained snapshots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no retry is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a failed snapshot. A newer sequence for the same play session
    /// replaces the older one, and the oldest entry is evicted at capacity.
    pub fn record(&mut self, mut entry: ProgressEntry) {
        if let Some(existing) = self
            .entries
            .iter()
            .find(|existing| same_key(existing, &entry))
        {
            if existing.sequence > entry.sequence {
                return;
            }
            // A replacement snapshot (including the same sequence after a
            // failed retry) keeps the attempt budget and any later backoff.
            entry.attempts = entry.attempts.max(existing.attempts);
            entry.next_attempt = entry.next_attempt.max(existing.next_attempt);
        }
        self.entries.retain(|existing| !same_key(existing, &entry));
        self.entries.push_back(entry);
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// Snapshots due at `now`.
    ///
    /// Expired snapshots are discarded. Only the newest sequence per play
    /// session is returned, so an older retry can never overwrite fresh
    /// progress.
    pub fn due(&mut self, now: Instant) -> Vec<ProgressEntry> {
        let max_attempts = MAX_PROGRESS_RETRIES;
        self.entries.retain(|entry| {
            now.saturating_duration_since(entry.recorded_at) <= MAX_PROGRESS_AGE
                && entry.attempts < max_attempts
        });
        self.entries
            .iter()
            .filter(|entry| entry.next_attempt <= now)
            .cloned()
            .collect()
    }

    /// Remove one sequence after a successful retry or a permanent failure.
    pub fn remove(&mut self, sequence: u64) {
        self.entries.retain(|entry| entry.sequence != sequence);
    }

    /// Drop retries superseded by a successful report at `sequence`.
    pub fn acknowledge(
        &mut self,
        user_id: &str,
        item_id: &str,
        play_session_id: Option<&str>,
        sequence: u64,
    ) {
        self.entries.retain(|entry| {
            !(entry.user_id == user_id
                && entry.item_id == item_id
                && entry.play_session_id.as_deref() == play_session_id
                && entry.sequence <= sequence)
        });
    }

    /// Discard every snapshot for one user.
    pub fn clear_user(&mut self, user_id: &str) {
        self.entries.retain(|entry| entry.user_id != user_id);
    }

    /// Discard every snapshot for one play session.
    pub fn clear_play_session(&mut self, play_session_id: Option<&str>) {
        self.entries
            .retain(|entry| entry.play_session_id.as_deref() != play_session_id);
    }

    /// Inspect pending snapshots in insertion order (tests and diagnostics).
    #[must_use]
    pub fn entries(&self) -> Vec<ProgressEntry> {
        self.entries.iter().cloned().collect()
    }
}

fn same_key(left: &ProgressEntry, right: &ProgressEntry) -> bool {
    left.user_id == right.user_id
        && left.item_id == right.item_id
        && left.play_session_id == right.play_session_id
}

#[derive(Debug)]
struct ActivePlayback {
    user_id: String,
    item_id: String,
    title: Option<String>,
    generation: Option<u64>,
    selection: MediaSelection,
    position_ticks: u64,
    last_reported_ticks: u64,
    duration_ticks: Option<u64>,
    paused: bool,
    start_reported: bool,
    stop_reported: bool,
    last_progress_at: Instant,
    next_progress_attempt: Option<Instant>,
}

impl ActivePlayback {
    fn duration_ms(&self) -> Option<u64> {
        self.duration_ticks.map(millis_from_ticks)
    }

    fn start_info(&self) -> PlaybackStartInfo {
        PlaybackStartInfo {
            item_id: self.item_id.clone(),
            media_source_id: source_id(&self.selection),
            play_session_id: self.selection.play_session_id.clone(),
            position_ticks: self.position_ticks,
            is_paused: self.paused,
            is_muted: false,
            play_method: self.selection.method,
            audio_stream_index: self.selection.audio_stream_index,
            subtitle_stream_index: self.selection.subtitle_stream_index,
            can_seek: true,
            volume_level: None,
        }
    }

    fn progress_info(&self) -> PlaybackProgressInfo {
        self.start_info()
    }

    fn stop_info(&self, failed: bool) -> PlaybackStopInfo {
        PlaybackStopInfo {
            item_id: self.item_id.clone(),
            media_source_id: source_id(&self.selection),
            play_session_id: self.selection.play_session_id.clone(),
            position_ticks: self.position_ticks,
            failed,
        }
    }
}

fn source_id(selection: &MediaSelection) -> Option<String> {
    (!selection.source_id.is_empty()).then(|| selection.source_id.clone())
}

#[derive(Debug)]
struct FailedPlayback {
    request: PlayRequest,
    /// Position captured at failure. `None` means playback had not started,
    /// so a retry must keep the request's resume/start values.
    position_ticks: Option<u64>,
    code: String,
    message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum SubtitleIntent {
    Preserve,
    Set(Option<i32>),
}

/// Owns one Jellyfin client, one native player session and the shared artwork
/// cache used by `JellyfinClient::image`.
pub struct PlaybackController {
    client: JellyfinClient,
    session: PlayerSession,
    config: PlaybackConfig,
    user_id: String,
    active: Option<ActivePlayback>,
    journal: ProgressJournal,
    next_sequence: u64,
    last_request: Option<PlayRequest>,
    last_item_id: Option<String>,
    last_failed: Option<FailedPlayback>,
    last_library_filter: LibraryFilter,
    last_generation: u64,
    loads_sent: u64,
    storage: Option<Storage>,
}

impl PlaybackController {
    /// Assemble a controller for an authenticated client and a player session.
    pub fn new(
        client: JellyfinClient,
        session: PlayerSession,
        config: PlaybackConfig,
    ) -> Result<Self, PlaybackError> {
        config.validate()?;
        let user_id = client
            .session()
            .ok_or(JellyfinError::NotAuthenticated)?
            .user_id()
            .to_owned();
        Ok(Self {
            client,
            session,
            config,
            user_id,
            active: None,
            journal: ProgressJournal::new(MAX_PROGRESS_JOURNAL),
            next_sequence: 1,
            last_request: None,
            last_item_id: None,
            last_failed: None,
            last_library_filter: LibraryFilter::Jellyfin,
            last_generation: 0,
            loads_sent: 0,
            storage: None,
        })
    }

    /// Attach the storage handle that backs artwork caching.
    ///
    /// Playback itself does not need storage; a controller built without one
    /// still plays items and only rejects `LoadArtwork`. The handle must point
    /// at the same state directory the client authenticated against.
    #[must_use]
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Whether artwork can be cached and served.
    #[must_use]
    pub fn has_artwork_storage(&self) -> bool {
        self.storage.is_some()
    }

    /// Jellyfin user this controller reports as.
    #[must_use]
    pub fn user_id(&self) -> &str {
        self.user_id.as_str()
    }

    /// Item currently loaded, if any.
    #[must_use]
    pub fn active_item_id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.item_id.as_str())
    }

    /// Negotiated method for the active playback.
    #[must_use]
    pub fn active_method(&self) -> Option<PlayMethod> {
        self.active.as_ref().map(|active| active.selection.method)
    }

    /// Adopted supervisor generation of the active playback.
    #[must_use]
    pub fn active_generation(&self) -> Option<u64> {
        self.active.as_ref().and_then(|active| active.generation)
    }

    /// Retry journal, for diagnostics and tests.
    #[must_use]
    pub fn journal(&self) -> &ProgressJournal {
        &self.journal
    }

    /// Negotiate, load and report a new playback.
    pub async fn play(
        &mut self,
        request: PlayRequest,
        cancel: &CancelToken,
    ) -> Result<Vec<PlaybackUpdate>, PlaybackError> {
        if !crate::model::is_valid_item_id(&request.item_id) {
            return Err(PlaybackError::Jellyfin(JellyfinError::InvalidItemId));
        }
        let item = self.client.item(&request.item_id, cancel).await?;
        let user_data = match item.user_data.clone() {
            Some(data) => data,
            None => self.client.user_data(&request.item_id, cancel).await?,
        };
        let duration_ticks = item.run_time_ticks;
        let start_ticks = match request.start_ticks {
            Some(ticks) => ticks,
            None if request.resume => resume_position(&user_data, duration_ticks, &self.config),
            None => 0,
        };
        if start_ticks > MAX_PLAYBACK_START_TICKS {
            return Err(PlaybackError::PositionOutOfRange);
        }
        let burn_in = request.burn_in_subtitles || request.subtitle_stream_index.is_some();
        let options = PlaybackInfoOptions {
            max_streaming_bitrate: self.config.max_streaming_bitrate,
            start_time_ticks: start_ticks,
            audio_stream_index: request.audio_stream_index,
            subtitle_stream_index: request.subtitle_stream_index,
            burn_in_subtitles: burn_in,
            media_source_id: request.media_source_id.clone(),
        };
        let response = self
            .client
            .playback_info_with(&request.item_id, &options, cancel)
            .await?;
        let selection = select_media_source(
            &response,
            &SelectionOptions {
                audio_stream_index: request.audio_stream_index,
                subtitle_stream_index: request.subtitle_stream_index,
                allow_direct_play: true,
                allow_direct_stream: true,
                allow_transcoding: true,
            },
        )?;
        let stream_url = self.build_stream_url(&request.item_id, &selection, start_ticks)?;
        let title = request.title.clone().or(item.name.clone());

        let mut updates = Vec::new();
        if let Some(previous) = self.active.take() {
            updates.extend(self.teardown(previous, false, cancel).await);
        }

        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            self.client.authorization_header(),
        );
        let load = LoadRequest {
            url: stream_url.to_string(),
            headers: Some(headers),
            start_position_ms: Some(millis_from_ticks(start_ticks)),
        };
        load.validate()?;
        self.session.send(PlayerCommand::Load(load)).await?;
        self.loads_sent = self.loads_sent.saturating_add(1);

        let active = ActivePlayback {
            user_id: self.user_id.clone(),
            item_id: request.item_id.clone(),
            title: title.clone(),
            generation: None,
            position_ticks: start_ticks,
            last_reported_ticks: start_ticks,
            duration_ticks: selection.duration_ticks.or(duration_ticks),
            paused: false,
            start_reported: false,
            stop_reported: false,
            last_progress_at: Instant::now(),
            next_progress_attempt: None,
            selection: selection.clone(),
        };
        self.active = Some(active);
        self.last_item_id = Some(request.item_id.clone());
        self.last_request = Some(request.clone());
        self.last_failed = None;

        updates.push(PlaybackUpdate::Loading {
            item_id: request.item_id.clone(),
            title,
        });
        updates.push(PlaybackUpdate::Method {
            method: selection.method,
        });
        updates.extend(tracks_update(&selection));
        updates.extend(self.report_start(cancel).await);
        Ok(updates)
    }

    /// Pause the active playback.
    pub async fn pause(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Some(active) = self.active.as_mut() else {
            return vec![PlaybackUpdate::Message {
                text: "Nothing is playing".to_string(),
            }];
        };
        if active.paused {
            return Vec::new();
        }
        active.paused = true;
        if let Err(error) = self.session.send(PlayerCommand::Pause).await {
            return self.player_failure(error);
        }
        self.report_progress(true, Instant::now(), cancel).await
    }

    /// Resume the active playback.
    pub async fn resume(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Some(active) = self.active.as_mut() else {
            return vec![PlaybackUpdate::Message {
                text: "Nothing is playing".to_string(),
            }];
        };
        if !active.paused {
            return Vec::new();
        }
        active.paused = false;
        if let Err(error) = self.session.send(PlayerCommand::Resume).await {
            return self.player_failure(error);
        }
        self.report_progress(true, Instant::now(), cancel).await
    }

    /// Seek the active playback and report the new position immediately.
    pub async fn seek(&mut self, position_ms: u64, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Ok(position_ticks) = ticks_from_millis(position_ms) else {
            return vec![PlaybackUpdate::Failed {
                item_id: self.active_item_id().map(str::to_owned),
                code: "position_out_of_range".to_string(),
                message: None,
                retry_available: self.last_request.is_some(),
            }];
        };
        let Some(active) = self.active.as_mut() else {
            return vec![PlaybackUpdate::Message {
                text: "Nothing is playing".to_string(),
            }];
        };
        if let Err(error) = self.session.send(PlayerCommand::Seek { position_ms }).await {
            return self.player_failure(error);
        }
        active.position_ticks = position_ticks;
        self.report_progress(true, Instant::now(), cancel).await
    }

    /// Stop and release the active playback.
    pub async fn stop(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        let item_id = active.item_id.clone();
        let mut updates = self.teardown(active, false, cancel).await;
        updates.push(PlaybackUpdate::Stopped { item_id });
        updates
    }

    /// Update progress timers and flush due retries.
    pub async fn tick(&mut self, now: Instant, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let mut updates = Vec::new();
        if self.active.as_ref().is_some_and(|active| {
            active.start_reported
                && active
                    .next_progress_attempt
                    .is_none_or(|attempt| now >= attempt)
                && now.saturating_duration_since(active.last_progress_at)
                    >= self.config.progress_interval
        }) {
            updates.extend(self.report_progress(false, now, cancel).await);
        }
        updates.extend(self.retry_progress(now, cancel).await);
        updates
    }

    /// Handle one player notification.
    pub async fn handle_notification(
        &mut self,
        notification: PlayerNotification,
        now: Instant,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        if let Some(generation) = notification_generation(&notification) {
            if self.active.is_none() {
                return Vec::new();
            }
            let loading = matches!(
                &notification,
                PlayerNotification::State {
                    state: PlayerState::Loading,
                    ..
                }
            );
            let Some(current) = self.active.as_ref().and_then(|active| active.generation) else {
                if !loading || generation != self.loads_sent || generation <= self.last_generation {
                    return Vec::new();
                }
                if let Some(active) = self.active.as_mut() {
                    active.generation = Some(generation);
                }
                self.last_generation = generation;
                // A Loading state carries no other UI transition.
                return Vec::new();
            };
            if current != generation {
                return Vec::new();
            }
        }
        match notification {
            PlayerNotification::State { state, .. } => match state {
                PlayerState::Loading => Vec::new(),
                PlayerState::Playing => {
                    let Some(active) = self.active.as_mut() else {
                        return Vec::new();
                    };
                    active.paused = false;
                    vec![position_update(active)]
                }
                PlayerState::Paused => {
                    if let Some(active) = self.active.as_mut() {
                        active.paused = true;
                    }
                    let mut updates = self.report_progress(true, now, cancel).await;
                    if let Some(active) = self.active.as_ref() {
                        updates.push(position_update(active));
                    }
                    updates
                }
                PlayerState::Ended => self.end_active(false, now, cancel).await,
                PlayerState::Error => self.end_active(true, now, cancel).await,
                PlayerState::Idle => self.end_active(false, now, cancel).await,
            },
            PlayerNotification::Ready { .. } => Vec::new(),
            PlayerNotification::Position {
                position_ms,
                duration_ms,
                ..
            } => {
                let Some(active) = self.active.as_mut() else {
                    return Vec::new();
                };
                active.position_ticks =
                    ticks_from_millis(position_ms).unwrap_or(MAX_PLAYBACK_START_TICKS);
                if let Some(duration) = duration_ms
                    && let Ok(ticks) = ticks_from_millis(duration)
                {
                    active.duration_ticks = Some(ticks);
                }
                let update = position_update(active);
                if active
                    .next_progress_attempt
                    .is_none_or(|attempt| now >= attempt)
                    && now.saturating_duration_since(active.last_progress_at)
                        >= self.config.progress_interval
                {
                    let mut updates = self.report_progress(false, now, cancel).await;
                    updates.push(update);
                    updates
                } else {
                    vec![update]
                }
            }
            PlayerNotification::Tracks {
                video,
                audio,
                subtitle,
                ..
            } => {
                let mut updates = Vec::new();
                if let Some(active) = self.active.as_ref() {
                    if let Some(label) = video {
                        updates.push(PlaybackUpdate::Message {
                            text: format!("Video: {label}"),
                        });
                    }
                    if let Some(label) = audio {
                        updates.push(PlaybackUpdate::Message {
                            text: format!("Audio: {label}"),
                        });
                    }
                    if let Some(label) = subtitle {
                        updates.push(PlaybackUpdate::Message {
                            text: format!("Subtitle: {label}"),
                        });
                    }
                    let _ = active;
                }
                updates
            }
            PlayerNotification::Ended { .. } => self.end_active(false, now, cancel).await,
            PlayerNotification::Failed { code, message, .. } => {
                let Some(active) = self.active.take() else {
                    return Vec::new();
                };
                let item_id = Some(active.item_id.clone());
                if let Some(request) = &self.last_request {
                    self.last_failed = Some(FailedPlayback {
                        request: request.clone(),
                        position_ticks: Some(active.position_ticks),
                        code: code.clone(),
                        message: message.clone(),
                    });
                }
                let mut updates = self.teardown(active, true, cancel).await;
                updates.push(PlaybackUpdate::Failed {
                    item_id,
                    code,
                    message,
                    retry_available: self.last_failed.is_some(),
                });
                updates
            }
        }
    }

    /// Handle one command and publish its updates.
    pub async fn handle_command(
        &mut self,
        command: PlaybackCommand,
        _now: Instant,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        match command {
            PlaybackCommand::Play(request) => match self.play(request.clone(), cancel).await {
                Ok(updates) => updates,
                Err(error) => {
                    self.last_failed = Some(FailedPlayback {
                        request,
                        position_ticks: None,
                        code: error_code(&error),
                        message: Some(error.to_string()),
                    });
                    vec![PlaybackUpdate::Failed {
                        item_id: None,
                        code: error_code(&error),
                        message: Some(error.to_string()),
                        retry_available: true,
                    }]
                }
            },
            PlaybackCommand::Pause => self.pause(cancel).await,
            PlaybackCommand::Resume => self.resume(cancel).await,
            PlaybackCommand::TogglePause => {
                if self.active.as_ref().is_some_and(|active| active.paused) {
                    self.resume(cancel).await
                } else {
                    self.pause(cancel).await
                }
            }
            PlaybackCommand::Seek { position_ms } => self.seek(position_ms, cancel).await,
            PlaybackCommand::Stop => self.stop(cancel).await,
            PlaybackCommand::Retry => self.retry(cancel).await,
            PlaybackCommand::NextEpisode => self.next_episode(cancel).await,
            PlaybackCommand::SelectAudio { index } => {
                self.renegotiate(index, SubtitleIntent::Preserve, cancel)
                    .await
            }
            PlaybackCommand::SelectSubtitle { index } => {
                self.renegotiate(None, SubtitleIntent::Set(index), cancel)
                    .await
            }
            PlaybackCommand::LoadLibrary { filter } => self.load_library(filter, cancel).await,
            PlaybackCommand::RetryLibrary => {
                let filter = self.last_library_filter;
                self.load_library(filter, cancel).await
            }
            PlaybackCommand::LoadJellyfinLibraries => self.load_jellyfin_libraries(cancel).await,
            PlaybackCommand::LoadJellyfinItems {
                library_id,
                start_index,
            } => {
                self.load_jellyfin_items(&library_id, start_index, cancel)
                    .await
            }
            PlaybackCommand::LoadArtwork {
                item_id,
                tag,
                class,
            } => self.load_artwork(&item_id, tag.as_deref(), class, cancel).await,
            PlaybackCommand::Unsupported { code, message } => {
                let mut updates = self.stop(cancel).await;
                updates.push(PlaybackUpdate::Failed {
                    item_id: None,
                    code,
                    message: Some(message),
                    retry_available: false,
                });
                updates
            }
        }
    }

    async fn load_library(
        &mut self,
        filter: LibraryFilter,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        self.last_library_filter = filter;
        let result = match filter {
            LibraryFilter::Jellyfin => {
                self.client
                    .items(
                        &ItemQuery::new().media_types(&["Video"]).page(Page::first()),
                        cancel,
                    )
                    .await
            }
            LibraryFilter::ContinueWatching => self.client.resume(Page::first(), cancel).await,
        };
        match result {
            Ok(result) => {
                let items = result
                    .items
                    .into_iter()
                    .map(|item| PlaybackItem {
                        id: item.id,
                        title: item.name.unwrap_or_else(|| "Untitled".to_string()),
                    })
                    .collect();
                vec![PlaybackUpdate::LibraryReady { filter, items }]
            }
            Err(JellyfinError::Unauthorized) => vec![PlaybackUpdate::SessionExpired],
            Err(error) => vec![PlaybackUpdate::LibraryFailed {
                filter,
                message: error.to_string(),
            }],
        }
    }

    async fn load_jellyfin_libraries(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        match self.client.libraries(cancel).await {
            Ok(libraries) => vec![PlaybackUpdate::Browse(BrowseUpdate::LibrariesReady {
                libraries,
            })],
            Err(JellyfinError::Unauthorized) => {
                vec![PlaybackUpdate::Browse(BrowseUpdate::SessionExpired)]
            }
            Err(error) => vec![PlaybackUpdate::Browse(BrowseUpdate::LibrariesFailed {
                message: error.to_string(),
            })],
        }
    }

    async fn load_jellyfin_items(
        &mut self,
        library_id: &str,
        start_index: u32,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        match self
            .client
            .browse(library_id, start_index, cancel)
            .await
        {
            Ok(result) => {
                let items: Vec<BrowseItem> = result
                    .items
                    .iter()
                    .take(crate::jellyfin::BROWSE_PAGE_SIZE as usize)
                    .map(BrowseItem::from_base_item)
                    .collect();
                let next = start_index.saturating_add(
                    u32::try_from(items.len()).unwrap_or(u32::MAX),
                );
                let next_start = if !items.is_empty() && u64::from(next) < result.total_record_count {
                    Some(next)
                } else {
                    None
                };
                vec![PlaybackUpdate::Browse(BrowseUpdate::ItemsReady {
                    library_id: library_id.to_owned(),
                    items,
                    total: result.total_record_count,
                    next_start,
                })]
            }
            Err(JellyfinError::Unauthorized) => {
                vec![PlaybackUpdate::Browse(BrowseUpdate::SessionExpired)]
            }
            Err(error) => vec![PlaybackUpdate::Browse(BrowseUpdate::ItemsFailed {
                library_id: library_id.to_owned(),
                message: error.to_string(),
            })],
        }
    }

    async fn load_artwork(
        &mut self,
        item_id: &str,
        tag: Option<&str>,
        class: ArtworkClass,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        // No tag means the item has no primary artwork; do not issue a request.
        let Some(tag) = tag.filter(|tag| !tag.is_empty()) else {
            return vec![PlaybackUpdate::Browse(BrowseUpdate::ArtworkMissing {
                item_id: item_id.to_owned(),
                class,
            })];
        };
        let request = match ImageRequest::with_class(
            item_id,
            ImageType::Primary,
            Some(tag),
            class,
        ) {
            Ok(request) => request,
            Err(error) => {
                return vec![PlaybackUpdate::Browse(BrowseUpdate::ArtworkFailed {
                    item_id: item_id.to_owned(),
                    class,
                    message: error.to_string(),
                })];
            }
        };
        let Self { client, storage, .. } = self;
        let Some(storage) = storage.as_mut() else {
            return vec![PlaybackUpdate::Browse(BrowseUpdate::ArtworkFailed {
                item_id: item_id.to_owned(),
                class,
                message: "artwork storage is unavailable".to_string(),
            })];
        };
        match client.image(storage, &request, cancel).await {
            Ok(artwork) => vec![PlaybackUpdate::Browse(BrowseUpdate::ArtworkReady {
                item_id: item_id.to_owned(),
                class,
                artwork,
            })],
            Err(JellyfinError::ArtworkMissing) => {
                vec![PlaybackUpdate::Browse(BrowseUpdate::ArtworkMissing {
                    item_id: item_id.to_owned(),
                    class,
                })]
            }
            Err(JellyfinError::Unauthorized) => {
                vec![PlaybackUpdate::Browse(BrowseUpdate::SessionExpired)]
            }
            Err(error) => vec![PlaybackUpdate::Browse(BrowseUpdate::ArtworkFailed {
                item_id: item_id.to_owned(),
                class,
                message: error.to_string(),
            })],
        }
    }

    async fn retry(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Some(failed) = self.last_failed.take() else {
            return vec![PlaybackUpdate::Message {
                text: "No retry is available".to_string(),
            }];
        };
        let mut request = failed.request.clone();
        if let Some(position_ticks) = failed.position_ticks {
            request.resume = false;
            request.start_ticks = Some(position_ticks);
        }
        match self.play(request.clone(), cancel).await {
            Ok(updates) => updates,
            Err(error) => {
                self.last_failed = Some(FailedPlayback {
                    request,
                    position_ticks: failed.position_ticks,
                    code: failed.code,
                    message: failed.message,
                });
                vec![PlaybackUpdate::Failed {
                    item_id: None,
                    code: error_code(&error),
                    message: Some(error.to_string()),
                    retry_available: true,
                }]
            }
        }
    }

    async fn next_episode(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Some(item_id) = self.last_item_id.clone() else {
            return vec![PlaybackUpdate::Message {
                text: "No next episode".to_string(),
            }];
        };
        match self.client.next_episode(&item_id, cancel).await {
            Ok(Some(episode)) => {
                let request = PlayRequest::new(episode.id.clone()).with_title(
                    episode
                        .name
                        .clone()
                        .unwrap_or_else(|| "Next episode".to_string()),
                );
                match self.play(request.clone(), cancel).await {
                    Ok(updates) => updates,
                    Err(error) => {
                        self.last_failed = Some(FailedPlayback {
                            request,
                            position_ticks: None,
                            code: error_code(&error),
                            message: Some(error.to_string()),
                        });
                        vec![PlaybackUpdate::Failed {
                            item_id: Some(episode.id),
                            code: error_code(&error),
                            message: Some(error.to_string()),
                            retry_available: true,
                        }]
                    }
                }
            }
            Ok(None) => vec![PlaybackUpdate::Message {
                text: "No next episode".to_string(),
            }],
            Err(JellyfinError::Unauthorized) => vec![PlaybackUpdate::SessionExpired],
            Err(error) => vec![PlaybackUpdate::Failed {
                item_id: Some(item_id),
                code: error_code(&PlaybackError::Jellyfin(error)),
                message: None,
                retry_available: true,
            }],
        }
    }

    async fn renegotiate(
        &mut self,
        audio: Option<i32>,
        subtitle: SubtitleIntent,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        let Some(active) = self.active.as_ref() else {
            return vec![PlaybackUpdate::Message {
                text: "Nothing is playing".to_string(),
            }];
        };
        let subtitle_stream_index = match subtitle {
            SubtitleIntent::Preserve => active.selection.subtitle_stream_index,
            SubtitleIntent::Set(index) => index,
        };
        let request = PlayRequest {
            item_id: active.item_id.clone(),
            title: active.title.clone(),
            resume: false,
            start_ticks: Some(active.position_ticks),
            audio_stream_index: audio.or(active.selection.audio_stream_index),
            subtitle_stream_index,
            burn_in_subtitles: subtitle_stream_index.is_some(),
            media_source_id: source_id(&active.selection),
        };
        match self.play(request.clone(), cancel).await {
            Ok(updates) => updates,
            Err(error) => {
                self.last_failed = Some(FailedPlayback {
                    request,
                    position_ticks: None,
                    code: error_code(&error),
                    message: Some(error.to_string()),
                });
                vec![PlaybackUpdate::Failed {
                    item_id: None,
                    code: error_code(&error),
                    message: Some(error.to_string()),
                    retry_available: true,
                }]
            }
        }
    }

    async fn end_active(
        &mut self,
        failed: bool,
        _now: Instant,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        let Some(mut active) = self.active.take() else {
            return Vec::new();
        };
        if failed {
            if let Some(request) = &self.last_request {
                self.last_failed = Some(FailedPlayback {
                    request: request.clone(),
                    position_ticks: Some(active.position_ticks),
                    code: "playback_failed".to_string(),
                    message: None,
                });
            }
        } else if let Some(duration) = active.duration_ticks {
            active.position_ticks = active.position_ticks.max(duration);
        }
        active.paused = false;
        let item_id = active.item_id.clone();
        let duration_ms = active.duration_ms();
        let position_ms = millis_from_ticks(active.position_ticks);
        let next_episode = if failed {
            None
        } else {
            match self.client.next_episode(&item_id, cancel).await {
                Ok(Some(episode)) => Some(PlaybackItem {
                    id: episode.id,
                    title: episode.name.unwrap_or_else(|| "Next episode".to_string()),
                }),
                Ok(None) | Err(_) => None,
            }
        };
        let mut updates = self.teardown(active, failed, cancel).await;
        if failed {
            updates.push(PlaybackUpdate::Failed {
                item_id: Some(item_id),
                code: "playback_failed".to_string(),
                message: None,
                retry_available: self.last_failed.is_some(),
            });
        } else {
            updates.push(PlaybackUpdate::Ended {
                item_id,
                position_ms,
                duration_ms,
                next_episode,
            });
        }
        updates
    }

    async fn report_start(&mut self, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if active.start_reported {
            return Vec::new();
        }
        let info = active.start_info();
        if let Some(active) = self.active.as_mut() {
            active.start_reported = true;
        }
        match self.client.report_playback_start(&info, cancel).await {
            Ok(()) => Vec::new(),
            Err(JellyfinError::Unauthorized) => {
                self.journal.clear_user(&self.user_id);
                vec![PlaybackUpdate::SessionExpired]
            }
            Err(_) => Vec::new(),
        }
    }

    async fn report_progress(
        &mut self,
        force: bool,
        now: Instant,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if !force
            && now.saturating_duration_since(active.last_progress_at)
                < self.config.progress_interval
        {
            return Vec::new();
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let info = active.progress_info();
        let entry = progress_entry(active, sequence, now, self.config.progress_retry_delay);
        let user = active.user_id.clone();
        let item = active.item_id.clone();
        let session = active.selection.play_session_id.clone();
        let position_ticks = active.position_ticks;
        let result = self.client.report_playback_progress(&info, cancel).await;
        match result {
            Ok(()) => {
                if let Some(active) = self.active.as_mut() {
                    active.last_progress_at = now;
                    active.last_reported_ticks = position_ticks;
                    active.next_progress_attempt = None;
                }
                self.journal
                    .acknowledge(&user, &item, session.as_deref(), sequence);
                Vec::new()
            }
            Err(JellyfinError::Unauthorized) => {
                self.journal.clear_user(&self.user_id);
                vec![PlaybackUpdate::SessionExpired]
            }
            Err(error) if retryable(&error) => {
                let mut entry = entry;
                entry.next_attempt = now + retry_delay(&error, self.config.progress_retry_delay);
                if let Some(active) = self.active.as_mut() {
                    active.next_progress_attempt = Some(entry.next_attempt);
                }
                self.journal.record(entry);
                Vec::new()
            }
            Err(_) => Vec::new(),
        }
    }

    async fn retry_progress(&mut self, now: Instant, cancel: &CancelToken) -> Vec<PlaybackUpdate> {
        let due = self.journal.due(now);
        if due.is_empty() {
            return Vec::new();
        }
        let mut updates = Vec::new();
        for mut entry in due {
            let Some(active) = self.active.as_ref() else {
                self.journal.remove(entry.sequence);
                continue;
            };
            if active.user_id != entry.user_id
                || active.item_id != entry.item_id
                || active.selection.play_session_id != entry.play_session_id
            {
                self.journal.remove(entry.sequence);
                continue;
            }
            if entry.position_ticks < active.last_reported_ticks {
                self.journal.remove(entry.sequence);
                continue;
            }
            // A retry always carries the freshest position for this play
            // session, never the stale snapshot that first failed.
            entry.position_ticks = active.position_ticks.max(entry.position_ticks);
            entry.is_paused = active.paused;
            let info = PlaybackStartInfo {
                item_id: entry.item_id.clone(),
                media_source_id: entry.media_source_id.clone(),
                play_session_id: entry.play_session_id.clone(),
                position_ticks: entry.position_ticks,
                is_paused: entry.is_paused,
                is_muted: false,
                play_method: entry.play_method,
                audio_stream_index: entry.audio_stream_index,
                subtitle_stream_index: entry.subtitle_stream_index,
                can_seek: true,
                volume_level: None,
            };
            match self.client.report_playback_progress(&info, cancel).await {
                Ok(()) => {
                    self.journal.remove(entry.sequence);
                    if let Some(active) = self.active.as_mut() {
                        active.last_progress_at = now;
                        active.last_reported_ticks = entry.position_ticks;
                        active.next_progress_attempt = None;
                    }
                }
                Err(JellyfinError::Unauthorized) => {
                    self.journal.clear_user(&self.user_id);
                    updates.push(PlaybackUpdate::SessionExpired);
                    break;
                }
                Err(error) if retryable(&error) => {
                    entry.attempts = entry.attempts.saturating_add(1);
                    entry.next_attempt =
                        now + retry_delay(&error, self.config.progress_retry_delay);
                    if let Some(active) = self.active.as_mut() {
                        active.next_progress_attempt = Some(entry.next_attempt);
                    }
                    if entry.attempts < self.config.max_progress_retries {
                        self.journal.record(entry);
                    } else {
                        self.journal.remove(entry.sequence);
                        if let Some(active) = self.active.as_mut() {
                            active.next_progress_attempt = None;
                        }
                    }
                }
                Err(_) => {
                    self.journal.remove(entry.sequence);
                    if let Some(active) = self.active.as_mut() {
                        active.next_progress_attempt = None;
                    }
                }
            }
        }
        updates
    }

    async fn teardown(
        &mut self,
        active: ActivePlayback,
        failed: bool,
        cancel: &CancelToken,
    ) -> Vec<PlaybackUpdate> {
        let mut updates = Vec::new();
        if !active.stop_reported {
            let info = active.stop_info(failed);
            match self.client.report_playback_stopped(&info, cancel).await {
                Ok(()) => {}
                Err(JellyfinError::Unauthorized) => {
                    self.journal.clear_user(&self.user_id);
                    updates.push(PlaybackUpdate::SessionExpired);
                }
                Err(_) => {}
            }
        }
        let _ = self.session.send(PlayerCommand::Stop).await;
        if active.selection.method == PlayMethod::Transcode
            && let Some(play_session) = active.selection.play_session_id.as_deref()
        {
            let _ = self.client.release_transcode(play_session, cancel).await;
        }
        self.journal
            .clear_play_session(active.selection.play_session_id.as_deref());
        updates
    }

    fn build_stream_url(
        &self,
        item_id: &str,
        selection: &MediaSelection,
        _start_ticks: u64,
    ) -> Result<Url, PlaybackError> {
        match &selection.stream {
            MediaStreamChoice::DirectPlay => {
                let mut url = self.client.endpoint(&["Videos", item_id, "stream"])?;
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("static", "true");
                    if !selection.source_id.is_empty() {
                        query.append_pair("mediaSourceId", &selection.source_id);
                    }
                    if let Some(session) = &selection.play_session_id {
                        query.append_pair("playSessionId", session);
                    }
                }
                Ok(url)
            }
            MediaStreamChoice::DirectStream(url) | MediaStreamChoice::Transcode(url) => url
                .resolve(self.client.base_url())
                .map_err(PlaybackError::from),
        }
    }

    fn player_failure(&mut self, error: PlayerError) -> Vec<PlaybackUpdate> {
        let code = error.code().to_string();
        let message = Some(error.to_string());
        let item_id = self.active.as_ref().map(|active| active.item_id.clone());
        let position_ticks = self.active.as_ref().map(|active| active.position_ticks);
        if let Some(request) = self.last_request.clone() {
            self.last_failed = Some(FailedPlayback {
                request,
                position_ticks,
                code: code.clone(),
                message: message.clone(),
            });
        }
        vec![PlaybackUpdate::Failed {
            item_id,
            code,
            message,
            retry_available: self.last_failed.is_some(),
        }]
    }

    /// Run the controller until shutdown, command-channel closure or a fatal
    /// player error.
    pub async fn run(
        mut self,
        mut commands: mpsc::Receiver<PlaybackCommand>,
        updates: mpsc::Sender<PlaybackUpdate>,
        mut shutdown: ShutdownSignal,
        cancel: CancelToken,
    ) -> Result<(), PlaybackError> {
        enum Event {
            Command(Option<PlaybackCommand>),
            Notification(Option<PlayerNotification>),
            Tick,
            Shutdown,
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let event = tokio::select! {
                biased;
                () = shutdown.cancelled() => Event::Shutdown,
                command = commands.recv() => Event::Command(command),
                notification = self.session.next() => Event::Notification(notification),
                _ = ticker.tick() => Event::Tick,
            };
            let produced = match event {
                Event::Shutdown => {
                    let produced = self.stop(&cancel).await;
                    let _ = send_all(&updates, produced).await;
                    break;
                }
                Event::Command(None) => {
                    let produced = self.stop(&cancel).await;
                    let _ = send_all(&updates, produced).await;
                    break;
                }
                Event::Command(Some(command)) => {
                    self.handle_command(command, Instant::now(), &cancel).await
                }
                Event::Notification(None) => {
                    let produced = self.stop(&cancel).await;
                    let _ = send_all(&updates, produced).await;
                    break;
                }
                Event::Notification(Some(notification)) => {
                    self.handle_notification(notification, Instant::now(), &cancel)
                        .await
                }
                Event::Tick => self.tick(Instant::now(), &cancel).await,
            };
            if !send_all(&updates, produced).await {
                break;
            }
        }
        Ok(())
    }
}

async fn send_all(sender: &mpsc::Sender<PlaybackUpdate>, updates: Vec<PlaybackUpdate>) -> bool {
    for update in updates {
        if sender.send(update).await.is_err() {
            return false;
        }
    }
    true
}

fn notification_generation(notification: &PlayerNotification) -> Option<u64> {
    match notification {
        PlayerNotification::State { generation, .. }
        | PlayerNotification::Ready { generation, .. }
        | PlayerNotification::Position { generation, .. }
        | PlayerNotification::Tracks { generation, .. }
        | PlayerNotification::Ended { generation }
        | PlayerNotification::Failed { generation, .. } => Some(*generation),
    }
}

fn progress_entry(
    active: &ActivePlayback,
    sequence: u64,
    now: Instant,
    retry_delay: Duration,
) -> ProgressEntry {
    ProgressEntry {
        user_id: active.user_id.clone(),
        item_id: active.item_id.clone(),
        media_source_id: source_id(&active.selection),
        play_session_id: active.selection.play_session_id.clone(),
        position_ticks: active.position_ticks,
        is_paused: active.paused,
        play_method: active.selection.method,
        audio_stream_index: active.selection.audio_stream_index,
        subtitle_stream_index: active.selection.subtitle_stream_index,
        sequence,
        recorded_at: now,
        attempts: 0,
        next_attempt: now + retry_delay,
    }
}

fn resume_position(
    user_data: &UserItemData,
    duration_ticks: Option<u64>,
    config: &PlaybackConfig,
) -> u64 {
    let position = user_data.playback_position_ticks;
    if user_data.played || position < config.resume_min_ticks {
        return 0;
    }
    if let Some(duration) = duration_ticks
        && position.saturating_add(config.resume_end_margin_ticks) >= duration
    {
        return 0;
    }
    position
}

fn position_update(active: &ActivePlayback) -> PlaybackUpdate {
    PlaybackUpdate::Position {
        item_id: active.item_id.clone(),
        position_ms: millis_from_ticks(active.position_ticks),
        duration_ms: active.duration_ms(),
        paused: active.paused,
    }
}

fn tracks_update(selection: &MediaSelection) -> Vec<PlaybackUpdate> {
    vec![PlaybackUpdate::Tracks {
        audio: track_choices(&selection.audio_streams),
        subtitles: track_choices(&selection.subtitle_streams),
        selected_audio: selection.audio_stream_index,
        selected_subtitle: selection.subtitle_stream_index,
    }]
}

fn track_choices(streams: &[MediaStreamInfo]) -> Vec<TrackChoice> {
    streams
        .iter()
        .filter_map(|stream| {
            let index = stream.index?;
            Some(TrackChoice {
                index,
                label: track_label(stream),
            })
        })
        .collect()
}

fn track_label(stream: &MediaStreamInfo) -> String {
    let mut parts = Vec::new();
    if let Some(title) = stream
        .display_title
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(title.to_string());
    } else if let Some(language) = stream.language.as_deref().filter(|value| !value.is_empty()) {
        parts.push(language.to_string());
    }
    if let Some(codec) = stream.codec.as_deref().filter(|value| !value.is_empty()) {
        parts.push(codec.to_ascii_uppercase());
    }
    if stream.channels.is_some_and(|channels| channels > 1) {
        parts.push(format!("{} ch", stream.channels.unwrap_or(2)));
    }
    let label = if parts.is_empty() {
        format!("Track {index}", index = stream.index.unwrap_or(0))
    } else {
        parts.join(" · ")
    };
    let sanitized: String = label
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(80)
        .collect();
    sanitized.trim().to_string()
}

fn error_code(error: &PlaybackError) -> String {
    match error {
        PlaybackError::Jellyfin(JellyfinError::Unauthorized) => "unauthorized".to_string(),
        PlaybackError::Jellyfin(JellyfinError::Forbidden) => "forbidden".to_string(),
        PlaybackError::Jellyfin(JellyfinError::Timeout) => "timeout".to_string(),
        PlaybackError::Jellyfin(JellyfinError::Network) => "network".to_string(),
        PlaybackError::Jellyfin(JellyfinError::Cancelled) => "cancelled".to_string(),
        PlaybackError::Jellyfin(JellyfinError::UnexpectedRedirect) => "redirect".to_string(),
        PlaybackError::Jellyfin(JellyfinError::InvalidResponse) => "invalid_response".to_string(),
        PlaybackError::Jellyfin(JellyfinError::Http { status }) => format!("http_{status}"),
        PlaybackError::Jellyfin(_) => "jellyfin".to_string(),
        PlaybackError::Player(error) => error.code().to_string(),
        PlaybackError::Selection(SelectionError::NoCompatibleSource) => {
            "no_compatible_source".to_string()
        }
        PlaybackError::Selection(SelectionError::ServerCode { code }) => code.clone(),
        PlaybackError::PositionOutOfRange => "position_out_of_range".to_string(),
        PlaybackError::NoActivePlayback => "no_active_playback".to_string(),
        PlaybackError::NoNextEpisode => "no_next_episode".to_string(),
        PlaybackError::InvalidConfig { .. } => "invalid_config".to_string(),
        PlaybackError::BitrateTooHigh => "bitrate_too_high".to_string(),
        PlaybackError::Closed => "player_closed".to_string(),
    }
}
