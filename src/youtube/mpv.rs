//! Private libmpv session over a per-session JSON IPC socket.
//!
//! mpv is started without a file and without `ytdl`; the resolved media URL is
//! sent only over the Unix socket, never on the command line. The socket lives
//! in a 0700 temporary directory that is removed when the session ends. No
//! token, media URL or child output is written to a log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;

use crate::config::YoutubeHwdec;
use crate::youtube::resolve::ResolvedMedia;

/// Longest accepted IPC line from mpv.
pub const MAX_IPC_LINE_BYTES: usize = 64 * 1024;
/// Default budget for mpv to create and accept its socket.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(10);
/// Default budget for mpv to exit on a stop.
pub const DEFAULT_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// mpv process configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpvOptions {
    /// mpv binary. The default deployment uses `mpv` from `PATH`.
    pub program: PathBuf,
    /// Fixed hardware-decoding preference.
    pub hwdec: YoutubeHwdec,
    /// Socket startup budget.
    pub start_timeout: Duration,
    /// Teardown budget.
    pub reap_timeout: Duration,
}

impl Default for MpvOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::from("mpv"),
            hwdec: YoutubeHwdec::AutoSafe,
            start_timeout: DEFAULT_START_TIMEOUT,
            reap_timeout: DEFAULT_REAP_TIMEOUT,
        }
    }
}

/// Commands the controller may send to one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCommand {
    /// Pause or resume.
    Pause(bool),
    /// Seek to an absolute position.
    Seek { position_ms: u64 },
    /// Set linear volume, 0 through 100.
    SetVolume { volume: u8 },
    /// Quit the backend and reap it.
    Stop,
}

/// Events reported by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    /// The media was opened and the first frame is pending.
    Loaded,
    /// A bounded media title.
    Title(String),
    /// Playback position.
    Position { position_ms: u64 },
    /// Media duration, when known.
    Duration { duration_ms: u64 },
    /// Pause state changed.
    Paused(bool),
    /// Playback reached the end of the media.
    Ended,
    /// The user explicitly stopped the local player.
    Stopped,
    /// The backend hit an error. `code` is never remote text.
    Failed { code: &'static str },
}

/// Backend failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// mpv could not be started or its socket never appeared.
    #[error("the local player is unavailable")]
    Unavailable,
    /// mpv did not become ready within the bound.
    #[error("the local player did not start in time")]
    Timeout,
    /// The IPC exchange was malformed or the child exited.
    #[error("the local player session failed")]
    Closed,
    /// The media URL was refused before it was sent.
    #[error("the media selection is invalid")]
    InvalidMedia,
}

impl BackendError {
    /// Stable code for the player screen.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "player_unavailable",
            Self::Timeout => "player_timeout",
            Self::Closed => "player_closed",
            Self::InvalidMedia => "player_invalid_media",
        }
    }
}

/// One supervised local player.
pub trait PlayerBackend: Send {
    /// Open resolved media and start playback.
    fn load<'a>(&'a mut self, media: &'a ResolvedMedia) -> BoxFuture<'a, Result<(), BackendError>>;

    /// Send a control command.
    fn command(&mut self, command: BackendCommand) -> BoxFuture<'_, Result<(), BackendError>>;

    /// Wait for the next meaningful event; `None` means the child closed.
    fn next_event(&mut self) -> BoxFuture<'_, Option<BackendEvent>>;

    /// Stop and reap the child, removing temporary resources.
    fn close(&mut self) -> BoxFuture<'_, ()>;
}

/// A backend factory owned by the player controller.
pub trait BackendFactory: Send + Sync {
    /// Start one backend with the configured options.
    fn start(&self) -> BoxFuture<'_, Result<Box<dyn PlayerBackend>, BackendError>>;
}

/// Production factory for [`MpvBackend`].
#[derive(Debug, Clone)]
pub struct MpvBackendFactory {
    options: MpvOptions,
}

impl MpvBackendFactory {
    /// Build a factory from mpv options.
    #[must_use]
    pub fn new(options: MpvOptions) -> Self {
        Self { options }
    }
}

impl BackendFactory for MpvBackendFactory {
    fn start(&self) -> BoxFuture<'_, Result<Box<dyn PlayerBackend>, BackendError>> {
        Box::pin(async move {
            let backend = MpvBackend::start(&self.options).await?;
            Ok(Box::new(backend) as Box<dyn PlayerBackend>)
        })
    }
}

/// mpv command line for one private IPC session.
///
/// The returned vector never contains a media URL, a token or a playlist
/// identifier. It is public so tests can assert that invariant.
#[must_use]
pub fn mpv_arguments(socket: &Path, hwdec: YoutubeHwdec, input_conf: &Path) -> Vec<String> {
    let hwdec = if hwdec == YoutubeHwdec::None {
        "none"
    } else {
        // dmabuf-wayland needs a dmabuf-capable decoder; the Pi's only
        // proven path is v4l2m2m. auto-safe probes VDPAU first and can fail.
        "v4l2m2m"
    };
    let mut arguments = [
        "--no-config".to_owned(),
        "--no-terminal".to_owned(),
        "--really-quiet".to_owned(),
        "--idle=yes".to_owned(),
        "--force-window=yes".to_owned(),
        "--fullscreen=yes".to_owned(),
        "--no-border".to_owned(),
        "--keep-open=no".to_owned(),
        "--no-resume-playback".to_owned(),
        "--save-position-on-quit=no".to_owned(),
        "--ytdl=no".to_owned(),
        "--osc=no".to_owned(),
        "--osd-level=0".to_owned(),
        "--vo=dmabuf-wayland".to_owned(),
        "--volume=50".to_owned(),
        format!("--hwdec={hwdec}"),
        format!("--input-ipc-server={}", socket.display()),
        format!("--input-conf={}", input_conf.display()),
    ]
    .to_vec();
    if std::env::var("TV_AUDIO_SINK").as_deref() == Ok("alsa") {
        arguments.push("--ao=alsa".to_owned());
        if let Ok(device) = std::env::var("TV_PLAYER_AUDIO_DEVICE")
            && !device.trim().is_empty()
        {
            arguments.push(format!("--audio-device={}", device.trim()));
        } else if let Ok(device) = std::env::var("TV_AUDIO_DEVICE")
            && !device.trim().is_empty()
            && (device.trim().starts_with("alsa/") || device.trim() == "auto")
        {
            arguments.push(format!("--audio-device={}", device.trim()));
        }
    }
    arguments
}

fn write_input_conf(path: &Path) -> Result<(), BackendError> {
    let contents = "\
ESC quit
SPACE cycle pause
LEFT seek -10
RIGHT seek 10
UP add volume 5
DOWN add volume -5
";
    std::fs::write(path, contents).map_err(|_| BackendError::Unavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Removes a temporary private socket directory unless ownership is moved into
/// a constructed [`MpvBackend`].
struct SocketGuard {
    path: Option<PathBuf>,
}

impl SocketGuard {
    fn create(path: PathBuf) -> Result<Self, BackendError> {
        std::fs::create_dir_all(&path).map_err(|_| BackendError::Unavailable)?;
        Ok(Self { path: Some(path) })
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("socket guard is disarmed only on success")
    }

    fn take(&mut self) -> PathBuf {
        self.path
            .take()
            .expect("socket guard is disarmed only once")
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// A live mpv child and its IPC connection.
pub struct MpvBackend {
    child: tokio::process::Child,
    writer: OwnedWriteHalf,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    socket_directory: PathBuf,
    reap_timeout: Duration,
    pending_audio: Option<String>,
    closed: bool,
}

impl MpvBackend {
    /// Start mpv with a private socket and wait until it accepts commands.
    pub async fn start(options: &MpvOptions) -> Result<Self, BackendError> {
        let mut guard = SocketGuard::create(
            std::env::temp_dir().join(format!("tv-client-mpv-{}", uuid::Uuid::new_v4().simple())),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(guard.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(|_| BackendError::Unavailable)?;
        }
        let socket = guard.path().join("ipc.sock");
        if socket.as_os_str().len() >= 100 {
            return Err(BackendError::Unavailable);
        }
        let input_conf = guard.path().join("input.conf");
        write_input_conf(&input_conf)?;
        let arguments = mpv_arguments(&socket, options.hwdec, &input_conf);
        let mut child = tokio::process::Command::new(&options.program)
            .args(&arguments)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| BackendError::Unavailable)?;

        let deadline = tokio::time::Instant::now() + options.start_timeout;
        let stream = loop {
            match UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(_) => {
                    if let Ok(Some(_)) = child.try_wait() {
                        return Err(BackendError::Unavailable);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        let _ = child.start_kill();
                        return Err(BackendError::Timeout);
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        };
        let (read, write) = stream.into_split();
        let mut backend = Self {
            child,
            writer: write,
            reader: BufReader::new(read),
            socket_directory: guard.take(),
            reap_timeout: options.reap_timeout,
            pending_audio: None,
            closed: false,
        };
        for (id, property) in [
            (1, "time-pos"),
            (2, "duration"),
            (3, "pause"),
            (4, "media-title"),
        ] {
            backend
                .send(&json!({"command": ["observe_property", id, property]}))
                .await?;
        }
        Ok(backend)
    }

    async fn send(&mut self, value: &Value) -> Result<(), BackendError> {
        let mut line = serde_json::to_vec(value).map_err(|_| BackendError::Closed)?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|_| BackendError::Closed)?;
        self.writer.flush().await.map_err(|_| BackendError::Closed)
    }

    async fn read_line(&mut self) -> Result<Option<String>, BackendError> {
        let mut collected = Vec::new();
        loop {
            let (take, had_newline, empty) = {
                let available = self
                    .reader
                    .fill_buf()
                    .await
                    .map_err(|_| BackendError::Closed)?;
                if available.is_empty() {
                    (0, false, true)
                } else {
                    let take = available
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map_or(available.len(), |position| position + 1);
                    if collected.len().saturating_add(take) > MAX_IPC_LINE_BYTES {
                        return Err(BackendError::Closed);
                    }
                    collected.extend_from_slice(&available[..take]);
                    (take, available[take - 1] == b'\n', false)
                }
            };
            if empty {
                if collected.is_empty() {
                    return Ok(None);
                }
                break;
            }
            self.reader.consume(take);
            if had_newline {
                break;
            }
        }
        let _ = collected.pop();
        String::from_utf8(collected)
            .map(Some)
            .map_err(|_| BackendError::Closed)
    }
}

impl PlayerBackend for MpvBackend {
    fn load<'a>(&'a mut self, media: &'a ResolvedMedia) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            self.pending_audio = media.audio_url.clone();
            self.send(&json!({"command": ["loadfile", media.video_url, "replace"]}))
                .await?;
            self.send(&json!({"command": ["set_property", "pause", false]}))
                .await
        })
    }

    fn command(&mut self, command: BackendCommand) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move {
            match command {
                BackendCommand::Pause(paused) => {
                    self.send(&json!({"command": ["set_property", "pause", paused]}))
                        .await
                }
                BackendCommand::Seek { position_ms } => {
                    let seconds = position_ms as f64 / 1000.0;
                    self.send(&json!({"command": ["seek", seconds, "absolute+exact"]}))
                        .await
                }
                BackendCommand::SetVolume { volume } => {
                    self.send(&json!({"command": ["set_property", "volume", volume]}))
                        .await
                }
                BackendCommand::Stop => {
                    let _ = self.send(&json!({"command": ["quit"]})).await;
                    self.reap().await;
                    Ok(())
                }
            }
        })
    }

    fn next_event(&mut self) -> BoxFuture<'_, Option<BackendEvent>> {
        Box::pin(async move {
            loop {
                let line = match self.read_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => return None,
                    Err(_) => return Some(BackendEvent::Failed { code: "player_ipc" }),
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match value.get("event").and_then(Value::as_str) {
                    Some("file-loaded") => {
                        if let Some(audio_url) = self.pending_audio.take() {
                            let _ = self
                                .send(&json!({"command": ["audio-add", audio_url]}))
                                .await;
                        }
                        return Some(BackendEvent::Loaded);
                    }
                    Some("end-file") => {
                        let reason = value.get("reason").and_then(Value::as_str);
                        return match reason {
                            Some("eof") => Some(BackendEvent::Ended),
                            Some("stop") | Some("quit") => Some(BackendEvent::Stopped),
                            _ => Some(BackendEvent::Failed {
                                code: "player_decode",
                            }),
                        };
                    }
                    Some("property-change") => {
                        let name = value.get("name").and_then(Value::as_str);
                        let data = value.get("data");
                        match name {
                            Some("time-pos") => {
                                let seconds = data.and_then(Value::as_f64).unwrap_or(0.0);
                                return Some(BackendEvent::Position {
                                    position_ms: (seconds.max(0.0) * 1000.0).round() as u64,
                                });
                            }
                            Some("duration") => {
                                if let Some(seconds) = data.and_then(Value::as_f64) {
                                    let duration_ms = (seconds.max(0.0) * 1000.0).round() as u64;
                                    if duration_ms > 0 {
                                        return Some(BackendEvent::Duration { duration_ms });
                                    }
                                }
                            }
                            Some("pause") => {
                                if let Some(paused) = data.and_then(Value::as_bool) {
                                    return Some(BackendEvent::Paused(paused));
                                }
                            }
                            Some("media-title") => {
                                if let Some(title) = data.and_then(Value::as_str) {
                                    let title = bounded_title(title);
                                    if !title.is_empty() {
                                        return Some(BackendEvent::Title(title));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        })
    }

    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self.closed {
                return;
            }
            self.closed = true;
            let _ = self.send(&json!({"command": ["quit"]})).await;
            self.reap().await;
            let _ = std::fs::remove_dir_all(&self.socket_directory);
        })
    }
}

impl MpvBackend {
    async fn reap(&mut self) {
        let wait = tokio::time::timeout(self.reap_timeout, self.child.wait()).await;
        if wait.is_err() {
            let _ = self.child.start_kill();
            let _ = tokio::time::timeout(self.reap_timeout, self.child.wait()).await;
        }
    }
}

impl Drop for MpvBackend {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.socket_directory);
    }
}

fn bounded_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_never_carry_a_media_url_or_token() {
        let arguments = mpv_arguments(
            Path::new("/tmp/tv-client-mpv-test/ipc.sock"),
            YoutubeHwdec::V4l2m2m,
            Path::new("/tmp/tv-client-mpv-test/input.conf"),
        );
        assert!(!arguments.iter().any(|argument| argument.contains("http")));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.contains("youtube"))
        );
        assert!(arguments.iter().any(|argument| argument == "--ytdl=no"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--hwdec=v4l2m2m")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--input-ipc-server=/tmp/tv-client-mpv-test/ipc.sock")
        );
    }
}
