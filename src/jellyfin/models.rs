//! Strictly typed Jellyfin wire models.
//!
//! The models intentionally omit playback URLs, file paths and transcoding
//! URLs. Browse state is expressed as opaque item IDs; media resolution happens
//! through the typed playback-info response, which exposes identifiers and
//! capabilities but never a stream URL.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// Default page size for item listings.
pub const PAGE_SIZE: u32 = 50;
/// Largest page a caller may request.
pub const MAX_PAGE_SIZE: u32 = 200;
/// Largest accepted search term, in bytes.
pub const MAX_SEARCH_TERM_BYTES: usize = 256;

/// One page request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    start_index: u32,
    limit: u32,
}

impl Page {
    /// First page with [`PAGE_SIZE`] entries.
    #[must_use]
    pub const fn first() -> Self {
        Self {
            start_index: 0,
            limit: PAGE_SIZE,
        }
    }

    /// Build a page, rejecting a zero or oversized limit.
    pub fn new(start_index: u32, limit: u32) -> Result<Self, crate::jellyfin::JellyfinError> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(crate::jellyfin::JellyfinError::InvalidRequest);
        }
        Ok(Self { start_index, limit })
    }

    /// Zero-based first item index.
    #[must_use]
    pub const fn start_index(self) -> u32 {
        self.start_index
    }

    /// Maximum items returned.
    #[must_use]
    pub const fn limit(self) -> u32 {
        self.limit
    }

    /// The following page, saturating at the end of the index range.
    #[must_use]
    pub const fn next(self) -> Self {
        Self {
            start_index: self.start_index.saturating_add(self.limit),
            limit: self.limit,
        }
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::first()
    }
}

/// Sort direction accepted by Jellyfin item queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortOrder {
    /// Smallest/first first.
    #[default]
    Ascending,
    /// Largest/last first.
    Descending,
}

impl SortOrder {
    /// Wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ascending => "Ascending",
            Self::Descending => "Descending",
        }
    }
}

/// Common Jellyfin item filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemFilter {
    /// Items with resumable progress.
    Resumable,
    /// Items marked played.
    IsPlayed,
    /// Items not marked played.
    IsUnplayed,
    /// Items marked favourite.
    IsFavorite,
}

impl ItemFilter {
    /// Wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resumable => "IsResumable",
            Self::IsPlayed => "IsPlayed",
            Self::IsUnplayed => "IsUnplayed",
            Self::IsFavorite => "IsFavorite",
        }
    }
}

/// Artwork kinds accepted by the image endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    /// Primary poster.
    Primary,
    /// Wide backdrop.
    Backdrop,
    /// Series or channel logo.
    Logo,
    /// Episode or chapter thumbnail.
    Thumb,
    /// Banner.
    Banner,
    /// Generic art.
    Art,
    /// Disc image.
    Disc,
    /// Box image.
    Box,
    /// Box rear image.
    BoxRear,
    /// Screenshot.
    Screenshot,
    /// Menu image.
    Menu,
    /// Chapter image.
    Chapter,
    /// Profile image.
    Profile,
}

impl ImageType {
    /// Wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Backdrop => "Backdrop",
            Self::Logo => "Logo",
            Self::Thumb => "Thumb",
            Self::Banner => "Banner",
            Self::Art => "Art",
            Self::Disc => "Disc",
            Self::Box => "Box",
            Self::BoxRear => "BoxRear",
            Self::Screenshot => "Screenshot",
            Self::Menu => "Menu",
            Self::Chapter => "Chapter",
            Self::Profile => "Profile",
        }
    }
}

impl fmt::Display for ImageType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Authenticated user returned by Jellyfin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserDto {
    /// Opaque user identifier.
    pub id: String,
    /// Display name. Never persisted or logged by this client.
    pub name: Option<String>,
    /// Optional server identifier.
    pub server_id: Option<String>,
}

/// Result of a successful authentication call.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticationResult {
    /// Authenticated user.
    #[serde(default)]
    pub user: UserDto,
    /// Bearer token. Never formatted by `Debug`.
    #[serde(default)]
    pub access_token: String,
    /// Server identifier, when supplied.
    #[serde(default)]
    pub server_id: Option<String>,
}

impl fmt::Debug for AuthenticationResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticationResult")
            .field("user", &self.user)
            .field("access_token", &"<redacted>")
            .field("server_id", &self.server_id)
            .finish()
    }
}

/// A page of Jellyfin items.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase", bound(deserialize = "T: Deserialize<'de>"))]
pub struct QueryResult<T> {
    /// Items on this page.
    #[serde(default, deserialize_with = "null_to_default")]
    pub items: Vec<T>,
    /// Total items matching the query across all pages.
    #[serde(default)]
    pub total_record_count: u64,
    /// Echoed zero-based start index.
    #[serde(default)]
    pub start_index: u32,
}

/// One browse item.
///
/// Only identifiers and display metadata are represented. `Path`, stream URLs
/// and transcoding URLs are deliberately absent from the model, so they can
/// never enter the browse state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct BaseItem {
    /// Opaque item identifier.
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Jellyfin item type, for example `Movie` or `Episode`.
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    /// Collection type for view folders.
    pub collection_type: Option<String>,
    /// Parent identifier.
    pub parent_id: Option<String>,
    /// Series identifier for seasons and episodes.
    pub series_id: Option<String>,
    /// Season identifier for episodes.
    pub season_id: Option<String>,
    /// Album identifier for audio items.
    pub album_id: Option<String>,
    /// Album artist.
    pub album_artist: Option<String>,
    /// Track artists.
    #[serde(default, deserialize_with = "null_to_default")]
    pub artists: Vec<String>,
    /// Zero-based index within the parent.
    pub index_number: Option<u32>,
    /// Zero-based season index for episodes.
    pub parent_index_number: Option<u32>,
    /// Production year.
    pub production_year: Option<u32>,
    /// Duration in Jellyfin's 100 ns ticks.
    pub run_time_ticks: Option<u64>,
    /// Whether the item is a folder.
    pub is_folder: bool,
    /// Long description.
    pub overview: Option<String>,
    /// Image tags keyed by [`ImageType::as_str`].
    #[serde(default, deserialize_with = "null_to_default")]
    pub image_tags: BTreeMap<String, String>,
    /// Per-user resume and played state.
    pub user_data: Option<UserItemData>,
}

/// Per-user playback metadata.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserItemData {
    /// Whether the item is marked played.
    pub played: bool,
    /// Number of completed plays.
    pub play_count: u32,
    /// Whether the user marked it favourite.
    pub is_favorite: bool,
    /// Resume position in 100 ns ticks.
    pub playback_position_ticks: u64,
    /// Played percentage reported by the server.
    pub played_percentage: f64,
}

/// Mutable user-data fields accepted by the update endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDataUpdate {
    /// Mark played or unplayed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played: Option<bool>,
    /// New resume position in 100 ns ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_position_ticks: Option<u64>,
    /// Favourite flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,
}

/// Playback negotiation response.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlaybackInfoResponse {
    /// Opaque play-session identifier.
    pub play_session_id: Option<String>,
    /// Media sources. URLs inside the upstream response are not modeled.
    #[serde(default, deserialize_with = "null_to_default")]
    pub media_sources: Vec<MediaSourceInfo>,
}

/// One playable media source, without URLs or file paths.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSourceInfo {
    /// Opaque source identifier.
    pub id: Option<String>,
    /// Display name.
    pub name: Option<String>,
    /// Container, for example `mkv`.
    pub container: Option<String>,
    /// Encoded size in bytes.
    pub size: Option<u64>,
    /// Nominal bitrate in bits per second.
    pub bitrate: Option<u64>,
    /// Direct-play capability.
    pub supports_direct_play: bool,
    /// Direct-stream capability.
    pub supports_direct_stream: bool,
    /// Transcoding capability.
    pub supports_transcoding: bool,
    /// Stream metadata.
    #[serde(default, deserialize_with = "null_to_default")]
    pub media_streams: Vec<MediaStreamInfo>,
}

/// One audio, video or subtitle stream descriptor.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStreamInfo {
    /// Stream kind, for example `Video` or `Audio`.
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    /// Codec name.
    pub codec: Option<String>,
    /// Language tag.
    pub language: Option<String>,
    /// Video width in pixels.
    pub width: Option<u32>,
    /// Video height in pixels.
    pub height: Option<u32>,
    /// Bitrate in bits per second.
    pub bit_rate: Option<u64>,
    /// Audio channel count.
    pub channels: Option<u32>,
    /// Audio sample rate.
    pub sample_rate: Option<u32>,
    /// Whether the stream is external to the container.
    pub is_external: bool,
}

/// A request for one cached artwork image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    item_id: String,
    image_type: ImageType,
    tag: Option<String>,
    max_width: u32,
    max_height: u32,
}

/// Default requested artwork width.
pub const DEFAULT_IMAGE_WIDTH: u32 = 1280;
/// Default requested artwork height.
pub const DEFAULT_IMAGE_HEIGHT: u32 = 720;
/// Largest dimension the server may be asked to produce.
pub const MAX_REQUESTED_IMAGE_DIMENSION: u32 = 8192;

impl ImageRequest {
    /// Request the primary image with the default bounds.
    pub fn new(
        item_id: &str,
        image_type: ImageType,
    ) -> Result<Self, crate::jellyfin::JellyfinError> {
        Self::with_tag(item_id, image_type, None)
    }

    /// Request an image variant identified by the server's tag.
    pub fn with_tag(
        item_id: &str,
        image_type: ImageType,
        tag: Option<&str>,
    ) -> Result<Self, crate::jellyfin::JellyfinError> {
        if !crate::model::is_valid_item_id(item_id) {
            return Err(crate::jellyfin::JellyfinError::InvalidItemId);
        }
        if let Some(tag) = tag
            && (tag.is_empty() || tag.len() > 128 || tag.chars().any(char::is_control))
        {
            return Err(crate::jellyfin::JellyfinError::InvalidRequest);
        }
        Ok(Self {
            item_id: item_id.to_owned(),
            image_type,
            tag: tag.map(str::to_owned),
            max_width: DEFAULT_IMAGE_WIDTH,
            max_height: DEFAULT_IMAGE_HEIGHT,
        })
    }

    /// Set the maximum produced dimensions.
    pub fn with_max_size(
        mut self,
        width: u32,
        height: u32,
    ) -> Result<Self, crate::jellyfin::JellyfinError> {
        if width == 0
            || height == 0
            || width > MAX_REQUESTED_IMAGE_DIMENSION
            || height > MAX_REQUESTED_IMAGE_DIMENSION
        {
            return Err(crate::jellyfin::JellyfinError::InvalidRequest);
        }
        self.max_width = width;
        self.max_height = height;
        Ok(self)
    }

    /// Item identifier.
    #[must_use]
    pub fn item_id(&self) -> &str {
        self.item_id.as_str()
    }

    /// Requested image kind.
    #[must_use]
    pub const fn image_type(&self) -> ImageType {
        self.image_type
    }

    /// Server image tag, when known.
    #[must_use]
    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// Maximum requested width.
    #[must_use]
    pub const fn max_width(&self) -> u32 {
        self.max_width
    }

    /// Maximum requested height.
    #[must_use]
    pub const fn max_height(&self) -> u32 {
        self.max_height
    }
}

/// Treat an explicit JSON `null` like an absent collection.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
