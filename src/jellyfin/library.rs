//! Paginated library browsing.
//!
//! All methods return typed metadata and opaque item identifiers. Stream URLs
//! are never requested, parsed or stored.

use serde::Serialize;
use url::Url;

use crate::jellyfin::JellyfinClient;
use crate::jellyfin::cancel::CancelToken;
use crate::jellyfin::error::JellyfinError;
use crate::jellyfin::models::{
    BaseItem, ItemFilter, MAX_SEARCH_TERM_BYTES, Page, QueryResult, SortOrder, UserDataUpdate,
    UserItemData,
};

const DEFAULT_FIELDS: &str = "UserData,ImageTags,Overview";

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

    /// Negotiate playback capabilities without exposing a stream URL.
    pub async fn playback_info(
        &self,
        item_id: &str,
        cancel: &CancelToken,
    ) -> Result<crate::jellyfin::models::PlaybackInfoResponse, JellyfinError> {
        if !crate::model::is_valid_item_id(item_id) {
            return Err(JellyfinError::InvalidItemId);
        }
        let user = self.user_id()?;
        let mut url = self.endpoint(&["Items", item_id, "PlaybackInfo"])?;
        url.query_pairs_mut().append_pair("UserId", user);
        self.post_json(url, &EmptyBody {}, cancel).await
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

#[derive(Serialize)]
struct EmptyBody {}
