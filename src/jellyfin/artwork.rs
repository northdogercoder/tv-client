//! Artwork download and bounded cache integration.

use std::path::PathBuf;

use reqwest::Method;

use crate::jellyfin::JellyfinClient;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::client::read_body_limited;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::image::{MAX_IMAGE_BYTES, image_dimensions};
use crate::jellyfin::models::ImageRequest;
use crate::storage::{Storage, StorageError};

impl JellyfinClient {
    /// Fetch one artwork image and return its private cached path.
    ///
    /// A cache hit is keyed by server origin, user, item, image kind and the
    /// server's image tag, so replacing a poster on the server invalidates the
    /// old entry. The payload is size-capped, its format header is parsed, and
    /// excessive dimensions are rejected before anything is written to disk.
    pub async fn image(
        &mut self,
        storage: &mut Storage,
        request: &ImageRequest,
        cancel: &CancelToken,
    ) -> Result<PathBuf, JellyfinError> {
        let key = self.artwork_cache_key(request.item_id(), request.image_type(), request.tag())?;
        if let Some(path) = storage.artwork_path(&key).map_err(JellyfinError::Artwork)? {
            return Ok(path);
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
            pairs.append_pair("quality", "90");
        }

        let response = self.send_raw(Method::GET, url, None, cancel).await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
        {
            return Err(JellyfinError::ArtworkTooLarge);
        }
        let body = read_body_limited(response, MAX_IMAGE_BYTES, cancel)
            .await
            .map_err(|error| match error {
                JellyfinError::ResponseTooLarge => JellyfinError::ArtworkTooLarge,
                other => other,
            })?;
        image_dimensions(&body)?;
        storage
            .store_artwork(&key, &body)
            .map_err(|error| match error {
                StorageError::ArtworkTooLarge => JellyfinError::ArtworkTooLarge,
                other => JellyfinError::Artwork(other),
            })?;
        self.remember_artwork_key(key.clone());
        storage
            .artwork_path(&key)
            .map_err(JellyfinError::Artwork)?
            .ok_or(JellyfinError::Artwork(StorageError::InvalidArtwork))
    }
}
