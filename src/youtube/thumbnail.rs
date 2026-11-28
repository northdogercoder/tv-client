//! Bounded fetch and validation for official YouTube thumbnails.
//!
//! The server validates the URL once; this module re-validates it and treats
//! the response as untrusted image data. Only `i.ytimg.com` HTTPS URLs are
//! fetched, redirects are refused, and encoded/decoded bounds are enforced.
//! The URL is never logged or stored; callers cache the bytes by video id.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::redirect::Policy;
use thiserror::Error;
use url::Url;

use crate::jellyfin::CancelToken;
use crate::jellyfin::image::image_dimensions;

/// Largest encoded thumbnail payload accepted.
pub const MAX_THUMBNAIL_BYTES: usize = 2 * 1024 * 1024;
/// Largest accepted thumbnail width.
pub const MAX_THUMBNAIL_WIDTH: u32 = 1920;
/// Largest accepted thumbnail height.
pub const MAX_THUMBNAIL_HEIGHT: u32 = 1080;
/// Fixed network budget for one thumbnail.
pub const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(8);

/// Thumbnail URL or payload was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ThumbnailError {
    /// Not an official HTTPS i.ytimg.com URL.
    #[error("thumbnail url is not an official YouTube image")]
    InvalidUrl,
    /// The network request failed or timed out.
    #[error("thumbnail request failed")]
    Request,
    /// The response was not a bounded image.
    #[error("thumbnail response was rejected")]
    InvalidImage,
    /// The request was cancelled.
    #[error("thumbnail request was cancelled")]
    Cancelled,
}

/// Validate the only thumbnail hosts the client accepts.
pub fn validate_thumbnail_url(raw: &str) -> Result<Url, ThumbnailError> {
    let url = Url::parse(raw).map_err(|_| ThumbnailError::InvalidUrl)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url
            .host_str()
            .is_none_or(|host| host != "i.ytimg.com" && !host.ends_with(".ytimg.com"))
    {
        return Err(ThumbnailError::InvalidUrl);
    }
    Ok(url)
}

/// One reusable bounded HTTP client.
#[derive(Debug, Clone)]
pub struct ThumbnailFetcher {
    client: reqwest::Client,
}

impl ThumbnailFetcher {
    /// Build a client with redirects disabled and a fixed timeout.
    pub fn new() -> Result<Self, ThumbnailError> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(THUMBNAIL_TIMEOUT)
            .build()
            .map_err(|_| ThumbnailError::Request)?;
        Ok(Self { client })
    }

    /// Fetch and validate one official thumbnail.
    pub async fn fetch(&self, url: &Url, cancel: &CancelToken) -> Result<Vec<u8>, ThumbnailError> {
        if url.scheme() != "https"
            || url.username() != ""
            || url.password().is_some()
            || url
                .host_str()
                .is_none_or(|host| host != "i.ytimg.com" && !host.ends_with(".ytimg.com"))
        {
            return Err(ThumbnailError::InvalidUrl);
        }
        if cancel.is_cancelled() {
            return Err(ThumbnailError::Cancelled);
        }
        let mut cancel = cancel.clone();
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ThumbnailError::Cancelled),
            result = self.client.get(url.clone()).send() => {
                result.map_err(|_| ThumbnailError::Request)?
            }
        };
        if !response.status().is_success() {
            return Err(ThumbnailError::Request);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_THUMBNAIL_BYTES as u64)
        {
            return Err(ThumbnailError::InvalidImage);
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| !value.starts_with("image/"))
        {
            return Err(ThumbnailError::InvalidImage);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ThumbnailError::Cancelled),
                next = stream.next() => next,
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|_| ThumbnailError::Request)?;
            if body.len().saturating_add(chunk.len()) > MAX_THUMBNAIL_BYTES {
                return Err(ThumbnailError::InvalidImage);
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(ThumbnailError::InvalidImage);
        }
        let (width, height) = image_dimensions(&body).map_err(|_| ThumbnailError::InvalidImage)?;
        if width == 0
            || height == 0
            || width > MAX_THUMBNAIL_WIDTH
            || height > MAX_THUMBNAIL_HEIGHT
            || u64::from(width) * u64::from(height) > 2_073_600
        {
            return Err(ThumbnailError::InvalidImage);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_official_https_thumbnails_are_accepted() {
        assert!(validate_thumbnail_url("https://i.ytimg.com/vi/x/hqdefault.jpg").is_ok());
        assert!(validate_thumbnail_url("https://foo.ytimg.com/vi/x/hqdefault.jpg").is_ok());
        assert!(validate_thumbnail_url("http://i.ytimg.com/vi/x/hqdefault.jpg").is_err());
        assert!(validate_thumbnail_url("https://evil.example/i.jpg").is_err());
        assert!(validate_thumbnail_url("https://i.ytimg.com.evil.example/i.jpg").is_err());
        assert!(validate_thumbnail_url("https://user@i.ytimg.com/i.jpg").is_err());
    }
}
