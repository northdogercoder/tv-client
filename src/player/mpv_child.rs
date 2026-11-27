//! Minimal mpv-backed implementation of the native-player child protocol.
//!
//! This mode exists because Cage's xdg-shell surfaces cannot composite the
//! Slint transparent control window reliably. mpv owns one window, renders its
//! native OSC (mouse show/hide, seek, elapsed/remaining, play/pause) and thus
//! needs no video filter and no second full-screen surface.
//!
//! The URL and every authentication header travel only through mpv's private
//! JSON IPC socket. They are never placed in argv, the environment or logs.
//! The child still speaks the same line-delimited [`PlayerRequest`] /
//! [`PlayerEvent`] protocol as the GStreamer child, so the parent controller and
//! supervision are unchanged.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::player::{self, LoadRequest, PlayerEvent, PlayerRequest};

const START_TIMEOUT: Duration = Duration::from_secs(4);
const STOP_TIMEOUT: Duration = Duration::from_millis(700);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_IPC_BYTES: usize = 16 * 1024;

enum Inbound {
    Request(PlayerRequest),
    Eof,
}

/// Run the mpv child until stop, EOF or natural exit.
pub fn run() -> Result<(), &'static str> {
    let (sender, receiver) = mpsc::channel::<Inbound>();
    std::thread::Builder::new()
        .name("mpv-child-stdin".to_string())
        .spawn(move || read_stdin(sender))
        .map_err(|_| "stdin reader failed")?;

    let mut session: Option<MpvSession> = None;
    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Inbound::Request(request)) => match request {
                PlayerRequest::Load(load) => {
                    if let Some(mut previous) = session.take() {
                        previous.stop();
                    }
                    let mut next = match MpvSession::start(&load) {
                        Ok(session) => session,
                        Err(error) => {
                            emit(PlayerEvent::Error {
                                code: "mpv_unavailable".to_string(),
                                message: Some(error.to_string()),
                            });
                            return Ok(());
                        }
                    };
                    emit(PlayerEvent::Ready {
                        decoder: None,
                        sink: Some("mpv-osc".to_string()),
                    });
                    next.emit_start_position();
                    session = Some(next);
                }
                PlayerRequest::Pause => {
                    if let Some(session) = session.as_mut() {
                        session.set_paused(true);
                    }
                }
                PlayerRequest::Resume => {
                    if let Some(session) = session.as_mut() {
                        session.set_paused(false);
                    }
                }
                PlayerRequest::Seek { position_ms } => {
                    if let Some(session) = session.as_mut() {
                        session.seek(position_ms);
                    }
                }
                PlayerRequest::SetVolume { volume } => {
                    if let Some(session) = session.as_mut() {
                        session.set_volume(volume);
                    }
                }
                PlayerRequest::Stop => {
                    if let Some(mut session) = session.take() {
                        session.stop();
                    }
                    emit(PlayerEvent::Stopped);
                    return Ok(());
                }
            },
            Ok(Inbound::Eof) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(mut session) = session.take() {
                    session.stop();
                }
                emit(PlayerEvent::Stopped);
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        let events = session
            .as_mut()
            .map(MpvSession::poll_events)
            .unwrap_or_default();
        let ended = events
            .iter()
            .any(|event| matches!(event, PlayerEvent::Ended));
        for event in events {
            emit(event);
        }
        if ended {
            if let Some(mut active) = session.take() {
                active.stop();
            }
            return Ok(());
        }

        if let Some(active) = session.as_mut()
            && active.exited()
        {
            emit(PlayerEvent::Ended);
            return Ok(());
        }
    }
}

fn read_stdin(sender: mpsc::Sender<Inbound>) {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = sender.send(Inbound::Eof);
                return;
            }
            Ok(_) if line.len() <= player::MAX_LINE_BYTES + 1 => {
                let request = player::decode_request_line(line.trim_end_matches(['\r', '\n']));
                if let Ok(request) = request
                    && sender.send(Inbound::Request(request)).is_err()
                {
                    return;
                }
            }
            Ok(_) | Err(_) => return,
        }
    }
}

fn emit(event: PlayerEvent) {
    if let Ok(mut line) = serde_json::to_string(&event.sanitized()) {
        line.push('\n');
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(line.as_bytes());
        let _ = lock.flush();
    }
}

struct MpvSession {
    child: Child,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    directory: PathBuf,
    start_position_ms: Option<u64>,
    duration_ms: Option<u64>,
    position_ms: u64,
    last_position_emit: Instant,
    stopped: bool,
    ended: bool,
}

impl MpvSession {
    fn start(load: &LoadRequest) -> Result<Self, &'static str> {
        let directory = std::env::temp_dir().join(format!(
            "tv-client-mpv-child-{}-{}",
            std::process::id(),
            monotonic_nonce()
        ));
        std::fs::create_dir_all(&directory).map_err(|_| "mpv temp directory failed")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
        }
        let socket = directory.join("ipc.sock");
        let input_conf = directory.join("input.conf");
        write_input_conf(&input_conf)?;
        let arguments = build_arguments(&socket, &input_conf);

        // URL-free diagnosis only: mpv expands these properties into a line
        // per frame. The file is world-readable so the operator can inspect
        // decoder/resolution/drop evidence while playback is live; it never
        // contains the media URL or headers.
        let status_file = std::fs::File::create("/tmp/tv-client-mpv-status.log").ok();
        let mut child = Command::new("mpv")
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(match status_file {
                Some(file) => Stdio::from(file),
                None => Stdio::null(),
            })
            .spawn()
            .map_err(|_| "mpv is not available")?;

        let deadline = Instant::now() + START_TIMEOUT;
        let stream = loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&directory);
                    return Err("mpv IPC did not become ready");
                }
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
        let reader = stream
            .try_clone()
            .map_err(|_| "mpv IPC clone failed")
            .map(BufReader::new)?;
        let mut session = Self {
            child,
            stream,
            reader,
            directory,
            start_position_ms: load.start_position_ms,
            duration_ms: None,
            position_ms: 0,
            last_position_emit: Instant::now(),
            stopped: false,
            ended: false,
        };
        for (id, property) in [(1, "time-pos"), (2, "duration"), (3, "eof-reached")] {
            session.send(&json!({"command": ["observe_property", id, property]}));
        }
        if let Some(headers) = load.headers.as_ref()
            && !headers.is_empty()
        {
            let fields: Vec<String> = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect();
            session.send(&json!({"command": ["set_property", "http-header-fields", fields]}));
        }
        session.send(&json!({"command": ["loadfile", load.url, "replace"]}));
        session.send(&json!({"command": ["set_property", "pause", false]}));
        Ok(session)
    }

    fn emit_start_position(&mut self) {
        if let Some(position_ms) = self.start_position_ms.take() {
            self.send(&json!({
                "command": ["seek", position_ms as f64 / 1000.0, "absolute+exact"]
            }));
        }
    }

    fn send(&mut self, value: &Value) -> bool {
        let Ok(mut line) = serde_json::to_vec(value) else {
            return false;
        };
        line.push(b'\n');
        line.len() <= MAX_IPC_BYTES
            && self.stream.write_all(&line).is_ok()
            && self.stream.flush().is_ok()
    }

    fn set_paused(&mut self, paused: bool) {
        self.send(&json!({"command": ["set_property", "pause", paused]}));
    }

    fn seek(&mut self, position_ms: u64) {
        self.send(&json!({
            "command": ["seek", position_ms as f64 / 1000.0, "absolute+exact"]
        }));
    }

    fn set_volume(&mut self, volume: u8) {
        self.send(&json!({"command": ["set_property", "volume", volume]}));
    }

    fn poll_events(&mut self) -> Vec<PlayerEvent> {
        let mut events = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if value.get("event").and_then(Value::as_str) == Some("end-file") {
                        if !self.ended {
                            self.ended = true;
                            events.push(PlayerEvent::Ended);
                        }
                        break;
                    }
                    if value.get("event").and_then(Value::as_str) != Some("property-change") {
                        continue;
                    }
                    let Some(id) = value.get("id").and_then(Value::as_i64) else {
                        continue;
                    };
                    if id == 2
                        && let Some(seconds) = value.get("data").and_then(Value::as_f64)
                    {
                        self.duration_ms = Some((seconds * 1000.0).round().max(0.0) as u64);
                    }
                    if id == 1
                        && let Some(seconds) = value.get("data").and_then(Value::as_f64)
                    {
                        self.position_ms = (seconds * 1000.0).round().max(0.0) as u64;
                        self.last_position_emit = Instant::now();
                        events.push(PlayerEvent::Position {
                            position_ms: self.position_ms,
                            duration_ms: self.duration_ms,
                        });
                    }
                    if id == 3 && value.get("data").and_then(Value::as_bool) == Some(true) {
                        if !self.ended {
                            self.ended = true;
                            events.push(PlayerEvent::Ended);
                        }
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(_) => break,
            }
            if line.len() > MAX_IPC_BYTES {
                break;
            }
        }
        // Keep the parent supervisor alive while mpv is paused or buffering;
        // its playback-event deadline is shorter than a long pause.
        if self.last_position_emit.elapsed() >= Duration::from_secs(2) {
            events.push(PlayerEvent::Position {
                position_ms: self.position_ms,
                duration_ms: self.duration_ms,
            });
            self.last_position_emit = Instant::now();
        }
        events
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.send(&json!({"command": ["quit"]}));
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if self.exited() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !self.exited() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Drop for MpvSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn write_input_conf(path: &Path) -> Result<(), &'static str> {
    let contents = "\
ESC quit
SPACE cycle pause
LEFT seek -10
RIGHT seek 10
UP add volume 5
DOWN add volume -5
";
    std::fs::write(path, contents).map_err(|_| "mpv input config failed")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Command line for mpv. Deliberately contains no media URL or headers.
fn build_arguments(socket: &Path, input_conf: &Path) -> Vec<String> {
    let mut arguments = vec![
        "--no-config".to_string(),
        "--terminal=yes".to_string(),
        "--msg-level=all=no,vo=error,vd=error".to_string(),
        "--term-status-msg=hwdec=${hwdec-current} codec=${video-codec} size=${width}x${height} fps=${container-fps} vfps=${estimated-vf-fps} drops=${drop-frame-count} mistimed=${mistimed-frame-count} vo-drops=${vo-drop-frame-count} audio=${audio-codec} sync=${video-sync}".to_string(),
        "--idle=yes".to_string(),
        "--force-window=yes".to_string(),
        "--fullscreen=yes".to_string(),
        "--no-border".to_string(),
        "--keep-open=no".to_string(),
        "--no-resume-playback".to_string(),
        "--save-position-on-quit=no".to_string(),
        "--ytdl=no".to_string(),
        "--osc=yes".to_string(),
        "--osd-level=1".to_string(),
        "--input-default-bindings=yes".to_string(),
        format!(
            "--vo={}",
            std::env::var("TV_PLAYER_VO")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "dmabuf-wayland".to_string())
        ),
        format!("--input-ipc-server={}", socket.display()),
        format!("--input-conf={}", input_conf.display()),
    ];
    if let Ok(hwdec) = std::env::var("TV_PLAYER_HWDEC") {
        if !hwdec.trim().is_empty() {
            arguments.push(format!("--hwdec={}", hwdec.trim()));
        }
    } else if arguments.iter().any(|arg| arg == "--vo=dmabuf-wayland") {
        // dmabuf-wayland only works with a dmabuf-capable decoder. Auto-safe
        // probes VDPAU first on this image and can fail before trying v4l2m2m.
        arguments.push("--hwdec=v4l2m2m".to_string());
    } else {
        arguments.push("--hwdec=auto-safe".to_string());
    }
    if std::env::var("TV_AUDIO_SINK").as_deref() == Ok("alsa") {
        arguments.push("--ao=alsa".to_string());
        if let Ok(device) = std::env::var("TV_PLAYER_AUDIO_DEVICE")
            && !device.trim().is_empty()
        {
            arguments.push(format!("--audio-device={}", device.trim()));
        } else if let Ok(device) = std::env::var("TV_AUDIO_DEVICE")
            && !device.trim().is_empty()
        {
            // The kiosk profile sets the mpv-specific name; this fallback
            // accepts a full mpv device string without inventing an invalid
            // `alsa/hw:N,M` path.
            if device.trim().starts_with("alsa/") || device.trim() == "auto" {
                arguments.push(format!("--audio-device={}", device.trim()));
            }
        }
    }
    arguments
}

fn monotonic_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpv_arguments_never_contain_a_url_or_header_value() {
        let arguments = build_arguments(
            Path::new("/tmp/tv-client/socket"),
            Path::new("/tmp/tv-client/input.conf"),
        );
        assert!(
            arguments
                .iter()
                .all(|argument| !argument.contains("http") && !argument.contains("Bearer"))
        );
        assert!(arguments.iter().any(|argument| argument == "--osc=yes"));
    }

    #[test]
    fn input_config_is_small_and_shell_free() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("input.conf");
        write_input_conf(&path).expect("input conf");
        let text = std::fs::read_to_string(path).expect("read");
        assert!(text.contains("ESC quit"));
        assert!(text.contains("SPACE cycle pause"));
        assert!(!text.contains('$'));
    }
}
