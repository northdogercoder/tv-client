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
