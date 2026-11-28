//! Local `yt-dlp` resolution with a fixed, shell-free argument vector.
//!
//! The extractor receives exactly one watch URL built from a validated
//! [`VideoId`]. It is asked to resolve metadata only (`--simulate`), with no
//! playlist expansion, no configuration file and no cache directory. Child
//! stdout and stderr are bounded; stderr is classified into a typed reason and
//! never logged or rendered, because it can contain a media URL or a challenge
//! page.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use url::Url;

use crate::jellyfin::CancelToken;
use crate::youtube::VideoId;

/// Largest accepted extractor stdout.
pub const MAX_INFO_BYTES: usize = 2 * 1024 * 1024;
/// Largest accepted extractor stderr used for classification.
pub const MAX_ERROR_BYTES: usize = 8 * 1024;
/// Budget for one managed yt-dlp self-update.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(45);
/// Do not start another self-update within this window after a failed attempt.
pub const UPDATE_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// Fixed socket timeout passed to yt-dlp.
const SOCKET_TIMEOUT_SECS: &str = "15";

type ExtractorTask =
    tokio::task::JoinHandle<Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus), ExtractorError>>;

/// Fixed H.264 selection: 720p30 first, a muxed progressive fallback, then
/// 1080p30 only when no 720p option exists. VP9 and AV1 are never preferred;
/// 1080p60 and 720p60 are excluded. The order follows measured Pi evidence:
/// 1080p30 dropped roughly 72% of frames while 720p30 was about 19%, so 720p30
/// is the least-bad option until the renderer is fixed.
pub const FORMAT_SELECTOR: &str = concat!(
    "bv*[height<=720][fps<=30][vcodec^=avc1]+ba[acodec^=mp4a]",
    "/b[height<=720][fps<=30][vcodec^=avc1][acodec^=mp4a]",
    "/bv*[height<=1080][fps<=30][vcodec^=avc1]+ba[acodec^=mp4a]",
    "/b[height<=720][fps<=30][vcodec^=avc1]"
);

/// A resolved, still-remote H.264 media selection.
///
/// The URLs are time-limited CDN addresses. They live only in this value for
/// the duration of one playback session; `Debug` redacts them so an accidental
/// `{:?}` cannot copy one into a log.
#[derive(Clone, PartialEq)]
pub struct ResolvedMedia {
    /// Video stream URL.
    pub video_url: String,
    /// Separate audio stream URL, when the selection is not muxed.
    pub audio_url: Option<String>,
    /// Selected video codec.
    pub vcodec: String,
    /// Selected audio codec.
    pub acodec: Option<String>,
    /// Video width in pixels.
    pub width: u32,
    /// Video height in pixels.
    pub height: u32,
    /// Frames per second.
    pub fps: f64,
    /// Extraction format identifier.
    pub format_id: String,
    /// Optional title reported by the extractor.
    pub title: Option<String>,
    /// Optional duration in milliseconds.
    pub duration_ms: Option<u64>,
}

impl std::fmt::Debug for ResolvedMedia {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedMedia")
            .field("vcodec", &self.vcodec)
            .field("acodec", &self.acodec)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("format_id", &self.format_id)
            .field("video_url", &"redacted")
            .field(
                "audio_url",
                &if self.audio_url.is_some() {
                    "redacted"
                } else {
                    "none"
                },
            )
            .finish()
    }
}

impl ResolvedMedia {
    /// Human-readable method label for the player screen.
    #[must_use]
    pub fn method(&self) -> String {
        format!(
            "yt-dlp · H.264 {}p{:.0} · mpv",
            self.height,
            self.fps.round()
        )
    }
}

/// Why the managed extractor could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ExtractorError {
    /// The configured extractor is missing or unusable.
    #[error("the managed yt-dlp extractor is unavailable")]
    ExtractorUnavailable {
        /// Classified reason.
        reason: ExtractorFailure,
    },
    /// The extractor ran but reported a failure.
    #[error("yt-dlp reported an extraction failure")]
    Failed {
        /// Classified reason.
        reason: ExtractorFailure,
    },
    /// The extractor exceeded its bounded timeout.
    #[error("yt-dlp resolution timed out")]
    Timeout,
    /// A newer request or a stop cancelled this resolution.
    #[error("yt-dlp resolution was cancelled")]
    Cancelled,
    /// The extractor output did not match the expected shape.
    #[error("yt-dlp output was not a usable single video")]
    InvalidOutput,
    /// The selected format is not the required H.264 profile.
    #[error("the available format is not supported")]
    UnsupportedFormat,
}

impl ExtractorError {
    /// Stable code for the player and trace fields.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ExtractorUnavailable { .. } => "extractor_unavailable",
            Self::Failed { .. } => "extractor_failed",
            Self::Timeout => "extractor_timeout",
            Self::Cancelled => "extractor_cancelled",
            Self::InvalidOutput => "extractor_invalid_output",
            Self::UnsupportedFormat => "unsupported_format",
        }
    }

    /// Whether an explicit operator retry can help.
    #[must_use]
    pub const fn retryable(self) -> bool {
        !matches!(self, Self::UnsupportedFormat | Self::Cancelled)
    }
}

/// Classified extractor failure, derived without keeping child output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractorFailure {
    /// The configured binary could not be spawned.
    MissingBinary,
    /// YouTube asked for sign-in or a bot check.
    Blocked,
    /// The extractor is stale and needs an update.
    Stale,
    /// The configured extractor could not update itself.
    UpdateFailed,
    /// YouTube rate-limited this address.
    RateLimited,
    /// The video is unavailable, private or removed.
    VideoUnavailable,
    /// The URL expanded to a playlist, which is never accepted.
    Playlist,
    /// Unclassified extraction failure.
    Unknown,
}

impl ExtractorFailure {
    /// Stable code suffix.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingBinary => "missing_binary",
            Self::Blocked => "blocked",
            Self::Stale => "stale_extractor",
            Self::UpdateFailed => "update_failed",
            Self::RateLimited => "rate_limited",
            Self::VideoUnavailable => "video_unavailable",
            Self::Playlist => "playlist_rejected",
            Self::Unknown => "unknown",
        }
    }
}

/// Resolution backend used by the player.
pub trait Extractor: Send + Sync {
    /// Resolve one validated video identifier into a remote media selection.
    fn resolve<'a>(
        &'a self,
        video_id: &'a VideoId,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, Result<ResolvedMedia, ExtractorError>>;
}

/// Production extractor backed by an operator-managed `yt-dlp`.
#[derive(Debug, Clone)]
pub struct YtdlpExtractor {
    program: PathBuf,
    timeout: Duration,
    update_timeout: Duration,
    last_update: Arc<Mutex<Option<Instant>>>,
}

impl YtdlpExtractor {
    /// Build an extractor with an explicit program and bound.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            update_timeout: timeout.min(UPDATE_TIMEOUT),
            timeout,
            last_update: Arc::new(Mutex::new(None)),
        }
    }

    /// Configured program path.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Configured timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl YtdlpExtractor {
    async fn resolve_once(
        &self,
        video_id: &VideoId,
        cancel: &CancelToken,
    ) -> Result<ResolvedMedia, ExtractorError> {
        if cancel.is_cancelled() {
            return Err(ExtractorError::Cancelled);
        }
        let url = video_id.watch_url();
        let arguments = build_ytdlp_arguments(&url);
        let (stdout, stderr, status) =
            run_bounded(&self.program, &arguments, self.timeout, cancel).await?;
        if !status.success() {
            return Err(ExtractorError::Failed {
                reason: classify_failure(&stderr),
            });
        }
        parse_resolved_media(&stdout)
    }

    fn claim_update(&self) -> bool {
        let mut last = self
            .last_update
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < UPDATE_COOLDOWN) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    async fn update(&self, cancel: &CancelToken) -> Result<(), ExtractorError> {
        let arguments = build_update_arguments();
        match run_bounded(&self.program, &arguments, self.update_timeout, cancel).await {
            Ok((_, _, status)) if status.success() => return Ok(()),
            Err(ExtractorError::Cancelled) => return Err(ExtractorError::Cancelled),
            Ok(_) | Err(_) => {}
        }
        if cancel.is_cancelled() {
            return Err(ExtractorError::Cancelled);
        }
        // A managed venv install cannot self-update with `-U`; its sibling
        // pip is the documented updater. Both commands are fixed, bounded and
        // shell-free.
        let pip = self
            .program
            .parent()
            .map(|parent| parent.join("pip"))
            .filter(|path| path.is_file());
        let Some(pip) = pip else {
            return Err(ExtractorError::Failed {
                reason: ExtractorFailure::UpdateFailed,
            });
        };
        let arguments = [
            "install",
            "--disable-pip-version-check",
            "--upgrade",
            "yt-dlp",
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
        match run_bounded(&pip, &arguments, self.update_timeout, cancel).await {
            Ok((_, _, status)) if status.success() => Ok(()),
            Ok(_) => Err(ExtractorError::Failed {
                reason: ExtractorFailure::UpdateFailed,
            }),
            Err(ExtractorError::Cancelled) => Err(ExtractorError::Cancelled),
            Err(_) => Err(ExtractorError::Failed {
                reason: ExtractorFailure::UpdateFailed,
            }),
        }
    }
}

impl Extractor for YtdlpExtractor {
    fn resolve<'a>(
        &'a self,
        video_id: &'a VideoId,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, Result<ResolvedMedia, ExtractorError>> {
        Box::pin(async move {
            let first = self.resolve_once(video_id, cancel).await;
            let stale = matches!(
                first,
                Err(ExtractorError::Failed {
                    reason: ExtractorFailure::Stale
                }) | Err(ExtractorError::ExtractorUnavailable {
                    reason: ExtractorFailure::Stale
                })
            );
            if !stale {
                return first;
            }
            if cancel.is_cancelled() {
                return Err(ExtractorError::Cancelled);
            }
            if !self.claim_update() {
                return first;
            }
            self.update(cancel).await?;
            self.resolve_once(video_id, cancel).await
        })
    }
}

/// Build the fixed self-update argument vector.
///
/// No URL, config file or caller-supplied flag reaches this command.
#[must_use]
pub fn build_update_arguments() -> Vec<String> {
    ["--ignore-config", "--no-warnings", "--update"]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect()
}

/// Build the fixed extractor argument vector.
///
/// The single URL argument is constructed by [`VideoId::watch_url`]; callers
/// cannot add flags, cookies, accounts, playlists or a cache directory.
#[must_use]
pub fn build_ytdlp_arguments(watch_url: &str) -> Vec<String> {
    [
        "--ignore-config",
        "--no-playlist",
        "--no-cache-dir",
        "--no-warnings",
        "--no-progress",
        "--simulate",
        "--dump-single-json",
        "--socket-timeout",
        SOCKET_TIMEOUT_SECS,
        "--format",
        FORMAT_SELECTOR,
    ]
    .iter()
    .map(|argument| (*argument).to_owned())
    .chain(std::iter::once(watch_url.to_owned()))
    .collect()
}

async fn run_bounded(
    program: &Path,
    arguments: &[String],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus), ExtractorError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    // A busy host can transiently fail fork/exec. Retry only non-permanent
    // spawn errors; a genuinely missing or non-executable binary still fails.
    let mut spawn_attempts = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) && spawn_attempts < 2 =>
            {
                spawn_attempts += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(_) => {
                return Err(ExtractorError::ExtractorUnavailable {
                    reason: ExtractorFailure::MissingBinary,
                });
            }
        }
    };

    let mut stdout = child.stdout.take().ok_or(ExtractorError::InvalidOutput)?;
    let mut stderr = child.stderr.take().ok_or(ExtractorError::InvalidOutput)?;
    let mut cancel = cancel.clone();

    let mut task = tokio::spawn(async move {
        let (stdout, stderr) = tokio::try_join!(
            read_capped(&mut stdout, MAX_INFO_BYTES),
            read_capped(&mut stderr, MAX_ERROR_BYTES)
        )?;
        let status = child
            .wait()
            .await
            .map_err(|_| ExtractorError::InvalidOutput)?;
        Ok::<_, ExtractorError>((stdout, stderr, status))
    });

    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            abort_and_reap(&mut task).await;
            Err(ExtractorError::Cancelled)
        }
        result = tokio::time::timeout(timeout, &mut task) => {
            match result {
                Err(_) => {
                    abort_and_reap(&mut task).await;
                    Err(ExtractorError::Timeout)
                }
                Ok(Err(_)) => Err(ExtractorError::InvalidOutput),
                Ok(Ok(result)) => result,
            }
        }
    }
}

async fn abort_and_reap(task: &mut ExtractorTask) {
    task.abort();
    // Poll until the aborted future has dropped its Child. The child has
    // kill_on_drop set, so this is the point at which the process is reaped
    // rather than merely detached.
    let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
}

async fn read_capped<R>(reader: &mut R, cap: usize) -> Result<Vec<u8>, ExtractorError>
where
    R: AsyncRead + Unpin,
{
    let mut collected = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|_| ExtractorError::InvalidOutput)?;
        if read == 0 {
            return Ok(collected);
        }
        if collected.len().saturating_add(read) > cap {
            return Err(ExtractorError::InvalidOutput);
        }
        collected.extend_from_slice(&chunk[..read]);
    }
}

fn classify_failure(stderr: &[u8]) -> ExtractorFailure {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if text.contains("sign in to confirm")
        || text.contains("not a bot")
        || text.contains("confirm your age")
    {
        ExtractorFailure::Blocked
    } else if text.contains("page needs to be reloaded") {
        ExtractorFailure::Stale
    } else if text.contains("http error 429") || text.contains("too many requests") {
        ExtractorFailure::RateLimited
    } else if text.contains("private video")
        || text.contains("video unavailable")
        || text.contains("has been removed")
    {
        ExtractorFailure::VideoUnavailable
    } else if text.contains("playlist") {
        ExtractorFailure::Playlist
    } else {
        ExtractorFailure::Unknown
    }
}

/// Parse one `yt-dlp --dump-single-json` document into the fixed format policy.
pub fn parse_resolved_media(json: &[u8]) -> Result<ResolvedMedia, ExtractorError> {
    let info: RawInfo = serde_json::from_slice(json).map_err(|_| ExtractorError::InvalidOutput)?;
    if info.kind.as_deref() == Some("playlist") || info.entries.is_some() {
        return Err(ExtractorError::Failed {
            reason: ExtractorFailure::Playlist,
        });
    }
    let requested = info.requested_formats.as_deref().unwrap_or_default();
    let (video, audio) = if requested.is_empty() {
        let video = RawFormat {
            url: info.url.clone(),
            vcodec: info.vcodec.clone(),
            acodec: info.acodec.clone(),
            height: info.height,
            width: info.width,
            fps: info.fps,
            format_id: info.format_id.clone(),
        };
        (video, None)
    } else {
        let video = requested
            .iter()
            .find(|format| format.vcodec.as_deref().is_some_and(is_avc))
            .ok_or(ExtractorError::UnsupportedFormat)?;
        let audio = requested
            .iter()
            .find(|format| format.acodec.as_deref().is_some_and(is_mp4a));
        (video.clone(), audio.cloned())
    };

    let video_codec = video
        .vcodec
        .as_deref()
        .ok_or(ExtractorError::InvalidOutput)?;
    if !is_avc(video_codec) {
        return Err(ExtractorError::UnsupportedFormat);
    }
    let video_url = video.url.as_deref().ok_or(ExtractorError::InvalidOutput)?;
    validate_media_url(video_url)?;
    let height = video.height.unwrap_or(info.height.unwrap_or(0));
    let width = video.width.unwrap_or(info.width.unwrap_or(0));
    let fps = video.fps.or(info.fps).unwrap_or(30.0);
    if !(1..=1080).contains(&height) || width == 0 {
        return Err(ExtractorError::UnsupportedFormat);
    }
    if height > 720 && fps > 30.5 {
        return Err(ExtractorError::UnsupportedFormat);
    }
    let audio_url = match audio.as_ref() {
        Some(audio) => {
            let codec = audio.acodec.as_deref().unwrap_or_default();
            if !is_mp4a(codec) {
                return Err(ExtractorError::UnsupportedFormat);
            }
            let url = audio.url.as_deref().ok_or(ExtractorError::InvalidOutput)?;
            validate_media_url(url)?;
            Some(url.to_owned())
        }
        None => {
            // A single progressive format must carry AAC audio; a video-only
            // entry would play silently and is refused instead.
            if !video.acodec.as_deref().is_some_and(is_mp4a) {
                return Err(ExtractorError::UnsupportedFormat);
            }
            None
        }
    };
    let acodec = audio
        .as_ref()
        .and_then(|audio| audio.acodec.clone())
        .or_else(|| info.acodec.clone());
    Ok(ResolvedMedia {
        video_url: video_url.to_owned(),
        audio_url,
        vcodec: video_codec.to_owned(),
        acodec,
        width,
        height,
        fps,
        format_id: video
            .format_id
            .or_else(|| info.format_id.clone())
            .unwrap_or_default(),
        title: info.title,
        duration_ms: info
            .duration
            .filter(|duration| duration.is_finite() && *duration >= 0.0)
            .map(|duration| (duration * 1000.0).round() as u64),
    })
}

fn validate_media_url(value: &str) -> Result<(), ExtractorError> {
    let url = Url::parse(value).map_err(|_| ExtractorError::InvalidOutput)?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(ExtractorError::InvalidOutput);
    }
    Ok(())
}

fn is_avc(codec: &str) -> bool {
    codec.to_ascii_lowercase().starts_with("avc1")
}

fn is_mp4a(codec: &str) -> bool {
    codec.to_ascii_lowercase().starts_with("mp4a")
}

#[derive(Deserialize)]
struct RawInfo {
    #[serde(rename = "_type")]
    kind: Option<String>,
    entries: Option<serde_json::Value>,
    url: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    height: Option<u32>,
    width: Option<u32>,
    fps: Option<f64>,
    format_id: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    requested_formats: Option<Vec<RawFormat>>,
}

#[derive(Clone, Deserialize)]
struct RawFormat {
    url: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    height: Option<u32>,
    width: Option<u32>,
    fps: Option<f64>,
    format_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const AV1_VIDEO: &str = "https://rr1---sn.example.googlevideo.com/videoplayback?v=1";
    const AUDIO: &str = "https://rr1---sn.example.googlevideo.com/videoplayback?m=audio";

    fn info(vcodec: &str, height: u32, fps: f64) -> String {
        serde_json::json!({
            "url": AV1_VIDEO,
            "vcodec": vcodec,
            "acodec": "mp4a.40.2",
            "height": height,
            "width": 1920,
            "fps": fps,
            "format_id": "137",
            "title": "Example",
            "duration": 12.5
        })
        .to_string()
    }

    #[test]
    fn arguments_are_fixed_and_shell_free() {
        let id = VideoId::parse("M7lc1UVf-VE").expect("id");
        let arguments = build_ytdlp_arguments(&id.watch_url());
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("https://www.youtube.com/watch?v=M7lc1UVf-VE")
        );
        assert!(arguments.contains(&"--no-playlist".to_owned()));
        assert!(arguments.contains(&"--no-cache-dir".to_owned()));
        assert!(arguments.contains(&"--ignore-config".to_owned()));
        assert!(arguments.contains(&FORMAT_SELECTOR.to_owned()));
        assert!(!arguments.iter().any(|argument| argument.contains("cookie")));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.contains("browser"))
        );
        assert!(!FORMAT_SELECTOR.contains("vp9"));
        assert!(!FORMAT_SELECTOR.contains("av01"));
        assert!(FORMAT_SELECTOR.contains("fps<=30"));
    }

    #[test]
    fn h264_1080p30_is_accepted_and_1080p60_is_rejected() {
        let media = parse_resolved_media(info("avc1.640028", 1080, 30.0).as_bytes())
            .expect("1080p30 accepted");
        assert_eq!(media.height, 1080);
        assert_eq!(media.vcodec, "avc1.640028");
        assert_eq!(
            parse_resolved_media(info("avc1.640028", 1080, 60.0).as_bytes()),
            Err(ExtractorError::UnsupportedFormat)
        );
    }

    #[test]
    fn vp9_and_av1_are_not_selected() {
        assert_eq!(
            parse_resolved_media(info("vp9", 1080, 30.0).as_bytes()),
            Err(ExtractorError::UnsupportedFormat)
        );
        assert_eq!(
            parse_resolved_media(info("av01.0.08M.08", 1080, 30.0).as_bytes()),
            Err(ExtractorError::UnsupportedFormat)
        );
    }

    #[test]
    fn split_h264_and_mp4a_formats_are_combined() {
        let json = serde_json::json!({
            "requested_formats": [
                {"url": AV1_VIDEO, "vcodec": "avc1.640028", "acodec": "none", "height": 1080, "width": 1920, "fps": 30.0, "format_id": "137"},
                {"url": AUDIO, "vcodec": "none", "acodec": "mp4a.40.2", "format_id": "140"}
            ],
            "title": "Example"
        })
        .to_string();
        let media = parse_resolved_media(json.as_bytes()).expect("combined");
        assert_eq!(media.audio_url.as_deref(), Some(AUDIO));
        assert_eq!(media.acodec.as_deref(), Some("mp4a.40.2"));
    }

    #[test]
    fn debug_never_prints_media_urls() {
        let media = parse_resolved_media(info("avc1.640028", 720, 30.0).as_bytes()).expect("media");
        let text = format!("{media:?}");
        assert!(!text.contains("googlevideo"));
        assert!(text.contains("redacted"));
    }

    #[test]
    fn playlist_output_is_refused() {
        let json = serde_json::json!({"_type": "playlist", "entries": [{"id": "x"}]}).to_string();
        assert_eq!(
            parse_resolved_media(json.as_bytes()),
            Err(ExtractorError::Failed {
                reason: ExtractorFailure::Playlist
            })
        );
    }
}
