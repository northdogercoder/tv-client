//! Command-line entry point for `tv-client`.

#[cfg(feature = "native-ui")]
use std::path::Path;
use std::path::PathBuf;
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
    let state_directory = match state_dir_argument {
        Some(path) => Some(path),
        None => storage::default_state_dir(),
    };
    let selection = config::select_config(config_argument.as_deref(), state_directory.as_deref());
    let config = load_selected_config(&selection)?;
    if check_config {
        println!("configuration OK");
        return Ok(());
    }

    let state_directory = state_directory.ok_or(CliError::MissingHome)?;
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

/// Control-plane request routed by the single media arbiter.
#[cfg(feature = "native-ui")]
#[derive(Debug, Clone)]
enum RouteCommand {
    /// Start Jellyfin playback through the C006 controller.
    PlayJellyfin(String),
    /// Start local YouTube playback; `remote` records the command origin.
    PlayYoutube { video_id: String, remote: bool },
    /// Pause or resume the active source.
    Pause(bool),
    /// Seek the active source.
    Seek(u64),
    /// Stop whichever source is active.
    Stop,
    /// Retry the last failed start of the active source.
    Retry,
    /// Forward a Jellyfin-only controller command.
    Jellyfin(tv_client::playback::PlaybackCommand),
    /// The cloud socket dropped; remote-initiated YouTube stops.
    NetworkLost,
}

/// One explicit YouTube search request from the UI.
#[cfg(feature = "native-ui")]
#[derive(Debug, Clone)]
enum SearchCommand {
    Search {
        request_id: u64,
        query: String,
        page_token: Option<String>,
    },
    Cancel,
}

#[cfg(feature = "native-ui")]
fn route_command(command: &tv_client::ui::UiCommand) -> Option<RouteCommand> {
    use tv_client::model::{Action, Direction};
    use tv_client::playback::{LibraryFilter, PlaybackCommand};
    use tv_client::ui::{SourceFilter, UiCommand, UiPlaybackCommand};

    match command {
        UiCommand::Action(Action::PlayJellyfin { item_id }) => {
            Some(RouteCommand::PlayJellyfin(item_id.clone()))
        }
        UiCommand::Action(Action::PlayYoutube { video_id }) => Some(RouteCommand::PlayYoutube {
            video_id: video_id.clone(),
            remote: false,
        }),
        UiCommand::Action(Action::SetPaused { paused }) => Some(RouteCommand::Pause(*paused)),
        UiCommand::Action(Action::Seek { position_ms }) => Some(RouteCommand::Seek(*position_ms)),
        UiCommand::Action(Action::Stop) => Some(RouteCommand::Stop),
        UiCommand::Action(Action::Navigate { direction })
            if matches!(*direction, Direction::Back | Direction::Home) =>
        {
            Some(RouteCommand::Stop)
        }
        UiCommand::Playback(UiPlaybackCommand::Retry) => Some(RouteCommand::Retry),
        UiCommand::Playback(UiPlaybackCommand::NextEpisode) => {
            Some(RouteCommand::Jellyfin(PlaybackCommand::NextEpisode))
        }
        UiCommand::Playback(UiPlaybackCommand::SelectAudio { index }) => {
            Some(RouteCommand::Jellyfin(PlaybackCommand::SelectAudio {
                index: *index,
            }))
        }
        UiCommand::Playback(UiPlaybackCommand::SelectSubtitle { index }) => {
            Some(RouteCommand::Jellyfin(PlaybackCommand::SelectSubtitle {
                index: *index,
            }))
        }
        UiCommand::Retry => Some(RouteCommand::Jellyfin(PlaybackCommand::RetryLibrary)),
        UiCommand::LoadLibrary { filter } => match *filter {
            SourceFilter::All | SourceFilter::Jellyfin => {
                Some(RouteCommand::Jellyfin(PlaybackCommand::LoadLibrary {
                    filter: LibraryFilter::Jellyfin,
                }))
            }
            SourceFilter::YouTube => None,
        },
        UiCommand::Action(_)
        | UiCommand::YoutubeSearch { .. }
        | UiCommand::YoutubeMore { .. }
        | UiCommand::YoutubeCancel
        | UiCommand::ApprovePairing { .. }
        | UiCommand::DenyPairing { .. }
        | UiCommand::StartPairing
        | UiCommand::JellyfinSignIn
        | UiCommand::JellyfinSignOut
        | UiCommand::CancelQuickConnect
        | UiCommand::SaveSettings { .. }
        | UiCommand::Restart => None,
    }
}

#[cfg(feature = "native-ui")]
fn search_command(command: &tv_client::ui::UiCommand) -> Option<SearchCommand> {
    use tv_client::ui::UiCommand;

    match command {
        UiCommand::YoutubeSearch { request_id, query } => Some(SearchCommand::Search {
            request_id: *request_id,
            query: query.clone(),
            page_token: None,
        }),
        UiCommand::YoutubeMore {
            request_id,
            query,
            page_token,
        } => Some(SearchCommand::Search {
            request_id: *request_id,
            query: query.clone(),
            page_token: Some(page_token.clone()),
        }),
        UiCommand::YoutubeCancel => Some(SearchCommand::Cancel),
        _ => None,
    }
}

#[cfg(feature = "native-ui")]
fn launch_native_ui(config: &ClientConfig, state_directory: &Path) -> Result<(), CliError> {
    use tokio::sync::{mpsc, watch};

    use tv_client::model::Source;
    use tv_client::playback::PlaybackCommand;
    use tv_client::ui::UiCommand;
    use tv_client::youtube::{
        PLAYER_COMMAND_CAPACITY, PlaybackConfig as YoutubePlaybackConfig, PlayerCommand,
    };

    let youtube_config = YoutubePlaybackConfig::from_client(config);
    let (commands, command_rx) = mpsc::channel::<PlaybackCommand>(32);
    let (youtube_commands, youtube_rx) = mpsc::channel::<PlayerCommand>(PLAYER_COMMAND_CAPACITY);
    let (route_commands, route_rx) = mpsc::channel::<RouteCommand>(16);
    let (search_commands, search_rx) = mpsc::channel::<SearchCommand>(4);
    let (activity, _activity_rx) = watch::channel::<Option<Source>>(None);
    let cloud_routes = route_commands.clone();
    let ui_routes = route_commands.clone();
    let ui_search = search_commands.clone();
    let (cloud_control, cloud_control_rx) = mpsc::channel::<CloudControl>(32);
    let (settings_requests, settings_rx) = mpsc::channel::<SettingsRequest>(16);
    let quick_connect: QuickConnectCancellation = Default::default();
    let ui_quick_connect = quick_connect.clone();
    let (shutdown, signal) = tv_client::state::shutdown_channel();
    let (media_shutdown, media_signal) = tv_client::state::shutdown_channel();
    let ui = tv_client::ui::NativeUi::new(tv_client::AppState::default(), move |command| {
        match &command {
            UiCommand::StartPairing => {
                let _ = cloud_control.try_send(CloudControl::StartPairing);
            }
            UiCommand::ApprovePairing { request_id } => {
                let _ = cloud_control.try_send(CloudControl::Decide {
                    request_id: *request_id,
                    accept: true,
                });
            }
            UiCommand::DenyPairing { request_id } => {
                let _ = cloud_control.try_send(CloudControl::Decide {
                    request_id: *request_id,
                    accept: false,
                });
            }
            UiCommand::SaveSettings { config } => {
                let _ = settings_requests.try_send(SettingsRequest::Save(config.clone()));
            }
            UiCommand::JellyfinSignIn => {
                // Signal an in-flight poll/token exchange before the queued
                // request reaches the worker.
                cancel_active_quick_connect(&ui_quick_connect);
                let _ = settings_requests.try_send(SettingsRequest::JellyfinSignIn);
            }
            UiCommand::JellyfinSignOut => {
                cancel_active_quick_connect(&ui_quick_connect);
                let _ = settings_requests.try_send(SettingsRequest::JellyfinSignOut);
            }
            UiCommand::CancelQuickConnect => {
                cancel_active_quick_connect(&ui_quick_connect);
                let _ = settings_requests.try_send(SettingsRequest::CancelQuickConnect);
            }
            UiCommand::Restart => {
                cancel_active_quick_connect(&ui_quick_connect);
                let _ = settings_requests.try_send(SettingsRequest::Restart);
            }
            other => {
                // The Slint callback never blocks and never performs I/O: a
                // full queue drops the command rather than stalling the UI.
                if let Some(search) = search_command(other) {
                    let _ = ui_search.try_send(search);
                    return;
                }
                if let Some(route) = route_command(other) {
                    let _ = ui_routes.try_send(route);
                }
            }
        }
    })?;
    let handle = ui.handle();
    handle.set_settings_config(config.clone());
    handle.set_safe_area_percent(config.ui_safe_area_percent);
    let cec_bridge = start_cec_bridge(&handle);

    #[cfg(feature = "gstreamer-player")]
    let playback_thread = start_playback_thread(
        config.clone(),
        state_directory.to_path_buf(),
        command_rx,
        signal,
        handle.clone(),
        activity.clone(),
    );
    #[cfg(not(feature = "gstreamer-player"))]
    {
        // Keep the native-ui-only build warning-free: the playback
        // controller is compiled out, so these wiring values are unused.
        drop(command_rx);
        drop(signal);
        tracing::warn!("native UI built without gstreamer-player; Jellyfin playback is disabled");
    }

    let media_thread = start_media_thread(
        config.clone(),
        state_directory.to_path_buf(),
        handle.clone(),
        youtube_config,
        youtube_rx,
        route_rx,
        search_rx,
        commands,
        youtube_commands,
        activity,
        media_signal,
    );

    let (cloud_shutdown, cloud_signal) = tv_client::state::shutdown_channel();
    let cloud_thread = start_cloud_thread(
        config.clone(),
        state_directory.to_path_buf(),
        handle.clone(),
        cloud_routes,
        cloud_control_rx,
        cloud_signal,
    );

    let (settings_shutdown, settings_signal) = tv_client::state::shutdown_channel();
    let settings_thread = start_settings_thread(
        config.clone(),
        state_directory.to_path_buf(),
        handle.clone(),
        settings_rx,
        settings_signal,
        quick_connect.clone(),
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
    media_shutdown.cancel();
    cloud_shutdown.cancel();
    // Stop an in-flight poll or token exchange before waiting for the worker.
    cancel_active_quick_connect(&quick_connect);
    settings_shutdown.cancel();
    #[cfg(feature = "gstreamer-player")]
    join_playback_thread(playback_thread);
    join_media_thread(media_thread);
    join_cloud_thread(cloud_thread);
    join_settings_thread(settings_thread);
    result?;
    Ok(())
}

/// Local settings and Quick Connect requests handled off the Slint loop.
#[cfg(feature = "native-ui")]
#[derive(Debug)]
enum SettingsRequest {
    /// Atomically write a validated configuration to the state directory.
    Save(ClientConfig),
    /// Start the Jellyfin Quick Connect flow.
    JellyfinSignIn,
    /// Revoke and clear the Jellyfin session.
    JellyfinSignOut,
    /// Cancel an in-flight Quick Connect poll.
    CancelQuickConnect,
    /// Exit cleanly for the supervisor to restart.
    Restart,
}

/// Shared handle to the Quick Connect request currently owned by the worker.
///
/// The UI callback and shutdown path cancel this handle directly so an
/// in-flight poll or token exchange stops before the queued request is
/// processed. The worker clears the slot whenever the flow ends.
#[cfg(feature = "native-ui")]
type QuickConnectCancellation =
    std::sync::Arc<std::sync::Mutex<Option<tv_client::jellyfin::CancelHandle>>>;

#[cfg(feature = "native-ui")]
fn quick_connect_guard(
    slot: &QuickConnectCancellation,
) -> std::sync::MutexGuard<'_, Option<tv_client::jellyfin::CancelHandle>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cancel and forget the active Quick Connect request, if any.
#[cfg(feature = "native-ui")]
fn cancel_active_quick_connect(slot: &QuickConnectCancellation) {
    let handle = quick_connect_guard(slot).take();
    if let Some(handle) = handle {
        handle.cancel();
    }
}

/// Forget the active Quick Connect request without cancelling it.
#[cfg(feature = "native-ui")]
fn clear_active_quick_connect(slot: &QuickConnectCancellation) {
    let _ = quick_connect_guard(slot).take();
}

/// Stop the active flow and shared handle; returns whether anything was active.
#[cfg(feature = "native-ui")]
fn stop_quick_connect(
    flow: &mut Option<QuickConnectFlow>,
    slot: &QuickConnectCancellation,
) -> bool {
    let mut stopped = false;
    if let Some(active) = flow.take() {
        active.cancel.cancel();
        stopped = true;
    }
    let handle = quick_connect_guard(slot).take();
    if let Some(handle) = handle {
        handle.cancel();
        stopped = true;
    }
    stopped
}

#[cfg(feature = "native-ui")]
fn start_cloud_thread(
    config: ClientConfig,
    state_directory: PathBuf,
    ui: tv_client::ui::UiHandle,
    routes: tokio::sync::mpsc::Sender<RouteCommand>,
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
                routes,
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
    routes: tokio::sync::mpsc::Sender<RouteCommand>,
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

    let youtube_ready = tv_client::youtube::PlaybackConfig::from_client(&config).enabled;
    let mut link_config = match LinkConfig::new(
        &server_url,
        device.device_id,
        env!("CARGO_PKG_VERSION"),
        cloud::capabilities(config.jellyfin_url.is_some(), youtube_ready),
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
        routes: routes.clone(),
        device_id: device.device_id,
        share_titles,
        youtube_enabled: youtube_ready,
    });
    let link = match DeviceLink::new(
        link_config,
        device.token,
        state_rx,
        events_tx,
        applier,
        CommandPolicy::with_youtube(config.jellyfin_url.is_some(), youtube_ready),
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
                        let _ = routes.try_send(RouteCommand::NetworkLost);
                        ui.set_cloud_status("Cloud offline; reconnecting".to_string());
                    }
                    LinkEvent::Revoked => {
                        let _ = routes.try_send(RouteCommand::NetworkLost);
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
                        let _ = routes.try_send(RouteCommand::NetworkLost);
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
    routes: tokio::sync::mpsc::Sender<RouteCommand>,
    device_id: uuid::Uuid,
    share_titles: bool,
    youtube_enabled: bool,
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

    fn send_route(&self, command: RouteCommand) -> Result<(), tv_client::cloud::Rejection> {
        self.routes.try_send(command).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                tv_client::cloud::Rejection::new("queue_full", "playback queue is full")
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => tv_client::cloud::Rejection::new(
                "playback_unavailable",
                "playback is not available in this build",
            ),
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
        if let Err(rejection) = self.send_route(RouteCommand::PlayJellyfin(item_id.clone())) {
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

    async fn apply_play_youtube(&self, video_id: String) -> tv_client::cloud::ApplyOutcome {
        use tv_client::cloud::{ApplyOutcome, Rejection};

        if !self.youtube_enabled {
            return ApplyOutcome::Rejected(Rejection::new(
                "youtube_disabled",
                "YouTube playback is disabled by local configuration",
            ));
        }
        if tv_client::youtube::VideoId::parse(&video_id).is_err() {
            return ApplyOutcome::Rejected(Rejection::invalid_action());
        }
        let was_error = self
            .snapshot()
            .is_some_and(|state| state.error_code.is_some());
        let ui = self.ui_handle();
        ui.clear_player_error();
        let mut saw_recovery = !was_error
            || self
                .snapshot()
                .is_some_and(|state| state.error_code.is_none());
        if let Err(rejection) = self.send_route(RouteCommand::PlayYoutube {
            video_id: video_id.clone(),
            remote: true,
        }) {
            return ApplyOutcome::Rejected(rejection);
        }
        match self
            .wait_for(std::time::Duration::from_secs(20), |state| {
                if state.error_code.is_none() {
                    saw_recovery = true;
                }
                let evidence = tv_client::youtube::playback_evidence_fields(
                    state.source,
                    state.item_id.as_deref(),
                    state.mode,
                    state.position_ms,
                    state.error_code.as_deref(),
                    &video_id,
                );
                evidence || (saw_recovery && state.error_code.is_some())
            })
            .await
        {
            None => {
                ApplyOutcome::Failed(Rejection::new("timeout", "YouTube playback did not start"))
            }
            Some(state) if state.error_code.is_some() => ApplyOutcome::Failed(Rejection::new(
                "playback_failed",
                "the local YouTube player reported a failure",
            )),
            Some(_) => ApplyOutcome::Applied,
        }
    }

    async fn apply_pause(&self, paused: bool) -> tv_client::cloud::ApplyOutcome {
        use tv_client::cloud::{ApplyOutcome, Rejection};

        if self.snapshot().is_some_and(|state| state.source.is_none()) {
            return ApplyOutcome::Rejected(Rejection::invalid_mode());
        }
        let command = RouteCommand::Pause(paused);
        if let Err(rejection) = self.send_route(command) {
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
        if let Err(rejection) = self.send_route(RouteCommand::Seek(position_ms)) {
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

        if let Err(rejection) = self.send_route(RouteCommand::Stop) {
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
                Action::PlayYoutube { video_id } => self.apply_play_youtube(video_id).await,
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

#[cfg(feature = "native-ui")]
fn start_settings_thread(
    config: ClientConfig,
    state_directory: PathBuf,
    ui: tv_client::ui::UiHandle,
    requests: tokio::sync::mpsc::Receiver<SettingsRequest>,
    shutdown: tv_client::state::ShutdownSignal,
    quick_connect: QuickConnectCancellation,
) -> Option<std::thread::JoinHandle<()>> {
    let status_ui = ui.clone();
    let thread = std::thread::Builder::new()
        .name("tv-settings".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "cannot start the settings runtime");
                    ui.set_settings_error("Settings are unavailable".to_string());
                    return;
                }
            };
            runtime.block_on(run_settings(
                config,
                state_directory,
                ui,
                requests,
                shutdown,
                quick_connect,
            ));
        });
    match thread {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "cannot spawn the settings thread");
            status_ui.set_settings_error("Settings are unavailable".to_string());
            None
        }
    }
}

#[cfg(feature = "native-ui")]
fn join_settings_thread(handle: Option<std::thread::JoinHandle<()>>) {
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
        tracing::warn!("settings thread did not stop within 8 s");
    }
}

/// One in-flight Jellyfin Quick Connect request.
#[cfg(feature = "native-ui")]
struct QuickConnectFlow {
    client: tv_client::jellyfin::JellyfinClient,
    storage: Storage,
    initiation: tv_client::jellyfin::QuickConnectInitiation,
    cancel: tv_client::jellyfin::CancelHandle,
    token: tv_client::jellyfin::CancelToken,
}

/// Result of one Quick Connect poll.
#[cfg(feature = "native-ui")]
enum QuickConnectStep {
    Pending,
    Approved(String),
    Expired,
    Cancelled,
    Failed(&'static str),
    Retry,
}

/// Settings notice asking the operator to apply a restart.
#[cfg(feature = "native-ui")]
enum RestartNotice {
    Info(String),
    Error(String),
}

/// Show the restart notice without exiting the worker.
///
/// The playback thread builds its Jellyfin client once at startup, and a
/// client that started without a token is parked in `run_unavailable`. Only
/// the settings Restart row can rebuild that client. The worker stays alive so
/// a Save queued during Quick Connect or sign-out is still processed; the
/// notice tells the operator to apply the restart.
#[cfg(feature = "native-ui")]
fn show_restart_notice(ui: &tv_client::ui::UiHandle, notice: RestartNotice) {
    match notice {
        RestartNotice::Info(message) => ui.set_settings_message(message),
        RestartNotice::Error(message) => ui.set_settings_error(message),
    }
}

/// Settings worker: configuration writes and Jellyfin sign-in.
///
/// All blocking file writes run on `spawn_blocking`; network calls are
/// bounded and cancellable. The Slint loop only receives posted updates.
#[cfg(feature = "native-ui")]
async fn run_settings(
    mut config: ClientConfig,
    state_directory: PathBuf,
    ui: tv_client::ui::UiHandle,
    mut requests: tokio::sync::mpsc::Receiver<SettingsRequest>,
    mut shutdown: tv_client::state::ShutdownSignal,
    quick_connect: QuickConnectCancellation,
) {
    use std::time::Duration;

    if let Some(user) = restore_jellyfin_session(&config, &state_directory).await {
        ui.set_jellyfin_user(Some(user));
    }
    let mut flow: Option<QuickConnectFlow> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                stop_quick_connect(&mut flow, &quick_connect);
                break;
            }
            request = requests.recv() => {
                let Some(request) = request else {
                    stop_quick_connect(&mut flow, &quick_connect);
                    break;
                };
                match request {
                    SettingsRequest::Save(new_config) => {
                        let directory = state_directory.clone();
                        let document = new_config.clone();
                        let written = tokio::task::spawn_blocking(move || {
                            tv_client::settings::save_state_config(&directory, &document)
                        })
                        .await;
                        match written {
                            Ok(Ok(_)) => {
                                config = new_config;
                                ui.set_settings_config(config.clone());
                                ui.settings_saved();
                                tracing::info!("state configuration written");
                            }
                            Ok(Err(error)) => {
                                tracing::warn!(%error, "state configuration write failed");
                                ui.settings_save_failed();
                            }
                            Err(error) => {
                                tracing::warn!(%error, "state configuration worker failed");
                                ui.settings_save_failed();
                            }
                        }
                    }
                    SettingsRequest::JellyfinSignIn => {
                        stop_quick_connect(&mut flow, &quick_connect);
                        ui.set_settings_message("Starting Jellyfin Quick Connect".to_string());
                        let (cancel, token) = tv_client::jellyfin::cancellation();
                        *quick_connect_guard(&quick_connect) = Some(cancel.clone());
                        match begin_quick_connect(&config, &state_directory, cancel, token).await {
                            Ok(active) => {
                                ui.set_quick_connect_code(active.initiation.code().to_owned());
                                ui.set_settings_message(
                                    "Enter the code in Jellyfin, then approve it".to_string(),
                                );
                                flow = Some(active);
                            }
                            Err(message) => {
                                cancel_active_quick_connect(&quick_connect);
                                ui.clear_quick_connect_code();
                                ui.set_settings_error(message.to_string());
                            }
                        }
                    }
                    SettingsRequest::JellyfinSignOut => {
                        stop_quick_connect(&mut flow, &quick_connect);
                        use tv_client::settings::JellyfinSignOutError;
                        match tv_client::settings::sign_out_jellyfin(
                            &config,
                            &state_directory,
                            Duration::from_secs(15),
                        )
                        .await
                        {
                            Ok(()) => {
                                ui.set_jellyfin_user(None);
                                ui.clear_quick_connect_code();
                                show_restart_notice(
                                    &ui,
                                    RestartNotice::Info(
                                        "Signed out of Jellyfin; press Restart to apply".to_string(),
                                    ),
                                );
                            }
                            Err(JellyfinSignOutError::Storage) => {
                                // Local cleanup failed, so the stored token may
                                // still be valid. Keep the signed-in row rather
                                // than claiming a sign-out that did not happen;
                                // restarting would not clear anything here.
                                ui.set_settings_error("Jellyfin storage is unavailable".to_string());
                            }
                            Err(JellyfinSignOutError::Timeout) => {
                                ui.set_jellyfin_user(None);
                                ui.clear_quick_connect_code();
                                show_restart_notice(
                                    &ui,
                                    RestartNotice::Error(
                                        "Jellyfin sign-out timed out; local token cleared; press Restart to finish signing out"
                                            .to_string(),
                                    ),
                                );
                            }
                            Err(JellyfinSignOutError::Revoke(error)) => {
                                ui.set_jellyfin_user(None);
                                ui.clear_quick_connect_code();
                                show_restart_notice(
                                    &ui,
                                    RestartNotice::Error(format!(
                                        "{}; local token cleared; press Restart to finish signing out",
                                        jellyfin_error_text(&error)
                                    )),
                                );
                            }
                        }
                    }
                    SettingsRequest::CancelQuickConnect => {
                        let was_active = stop_quick_connect(&mut flow, &quick_connect);
                        ui.clear_quick_connect_code();
                        if was_active {
                            ui.set_settings_message("Quick Connect cancelled".to_string());
                        }
                    }
                    SettingsRequest::Restart => {
                        stop_quick_connect(&mut flow, &quick_connect);
                        tracing::info!("restart requested from settings");
                        ui.request_restart();
                    }
                }
            }
            _ = tick.tick(), if flow.is_some() => {
                let Some(active) = flow.as_mut() else {
                    continue;
                };
                match poll_quick_connect(active).await {
                    QuickConnectStep::Pending => {}
                    QuickConnectStep::Retry => {}
                    QuickConnectStep::Approved(user) => {
                        // The UI or shutdown path empties the slot to cancel.
                        // If that happened after the token exchange's own
                        // check, drop the token before accepting the sign-in.
                        let cancelled_late = quick_connect_guard(&quick_connect).is_none();
                        let active = flow.take();
                        clear_active_quick_connect(&quick_connect);
                        if cancelled_late {
                            if let Some(active) = active {
                                discard_stored_jellyfin_token(&active.storage);
                            }
                            ui.set_settings_message("Quick Connect cancelled".to_string());
                            ui.clear_quick_connect_code();
                        } else {
                            ui.set_jellyfin_user(Some(user));
                            ui.clear_quick_connect_code();
                            // The playback thread already reached
                            // `run_unavailable` without a token at startup, so
                            // the library is not usable until the supervisor
                            // rebuilds the process with the stored token.
                            show_restart_notice(
                                &ui,
                                RestartNotice::Info(
                                    "Quick Connect succeeded; press Restart to load Jellyfin"
                                        .to_string(),
                                ),
                            );
                        }
                    }
                    QuickConnectStep::Expired => {
                        flow = None;
                        clear_active_quick_connect(&quick_connect);
                        ui.set_settings_error(
                            "Quick Connect expired; start sign-in again".to_string(),
                        );
                        ui.clear_quick_connect_code();
                    }
                    QuickConnectStep::Cancelled => {
                        flow = None;
                        clear_active_quick_connect(&quick_connect);
                        ui.set_settings_message("Quick Connect cancelled".to_string());
                        ui.clear_quick_connect_code();
                    }
                    QuickConnectStep::Failed(message) => {
                        flow = None;
                        clear_active_quick_connect(&quick_connect);
                        ui.set_settings_error(message.to_string());
                        ui.clear_quick_connect_code();
                    }
                }
            }
        }
    }
}

#[cfg(feature = "native-ui")]
async fn begin_quick_connect(
    config: &ClientConfig,
    state_directory: &Path,
    cancel: tv_client::jellyfin::CancelHandle,
    token: tv_client::jellyfin::CancelToken,
) -> Result<QuickConnectFlow, &'static str> {
    use std::time::Duration;

    let mut storage = Storage::open(state_directory, config.artwork_cache_bytes)
        .map_err(|_| "Jellyfin storage is unavailable")?;
    let client = tv_client::jellyfin::JellyfinClient::new(config, "tv-client", &mut storage)
        .map_err(|error| jellyfin_error_text(&error))?;
    let initiation = match tokio::time::timeout(
        Duration::from_secs(15),
        client.quick_connect_initiate(&token),
    )
    .await
    {
        Ok(Ok(initiation)) => initiation,
        Ok(Err(error)) => return Err(jellyfin_error_text(&error)),
        Err(_) => return Err("Quick Connect timed out"),
    };
    Ok(QuickConnectFlow {
        client,
        storage,
        initiation,
        cancel,
        token,
    })
}

#[cfg(feature = "native-ui")]
async fn poll_quick_connect(flow: &mut QuickConnectFlow) -> QuickConnectStep {
    use std::time::Duration;

    use tv_client::jellyfin::{JellyfinError, QuickConnectPoll};

    if flow.initiation.is_expired() {
        return QuickConnectStep::Expired;
    }
    let QuickConnectFlow {
        client,
        storage,
        initiation,
        cancel,
        token,
        ..
    } = flow;
    if cancel.is_cancelled() {
        return QuickConnectStep::Cancelled;
    }
    let polled = tokio::time::timeout(
        Duration::from_secs(15),
        client.quick_connect_poll(initiation, token),
    )
    .await;
    match polled {
        Ok(Ok(QuickConnectPoll::Pending)) => QuickConnectStep::Pending,
        Ok(Ok(QuickConnectPoll::Approved)) => {
            if cancel.is_cancelled() {
                return QuickConnectStep::Cancelled;
            }
            let authenticated = tokio::time::timeout(
                Duration::from_secs(20),
                client.quick_connect_authenticate(storage, initiation, token),
            )
            .await;
            match authenticated {
                Ok(Ok(session)) => {
                    if cancel.is_cancelled() {
                        // Cancellation landed while the token exchange was
                        // completing; remove the token that was just written.
                        discard_stored_jellyfin_token(storage);
                        QuickConnectStep::Cancelled
                    } else {
                        QuickConnectStep::Approved(
                            session
                                .user_name()
                                .map_or_else(|| session.user_id().to_owned(), str::to_owned),
                        )
                    }
                }
                Ok(Err(JellyfinError::Cancelled)) => QuickConnectStep::Cancelled,
                Ok(Err(JellyfinError::QuickConnectExpired)) => QuickConnectStep::Expired,
                Ok(Err(error)) => QuickConnectStep::Failed(jellyfin_error_text(&error)),
                Err(_) if cancel.is_cancelled() => QuickConnectStep::Cancelled,
                Err(_) => QuickConnectStep::Failed("Jellyfin sign-in timed out"),
            }
        }
        Ok(Err(JellyfinError::QuickConnectExpired)) => QuickConnectStep::Expired,
        Ok(Err(JellyfinError::Cancelled)) => QuickConnectStep::Cancelled,
        Ok(Err(JellyfinError::Timeout | JellyfinError::Network)) => QuickConnectStep::Retry,
        Ok(Err(error)) => QuickConnectStep::Failed(jellyfin_error_text(&error)),
        Err(_) if cancel.is_cancelled() => QuickConnectStep::Cancelled,
        Err(_) => QuickConnectStep::Retry,
    }
}

/// Remove a token written by an exchange that was cancelled at the last moment.
#[cfg(feature = "native-ui")]
fn discard_stored_jellyfin_token(storage: &Storage) {
    if let Err(error) = tv_client::settings::clear_jellyfin_access_token(storage) {
        tracing::warn!(%error, "could not remove a cancelled Jellyfin token");
    }
}

#[cfg(feature = "native-ui")]
async fn restore_jellyfin_session(config: &ClientConfig, state_directory: &Path) -> Option<String> {
    use std::time::Duration;

    let mut storage = Storage::open(state_directory, config.artwork_cache_bytes).ok()?;
    let mut client =
        tv_client::jellyfin::JellyfinClient::new(config, "tv-client", &mut storage).ok()?;
    let (cancel, token) = tv_client::jellyfin::cancellation();
    let restored = tokio::time::timeout(
        Duration::from_secs(5),
        client.restore_session(&mut storage, &token),
    )
    .await;
    cancel.cancel();
    match restored {
        Ok(Ok(true)) => client.session().map(|session| {
            session
                .user_name()
                .map_or_else(|| session.user_id().to_owned(), str::to_owned)
        }),
        _ => None,
    }
}

/// Static, value-free text for a typed Jellyfin failure.
#[cfg(feature = "native-ui")]
fn jellyfin_error_text(error: &tv_client::jellyfin::JellyfinError) -> &'static str {
    use tv_client::jellyfin::JellyfinError;

    match error {
        JellyfinError::NotConfigured => "Set the Jellyfin URL first",
        JellyfinError::QuickConnectUnavailable => "Quick Connect is disabled on the server",
        JellyfinError::QuickConnectExpired => "Quick Connect expired",
        JellyfinError::QuickConnectNotApproved => "The code was not approved on the server",
        JellyfinError::AuthenticationRejected => "Jellyfin rejected the sign-in",
        JellyfinError::Timeout => "Jellyfin timed out",
        JellyfinError::Network => "Jellyfin is not reachable",
        JellyfinError::Cancelled => "Jellyfin sign-in was cancelled",
        JellyfinError::Unauthorized | JellyfinError::Forbidden => "Jellyfin rejected the session",
        JellyfinError::NotAuthenticated => "Jellyfin is not signed in",
        _ => "Jellyfin sign-in failed",
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

/// Run the Jellyfin client and native player on a dedicated thread.
#[cfg(all(feature = "native-ui", feature = "gstreamer-player"))]
fn start_playback_thread(
    config: ClientConfig,
    state_directory: PathBuf,
    commands: tokio::sync::mpsc::Receiver<tv_client::playback::PlaybackCommand>,
    shutdown: tv_client::state::ShutdownSignal,
    ui: tv_client::ui::UiHandle,
    activity: tokio::sync::watch::Sender<Option<tv_client::model::Source>>,
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
                let activity_updates = activity;
                let updater = tokio::spawn(async move {
                    while let Some(update) = received.recv().await {
                        match &update {
                            tv_client::playback::PlaybackUpdate::Loading { .. }
                            | tv_client::playback::PlaybackUpdate::Position { .. }
                            | tv_client::playback::PlaybackUpdate::Method { .. }
                            | tv_client::playback::PlaybackUpdate::Tracks { .. } => {
                                let _ =
                                    activity_updates.send(Some(tv_client::model::Source::Jellyfin));
                            }
                            tv_client::playback::PlaybackUpdate::Stopped { .. }
                            | tv_client::playback::PlaybackUpdate::Ended { .. }
                            | tv_client::playback::PlaybackUpdate::Failed { .. }
                            | tv_client::playback::PlaybackUpdate::SessionExpired => {
                                let _ = activity_updates.send(None);
                            }
                            tv_client::playback::PlaybackUpdate::LibraryReady { .. }
                            | tv_client::playback::PlaybackUpdate::LibraryFailed { .. }
                            | tv_client::playback::PlaybackUpdate::Message { .. } => {}
                        }
                        ui_updates.apply_playback_update(update);
                    }
                    let _ = activity_updates.send(None);
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

#[cfg(feature = "native-ui")]
#[allow(clippy::too_many_arguments)]
fn start_media_thread(
    config: ClientConfig,
    state_directory: PathBuf,
    ui: tv_client::ui::UiHandle,
    youtube: tv_client::youtube::PlaybackConfig,
    youtube_commands: tokio::sync::mpsc::Receiver<tv_client::youtube::PlayerCommand>,
    routes: tokio::sync::mpsc::Receiver<RouteCommand>,
    searches: tokio::sync::mpsc::Receiver<SearchCommand>,
    jellyfin_commands: tokio::sync::mpsc::Sender<tv_client::playback::PlaybackCommand>,
    youtube_sender: tokio::sync::mpsc::Sender<tv_client::youtube::PlayerCommand>,
    activity: tokio::sync::watch::Sender<Option<tv_client::model::Source>>,
    mut shutdown: tv_client::state::ShutdownSignal,
) -> Option<std::thread::JoinHandle<()>> {
    let thread = std::thread::Builder::new()
        .name("tv-media".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "cannot start the media runtime");
                    return;
                }
            };
            runtime.block_on(async move {
                use std::sync::Arc;
                use std::time::Duration;

                use tv_client::state::shutdown_channel;
                use tv_client::youtube::{
                    Extractor, MpvBackendFactory, PLAYER_EVENT_CAPACITY, YtdlpExtractor, run_player,
                };

                let client = youtube_search_client(&config, &state_directory);
                let (search_shutdown, search_signal) = shutdown_channel();
                let (player_shutdown, player_signal) = shutdown_channel();
                let (router_shutdown, router_signal) = shutdown_channel();
                let (event_tx, event_rx) = tokio::sync::mpsc::channel(PLAYER_EVENT_CAPACITY);
                let extractor: Arc<dyn Extractor> = Arc::new(YtdlpExtractor::new(
                    youtube.extractor.clone(),
                    youtube.timeout,
                ));
                let factory = Arc::new(MpvBackendFactory::new(youtube.mpv.clone()));

                let mut player = tokio::spawn(run_player(
                    youtube,
                    extractor,
                    factory,
                    youtube_commands,
                    event_tx,
                    player_signal,
                ));
                let mut search = tokio::spawn(run_youtube_search(
                    client,
                    searches,
                    ui.clone(),
                    search_signal,
                ));
                let bridge =
                    tokio::spawn(run_youtube_bridge(event_rx, ui.clone(), activity.clone()));
                let mut router = tokio::spawn(run_router(
                    routes,
                    jellyfin_commands,
                    youtube_sender,
                    activity.subscribe(),
                    router_signal,
                ));

                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => {}
                    result = &mut player => {
                        if let Ok(Err(error)) = result {
                            tracing::warn!(code = error.code(), "youtube player stopped");
                        }
                    }
                    result = &mut search => {
                        if result.is_err() {
                            tracing::warn!("youtube search worker stopped");
                        }
                    }
                    result = &mut router => {
                        if result.is_err() {
                            tracing::warn!("media router stopped");
                        }
                    }
                }
                player_shutdown.cancel();
                search_shutdown.cancel();
                router_shutdown.cancel();
                if tokio::time::timeout(Duration::from_secs(5), &mut player)
                    .await
                    .is_err()
                {
                    player.abort();
                }
                if tokio::time::timeout(Duration::from_secs(5), &mut search)
                    .await
                    .is_err()
                {
                    search.abort();
                }
                if tokio::time::timeout(Duration::from_secs(5), &mut router)
                    .await
                    .is_err()
                {
                    router.abort();
                }
                bridge.abort();
            });
        });
    match thread {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "cannot spawn the media thread");
            None
        }
    }
}

#[cfg(feature = "native-ui")]
fn youtube_search_client(
    config: &ClientConfig,
    state_directory: &Path,
) -> Option<tv_client::youtube::SearchClient> {
    if !config.youtube_enabled {
        return None;
    }
    let server_url = config.server_url.as_deref()?;
    let storage =
        tv_client::storage::Storage::open(state_directory, config.artwork_cache_bytes).ok()?;
    let device = tv_client::cloud::load_device(&storage).ok()??;
    tv_client::youtube::SearchClient::new(server_url, device.token).ok()
}

#[cfg(feature = "native-ui")]
async fn run_youtube_search(
    client: Option<tv_client::youtube::SearchClient>,
    mut commands: tokio::sync::mpsc::Receiver<SearchCommand>,
    ui: tv_client::ui::UiHandle,
    mut shutdown: tv_client::state::ShutdownSignal,
) {
    use tv_client::jellyfin::cancellation;
    use tv_client::model::Source;
    use tv_client::ui::MediaItem;
    use tv_client::youtube::SearchRequest;

    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                if let Some(task) = current.take() {
                    task.abort();
                }
                return;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    if let Some(task) = current.take() {
                        task.abort();
                    }
                    return;
                };
                if let Some(task) = current.take() {
                    task.abort();
                }
                let SearchCommand::Search {
                    request_id,
                    query,
                    page_token,
                } = command
                else {
                    continue;
                };
                let Some(client) = client.clone() else {
                    ui.set_youtube_error(
                        request_id,
                        "YouTube search is not configured".to_string(),
                    );
                    continue;
                };
                let ui = ui.clone();
                current = Some(tokio::spawn(async move {
                    let request = match SearchRequest::new(&query, page_token.as_deref()) {
                        Ok(request) => request,
                        Err(error) => {
                            ui.set_youtube_error(request_id, error.to_string());
                            return;
                        }
                    };
                    ui.set_youtube_loading(request_id);
                    let (_cancel_handle, token) = cancellation();
                    match client.search(&request, &token).await {
                        Ok(page) => {
                            let items = page
                                .items
                                .into_iter()
                                .map(|result| {
                                    let channel = if result.channel_title.is_empty() {
                                        "YouTube".to_string()
                                    } else {
                                        result.channel_title
                                    };
                                    MediaItem::new(Source::YouTube, result.video_id, result.title)
                                        .with_subtitle(channel)
                                })
                                .collect();
                            ui.set_youtube_page(request_id, items, page.next_page_token);
                        }
                        Err(error) => ui.set_youtube_error(request_id, error.to_string()),
                    }
                }));
            }
        }
    }
}

#[cfg(feature = "native-ui")]
async fn run_youtube_bridge(
    mut events: tokio::sync::mpsc::Receiver<tv_client::youtube::PlayerEvent>,
    ui: tv_client::ui::UiHandle,
    activity: tokio::sync::watch::Sender<Option<tv_client::model::Source>>,
) {
    use tv_client::model::Source;
    use tv_client::youtube::PlayerEvent;

    while let Some(event) = events.recv().await {
        match event {
            PlayerEvent::Loading { video_id, title } => {
                ui.youtube_loading(video_id, title);
                let _ = activity.send(Some(Source::YouTube));
            }
            PlayerEvent::Playing {
                video_id,
                title,
                position_ms,
                duration_ms,
                method,
            } => {
                ui.youtube_playing(video_id, title, position_ms, duration_ms, method);
                let _ = activity.send(Some(Source::YouTube));
            }
            PlayerEvent::Progress {
                position_ms,
                duration_ms,
                paused,
                ..
            } => {
                ui.youtube_progress(position_ms, duration_ms, paused);
            }
            PlayerEvent::Paused { paused } => ui.youtube_paused(paused),
            PlayerEvent::Ended { .. } => {
                ui.youtube_ended();
                let _ = activity.send(None);
            }
            PlayerEvent::Stopped { .. } => {
                ui.youtube_stopped();
                let _ = activity.send(None);
            }
            PlayerEvent::Failed {
                code,
                message,
                retry_available,
                ..
            } => {
                ui.youtube_failed(code, message, retry_available);
                let _ = activity.send(None);
            }
        }
    }
    let _ = activity.send(None);
}

#[cfg(feature = "native-ui")]
async fn run_router(
    mut routes: tokio::sync::mpsc::Receiver<RouteCommand>,
    jellyfin: tokio::sync::mpsc::Sender<tv_client::playback::PlaybackCommand>,
    youtube: tokio::sync::mpsc::Sender<tv_client::youtube::PlayerCommand>,
    mut activity: tokio::sync::watch::Receiver<Option<tv_client::model::Source>>,
    mut shutdown: tv_client::state::ShutdownSignal,
) {
    use std::time::Duration;

    use tv_client::model::Source;
    use tv_client::playback::{PlayRequest, PlaybackCommand};
    use tv_client::youtube::{Ownership, PlayerCommand, VideoId};

    let mut ownership = Ownership::default();
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                let _ = youtube.send(PlayerCommand::Stop).await;
                let _ = jellyfin.send(PlaybackCommand::Stop).await;
                return;
            }
            changed = activity.changed() => {
                if changed.is_ok() {
                    let current = { *activity.borrow_and_update() };
                    ownership.observe(current);
                }
            }
            command = routes.recv() => {
                let Some(command) = command else {
                    let _ = youtube.send(PlayerCommand::Stop).await;
                    let _ = jellyfin.send(PlaybackCommand::Stop).await;
                    return;
                };
                match command {
                    RouteCommand::PlayJellyfin(item_id) => {
                        if ownership.active_is(Source::YouTube) {
                            let _ = youtube.send(PlayerCommand::Stop).await;
                            wait_for_source_gone(
                                &mut activity,
                                Source::YouTube,
                                Duration::from_secs(5),
                            )
                            .await;
                        }
                        ownership.starting(Source::Jellyfin, false);
                        let _ = jellyfin
                            .send(PlaybackCommand::Play(PlayRequest::new(item_id)))
                            .await;
                    }
                    RouteCommand::PlayYoutube { video_id, remote } => {
                        if ownership.active_is(Source::Jellyfin) {
                            let _ = jellyfin.send(PlaybackCommand::Stop).await;
                            wait_for_source_gone(
                                &mut activity,
                                Source::Jellyfin,
                                Duration::from_secs(5),
                            )
                            .await;
                        }
                        match VideoId::parse(&video_id) {
                            Ok(video_id) => {
                                ownership.starting(Source::YouTube, remote);
                                let _ = youtube
                                    .send(PlayerCommand::Play {
                                        video_id,
                                        title: None,
                                    })
                                    .await;
                            }
                            Err(_) => {
                                ownership.stopped();
                            }
                        }
                    }
                    RouteCommand::Pause(paused) => match ownership.active() {
                        Some(Source::YouTube) => {
                            let _ = youtube.send(PlayerCommand::Pause { paused }).await;
                        }
                        _ => {
                            let command = if paused {
                                PlaybackCommand::Pause
                            } else {
                                PlaybackCommand::Resume
                            };
                            let _ = jellyfin.send(command).await;
                        }
                    },
                    RouteCommand::Seek(position_ms) => match ownership.active() {
                        Some(Source::YouTube) => {
                            let _ = youtube.send(PlayerCommand::Seek { position_ms }).await;
                        }
                        _ => {
                            let _ = jellyfin.send(PlaybackCommand::Seek { position_ms }).await;
                        }
                    },
                    RouteCommand::Stop => {
                        ownership.stopped();
                        let _ = youtube.send(PlayerCommand::Stop).await;
                        let _ = jellyfin.send(PlaybackCommand::Stop).await;
                    }
                    RouteCommand::Retry => {
                        if ownership.retry_source() == Some(Source::YouTube) {
                            let _ = youtube.send(PlayerCommand::Retry).await;
                        } else {
                            let _ = jellyfin.send(PlaybackCommand::Retry).await;
                        }
                    }
                    RouteCommand::Jellyfin(command) => {
                        if !ownership.active_is(Source::YouTube) {
                            let _ = jellyfin.send(command).await;
                        }
                    }
                    RouteCommand::NetworkLost => {
                        if ownership.active_is(Source::YouTube) && ownership.remote() {
                            ownership.stopped();
                            let _ = youtube.send(PlayerCommand::Stop).await;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(feature = "native-ui")]
async fn wait_for_source_gone(
    activity: &mut tokio::sync::watch::Receiver<Option<tv_client::model::Source>>,
    source: tv_client::model::Source,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let current = { *activity.borrow_and_update() };
        if current != Some(source) {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        tokio::select! {
            _ = activity.changed() => {}
            _ = tokio::time::sleep(remaining) => return,
        }
    }
}

#[cfg(feature = "native-ui")]
fn join_media_thread(handle: Option<std::thread::JoinHandle<()>>) {
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
        tracing::warn!("media thread did not stop within 8 s");
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
    let state_directory = args
        .state_dir
        .or(parent_state_dir)
        .or_else(storage::default_state_dir);
    let selection = config::select_config(
        args.config.as_deref().or(parent_config.as_deref()),
        state_directory.as_deref(),
    );
    let config = load_selected_config(&selection)?;
    let server_url = config
        .server_url
        .clone()
        .ok_or(CliError::MissingServerUrl)?;
    let state_directory = state_directory.ok_or(CliError::MissingHome)?;
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

fn load_selected_config(selection: &config::ConfigSelection) -> Result<ClientConfig, CliError> {
    if selection.origin == config::ConfigOrigin::Explicit
        && let Some(path) = selection.path.as_deref()
        && !path.exists()
    {
        return Err(CliError::MissingConfig {
            path: path.to_path_buf(),
        });
    }
    Ok(selection.load()?)
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
