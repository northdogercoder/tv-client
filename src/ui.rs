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
use crate::playback::{LibraryFilter, PlaybackUpdate};

/// Number of entries on the home screen.
pub const HOME_ROW_COUNT: usize = 5;
/// Maximum number of library entries retained by the view model.
pub const MAX_LIBRARY_ITEMS: usize = 64;
/// Maximum search query length in characters.
pub const MAX_QUERY_CHARS: usize = 48;
/// Maximum error text length in characters.
pub const MAX_ERROR_CHARS: usize = 160;
/// Minimum interval between accepted repeats of a navigation key.
pub const REPEAT_INTERVAL: Duration = crate::input::REPEAT_INTERVAL;

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

    /// Convert a physical input key into a launcher key.
    ///
    /// `PlayPause` is handled by [`UiModel::handle_event`] only on the player
    /// screen and volume keys have no launcher representation; both return
    /// `None` so neither can become a printable character.
    #[must_use]
    pub fn from_input(key: crate::input::Key) -> Option<Self> {
        Some(match key {
            crate::input::Key::Up => Self::Up,
            crate::input::Key::Down => Self::Down,
            crate::input::Key::Left => Self::Left,
            crate::input::Key::Right => Self::Right,
            crate::input::Key::Select => Self::Enter,
            crate::input::Key::Back => Self::Escape,
            crate::input::Key::Home => Self::Home,
            crate::input::Key::Char(character) => Self::Char(character),
            crate::input::Key::Backspace => Self::Backspace,
            crate::input::Key::PlayPause
            | crate::input::Key::VolumeUp
            | crate::input::Key::VolumeDown
            | crate::input::Key::Mute => return None,
        })
    }

    /// Convert a launcher key into a physical input key.
    #[must_use]
    pub const fn into_input(self) -> crate::input::Key {
        match self {
            Self::Up => crate::input::Key::Up,
            Self::Down => crate::input::Key::Down,
            Self::Left => crate::input::Key::Left,
            Self::Right => crate::input::Key::Right,
            Self::Enter => crate::input::Key::Select,
            Self::Escape => crate::input::Key::Back,
            Self::Home => crate::input::Key::Home,
            Self::Backspace => crate::input::Key::Backspace,
            Self::Char(character) => crate::input::Key::Char(character),
        }
    }
}

/// Maximum continue-watching entries retained by the home screen.
pub const MAX_CONTINUE_WATCHING: usize = 8;
/// Largest accepted continue-watching title.
const MAX_RESUME_TITLE_CHARS: usize = 80;

/// Controller command emitted by local player input.
///
/// These are deliberately separate from wire [`Action`] values: they need
/// playback negotiation state that only the controller owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiPlaybackCommand {
    /// Retry the last failed playback.
    Retry,
    /// Play the next episode of the current series.
    NextEpisode,
    /// Select an audio stream by index.
    SelectAudio {
        /// Stream index, or `None` for the server default.
        index: Option<i32>,
    },
    /// Select or disable subtitles.
    SelectSubtitle {
        /// Stream index, or `None` to disable subtitles.
        index: Option<i32>,
    },
}

/// Player menu overlay currently owned by the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerMenu {
    /// Subtitle track chooser.
    Subtitles,
    /// Audio track chooser.
    Audio,
}

impl PlayerMenu {
    /// Stable label used by the Slint overlay.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Subtitles => "Subtitles",
            Self::Audio => "Audio",
        }
    }
}

/// One player track row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiTrack {
    /// Stream index.
    pub index: i32,
    /// Display label.
    pub label: String,
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

fn library_source_filter(filter: LibraryFilter) -> SourceFilter {
    match filter {
        LibraryFilter::Jellyfin => SourceFilter::Jellyfin,
        LibraryFilter::ContinueWatching => SourceFilter::All,
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
    /// The Slint event loop is no longer available.
    #[error("the user interface is unavailable")]
    Unavailable,
}

/// Result of feeding one input event to the view model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiOutcome {
    /// Send this protocol action to the reducer.
    Action(Action),
    /// Approve a pending pairing request from local input.
    ApprovePairing { request_id: Uuid },
    /// Deny a pending pairing request from local input.
    DenyPairing { request_id: Uuid },
    /// Start a pairing-code request from the local settings screen.
    StartPairing,
    /// Ask the owner to retry the current library load.
    Retry,
    /// Ask the controller to act on the active playback.
    Playback(UiPlaybackCommand),
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
    /// Server comparison code, already reduced to four ASCII digits.
    pub comparison_code: Option<String>,
}

impl PairPrompt {
    /// Fixed prompt text. It never interpolates remote-controlled strings.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        "A device wants to pair with this TV. Approve only if you started pairing."
    }

    /// Text shown by the TV dialog.
    ///
    /// The comparison code is not remote free-form text: callers must pass a
    /// four-digit value through [`UiModel::present_pairing_request_with_code`].
    #[must_use]
    pub fn display_text(&self) -> String {
        match self.comparison_code.as_deref() {
            Some(code) => format!("{}\n\nComparison code: {code}", self.message()),
            None => self.message().to_owned(),
        }
    }
}

/// Repeat gate for held navigation keys.
///
/// The interval and slot table live in [`crate::input`] so the keyboard and
/// CEC paths cannot drift.
#[derive(Debug, Clone, Default)]
struct RepeatThrottle {
    gate: crate::input::RepeatGate,
}

impl RepeatThrottle {
    fn accept(&mut self, key: Key, now: Duration) -> bool {
        let slot = match key {
            Key::Up => 0,
            Key::Down => 1,
            Key::Left => 2,
            Key::Right => 3,
            Key::Backspace => 4,
            Key::Enter | Key::Escape | Key::Home | Key::Char(_) => return true,
        };
        self.gate.accept(slot, now)
    }
}

/// Player-screen projection of the reducer state.
#[derive(Debug, Clone)]
struct PlayerView {
    title: String,
    source: Option<Source>,
    item_id: Option<String>,
    method: String,
    position_ms: u64,
    duration_ms: Option<u64>,
    paused: bool,
    loading: bool,
    error: Option<String>,
    audio_tracks: Vec<UiTrack>,
    subtitle_tracks: Vec<UiTrack>,
    selected_audio: Option<i32>,
    selected_subtitle: Option<i32>,
    next_episode: Option<MediaItem>,
    retry_available: bool,
    menu: Option<PlayerMenu>,
    menu_focus: usize,
}

impl Default for PlayerView {
    fn default() -> Self {
        Self {
            title: "Nothing playing".to_owned(),
            source: None,
            item_id: None,
            method: String::new(),
            position_ms: 0,
            duration_ms: None,
            paused: false,
            loading: false,
            error: None,
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            selected_audio: None,
            selected_subtitle: None,
            next_episode: None,
            retry_available: false,
            menu: None,
            menu_focus: 0,
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
    cloud_status: String,
    pair_code: String,
    volume: u8,
    modal: Option<PairPrompt>,
    throttle: RepeatThrottle,
    resume: Option<MediaItem>,
    continue_watching: Vec<MediaItem>,
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
            cloud_status: String::new(),
            pair_code: String::new(),
            volume: crate::model::DEFAULT_VOLUME,
            modal: None,
            throttle: RepeatThrottle::default(),
            resume: None,
            continue_watching: Vec::new(),
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
    ///
    /// An explicitly supplied resume item wins; otherwise the first
    /// continue-watching entry is offered.
    #[must_use]
    pub fn resume_item(&self) -> Option<&MediaItem> {
        self.resume
            .as_ref()
            .or_else(|| self.continue_watching.first())
    }

    /// Set or clear the explicit resume item.
    pub fn set_resume(&mut self, item: Option<MediaItem>) {
        self.resume = item;
    }

    /// Bounded continue-watching entries from the controller.
    #[must_use]
    pub fn continue_watching(&self) -> &[MediaItem] {
        &self.continue_watching
    }

    /// Replace continue-watching entries, bounded and title-sanitized.
    pub fn set_continue_watching(&mut self, items: Vec<MediaItem>) {
        self.continue_watching = items
            .into_iter()
            .take(MAX_CONTINUE_WATCHING)
            .map(|mut item| {
                item.title = sanitize(&item.title, MAX_RESUME_TITLE_CHARS, "Continue watching");
                item
            })
            .collect();
        self.refresh_status();
    }

    /// Label for the resume row, or an empty string when nothing is queued.
    #[must_use]
    pub fn resume_title(&self) -> &str {
        self.resume_item()
            .map(|item| item.title.as_str())
            .unwrap_or_default()
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

    /// Negotiated play method text, for example `Direct play`.
    #[must_use]
    pub fn player_method(&self) -> &str {
        &self.player.method
    }

    /// Audio tracks offered by the controller.
    #[must_use]
    pub fn audio_tracks(&self) -> &[UiTrack] {
        &self.player.audio_tracks
    }

    /// Subtitle tracks offered by the controller.
    #[must_use]
    pub fn subtitle_tracks(&self) -> &[UiTrack] {
        &self.player.subtitle_tracks
    }

    /// Selected audio stream index.
    #[must_use]
    pub const fn selected_audio(&self) -> Option<i32> {
        self.player.selected_audio
    }

    /// Selected subtitle stream index.
    #[must_use]
    pub const fn selected_subtitle(&self) -> Option<i32> {
        self.player.selected_subtitle
    }

    /// Next episode offered after a natural end.
    #[must_use]
    pub fn next_episode(&self) -> Option<&MediaItem> {
        self.player.next_episode.as_ref()
    }

    /// Whether the error screen offers a retry.
    #[must_use]
    pub const fn retry_available(&self) -> bool {
        self.player.retry_available
    }

    /// Open player track menu, if any.
    #[must_use]
    pub const fn player_menu(&self) -> Option<PlayerMenu> {
        self.player.menu
    }

    /// Focused row inside the open track menu.
    #[must_use]
    pub const fn player_menu_focus(&self) -> usize {
        self.player.menu_focus
    }

    /// Rows of the open menu. The first row disables subtitles when the menu
    /// is the subtitle chooser.
    #[must_use]
    pub fn player_menu_rows(&self) -> Vec<UiTrack> {
        let Some(menu) = self.player.menu else {
            return Vec::new();
        };
        let tracks = match menu {
            PlayerMenu::Subtitles => &self.player.subtitle_tracks,
            PlayerMenu::Audio => &self.player.audio_tracks,
        };
        let mut rows = Vec::with_capacity(tracks.len() + 1);
        if menu == PlayerMenu::Subtitles {
            rows.push(UiTrack {
                index: -1,
                label: "Off".to_string(),
            });
        }
        rows.extend(tracks.iter().cloned());
        rows
    }

    /// Install tracks and selections reported by the controller.
    pub fn set_player_tracks(
        &mut self,
        audio: Vec<UiTrack>,
        subtitles: Vec<UiTrack>,
        selected_audio: Option<i32>,
        selected_subtitle: Option<i32>,
    ) {
        self.player.audio_tracks = audio;
        self.player.subtitle_tracks = subtitles;
        self.player.selected_audio = selected_audio;
        self.player.selected_subtitle = selected_subtitle;
        self.player.menu = None;
        self.player.menu_focus = 0;
        self.refresh_status();
    }

    /// Set the next episode offered by the controller.
    pub fn set_next_episode(&mut self, item: Option<MediaItem>) {
        self.player.next_episode = item;
    }

    /// Set whether the error state offers retry.
    pub const fn set_retry_available(&mut self, available: bool) {
        self.player.retry_available = available;
    }

    /// Set the negotiated play method label.
    pub fn set_player_method(&mut self, method: &str) {
        self.player.method = sanitize(method, 32, "");
    }

    /// Move the open track-menu focus.
    pub fn set_player_menu_focus(&mut self, index: usize) {
        let rows = self.player_menu_rows().len();
        if rows > 0 {
            self.player.menu_focus = index.min(rows - 1);
        }
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

    /// Eight-digit pairing code to show on the TV, or empty when inactive.
    #[must_use]
    pub fn pairing_code(&self) -> &str {
        &self.pair_code
    }

    /// Display a server-issued pairing code on the TV.
    ///
    /// The code is validated as eight ASCII digits before it can reach the
    /// declarative UI, so a server cannot inject layout or markup.
    pub fn set_pairing_code(&mut self, code: &str) -> Result<(), UiReject> {
        if code.len() != 8 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(UiReject::InvalidRequest);
        }
        self.pair_code = code.to_owned();
        Ok(())
    }

    /// Remove the pairing code from the TV.
    pub fn clear_pairing_code(&mut self) {
        self.pair_code.clear();
    }

    /// Dismiss the confirmation dialog when it belongs to `request_id`.
    ///
    /// Used when the server reports a terminal pairing status, the request
    /// expires or the device is revoked. A stale dialog must never remain
    /// available for a decision that can no longer be submitted.
    pub fn dismiss_pairing_request(&mut self, request_id: Uuid) {
        if self
            .modal
            .as_ref()
            .is_some_and(|prompt| prompt.request_id == request_id)
        {
            self.modal = None;
            self.refresh_status();
        }
    }

    /// Clear a previous playback error before retrying a remote play command.
    ///
    /// The stale source and position are removed as well: otherwise an old
    /// `playing` snapshot for the same item could satisfy the retry wait
    /// before the queued play command has actually started.
    pub fn clear_player_error(&mut self) {
        if self.player.error.is_some() {
            self.player.error = None;
            self.player.loading = false;
            self.player.source = None;
            self.player.item_id = None;
            self.player.paused = false;
            self.player.position_ms = 0;
            self.player.duration_ms = None;
        }
        self.refresh_status();
    }

    /// Persistent cloud status shown in the header.
    #[must_use]
    pub fn cloud_status(&self) -> &str {
        &self.cloud_status
    }

    /// Update the persistent cloud status line.
    pub fn set_cloud_status(&mut self, status: &str) {
        self.cloud_status = sanitize(status, 80, "");
    }

    /// Project the current presentation state for cloud publication.
    ///
    /// This never includes a media URL. The caller adds the device id and
    /// applies `share_titles`.
    #[must_use]
    pub fn snapshot(&self) -> AppState {
        let mode = if self.player.error.is_some() {
            Mode::Error
        } else if self.player.loading {
            Mode::Loading
        } else if self.player.source.is_some() {
            if self.player.paused {
                Mode::Paused
            } else {
                Mode::Playing
            }
        } else {
            match self.screen {
                Screen::Library | Screen::Settings => Mode::Browsing,
                Screen::Cast => Mode::Casting,
                Screen::Home | Screen::Player => Mode::Home,
            }
        };
        let playing = self.player.source.is_some();
        AppState {
            device_id: None,
            mode,
            source: self.player.source,
            item_id: self.player.item_id.clone(),
            title: if playing {
                Some(self.player.title.clone())
            } else {
                None
            },
            position_ms: playing.then_some(self.player.position_ms),
            duration_ms: self.player.duration_ms,
            volume: self.volume,
            paused: self.player.paused,
            error: self
                .player
                .error
                .as_ref()
                .map(|_| "playback_error".to_owned()),
            playback: None,
        }
    }

    /// Apply a wire navigation action through the local view-model path.
    ///
    /// Only navigation is represented here; playback actions travel through
    /// the playback command channel. The pairing dialog is never reachable
    /// from this method.
    pub fn handle_remote_action(&mut self, action: Action, now: Duration) -> UiOutcome {
        if self.modal.is_some() {
            return UiOutcome::Rejected(UiReject::RemoteApprovalDenied);
        }
        let Action::Navigate { direction } = action else {
            return UiOutcome::Ignored;
        };
        if direction == Direction::Select && self.screen == Screen::Settings {
            return UiOutcome::Ignored;
        }
        let key = match direction {
            Direction::Up => Key::Up,
            Direction::Down => Key::Down,
            Direction::Left => Key::Left,
            Direction::Right => Key::Right,
            Direction::Select => Key::Enter,
            Direction::Back => Key::Escape,
            Direction::Home => Key::Home,
        };
        self.handle_local_key(key, now)
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
        self.present_pairing_request_with_code(request_id, None)
    }

    /// Present a pairing request together with the server comparison code.
    ///
    /// The code must be exactly four ASCII digits; anything else is refused
    /// instead of being rendered.
    pub fn present_pairing_request_with_code(
        &mut self,
        request_id: Uuid,
        comparison_code: Option<&str>,
    ) -> Result<(), UiReject> {
        if request_id.is_nil() {
            return Err(UiReject::InvalidRequest);
        }
        if let Some(code) = comparison_code
            && (code.len() != 4 || !code.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(UiReject::InvalidRequest);
        }
        if self.modal.is_some() {
            return Err(UiReject::ConfirmationBusy);
        }
        self.modal = Some(PairPrompt {
            request_id,
            choice: PairChoice::Deny,
            comparison_code: comparison_code.map(str::to_owned),
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
        self.player.item_id = state.item_id.clone();
        self.player.position_ms = state.position_ms.unwrap_or(0);
        self.volume = state.volume;
        self.player.duration_ms = state.duration_ms;
        self.player.paused = state.paused;

        match state.mode {
            Mode::Home => {
                if matches!(self.screen, Screen::Player | Screen::Cast) {
                    self.screen = Screen::Home;
                }
                self.player.loading = false;
                self.player.error = None;
                self.player.menu = None;
                self.player.menu_focus = 0;
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
                self.player.menu = None;
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

    /// Project a controller update onto the player screen.
    ///
    /// This performs no I/O; the controller runs on its own thread and posts
    /// updates through [`UiHandle`] on the Slint event loop.
    pub fn apply_playback_update(&mut self, update: &PlaybackUpdate) {
        match update {
            PlaybackUpdate::Loading { item_id, title } => {
                self.screen = Screen::Player;
                self.player.title = title.clone().unwrap_or_else(|| format!("Item {item_id}"));
                self.player.source = Some(Source::Jellyfin);
                self.player.item_id = Some(item_id.clone());
                self.player.method.clear();
                self.player.position_ms = 0;
                self.player.duration_ms = None;
                self.player.paused = false;
                self.player.loading = true;
                self.player.error = None;
                self.player.audio_tracks.clear();
                self.player.subtitle_tracks.clear();
                self.player.selected_audio = None;
                self.player.selected_subtitle = None;
                self.player.next_episode = None;
                self.player.retry_available = false;
                self.player.menu = None;
                self.player_focus = 0;
                self.status = format!("Opening {}", self.player.title);
            }
            PlaybackUpdate::Position {
                item_id,
                position_ms,
                duration_ms,
                paused,
            } => {
                self.screen = Screen::Player;
                self.player.item_id = Some(item_id.clone());
                self.player.loading = false;
                self.player.error = None;
                self.player.position_ms = *position_ms;
                self.player.duration_ms = *duration_ms;
                self.player.paused = *paused;
                self.refresh_status();
            }
            PlaybackUpdate::Method { method } => {
                self.player.method = method.label().to_string();
                self.refresh_status();
            }
            PlaybackUpdate::Tracks {
                audio,
                subtitles,
                selected_audio,
                selected_subtitle,
            } => {
                self.player.audio_tracks = audio
                    .iter()
                    .map(|track| UiTrack {
                        index: track.index,
                        label: sanitize(&track.label, 64, "Track"),
                    })
                    .collect();
                self.player.subtitle_tracks = subtitles
                    .iter()
                    .map(|track| UiTrack {
                        index: track.index,
                        label: sanitize(&track.label, 64, "Track"),
                    })
                    .collect();
                self.player.selected_audio = *selected_audio;
                self.player.selected_subtitle = *selected_subtitle;
                self.player.menu = None;
                self.player.menu_focus = 0;
            }
            PlaybackUpdate::LibraryReady { filter, items } => {
                if self.screen == Screen::Library {
                    self.open_library(library_source_filter(*filter));
                    self.set_library_items(
                        items
                            .iter()
                            .map(|item| {
                                MediaItem::new(
                                    Source::Jellyfin,
                                    item.id.clone(),
                                    item.title.clone(),
                                )
                            })
                            .collect(),
                    );
                }
            }
            PlaybackUpdate::LibraryFailed { filter, message } => {
                if self.screen == Screen::Library {
                    self.open_library(library_source_filter(*filter));
                    self.set_library_error(message);
                }
            }
            PlaybackUpdate::Ended {
                item_id,
                position_ms,
                duration_ms,
                next_episode,
            } => {
                self.screen = Screen::Player;
                self.player.item_id = Some(item_id.clone());
                self.player.source = None;
                self.player.loading = false;
                self.player.paused = true;
                self.player.position_ms = *position_ms;
                self.player.duration_ms = *duration_ms;
                self.player.retry_available = false;
                self.player.next_episode = next_episode.as_ref().map(|episode| {
                    MediaItem::new(Source::Jellyfin, episode.id.clone(), episode.title.clone())
                });
                self.refresh_status();
            }
            PlaybackUpdate::Failed {
                item_id,
                code,
                message,
                retry_available,
            } => {
                if let Some(item_id) = item_id {
                    self.player.item_id = Some(item_id.clone());
                }
                self.screen = Screen::Player;
                self.player.loading = false;
                self.player.paused = false;
                self.player.retry_available = *retry_available;
                let detail = message.as_deref().unwrap_or(code);
                self.player.error = Some(sanitize(detail, MAX_ERROR_CHARS, "playback failed"));
                self.refresh_status();
            }
            PlaybackUpdate::Stopped { item_id: _ } => {
                self.screen = Screen::Home;
                self.player.title = "Nothing playing".to_string();
                self.player.source = None;
                self.player.item_id = None;
                self.player.method.clear();
                self.player.position_ms = 0;
                self.player.duration_ms = None;
                self.player.paused = false;
                self.player.loading = false;
                self.player.error = None;
                self.player.audio_tracks.clear();
                self.player.subtitle_tracks.clear();
                self.player.selected_audio = None;
                self.player.selected_subtitle = None;
                self.player.next_episode = None;
                self.player.retry_available = false;
                self.player.menu = None;
                self.status = "Choose a source".to_string();
            }
            PlaybackUpdate::SessionExpired => {
                self.screen = Screen::Player;
                self.player.loading = false;
                self.player.retry_available = false;
                self.player.error = Some("Jellyfin session expired; sign in again".to_string());
                self.refresh_status();
            }
            PlaybackUpdate::Message { text } => {
                self.status = sanitize(text, MAX_ERROR_CHARS, "");
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

    /// Feed one physical input event through the provenance router.
    ///
    /// This is the single path used by the Slint keyboard callback and the CEC
    /// bridge. A remote origin is refused before it can become a key; while a
    /// local confirmation is pending it produces the same rejection as the
    /// existing remote path. The `repeat` flag is metadata: the shared repeat
    /// gate applies the interval to both keyboard and CEC presses.
    pub fn handle_event(&mut self, event: crate::input::InputEvent, now: Duration) -> UiOutcome {
        match crate::input::Router::route(event) {
            crate::input::Route::Ui {
                key: crate::input::Key::PlayPause,
                ..
            } => self.handle_play_pause(),
            crate::input::Route::Ui { key, origin, .. } => {
                let Some(key) = Key::from_input(key) else {
                    return UiOutcome::Ignored;
                };
                self.handle_input(
                    UiInput::LocalKey {
                        key,
                        origin: origin.into(),
                    },
                    now,
                )
            }
            crate::input::Route::Volume { .. } => UiOutcome::Ignored,
            crate::input::Route::Rejected(_) => {
                if self.modal.is_some() {
                    UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
                } else {
                    UiOutcome::Ignored
                }
            }
        }
    }

    /// Toggle pause for a routed PlayPause key, only while the player owns the
    /// surface. On every other screen (and while a pairing prompt is open) the
    /// key is ignored and never becomes search text.
    fn handle_play_pause(&self) -> UiOutcome {
        if self.screen != Screen::Player || self.modal.is_some() {
            return UiOutcome::Ignored;
        }
        UiOutcome::Action(Action::SetPaused {
            paused: !self.player.paused,
        })
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
                    PairChoice::Deny => UiOutcome::DenyPairing { request_id },
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
                if let Some(item) = self.resume_item().cloned() {
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
        self.player.method.clear();
        self.player.audio_tracks.clear();
        self.player.subtitle_tracks.clear();
        self.player.selected_audio = None;
        self.player.selected_subtitle = None;
        self.player.next_episode = None;
        self.player.retry_available = false;
        self.player.menu = None;
        self.player.menu_focus = 0;
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
        if self.player.menu.is_some() {
            return self.handle_player_menu_key(key);
        }
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

    fn handle_player_menu_key(&mut self, key: Key) -> UiOutcome {
        let Some(menu) = self.player.menu else {
            return UiOutcome::Ignored;
        };
        let rows = self.player_menu_rows();
        if rows.is_empty() {
            self.player.menu = None;
            return self.navigated();
        }
        match key {
            Key::Up | Key::Left => {
                self.player.menu_focus = (self.player.menu_focus + rows.len() - 1) % rows.len();
                self.navigated()
            }
            Key::Down | Key::Right => {
                self.player.menu_focus = (self.player.menu_focus + 1) % rows.len();
                self.navigated()
            }
            Key::Enter => {
                let row = rows
                    .get(self.player.menu_focus.min(rows.len() - 1))
                    .cloned();
                self.player.menu = None;
                self.player.menu_focus = 0;
                match (menu, row) {
                    (PlayerMenu::Subtitles, Some(row)) => {
                        self.player.selected_subtitle = (row.index >= 0).then_some(row.index);
                        UiOutcome::Playback(UiPlaybackCommand::SelectSubtitle {
                            index: (row.index >= 0).then_some(row.index),
                        })
                    }
                    (PlayerMenu::Audio, Some(row)) => {
                        self.player.selected_audio = Some(row.index);
                        UiOutcome::Playback(UiPlaybackCommand::SelectAudio {
                            index: Some(row.index),
                        })
                    }
                    (_, None) => UiOutcome::Dismissed,
                }
            }
            Key::Escape => {
                self.player.menu = None;
                self.player.menu_focus = 0;
                UiOutcome::Dismissed
            }
            Key::Home => {
                self.player.menu = None;
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
                if self.player.subtitle_tracks.is_empty() {
                    self.status = "No subtitles available".to_owned();
                    return self.navigated();
                }
                self.player.menu = Some(PlayerMenu::Subtitles);
                self.player.menu_focus = self
                    .player
                    .selected_subtitle
                    .and_then(|selected| {
                        self.player
                            .subtitle_tracks
                            .iter()
                            .position(|track| track.index == selected)
                            .map(|position| position + 1)
                    })
                    .unwrap_or(0);
                self.refresh_status();
                self.navigated()
            }
            2 => {
                if self.player.audio_tracks.is_empty() {
                    self.status = "No audio tracks available".to_owned();
                    return self.navigated();
                }
                self.player.menu = Some(PlayerMenu::Audio);
                self.player.menu_focus = self
                    .player
                    .selected_audio
                    .and_then(|selected| {
                        self.player
                            .audio_tracks
                            .iter()
                            .position(|track| track.index == selected)
                    })
                    .unwrap_or(0);
                self.refresh_status();
                self.navigated()
            }
            3 => {
                if self.player.next_episode.is_some() {
                    UiOutcome::Playback(UiPlaybackCommand::NextEpisode)
                } else {
                    self.status = "No next episode".to_owned();
                    self.navigated()
                }
            }
            4 => {
                if self.player.retry_available {
                    UiOutcome::Playback(UiPlaybackCommand::Retry)
                } else {
                    self.status = "No retry available".to_owned();
                    self.navigated()
                }
            }
            5 => {
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
            3 => UiOutcome::StartPairing,
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
        self.player.menu = None;
        self.player.menu_focus = 0;
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

/// Number of player controls: pause, subtitles, audio, next, retry, stop, back.
const PLAYER_CONTROLS: usize = 7;

/// Settings rows, in display order.
pub const SETTINGS_ROWS: [&str; 5] = [
    "Video: 720p / 1080p auto",
    "Audio: HDMI (compositor)",
    "Licence: GPL-3.0-only",
    "Pair a remote",
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
    Key::from_input(crate::input::key_from_text(text)?)
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
        Key, LoadState, MediaItem, PairChoice, PlayerMenu, Screen, UiInput, UiModel, UiOutcome,
        UiPlaybackCommand, UiReject, UiTrack, VERSION_LABEL, source_label,
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
        /// A local operator denied a pending pairing request.
        DenyPairing { request_id: Uuid },
        /// The operator asked to start a pairing-code request.
        StartPairing,
        /// The operator asked to retry the current library load.
        Retry,
        /// The operator opened a library source; the controller should fetch it.
        LoadLibrary { filter: super::SourceFilter },
        /// Act on the active playback without touching the network here.
        Playback(UiPlaybackCommand),
    }

    /// Errors starting or running the native window.
    #[derive(Debug, Error)]
    pub enum NativeUiError {
        /// The windowing backend or event loop failed.
        #[error("native UI failed: {0}")]
        Platform(#[from] slint::PlatformError),
    }

    /// Type-erased callback for commands produced by local input.
    type CommandSink = Arc<dyn Fn(UiCommand) + Send + Sync>;

    /// Weak, thread-safe handle for pushing reducer state into the window.
    ///
    /// All window mutation is queued on the Slint event loop; the handle never
    /// blocks the caller and never performs I/O. Physical input submitted from
    /// a bridge thread is first placed in a bounded [`crate::input::InputQueue`]
    /// and drained on the event loop, so a stalled UI cannot block CEC.
    #[derive(Clone)]
    pub struct UiHandle {
        window: slint::Weak<AppWindow>,
        model: Arc<Mutex<UiModel>>,
        on_command: CommandSink,
        inputs: Arc<crate::input::InputQueue>,
        start: Instant,
    }

    impl UiHandle {
        /// Submit one physical input event from a bridge thread.
        ///
        /// A full queue drops the event instead of blocking. The event keeps
        /// its origin all the way to [`UiModel::handle_event`], so a remote
        /// origin can never become a local key.
        pub fn submit_input(&self, event: crate::input::InputEvent) {
            if let Err(error) = self.inputs.try_push(event) {
                let dropped = self.inputs.dropped();
                if dropped == 1 || dropped.is_multiple_of(32) {
                    tracing::warn!(%error, dropped, "physical input queue full; event dropped");
                }
                return;
            }
            let window = self.window.clone();
            let model = Arc::clone(&self.model);
            let on_command = Arc::clone(&self.on_command);
            let inputs = Arc::clone(&self.inputs);
            let start = self.start;
            let _ = window.upgrade_in_event_loop(move |window| {
                let mut model = lock_model(&model);
                while let Some(event) = inputs.pop() {
                    apply_input_event(&mut model, event, start.elapsed(), &on_command);
                }
                refresh(&window, &model);
            });
        }

        /// Post a reducer snapshot to the event loop.
        pub fn apply_state(&self, state: crate::model::AppState) {
            self.post(move |model| model.apply_state(&state));
        }

        /// Post one playback-controller update to the event loop.
        pub fn apply_playback_update(&self, update: crate::playback::PlaybackUpdate) {
            self.post(move |model| model.apply_playback_update(&update));
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

        /// Replace the continue-watching row from the playback controller.
        pub fn set_continue_watching(&self, items: Vec<MediaItem>) {
            self.post(move |model| model.set_continue_watching(items));
        }

        /// Set or clear the explicit resume item.
        pub fn set_resume_item(&self, item: Option<MediaItem>) {
            self.post(move |model| model.set_resume(item));
        }

        /// Install the tracks offered by the playback controller.
        pub fn set_player_tracks(
            &self,
            audio: Vec<UiTrack>,
            subtitles: Vec<UiTrack>,
            selected_audio: Option<i32>,
            selected_subtitle: Option<i32>,
        ) {
            self.post(move |model| {
                model.set_player_tracks(audio, subtitles, selected_audio, selected_subtitle);
            });
        }

        /// Offer (or clear) the next episode.
        pub fn set_next_episode(&self, item: Option<MediaItem>) {
            self.post(move |model| model.set_next_episode(item));
        }

        /// Enable or disable the retry control.
        pub fn set_retry_available(&self, available: bool) {
            self.post(move |model| model.set_retry_available(available));
        }

        /// Set the negotiated play-method label.
        pub fn set_player_method(&self, method: String) {
            self.post(move |model| model.set_player_method(&method));
        }

        /// Present a wire pairing request for local confirmation.
        pub fn present_pairing_request(&self, request_id: Uuid) {
            self.post(move |model| {
                let _ = model.present_pairing_request(request_id);
            });
        }

        /// Present a pairing request with its server comparison code.
        pub fn present_pairing_request_with_code(
            &self,
            request_id: Uuid,
            comparison_code: Option<String>,
        ) {
            self.post(move |model| {
                let _ =
                    model.present_pairing_request_with_code(request_id, comparison_code.as_deref());
            });
        }

        /// Show the server-issued eight-digit pairing code on the TV.
        pub fn set_pairing_code(&self, code: String) {
            self.post(move |model| {
                if model.set_pairing_code(&code).is_err() {
                    tracing::warn!("ignoring malformed pairing code");
                }
            });
        }

        /// Clear the pairing code banner.
        pub fn clear_pairing_code(&self) {
            self.post(UiModel::clear_pairing_code);
        }

        /// Dismiss the confirmation dialog for a terminal or expired request.
        pub fn dismiss_pairing_request(&self, request_id: Uuid) {
            self.post(move |model| model.dismiss_pairing_request(request_id));
        }

        /// Clear a stale playback error before a remote retry.
        ///
        /// The model is updated synchronously so the cloud applier can seed
        /// its failure detection from the cleared state; the event loop post
        /// only refreshes the window.
        pub fn clear_player_error(&self) {
            lock_model(&self.model).clear_player_error();
            self.post(|_model| {});
        }

        /// Update the persistent cloud status line.
        pub fn set_cloud_status(&self, status: String) {
            self.post(move |model| model.set_cloud_status(&status));
        }

        /// Snapshot the presentation state for cloud publication.
        pub fn snapshot(&self) -> crate::model::AppState {
            lock_model(&self.model).snapshot()
        }

        /// Apply one wire action through the local view-model command path.
        pub async fn apply_remote_action(
            self,
            action: crate::model::Action,
        ) -> Result<UiOutcome, UiReject> {
            let (reply, response) = tokio::sync::oneshot::channel();
            let window = self.window.clone();
            let model = std::sync::Arc::clone(&self.model);
            let on_command = std::sync::Arc::clone(&self.on_command);
            let start = self.start;
            let _ = window.upgrade_in_event_loop(move |window| {
                let mut model = lock_model(&model);
                let outcome = model.handle_remote_action(action, start.elapsed());
                dispatch(&on_command, outcome.clone());
                refresh(&window, &model);
                let _ = reply.send(outcome);
            });
            response.await.map_err(|_| UiReject::Unavailable)
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
        on_command: CommandSink,
        inputs: Arc<crate::input::InputQueue>,
        start: Instant,
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
            let on_command: CommandSink = Arc::new(on_command);
            let inputs = Arc::new(
                crate::input::InputQueue::new(crate::input::DEFAULT_INPUT_CAPACITY)
                    .expect("the default physical-input capacity is valid"),
            );
            let start = Instant::now();

            {
                let weak = window.as_weak();
                let model = Arc::clone(&model);
                let on_command = Arc::clone(&on_command);
                window.on_key_pressed(move |text, repeat| {
                    let Some(key) = crate::input::key_from_text(text.as_str()) else {
                        return;
                    };
                    let event = crate::input::InputEvent {
                        origin: crate::model::InputOrigin::LocalKeyboard,
                        key,
                        repeat,
                    };
                    with_window(&weak, &model, |window, model| {
                        handle_input_event(window, model, event, start.elapsed(), &on_command);
                    });
                });
            }

            install_pointer_callbacks(&window, &model, &on_command, start);

            Ok(Self {
                window,
                model,
                on_command,
                inputs,
                start,
            })
        }

        /// Weak handle for a controller running on another thread.
        #[must_use]
        pub fn handle(&self) -> UiHandle {
            UiHandle {
                window: self.window.as_weak(),
                model: Arc::clone(&self.model),
                on_command: Arc::clone(&self.on_command),
                inputs: Arc::clone(&self.inputs),
                start: self.start,
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
        on_command: &CommandSink,
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
            window.on_track_activated(move |index| {
                with_window(&weak, &model, |window, model| {
                    model.set_player_menu_focus(index.max(0) as usize);
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

    fn handle_local_input(
        window: &AppWindow,
        model: &mut UiModel,
        key: Key,
        now: Duration,
        on_command: &CommandSink,
    ) {
        let event = crate::input::InputEvent {
            origin: crate::model::InputOrigin::LocalKeyboard,
            key: key.into_input(),
            repeat: false,
        };
        handle_input_event(window, model, event, now, on_command);
    }

    fn handle_input_event(
        window: &AppWindow,
        model: &mut UiModel,
        event: crate::input::InputEvent,
        now: Duration,
        on_command: &CommandSink,
    ) {
        apply_input_event(model, event, now, on_command);
        refresh(window, model);
    }

    fn apply_input_event(
        model: &mut UiModel,
        event: crate::input::InputEvent,
        now: Duration,
        on_command: &CommandSink,
    ) {
        let was_loading = model.is_library_loading();
        let outcome = model.handle_event(event, now);
        dispatch(on_command, outcome);
        if let Some(filter) = model.pending_library_load(was_loading) {
            (**on_command)(UiCommand::LoadLibrary { filter });
        }
    }

    fn dispatch(on_command: &CommandSink, outcome: UiOutcome) {
        match outcome {
            UiOutcome::Action(action) => (**on_command)(UiCommand::Action(action)),
            UiOutcome::ApprovePairing { request_id } => {
                (**on_command)(UiCommand::ApprovePairing { request_id });
            }
            UiOutcome::DenyPairing { request_id } => {
                (**on_command)(UiCommand::DenyPairing { request_id });
            }
            UiOutcome::StartPairing => (**on_command)(UiCommand::StartPairing),
            UiOutcome::Retry => (**on_command)(UiCommand::Retry),
            UiOutcome::Playback(command) => (**on_command)(UiCommand::Playback(command)),
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
        window.set_player_method(model.player_method().into());
        window.set_player_position(model.player_position_text().into());
        window.set_player_progress(model.player_progress());
        window.set_player_paused(model.player_paused());
        window.set_player_loading(model.player_loading());
        window.set_resume_title(model.resume_title().into());
        window.set_next_episode_available(model.next_episode().is_some());
        window.set_retry_available(model.retry_available());
        let menu = model.player_menu();
        let selected_index = match menu {
            Some(PlayerMenu::Subtitles) => model.selected_subtitle().unwrap_or(-1),
            Some(PlayerMenu::Audio) => model.selected_audio().unwrap_or(i32::MIN),
            None => i32::MIN,
        };
        let tracks: Vec<TrackRow> = model
            .player_menu_rows()
            .into_iter()
            .map(|row| TrackRow {
                label: row.label.as_str().into(),
                index: row.index,
                selected: row.index == selected_index,
            })
            .collect();
        window.set_track_items(ModelRc::new(VecModel::from(tracks)));
        window.set_track_menu_visible(menu.is_some());
        window.set_track_menu_title(menu.map_or("", PlayerMenu::title).into());
        window.set_track_focus(model.player_menu_focus() as i32);
        window.set_cast_state(model.cast_state().into());
        window.set_settings_version(VERSION_LABEL.into());

        window.set_pair_code(model.pairing_code().into());
        window.set_cloud_status(model.cloud_status().into());
        let prompt = model.pair_prompt();
        window.set_pair_visible(prompt.is_some());
        window.set_pair_message(
            prompt
                .map_or_else(String::new, |prompt| prompt.display_text())
                .into(),
        );
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
