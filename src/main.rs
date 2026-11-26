//! Command-line entry point for `tv-client`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use thiserror::Error;

use tv_client::cloud::{self, CloudError, CloudHttp, EnrollmentToken};
use tv_client::config::{self, ClientConfig, ConfigError};
use tv_client::storage::{self, Storage, StorageError};

/// Headless-first TV client.
#[derive(Debug, Parser)]
#[command(name = "tv-client", version, about = "Headless-first TV client")]
struct Cli {
    /// Path to the client TOML configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Directory for local state, artwork and credentials.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Validate configuration and exit without starting the client.
    #[arg(long)]
    check_config: bool,
    /// Optional administrative subcommand.
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Enroll this TV with the control server.
    ///
    /// The one-use token is read from stdin, never from the command line.
    Enroll(EnrollArgs),
}

#[derive(Debug, Args)]
struct EnrollArgs {
    /// Path to the client TOML configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Directory for local state and the 0600 device credential.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Label recorded by the server for this device.
    #[arg(long, default_value = "tv-client", value_name = "LABEL")]
    label: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tv-client: {error}");
            error.exit_code()
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let Cli {
        config: config_argument,
        state_dir: state_dir_argument,
        check_config,
        command,
    } = cli;
    if let Some(CliCommand::Enroll(args)) = command {
        return run_enroll(args, config_argument, state_dir_argument);
    }
    let explicit_config = config_argument.is_some();
    let config_path = config_argument.or_else(config::default_config_path);
    let config = load_config(config_path.as_deref(), explicit_config)?;
    if check_config {
        println!("configuration OK");
        return Ok(());
    }

    let state_directory = match state_dir_argument {
        Some(path) => path,
        None => storage::default_state_dir().ok_or(CliError::MissingHome)?,
    };
    let storage = Storage::open(&state_directory, config.artwork_cache_bytes)?;
    let schema_version = storage.schema_version()?;
    tracing::debug!(
        state_dir = %state_directory.display(),
        schema_version,
        "headless initialization complete"
    );

    #[cfg(feature = "native-ui")]
    launch_native_ui(&config, &state_directory)?;

    Ok(())
}

#[cfg(feature = "native-ui")]
#[derive(Debug, Clone, Copy)]
enum CloudControl {
    StartPairing,
    Decide {
        request_id: uuid::Uuid,
        accept: bool,
    },
}

#[cfg(feature = "native-ui")]
fn launch_native_ui(config: &ClientConfig, state_directory: &Path) -> Result<(), CliError> {
    use tokio::sync::mpsc;

    use tv_client::playback::PlaybackCommand;
    use tv_client::ui::UiCommand;

    let (commands, command_rx) = mpsc::channel::<PlaybackCommand>(32);
    let cloud_commands = commands.clone();
    let (cloud_control, cloud_control_rx) = mpsc::channel::<CloudControl>(32);
    let (shutdown, signal) = tv_client::state::shutdown_channel();
    let ui = tv_client::ui::NativeUi::new(tv_client::AppState::default(), move |command| {
        match command {
            UiCommand::StartPairing => {
                let _ = cloud_control.try_send(CloudControl::StartPairing);
            }
            UiCommand::ApprovePairing { request_id } => {
                let _ = cloud_control.try_send(CloudControl::Decide {
                    request_id,
                    accept: true,
                });
            }
            UiCommand::DenyPairing { request_id } => {
                let _ = cloud_control.try_send(CloudControl::Decide {
                    request_id,
                    accept: false,
                });
            }
            other => {
                if let Some(command) = controller_command(&other) {
                    // The Slint callback never blocks: a full queue drops the
                    // command rather than stalling the UI thread.
                    let _ = commands.try_send(command);
                }
            }
        }
    })?;
    let handle = ui.handle();
    let cec_bridge = start_cec_bridge(&handle);

    #[cfg(feature = "gstreamer-player")]
    let playback_thread = start_playback_thread(
        config.clone(),
        state_directory.to_path_buf(),
        command_rx,
        signal,
        handle.clone(),
    );
    #[cfg(not(feature = "gstreamer-player"))]
    {
        // Keep the native-ui-only build warning-free: the playback
        // controller is compiled out, so these wiring values are unused.
        drop(command_rx);
        drop(signal);
        tracing::warn!("native UI built without gstreamer-player; playback is disabled");
    }

    let (cloud_shutdown, cloud_signal) = tv_client::state::shutdown_channel();
    let cloud_thread = start_cloud_thread(
        config.clone(),
        state_directory.to_path_buf(),
        handle.clone(),
        cloud_commands,
        cloud_control_rx,
        cloud_signal,
    );

    let result = ui.run();
    if let Some(bridge) = cec_bridge {
        match bridge.shutdown(std::time::Duration::from_secs(2)) {
            tv_client::input::ShutdownOutcome::Stopped => {}
            tv_client::input::ShutdownOutcome::TimedOut => {
                tracing::warn!("CEC bridge did not stop within 2 s");
            }
        }
    }
    shutdown.cancel();
    cloud_shutdown.cancel();
    #[cfg(feature = "gstreamer-player")]
    join_playback_thread(playback_thread);
    join_cloud_thread(cloud_thread);
    result?;
    Ok(())
}

#[cfg(feature = "native-ui")]
fn start_cloud_thread(
    config: ClientConfig,
    state_directory: PathBuf,
    ui: tv_client::ui::UiHandle,
    commands: tokio::sync::mpsc::Sender<tv_client::playback::PlaybackCommand>,
    control: tokio::sync::mpsc::Receiver<CloudControl>,
    shutdown: tv_client::state::ShutdownSignal,
) -> Option<std::thread::JoinHandle<()>> {
    let status_ui = ui.clone();
    let thread = std::thread::Builder::new()
        .name("tv-cloud".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "cannot start the cloud runtime");
                    ui.set_cloud_status("Cloud unavailable".to_string());
                    return;
                }
            };
            runtime.block_on(run_cloud(
                config,
                state_directory,
                ui,
                commands,
                control,
                shutdown,
            ));
        });
    match thread {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "cannot spawn the cloud thread");
            status_ui.set_cloud_status("Cloud unavailable".to_string());
            None
        }
    }
}

#[cfg(feature = "native-ui")]
async fn run_cloud(
    config: ClientConfig,
    state_directory: PathBuf,
    ui: tv_client::ui::UiHandle,
    commands: tokio::sync::mpsc::Sender<tv_client::playback::PlaybackCommand>,
    mut control: tokio::sync::mpsc::Receiver<CloudControl>,
    mut shutdown: tv_client::state::ShutdownSignal,
) {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::watch;
    use tv_client::cloud::{
        self, CloudError, CloudHttp, CommandPolicy, DeviceLink, HEARTBEAT_TIMEOUT,
        LINK_EVENT_CAPACITY, LinkConfig, LinkEnd, LinkEvent, PairStatus, PairingSession,
        StatePayload,
    };
    use tv_client::model::{AppState, InputOrigin};
    use tv_client::state::shutdown_channel;

    if !cloud::enabled(&config) {
        run_offline_cloud(control, ui, shutdown, "Cloud disabled".to_string()).await;
        return;
    }
    let Some(server_url) = config.server_url.clone() else {
        run_offline_cloud(control, ui, shutdown, "Cloud not configured".to_string()).await;
        return;
    };
    let storage =
        match tv_client::storage::Storage::open(&state_directory, config.artwork_cache_bytes) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(%error, "cannot open cloud storage");
                run_offline_cloud(
                    control,
                    ui,
                    shutdown,
                    "Cloud storage unavailable".to_string(),
                )
                .await;
                return;
            }
        };
    let device = match cloud::load_device(&storage) {
        Ok(Some(device)) => device,
        Ok(None) => {
            run_offline_cloud(control, ui, shutdown, "Cloud not enrolled".to_string()).await;
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "cannot load cloud device");
            run_offline_cloud(
                control,
                ui,
                shutdown,
                "Cloud credentials unavailable".to_string(),
            )
            .await;
            return;
        }
    };

    let http = match CloudHttp::new(&server_url) {
        Ok(http) => http,
        Err(_) => {
            run_offline_cloud(control, ui, shutdown, "Cloud not configured".to_string()).await;
            return;
        }
    }
    .with_token(device.token.clone());
    let device_config =
        match tokio::time::timeout(Duration::from_secs(5), http.device_config()).await {
            Ok(Ok(config)) => Some(config),
            Ok(Err(CloudError::Unauthorized)) | Ok(Err(CloudError::Revoked)) => {
                run_offline_cloud(control, ui, shutdown, "Cloud access revoked".to_string()).await;
                return;
            }
            Ok(Err(_)) | Err(_) => {
                ui.set_cloud_status("Cloud offline; retrying".to_string());
                None
            }
        };

    let mut link_config = match LinkConfig::new(
        &server_url,
        device.device_id,
        env!("CARGO_PKG_VERSION"),
        cloud::capabilities(config.jellyfin_url.is_some(), false),
    ) {
        Ok(config) => config,
        Err(_) => {
            run_offline_cloud(control, ui, shutdown, "Cloud not configured".to_string()).await;
            return;
        }
    };
    if let Some(device_config) = device_config {
        let heartbeat = device_config.heartbeat();
        if heartbeat >= Duration::from_secs(1) && heartbeat < HEARTBEAT_TIMEOUT {
            link_config = link_config.with_heartbeat(heartbeat, HEARTBEAT_TIMEOUT);
        }
    }
    let share_titles = config.share_titles;
    let initial = cloud_state(&ui, device.device_id, share_titles).unwrap_or_else(|| {
        StatePayload::from_app_state(&AppState::default(), device.device_id, share_titles)
            .expect("default cloud state is valid")
    });
    let (state_tx, state_rx) = watch::channel(initial.clone());
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(LINK_EVENT_CAPACITY);
    let applier = Arc::new(NativeCloudApplier {
        ui: Arc::new(Mutex::new(ui.clone())),
        commands,
        device_id: device.device_id,
        share_titles,
    });
    let link = match DeviceLink::new(
        link_config,
        device.token,
        state_rx,
        events_tx,
        applier,
        CommandPolicy::for_build(config.jellyfin_url.is_some()),
    ) {
        Ok(link) => link,
        Err(_) => {
            run_offline_cloud(control, ui, shutdown, "Cloud not configured".to_string()).await;
            return;
        }
    };
    let (link_shutdown, link_signal) = shutdown_channel();
    let mut link_task = tokio::spawn(link.run(link_signal));
    let mut pairing = PairingSession::new(http);
    let mut last_state = Some(initial);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut link_finished = false;

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            command = control.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    CloudControl::StartPairing => {
                        match tokio::time::timeout(Duration::from_secs(5), pairing.begin()).await {
                            Ok(Ok(code)) => {
                                ui.set_pairing_code(code.code.clone());
                                ui.set_cloud_status(format!("Pairing code {}", code.code));
                            }
                            Ok(Err(_)) => {
                                pairing.reset();
                                ui.clear_pairing_code();
                                ui.set_cloud_status("Pairing code unavailable".to_string());
                            }
                            Err(_) => {
                                pairing.reset();
                                ui.clear_pairing_code();
                                ui.set_cloud_status("Pairing request timed out".to_string());
                            }
                        }
                    }
                    CloudControl::Decide { request_id, accept } => {
                        let decision = tokio::time::timeout(
                            Duration::from_secs(5),
                            pairing.decide(
                                InputOrigin::LocalKeyboard,
                                request_id,
                                accept,
                                std::time::SystemTime::now(),
                            ),
                        )
                        .await;
                        match decision {
                            Ok(Ok(decision)) => {
                                ui.clear_pairing_code();
                                ui.set_cloud_status(format!(
                                    "Pairing {} submitted",
                                    decision.label()
                                ));
                            }
                            Ok(Err(_)) => {
                                ui.set_cloud_status("Pairing decision failed".to_string());
                            }
                            Err(_) => {
                                ui.set_cloud_status("Pairing decision timed out".to_string());
                            }
                        }
                    }
                }
            }
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    LinkEvent::Connected => {
                        ui.set_cloud_status("Cloud online".to_string());
                    }
                    LinkEvent::HelloAck(_) => {}
                    LinkEvent::PairRequest(request) => {
                        match pairing.on_request(request, std::time::SystemTime::now()) {
                            Ok(pending) => {
                                let request_id = pending.request_id;
                                let comparison_code = pending.comparison_code.clone();
                                ui.set_cloud_status(format!(
                                    "Confirm pairing code {comparison_code}"
                                ));
                                ui.present_pairing_request_with_code(
                                    request_id,
                                    Some(comparison_code),
                                );
                            }
                            Err(error) => {
                                tracing::debug!(%error, "cloud pairing request rejected");
                                ui.set_cloud_status("Pairing request expired".to_string());
                            }
                        }
                    }
                    LinkEvent::PairStatus(status) => {
                        let request_id = status.request_id;
                        pairing.on_status(status);
                        if status.status != PairStatus::Pending {
                            ui.dismiss_pairing_request(request_id);
                        }
                        match status.status {
                            PairStatus::Pending => {}
                            PairStatus::Accepted => {
                                pairing.reset();
                                ui.clear_pairing_code();
                                ui.set_cloud_status("Pairing accepted".to_string());
                            }
                            PairStatus::Rejected => {
                                pairing.reset();
                                ui.clear_pairing_code();
                                ui.set_cloud_status("Pairing rejected".to_string());
                            }
                            PairStatus::Expired => {
                                pairing.reset();
                                ui.clear_pairing_code();
                                ui.set_cloud_status("Pairing expired".to_string());
                            }
                        }
                    }
                    LinkEvent::Disconnected(_) => {
                        ui.set_cloud_status("Cloud offline; reconnecting".to_string());
                    }
                    LinkEvent::Revoked => {
                        if let Some(request_id) = pairing.pending().map(|pending| pending.request_id) {
                            ui.dismiss_pairing_request(request_id);
                        }
                        pairing.reset();
                        ui.clear_pairing_code();
                        ui.set_cloud_status("Cloud access revoked".to_string());
                    }
                }
            }
            _ = tick.tick() => {
                if let Some(payload) = cloud_state(&ui, device.device_id, share_titles)
                    && last_state.as_ref() != Some(&payload)
                {
                    let _ = state_tx.send(payload.clone());
                    last_state = Some(payload);
                }
                let now = std::time::SystemTime::now();
                let request_id = pairing
                    .pending()
                    .filter(|pending| now >= pending.expires_at)
                    .map(|pending| pending.request_id);
                if let Some(request_id) = request_id {
                    ui.dismiss_pairing_request(request_id);
                    pairing.reset();
                    ui.clear_pairing_code();
                    ui.set_cloud_status("Pairing request expired".to_string());
                } else if pairing
                    .code()
                    .is_some_and(|code| now >= code.expires_at)
                {
                    if let Some(request_id) = pairing.pending().map(|pending| pending.request_id) {
                        ui.dismiss_pairing_request(request_id);
                    }
                    ui.clear_pairing_code();
                    ui.set_cloud_status("Pairing code expired".to_string());
                    pairing.reset();
                }
            }
            result = &mut link_task => {
                link_finished = true;
                match result {
                    Ok(LinkEnd::Revoked) => {
                        if let Some(request_id) = pairing.pending().map(|pending| pending.request_id) {
                            ui.dismiss_pairing_request(request_id);
                        }
                        pairing.reset();
                        ui.clear_pairing_code();
                        ui.set_cloud_status("Cloud access revoked".to_string());
                    }
                    Ok(LinkEnd::Shutdown) => {}
                    Err(error) => {
                        tracing::warn!(%error, "cloud link task stopped");
                        ui.set_cloud_status("Cloud unavailable".to_string());
                    }
                }
                break;
            }
        }
    }

    if !link_finished {
        link_shutdown.cancel();
        if tokio::time::timeout(Duration::from_secs(2), &mut link_task)
            .await
            .is_err()
        {
            link_task.abort();
            let _ = link_task.await;
        }
    }
}

#[cfg(feature = "native-ui")]
async fn run_offline_cloud(
    mut control: tokio::sync::mpsc::Receiver<CloudControl>,
    ui: tv_client::ui::UiHandle,
    mut shutdown: tv_client::state::ShutdownSignal,
    message: String,
) {
    ui.set_cloud_status(message.clone());
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            command = control.recv() => {
                if command.is_none() {
                    break;
                }
                ui.set_cloud_status(message.clone());
            }
        }
    }
}

#[cfg(feature = "native-ui")]
fn cloud_state(
    ui: &tv_client::ui::UiHandle,
    device_id: uuid::Uuid,
    share_titles: bool,
) -> Option<tv_client::cloud::StatePayload> {
    tv_client::cloud::StatePayload::from_app_state(&ui.snapshot(), device_id, share_titles).ok()
}

#[cfg(feature = "native-ui")]
struct NativeCloudApplier {
    ui: std::sync::Arc<std::sync::Mutex<tv_client::ui::UiHandle>>,
    commands: tokio::sync::mpsc::Sender<tv_client::playback::PlaybackCommand>,
    device_id: uuid::Uuid,
    share_titles: bool,
}

#[cfg(feature = "native-ui")]
impl NativeCloudApplier {
    fn ui_handle(&self) -> tv_client::ui::UiHandle {
        self.ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn snapshot(&self) -> Option<tv_client::cloud::StatePayload> {
        let ui = self.ui_handle();
        tv_client::cloud::StatePayload::from_app_state(
            &ui.snapshot(),
            self.device_id,
            self.share_titles,
        )
        .ok()
    }

    async fn wait_for<F>(
        &self,
        timeout: std::time::Duration,
        mut predicate: F,
    ) -> Option<tv_client::cloud::StatePayload>
    where
        F: FnMut(&tv_client::cloud::StatePayload) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(state) = self.snapshot()
                && predicate(&state)
            {
                return Some(state);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    fn send_command(
        &self,
        command: tv_client::playback::PlaybackCommand,
    ) -> Result<(), tv_client::cloud::Rejection> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    tv_client::cloud::Rejection::new("queue_full", "playback queue is full")
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    tv_client::cloud::Rejection::new(
                        "playback_unavailable",
                        "playback is not available in this build",
                    )
                }
            })
    }

    async fn apply_play(&self, item_id: String) -> tv_client::cloud::ApplyOutcome {
        use tv_client::cloud::{ApplyOutcome, Rejection};
        use tv_client::model::{Mode, Source};

        let was_error = self
            .snapshot()
            .is_some_and(|state| state.error_code.is_some());
        let ui = self.ui_handle();
        ui.clear_player_error();
        // `clear_player_error` updates the model synchronously, so a fast
        // failure that sets a new error before the first 100 ms snapshot is
        // still classified as a playback failure rather than a timeout.
        let mut saw_recovery = !was_error
            || self
                .snapshot()
                .is_some_and(|state| state.error_code.is_none());
        if let Err(rejection) = self.send_command(tv_client::playback::PlaybackCommand::Play(
            tv_client::playback::PlayRequest::new(item_id.clone()),
        )) {
            return ApplyOutcome::Rejected(rejection);
        }
        let target = item_id;
        match self
            .wait_for(std::time::Duration::from_secs(8), |state| {
                if state.error_code.is_none() {
                    saw_recovery = true;
                }
                (state.source == Some(Source::Jellyfin)
                    && state.item_id.as_deref() == Some(target.as_str())
                    && matches!(state.mode, Mode::Loading | Mode::Playing | Mode::Paused))
                    || (saw_recovery && state.error_code.is_some())
            })
            .await
        {
            None => ApplyOutcome::Failed(Rejection::new("timeout", "playback did not start")),
            Some(state) if state.error_code.is_some() => ApplyOutcome::Failed(Rejection::new(
                "playback_failed",
                "the player reported a failure",
            )),
            Some(_) => ApplyOutcome::Applied,
        }
    }

    async fn apply_pause(&self, paused: bool) -> tv_client::cloud::ApplyOutcome {
        use tv_client::cloud::{ApplyOutcome, Rejection};

        if self.snapshot().is_some_and(|state| state.source.is_none()) {
            return ApplyOutcome::Rejected(Rejection::invalid_mode());
        }
        let command = if paused {
            tv_client::playback::PlaybackCommand::Pause
        } else {
            tv_client::playback::PlaybackCommand::Resume
        };
        if let Err(rejection) = self.send_command(command) {
            return ApplyOutcome::Rejected(rejection);
        }
        match self
            .wait_for(std::time::Duration::from_secs(5), |state| {
                state.source.is_some() && state.paused == paused
            })
            .await
        {
            None => ApplyOutcome::Failed(Rejection::new("timeout", "pause state did not change")),
            Some(state) if state.error_code.is_some() => ApplyOutcome::Failed(Rejection::new(
                "playback_failed",
                "the player reported a failure",
            )),
            Some(_) => ApplyOutcome::Applied,
        }
    }

    async fn apply_seek(&self, position_ms: u64) -> tv_client::cloud::ApplyOutcome {
        use tv_client::cloud::{ApplyOutcome, Rejection};

        if self
            .snapshot()
            .and_then(|state| state.duration_ms)
            .is_some_and(|duration| position_ms > duration)
        {
            return ApplyOutcome::Rejected(Rejection::new(
                "invalid_position",
                "seek position exceeds the reported duration",
            ));
        }
        if let Err(rejection) =
            self.send_command(tv_client::playback::PlaybackCommand::Seek { position_ms })
        {
            return ApplyOutcome::Rejected(rejection);
        }
        match self
            .wait_for(std::time::Duration::from_secs(8), |state| {
                state.error_code.is_some()
                    || state
                        .position_ms
                        .is_some_and(|position| position.abs_diff(position_ms) <= 1_500)
            })
            .await
        {
            None => ApplyOutcome::Failed(Rejection::new("timeout", "seek did not complete")),
            Some(state) if state.error_code.is_some() => ApplyOutcome::Failed(Rejection::new(
                "playback_failed",
                "the player reported a failure",
            )),
            Some(_) => ApplyOutcome::Applied,
        }
    }

    async fn apply_stop(&self) -> tv_client::cloud::ApplyOutcome {
        use tv_client::cloud::{ApplyOutcome, Rejection};

        if let Err(rejection) = self.send_command(tv_client::playback::PlaybackCommand::Stop) {
            return ApplyOutcome::Rejected(rejection);
        }
        match self
            .wait_for(std::time::Duration::from_secs(5), |state| {
                state.source.is_none()
            })
            .await
        {
            None => ApplyOutcome::Failed(Rejection::new("timeout", "playback did not stop")),
            Some(_) => ApplyOutcome::Applied,
        }
    }
}

#[cfg(feature = "native-ui")]
impl tv_client::cloud::CommandApplier for NativeCloudApplier {
    fn apply(
        &self,
        request: tv_client::cloud::ApplyRequest,
    ) -> futures_util::future::BoxFuture<'_, tv_client::cloud::ApplyOutcome> {
        use tv_client::cloud::{ApplyOutcome, Rejection};
        use tv_client::model::Action;

        Box::pin(async move {
            match request.action.clone() {
                Action::Navigate { .. } => {
                    let ui = self.ui_handle();
                    match ui.apply_remote_action(request.action).await {
                        Ok(tv_client::ui::UiOutcome::Action(_))
                        | Ok(tv_client::ui::UiOutcome::Navigated { .. })
                        | Ok(tv_client::ui::UiOutcome::Playback(_))
                        | Ok(tv_client::ui::UiOutcome::Retry) => ApplyOutcome::Applied,
                        Ok(tv_client::ui::UiOutcome::Ignored) => ApplyOutcome::Rejected(
                            Rejection::new("not_applicable", "the UI ignored the navigation"),
                        ),
                        Ok(tv_client::ui::UiOutcome::Rejected(_)) => {
                            ApplyOutcome::Rejected(Rejection::new(
                                "remote_input_rejected",
                                "the UI rejected the navigation",
                            ))
                        }
                        Ok(tv_client::ui::UiOutcome::Throttled) => ApplyOutcome::Rejected(
                            Rejection::new("throttled", "input was throttled"),
                        ),
                        Ok(_) => ApplyOutcome::Rejected(Rejection::new(
                            "remote_input_rejected",
                            "the navigation did not change the UI",
                        )),
                        Err(tv_client::ui::UiReject::RemoteApprovalDenied) => {
                            ApplyOutcome::Rejected(Rejection::new(
                                "pairing_confirmation_pending",
                                "a local pairing confirmation is open",
                            ))
                        }
                        Err(_) => ApplyOutcome::Failed(Rejection::new(
                            "ui_unavailable",
                            "the user interface is unavailable",
                        )),
                    }
                }
                Action::PlayJellyfin { item_id } => self.apply_play(item_id).await,
                Action::SetPaused { paused } => self.apply_pause(paused).await,
                Action::Seek { position_ms } => self.apply_seek(position_ms).await,
                Action::Stop => self.apply_stop().await,
                Action::SetVolume { .. } => ApplyOutcome::Rejected(Rejection::new(
                    "unsupported_action",
                    "remote volume control is not wired",
                )),
                Action::Standby { .. } => ApplyOutcome::Rejected(Rejection::new(
                    "unsupported_action",
                    "remote standby is not wired",
                )),
                Action::PlayYoutube { .. } => ApplyOutcome::Rejected(Rejection::new(
                    "unsupported_source",
                    "YouTube playback is not available in this build",
                )),
            }
        })
    }
}

#[cfg(feature = "native-ui")]
fn join_cloud_thread(handle: Option<std::thread::JoinHandle<()>>) {
    let Some(handle) = handle else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        tracing::warn!("cloud thread did not stop within 8 s");
    }
}

/// Start the kernel HDMI-CEC bridge for local physical input.
///
/// A missing adapter or helper is reported as a typed [`tv_client::input::CecStatus`]
/// by the bridge thread; startup itself only fails when the supervisor thread
/// cannot be created, in which case the keyboard path remains usable.
#[cfg(feature = "native-ui")]
fn start_cec_bridge(ui: &tv_client::ui::UiHandle) -> Option<tv_client::input::CecBridge> {
    use tv_client::input::{CecBridge, CecConfig};

    let sink = ui.clone();
    match CecBridge::start(
        CecConfig::kernel(),
        move |event| sink.submit_input(event),
        log_cec_status,
    ) {
        Ok(bridge) => Some(bridge),
        Err(error) => {
            tracing::warn!(%error, "CEC bridge unavailable; keyboard input remains available");
            None
        }
    }
}

#[cfg(feature = "native-ui")]
fn log_cec_status(status: &tv_client::input::CecStatus) {
    use tv_client::input::CecStatus;

    match status {
        CecStatus::Starting => tracing::debug!("CEC bridge starting"),
        CecStatus::Monitoring { device } => {
            tracing::info!(device = %device.display(), "CEC monitoring started");
        }
        CecStatus::Unavailable { reason } => {
            tracing::warn!(
                ?reason,
                "CEC unavailable; USB keyboard input remains available"
            );
        }
        CecStatus::Disconnected { reason, retry_in } => {
            tracing::warn!(?reason, ?retry_in, "CEC disconnected; retrying");
        }
        CecStatus::Stopped => tracing::debug!("CEC bridge stopped"),
    }
}

/// Wait for the controller to finish its final Stop/progress/transcode
/// release, without blocking the UI process forever.
#[cfg(all(feature = "native-ui", feature = "gstreamer-player"))]
fn join_playback_thread(handle: Option<std::thread::JoinHandle<()>>) {
    use std::time::{Duration, Instant};

    let Some(handle) = handle else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        tracing::warn!("playback thread did not stop within 8 s");
    }
}

/// Translate a local UI command into a controller command.
#[cfg(feature = "native-ui")]
fn controller_command(
    command: &tv_client::ui::UiCommand,
) -> Option<tv_client::playback::PlaybackCommand> {
    use tv_client::model::{Action, Direction};
    use tv_client::playback::{LibraryFilter, PlayRequest, PlaybackCommand};
    use tv_client::ui::{SourceFilter, UiCommand, UiPlaybackCommand};

    match command {
        UiCommand::Action(Action::PlayJellyfin { item_id }) => {
            Some(PlaybackCommand::Play(PlayRequest::new(item_id.clone())))
        }
        UiCommand::Action(Action::PlayYoutube { .. }) => Some(PlaybackCommand::Unsupported {
            code: "unsupported_source".to_string(),
            message: "YouTube playback is not supported by this build".to_string(),
        }),
        UiCommand::Action(Action::SetPaused { paused }) => Some(if *paused {
            PlaybackCommand::Pause
        } else {
            PlaybackCommand::Resume
        }),
        UiCommand::Action(Action::Seek { position_ms }) => Some(PlaybackCommand::Seek {
            position_ms: *position_ms,
        }),
        UiCommand::Action(Action::Stop) => Some(PlaybackCommand::Stop),
        UiCommand::Action(Action::Navigate { direction })
            if matches!(*direction, Direction::Back | Direction::Home) =>
        {
            Some(PlaybackCommand::Stop)
        }
        UiCommand::Playback(UiPlaybackCommand::Retry) => Some(PlaybackCommand::Retry),
        UiCommand::Playback(UiPlaybackCommand::NextEpisode) => Some(PlaybackCommand::NextEpisode),
        UiCommand::Playback(UiPlaybackCommand::SelectAudio { index }) => {
            Some(PlaybackCommand::SelectAudio { index: *index })
        }
        UiCommand::Playback(UiPlaybackCommand::SelectSubtitle { index }) => {
            Some(PlaybackCommand::SelectSubtitle { index: *index })
        }
        UiCommand::Retry => Some(PlaybackCommand::RetryLibrary),
        UiCommand::LoadLibrary { filter } => Some(match *filter {
            SourceFilter::All | SourceFilter::Jellyfin => PlaybackCommand::LoadLibrary {
                filter: LibraryFilter::Jellyfin,
            },
            SourceFilter::YouTube => PlaybackCommand::Unsupported {
                code: "unsupported_source".to_string(),
                message: "YouTube playback is not supported by this build".to_string(),
            },
        }),
        UiCommand::Action(_)
        | UiCommand::ApprovePairing { .. }
        | UiCommand::DenyPairing { .. }
        | UiCommand::StartPairing => None,
    }
}

/// Run the Jellyfin client and native player on a dedicated thread.
#[cfg(all(feature = "native-ui", feature = "gstreamer-player"))]
fn start_playback_thread(
    config: ClientConfig,
    state_directory: PathBuf,
    commands: tokio::sync::mpsc::Receiver<tv_client::playback::PlaybackCommand>,
    shutdown: tv_client::state::ShutdownSignal,
    ui: tv_client::ui::UiHandle,
) -> Option<std::thread::JoinHandle<()>> {
    let thread = std::thread::Builder::new()
        .name("tv-playback".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "cannot start the playback runtime");
                    return;
                }
            };
            runtime.block_on(async move {
                let mut storage = match tv_client::storage::Storage::open(
                    &state_directory,
                    config.artwork_cache_bytes,
                ) {
                    Ok(storage) => storage,
                    Err(error) => {
                        tracing::error!(%error, "cannot open playback storage");
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "jellyfin_unavailable",
                            "Playback storage is unavailable",
                        )
                        .await;
                        return;
                    }
                };
                let mut client = match tv_client::jellyfin::JellyfinClient::new(
                    &config,
                    "tv-client",
                    &mut storage,
                ) {
                    Ok(client) => client,
                    Err(error) => {
                        tracing::warn!(%error, "Jellyfin is not configured; playback disabled");
                        ui.set_library_error("Jellyfin is not configured".to_string());
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "jellyfin_unconfigured",
                            "Jellyfin is not configured",
                        )
                        .await;
                        return;
                    }
                };
                let (cancel_handle, cancel) = tv_client::jellyfin::cancellation();
                match client.restore_session(&mut storage, &cancel).await {
                    Ok(true) => {}
                    Ok(false) => {
                        cancel_handle.cancel();
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "jellyfin_sign_in",
                            "Jellyfin sign-in required",
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Jellyfin session restore failed");
                        cancel_handle.cancel();
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "jellyfin_unavailable",
                            "Jellyfin is unavailable",
                        )
                        .await;
                        return;
                    }
                }

                if let Ok(result) = client
                    .resume(tv_client::jellyfin::Page::first(), &cancel)
                    .await
                {
                    let items = result
                        .items
                        .into_iter()
                        .take(tv_client::ui::MAX_CONTINUE_WATCHING)
                        .map(|item| {
                            tv_client::ui::MediaItem::new(
                                tv_client::model::Source::Jellyfin,
                                item.id,
                                item.name.unwrap_or_else(|| "Continue watching".to_string()),
                            )
                        })
                        .collect();
                    ui.set_continue_watching(items);
                }

                let factory = match tv_client::player::GStreamerChildFactory::sibling() {
                    Ok(factory) => factory,
                    Err(error) => {
                        tracing::warn!(%error, "native player binary is unavailable");
                        cancel_handle.cancel();
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "player_unavailable",
                            "Native player is unavailable",
                        )
                        .await;
                        return;
                    }
                };
                let session = match tv_client::player::PlayerSession::start(
                    factory,
                    tv_client::player::SupervisorConfig::default(),
                    8,
                    32,
                ) {
                    Ok(session) => session,
                    Err(error) => {
                        tracing::warn!(%error, "cannot start the player supervisor");
                        cancel_handle.cancel();
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "player_unavailable",
                            "Native player is unavailable",
                        )
                        .await;
                        return;
                    }
                };
                let controller = match tv_client::playback::PlaybackController::new(
                    client,
                    session,
                    tv_client::playback::PlaybackConfig::default(),
                ) {
                    Ok(controller) => controller,
                    Err(error) => {
                        tracing::warn!(%error, "cannot start the playback controller");
                        cancel_handle.cancel();
                        run_unavailable(
                            commands,
                            ui,
                            shutdown,
                            "controller_unavailable",
                            "Playback controller is unavailable",
                        )
                        .await;
                        return;
                    }
                };
                let (updates, mut received) = tokio::sync::mpsc::channel(32);
                let ui_updates = ui.clone();
                let updater = tokio::spawn(async move {
                    while let Some(update) = received.recv().await {
                        ui_updates.apply_playback_update(update);
                    }
                });
                if let Err(error) = controller.run(commands, updates, shutdown, cancel).await {
                    tracing::warn!(%error, "playback controller stopped");
                }
                let _ = updater.await;
                cancel_handle.cancel();
            });
        });
    match thread {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "cannot spawn the playback thread");
            None
        }
    }
}

#[cfg(all(feature = "native-ui", feature = "gstreamer-player"))]
async fn run_unavailable(
    mut commands: tokio::sync::mpsc::Receiver<tv_client::playback::PlaybackCommand>,
    ui: tv_client::ui::UiHandle,
    mut shutdown: tv_client::state::ShutdownSignal,
    code: &'static str,
    message: &'static str,
) {
    use tv_client::playback::PlaybackCommand;

    ui.set_library_error(message.to_string());
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    PlaybackCommand::LoadLibrary { .. } | PlaybackCommand::RetryLibrary => {
                        ui.set_library_error(message.to_string());
                    }
                    // Navigation/back and player controls on an error screen are
                    // no-ops, not new failures; otherwise Escape would re-open
                    // the error screen and trap the UI.
                    PlaybackCommand::Pause
                    | PlaybackCommand::Resume
                    | PlaybackCommand::TogglePause
                    | PlaybackCommand::Seek { .. }
                    | PlaybackCommand::Stop => {}
                    _ => ui.apply_playback_update(playback_failure(code, message)),
                }
            }
        }
    }
}

#[cfg(all(feature = "native-ui", feature = "gstreamer-player"))]
fn playback_failure(code: &str, message: &str) -> tv_client::playback::PlaybackUpdate {
    tv_client::playback::PlaybackUpdate::Failed {
        item_id: None,
        code: code.to_string(),
        message: Some(message.to_string()),
        retry_available: false,
    }
}

fn run_enroll(
    args: EnrollArgs,
    parent_config: Option<PathBuf>,
    parent_state_dir: Option<PathBuf>,
) -> Result<(), CliError> {
    let config_path = args
        .config
        .or(parent_config)
        .or_else(config::default_config_path);
    let config = load_config(config_path.as_deref(), true)?;
    let server_url = config
        .server_url
        .clone()
        .ok_or(CliError::MissingServerUrl)?;
    let state_directory = args
        .state_dir
        .or(parent_state_dir)
        .or_else(storage::default_state_dir)
        .ok_or(CliError::MissingHome)?;
    let mut storage = Storage::open(&state_directory, config.artwork_cache_bytes)?;
    let token = read_enrollment_token()?;
    let http = CloudHttp::new(&server_url)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let enrollment = runtime.block_on(http.enroll(&token, &args.label))?;
    cloud::store_device(&mut storage, &enrollment)?;
    println!(
        "Enrolled device {} with protocol {}",
        enrollment.device_id, enrollment.protocol
    );
    Ok(())
}

/// Read the one-use token from stdin without echoing it ourselves.
///
/// The token is never accepted from argv, the environment or a URL. Terminal
/// echo is a property of the caller's TTY; operators can pipe the value to
/// keep it out of the terminal scrollback entirely.
fn read_enrollment_token() -> Result<EnrollmentToken, CliError> {
    eprintln!("One-use enrollment token (read from stdin; not stored in argv):");
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Err(CliError::MissingEnrollmentToken);
    }
    let token = line.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        return Err(CliError::MissingEnrollmentToken);
    }
    EnrollmentToken::new(token).map_err(|_| CliError::InvalidEnrollmentToken)
}

fn load_config(path: Option<&Path>, explicit: bool) -> Result<ClientConfig, CliError> {
    if let Some(path) = path {
        if path.exists() {
            return Ok(ClientConfig::load(path)?);
        }
        if explicit {
            return Err(CliError::MissingConfig {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(ClientConfig::default())
}

#[derive(Debug, Error)]
enum CliError {
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("configuration file not found: {path:?}")]
    MissingConfig { path: PathBuf },
    #[error("cannot determine a default directory; set HOME or pass an explicit path")]
    MissingHome,
    #[error("server_url is required for enrollment")]
    MissingServerUrl,
    #[error("no enrollment token was provided on stdin")]
    MissingEnrollmentToken,
    #[error("the enrollment token is not valid")]
    InvalidEnrollmentToken,
    #[error("cloud error: {0}")]
    Cloud(#[from] CloudError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "native-ui")]
    #[error("native UI error: {0}")]
    Ui(#[from] tv_client::ui::NativeUiError),
}

impl CliError {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_)
            | Self::MissingConfig { .. }
            | Self::MissingServerUrl
            | Self::MissingEnrollmentToken
            | Self::InvalidEnrollmentToken => ExitCode::from(2),
            Self::Storage(_) | Self::MissingHome | Self::Cloud(_) | Self::Io(_) => {
                ExitCode::FAILURE
            }
            #[cfg(feature = "native-ui")]
            Self::Ui(_) => ExitCode::FAILURE,
        }
    }
}
