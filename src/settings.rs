//! On-device settings: writable state configuration and the pure edit model.
//!
//! Two responsibilities live here, both free of Slint and blocking I/O on the
//! UI thread:
//!
//! * [`save_state_config`] validates a [`ClientConfig`] and replaces
//!   `<state-dir>/client.toml` atomically with a 0600 file inside a 0700
//!   directory. Configuration values are never logged.
//! * [`SettingsForm`] is the keyboard-driven editor rendered by the settings
//!   screen. It owns only buffers, focus and validation messages; the caller
//!   performs the write and the process restart.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{
    ClientConfig, ConfigError, MAX_UI_SAFE_AREA_PERCENT, MAX_YOUTUBE_TIMEOUT_SECS,
    MIN_UI_SAFE_AREA_PERCENT, MIN_YOUTUBE_TIMEOUT_SECS, YoutubeHwdec, state_config_path,
};
use crate::ui::Key;

/// Number of editable settings rows.
pub const SETTINGS_ROW_COUNT: usize = 14;
/// Rows before this index are rendered in the left column.
pub const SETTINGS_COLUMN_BREAK: usize = 8;
/// Longest accepted text-field buffer, in characters.
pub const MAX_SETTINGS_FIELD_CHARS: usize = 2048;
const MAX_NUMERIC_FIELD_CHARS: usize = 12;

/// One settings row, in keyboard order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingRow {
    JellyfinUrl,
    ServerUrl,
    CloudEnabled,
    YoutubeEnabled,
    YoutubePlayback,
    YtdlpPath,
    Timeout,
    Hwdec,
    SafeArea,
    PairRemote,
    JellyfinSignIn,
    Save,
    Restart,
    Back,
}

impl SettingRow {
    /// Rows in display and keyboard order.
    pub const ALL: [Self; SETTINGS_ROW_COUNT] = [
        Self::JellyfinUrl,
        Self::ServerUrl,
        Self::CloudEnabled,
        Self::YoutubeEnabled,
        Self::YoutubePlayback,
        Self::YtdlpPath,
        Self::Timeout,
        Self::Hwdec,
        Self::SafeArea,
        Self::PairRemote,
        Self::JellyfinSignIn,
        Self::Save,
        Self::Restart,
        Self::Back,
    ];

    /// Position in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::JellyfinUrl => 0,
            Self::ServerUrl => 1,
            Self::CloudEnabled => 2,
            Self::YoutubeEnabled => 3,
            Self::YoutubePlayback => 4,
            Self::YtdlpPath => 5,
            Self::Timeout => 6,
            Self::Hwdec => 7,
            Self::SafeArea => 8,
            Self::PairRemote => 9,
            Self::JellyfinSignIn => 10,
            Self::Save => 11,
            Self::Restart => 12,
            Self::Back => 13,
        }
    }

    /// Row kind shared with the declarative UI.
    #[must_use]
    pub const fn kind(self) -> RowKind {
        match self {
            Self::JellyfinUrl | Self::ServerUrl | Self::YtdlpPath | Self::Timeout | Self::SafeArea => {
                RowKind::Text
            }
            Self::CloudEnabled | Self::YoutubeEnabled | Self::YoutubePlayback => RowKind::Toggle,
            Self::Hwdec => RowKind::Choice,
            Self::PairRemote | Self::JellyfinSignIn | Self::Save | Self::Restart | Self::Back => {
                RowKind::Action
            }
        }
    }

    /// Static label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::JellyfinUrl => "Jellyfin URL",
            Self::ServerUrl => "Control server URL",
            Self::CloudEnabled => "Cloud control",
            Self::YoutubeEnabled => "YouTube search",
            Self::YoutubePlayback => "YouTube playback",
            Self::YtdlpPath => "yt-dlp path",
            Self::Timeout => "Playback timeout (s)",
            Self::Hwdec => "Hardware decode",
            Self::SafeArea => "UI safe area (%)",
            Self::PairRemote => "Pair a remote",
            Self::JellyfinSignIn => "Jellyfin sign-in",
            Self::Save => "Save settings",
            Self::Restart => "Restart app",
            Self::Back => "Back to home",
        }
    }
}

/// Presentation kind of a settings row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Editable text; Enter starts editing.
    Text,
    /// Boolean toggle; Enter or Left/Right flips it.
    Toggle,
    /// Fixed choice; Enter or Left/Right cycles it.
    Choice,
    /// Immediate action.
    Action,
}

impl RowKind {
    /// Stable integer shared with the Slint row model.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Text => 0,
            Self::Toggle => 1,
            Self::Choice => 2,
            Self::Action => 3,
        }
    }
}

/// One row rendered by the settings screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsRowView {
    /// Absolute row index, used by pointer activation.
    pub index: i32,
    /// Row label.
    pub label: String,
    /// Current or committed value.
    pub value: String,
    /// [`RowKind::index`].
    pub kind: i32,
    /// Current toggle state.
    pub toggle_on: bool,
    /// Whether this row owns the character buffer.
    pub editing: bool,
}

/// Result of one key press on the settings screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Focus or edit state changed.
    Navigated,
    /// Enter started editing a text row.
    Editing,
    /// A value changed locally.
    Edited,
    /// The operator asked for a cloud pairing code.
    StartPairing,
    /// Start the Jellyfin Quick Connect flow.
    StartJellyfinSignIn,
    /// Revoke and clear the Jellyfin session.
    SignOutJellyfin,
    /// A saved configuration is ready to be written by the worker.
    Save(ClientConfig),
    /// Exit cleanly so the supervisor restarts the process.
    Restart,
    /// Leave the settings screen.
    Back,
    /// The action was refused; the form error explains why.
    Rejected,
    /// The key has no effect in the current state.
    Ignored,
}

/// Settings validation and persistence failures.
///
/// Messages name fields and static reasons only. They never contain a URL, a
/// path, a token or any other supplied value, so they are safe to render and
/// log.
#[derive(Debug, Error)]
pub enum SettingsError {
    /// The edited or loaded configuration is invalid.
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// One field failed a local range or format check.
    #[error("invalid value for {field}: {reason}")]
    InvalidField {
        /// Field name.
        field: &'static str,
        /// Static reason.
        reason: &'static str,
    },
    /// The state directory is missing, unsafe or not writable.
    #[error("the state directory is not usable")]
    StateDir,
    /// The configuration path is a symlink or not a regular file.
    #[error("the configuration path is not a regular file")]
    UnsafePath,
    /// A local filesystem operation failed.
    #[error("could not write the configuration")]
    Io(#[from] std::io::Error),
}

/// Write `<state-dir>/client.toml` atomically.
///
/// The configuration is validated before any file is touched. The state
/// directory is created if absent and forced to 0700; the document is written
/// to a fresh 0600 temporary file in the same directory, `fsync`ed, renamed
/// over the target and the directory is `fsync`ed. No value is logged.
pub fn save_state_config(state_dir: &Path, config: &ClientConfig) -> Result<PathBuf, SettingsError> {
    config.validate()?;
    let document = config.to_toml_string()?;
    ensure_private_dir(state_dir)?;
    let target = state_config_path(state_dir);
    atomic_replace(&target, document.as_bytes())?;
    Ok(target)
}

fn ensure_private_dir(directory: &Path) -> Result<(), SettingsError> {
    if directory.as_os_str().is_empty() || directory.file_name().is_none() {
        return Err(SettingsError::StateDir);
    }
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SettingsError::StateDir);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
            let metadata = fs::symlink_metadata(directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SettingsError::StateDir);
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn atomic_replace(target: &Path, bytes: &[u8]) -> Result<(), SettingsError> {
    if let Ok(metadata) = fs::symlink_metadata(target)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(SettingsError::UnsafePath);
    }
    let directory = target.parent().ok_or(SettingsError::StateDir)?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SettingsError::StateDir)?;

    let mut attempt = 0u32;
    let (temporary, mut file) = loop {
        let candidate = directory.join(format!(
            "{name}.tmp.{}.{attempt}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 =>
            {
                attempt += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };

    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            Ok(())
        });
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    File::open(directory)?.sync_all()?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Keyboard-driven settings form.
///
/// The form never performs I/O. [`Self::handle_key`] returns a
/// [`SettingsEvent::Save`] carrying a fully validated [`ClientConfig`]; the
/// owner hands that value to a worker, which calls [`save_state_config`].
#[derive(Debug, Clone)]
pub struct SettingsForm {
    original: ClientConfig,
    jellyfin_url: String,
    server_url: String,
    ytdlp_path: String,
    timeout: String,
    safe_area: String,
    hwdec: YoutubeHwdec,
    cloud_enabled: bool,
    youtube_enabled: bool,
    youtube_playback_enabled: bool,
    focus: usize,
    editing: Option<usize>,
    edit_buffer: String,
    message: String,
    error: Option<String>,
    dirty: bool,
    pending_save: Option<ClientConfig>,
    jellyfin_user: Option<String>,
    quick_connect_code: Option<String>,
    quick_connect_active: bool,
}

impl SettingsForm {
    /// Build a form from the effective configuration.
    #[must_use]
    pub fn new(config: &ClientConfig) -> Self {
        Self {
            original: config.clone(),
            jellyfin_url: config.jellyfin_url.clone().unwrap_or_default(),
            server_url: config.server_url.clone().unwrap_or_default(),
            ytdlp_path: config
                .youtube_ytdlp_path
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            timeout: config.youtube_playback_timeout_secs.to_string(),
            safe_area: config.ui_safe_area_percent.to_string(),
            hwdec: config.youtube_hwdec,
            cloud_enabled: config.cloud_enabled,
            youtube_enabled: config.youtube_enabled,
            youtube_playback_enabled: config.youtube_playback_enabled,
            focus: 0,
            editing: None,
            edit_buffer: String::new(),
            message: String::new(),
            error: None,
            dirty: false,
            pending_save: None,
            jellyfin_user: None,
            quick_connect_code: None,
            quick_connect_active: false,
        }
    }

    /// Focused row index.
    #[must_use]
    pub const fn focus(&self) -> usize {
        self.focus
    }

    /// Move focus, clamped to the row list.
    pub fn set_focus(&mut self, index: usize) {
        self.focus = index.min(SETTINGS_ROW_COUNT - 1);
    }

    /// Focused row.
    #[must_use]
    pub fn focused_row(&self) -> SettingRow {
        SettingRow::ALL[self.focus]
    }

    /// Whether a text row currently owns typed characters.
    #[must_use]
    pub const fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Current edit buffer (empty when no row is being edited).
    #[must_use]
    pub fn edit_value(&self) -> &str {
        &self.edit_buffer
    }

    /// Short status message for the settings screen.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Validation error, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether local edits have not been persisted.
    #[must_use]
    pub const fn dirty(&self) -> bool {
        self.dirty
    }

    /// Signed-in Jellyfin user, when one is known.
    #[must_use]
    pub fn jellyfin_user(&self) -> Option<&str> {
        self.jellyfin_user.as_deref()
    }

    /// Set or clear the signed-in Jellyfin user.
    pub fn set_jellyfin_user(&mut self, user: Option<String>) {
        self.jellyfin_user = user;
    }

    /// Show or clear the Quick Connect code.
    pub fn set_quick_connect_code(&mut self, code: Option<String>) {
        self.quick_connect_code = code;
    }

    /// Quick Connect code currently shown on the TV.
    #[must_use]
    pub fn quick_connect_code(&self) -> Option<&str> {
        self.quick_connect_code.as_deref()
    }

    /// Whether a Quick Connect poll is in flight.
    #[must_use]
    pub const fn quick_connect_active(&self) -> bool {
        self.quick_connect_active
    }

    /// Mark the Quick Connect flow as started or finished.
    pub fn set_quick_connect_active(&mut self, active: bool) {
        self.quick_connect_active = active;
        if !active {
            self.quick_connect_code = None;
        }
    }

    /// Set a non-error status message.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = None;
    }

    /// Set a validation or worker error. `message` must not contain secrets.
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.message.clear();
    }

    /// Clear status and validation text.
    pub fn clear_messages(&mut self) {
        self.message.clear();
        self.error = None;
    }

    /// Confirm a successful worker write.
    pub fn mark_saved(&mut self) {
        if let Some(saved) = self.pending_save.take() {
            self.original = saved;
        }
        self.dirty = false;
        self.error = None;
        self.message = "Saved. Restart to apply.".to_owned();
    }

    /// Confirm that the worker failed to write.
    pub fn mark_save_failed(&mut self) {
        self.error = Some("Not saved: the state configuration could not be written.".to_owned());
        self.message.clear();
    }

    /// Current row projection for the declarative UI.
    #[must_use]
    pub fn rows(&self) -> Vec<SettingsRowView> {
        SettingRow::ALL
            .iter()
            .map(|row| SettingsRowView {
                index: row.index() as i32,
                label: row.label().to_owned(),
                value: self.row_value(*row),
                kind: row.kind().index(),
                toggle_on: self.row_toggle(*row),
                editing: self.editing == Some(row.index()),
            })
            .collect()
    }

    /// Build a validated configuration from the current buffers.
    pub fn build_config(&self) -> Result<ClientConfig, SettingsError> {
        let mut config = self.original.clone();
        config.jellyfin_url = non_empty(&self.jellyfin_url);
        config.server_url = non_empty(&self.server_url);
        config.cloud_enabled = self.cloud_enabled;
        config.youtube_enabled = self.youtube_enabled;
        config.youtube_playback_enabled = self.youtube_playback_enabled;
        config.youtube_ytdlp_path = non_empty(&self.ytdlp_path).map(PathBuf::from);
        config.youtube_playback_timeout_secs = parse_bounded(
            "youtube_playback_timeout_secs",
            &self.timeout,
            MIN_YOUTUBE_TIMEOUT_SECS,
            MAX_YOUTUBE_TIMEOUT_SECS,
            "must be a whole number within 10..=300",
        )?;
        config.ui_safe_area_percent = parse_bounded(
            "ui_safe_area_percent",
            &self.safe_area,
            u64::from(MIN_UI_SAFE_AREA_PERCENT),
            u64::from(MAX_UI_SAFE_AREA_PERCENT),
            "must be a whole number within 0..=10",
        )? as u8;
        config.youtube_hwdec = self.hwdec;
        config.validate()?;
        Ok(config)
    }

    /// Apply one key press.
    pub fn handle_key(&mut self, key: Key) -> SettingsEvent {
        if self.editing.is_some() {
            return self.handle_edit_key(key);
        }
        match key {
            Key::Up => {
                self.move_vertical(false);
                SettingsEvent::Navigated
            }
            Key::Down => {
                self.move_vertical(true);
                SettingsEvent::Navigated
            }
            Key::Left => {
                self.move_horizontal(false);
                SettingsEvent::Navigated
            }
            Key::Right => {
                self.move_horizontal(true);
                SettingsEvent::Navigated
            }
            Key::Enter => self.activate(),
            Key::Escape | Key::Home => SettingsEvent::Back,
            Key::Backspace | Key::Char(_) => SettingsEvent::Ignored,
        }
    }

    fn row_value(&self, row: SettingRow) -> String {
        match row {
            SettingRow::JellyfinUrl => self.jellyfin_url.clone(),
            SettingRow::ServerUrl => self.server_url.clone(),
            SettingRow::CloudEnabled => on_off(self.cloud_enabled),
            SettingRow::YoutubeEnabled => on_off(self.youtube_enabled),
            SettingRow::YoutubePlayback => on_off(self.youtube_playback_enabled),
            SettingRow::YtdlpPath => self.ytdlp_path.clone(),
            SettingRow::Timeout => self.timeout.clone(),
            SettingRow::Hwdec => hwdec_label(self.hwdec).to_owned(),
            SettingRow::SafeArea => self.safe_area.clone(),
            SettingRow::PairRemote => "Show code".to_owned(),
            SettingRow::JellyfinSignIn => match self.jellyfin_user.as_deref() {
                Some(user) => format!("Sign out ({user})"),
                None if self.quick_connect_active => "Waiting for approval".to_owned(),
                None => "Sign in".to_owned(),
            },
            SettingRow::Save => "Write config".to_owned(),
            SettingRow::Restart => "Exit; supervisor restarts".to_owned(),
            SettingRow::Back => String::new(),
        }
    }

    fn row_toggle(&self, row: SettingRow) -> bool {
        match row {
            SettingRow::CloudEnabled => self.cloud_enabled,
            SettingRow::YoutubeEnabled => self.youtube_enabled,
            SettingRow::YoutubePlayback => self.youtube_playback_enabled,
            _ => false,
        }
    }

    fn move_vertical(&mut self, down: bool) {
        let (start, end) = if self.focus < SETTINGS_COLUMN_BREAK {
            (0, SETTINGS_COLUMN_BREAK - 1)
        } else {
            (SETTINGS_COLUMN_BREAK, SETTINGS_ROW_COUNT - 1)
        };
        self.focus = if down {
            (self.focus + 1).min(end)
        } else {
            self.focus.saturating_sub(1).max(start)
        };
    }

    fn move_horizontal(&mut self, right: bool) {
        let (offset, target_start, target_end) = if right {
            if self.focus >= SETTINGS_COLUMN_BREAK {
                return;
            }
            (
                self.focus,
                SETTINGS_COLUMN_BREAK,
                SETTINGS_ROW_COUNT - 1,
            )
        } else {
            if self.focus < SETTINGS_COLUMN_BREAK {
                return;
            }
            (
                self.focus - SETTINGS_COLUMN_BREAK,
                0,
                SETTINGS_COLUMN_BREAK - 1,
            )
        };
        self.focus = (target_start + offset).min(target_end);
    }

    fn activate(&mut self) -> SettingsEvent {
        match self.focused_row() {
            SettingRow::JellyfinUrl
            | SettingRow::ServerUrl
            | SettingRow::YtdlpPath
            | SettingRow::Timeout
            | SettingRow::SafeArea => {
                self.editing = Some(self.focus);
                self.edit_buffer = self.row_value(self.focused_row());
                self.error = None;
                self.message = "Enter to apply, Esc to cancel".to_owned();
                SettingsEvent::Editing
            }
            SettingRow::CloudEnabled => {
                self.cloud_enabled = !self.cloud_enabled;
                self.changed("Cloud control")
            }
            SettingRow::YoutubeEnabled => {
                self.youtube_enabled = !self.youtube_enabled;
                self.changed("YouTube search")
            }
            SettingRow::YoutubePlayback => {
                self.youtube_playback_enabled = !self.youtube_playback_enabled;
                self.changed("YouTube playback")
            }
            SettingRow::Hwdec => {
                self.hwdec = next_hwdec(self.hwdec);
                self.changed("Hardware decode")
            }
            SettingRow::PairRemote => SettingsEvent::StartPairing,
            SettingRow::JellyfinSignIn => {
                if self.jellyfin_user.is_some() {
                    SettingsEvent::SignOutJellyfin
                } else {
                    SettingsEvent::StartJellyfinSignIn
                }
            }
            SettingRow::Save => match self.build_config() {
                Ok(config) => {
                    self.error = None;
                    self.message = "Writing configuration...".to_owned();
                    self.pending_save = Some(config.clone());
                    SettingsEvent::Save(config)
                }
                Err(error) => {
                    self.set_error(error.to_string());
                    SettingsEvent::Rejected
                }
            },
            SettingRow::Restart => SettingsEvent::Restart,
            SettingRow::Back => SettingsEvent::Back,
        }
    }

    fn changed(&mut self, label: &str) -> SettingsEvent {
        self.dirty = true;
        self.error = None;
        self.message = format!("Changed {label}; press Save");
        SettingsEvent::Edited
    }

    fn handle_edit_key(&mut self, key: Key) -> SettingsEvent {
        let row_index = self.editing.unwrap_or(self.focus);
        let row = SettingRow::ALL[row_index.min(SETTINGS_ROW_COUNT - 1)];
        match key {
            Key::Escape => {
                self.editing = None;
                self.edit_buffer.clear();
                self.error = None;
                self.message = "Edit cancelled".to_owned();
                SettingsEvent::Navigated
            }
            Key::Enter => self.commit_edit(row),
            Key::Backspace => {
                self.edit_buffer.pop();
                SettingsEvent::Edited
            }
            Key::Char(character) => {
                if !character.is_control() && self.accept_character(row, character) {
                    self.edit_buffer.push(character);
                }
                SettingsEvent::Edited
            }
            Key::Up | Key::Down | Key::Left | Key::Right | Key::Home => SettingsEvent::Ignored,
        }
    }

    fn accept_character(&self, row: SettingRow, character: char) -> bool {
        let limit = match row {
            SettingRow::Timeout | SettingRow::SafeArea => {
                return character.is_ascii_digit()
                    && self.edit_buffer.chars().count() < MAX_NUMERIC_FIELD_CHARS;
            }
            _ => MAX_SETTINGS_FIELD_CHARS,
        };
        self.edit_buffer.chars().count() < limit
    }

    fn commit_edit(&mut self, row: SettingRow) -> SettingsEvent {
        let value = self.edit_buffer.trim().to_owned();
        match row {
            SettingRow::JellyfinUrl => {
                if let Err(error) = validate_optional_url(&value, true) {
                    self.set_error(error.to_string());
                    return SettingsEvent::Rejected;
                }
                self.jellyfin_url = value;
            }
            SettingRow::ServerUrl => {
                if let Err(error) = validate_optional_url(&value, false) {
                    self.set_error(error.to_string());
                    return SettingsEvent::Rejected;
                }
                self.server_url = value;
            }
            SettingRow::YtdlpPath => {
                if !value.is_empty()
                    && let Err(error) = crate::config::validate_ytdlp_path(&value)
                {
                    self.set_error(error.to_string());
                    return SettingsEvent::Rejected;
                }
                self.ytdlp_path = value;
            }
            SettingRow::Timeout => {
                if let Err(error) = parse_bounded(
                    "youtube_playback_timeout_secs",
                    &value,
                    MIN_YOUTUBE_TIMEOUT_SECS,
                    MAX_YOUTUBE_TIMEOUT_SECS,
                    "must be a whole number within 10..=300",
                ) {
                    self.set_error(error.to_string());
                    return SettingsEvent::Rejected;
                }
                self.timeout = value;
            }
            SettingRow::SafeArea => {
                if let Err(error) = parse_bounded(
                    "ui_safe_area_percent",
                    &value,
                    u64::from(MIN_UI_SAFE_AREA_PERCENT),
                    u64::from(MAX_UI_SAFE_AREA_PERCENT),
                    "must be a whole number within 0..=10",
                ) {
                    self.set_error(error.to_string());
                    return SettingsEvent::Rejected;
                }
                self.safe_area = value;
            }
            _ => return SettingsEvent::Ignored,
        }
        self.editing = None;
        self.edit_buffer.clear();
        self.dirty = true;
        self.set_message(format!("Edited {}", row.label()));
        SettingsEvent::Edited
    }
}

fn validate_optional_url(value: &str, local_http: bool) -> Result<(), ConfigError> {
    if value.is_empty() {
        return Ok(());
    }
    if local_http {
        crate::config::validate_jellyfin_origin(value)
    } else {
        crate::config::validate_server_origin(value)
    }
}

fn parse_bounded(
    field: &'static str,
    value: &str,
    minimum: u64,
    maximum: u64,
    reason: &'static str,
) -> Result<u64, SettingsError> {
    let parsed: u64 = value
        .parse()
        .map_err(|_| SettingsError::InvalidField { field, reason })?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(SettingsError::InvalidField { field, reason });
    }
    Ok(parsed)
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn on_off(value: bool) -> String {
    if value { "On" } else { "Off" }.to_owned()
}

fn hwdec_label(value: YoutubeHwdec) -> &'static str {
    match value {
        YoutubeHwdec::AutoSafe => "auto-safe",
        YoutubeHwdec::V4l2m2m => "v4l2m2m",
        YoutubeHwdec::None => "none",
    }
}

fn next_hwdec(value: YoutubeHwdec) -> YoutubeHwdec {
    match value {
        YoutubeHwdec::AutoSafe => YoutubeHwdec::V4l2m2m,
        YoutubeHwdec::V4l2m2m => YoutubeHwdec::None,
        YoutubeHwdec::None => YoutubeHwdec::AutoSafe,
    }
}
