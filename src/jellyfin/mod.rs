//! Direct, local-first Jellyfin integration.
//!
//! The client speaks only to the configured Jellyfin origin, uses the
//! `MediaBrowser` authorization header, refuses HTTP redirects and keeps the
//! device identity and access token in [`crate::storage::Storage`]. Browse
//! methods return typed metadata and item identifiers; stream URLs are never
//! modeled. See `docs/jellyfin.md` for endpoint and error details.
//!
//! ```no_run
//! use tv_client::config::ClientConfig;
//! use tv_client::jellyfin::{JellyfinClient, cancellation};
//! use tv_client::storage::Storage;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = ClientConfig {
//!     jellyfin_url: Some("https://jellyfin.example".to_owned()),
//!     ..ClientConfig::default()
//! };
//! let mut storage = Storage::open(std::path::Path::new("/tmp/tv-client-state"), config.artwork_cache_bytes)?;
//! let mut client = JellyfinClient::new(&config, "tv-client", &mut storage)?;
//! let (cancel_handle, cancel) = cancellation();
//! if client.restore_session(&mut storage, &cancel).await? {
//!     let views = client.views(&cancel).await?;
//!     println!("{} views", views.total_record_count);
//! }
//! cancel_handle.cancel();
//! # Ok(())
//! # }
//! ```

mod artwork;
mod auth;
mod cancel;
mod client;
mod error;
mod image;
mod library;
mod models;
mod playback;

pub use artwork::CachedArtwork;
pub use auth::{QuickConnectInitiation, QuickConnectPoll};
pub use cancel::{CancelHandle, CancelToken, cancellation};
pub use client::{
    ACCESS_TOKEN_CREDENTIAL, CLIENT_NAME, DEFAULT_DEVICE_NAME, DEVICE_ID_PREFERENCE,
    JellyfinClient, QUICK_CONNECT_TTL, Session, Timeouts,
};
pub use error::JellyfinError;
pub use image::{MAX_IMAGE_BYTES, MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS};
pub use library::{
    BROWSE_INCLUDE_ITEM_TYPES, BROWSE_PAGE_SIZE, BrowseItem, ItemQuery, LibraryEntry,
    MAX_LIBRARIES, PreviewKind, browse_types,
};
pub use models::{
    ArtworkClass, AuthenticationResult, BaseItem, DEFAULT_IMAGE_HEIGHT, DEFAULT_IMAGE_WIDTH,
    ImageRequest, ImageType, ItemFilter, MAX_PAGE_SIZE, MAX_REQUESTED_IMAGE_DIMENSION,
    MAX_SEARCH_TERM_BYTES, MediaSourceInfo, MediaStreamInfo, PAGE_SIZE, Page, PlaybackInfoResponse,
    QueryResult, SortOrder, StreamUrl, UserDataUpdate, UserDto, UserItemData,
};
pub use playback::{
    CodecProfile, DIRECT_PLAY_CONTAINER, DeviceProfile, DirectPlayProfile, MAX_AUDIO_CHANNELS,
    MAX_FRAME_RATE, MAX_H264_LEVEL, MAX_PLAYBACK_START_TICKS, MAX_STREAMING_BITRATE,
    MAX_VIDEO_HEIGHT, MAX_VIDEO_WIDTH, MediaSelection, MediaStreamChoice, PlayMethod,
    PlaybackInfoOptions, PlaybackInfoRequest, PlaybackProgressInfo, PlaybackStartInfo,
    PlaybackStopInfo, ProfileCondition, ProfileConditionProperty, ProfileConditionType,
    ProfileType, START_STREAMING_BITRATE, SelectionError, SelectionOptions, SubtitleDeliveryMethod,
    SubtitleProfile, TICKS_PER_MILLISECOND, TRANSCODE_CONTAINER, TranscodingProfile,
    device_profile, select_media_source,
};
