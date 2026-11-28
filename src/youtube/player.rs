//! Single-owner YouTube playback controller.
//!
//! The controller owns at most one extractor resolution and one libmpv child.
//! A new `Play` cancels and reaps the previous generation before starting a
//! new one, so late results and duplicate commands cannot start a second
//! player. There is no automatic retry: [`PlayerCommand::Retry`] is an explicit
//! bounded attempt. The local kill switch is checked before any process is
//! spawned.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::ClientConfig;
use crate::state::ShutdownSignal;
use crate::youtube::VideoId;
use crate::youtube::mpv::{
    BackendCommand, BackendError, BackendEvent, BackendFactory, MpvOptions, PlayerBackend,
};
use crate::youtube::resolve::{Extractor, ExtractorError, ResolvedMedia};

/// Bound for queued player commands.
pub const PLAYER_COMMAND_CAPACITY: usize = 8;
/// Bound for emitted player events.
pub const PLAYER_EVENT_CAPACITY: usize = 32;
const BACKEND_COMMAND_CAPACITY: usize = 8;
const BACKEND_EVENT_CAPACITY: usize = 32;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Resolved local playback settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackConfig {
    /// Local kill switch; false means no process may be spawned.
    pub enabled: bool,
    /// Operator-managed extractor path.
    pub extractor: PathBuf,
    /// Overall resolve-and-start budget.
    pub timeout: Duration,
    /// libmpv process settings.
    pub mpv: MpvOptions,
}

impl PlaybackConfig {
    /// Build the effective configuration from the validated client settings.
    ///
    /// The switch is false when the managed extractor path is missing or not
    /// executable, so a broken installation cannot half-enable the feature.
    #[must_use]
    pub fn from_client(config: &ClientConfig) -> Self {
        let extractor = config
            .youtube_ytdlp_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("yt-dlp"));
        let program = config
            .youtube_mpv_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("mpv"));
        let timeout = config.youtube_timeout();
        Self {
            enabled: config.youtube_playback_ready(),
            extractor,
            timeout,
            mpv: MpvOptions {
                program,
                hwdec: config.youtube_hwdec,
                start_timeout: timeout.min(Duration::from_secs(15)),
                ..MpvOptions::default()
            },
        }
    }
}

/// Controller commands. Only the validated identifier form is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerCommand {
    /// Start local playback of one validated identifier.
    Play {
        /// Validated 11-character identifier.
        video_id: VideoId,
        /// Optional display title already known by the UI.
        title: Option<String>,
    },
    /// Pause or resume.
    Pause {
        /// Requested pause state.
        paused: bool,
    },
    /// Seek within the active media.
    Seek {
        /// Absolute position in milliseconds.
        position_ms: u64,
    },
    /// Stop and reap the player.
    Stop,
    /// Explicitly retry the last request after a failure.
    Retry,
}

/// Controller events consumed by the UI bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    /// Resolution or player startup began.
    Loading {
        /// Identifier being opened.
        video_id: String,
        /// Display title when known.
        title: Option<String>,
    },
    /// The backend opened the media.
    Playing {
        /// Identifier.
        video_id: String,
        /// Display title.
        title: Option<String>,
        /// Position at the first report.
        position_ms: u64,
        /// Duration when known.
        duration_ms: Option<u64>,
        /// Codec/renderer summary.
        method: String,
    },
    /// Playback position advanced.
    Progress {
        /// Identifier.
        video_id: String,
        /// Position in milliseconds.
        position_ms: u64,
        /// Duration when known.
        duration_ms: Option<u64>,
        /// Pause state.
        paused: bool,
    },
    /// Pause state changed without a position update.
    Paused {
        /// Whether playback is paused.
        paused: bool,
    },
    /// Playback reached the end.
    Ended {
        /// Identifier.
        video_id: String,
        /// Final position.
        position_ms: u64,
    },
    /// Playback failed with a stable typed reason.
    Failed {
        /// Identifier, when one was being opened.
        video_id: Option<String>,
        /// Stable code.
        code: String,
        /// Bounded local explanation.
        message: String,
        /// Whether the operator may retry.
        retry_available: bool,
    },
    /// Playback stopped and resources were released.
    Stopped {
        /// Identifier that was stopped, when any.
        video_id: Option<String>,
    },
}

/// Why the controller loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlayerError {
    /// The command or event channel was closed.
    #[error("the youtube player channel closed")]
    Closed,
}

impl PlayerError {
    /// Stable code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Closed => "player_channel_closed",
        }
    }
}

struct ResolveTask {
    handle: JoinHandle<Result<ResolvedMedia, ExtractorError>>,
    cancel: crate::jellyfin::CancelHandle,
    video_id: String,
}

struct BackendHandle {
    commands: mpsc::Sender<BackendCommand>,
    events: mpsc::Receiver<BackendEvent>,
    task: JoinHandle<()>,
}

struct Session {
    video_id: String,
    title: Option<String>,
    position_ms: u64,
    duration_ms: Option<u64>,
    paused: bool,
    method: String,
    last_progress: tokio::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Resolving,
    Starting,
    Active,
}

/// Run the player controller until shutdown or channel closure.
pub async fn run_player(
    config: PlaybackConfig,
    extractor: Arc<dyn Extractor>,
    factory: Arc<dyn BackendFactory>,
    mut commands: mpsc::Receiver<PlayerCommand>,
    events: mpsc::Sender<PlayerEvent>,
    mut shutdown: ShutdownSignal,
) -> Result<(), PlayerError> {
    let mut phase = Phase::Idle;
    let mut resolver: Option<ResolveTask> = None;
    let mut backend: Option<BackendHandle> = None;
    let mut session: Option<Session> = None;
    let mut last_request: Option<(VideoId, Option<String>)> = None;
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                cancel_resolve(&mut resolver, config.mpv.reap_timeout).await;
                stop_backend(&mut backend, config.mpv.reap_timeout).await;
                return Ok(());
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    cancel_resolve(&mut resolver, config.mpv.reap_timeout).await;
                    stop_backend(&mut backend, config.mpv.reap_timeout).await;
                    return Err(PlayerError::Closed);
                };
                match command {
                    PlayerCommand::Play { video_id, title } => {
                        if !config.enabled {
                            emit_failure(
                                &events,
                                Some(video_id.as_str().to_owned()),
                                "youtube_disabled",
                                "YouTube playback is disabled by local configuration",
                                false,
                            )
                            .await?;
                            continue;
                        }
                        if phase != Phase::Idle
                            && session
                                .as_ref()
                                .is_some_and(|session| session.video_id == video_id.as_str())
                        {
                            // Duplicate or repeated play of the same item while
                            // it is already opening/playing is not a restart.
                            continue;
                        }
                        cancel_resolve(&mut resolver, config.mpv.reap_timeout).await;
                        stop_backend(&mut backend, config.mpv.reap_timeout).await;
                        phase = Phase::Resolving;
                        let title = title.map(|title| bounded_title(&title));
                        let video_key = video_id.as_str().to_owned();
                        session = Some(Session {
                            video_id: video_key.clone(),
                            title: title.clone(),
                            position_ms: 0,
                            duration_ms: None,
                            paused: false,
                            method: String::new(),
                            last_progress: tokio::time::Instant::now(),
                        });
                        last_request = Some((video_id.clone(), title.clone()));
                        emit(
                            &events,
                            PlayerEvent::Loading {
                                video_id: video_key.clone(),
                                title,
                            },
                        )
                        .await?;
                        let (cancel_handle, cancel) = crate::jellyfin::cancellation();
                        let extractor = Arc::clone(&extractor);
                        let id = video_id;
                        let task = tokio::spawn(async move { extractor.resolve(&id, &cancel).await });
                        resolver = Some(ResolveTask {
                            handle: task,
                            cancel: cancel_handle,
                            video_id: video_key,
                        });
                        deadline = Some(tokio::time::Instant::now() + config.timeout);
                    }
                    PlayerCommand::Stop => {
                        cancel_resolve(&mut resolver, config.mpv.reap_timeout).await;
                        stop_backend(&mut backend, config.mpv.reap_timeout).await;
                        phase = Phase::Idle;
                        deadline = None;
                        let video_id = session.take().map(|session| session.video_id);
                        emit(&events, PlayerEvent::Stopped { video_id }).await?;
                    }
                    PlayerCommand::Retry => {
                        if phase != Phase::Idle {
                            continue;
                        }
                        let Some((video_id, title)) = last_request.clone() else {
                            continue;
                        };
                        if !config.enabled {
                            emit_failure(
                                &events,
                                Some(video_id.as_str().to_owned()),
                                "youtube_disabled",
                                "YouTube playback is disabled by local configuration",
                                false,
                            )
                            .await?;
                            continue;
                        }
                        phase = Phase::Resolving;
                        let video_key = video_id.as_str().to_owned();
                        session = Some(Session {
                            video_id: video_key.clone(),
                            title: title.clone(),
                            position_ms: 0,
                            duration_ms: None,
                            paused: false,
                            method: String::new(),
                            last_progress: tokio::time::Instant::now(),
                        });
                        emit(
                            &events,
                            PlayerEvent::Loading {
                                video_id: video_key.clone(),
                                title,
                            },
                        )
                        .await?;
                        let (cancel_handle, cancel) = crate::jellyfin::cancellation();
                        let extractor = Arc::clone(&extractor);
                        let id = video_id;
                        let task = tokio::spawn(async move { extractor.resolve(&id, &cancel).await });
                        resolver = Some(ResolveTask {
                            handle: task,
                            cancel: cancel_handle,
                            video_id: video_key,
                        });
                        deadline = Some(tokio::time::Instant::now() + config.timeout);
                    }
                    PlayerCommand::Pause { paused } => {
                        if phase != Phase::Active {
                            continue;
                        }
                        if let Some(session) = session.as_mut() {
                            session.paused = paused;
                        }
                        let send_failed = match backend.as_ref() {
                            Some(backend) => backend
                                .commands
                                .send(BackendCommand::Pause(paused))
                                .await
                                .is_err(),
                            None => false,
                        };
                        if send_failed {
                            fail_active(
                                &events,
                                &mut phase,
                                &mut session,
                                &mut backend,
                                &mut deadline,
                                config.mpv.reap_timeout,
                                "player_closed",
                                "The local player stopped",
                            )
                            .await?;
                            continue;
                        }
                        emit(&events, PlayerEvent::Paused { paused }).await?;
                    }
                    PlayerCommand::Seek { position_ms } => {
                        if phase != Phase::Active {
                            continue;
                        }
                        let position_ms = session.as_ref().map_or(position_ms, |session| {
                            session
                                .duration_ms
                                .map_or(position_ms, |duration| position_ms.min(duration))
                        });
                        let send_failed = match backend.as_ref() {
                            Some(backend) => backend
                                .commands
                                .send(BackendCommand::Seek { position_ms })
                                .await
                                .is_err(),
                            None => false,
                        };
                        if send_failed {
                            fail_active(
                                &events,
                                &mut phase,
                                &mut session,
                                &mut backend,
                                &mut deadline,
                                config.mpv.reap_timeout,
                                "player_closed",
                                "The local player stopped",
                            )
                            .await?;
                            continue;
                        }
                        if let Some(session) = session.as_mut() {
                            session.position_ms = position_ms;
                            session.last_progress = tokio::time::Instant::now();
                        }
                        emit(
                            &events,
                            PlayerEvent::Progress {
                                video_id: session
                                    .as_ref()
                                    .map_or_else(String::new, |session| session.video_id.clone()),
                                position_ms,
                                duration_ms: session.as_ref().and_then(|session| session.duration_ms),
                                paused: session.as_ref().is_some_and(|session| session.paused),
                            },
                        )
                        .await?;
                    }
                }
            }
            result = await_resolve(&mut resolver) => {
                let Some((video_id, result)) = result else {
                    continue;
                };
                if phase != Phase::Resolving {
                    continue;
                }
                resolver = None;
                match result {
                    Ok(media) => {
                        if let Some(session) = session.as_mut() {
                            session.method = media.method();
                            if session.title.is_none() {
                                session.title = media.title.clone();
                            }
                            if session.duration_ms.is_none() {
                                session.duration_ms = media.duration_ms;
                            }
                        }
                        match tokio::time::timeout(config.mpv.start_timeout, factory.start()).await {
                            Err(_) => {
                                phase = Phase::Idle;
                                deadline = None;
                                let video_id = session.take().map(|session| session.video_id);
                                emit_failure(
                                    &events,
                                    video_id,
                                    BackendError::Timeout.code(),
                                    "The local player did not start in time",
                                    true,
                                )
                                .await?;
                            }
                            Ok(Err(error)) => {
                                phase = Phase::Idle;
                                deadline = None;
                                let video_id = session.take().map(|session| session.video_id);
                                emit_failure(
                                    &events,
                                    video_id,
                                    error.code(),
                                    "The local player could not start",
                                    true,
                                )
                                .await?;
                            }
                            Ok(Ok(mut player)) => {
                                if let Err(error) = player.load(&media).await {
                                    let _ = player.close().await;
                                    phase = Phase::Idle;
                                    deadline = None;
                                    let video_id =
                                        session.take().map(|session| session.video_id);
                                    emit_failure(
                                        &events,
                                        video_id,
                                        error.code(),
                                        "The local player refused the media",
                                        true,
                                    )
                                    .await?;
                                } else {
                                    backend = Some(spawn_backend(player));
                                    phase = Phase::Starting;
                                    deadline =
                                        Some(tokio::time::Instant::now() + config.mpv.start_timeout);
                                }
                            }
                        }
                    }
                    Err(ExtractorError::Cancelled) => {
                        phase = Phase::Idle;
                        deadline = None;
                        session = None;
                    }
                    Err(error) => {
                        phase = Phase::Idle;
                        deadline = None;
                        let video_id = session
                            .as_ref()
                            .map(|session| session.video_id.clone())
                            .or(Some(video_id));
                        session = None;
                        emit_failure(
                            &events,
                            video_id,
                            error.code(),
                            extractor_message(error),
                            error.retryable(),
                        )
                        .await?;
                    }
                }
            }
            event = await_backend(&mut backend) => {
                let Some(event) = event else {
                    if phase == Phase::Idle {
                        backend = None;
                        continue;
                    }
                    fail_active(
                        &events,
                        &mut phase,
                        &mut session,
                        &mut backend,
                        &mut deadline,
                        config.mpv.reap_timeout,
                        "player_closed",
                        "The local player closed unexpectedly",
                    )
                    .await?;
                    continue;
                };
                match event {
                    BackendEvent::Loaded => {
                        if phase != Phase::Starting {
                            continue;
                        }
                        phase = Phase::Active;
                        deadline = None;
                        if let Some(session) = session.as_mut() {
                            session.last_progress = tokio::time::Instant::now();
                            emit(
                                &events,
                                PlayerEvent::Playing {
                                    video_id: session.video_id.clone(),
                                    title: session.title.clone(),
                                    position_ms: session.position_ms,
                                    duration_ms: session.duration_ms,
                                    method: session.method.clone(),
                                },
                            )
                            .await?;
                        }
                    }
                    BackendEvent::Title(title) => {
                        if let Some(session) = session.as_mut()
                            && session.title.is_none()
                        {
                            session.title = Some(bounded_title(&title));
                        }
                    }
                    BackendEvent::Duration { duration_ms } => {
                        if let Some(session) = session.as_mut() {
                            session.duration_ms = Some(duration_ms);
                        }
                    }
                    BackendEvent::Position { position_ms } => {
                        let mut emit_progress = false;
                        if let Some(session) = session.as_mut() {
                            session.position_ms = position_ms;
                            if phase == Phase::Active {
                                let now = tokio::time::Instant::now();
                                if now.saturating_duration_since(session.last_progress)
                                    >= PROGRESS_INTERVAL
                                {
                                    session.last_progress = now;
                                    emit_progress = true;
                                }
                            }
                        }
                        if emit_progress
                            && let Some(session) = session.as_ref()
                        {
                            emit(
                                &events,
                                PlayerEvent::Progress {
                                    video_id: session.video_id.clone(),
                                    position_ms: session.position_ms,
                                    duration_ms: session.duration_ms,
                                    paused: session.paused,
                                },
                            )
                            .await?;
                        }
                    }
                    BackendEvent::Paused(paused) => {
                        if let Some(session) = session.as_mut() {
                            session.paused = paused;
                        }
                        emit(&events, PlayerEvent::Paused { paused }).await?;
                    }
                    BackendEvent::Ended => {
                        let (video_id, position_ms) = session.as_ref().map_or_else(
                            || (String::new(), 0),
                            |session| (session.video_id.clone(), session.position_ms),
                        );
                        stop_backend(&mut backend, config.mpv.reap_timeout).await;
                        phase = Phase::Idle;
                        deadline = None;
                        session = None;
                        emit(&events, PlayerEvent::Ended { video_id, position_ms }).await?;
                    }
                    BackendEvent::Failed { code } => {
                        let video_id = session.as_ref().map(|session| session.video_id.clone());
                        stop_backend(&mut backend, config.mpv.reap_timeout).await;
                        phase = Phase::Idle;
                        deadline = None;
                        session = None;
                        emit_failure(
                            &events,
                            video_id,
                            code,
                            "The local player could not play this video",
                            true,
                        )
                        .await?;
                    }
                }
            }
            () = wait_deadline(deadline) => {
                deadline = None;
                if phase == Phase::Resolving {
                    cancel_resolve(&mut resolver, config.mpv.reap_timeout).await;
                }
                stop_backend(&mut backend, config.mpv.reap_timeout).await;
                phase = Phase::Idle;
                let video_id = session.as_ref().map(|session| session.video_id.clone());
                session = None;
                emit_failure(
                    &events,
                    video_id,
                    "playback_timeout",
                    "Local YouTube playback timed out",
                    true,
                )
                .await?;
            }
        }
    }
}

async fn cancel_resolve(resolver: &mut Option<ResolveTask>, timeout: Duration) {
    if let Some(task) = resolver.take() {
        task.cancel.cancel();
        let ResolveTask { mut handle, .. } = task;
        // Wait for the extractor future to drop its child before a new
        // generation may start. Abort only if it ignores cooperative
        // cancellation.
        if tokio::time::timeout(timeout, &mut handle).await.is_err() {
            handle.abort();
        }
    }
}

fn spawn_backend(mut backend: Box<dyn PlayerBackend>) -> BackendHandle {
    let (commands, mut command_rx) = mpsc::channel(BACKEND_COMMAND_CAPACITY);
    let (event_tx, events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                command = command_rx.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    let stop = command == BackendCommand::Stop;
                    if backend.command(command).await.is_err() {
                        let _ = event_tx
                            .send(BackendEvent::Failed { code: "player_ipc" })
                            .await;
                        break;
                    }
                    if stop {
                        break;
                    }
                }
                event = backend.next_event() => {
                    match event {
                        Some(event) => {
                            let terminal = matches!(
                                event,
                                BackendEvent::Ended | BackendEvent::Failed { .. }
                            );
                            if event_tx.send(event).await.is_err() {
                                break;
                            }
                            if terminal {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        backend.close().await;
    });
    BackendHandle {
        commands,
        events,
        task,
    }
}

async fn close_backend(backend: &mut BackendHandle, reap_timeout: Duration) {
    let _ = backend.commands.send(BackendCommand::Stop).await;
    if tokio::time::timeout(reap_timeout, &mut backend.task)
        .await
        .is_err()
    {
        backend.task.abort();
    }
}

async fn stop_backend(backend: &mut Option<BackendHandle>, reap_timeout: Duration) {
    if let Some(mut handle) = backend.take() {
        close_backend(&mut handle, reap_timeout).await;
    }
}

async fn await_resolve(
    resolver: &mut Option<ResolveTask>,
) -> Option<(String, Result<ResolvedMedia, ExtractorError>)> {
    match resolver.as_mut() {
        Some(task) => {
            let video_id = task.video_id.clone();
            match (&mut task.handle).await {
                Ok(result) => Some((video_id, result)),
                Err(_) => Some((video_id, Err(ExtractorError::InvalidOutput))),
            }
        }
        None => std::future::pending().await,
    }
}

async fn await_backend(backend: &mut Option<BackendHandle>) -> Option<BackendEvent> {
    match backend.as_mut() {
        Some(backend) => backend.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn emit(events: &mpsc::Sender<PlayerEvent>, event: PlayerEvent) -> Result<(), PlayerError> {
    events.send(event).await.map_err(|_| PlayerError::Closed)
}

async fn emit_failure(
    events: &mpsc::Sender<PlayerEvent>,
    video_id: Option<String>,
    code: &str,
    message: &str,
    retry_available: bool,
) -> Result<(), PlayerError> {
    emit(
        events,
        PlayerEvent::Failed {
            video_id,
            code: code.to_owned(),
            message: message.to_owned(),
            retry_available,
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fail_active(
    events: &mpsc::Sender<PlayerEvent>,
    phase: &mut Phase,
    session: &mut Option<Session>,
    backend: &mut Option<BackendHandle>,
    deadline: &mut Option<tokio::time::Instant>,
    reap_timeout: Duration,
    code: &str,
    message: &str,
) -> Result<(), PlayerError> {
    let video_id = session.as_ref().map(|session| session.video_id.clone());
    stop_backend(backend, reap_timeout).await;
    *phase = Phase::Idle;
    *deadline = None;
    *session = None;
    emit_failure(events, video_id, code, message, true).await
}

fn extractor_message(error: ExtractorError) -> &'static str {
    match error {
        ExtractorError::ExtractorUnavailable { reason } => match reason {
            crate::youtube::resolve::ExtractorFailure::MissingBinary => {
                "The managed yt-dlp is not installed"
            }
            crate::youtube::resolve::ExtractorFailure::Blocked => {
                "YouTube requires a sign-in or bot check"
            }
            crate::youtube::resolve::ExtractorFailure::Stale => {
                "The managed yt-dlp needs an update"
            }
            crate::youtube::resolve::ExtractorFailure::UpdateFailed => {
                "The managed yt-dlp could not update itself; check that its path is writable"
            }
            _ => "The managed yt-dlp could not be used",
        },
        ExtractorError::Failed { reason } => match reason {
            crate::youtube::resolve::ExtractorFailure::Blocked => {
                "YouTube requires a sign-in or bot check"
            }
            crate::youtube::resolve::ExtractorFailure::Stale => {
                "The managed yt-dlp needs an update"
            }
            crate::youtube::resolve::ExtractorFailure::UpdateFailed => {
                "The managed yt-dlp could not update itself; check that its path is writable"
            }
            crate::youtube::resolve::ExtractorFailure::RateLimited => {
                "YouTube rate-limited this device"
            }
            crate::youtube::resolve::ExtractorFailure::VideoUnavailable => {
                "This video is unavailable"
            }
            _ => "The video could not be resolved",
        },
        ExtractorError::Timeout => "Resolving the video timed out",
        ExtractorError::Cancelled => "Resolving the video was cancelled",
        ExtractorError::InvalidOutput => "The extractor returned unusable output",
        ExtractorError::UnsupportedFormat => "No supported H.264 format was available",
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
    use crate::youtube::resolve::ExtractorFailure;

    #[test]
    fn client_configuration_is_disabled_without_an_executable_extractor() {
        let mut config = ClientConfig::default();
        assert!(!PlaybackConfig::from_client(&config).enabled);
        config.youtube_playback_enabled = true;
        config.youtube_ytdlp_path = Some(PathBuf::from("/nonexistent/yt-dlp"));
        assert!(!PlaybackConfig::from_client(&config).enabled);
    }

    #[test]
    fn failures_map_to_stable_codes() {
        assert_eq!(
            ExtractorError::ExtractorUnavailable {
                reason: ExtractorFailure::MissingBinary
            }
            .code(),
            "extractor_unavailable"
        );
        assert_eq!(ExtractorError::Timeout.code(), "extractor_timeout");
        assert!(!ExtractorError::UnsupportedFormat.retryable());
    }
}
