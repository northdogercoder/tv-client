//! Strict client configuration loaded from TOML.
//!
//! Every key is validated before the client starts. Cloud features default to
//! disabled, and an HTTP server URL is never accepted; HTTP is only allowed for
//! an explicitly local Jellyfin host.

use std::env;
use std::fmt;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Lowest accepted horizontal resolution.
pub const MIN_WIDTH: u32 = 320;
/// Highest accepted horizontal resolution.
pub const MAX_WIDTH: u32 = 7680;
/// Lowest accepted vertical resolution.
pub const MIN_HEIGHT: u32 = 240;
/// Highest accepted vertical resolution.
pub const MAX_HEIGHT: u32 = 4320;
/// Lowest accepted frame rate.
pub const MIN_FPS: u32 = 1;
/// Highest accepted frame rate.
pub const MAX_FPS: u32 = 120;
/// Lowest accepted nominal bitrate.
pub const MIN_VIDEO_BITRATE_BPS: u64 = 64_000;
/// Highest accepted nominal bitrate.
pub const MAX_VIDEO_BITRATE_BPS: u64 = 100_000_000;
/// Hard ceiling for the artwork cache.
pub const MAX_ARTWORK_CACHE_BYTES: u64 = 128 * 1024 * 1024;
/// Largest configuration file accepted.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_URL_BYTES: usize = 2048;
const MAX_TOOL_PATH_BYTES: usize = 1024;

/// Lowest accepted YouTube playback timeout.
pub const MIN_YOUTUBE_TIMEOUT_SECS: u64 = 10;
/// Highest accepted YouTube playback timeout.
pub const MAX_YOUTUBE_TIMEOUT_SECS: u64 = 300;
/// Default resolve-and-start budget for local YouTube playback.
pub const DEFAULT_YOUTUBE_TIMEOUT_SECS: u64 = 60;

/// Hardware decoding preference for the local YouTube libmpv child.
///
/// The value becomes an mpv `--hwdec=` argument from a fixed allowlist; it is
/// never interpolated from remote input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum YoutubeHwdec {
    /// Let mpv choose a safe hardware decoder, falling back to software.
    #[default]
    AutoSafe,
    /// Raspberry Pi V4L2 mem-to-mem decoder.
    V4l2m2m,
    /// Software decoding only.
    None,
}

impl YoutubeHwdec {
    /// Value passed to mpv's `--hwdec` option.
    #[must_use]
    pub const fn as_arg(self) -> &'static str {
        match self {
            Self::AutoSafe => "auto-safe",
            Self::V4l2m2m => "v4l2m2m",
            Self::None => "no",
        }
    }
}

/// Strict client settings.
///
/// Field names and defaults match `deploy/client.toml.example`. Unknown keys
/// are rejected so typos cannot silently disable a safety-relevant option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// Direct Jellyfin origin. May be absent for a cloud-only client.
    pub jellyfin_url: Option<String>,
    /// Optional control-server origin. Required for cloud features.
    pub server_url: Option<String>,
    pub cloud_enabled: bool,
    pub youtube_enabled: bool,
    /// Local YouTube playback kill switch.
    ///
    /// Effective only when [`Self::youtube_ytdlp_path`] names an executable
    /// managed extractor; a missing or invalid path silently leaves playback
    /// disabled rather than failing startup.
    pub youtube_playback_enabled: bool,
    /// Absolute path to the managed `yt-dlp` release used for extraction.
    pub youtube_ytdlp_path: Option<PathBuf>,
    /// Optional absolute path to the mpv binary; the default is `mpv` on PATH.
    pub youtube_mpv_path: Option<PathBuf>,
    /// Bounded resolve-and-start budget in whole seconds.
    pub youtube_playback_timeout_secs: u64,
    /// Hardware decoding preference for the libmpv child.
    pub youtube_hwdec: YoutubeHwdec,
    pub airplay_enabled: bool,
    pub webrtc_enabled: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    pub artwork_cache_bytes: u64,
    pub share_titles: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            jellyfin_url: None,
            server_url: None,
            cloud_enabled: false,
            youtube_enabled: false,
            youtube_playback_enabled: false,
            youtube_ytdlp_path: None,
            youtube_mpv_path: None,
            youtube_playback_timeout_secs: DEFAULT_YOUTUBE_TIMEOUT_SECS,
            youtube_hwdec: YoutubeHwdec::AutoSafe,
            airplay_enabled: false,
            webrtc_enabled: false,
            max_width: 1280,
            max_height: 720,
            max_fps: 30,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 10_000_000,
            artwork_cache_bytes: MAX_ARTWORK_CACHE_BYTES,
            share_titles: false,
        }
    }
}

impl ClientConfig {
    /// Parse and validate a TOML document.
    pub fn from_toml_str(document: &str) -> Result<Self, ConfigError> {
        if document.len() > MAX_CONFIG_BYTES as usize {
            return Err(ConfigError::TooLarge);
        }
        // Never format the TOML error itself: its message quotes the offending
        // value, which may contain a credential-bearing URL. Only the parser
        // position is retained.
        let config: Self = toml::from_str(document).map_err(|error| {
            let span = error.span();
            let position = span.map(|span| toml_position(document, span.start));
            ConfigError::InvalidToml { position }
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Read and validate a configuration file with a bounded read.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let document = fs::read_to_string(path)?;
        Self::from_toml_str(&document)
    }

    /// Validate cross-field constraints and URL policy.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&self.max_width) {
            return Err(ConfigError::InvalidValue {
                field: "max_width",
                reason: "must be within 320..=7680",
            });
        }
        if !(MIN_HEIGHT..=MAX_HEIGHT).contains(&self.max_height) {
            return Err(ConfigError::InvalidValue {
                field: "max_height",
                reason: "must be within 240..=4320",
            });
        }
        if !(MIN_FPS..=MAX_FPS).contains(&self.max_fps) {
            return Err(ConfigError::InvalidValue {
                field: "max_fps",
                reason: "must be within 1..=120",
            });
        }
        if !(MIN_VIDEO_BITRATE_BPS..=MAX_VIDEO_BITRATE_BPS).contains(&self.bitrate_bps) {
            return Err(ConfigError::InvalidValue {
                field: "bitrate_bps",
                reason: "must be within 64000..=100000000",
            });
        }
        if !(MIN_VIDEO_BITRATE_BPS..=MAX_VIDEO_BITRATE_BPS).contains(&self.max_bitrate_bps) {
            return Err(ConfigError::InvalidValue {
                field: "max_bitrate_bps",
                reason: "must be within 64000..=100000000",
            });
        }
        if self.max_bitrate_bps < self.bitrate_bps {
            return Err(ConfigError::InvalidValue {
                field: "max_bitrate_bps",
                reason: "must be at least bitrate_bps",
            });
        }
        if self.artwork_cache_bytes > MAX_ARTWORK_CACHE_BYTES {
            return Err(ConfigError::InvalidValue {
                field: "artwork_cache_bytes",
                reason: "must not exceed 134217728",
            });
        }

        if let Some(url) = self.jellyfin_url.as_deref() {
            validate_url("jellyfin_url", url, true)?;
        }
        if let Some(url) = self.server_url.as_deref() {
            validate_url("server_url", url, false)?;
        }
        if !(MIN_YOUTUBE_TIMEOUT_SECS..=MAX_YOUTUBE_TIMEOUT_SECS)
            .contains(&self.youtube_playback_timeout_secs)
        {
            return Err(ConfigError::InvalidValue {
                field: "youtube_playback_timeout_secs",
                reason: "must be within 10..=300",
            });
        }
        if let Some(path) = self.youtube_ytdlp_path.as_deref() {
            validate_tool_path("youtube_ytdlp_path", path)?;
        }
        if let Some(path) = self.youtube_mpv_path.as_deref() {
            validate_tool_path("youtube_mpv_path", path)?;
        }
        if self.cloud_enabled && self.server_url.is_none() {
            return Err(ConfigError::Missing {
                field: "server_url",
                condition: "cloud_enabled is true",
            });
        }
        if (self.youtube_enabled || self.webrtc_enabled) && self.server_url.is_none() {
            return Err(ConfigError::Missing {
                field: "server_url",
                condition: "youtube_enabled or webrtc_enabled is true",
            });
        }
        Ok(())
    }

    /// Whether local YouTube playback may start.
    ///
    /// The kill switch is `youtube_playback_enabled`; it only takes effect
    /// when the managed extractor exists and is executable. A missing or
    /// invalid path therefore fails safe instead of surfacing a partial
    /// feature.
    #[must_use]
    pub fn youtube_playback_ready(&self) -> bool {
        self.youtube_playback_enabled
            && self
                .youtube_ytdlp_path
                .as_deref()
                .is_some_and(is_executable_file)
    }

    /// Configured resolve-and-start budget.
    #[must_use]
    pub fn youtube_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.youtube_playback_timeout_secs)
    }
}

/// Configuration loading and validation failures.
///
/// URL validation messages name only the field and never include the supplied
/// URL, so a token pasted into a URL is not copied into logs.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid TOML configuration{}", position_suffix(.position))]
    InvalidToml {
        /// One-based position reported by the parser, when it reports one.
        position: Option<TomlPosition>,
    },
    #[error("invalid value for {field}: {reason}")]
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
    #[error("{field} is required when {condition}")]
    Missing {
        field: &'static str,
        condition: &'static str,
    },
    #[error("configuration file is too large")]
    TooLarge,
    #[error("could not read configuration file")]
    Io(#[from] std::io::Error),
}

/// One-based position in a configuration document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TomlPosition {
    /// One-based line number.
    pub line: usize,
    /// One-based column number, counted in characters.
    pub column: usize,
}

impl fmt::Display for TomlPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}, column {}", self.line, self.column)
    }
}

fn position_suffix(position: &Option<TomlPosition>) -> String {
    (*position).map_or_else(String::new, |position| format!(" at {position}"))
}

fn toml_position(document: &str, offset: usize) -> TomlPosition {
    let prefix = document.get(..offset).unwrap_or(document);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit('\n')
        .next()
        .map_or(1, |line| line.chars().count() + 1);
    TomlPosition { line, column }
}

/// Default configuration path: `$XDG_CONFIG_HOME/tv-client/client.toml` or
/// `$HOME/.config/tv-client/client.toml`.
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(config_home).join("tv-client/client.toml"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config/tv-client/client.toml"))
}

fn validate_tool_path(field: &'static str, path: &Path) -> Result<(), ConfigError> {
    let raw = path.as_os_str();
    let bytes = raw.len();
    if bytes == 0 || bytes > MAX_TOOL_PATH_BYTES {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must be a non-empty absolute path of at most 1024 bytes",
        });
    }
    if !path.is_absolute() {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must be an absolute path",
        });
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must not contain parent-directory components",
        });
    }
    if !raw.to_str().is_some_and(|text| {
        !text.is_empty() && !text.chars().any(|character| character.is_control())
    }) {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must be valid UTF-8 without control characters",
        });
    }
    Ok(())
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn validate_url(
    field: &'static str,
    value: &str,
    allow_local_http: bool,
) -> Result<(), ConfigError> {
    if value.is_empty() || value.len() > MAX_URL_BYTES {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must be a non-empty absolute URL",
        });
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must not contain whitespace or control characters",
        });
    }
    let Some((scheme, remainder)) = value.split_once("://") else {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must include an explicit scheme",
        });
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && !(scheme == "http" && allow_local_http) {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must use https unless an explicitly local Jellyfin host uses http",
        });
    }

    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let tail = &remainder[authority_end..];
    if authority.is_empty() {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must include a host",
        });
    }
    if authority.contains('@') {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must not contain credentials",
        });
    }
    if tail.contains('?') || tail.contains('#') || tail.contains('\\') {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "must be an origin or path without query, fragment or backslashes",
        });
    }

    let (host, port) = split_authority(field, authority)?;
    if let Some(port) = port {
        let parsed: u16 = port.parse().map_err(|_| ConfigError::InvalidValue {
            field,
            reason: "port must be a number within 1..=65535",
        })?;
        if parsed == 0 {
            return Err(ConfigError::InvalidValue {
                field,
                reason: "port must be a number within 1..=65535",
            });
        }
    }
    validate_host(field, host)?;

    if scheme == "http" && !is_local_host(host) {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "http is only allowed for local Jellyfin hosts",
        });
    }
    Ok(())
}

fn split_authority<'a>(
    field: &'static str,
    authority: &'a str,
) -> Result<(&'a str, Option<&'a str>), ConfigError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(ConfigError::InvalidValue {
                field,
                reason: "IPv6 hosts must be enclosed in brackets",
            });
        };
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':').ok_or(ConfigError::InvalidValue {
                field,
                reason: "invalid bracketed host suffix",
            })?)
        };
        return Ok((host, port));
    }
    if authority.matches(':').count() > 1 {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "IPv6 hosts must be enclosed in brackets",
        });
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((host, Some(port))),
        None => Ok((authority, None)),
    }
}

fn validate_host(field: &'static str, host: &str) -> Result<(), ConfigError> {
    if host.is_empty() || host.len() > 253 {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "host must be non-empty and at most 253 bytes",
        });
    }
    if !host.is_ascii() || host.chars().any(char::is_control) {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "host must be ASCII without control characters",
        });
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty()
        || !host.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_')
        })
    {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "host contains unsupported characters",
        });
    }
    if host.split('.').any(str::is_empty) {
        return Err(ConfigError::InvalidValue {
            field,
            reason: "host labels must not be empty",
        });
    }
    Ok(())
}

fn is_local_host(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    if let Ok(address) = host.parse::<IpAddr>() {
        return is_local_address(address);
    }
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    !host.contains('.')
}

fn is_local_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        IpAddr::V6(address) => {
            if address.is_loopback() {
                return true;
            }
            let first = address.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}
