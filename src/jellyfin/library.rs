//! Paginated library browsing.
//!
//! All methods return typed metadata and opaque item identifiers. Stream URLs
//! are never requested, parsed or stored.

use url::Url;

use crate::jellyfin::JellyfinClient;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::models::{
    BaseItem, ItemFilter, MAX_SEARCH_TERM_BYTES, Page, QueryResult, SortOrder, UserDataUpdate,
    UserItemData,
};

const DEFAULT_FIELDS: &str = "UserData,ImageTags,Overview";

/// Item page size used by a poster/library grid.
pub const BROWSE_PAGE_SIZE: u32 = 24;
/// Playable item types shown in a library grid.
pub const BROWSE_INCLUDE_ITEM_TYPES: [&str; 3] = ["Movie", "Episode", "Video"];
/// Maximum number of library views handed to the UI in one list.
pub const MAX_LIBRARIES: usize = 32;

const MAX_LIBRARY_NAME_CHARS: usize = 128;
const MAX_BROWSE_TITLE_CHARS: usize = 256;
const MAX_BROWSE_SUBTITLE_CHARS: usize = 96;
const MAX_BROWSE_OVERVIEW_CHARS: usize = 512;

/// One top-level library view.
///
/// Only the display name and collection type are copied; no URL, token or
/// filesystem path is modeled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryEntry {
    /// Opaque library (view) identifier, usable as a browse parent.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Jellyfin collection type, for example `movies` or `tvshows`.
    pub collection_type: Option<String>,
}

impl LibraryEntry {
    /// Project a server item onto the bounded browse model.
    #[must_use]
    pub fn from_base_item(item: &BaseItem) -> Self {
        let name = item.name.as_deref().unwrap_or("Library");
        Self {
            id: item.id.clone(),
            name: truncate_chars(name, MAX_LIBRARY_NAME_CHARS),
            collection_type: item.collection_type.clone(),
        }
    }
}

/// One poster-grid item.
///
/// The primary image tag is copied so the UI can request artwork through
/// [`crate::jellyfin::ImageRequest`] without seeing a server URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowseItem {
    /// Opaque item identifier.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Secondary line, for example `Movie · 2021 · 1h 52m`.
    pub subtitle: String,
    /// Bounded description, when supplied.
    pub overview: Option<String>,
    /// Server image tag for the primary artwork, when supplied.
    pub primary_tag: Option<String>,
    /// Jellyfin item type, for example `Movie` or `Episode`.
    pub item_type: Option<String>,
}

impl BrowseItem {
    /// Project a server item onto the bounded browse model.
    #[must_use]
    pub fn from_base_item(item: &BaseItem) -> Self {
        let title = item.name.as_deref().unwrap_or("Untitled");
        let item_type = item.item_type.clone();
        Self {
            id: item.id.clone(),
            title: truncate_chars(title, MAX_BROWSE_TITLE_CHARS),
            subtitle: truncate_chars(&subtitle_for(item), MAX_BROWSE_SUBTITLE_CHARS),
            overview: item
                .overview
                .as_deref()
                .map(|overview| truncate_chars(overview, MAX_BROWSE_OVERVIEW_CHARS)),
            primary_tag: item.image_tags.get("Primary").cloned(),
            item_type,
        }
    }
}

/// Build the bounded secondary line shown under a title.
fn subtitle_for(item: &BaseItem) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(item_type) = item.item_type.as_deref()
        && !item_type.is_empty()
    {
        parts.push(item_type.to_owned());
    }
    if let Some(year) = item.production_year {
        parts.push(year.to_string());
    }
    if let Some(ticks) = item.run_time_ticks {
        let minutes = ticks / 600_000_000;
        if minutes >= 60 {
            parts.push(format!("{}h {}m", minutes / 60, minutes % 60));
        } else if minutes > 0 {
            parts.push(format!("{minutes}m"));
        }
    }
    parts.join(" · ")
}

/// Truncate to a fixed character count without splitting a UTF-8 code point.
fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    value.chars().take(max).collect()
}

/// Query parameters for `/Users/{user}/Items`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemQuery {
    /// Restrict to a parent folder.
    pub parent_id: Option<String>,
    /// Restrict item types, for example `Movie` or `Episode`.
    pub include_item_types: Vec<String>,
    /// Restrict media types, for example `Video` or `Audio`.
    pub media_types: Vec<String>,
    /// Recurse into child folders.
    pub recursive: bool,
    /// Server-side search term.
    pub search_term: Option<String>,
    /// Sort fields, in order.
    pub sort_by: Vec<String>,
    /// Sort direction.
    pub sort_order: SortOrder,
    /// Item filters.
    pub filters: Vec<ItemFilter>,
    /// Metadata fields to include.
    pub fields: Vec<String>,
    /// Page window.
    pub page: Page,
}

impl Default for ItemQuery {
    fn default() -> Self {
        Self {
            parent_id: None,
            include_item_types: Vec::new(),
            media_types: Vec::new(),
            recursive: true,
            search_term: None,
            sort_by: Vec::new(),
            sort_order: SortOrder::Ascending,
            filters: Vec::new(),
            fields: vec![
                "UserData".to_owned(),
                "ImageTags".to_owned(),
                "Overview".to_owned(),
            ],
            page: Page::default(),
        }
    }
}

impl ItemQuery {
    /// Empty query with the default page size.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One bounded page of playable movies, episodes and videos below a
    /// library view.
    ///
    /// The query requests `UserData`, `ImageTags` and `Overview`, restricts
    /// `IncludeItemTypes` to `Movie,Episode,Video` and `MediaTypes` to `Video`,
    /// and uses [`BROWSE_PAGE_SIZE`] (24) to bound poster fetches per page.
    pub fn browse(parent_id: &str, start_index: u32) -> Result<Self, JellyfinError> {
        Ok(Self::new()
            .parent(parent_id)
            .include_types(&BROWSE_INCLUDE_ITEM_TYPES)
            .media_types(&["Video"])
            .fields(&["UserData", "ImageTags", "Overview"])
            .page(Page::new(start_index, BROWSE_PAGE_SIZE)?))
    }

    /// Restrict to a parent folder.
    #[must_use]
    pub fn parent(mut self, parent_id: &str) -> Self {
        self.parent_id = Some(parent_id.to_owned());
        self
    }

    /// Restrict item types.
    #[must_use]
    pub fn include_types(mut self, types: &[&str]) -> Self {
        self.include_item_types = types.iter().map(|value| (*value).to_owned()).collect();
        self
    }

    /// Restrict media types.
    #[must_use]
    pub fn media_types(mut self, types: &[&str]) -> Self {
        self.media_types = types.iter().map(|value| (*value).to_owned()).collect();
        self
    }

    /// Set recursion.
    #[must_use]
    pub fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// Set the search term.
    #[must_use]
    pub fn search_term(mut self, term: &str) -> Self {
        self.search_term = Some(term.to_owned());
        self
    }

    /// Set the sort field list.
    #[must_use]
    pub fn sort_by(mut self, fields: &[&str]) -> Self {
        self.sort_by = fields.iter().map(|value| (*value).to_owned()).collect();
        self
    }

    /// Set the sort direction.
    #[must_use]
    pub fn sort_order(mut self, order: SortOrder) -> Self {
        self.sort_order = order;
        self
    }

    /// Add one item filter.
    #[must_use]
    pub fn filter(mut self, filter: ItemFilter) -> Self {
        self.filters.push(filter);
        self
    }

    /// Replace the requested metadata fields.
    #[must_use]
    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.fields = fields.iter().map(|value| (*value).to_owned()).collect();
        self
    }

    /// Set the page window.
    #[must_use]
    pub fn page(mut self, page: Page) -> Self {
        self.page = page;
        self
    }

    fn validate(&self) -> Result<(), JellyfinError> {
        if let Some(parent) = &self.parent_id
            && !crate::model::is_valid_item_id(parent)
        {
            return Err(JellyfinError::InvalidItemId);
        }
        if let Some(term) = &self.search_term
            && (term.is_empty()
                || term.len() > MAX_SEARCH_TERM_BYTES
                || term.chars().any(char::is_control))
        {
            return Err(JellyfinError::InvalidRequest);
        }
        for value in self
            .include_item_types
            .iter()
            .chain(&self.media_types)
            .chain(&self.sort_by)
            .chain(&self.fields)
        {
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(JellyfinError::InvalidRequest);
            }
        }
        Ok(())
    }
}

impl JellyfinClient {
    /// List the user's top-level views.
    pub async fn views(
        &self,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        let user = self.user_id()?;
        let url = self.endpoint(&["Users", user, "Views"])?;
        self.get_json(url, cancel).await
    }

    /// List bounded, UI-ready library views.
    ///
    /// At most [`MAX_LIBRARIES`] entries are returned, so a hostile or
    /// misconfigured server cannot grow the browse model without bound.
    pub async fn libraries(
        &self,
        cancel: &CancelToken,
    ) -> Result<Vec<LibraryEntry>, JellyfinError> {
        let views = self.views(cancel).await?;
        Ok(views
            .items
            .iter()
            .take(MAX_LIBRARIES)
            .map(LibraryEntry::from_base_item)
            .collect())
    }

    /// List one bounded page of playable items below a library view.
    pub async fn browse(
        &self,
        library_id: &str,
        start_index: u32,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        let query = ItemQuery::browse(library_id, start_index)?;
        self.items(&query, cancel).await
    }

    /// List items matching a typed query.
    pub async fn items(
        &self,
        query: &ItemQuery,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        query.validate()?;
        let mut url = self.user_items_url(&[])?;
        append_item_query(&mut url, query);
        self.get_json(url, cancel).await
    }

    /// List resumable videos for the user.
    pub async fn resume(
        &self,
        page: Page,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        let query = ItemQuery::new()
            .media_types(&["Video"])
            .filter(ItemFilter::Resumable)
            .sort_by(&["DatePlayed"])
            .sort_order(SortOrder::Descending)
            .page(page);
        self.items(&query, cancel).await
    }

    /// List recently added items, optionally within one parent.
    pub async fn latest(
        &self,
        parent_id: Option<&str>,
        limit: u32,
        cancel: &CancelToken,
    ) -> Result<Vec<BaseItem>, JellyfinError> {
        if limit == 0 || limit > crate::jellyfin::models::MAX_PAGE_SIZE {
            return Err(JellyfinError::InvalidRequest);
        }
        if let Some(parent) = parent_id
            && !crate::model::is_valid_item_id(parent)
        {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        let mut url = self.endpoint(&["Users", user, "Items", "Latest"])?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("Limit", &limit.to_string());
            if let Some(parent) = parent_id {
                pairs.append_pair("ParentId", parent);
            }
            pairs.append_pair("Fields", DEFAULT_FIELDS);
        }
        self.get_json(url, cancel).await
    }

    /// List seasons for a series.
    pub async fn seasons(
        &self,
        series_id: &str,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        if !crate::model::is_valid_item_id(series_id) {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        let mut url = self.endpoint(&["Shows", series_id, "Seasons"])?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("userId", user);
            pairs.append_pair("Fields", DEFAULT_FIELDS);
        }
        self.get_json(url, cancel).await
    }

    /// List episodes for a series, optionally within one season.
    pub async fn episodes(
        &self,
        series_id: &str,
        season_id: Option<&str>,
        page: Page,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        if !crate::model::is_valid_item_id(series_id) {
            return Err(JellyfinError::InvalidItemId);
        }
        if let Some(season) = season_id
            && !crate::model::is_valid_item_id(season)
        {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        let mut url = self.endpoint(&["Shows", series_id, "Episodes"])?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("userId", user);
            if let Some(season) = season_id {
                pairs.append_pair("SeasonId", season);
            }
            pairs.append_pair("StartIndex", &page.start_index().to_string());
            pairs.append_pair("Limit", &page.limit().to_string());
            pairs.append_pair("SortBy", "ParentIndexNumber,IndexNumber");
            pairs.append_pair("SortOrder", "Ascending");
            pairs.append_pair("Fields", DEFAULT_FIELDS);
        }
        self.get_json(url, cancel).await
    }

    /// List audio items, optionally within one parent.
    pub async fn music(
        &self,
        parent_id: Option<&str>,
        page: Page,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        let mut query = ItemQuery::new()
            .include_types(&["Audio"])
            .media_types(&["Audio"])
            .page(page);
        if let Some(parent) = parent_id {
            query = query.parent(parent);
        }
        self.items(&query, cancel).await
    }

    /// Search the library with the server's item search.
    pub async fn search(
        &self,
        term: &str,
        page: Page,
        cancel: &CancelToken,
    ) -> Result<QueryResult<BaseItem>, JellyfinError> {
        let query = ItemQuery::new()
            .search_term(term)
            .sort_by(&["SortName"])
            .page(page);
        self.items(&query, cancel).await
    }

    /// Read one item's per-user playback metadata.
    pub async fn user_data(
        &self,
        item_id: &str,
        cancel: &CancelToken,
    ) -> Result<UserItemData, JellyfinError> {
        let url = self.item_user_data_url(item_id)?;
        self.get_json(url, cancel).await
    }

    /// Update played state, resume position or favourite flag.
    pub async fn update_user_data(
        &self,
        item_id: &str,
        update: &UserDataUpdate,
        cancel: &CancelToken,
    ) -> Result<UserItemData, JellyfinError> {
        let url = self.item_user_data_url(item_id)?;
        self.post_json(url, update, cancel).await
    }

    /// Read one item's metadata, including per-user resume state.
    pub async fn item(
        &self,
        item_id: &str,
        cancel: &CancelToken,
    ) -> Result<BaseItem, JellyfinError> {
        if !crate::model::is_valid_item_id(item_id) {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        let mut url = self.endpoint(&["Items", item_id])?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("UserId", user);
            query.append_pair("Fields", DEFAULT_FIELDS);
        }
        self.get_json(url, cancel).await
    }

    /// Find the episode that follows `item_id` in the same series.
    ///
    /// Returns `None` for non-episode items and for the final episode. The
    /// lookup is confined to the configured server and the signed-in user.
    pub async fn next_episode(
        &self,
        item_id: &str,
        cancel: &CancelToken,
    ) -> Result<Option<BaseItem>, JellyfinError> {
        let current = self.item(item_id, cancel).await?;
        if !current
            .item_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("episode"))
        {
            return Ok(None);
        }
        let Some(series_id) = current.series_id.as_deref() else {
            return Ok(None);
        };
        let page = Page::new(0, crate::jellyfin::MAX_PAGE_SIZE)?;
        let episodes = self.episodes(series_id, None, page, cancel).await?;
        let mut items = episodes.items.into_iter();
        let Some(_) = items.position(|episode| episode.id == current.id) else {
            return Ok(None);
        };
        // `position` consumes through the current episode, so the next value is
        // the following episode.
        Ok(items.next())
    }

    fn user_items_url(&self, suffix: &[&str]) -> Result<Url, JellyfinError> {
        let user = self.user_id()?;
        let mut segments = vec!["Users", user, "Items"];
        segments.extend_from_slice(suffix);
        self.endpoint(&segments)
    }

    fn item_user_data_url(&self, item_id: &str) -> Result<Url, JellyfinError> {
        if !crate::model::is_valid_item_id(item_id) {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        self.endpoint(&["Users", user, "Items", item_id, "UserData"])
    }
}

fn append_item_query(url: &mut Url, query: &ItemQuery) {
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair("StartIndex", &query.page.start_index().to_string());
    pairs.append_pair("Limit", &query.page.limit().to_string());
    pairs.append_pair("Recursive", if query.recursive { "true" } else { "false" });
    if let Some(parent) = &query.parent_id {
        pairs.append_pair("ParentId", parent);
    }
    if let Some(term) = &query.search_term {
        pairs.append_pair("SearchTerm", term);
    }
    if !query.include_item_types.is_empty() {
        pairs.append_pair("IncludeItemTypes", &query.include_item_types.join(","));
    }
    if !query.media_types.is_empty() {
        pairs.append_pair("MediaTypes", &query.media_types.join(","));
    }
    if !query.sort_by.is_empty() {
        pairs.append_pair("SortBy", &query.sort_by.join(","));
        pairs.append_pair("SortOrder", query.sort_order.as_str());
    }
    if !query.filters.is_empty() {
        let filters = query
            .filters
            .iter()
            .map(|filter| filter.as_str())
            .collect::<Vec<_>>()
            .join(",");
        pairs.append_pair("Filters", &filters);
    }
    if !query.fields.is_empty() {
        pairs.append_pair("Fields", &query.fields.join(","));
    }
}
