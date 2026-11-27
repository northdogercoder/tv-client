//! Artwork download and bounded cache integration.

use std::fs;
use std::io::Read;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use reqwest::Method;

use crate::jellyfin::JellyfinClient;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::client::read_body_limited;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::image::validate_artwork;
use crate::jellyfin::models::{ArtworkClass, ImageRequest};
use crate::storage::{Storage, StorageError};

/// A cached artwork file plus the metadata validated when it was stored.
///
/// The path is produced only by [`Storage`]; the client never exposes a
/// server-supplied URL or path. [`Deref`] and [`AsRef`] make it usable wherever
/// a local path is expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedArtwork {
    path: PathBuf,
    width: u32,
    height: u32,
    bytes: usize,
}

impl CachedArtwork {
    /// Local cache path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Validated decoded width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Validated decoded height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Encoded payload size in bytes.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Consume the value and return its local path.
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

impl Deref for CachedArtwork {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path.as_path()
    }
}

impl AsRef<Path> for CachedArtwork {
    fn as_ref(&self) -> &Path {
        self.path.as_path()
    }
}

impl JellyfinClient {
    /// Fetch one artwork image and return its private cached path and metadata.
    ///
    /// A cache hit is keyed by server origin, user, item, image kind, server
    /// tag and [`ArtworkClass`], so a poster and a preview of the same item never
    /// collide, and replacing a poster on the server invalidates the old entry.
    /// The payload is capped per class, its format header is parsed, and
    /// excessive dimensions are rejected before anything is written to disk.
    ///
    /// A missing image (`HTTP 404`) is reported as
    /// [`JellyfinError::ArtworkMissing`]; callers treat that as a normal
    /// placeholder, not a browse failure.
    pub async fn image(
        &mut self,
        storage: &mut Storage,
        request: &ImageRequest,
        cancel: &CancelToken,
    ) -> Result<CachedArtwork, JellyfinError> {
        let class = request.class();
        let key = self.artwork_cache_key(
            request.item_id(),
            request.image_type(),
            request.tag(),
            class,
            request.max_width(),
            request.max_height(),
        )?;
        if let Some(path) = storage.artwork_path(&key).map_err(JellyfinError::Artwork)? {
            if let Some(cached) = read_cached_artwork(path, class) {
                return Ok(cached);
            }
            // A corrupt, tampered or outdated payload is treated as a miss and
            // replaced in place under the same key.
        }

        let mut url = self.endpoint(&[
            "Items",
            request.item_id(),
            "Images",
            request.image_type().as_str(),
        ])?;
        {
            let mut pairs = url.query_pairs_mut();
            if let Some(tag) = request.tag() {
                pairs.append_pair("tag", tag);
            }
            pairs.append_pair("maxWidth", &request.max_width().to_string());
            pairs.append_pair("maxHeight", &request.max_height().to_string());
            pairs.append_pair("format", request.format());
            pairs.append_pair("quality", "90");
        }

        let response = match self.send_raw(Method::GET, url, None, cancel).await {
            Ok(response) => response,
            Err(JellyfinError::Http { status: 404 }) => {
                return Err(JellyfinError::ArtworkMissing);
            }
            Err(error) => return Err(error),
        };
        let limit = class.max_bytes();
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(JellyfinError::ArtworkTooLarge);
        }
        let body = read_body_limited(response, limit, cancel)
            .await
            .map_err(|error| match error {
                JellyfinError::ResponseTooLarge => JellyfinError::ArtworkTooLarge,
                other => other,
            })?;
        let (width, height) = validate_artwork(&body, class)?;
        storage
            .store_artwork(&key, &body)
            .map_err(|error| match error {
                StorageError::ArtworkTooLarge => JellyfinError::ArtworkTooLarge,
                other => JellyfinError::Artwork(other),
            })?;
        self.remember_artwork_key(key.clone());
        let path = storage
            .artwork_path(&key)
            .map_err(JellyfinError::Artwork)?
            .ok_or(JellyfinError::Artwork(StorageError::InvalidArtwork))?;
        Ok(CachedArtwork {
            path,
            width,
            height,
            bytes: body.len(),
        })
    }
}

/// Re-validate a cached payload before handing it back to the UI.
///
/// The read is bounded by the class's byte cap. A missing, truncated,
/// oversized or out-of-class file yields `None`, so the caller downloads a
/// fresh copy.
fn read_cached_artwork(path: PathBuf, class: ArtworkClass) -> Option<CachedArtwork> {
    let limit = class.max_bytes();
    let metadata = fs::metadata(&path).ok()?;
    if metadata.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(&path)
        .ok()?
        .take(u64::try_from(limit).ok()?.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    let (width, height) = validate_artwork(&bytes, class).ok()?;
    Some(CachedArtwork {
        path,
        width,
        height,
        bytes: bytes.len(),
    })
}
