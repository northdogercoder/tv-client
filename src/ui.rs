//! Pure view model for the ten-foot launcher, plus the optional Slint bridge.
//!
//! Everything except the `native` child module is always compiled and uses no
//! Slint types. The view model owns only navigation, focus and presentation
//! state; it never performs network, player or storage I/O. The bridge is
//! compiled only with the `native-ui` feature and turns keyboard input into
//! `UiCommand` values for the owning controller.
//!
//! Remote wire actions and local physical input are distinct input paths. A
//! remote action can never approve the local confirmation prompt.

use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    Action, AppState, Direction, InputOrigin, Mode, Source, is_valid_item_id, is_valid_youtube_id,
};

/// Number of entries on the home screen.
pub const HOME_ROW_COUNT: usize = 5;
/// Maximum number of library entries retained by the view model.
pub const MAX_LIBRARY_ITEMS: usize = 64;
/// Maximum search query length in characters.
pub const MAX_QUERY_CHARS: usize = 48;
/// Maximum error text length in characters.
pub const MAX_ERROR_CHARS: usize = 160;
/// Minimum interval between accepted repeats of a navigation key.
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(120);

/// A key that the launcher understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Home,
    Backspace,
    Char(char),
}

impl Key {
    /// Whether holding this key should be rate limited.
    #[must_use]
    pub const fn is_repeatable(self) -> bool {
        matches!(
            self,
            Self::Up | Self::Down | Self::Left | Self::Right | Self::Backspace
        )
    }
}

/// One input event, tagged with its trust boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiInput {
    /// A key produced on the physical device (keyboard or CEC).
    LocalKey { key: Key, origin: InputOrigin },
    /// A command decoded from the network. It is never local input.
    RemoteAction(Action),
}

/// Top-level screen shown by the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Library,
    Player,
    Settings,
    Cast,
}

impl Screen {
    /// Stable index shared with the declarative UI.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Home => 0,
            Self::Library => 1,
            Self::Player => 2,
            Self::Settings => 3,
            Self::Cast => 4,
        }
    }
}

/// Fixed home-screen rows, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeRow {
    Resume,
    Jellyfin,
    YouTube,
    Cast,
    Settings,
}

impl HomeRow {
    /// Rows in display order.
    pub const ALL: [Self; HOME_ROW_COUNT] = [
        Self::Resume,
        Self::Jellyfin,
        Self::YouTube,
        Self::Cast,
        Self::Settings,
    ];

    /// Position in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Resume => 0,
            Self::Jellyfin => 1,
            Self::YouTube => 2,
            Self::Cast => 3,
            Self::Settings => 4,
        }
    }

    /// Large-print row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Resume => "Resume",
            Self::Jellyfin => "Jellyfin",
            Self::YouTube => "YouTube",
            Self::Cast => "Cast",
            Self::Settings => "Settings",
        }
    }
}

/// Human-readable label for a media source.
#[must_use]
pub const fn source_label(source: Source) -> &'static str {
    match source {
        Source::Jellyfin => "Jellyfin",
        Source::YouTube => "YouTube",
        Source::AirPlay => "AirPlay",
        Source::WebRtc => "WebRTC",
    }
}

/// Library source filter shown above the item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    Jellyfin,
    YouTube,
}

impl SourceFilter {
    /// Human-readable filter label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All sources",
            Self::Jellyfin => "Jellyfin",
            Self::YouTube => "YouTube",
        }
    }

    /// Whether an item from `source` is visible under this filter.
    #[must_use]
    pub const fn matches(self, source: Source) -> bool {
        match self {
            Self::All => true,
            Self::Jellyfin => matches!(source, Source::Jellyfin),
            Self::YouTube => matches!(source, Source::YouTube),
        }
    }

    /// Next filter in the left-to-right cycle.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Jellyfin,
            Self::Jellyfin => Self::YouTube,
            Self::YouTube => Self::All,
        }
    }

    /// Previous filter in the left-to-right cycle.
    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::All => Self::YouTube,
            Self::Jellyfin => Self::All,
            Self::YouTube => Self::Jellyfin,
        }
    }
}

/// One library entry. Text only: the launcher never loads unbounded images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub source: Source,
}

impl MediaItem {
    /// Create an item with no subtitle.
    #[must_use]
    pub fn new(source: Source, id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: String::new(),
            source,
        }
    }

    /// Attach a secondary line of text.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    /// Display label for this item's source.
    #[must_use]
    pub fn source_label(&self) -> &'static str {
        source_label(self.source)
    }
}

/// Loading state of the library screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// The source has not been opened yet.
    Idle,
    /// A fetch is in flight. The UI never performs that fetch itself.
    Loading,
    /// Items are available.
    Ready,
    /// The fetch succeeded but returned no visible entries.
    Empty,
    /// The fetch failed and may be retried.
    Error { message: String },
}

impl LoadState {
    /// Whether a retry can be offered.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    /// Error text, when the state is [`Self::Error`].
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { message } => Some(message),
            _ => None,
        }
    }
}

/// Why the view model refused an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UiReject {
    /// Physical confirmation must be produced locally.
    #[error("remote input cannot confirm a physical action")]
    RemoteApprovalDenied,
    /// Pairing requests must carry a non-nil identifier.
    #[error("pairing request identifier must not be nil")]
    InvalidRequest,
    /// A confirmation is already waiting for the operator.
    #[error("a confirmation is already pending")]
    ConfirmationBusy,
    /// No item is focused.
    #[error("no item is focused")]
    NoSelection,
    /// The focused identifier is not valid for its source.
    #[error("the focused item identifier is invalid for its source")]
    InvalidItem,
    /// Retry was requested outside an error state.
    #[error("no retry is available")]
    NothingToRetry,
}

/// Result of feeding one input event to the view model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiOutcome {
    /// Send this protocol action to the reducer.
    Action(Action),
    /// Approve a pending pairing request from local input.
    ApprovePairing { request_id: Uuid },
    /// Ask the owner to retry the current library load.
    Retry,
    /// The prompt or search field was closed.
    Dismissed,
    /// The screen or focus changed without producing a reducer action.
    Navigated { screen: Screen, focus: usize },
    /// The input was inside the repeat throttle window.
    Throttled,
    /// The input has no effect on the current screen.
    Ignored,
    /// The input was refused.
    Rejected(UiReject),
}

/// Choice in the local confirmation prompt. Deny is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairChoice {
    Deny,
    Approve,
}

impl PairChoice {
    /// Stable index shared with the declarative UI.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Deny => 0,
            Self::Approve => 1,
        }
    }
}

/// A pairing request waiting for a local physical decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairPrompt {
    pub request_id: Uuid,
    pub choice: PairChoice,
}

impl PairPrompt {
    /// Fixed prompt text. It never interpolates remote-controlled strings.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        "A device wants to pair with this TV. Approve only if you started pairing."
    }
}

/// Repeat gate for held navigation keys.
#[derive(Debug, Clone, Default)]
struct RepeatThrottle {
    last: [Option<Duration>; 5],
}

impl RepeatThrottle {
    const fn slot(key: Key) -> Option<usize> {
        match key {
            Key::Up => Some(0),
            Key::Down => Some(1),
            Key::Left => Some(2),
            Key::Right => Some(3),
            Key::Backspace => Some(4),
            Key::Enter | Key::Escape | Key::Home | Key::Char(_) => None,
        }
    }

    fn accept(&mut self, key: Key, now: Duration) -> bool {
        let Some(slot) = Self::slot(key) else {
            return true;
        };
        if let Some(last) = self.last[slot]
            && now.saturating_sub(last) < REPEAT_INTERVAL
        {
            return false;
        }
        self.last[slot] = Some(now);
        true
    }
}

/// Player-screen projection of the reducer state.
#[derive(Debug, Clone)]
struct PlayerView {
    title: String,
    source: Option<Source>,
    position_ms: u64,
    duration_ms: Option<u64>,
    paused: bool,
    loading: bool,
    error: Option<String>,
}

impl Default for PlayerView {
    fn default() -> Self {
        Self {
            title: "Nothing playing".to_owned(),
            source: None,
            position_ms: 0,
            duration_ms: None,
            paused: false,
            loading: false,
            error: None,
        }
    }
}

/// Library-screen projection with stable selection across refreshes.
#[derive(Debug, Clone)]
struct LibraryView {
    filter: SourceFilter,
    items: Vec<MediaItem>,
    focus: usize,
    selected: Option<String>,
    load: LoadState,
    query: String,
    searching: bool,
}

impl Default for LibraryView {
    fn default() -> Self {
        Self {
            filter: SourceFilter::All,
            items: Vec::new(),
            focus: 0,
            selected: None,
            load: LoadState::Idle,
            query: String::new(),
            searching: false,
        }
    }
}

impl LibraryView {
    fn matches_query(&self, item: &MediaItem) -> bool {
        let query = self.query.trim().to_lowercase();
        query.is_empty()
            || item.title.to_lowercase().contains(&query)
            || item.subtitle.to_lowercase().contains(&query)
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.filter.matches(item.source) && self.matches_query(item))
            .map(|(index, _)| index)
            .collect()
    }

    fn visible(&self) -> Vec<&MediaItem> {
        self.visible_indices()
            .into_iter()
            .map(|index| &self.items[index])
            .collect()
    }

    fn set_items(&mut self, mut items: Vec<MediaItem>) {
        items.truncate(MAX_LIBRARY_ITEMS);
        self.items = items;
        self.load = LoadState::Ready;
        self.refresh_selection();
    }

    fn refresh_selection(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.focus = 0;
            self.selected = None;
        } else {
            let previous = self.selected.take();
            let position = previous
                .as_ref()
                .and_then(|id| {
                    visible
                        .iter()
                        .position(|index| &self.items[*index].id == id)
                })
                .unwrap_or_else(|| self.focus.min(visible.len() - 1));
            self.focus = position;
            self.selected = Some(self.items[visible[position]].id.clone());
        }
        if !matches!(self.load, LoadState::Loading | LoadState::Error { .. }) {
            self.load = if visible.is_empty() {
                LoadState::Empty
            } else {
                LoadState::Ready
            };
        }
    }

    fn select(&mut self, index: usize) -> bool {
        let visible = self.visible_indices();
        let Some(item) = visible.get(index) else {
            return false;
        };
        self.focus = index;
        self.selected = Some(self.items[*item].id.clone());
        true
    }

    fn select_first(&mut self) {
        self.selected = None;
        self.focus = 0;
        self.refresh_selection();
    }
}

/// Complete UI-local state. This type is always compiled and Slint-free.
#[derive(Debug, Clone)]
pub struct UiModel {
    screen: Screen,
    home_focus: usize,
    library: LibraryView,
    settings_focus: usize,
    player_focus: usize,
    player: PlayerView,
    cast_state: String,
    status: String,
    modal: Option<PairPrompt>,
    throttle: RepeatThrottle,
    resume: Option<MediaItem>,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            screen: Screen::Home,
            home_focus: 0,
            library: LibraryView::default(),
            settings_focus: 0,
            player_focus: 0,
            player: PlayerView::default(),
            cast_state: "No receiver attached".to_owned(),
            status: "Choose a source".to_owned(),
            modal: None,
            throttle: RepeatThrottle::default(),
            resume: None,
        }
    }
}

impl UiModel {
    /// Create a model in the home screen.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current screen.
    #[must_use]
    pub const fn screen(&self) -> Screen {
        self.screen
    }

    /// Focused home row index.
    #[must_use]
    pub const fn home_focus(&self) -> usize {
        self.home_focus
    }

    /// Focused settings row index.
    #[must_use]
    pub const fn settings_focus(&self) -> usize {
        self.settings_focus
    }

    /// Focused player control index.
    #[must_use]
    pub const fn player_focus(&self) -> usize {
        self.player_focus
    }

    /// Current status line.
    #[must_use]
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// Library load state.
    #[must_use]
    pub fn library_load(&self) -> &LoadState {
        &self.library.load
    }

    /// Library source filter.
    #[must_use]
    pub const fn library_filter(&self) -> SourceFilter {
        self.library.filter
    }

    /// Whether the library screen is waiting for a controller fetch.
    #[must_use]
    pub fn is_library_loading(&self) -> bool {
        self.screen == Screen::Library && matches!(&self.library.load, LoadState::Loading)
    }

    /// The filter to fetch when an input transitioned into Loading.
    ///
    /// `was_loading` is sampled before the input is applied. Every local input
    /// path (keyboard, pointer activation, retry) uses this to emit exactly one
    /// load request per transition.
    #[must_use]
    pub fn pending_library_load(&self, was_loading: bool) -> Option<SourceFilter> {
        if was_loading || !self.is_library_loading() {
            None
        } else {
            Some(self.library.filter)
        }
    }

    /// Focused index within the visible library list.
    #[must_use]
    pub const fn library_focus(&self) -> usize {
        self.library.focus
    }

    /// Current search query.
    #[must_use]
    pub fn library_query(&self) -> &str {
        &self.library.query
    }

    /// Whether the search field currently owns keyboard input.
    #[must_use]
    pub const fn searching(&self) -> bool {
        self.library.searching
    }

    /// Error text for the library screen, if any.
    #[must_use]
    pub fn library_error_text(&self) -> &str {
        self.library.load.error_message().unwrap_or_default()
    }

    /// Items matching the current filter and search query.
    #[must_use]
    pub fn visible_items(&self) -> Vec<&MediaItem> {
        self.library.visible()
    }

    /// Number of visible items.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.library.visible_indices().len()
    }

    /// Focused visible item, if any.
    #[must_use]
    pub fn focused_item(&self) -> Option<&MediaItem> {
        let visible = self.library.visible();
        visible.get(self.library.focus).copied()
    }

    /// Stored item count, bounded by [`MAX_LIBRARY_ITEMS`].
    #[must_use]
    pub fn stored_item_count(&self) -> usize {
        self.library.items.len()
    }

    /// Resume item shown by the first home row.
    #[must_use]
    pub fn resume_item(&self) -> Option<&MediaItem> {
        self.resume.as_ref()
    }

    /// Set or clear the resume item.
    pub fn set_resume(&mut self, item: Option<MediaItem>) {
        self.resume = item;
    }

    /// Player title.
    #[must_use]
    pub fn player_title(&self) -> &str {
        &self.player.title
    }

    /// Player source label, or an em dash when idle.
    #[must_use]
    pub fn player_source_label(&self) -> &'static str {
        self.player.source.map(source_label).unwrap_or("--")
    }

    /// Whether playback is paused.
    #[must_use]
    pub const fn player_paused(&self) -> bool {
        self.player.paused
    }

    /// Whether the player is loading.
    #[must_use]
    pub const fn player_loading(&self) -> bool {
        self.player.loading
    }

    /// Playback error text, if any.
    #[must_use]
    pub fn player_error(&self) -> Option<&str> {
        self.player.error.as_deref()
    }

    /// Position and duration rendered as `m:ss / m:ss`.
    #[must_use]
    pub fn player_position_text(&self) -> String {
        let duration = self
            .player
            .duration_ms
            .map_or_else(|| "--:--".to_owned(), format_time);
        format!("{} / {duration}", format_time(self.player.position_ms))
    }

    /// Progress fraction in `0.0..=1.0`.
    #[must_use]
    pub fn player_progress(&self) -> f32 {
        let Some(duration) = self.player.duration_ms else {
            return 0.0;
        };
        if duration == 0 {
            return 0.0;
        }
        (self.player.position_ms as f32 / duration as f32).clamp(0.0, 1.0)
    }

    /// Cast screen status text.
    #[must_use]
    pub fn cast_state(&self) -> &str {
        &self.cast_state
    }

    /// Set the cast screen status text, bounded to keep the UI predictable.
    pub fn set_cast_state(&mut self, state: &str) {
        self.cast_state = sanitize(state, 80, "No receiver attached");
    }

    /// Pending local confirmation, if any.
    #[must_use]
    pub fn pair_prompt(&self) -> Option<&PairPrompt> {
        self.modal.as_ref()
    }

    /// Whether a confirmation prompt is visible.
    #[must_use]
    pub const fn pair_visible(&self) -> bool {
        self.modal.is_some()
    }

    /// Set home focus, clamped to the row list.
    pub fn set_home_focus(&mut self, index: usize) {
        self.home_focus = index.min(HOME_ROW_COUNT - 1);
    }

    /// Set settings focus, clamped to the row list.
    pub fn set_settings_focus(&mut self, index: usize) {
        self.settings_focus = index.min(SETTINGS_ROWS.len() - 1);
    }

    /// Set player control focus, clamped to the control row.
    pub fn set_player_focus(&mut self, index: usize) {
        self.player_focus = index.min(PLAYER_CONTROLS - 1);
    }

    /// Set the confirmation choice, clamped to the two buttons.
    pub fn focus_pair_choice(&mut self, index: usize) {
        if let Some(prompt) = self.modal.as_mut() {
            prompt.choice = if index == 0 {
                PairChoice::Deny
            } else {
                PairChoice::Approve
            };
        }
    }

    /// Focus one visible library item.
    pub fn select_library(&mut self, index: usize) -> bool {
        self.library.select(index)
    }

    /// Open a library filter and mark it as loading.
    pub fn open_library(&mut self, filter: SourceFilter) {
        self.screen = Screen::Library;
        self.library.filter = filter;
        self.library.items.clear();
        self.library.selected = None;
        self.library.focus = 0;
        self.library.query.clear();
        self.library.searching = false;
        self.library.load = LoadState::Loading;
        self.refresh_status();
    }

    /// Replace the library contents, preserving focus by item identifier.
    pub fn set_library_items(&mut self, items: Vec<MediaItem>) {
        self.library.set_items(items);
        if self.screen == Screen::Library {
            self.refresh_status();
        }
    }

    /// Mark the library as loading without touching the retained items.
    pub fn set_library_loading(&mut self) {
        self.library.load = LoadState::Loading;
        if self.screen == Screen::Library {
            self.refresh_status();
        }
    }

    /// Show a bounded library error message.
    pub fn set_library_error(&mut self, message: &str) {
        let message = sanitize(message, MAX_ERROR_CHARS, "unknown error");
        self.library.load = LoadState::Error { message };
        self.library.items.clear();
        self.library.selected = None;
        self.library.focus = 0;
        self.library.searching = false;
        self.refresh_status();
    }

    /// Enter search mode; only meaningful on the library screen.
    pub fn begin_search(&mut self) {
        if self.screen != Screen::Library {
            return;
        }
        self.library.searching = true;
        self.library.query.clear();
        self.library.select_first();
        self.refresh_status();
    }

    /// Leave search mode and clear the query.
    pub fn end_search(&mut self) {
        if !self.library.searching && self.library.query.is_empty() {
            return;
        }
        self.library.searching = false;
        self.library.query.clear();
        self.library.select_first();
        if self.screen == Screen::Library {
            self.refresh_status();
        }
    }

    /// Switch the visible source filter.
    pub fn set_library_filter(&mut self, filter: SourceFilter) {
        self.library.filter = filter;
        self.library.select_first();
        if self.screen == Screen::Library {
            self.refresh_status();
        }
    }

    /// Present a pairing request for local confirmation.
    ///
    /// The wire layer may call this when a device asks to pair, but approval
    /// still requires a local key: [`UiInput::RemoteAction`] is always refused
    /// while the prompt is visible.
    pub fn present_pairing_request(&mut self, request_id: Uuid) -> Result<(), UiReject> {
        if request_id.is_nil() {
            return Err(UiReject::InvalidRequest);
        }
        if self.modal.is_some() {
            return Err(UiReject::ConfirmationBusy);
        }
        self.modal = Some(PairPrompt {
            request_id,
            choice: PairChoice::Deny,
        });
        self.status = "Pairing confirmation required".to_owned();
        Ok(())
    }

    /// Open a confirmation that is only accepted from local input.
    pub fn open_local_confirmation(
        &mut self,
        origin: InputOrigin,
        request_id: Uuid,
    ) -> Result<(), UiReject> {
        if !origin.is_local() {
            return Err(UiReject::RemoteApprovalDenied);
        }
        self.present_pairing_request(request_id)
    }

    /// Project reducer state onto the player and screen.
    ///
    /// This never performs I/O and never closes the local confirmation prompt.
    pub fn apply_state(&mut self, state: &AppState) {
        self.player.title = state
            .title
            .clone()
            .unwrap_or_else(|| "Nothing playing".to_owned());
        self.player.source = state.source;
        self.player.position_ms = state.position_ms.unwrap_or(0);
        self.player.duration_ms = state.duration_ms;
        self.player.paused = state.paused;

        match state.mode {
            Mode::Home => {
                if matches!(self.screen, Screen::Player | Screen::Cast) {
                    self.screen = Screen::Home;
                }
                self.player.loading = false;
                self.player.error = None;
                self.refresh_status();
            }
            Mode::Browsing => {
                if self.screen == Screen::Home {
                    self.screen = Screen::Library;
                }
                self.refresh_status();
            }
            Mode::Loading => {
                self.screen = Screen::Player;
                self.player.loading = true;
                self.player.error = None;
                self.refresh_status();
            }
            Mode::Playing | Mode::Paused => {
                self.screen = Screen::Player;
                self.player.loading = false;
                self.player.error = None;
                self.refresh_status();
            }
            Mode::Casting => {
                self.screen = Screen::Cast;
                self.cast_state = format!(
                    "Casting {} ({})",
                    self.player.title,
                    self.player_source_label()
                );
                self.refresh_status();
            }
            Mode::Standby => {
                self.screen = Screen::Home;
                self.status = "Standby".to_owned();
            }
            Mode::Error => {
                self.screen = Screen::Player;
                self.player.loading = false;
                self.player.error = Some(
                    state
                        .error
                        .clone()
                        .unwrap_or_else(|| "unknown playback error".to_owned()),
                );
                self.refresh_status();
            }
        }
    }

    /// Feed one input event at a monotonic timestamp.
    pub fn handle_input(&mut self, input: UiInput, now: Duration) -> UiOutcome {
        match input {
            UiInput::LocalKey { key, origin } => {
                if origin.is_remote() {
                    return if self.modal.is_some() {
                        UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
                    } else {
                        UiOutcome::Ignored
                    };
                }
                self.handle_local_key(key, now)
            }
            UiInput::RemoteAction(_) => {
                if self.modal.is_some() {
                    UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
                } else {
                    UiOutcome::Ignored
                }
            }
        }
    }

    /// Convenience wrapper for local keyboard input.
    pub fn handle_key(&mut self, key: Key, now: Duration) -> UiOutcome {
        self.handle_input(
            UiInput::LocalKey {
                key,
                origin: InputOrigin::LocalKeyboard,
            },
            now,
        )
    }

    fn handle_local_key(&mut self, key: Key, now: Duration) -> UiOutcome {
        if !self.throttle.accept(key, now) {
            return UiOutcome::Throttled;
        }
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if self.screen == Screen::Library && self.library.searching {
            return self.handle_search_key(key);
        }
        match self.screen {
            Screen::Home => self.handle_home_key(key),
            Screen::Library => self.handle_library_key(key),
            Screen::Player => self.handle_player_key(key),
            Screen::Settings => self.handle_settings_key(key),
            Screen::Cast => self.handle_cast_key(key),
        }
    }

    fn handle_modal_key(&mut self, key: Key) -> UiOutcome {
        let Some(prompt) = self.modal.as_mut() else {
            return UiOutcome::Ignored;
        };
        match key {
            Key::Left | Key::Up => {
                prompt.choice = PairChoice::Deny;
                UiOutcome::Ignored
            }
            Key::Right | Key::Down => {
                prompt.choice = PairChoice::Approve;
                UiOutcome::Ignored
            }
            Key::Enter => {
                let request_id = prompt.request_id;
                let choice = prompt.choice;
                self.modal = None;
                self.refresh_status();
                match choice {
                    PairChoice::Deny => UiOutcome::Dismissed,
                    PairChoice::Approve => UiOutcome::ApprovePairing { request_id },
                }
            }
            Key::Escape => {
                self.modal = None;
                self.refresh_status();
                UiOutcome::Dismissed
            }
            Key::Home | Key::Backspace | Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn handle_home_key(&mut self, key: Key) -> UiOutcome {
        match key {
            Key::Up => {
                self.home_focus = self.home_focus.saturating_sub(1);
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Up,
                })
            }
            Key::Down => {
                self.home_focus = (self.home_focus + 1).min(HOME_ROW_COUNT - 1);
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Down,
                })
            }
            Key::Left => UiOutcome::Action(Action::Navigate {
                direction: Direction::Left,
            }),
            Key::Right => UiOutcome::Action(Action::Navigate {
                direction: Direction::Right,
            }),
            Key::Enter => self.activate_home(),
            Key::Escape => UiOutcome::Action(Action::Navigate {
                direction: Direction::Back,
            }),
            Key::Home => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Home,
                })
            }
            Key::Backspace => UiOutcome::Ignored,
            Key::Char('/') => self.activate_search(),
            Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn activate_home(&mut self) -> UiOutcome {
        let row = HomeRow::ALL[self.home_focus.min(HOME_ROW_COUNT - 1)];
        match row {
            HomeRow::Resume => {
                if let Some(item) = self.resume.clone() {
                    self.play_item(&item)
                } else {
                    self.status = "Nothing to resume".to_owned();
                    self.navigated()
                }
            }
            HomeRow::Jellyfin => {
                self.open_library(SourceFilter::Jellyfin);
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Select,
                })
            }
            HomeRow::YouTube => {
                self.open_library(SourceFilter::YouTube);
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Select,
                })
            }
            HomeRow::Cast => {
                self.screen = Screen::Cast;
                self.refresh_status();
                self.navigated()
            }
            HomeRow::Settings => {
                self.screen = Screen::Settings;
                self.status = "Settings".to_owned();
                self.navigated()
            }
        }
    }

    fn activate_search(&mut self) -> UiOutcome {
        if self.screen != Screen::Library {
            return UiOutcome::Ignored;
        }
        self.begin_search();
        self.navigated()
    }

    fn handle_library_key(&mut self, key: Key) -> UiOutcome {
        match key {
            Key::Escape => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Back,
                })
            }
            Key::Up => {
                self.library.focus = self.library.focus.saturating_sub(1);
                self.sync_focus_selection();
                self.focus_status();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Up,
                })
            }
            Key::Down => {
                let last = self.visible_len().saturating_sub(1);
                self.library.focus = (self.library.focus + 1).min(last);
                self.sync_focus_selection();
                self.focus_status();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Down,
                })
            }
            Key::Left => {
                self.set_library_filter(self.library.filter.previous());
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Left,
                })
            }
            Key::Right => {
                self.set_library_filter(self.library.filter.next());
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Right,
                })
            }
            Key::Home => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Home,
                })
            }
            Key::Enter => self.activate_library(),
            Key::Backspace => UiOutcome::Ignored,
            Key::Char('/') => self.activate_search(),
            Key::Char(character) if !character.is_control() => {
                self.begin_search();
                self.push_query_char(character);
                self.navigated()
            }
            Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn activate_library(&mut self) -> UiOutcome {
        let load = self.library.load.clone();
        match load {
            LoadState::Loading | LoadState::Idle => {
                self.status = "Still loading".to_owned();
                UiOutcome::Ignored
            }
            LoadState::Empty => {
                self.status = format!("No items in {}", self.library.filter.label());
                UiOutcome::Ignored
            }
            LoadState::Error { .. } => {
                self.library.load = LoadState::Loading;
                self.status = format!("Retrying {}", self.library.filter.label());
                UiOutcome::Retry
            }
            LoadState::Ready => {
                let Some(item) = self.focused_item().cloned() else {
                    return UiOutcome::Rejected(UiReject::NoSelection);
                };
                self.play_item(&item)
            }
        }
    }

    fn play_item(&mut self, item: &MediaItem) -> UiOutcome {
        let valid = match item.source {
            Source::Jellyfin => is_valid_item_id(&item.id),
            Source::YouTube => is_valid_youtube_id(&item.id),
            Source::AirPlay | Source::WebRtc => false,
        };
        if !valid {
            self.status = format!("Invalid {} item identifier", source_label(item.source));
            return UiOutcome::Rejected(UiReject::InvalidItem);
        }

        self.library.searching = false;
        self.library.query.clear();
        self.library.refresh_selection();
        self.screen = Screen::Player;
        self.player.title = item.title.clone();
        self.player.source = Some(item.source);
        self.player.position_ms = 0;
        self.player.duration_ms = None;
        self.player.paused = false;
        self.player.loading = true;
        self.player.error = None;
        self.player_focus = 0;
        self.status = format!("Opening {} ({})", item.title, item.source_label());

        let action = match item.source {
            Source::Jellyfin => Action::PlayJellyfin {
                item_id: item.id.clone(),
            },
            Source::YouTube => Action::PlayYoutube {
                video_id: item.id.clone(),
            },
            Source::AirPlay | Source::WebRtc => unreachable!("validated above"),
        };
        UiOutcome::Action(action)
    }

    fn handle_search_key(&mut self, key: Key) -> UiOutcome {
        match key {
            Key::Escape => {
                self.end_search();
                UiOutcome::Dismissed
            }
            Key::Backspace => {
                self.library.query.pop();
                self.library.select_first();
                self.refresh_status();
                self.navigated()
            }
            Key::Char(character) if !character.is_control() => {
                self.push_query_char(character);
                self.navigated()
            }
            Key::Up => {
                self.library.focus = self.library.focus.saturating_sub(1);
                self.sync_focus_selection();
                self.focus_status();
                self.navigated()
            }
            Key::Down => {
                let last = self.visible_len().saturating_sub(1);
                self.library.focus = (self.library.focus + 1).min(last);
                self.sync_focus_selection();
                self.focus_status();
                self.navigated()
            }
            Key::Left => {
                self.set_library_filter(self.library.filter.previous());
                self.navigated()
            }
            Key::Right => {
                self.set_library_filter(self.library.filter.next());
                self.navigated()
            }
            Key::Enter => {
                let outcome = self.activate_library();
                if !matches!(outcome, UiOutcome::Ignored) {
                    self.library.searching = false;
                    self.library.query.clear();
                    self.library.select_first();
                }
                outcome
            }
            Key::Home => {
                self.end_search();
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Home,
                })
            }
            Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn push_query_char(&mut self, character: char) {
        if self.library.query.chars().count() >= MAX_QUERY_CHARS {
            return;
        }
        self.library.query.push(character);
        self.library.select_first();
        self.refresh_status();
    }

    fn handle_player_key(&mut self, key: Key) -> UiOutcome {
        match key {
            Key::Up => {
                self.player_focus = (self.player_focus + PLAYER_CONTROLS - 1) % PLAYER_CONTROLS;
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Up,
                })
            }
            Key::Left => {
                self.player_focus = (self.player_focus + PLAYER_CONTROLS - 1) % PLAYER_CONTROLS;
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Left,
                })
            }
            Key::Down => {
                self.player_focus = (self.player_focus + 1) % PLAYER_CONTROLS;
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Down,
                })
            }
            Key::Right => {
                self.player_focus = (self.player_focus + 1) % PLAYER_CONTROLS;
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Right,
                })
            }
            Key::Enter => self.activate_player_control(),
            Key::Escape => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Back,
                })
            }
            Key::Home => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Home,
                })
            }
            Key::Char(' ') => UiOutcome::Action(Action::SetPaused {
                paused: !self.player.paused,
            }),
            Key::Backspace | Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn activate_player_control(&mut self) -> UiOutcome {
        match self.player_focus {
            0 => UiOutcome::Action(Action::SetPaused {
                paused: !self.player.paused,
            }),
            1 => {
                self.go_home();
                UiOutcome::Action(Action::Stop)
            }
            _ => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Back,
                })
            }
        }
    }

    fn handle_settings_key(&mut self, key: Key) -> UiOutcome {
        match key {
            Key::Up => {
                self.settings_focus = self.settings_focus.saturating_sub(1);
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Up,
                })
            }
            Key::Down => {
                self.settings_focus = (self.settings_focus + 1).min(SETTINGS_ROWS.len() - 1);
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Down,
                })
            }
            Key::Left => UiOutcome::Action(Action::Navigate {
                direction: Direction::Left,
            }),
            Key::Right => UiOutcome::Action(Action::Navigate {
                direction: Direction::Right,
            }),
            Key::Enter => self.activate_settings(),
            Key::Escape => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Back,
                })
            }
            Key::Home => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Home,
                })
            }
            Key::Backspace | Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn activate_settings(&mut self) -> UiOutcome {
        match self.settings_focus {
            0 => {
                self.status = "Video follows the 720p/1080p compositor mode".to_owned();
                self.navigated()
            }
            1 => {
                self.status = "Audio follows the HDMI sink selected by the compositor".to_owned();
                self.navigated()
            }
            2 => {
                self.status = "tv-client is licensed GPL-3.0-only".to_owned();
                self.navigated()
            }
            _ => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Back,
                })
            }
        }
    }

    fn handle_cast_key(&mut self, key: Key) -> UiOutcome {
        match key {
            Key::Escape | Key::Enter => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Back,
                })
            }
            Key::Home => {
                self.go_home();
                UiOutcome::Action(Action::Navigate {
                    direction: Direction::Home,
                })
            }
            Key::Up | Key::Down | Key::Left | Key::Right => UiOutcome::Action(Action::Navigate {
                direction: match key {
                    Key::Up => Direction::Up,
                    Key::Down => Direction::Down,
                    Key::Left => Direction::Left,
                    _ => Direction::Right,
                },
            }),
            Key::Backspace | Key::Char(_) => UiOutcome::Ignored,
        }
    }

    fn sync_focus_selection(&mut self) {
        if let Some(item) = self.library.visible().get(self.library.focus) {
            self.library.selected = Some(item.id.clone());
        }
    }

    fn focus_status(&mut self) {
        if let Some(item) = self.focused_item() {
            self.status = format!("{} [{}]", item.title, item.source_label());
        }
    }

    fn go_home(&mut self) {
        self.screen = Screen::Home;
        self.library.searching = false;
        self.library.query.clear();
        self.status = "Choose a source".to_owned();
    }

    fn navigated(&self) -> UiOutcome {
        UiOutcome::Navigated {
            screen: self.screen,
            focus: self.current_focus(),
        }
    }

    fn current_focus(&self) -> usize {
        match self.screen {
            Screen::Home => self.home_focus,
            Screen::Library => self.library.focus,
            Screen::Player => self.player_focus,
            Screen::Settings => self.settings_focus,
            Screen::Cast => 0,
        }
    }

    fn refresh_status(&mut self) {
        if self.modal.is_some() {
            self.status = "Pairing confirmation required".to_owned();
            return;
        }
        self.status = match self.screen {
            Screen::Home => "Choose a source".to_owned(),
            Screen::Library => self.library_status(),
            Screen::Player => {
                if let Some(error) = &self.player.error {
                    format!("Playback failed: {error}")
                } else if self.player.loading {
                    format!("Opening {}", self.player.title)
                } else if self.player.paused {
                    format!("Paused: {}", self.player.title)
                } else {
                    format!("Playing: {}", self.player.title)
                }
            }
            Screen::Settings => "Settings".to_owned(),
            Screen::Cast => self.cast_state.clone(),
        };
    }

    fn library_status(&self) -> String {
        let filter = self.library.filter.label();
        match &self.library.load {
            LoadState::Idle => "Choose a source".to_owned(),
            LoadState::Loading => format!("Loading {filter}"),
            LoadState::Error { message } => format!("{filter} could not load: {message}"),
            LoadState::Empty => {
                if self.library.query.is_empty() {
                    format!("No items in {filter}")
                } else {
                    format!("No results for \"{}\" in {filter}", self.library.query)
                }
            }
            LoadState::Ready => {
                let count = self.visible_len();
                if self.library.query.is_empty() {
                    format!("{count} items in {filter}")
                } else {
                    format!("{count} results for \"{}\" in {filter}", self.library.query)
                }
            }
        }
    }
}

/// Number of player controls: pause, stop, back.
const PLAYER_CONTROLS: usize = 3;

/// Settings rows, in display order.
pub const SETTINGS_ROWS: [&str; 4] = [
    "Video: 720p / 1080p auto",
    "Audio: HDMI (compositor)",
    "Licence: GPL-3.0-only",
    "Back to home",
];

/// Package version rendered on the settings screen.
pub const VERSION_LABEL: &str = concat!("tv-client ", env!("CARGO_PKG_VERSION"));

/// Translate the text field of a Slint `KeyEvent` into a launcher key.
///
/// Slint uses private-use code points (`U+F700..=U+F7FF`) for named keys.
/// Named keys without a launcher action are rejected here so they can never
/// become search text; only printable characters produce [`Key::Char`].
#[must_use]
pub fn key_from_text(text: &str) -> Option<Key> {
    Some(match text {
        "\u{000a}" | "\r" => Key::Enter,
        "\u{001b}" => Key::Escape,
        "\u{0008}" | "\u{007f}" => Key::Backspace,
        "\u{F700}" => Key::Up,
        "\u{F701}" => Key::Down,
        "\u{F702}" => Key::Left,
        "\u{F703}" => Key::Right,
        "\u{F729}" => Key::Home,
        _ => {
            let mut characters = text.chars();
            let character = characters.next()?;
            if characters.next().is_some()
                || character.is_control()
                || ('\u{F700}'..='\u{F7FF}').contains(&character)
            {
                return None;
            }
            Key::Char(character)
        }
    })
}

fn format_time(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn sanitize(value: &str, limit: usize, fallback: &str) -> String {
    let filtered: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(limit)
        .collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(feature = "native-ui")]
pub use native::{NativeUi, NativeUiError, UiCommand, UiHandle};

#[cfg(feature = "native-ui")]
mod native {
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use slint::{ComponentHandle, ModelRc, VecModel};
    use thiserror::Error;
    use uuid::Uuid;

    use super::{
        Key, LoadState, MediaItem, PairChoice, Screen, UiInput, UiModel, UiOutcome, UiReject,
        VERSION_LABEL, key_from_text, source_label,
    };
    use crate::model::Action;

    slint::include_modules!();

    /// A command emitted by local UI input for the owning controller.
    ///
    /// There is deliberately no variant for remote approval: only the local
    /// keyboard path can produce [`Self::ApprovePairing`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum UiCommand {
        /// Forward a validated protocol action to the reducer.
        Action(Action),
        /// A local operator approved a pending pairing request.
        ApprovePairing { request_id: Uuid },
        /// The operator asked to retry the current library load.
        Retry,
        /// The operator opened a library source; the controller should fetch it.
        LoadLibrary { filter: super::SourceFilter },
    }

    /// Errors starting or running the native window.
    #[derive(Debug, Error)]
    pub enum NativeUiError {
        /// The windowing backend or event loop failed.
        #[error("native UI failed: {0}")]
        Platform(#[from] slint::PlatformError),
    }

    /// Weak, thread-safe handle for pushing reducer state into the window.
    ///
    /// All window mutation is queued on the Slint event loop; the handle never
    /// blocks the caller and never performs I/O.
    #[derive(Clone)]
    pub struct UiHandle {
        window: slint::Weak<AppWindow>,
        model: Arc<Mutex<UiModel>>,
    }

    impl UiHandle {
        /// Post a reducer snapshot to the event loop.
        pub fn apply_state(&self, state: crate::model::AppState) {
            self.post(move |model| model.apply_state(&state));
        }

        /// Replace the library contents from a completed fetch.
        pub fn set_library_items(&self, items: Vec<MediaItem>) {
            self.post(move |model| model.set_library_items(items));
        }

        /// Mark the library as loading from a controller thread.
        pub fn set_library_loading(&self) {
            self.post(UiModel::set_library_loading);
        }

        /// Report a bounded library fetch error.
        pub fn set_library_error(&self, message: String) {
            self.post(move |model| model.set_library_error(&message));
        }

        /// Present a wire pairing request for local confirmation.
        pub fn present_pairing_request(&self, request_id: Uuid) {
            self.post(move |model| {
                let _ = model.present_pairing_request(request_id);
            });
        }

        fn post(&self, update: impl FnOnce(&mut UiModel) + Send + 'static) {
            let model = Arc::clone(&self.model);
            let _ = self.window.upgrade_in_event_loop(move |window| {
                let mut model = lock_model(&model);
                update(&mut model);
                refresh(&window, &model);
            });
        }
    }

    /// A constructed window, ready to run.
    pub struct NativeUi {
        window: AppWindow,
        model: Arc<Mutex<UiModel>>,
    }

    impl NativeUi {
        /// Build the window and install input callbacks.
        ///
        /// The callback runs on the Slint event loop and must not block.
        pub fn new<F>(
            initial_state: crate::model::AppState,
            on_command: F,
        ) -> Result<Self, NativeUiError>
        where
            F: Fn(UiCommand) + Send + Sync + 'static,
        {
            let window = AppWindow::new()?;
            let model = Arc::new(Mutex::new(UiModel::new()));
            {
                let mut model_guard = lock_model(&model);
                model_guard.apply_state(&initial_state);
                refresh(&window, &model_guard);
            }
            let on_command = Arc::new(on_command);
            let start = Instant::now();

            {
                let weak = window.as_weak();
                let model = Arc::clone(&model);
                let on_command = Arc::clone(&on_command);
                window.on_key_pressed(move |text, _repeat| {
                    let Some(key) = key_from_text(text.as_str()) else {
                        return;
                    };
                    with_window(&weak, &model, |window, model| {
                        handle_local_input(window, model, key, start.elapsed(), &on_command);
                    });
                });
            }

            install_pointer_callbacks(&window, &model, &on_command, start);

            Ok(Self { window, model })
        }

        /// Weak handle for a controller running on another thread.
        #[must_use]
        pub fn handle(&self) -> UiHandle {
            UiHandle {
                window: self.window.as_weak(),
                model: Arc::clone(&self.model),
            }
        }

        /// Show the window, enter fullscreen, and run until it closes.
        ///
        /// Fullscreen is requested after `show()`; requesting it before the
        /// window is mapped is ignored by some Wayland compositors.
        pub fn run(self) -> Result<(), NativeUiError> {
            self.window.show()?;
            // The kiosk target is a 1080p panel. Set an explicit physical size
            // as a fallback for compositors that ignore the fullscreen request.
            self.window
                .window()
                .set_size(slint::PhysicalSize::new(1920, 1080));
            let weak = self.window.as_weak();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.window().set_fullscreen(true);
                }
            });
            slint::run_event_loop()?;
            Ok(())
        }
    }

    fn install_pointer_callbacks(
        window: &AppWindow,
        model: &Arc<Mutex<UiModel>>,
        on_command: &Arc<impl Fn(UiCommand) + Send + Sync + 'static>,
        start: Instant,
    ) {
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_home_activated(move |index| {
                with_window(&weak, &model, |window, model| {
                    model.set_home_focus(index.max(0) as usize);
                    handle_local_input(window, model, Key::Enter, start.elapsed(), &on_command);
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_library_activated(move |index| {
                with_window(&weak, &model, |window, model| {
                    if model.select_library(index.max(0) as usize) {
                        handle_local_input(window, model, Key::Enter, start.elapsed(), &on_command);
                    }
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_library_retry_requested(move || {
                with_window(&weak, &model, |window, model| {
                    handle_local_input(window, model, Key::Enter, start.elapsed(), &on_command);
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            window.on_library_search_requested(move || {
                with_window(&weak, &model, |window, model| {
                    model.begin_search();
                    refresh(window, model);
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_player_control_activated(move |index| {
                with_window(&weak, &model, |window, model| {
                    model.set_player_focus(index.max(0) as usize);
                    handle_local_input(window, model, Key::Enter, start.elapsed(), &on_command);
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_settings_activated(move |index| {
                with_window(&weak, &model, |window, model| {
                    model.set_settings_focus(index.max(0) as usize);
                    handle_local_input(window, model, Key::Enter, start.elapsed(), &on_command);
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_pair_choice_activated(move |index| {
                with_window(&weak, &model, |window, model| {
                    model.focus_pair_choice(index.max(0) as usize);
                    handle_local_input(window, model, Key::Enter, start.elapsed(), &on_command);
                });
            });
        }
        {
            let weak = window.as_weak();
            let model = Arc::clone(model);
            let on_command = Arc::clone(on_command);
            window.on_back_requested(move || {
                with_window(&weak, &model, |window, model| {
                    handle_local_input(window, model, Key::Escape, start.elapsed(), &on_command);
                });
            });
        }
    }

    fn handle_local_input<F>(
        window: &AppWindow,
        model: &mut UiModel,
        key: Key,
        now: Duration,
        on_command: &Arc<F>,
    ) where
        F: Fn(UiCommand) + Send + Sync + 'static,
    {
        let was_loading = model.is_library_loading();
        let outcome = model.handle_key(key, now);
        dispatch(on_command, outcome);
        if let Some(filter) = model.pending_library_load(was_loading) {
            (**on_command)(UiCommand::LoadLibrary { filter });
        }
        refresh(window, model);
    }

    fn dispatch<F>(on_command: &Arc<F>, outcome: UiOutcome)
    where
        F: Fn(UiCommand) + Send + Sync + 'static,
    {
        match outcome {
            UiOutcome::Action(action) => (**on_command)(UiCommand::Action(action)),
            UiOutcome::ApprovePairing { request_id } => {
                (**on_command)(UiCommand::ApprovePairing { request_id });
            }
            UiOutcome::Retry => (**on_command)(UiCommand::Retry),
            UiOutcome::Rejected(UiReject::RemoteApprovalDenied) => {
                tracing::warn!("remote input attempted to confirm a local action");
            }
            UiOutcome::Dismissed
            | UiOutcome::Navigated { .. }
            | UiOutcome::Throttled
            | UiOutcome::Ignored
            | UiOutcome::Rejected(_) => {}
        }
    }

    fn lock_model(model: &Arc<Mutex<UiModel>>) -> MutexGuard<'_, UiModel> {
        model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn with_window(
        weak: &slint::Weak<AppWindow>,
        model: &Arc<Mutex<UiModel>>,
        action: impl FnOnce(&AppWindow, &mut UiModel),
    ) {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let mut model = lock_model(model);
        action(&window, &mut model);
    }

    fn refresh(window: &AppWindow, model: &UiModel) {
        window.set_screen(model.screen().index());
        window.set_status_text(model.status_text().into());
        window.set_home_focus(model.home_focus() as i32);
        window.set_library_focus(model.library_focus() as i32);
        window.set_settings_focus(model.settings_focus() as i32);
        window.set_player_focus(model.player_focus() as i32);
        window.set_filter_label(model.library_filter().label().into());
        window.set_search_active(model.searching());
        window.set_query(model.library_query().into());

        let load = model.library_load();
        window.set_library_loading(matches!(load, LoadState::Loading));
        window.set_library_empty(matches!(load, LoadState::Empty));
        window.set_library_error(matches!(load, LoadState::Error { .. }));
        window.set_library_error_text(model.library_error_text().into());

        let cards: Vec<MediaCard> = model
            .visible_items()
            .into_iter()
            .enumerate()
            .map(|(index, item)| MediaCard {
                title: item.title.as_str().into(),
                subtitle: item.subtitle.as_str().into(),
                source: source_label(item.source).into(),
                index: index as i32,
                focused: index == model.library_focus(),
            })
            .collect();
        window.set_library_items(ModelRc::new(VecModel::from(cards)));

        window.set_player_title(model.player_title().into());
        window.set_player_source(model.player_source_label().into());
        window.set_player_position(model.player_position_text().into());
        window.set_player_progress(model.player_progress());
        window.set_player_paused(model.player_paused());
        window.set_player_loading(model.player_loading());
        window.set_cast_state(model.cast_state().into());
        window.set_settings_version(VERSION_LABEL.into());

        let prompt = model.pair_prompt();
        window.set_pair_visible(prompt.is_some());
        window.set_pair_message(prompt.map_or("", |prompt| prompt.message()).into());
        window.set_pair_choice(
            prompt
                .map_or(PairChoice::Deny, |prompt| prompt.choice)
                .index(),
        );
    }

    // Keep the screen enum and item type exercised from the bridge so that a
    // future controller has one obvious place to convert them.
    #[allow(dead_code)]
    const fn screen_of(screen: Screen) -> i32 {
        screen.index()
    }

    #[allow(dead_code)]
    fn item_source_label(item: &MediaItem) -> &'static str {
        item.source_label()
    }

    #[allow(dead_code)]
    fn input_is_local(input: &UiInput) -> bool {
        match input {
            UiInput::LocalKey { origin, .. } => origin.is_local(),
            UiInput::RemoteAction(_) => false,
        }
    }
}
