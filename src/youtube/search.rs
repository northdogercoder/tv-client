//! Bounded client for the control server's YouTube search endpoint.
//!
//! The device bearer is the only credential. It is attached to the
//! `Authorization` header and never appears in the URL, a page token, an error
//! message or a trace field. Responses are limited to 64 KiB and 10 items;
//! thumbnails are validated as official `i.ytimg.com` URLs but are never
//! fetched here.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::cloud::DeviceToken;
use crate::cloud::http::MAX_HTTP_BODY_BYTES;
use crate::jellyfin::CancelToken;
use crate::model::is_valid_youtube_id;
use crate::youtube::VideoId;

/// Longest accepted search query, in characters.
pub const MAX_QUERY_CHARS: usize = 100;
/// Longest accepted opaque page token, in bytes.
pub const MAX_PAGE_TOKEN_BYTES: usize = 512;
/// Hard limit on rendered search results per page.
pub const MAX_SEARCH_RESULTS: usize = 10;
/// Longest accepted video title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest accepted channel title, in characters.
pub const MAX_CHANNEL_CHARS: usize = 100;
/// Fixed request budget for one search page.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const SEARCH_PATH: &str = "/v1/youtube/search";

/// One validated search page request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    query: String,
    page_token: Option<String>,
}

impl SearchRequest {
    /// Validate a query and optional opaque page token.
    pub fn new(query: &str, page_token: Option<&str>) -> Result<Self, SearchError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::QueryTooLong);
        }
        if query.chars().any(char::is_control) {
            return Err(SearchError::InvalidQuery);
        }
        let page_token = match page_token {
            None | Some("") => None,
            Some(token) => {
                if token.len() > MAX_PAGE_TOKEN_BYTES {
                    return Err(SearchError::InvalidPageToken);
                }
                if !token
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ' || byte == b'=')
                {
                    return Err(SearchError::InvalidPageToken);
                }
                Some(token.to_owned())
            }
        };
        Ok(Self {
            query: query.to_owned(),
            page_token,
        })
    }

    /// Query text.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Opaque continuation token, when requesting a later page.
    #[must_use]
    pub fn page_token(&self) -> Option<&str> {
        self.page_token.as_deref()
    }
}

/// One validated search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// 11-character video identifier.
    pub video_id: String,
    /// Video title.
    pub title: String,
    /// Channel display name.
    pub channel_title: String,
    /// Official thumbnail URL; display only, never a playback source.
    pub thumbnail_url: String,
}

/// One search response page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchPage {
    /// At most [`MAX_SEARCH_RESULTS`] items.
    pub items: Vec<SearchResult>,
    /// Continuation token for the next page.
    pub next_page_token: Option<String>,
}

/// Typed search failures.
///
/// No variant carries upstream text, a URL, a token or an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SearchError {
    /// The query was empty after trimming.
    #[error("youtube search query must not be empty")]
    EmptyQuery,
    /// The query exceeds the protocol limit.
    #[error("youtube search query is longer than 100 characters")]
    QueryTooLong,
    /// The query contains control characters.
    #[error("youtube search query contains control characters")]
    InvalidQuery,
    /// The continuation token is malformed or too long.
    #[error("youtube page token is invalid")]
    InvalidPageToken,
    /// The configured origin is not an acceptable HTTPS origin.
    #[error("youtube search endpoint is invalid")]
    InvalidEndpoint,
    /// The server requires a different or renewed credential.
    #[error("youtube search is unauthorized")]
    Unauthorized,
    /// The feature is disabled server-side or not deployed.
    #[error("youtube search is not available")]
    FeatureDisabled,
    /// The request was malformed for the current server.
    #[error("youtube search request was rejected")]
    BadRequest,
    /// Rate or quota limit reached; retry later.
    #[error("youtube search is rate limited")]
    RateLimited,
    /// Upstream search is temporarily unavailable.
    #[error("youtube search is temporarily unavailable")]
    Unavailable,
    /// Any other HTTP status.
    #[error("youtube search failed with status {status}")]
    Server {
        /// HTTP status code.
        status: u16,
    },
    /// The response body exceeded the accepted bound.
    #[error("youtube search response is too large")]
    ResponseTooLarge,
    /// The response did not match the contract.
    #[error("youtube search response is invalid")]
    InvalidResponse,
    /// The transport failed before a response was received.
    #[error("youtube search transport failed")]
    Transport,
    /// The request exceeded the fixed timeout.
    #[error("youtube search timed out")]
    Timeout,
    /// The request was cancelled by the caller.
    #[error("youtube search was cancelled")]
    Cancelled,
}

/// Raw HTTP result handed back by a search transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResponse {
    /// HTTP status code.
    pub status: u16,
    /// Bounded response body.
    pub body: Vec<u8>,
}

/// One HTTP GET, with the bearer supplied separately from the URL.
pub trait SearchTransport: Send + Sync {
    /// Perform a GET and return the status and body without interpreting it.
    fn get<'a>(
        &'a self,
        url: &'a str,
        authorization: &'a str,
    ) -> BoxFuture<'a, Result<SearchResponse, SearchError>>;
}

/// `reqwest`-backed transport with no redirects and a bounded body.
#[derive(Debug, Clone)]
pub struct HttpSearchTransport {
    client: reqwest::Client,
}

impl HttpSearchTransport {
    /// Build the production transport.
    pub fn new() -> Result<Self, SearchError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(SEARCH_TIMEOUT)
            .build()
            .map_err(|_| SearchError::Transport)?;
        Ok(Self { client })
    }
}

impl SearchTransport for HttpSearchTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        authorization: &'a str,
    ) -> BoxFuture<'a, Result<SearchResponse, SearchError>> {
        Box::pin(async move {
            let mut response = self
                .client
                .get(url)
                .header(AUTHORIZATION, authorization)
                .send()
                .await
                .map_err(|_| SearchError::Transport)?;
            let status = response.status().as_u16();
            if response
                .content_length()
                .is_some_and(|length| length > MAX_HTTP_BODY_BYTES as u64)
            {
                return Err(SearchError::ResponseTooLarge);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| SearchError::Transport)? {
                if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
                    return Err(SearchError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(SearchResponse { status, body })
        })
    }
}

/// Search client bound to one control-server origin and device bearer.
#[derive(Clone)]
pub struct SearchClient {
    origin: Url,
    token: DeviceToken,
    transport: Arc<dyn SearchTransport>,
    timeout: Duration,
}

impl std::fmt::Debug for SearchClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearchClient")
            .field("origin", &self.origin.as_str())
            .field("token", &"redacted")
            .finish()
    }
}

impl SearchClient {
    /// Build a production client for an enrolled device.
    pub fn new(server_url: &str, token: DeviceToken) -> Result<Self, SearchError> {
        Ok(Self {
            origin: parse_origin(server_url)?,
            token,
            transport: Arc::new(HttpSearchTransport::new()?),
            timeout: SEARCH_TIMEOUT,
        })
    }

    /// Build a client with a caller-supplied transport.
    pub fn with_transport(
        server_url: &str,
        token: DeviceToken,
        transport: Arc<dyn SearchTransport>,
    ) -> Result<Self, SearchError> {
        Ok(Self {
            origin: parse_origin(server_url)?,
            token,
            transport,
            timeout: SEARCH_TIMEOUT,
        })
    }

    /// Search one page, honouring cooperative cancellation.
    pub async fn search(
        &self,
        request: &SearchRequest,
        cancel: &CancelToken,
    ) -> Result<SearchPage, SearchError> {
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let url = self.endpoint(request)?;
        let authorization = format!("Bearer {}", self.token.expose());
        let mut cancel = cancel.clone();
        let future = self.transport.get(url.as_str(), authorization.as_str());
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(SearchError::Cancelled),
            result = tokio::time::timeout(self.timeout, future) => {
                match result {
                    Err(_) => return Err(SearchError::Timeout),
                    Ok(result) => result?,
                }
            }
        };
        interpret(response)
    }

    fn endpoint(&self, request: &SearchRequest) -> Result<Url, SearchError> {
        let mut url = self.origin.clone();
        url.set_path(SEARCH_PATH);
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", request.query());
        if let Some(token) = request.page_token() {
            pairs.append_pair("page_token", token);
        }
        drop(pairs);
        url.set_fragment(None);
        Ok(url)
    }
}

fn parse_origin(value: &str) -> Result<Url, SearchError> {
    let url = Url::parse(value).map_err(|_| SearchError::InvalidEndpoint)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SearchError::InvalidEndpoint);
    }
    Ok(url)
}

fn interpret(response: SearchResponse) -> Result<SearchPage, SearchError> {
    match response.status {
        200 => {}
        400 => return Err(SearchError::BadRequest),
        401 | 403 => return Err(SearchError::Unauthorized),
        404 => return Err(SearchError::FeatureDisabled),
        429 => return Err(SearchError::RateLimited),
        503 => return Err(SearchError::Unavailable),
        status => return Err(SearchError::Server { status }),
    }
    if response.body.len() > MAX_HTTP_BODY_BYTES {
        return Err(SearchError::ResponseTooLarge);
    }
    let raw: RawResponse =
        serde_json::from_slice(&response.body).map_err(|_| SearchError::InvalidResponse)?;
    if raw.items.len() > MAX_SEARCH_RESULTS {
        return Err(SearchError::InvalidResponse);
    }
    let next_page_token = match raw.next_page_token {
        None => None,
        Some(token) => {
            if token.is_empty() || token.len() > MAX_PAGE_TOKEN_BYTES {
                return Err(SearchError::InvalidResponse);
            }
            Some(token)
        }
    };
    let mut items = Vec::with_capacity(raw.items.len());
    for item in raw.items {
        items.push(validate_item(item)?);
    }
    Ok(SearchPage {
        items,
        next_page_token,
    })
}

fn validate_item(item: RawItem) -> Result<SearchResult, SearchError> {
    if !is_valid_youtube_id(&item.video_id) {
        return Err(SearchError::InvalidResponse);
    }
    if item.title.is_empty()
        || item.title.chars().count() > MAX_TITLE_CHARS
        || item.title.chars().any(char::is_control)
    {
        return Err(SearchError::InvalidResponse);
    }
    if item.channel_title.chars().count() > MAX_CHANNEL_CHARS
        || item.channel_title.chars().any(char::is_control)
    {
        return Err(SearchError::InvalidResponse);
    }
    let thumbnail = Url::parse(&item.thumbnail_url).map_err(|_| SearchError::InvalidResponse)?;
    let official_thumbnail = thumbnail.scheme() == "https"
        && thumbnail
            .host_str()
            .is_some_and(|host| host == "i.ytimg.com" || host.ends_with(".ytimg.com"))
        && thumbnail.username().is_empty()
        && thumbnail.password().is_none();
    if !official_thumbnail {
        return Err(SearchError::InvalidResponse);
    }
    Ok(SearchResult {
        video_id: item.video_id,
        title: item.title,
        channel_title: item.channel_title,
        thumbnail_url: item.thumbnail_url,
    })
}

/// Convenience: validate a server-supplied identifier before rendering it.
pub fn result_video_id(result: &SearchResult) -> Option<VideoId> {
    VideoId::parse(&result.video_id).ok()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResponse {
    items: Vec<RawItem>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawItem {
    video_id: String,
    title: String,
    channel_title: String,
    thumbnail_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_limits_are_enforced_before_any_io() {
        assert_eq!(
            SearchRequest::new("   ", None),
            Err(SearchError::EmptyQuery)
        );
        assert_eq!(
            SearchRequest::new(&"a".repeat(MAX_QUERY_CHARS + 1), None),
            Err(SearchError::QueryTooLong)
        );
        assert_eq!(
            SearchRequest::new("cats", Some(&"t".repeat(MAX_PAGE_TOKEN_BYTES + 1))),
            Err(SearchError::InvalidPageToken)
        );
        let request = SearchRequest::new("  lofi  ", None).expect("valid");
        assert_eq!(request.query(), "lofi");
        assert_eq!(request.page_token(), None);
    }

    #[test]
    fn endpoint_never_contains_the_bearer_and_encodes_the_query() {
        let token = DeviceToken::new("device-token-value-123").expect("token");
        let client = SearchClient::with_transport(
            "https://control.example.test",
            token,
            Arc::new(NoopTransport),
        )
        .expect("client");
        let request = SearchRequest::new("jazz & rain", Some("abc+/=")).expect("request");
        let url = client.endpoint(&request).expect("endpoint");
        assert_eq!(url.path(), "/v1/youtube/search");
        assert!(!url.as_str().contains("device-token-value-123"));
        assert!(url.query().is_some_and(|query| query.contains("q=jazz")));
        assert!(
            url.query()
                .is_some_and(|query| query.contains("page_token="))
        );
    }

    #[test]
    fn response_validation_rejects_non_official_thumbnails() {
        let body = br#"{"items":[{"video_id":"M7lc1UVf-VE","title":"T","channel_title":"C","thumbnail_url":"https://evil.example/x.jpg"}],"next_page_token":null}"#;
        assert_eq!(
            interpret(SearchResponse {
                status: 200,
                body: body.to_vec(),
            }),
            Err(SearchError::InvalidResponse)
        );
    }

    #[test]
    fn status_mapping_is_typed() {
        for (status, expected) in [
            (400, SearchError::BadRequest),
            (401, SearchError::Unauthorized),
            (404, SearchError::FeatureDisabled),
            (429, SearchError::RateLimited),
            (503, SearchError::Unavailable),
            (500, SearchError::Server { status: 500 }),
        ] {
            assert_eq!(
                interpret(SearchResponse {
                    status,
                    body: Vec::new(),
                }),
                Err(expected)
            );
        }
    }

    struct NoopTransport;

    impl SearchTransport for NoopTransport {
        fn get<'a>(
            &'a self,
            _url: &'a str,
            _authorization: &'a str,
        ) -> BoxFuture<'a, Result<SearchResponse, SearchError>> {
            Box::pin(async { Err(SearchError::Transport) })
        }
    }
}
