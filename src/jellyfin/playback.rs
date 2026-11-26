//! PlaybackInfo negotiation, device profile and playback reporting.
//!
//! The profile describes what the C006 GStreamer child has actually proven on
//! the target hardware: H.264 8-bit SDR up to 1080p30 and AAC-LC stereo in an
//! MP4 container. Everything else is remuxed or transcoded by Jellyfin to HLS
//! H.264/AAC. Stream URLs are only ever resolved against the configured
//! Jellyfin origin and base path; nothing is proxied through another host.

use serde::Serialize;
use thiserror::Error;

use crate::jellyfin::JellyfinClient;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::models::{MediaSourceInfo, MediaStreamInfo, PlaybackInfoResponse, StreamUrl};

/// Initial streaming bitrate requested from the server (6 Mbit/s).
pub const START_STREAMING_BITRATE: u64 = 6_000_000;
/// Hard cap for any requested streaming bitrate (10 Mbit/s).
pub const MAX_STREAMING_BITRATE: u64 = 10_000_000;
/// Largest proven video width.
pub const MAX_VIDEO_WIDTH: u32 = 1920;
/// Largest proven video height.
pub const MAX_VIDEO_HEIGHT: u32 = 1080;
/// Highest proven frame rate.
pub const MAX_FRAME_RATE: u32 = 30;
/// Highest proven H.264 level (4.1).
pub const MAX_H264_LEVEL: i32 = 41;
/// Highest proven audio channel count.
pub const MAX_AUDIO_CHANNELS: u32 = 2;
/// Ticks in one millisecond (Jellyfin uses 100 ns ticks).
pub const TICKS_PER_MILLISECOND: u64 = 10_000;
/// Largest accepted start position (24 hours).
pub const MAX_PLAYBACK_START_TICKS: u64 = 24 * 60 * 60 * TICKS_PER_MILLISECOND;

/// Direct-play container proven by C006 hardware runs.
pub const DIRECT_PLAY_CONTAINER: &str = "mp4";
/// HLS transcoding container.
pub const TRANSCODE_CONTAINER: &str = "ts";

/// Options for one PlaybackInfo negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackInfoOptions {
    /// Requested maximum streaming bitrate. Defaults to
    /// [`START_STREAMING_BITRATE`].
    pub max_streaming_bitrate: u64,
    /// Start position in 100 ns ticks, used for resume.
    pub start_time_ticks: u64,
    /// Explicit audio stream index, otherwise the server default.
    pub audio_stream_index: Option<i32>,
    /// Explicit subtitle stream index, otherwise subtitles are disabled.
    pub subtitle_stream_index: Option<i32>,
    /// Ask the server to burn subtitles into the video stream.
    pub burn_in_subtitles: bool,
    /// Select one media source explicitly.
    pub media_source_id: Option<String>,
}

impl Default for PlaybackInfoOptions {
    fn default() -> Self {
        Self {
            max_streaming_bitrate: START_STREAMING_BITRATE,
            start_time_ticks: 0,
            audio_stream_index: None,
            subtitle_stream_index: None,
            burn_in_subtitles: false,
            media_source_id: None,
        }
    }
}

impl PlaybackInfoOptions {
    /// Validate bitrate and start position bounds.
    pub fn validate(&self) -> Result<(), JellyfinError> {
        if self.max_streaming_bitrate == 0
            || self.max_streaming_bitrate > MAX_STREAMING_BITRATE
            || self.start_time_ticks > MAX_PLAYBACK_START_TICKS
        {
            return Err(JellyfinError::InvalidRequest);
        }
        if self
            .media_source_id
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.len() > 512)
        {
            return Err(JellyfinError::InvalidRequest);
        }
        Ok(())
    }

    /// Clamp a requested bitrate into the accepted range.
    #[must_use]
    pub fn with_bitrate(mut self, bitrate: u64) -> Self {
        self.max_streaming_bitrate = bitrate.clamp(1, MAX_STREAMING_BITRATE);
        self
    }

    /// Set the resume position from milliseconds.
    pub fn with_start_ms(mut self, milliseconds: u64) -> Result<Self, JellyfinError> {
        if milliseconds > MAX_PLAYBACK_START_TICKS / TICKS_PER_MILLISECOND {
            return Err(JellyfinError::InvalidRequest);
        }
        self.start_time_ticks = milliseconds.saturating_mul(TICKS_PER_MILLISECOND);
        Ok(self)
    }
}

/// DLNA profile type value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProfileType {
    /// Audio profile.
    Audio,
    /// Video profile.
    Video,
    /// Subtitle profile.
    Subtitle,
}

/// One direct-play profile entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DirectPlayProfile {
    /// Accepted container list.
    pub container: String,
    /// Accepted audio codec list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    /// Accepted video codec list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    /// Profile kind.
    #[serde(rename = "Type")]
    pub profile_type: ProfileType,
}

/// One transcoding profile entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct TranscodingProfile {
    /// Output container.
    pub container: String,
    /// Profile kind.
    #[serde(rename = "Type")]
    pub profile_type: ProfileType,
    /// Output video codec.
    pub video_codec: String,
    /// Output audio codec.
    pub audio_codec: String,
    /// `hls` or `http`.
    pub protocol: String,
    /// Encoding context, `Streaming` for live playback.
    pub context: String,
    /// Maximum audio channels.
    pub max_audio_channels: String,
    /// Minimum HLS segments before playback starts.
    pub min_segments: u32,
    /// Whether a non-keyframe may start a segment.
    pub break_on_non_key_frames: bool,
    /// Whether timestamps are copied.
    pub copy_timestamps: bool,
    /// Whether subtitles are advertised in the manifest (we burn in instead).
    pub enable_subtitles_in_manifest: bool,
}

/// Condition operator for a codec profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProfileConditionType {
    /// Exact equality.
    Equals,
    /// Inequality.
    NotEquals,
    /// Upper bound.
    LessThanEqual,
    /// Lower bound.
    GreaterThanEqual,
    /// One of a comma-separated set.
    EqualsAny,
}

/// Codec property constrained by a profile condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProfileConditionProperty {
    /// Audio channel count.
    AudioChannels,
    /// Video width.
    Width,
    /// Video height.
    Height,
    /// Video bit depth.
    VideoBitDepth,
    /// Video frame rate.
    VideoFramerate,
    /// H.264 level.
    VideoLevel,
    /// SDR/HDR range type.
    VideoRangeType,
}

/// One codec constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProfileCondition {
    /// Comparison operator.
    pub condition: ProfileConditionType,
    /// Constrained property.
    pub property: ProfileConditionProperty,
    /// Compared value.
    pub value: String,
    /// Whether violating the condition forces a transcode.
    pub is_required: bool,
}

/// One codec profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CodecProfile {
    /// Profile kind.
    #[serde(rename = "Type")]
    pub profile_type: ProfileType,
    /// Codec list the profile applies to.
    pub codec: String,
    /// Conditions that must hold for direct playback.
    pub conditions: Vec<ProfileCondition>,
    /// Conditions applied when the profile matches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apply_conditions: Vec<ProfileCondition>,
}

/// Subtitle delivery method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SubtitleDeliveryMethod {
    /// Burn the subtitle into the video.
    Encode,
    /// Mux the subtitle into the stream.
    Embed,
    /// Serve a sidecar subtitle.
    External,
    /// Serve subtitles through HLS.
    Hls,
    /// Ignore the subtitle.
    Drop,
}

/// One subtitle profile entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleProfile {
    /// Subtitle format.
    pub format: String,
    /// Delivery method.
    pub method: SubtitleDeliveryMethod,
}

/// Jellyfin device profile describing the proven C006 pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceProfile {
    /// Human-readable profile name.
    pub name: String,
    /// Maximum streamed content bitrate.
    pub max_streaming_bitrate: u64,
    /// Maximum statically streamed content bitrate.
    pub max_static_bitrate: u64,
    /// Proven direct-play combinations only.
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    /// HLS H.264/AAC transcode profile.
    pub transcoding_profiles: Vec<TranscodingProfile>,
    /// Codec constraints that force a transcode.
    pub codec_profiles: Vec<CodecProfile>,
    /// Subtitle delivery profile (burn-in only).
    pub subtitle_profiles: Vec<SubtitleProfile>,
}

/// Build the device profile for a requested streaming bitrate.
///
/// The bitrate is clamped to [`MAX_STREAMING_BITRATE`]. Direct play is limited
/// to MP4/H.264 8-bit SDR at or below 1080p30 with AAC stereo. HDR, HEVC, AV1
/// and VP9 therefore always negotiate a transcode.
#[must_use]
pub fn device_profile(bitrate: u64) -> DeviceProfile {
    let bitrate = bitrate.clamp(1, MAX_STREAMING_BITRATE);
    let video_conditions = vec![
        condition(
            ProfileConditionType::Equals,
            ProfileConditionProperty::VideoRangeType,
            "SDR",
        ),
        condition(
            ProfileConditionType::LessThanEqual,
            ProfileConditionProperty::VideoBitDepth,
            "8",
        ),
        condition(
            ProfileConditionType::LessThanEqual,
            ProfileConditionProperty::Width,
            &MAX_VIDEO_WIDTH.to_string(),
        ),
        condition(
            ProfileConditionType::LessThanEqual,
            ProfileConditionProperty::Height,
            &MAX_VIDEO_HEIGHT.to_string(),
        ),
        condition(
            ProfileConditionType::LessThanEqual,
            ProfileConditionProperty::VideoFramerate,
            &MAX_FRAME_RATE.to_string(),
        ),
        condition(
            ProfileConditionType::LessThanEqual,
            ProfileConditionProperty::VideoLevel,
            &MAX_H264_LEVEL.to_string(),
        ),
    ];
    let audio_conditions = vec![condition(
        ProfileConditionType::LessThanEqual,
        ProfileConditionProperty::AudioChannels,
        &MAX_AUDIO_CHANNELS.to_string(),
    )];
    let subtitle_formats = [
        "ass", "ssa", "subrip", "srt", "pgs", "pgssub", "dvdsub", "dvbsub", "vobsub", "mov_text",
    ];
    DeviceProfile {
        name: "tv-client 1080p30 H264 AAC stereo".to_string(),
        max_streaming_bitrate: bitrate,
        max_static_bitrate: bitrate,
        direct_play_profiles: vec![DirectPlayProfile {
            container: DIRECT_PLAY_CONTAINER.to_string(),
            audio_codec: Some("aac".to_string()),
            video_codec: Some("h264".to_string()),
            profile_type: ProfileType::Video,
        }],
        transcoding_profiles: vec![TranscodingProfile {
            container: TRANSCODE_CONTAINER.to_string(),
            profile_type: ProfileType::Video,
            video_codec: "h264".to_string(),
            audio_codec: "aac".to_string(),
            protocol: "hls".to_string(),
            context: "Streaming".to_string(),
            max_audio_channels: MAX_AUDIO_CHANNELS.to_string(),
            min_segments: 1,
            break_on_non_key_frames: false,
            copy_timestamps: false,
            enable_subtitles_in_manifest: false,
        }],
        codec_profiles: vec![
            CodecProfile {
                profile_type: ProfileType::Video,
                codec: "h264".to_string(),
                conditions: video_conditions,
                apply_conditions: Vec::new(),
            },
            CodecProfile {
                profile_type: ProfileType::Audio,
                codec: "aac".to_string(),
                conditions: audio_conditions,
                apply_conditions: Vec::new(),
            },
        ],
        subtitle_profiles: subtitle_formats
            .into_iter()
            .map(|format| SubtitleProfile {
                format: format.to_string(),
                method: SubtitleDeliveryMethod::Encode,
            })
            .collect(),
    }
}

fn condition(
    condition: ProfileConditionType,
    property: ProfileConditionProperty,
    value: &str,
) -> ProfileCondition {
    ProfileCondition {
        condition,
        property,
        value: value.to_string(),
        is_required: true,
    }
}

/// Request body for `POST /Items/{itemId}/PlaybackInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoRequest {
    /// User to negotiate for.
    pub user_id: String,
    /// Maximum streamed bitrate.
    pub max_streaming_bitrate: u64,
    /// Resume position in 100 ns ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<u64>,
    /// Explicit audio stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    /// Explicit subtitle stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    /// Maximum audio channels.
    pub max_audio_channels: u32,
    /// Explicit media source identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    /// Device capabilities.
    pub device_profile: DeviceProfile,
    /// Allow direct play.
    pub enable_direct_play: bool,
    /// Allow direct stream / remux.
    pub enable_direct_stream: bool,
    /// Allow transcoding.
    pub enable_transcoding: bool,
    /// Allow the server to copy the video stream.
    pub allow_video_stream_copy: bool,
    /// Allow the server to copy the audio stream.
    pub allow_audio_stream_copy: bool,
    /// Never open a live stream automatically.
    pub auto_open_live_stream: bool,
    /// Burn in the selected subtitle when transcoding.
    pub always_burn_in_subtitle_when_transcoding: bool,
}

impl PlaybackInfoRequest {
    /// Build a validated request for `user_id`.
    pub fn new(user_id: &str, options: &PlaybackInfoOptions) -> Result<Self, JellyfinError> {
        options.validate()?;
        if user_id.is_empty() || user_id.len() > 128 {
            return Err(JellyfinError::InvalidRequest);
        }
        Ok(Self {
            user_id: user_id.to_string(),
            max_streaming_bitrate: options.max_streaming_bitrate,
            start_time_ticks: (options.start_time_ticks > 0).then_some(options.start_time_ticks),
            audio_stream_index: options.audio_stream_index,
            subtitle_stream_index: options.subtitle_stream_index,
            max_audio_channels: MAX_AUDIO_CHANNELS,
            media_source_id: options.media_source_id.clone(),
            device_profile: device_profile(options.max_streaming_bitrate),
            enable_direct_play: true,
            enable_direct_stream: true,
            enable_transcoding: true,
            allow_video_stream_copy: true,
            allow_audio_stream_copy: true,
            auto_open_live_stream: false,
            always_burn_in_subtitle_when_transcoding: options.burn_in_subtitles,
        })
    }
}

/// How a selected media source will be played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PlayMethod {
    /// Static file stream, no server processing.
    DirectPlay,
    /// Server remuxes without re-encoding.
    DirectStream,
    /// Server re-encodes to HLS H.264/AAC.
    Transcode,
}

impl PlayMethod {
    /// Stable Jellyfin PascalCase spelling used in session reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectPlay => "DirectPlay",
            Self::DirectStream => "DirectStream",
            Self::Transcode => "Transcode",
        }
    }

    /// Short UI label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DirectPlay => "Direct play",
            Self::DirectStream => "Remux",
            Self::Transcode => "Transcode",
        }
    }
}

/// Stream chosen for a media source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaStreamChoice {
    /// Build `/Videos/{itemId}/stream?static=true` below the base path.
    DirectPlay,
    /// Use the server-provided direct-stream URL.
    DirectStream(StreamUrl),
    /// Use the server-provided HLS URL.
    Transcode(StreamUrl),
}

/// One selected media source and stream.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaSelection {
    /// Media source identifier.
    pub source_id: String,
    /// Play session identifier, when supplied.
    pub play_session_id: Option<String>,
    /// Chosen play method.
    pub method: PlayMethod,
    /// Chosen stream.
    pub stream: MediaStreamChoice,
    /// Source duration in 100 ns ticks.
    pub duration_ticks: Option<u64>,
    /// Selected audio stream index.
    pub audio_stream_index: Option<i32>,
    /// Selected subtitle stream index.
    pub subtitle_stream_index: Option<i32>,
    /// Audio streams available in the selected source.
    pub audio_streams: Vec<MediaStreamInfo>,
    /// Subtitle streams available in the selected source.
    pub subtitle_streams: Vec<MediaStreamInfo>,
}

/// Selection options derived from the caller's intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionOptions {
    /// Explicit audio stream index.
    pub audio_stream_index: Option<i32>,
    /// Explicit subtitle stream index. Any selected subtitle forces burn-in.
    pub subtitle_stream_index: Option<i32>,
    /// Allow direct play.
    pub allow_direct_play: bool,
    /// Allow direct stream / remux.
    pub allow_direct_stream: bool,
    /// Allow transcoding.
    pub allow_transcoding: bool,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            audio_stream_index: None,
            subtitle_stream_index: None,
            allow_direct_play: true,
            allow_direct_stream: true,
            allow_transcoding: true,
        }
    }
}

/// Reasons no source could be selected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SelectionError {
    /// The server rejected the item outright.
    #[error("jellyfin reported {code}")]
    ServerCode {
        /// Error code returned by Jellyfin.
        code: String,
    },
    /// No source matched the proven device profile.
    #[error("no compatible media source")]
    NoCompatibleSource,
}

/// Choose a compatible media source.
///
/// Sources are scored direct play (3), remux (2), transcode (1); the first
/// source with the highest usable score wins, preserving server order among
/// equals. HEVC, AV1, VP9, HDR, 10-bit, interlaced, over-1080p or over-30 fps
/// video never direct-plays. A selected subtitle always forces a transcode so
/// the server can burn it in.
pub fn select_media_source(
    response: &PlaybackInfoResponse,
    options: &SelectionOptions,
) -> Result<MediaSelection, SelectionError> {
    if let Some(code) = &response.error_code
        && !code.is_empty()
    {
        return Err(SelectionError::ServerCode { code: code.clone() });
    }

    let mut best: Option<(u8, MediaSelection)> = None;
    for source in &response.media_sources {
        let Some((score, selection)) = evaluate_source(source, response, options) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, selection));
        }
    }
    best.map(|(_, selection)| selection)
        .ok_or(SelectionError::NoCompatibleSource)
}

fn evaluate_source(
    source: &MediaSourceInfo,
    response: &PlaybackInfoResponse,
    options: &SelectionOptions,
) -> Option<(u8, MediaSelection)> {
    let video = video_stream(source)?;
    let video_ok = proven_video(video);
    let audio_index = options
        .audio_stream_index
        .or(source.default_audio_stream_index);
    let audio_ok = audio_stream(source, audio_index).is_none_or(proven_audio);
    let subtitle = options.subtitle_stream_index;
    let container_ok = proven_container(source.container.as_deref());
    let direct_base =
        video_ok && audio_ok && subtitle.is_none() && source.supports_direct_play && container_ok;

    if options.allow_direct_play && direct_base {
        return Some((
            3,
            selection_for(
                source,
                response,
                PlayMethod::DirectPlay,
                MediaStreamChoice::DirectPlay,
                audio_index,
                subtitle,
            ),
        ));
    }

    let remux_base = video_ok && audio_ok && subtitle.is_none() && container_ok;
    if options.allow_direct_stream
        && remux_base
        && source.supports_direct_stream
        && let Some(url) = &source.direct_stream_url
    {
        return Some((
            2,
            selection_for(
                source,
                response,
                PlayMethod::DirectStream,
                MediaStreamChoice::DirectStream(url.clone()),
                audio_index,
                subtitle,
            ),
        ));
    }

    if options.allow_transcoding
        && source.supports_transcoding
        && let Some(url) = &source.transcoding_url
        && is_hls(source)
    {
        return Some((
            1,
            selection_for(
                source,
                response,
                PlayMethod::Transcode,
                MediaStreamChoice::Transcode(url.clone()),
                audio_index,
                subtitle,
            ),
        ));
    }
    None
}

fn selection_for(
    source: &MediaSourceInfo,
    response: &PlaybackInfoResponse,
    method: PlayMethod,
    stream: MediaStreamChoice,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
) -> MediaSelection {
    MediaSelection {
        source_id: source.id.clone().unwrap_or_default(),
        play_session_id: response.play_session_id.clone(),
        method,
        stream,
        duration_ticks: source.run_time_ticks,
        audio_stream_index,
        subtitle_stream_index,
        audio_streams: source
            .media_streams
            .iter()
            .filter(|stream| is_kind(stream, "audio"))
            .cloned()
            .collect(),
        subtitle_streams: source
            .media_streams
            .iter()
            .filter(|stream| is_kind(stream, "subtitle"))
            .cloned()
            .collect(),
    }
}

fn video_stream(source: &MediaSourceInfo) -> Option<&MediaStreamInfo> {
    source
        .media_streams
        .iter()
        .find(|stream| is_kind(stream, "video"))
}

fn audio_stream(source: &MediaSourceInfo, requested: Option<i32>) -> Option<&MediaStreamInfo> {
    let requested = requested.or(source.default_audio_stream_index);
    source.media_streams.iter().find(|stream| {
        is_kind(stream, "audio") && requested.is_none_or(|index| stream.index == Some(index))
    })
}

fn is_kind(stream: &MediaStreamInfo, expected: &str) -> bool {
    stream
        .stream_type
        .as_deref()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn proven_container(container: Option<&str>) -> bool {
    container.is_some_and(|value| {
        value
            .split(',')
            .map(str::trim)
            .any(|token| token.eq_ignore_ascii_case(DIRECT_PLAY_CONTAINER))
    })
}

fn proven_video(stream: &MediaStreamInfo) -> bool {
    let codec = stream
        .codec
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(codec.as_str(), "h264" | "avc" | "avc1") {
        return false;
    }
    if stream.width.is_some_and(|width| width > MAX_VIDEO_WIDTH)
        || stream
            .height
            .is_some_and(|height| height > MAX_VIDEO_HEIGHT)
        || stream.bit_depth.is_some_and(|depth| depth > 8)
        || stream.level.is_some_and(|level| level > MAX_H264_LEVEL)
        || stream.is_interlaced
    {
        return false;
    }
    if let Some(range) = stream
        .video_range_type
        .as_deref()
        .or(stream.video_range.as_deref())
        && !range.eq_ignore_ascii_case("sdr")
        && !range.is_empty()
    {
        return false;
    }
    let frame_rate = stream.average_frame_rate.or(stream.real_frame_rate);
    !frame_rate.is_some_and(|rate| rate > f64::from(MAX_FRAME_RATE) + 0.01)
}

fn proven_audio(stream: &MediaStreamInfo) -> bool {
    let codec = stream.codec.as_deref().unwrap_or_default();
    codec.eq_ignore_ascii_case("aac")
        && stream
            .channels
            .is_none_or(|channels| channels <= MAX_AUDIO_CHANNELS)
}

fn is_hls(source: &MediaSourceInfo) -> bool {
    source
        .transcoding_sub_protocol
        .as_deref()
        .is_none_or(|value| value.eq_ignore_ascii_case("hls"))
}

/// One `/Sessions/Playing` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStartInfo {
    /// Item being played.
    pub item_id: String,
    /// Selected media source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    /// Negotiated play session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    /// Start position in 100 ns ticks.
    pub position_ticks: u64,
    /// Whether playback is paused.
    pub is_paused: bool,
    /// Whether audio is muted.
    pub is_muted: bool,
    /// Negotiated play method.
    pub play_method: PlayMethod,
    /// Selected audio stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    /// Selected subtitle stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    /// Whether the player can seek.
    pub can_seek: bool,
    /// Linear volume, 0 through 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_level: Option<u8>,
}

/// One `/Sessions/Playing/Progress` report.
pub type PlaybackProgressInfo = PlaybackStartInfo;

/// One `/Sessions/Playing/Stopped` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStopInfo {
    /// Item that was played.
    pub item_id: String,
    /// Selected media source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    /// Negotiated play session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    /// Final position in 100 ns ticks.
    pub position_ticks: u64,
    /// Whether playback failed.
    pub failed: bool,
}

impl JellyfinClient {
    /// Negotiate playback with the default six-megabit profile.
    ///
    /// This compatibility entry point returns the typed response without
    /// resolving stream URLs. Prefer [`Self::playback_info_with`], which also
    /// enforces the stream-origin allowlist.
    pub async fn playback_info(
        &self,
        item_id: &str,
        cancel: &CancelToken,
    ) -> Result<PlaybackInfoResponse, JellyfinError> {
        self.playback_info_inner(item_id, &PlaybackInfoOptions::default(), false, cancel)
            .await
    }

    /// Negotiate playback with explicit resume, stream and bitrate options.
    ///
    /// The response is rejected if any stream URL escapes the configured
    /// origin or base path.
    pub async fn playback_info_with(
        &self,
        item_id: &str,
        options: &PlaybackInfoOptions,
        cancel: &CancelToken,
    ) -> Result<PlaybackInfoResponse, JellyfinError> {
        self.playback_info_inner(item_id, options, true, cancel)
            .await
    }

    async fn playback_info_inner(
        &self,
        item_id: &str,
        options: &PlaybackInfoOptions,
        enforce_origin: bool,
        cancel: &CancelToken,
    ) -> Result<PlaybackInfoResponse, JellyfinError> {
        if !crate::model::is_valid_item_id(item_id) {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        let request = PlaybackInfoRequest::new(user, options)?;
        let mut url = self.endpoint(&["Items", item_id, "PlaybackInfo"])?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("UserId", user);
            if let Some(source) = &options.media_source_id {
                query.append_pair("MediaSourceId", source);
            }
            if options.start_time_ticks > 0 {
                query.append_pair("StartTimeTicks", &options.start_time_ticks.to_string());
            }
            if let Some(index) = options.audio_stream_index {
                query.append_pair("AudioStreamIndex", &index.to_string());
            }
            if let Some(index) = options.subtitle_stream_index {
                query.append_pair("SubtitleStreamIndex", &index.to_string());
            }
            query.append_pair(
                "MaxStreamingBitrate",
                &options.max_streaming_bitrate.to_string(),
            );
        }
        let response: PlaybackInfoResponse = self.post_json(url, &request, cancel).await?;
        if enforce_origin {
            response.restrict_stream_origins(self.base_url())?;
        }
        Ok(response)
    }

    /// Report that playback started.
    pub async fn report_playback_start(
        &self,
        info: &PlaybackStartInfo,
        cancel: &CancelToken,
    ) -> Result<(), JellyfinError> {
        let url = self.endpoint(&["Sessions", "Playing"])?;
        self.post_json_empty(url, info, cancel).await
    }

    /// Report playback progress, or a pause/seek transition.
    pub async fn report_playback_progress(
        &self,
        info: &PlaybackProgressInfo,
        cancel: &CancelToken,
    ) -> Result<(), JellyfinError> {
        let url = self.endpoint(&["Sessions", "Playing", "Progress"])?;
        self.post_json_empty(url, info, cancel).await
    }

    /// Report that playback stopped.
    pub async fn report_playback_stopped(
        &self,
        info: &PlaybackStopInfo,
        cancel: &CancelToken,
    ) -> Result<(), JellyfinError> {
        let url = self.endpoint(&["Sessions", "Playing", "Stopped"])?;
        self.post_json_empty(url, info, cancel).await
    }

    /// Best-effort release of an active transcode.
    ///
    /// Jellyfin 12.4 does not list `/Videos/ActiveEncodings` in its OpenAPI
    /// document, so a 404/405 is treated as "not supported" rather than an
    /// error. `/Sessions/Playing/Stopped` remains the primary release path.
    pub async fn release_transcode(
        &self,
        play_session_id: &str,
        cancel: &CancelToken,
    ) -> Result<bool, JellyfinError> {
        if play_session_id.is_empty() || play_session_id.len() > 256 {
            return Err(JellyfinError::InvalidRequest);
        }
        let mut url = self.endpoint(&["Videos", "ActiveEncodings"])?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("deviceId", self.device_id());
            query.append_pair("playSessionId", play_session_id);
        }
        match self.delete_no_content(url, cancel).await {
            Ok(()) => Ok(true),
            Err(JellyfinError::Http { status: 404 | 405 }) => Ok(false),
            Err(error) => Err(error),
        }
    }
}
