//! YouTube discovery and native local playback.
//!
//! Discovery is a thin HTTPS client for the control server's
//! `GET /v1/youtube/search` endpoint. It never receives, requests or stores a
//! YouTube Data API key, a stream URL or a player ticket.
//!
//! Playback is entirely local: [`resolve`] runs the operator-managed `yt-dlp`
//! with a fixed argument vector, and [`mpv`] plays the resolved H.264 streams
//! through libmpv's private JSON IPC socket. Nothing is downloaded to disk,
//! no cookie jar is used and no URL or child output is logged. The extractor
//! is unofficial and may break; [`player::PlaybackConfig`] carries the local
//! kill switch so the feature can be removed without a rebuild.

pub mod mpv;
pub mod player;
pub mod resolve;
pub mod search;

pub use mpv::{
    BackendCommand, BackendError, BackendEvent, BackendFactory, MpvBackend, MpvBackendFactory,
    MpvOptions, PlayerBackend,
};
pub use player::{
    PLAYER_COMMAND_CAPACITY, PLAYER_EVENT_CAPACITY, PlaybackConfig, PlayerCommand, PlayerError,
    PlayerEvent, run_player,
};
pub use resolve::{
    Extractor, ExtractorError, ExtractorFailure, ResolvedMedia, YtdlpExtractor,
    build_ytdlp_arguments, parse_resolved_media,
};
pub use search::{
    HttpSearchTransport, MAX_PAGE_TOKEN_BYTES, MAX_QUERY_CHARS, MAX_SEARCH_RESULTS, SearchClient,
    SearchError, SearchPage, SearchRequest, SearchResult, SearchTransport,
};

use crate::model::{AppState, Mode, Source, YOUTUBE_ID_LEN, is_valid_youtube_id};

/// Prefix of the only watch URL this client ever constructs.
pub const WATCH_URL_PREFIX: &str = "https://www.youtube.com/watch?v=";

/// A validated 11-character YouTube video identifier.
///
/// Constructing this type is the only way to reach the extractor. Arbitrary
/// URLs, playlist identifiers and server-supplied media addresses are
/// therefore impossible to pass to local playback.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VideoId(String);

impl VideoId {
    /// Validate a wire-supplied identifier.
    pub fn parse(value: &str) -> Result<Self, InvalidVideoId> {
        if is_valid_youtube_id(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidVideoId)
        }
    }

    /// Borrow the identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fixed watch URL passed to the extractor.
    ///
    /// The value is built from the validated identifier only; it is never
    /// accepted from a remote or stored in configuration.
    #[must_use]
    pub fn watch_url(&self) -> String {
        format!("{WATCH_URL_PREFIX}{}", self.0)
    }
}

impl std::fmt::Display for VideoId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A value is not a YouTube video identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("youtube video id must be 11 URL-safe characters")]
pub struct InvalidVideoId;

impl InvalidVideoId {
    /// Stable code used by callers that need a typed reason.
    #[must_use]
    pub const fn code(self) -> &'static str {
        "invalid_video_id"
    }
}

/// Byte length of the identifier form accepted by [`VideoId`].
pub const VIDEO_ID_LEN: usize = YOUTUBE_ID_LEN;

/// Whether a snapshot proves that playback of `video_id` has started.
///
/// A remote `play_youtube` acknowledgement is derived only from this
/// predicate: the player must report the YouTube source, the same item, an
/// active playing/paused mode and a non-zero position. Merely entering the
/// loading state is never enough.
#[must_use]
pub fn playback_evidence(state: &AppState, video_id: &str) -> bool {
    playback_evidence_fields(
        state.source,
        state.item_id.as_deref(),
        state.mode,
        state.position_ms,
        state.error.as_deref(),
        video_id,
    )
}

/// Field-level form of [`playback_evidence`] for protocol snapshots.
#[must_use]
pub fn playback_evidence_fields(
    source: Option<Source>,
    item_id: Option<&str>,
    mode: Mode,
    position_ms: Option<u64>,
    error: Option<&str>,
    video_id: &str,
) -> bool {
    source == Some(Source::YouTube)
        && item_id == Some(video_id)
        && matches!(mode, Mode::Playing | Mode::Paused)
        && error.is_none()
        && position_ms.is_some_and(|position| position > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ids_are_validated_before_any_url_is_built() {
        let id = VideoId::parse("M7lc1UVf-VE").expect("valid id");
        assert_eq!(
            id.watch_url(),
            "https://www.youtube.com/watch?v=M7lc1UVf-VE"
        );
        assert_eq!(VideoId::parse("short"), Err(InvalidVideoId));
        assert_eq!(VideoId::parse("../../etc/passwd"), Err(InvalidVideoId));
        assert_eq!(VideoId::parse("M7lc1UVf-VE&x=1"), Err(InvalidVideoId));
    }

    #[test]
    fn evidence_requires_position_and_source_match() {
        let mut state = AppState {
            mode: Mode::Playing,
            source: Some(Source::YouTube),
            item_id: Some("M7lc1UVf-VE".to_owned()),
            position_ms: Some(1),
            ..AppState::default()
        };
        assert!(playback_evidence(&state, "M7lc1UVf-VE"));
        state.position_ms = Some(0);
        assert!(!playback_evidence(&state, "M7lc1UVf-VE"));
        state.position_ms = Some(900);
        assert!(!playback_evidence(&state, "M7lc1UVf-VF"));
        state.mode = Mode::Loading;
        assert!(!playback_evidence(&state, "M7lc1UVf-VE"));
    }
}
