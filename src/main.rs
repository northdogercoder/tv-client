//! Command-line entry point for `tv-client`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use thiserror::Error;

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
    } = cli;
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
fn launch_native_ui(config: &ClientConfig, state_directory: &Path) -> Result<(), CliError> {
    use tokio::sync::mpsc;

    use tv_client::playback::PlaybackCommand;

    let (commands, command_rx) = mpsc::channel::<PlaybackCommand>(32);
    let (shutdown, signal) = tv_client::state::shutdown_channel();
    let ui = tv_client::ui::NativeUi::new(tv_client::AppState::default(), move |command| {
        if let Some(command) = controller_command(&command) {
            // The Slint callback never blocks: a full queue drops the command
            // rather than stalling the UI thread.
            let _ = commands.try_send(command);
        }
    })?;
    let handle = ui.handle();

    #[cfg(feature = "gstreamer-player")]
    start_playback_thread(
        config.clone(),
        state_directory.to_path_buf(),
        command_rx,
        signal,
        handle,
    );
    #[cfg(not(feature = "gstreamer-player"))]
    {
        // Keep the native-ui-only build warning-free: the playback
        // controller is compiled out, so these wiring values are unused.
        let _ = (config, state_directory, handle);
        drop(command_rx);
        drop(signal);
        tracing::warn!("native UI built without gstreamer-player; playback is disabled");
    }

    ui.run()?;
    shutdown.cancel();
    Ok(())
}

/// Translate a local UI command into a controller command.
#[cfg(feature = "native-ui")]
fn controller_command(
    command: &tv_client::ui::UiCommand,
) -> Option<tv_client::playback::PlaybackCommand> {
    use tv_client::model::{Action, Direction};
    use tv_client::playback::{PlayRequest, PlaybackCommand};
    use tv_client::ui::{UiCommand, UiPlaybackCommand};

    match command {
        UiCommand::Action(Action::PlayJellyfin { item_id }) => {
            Some(PlaybackCommand::Play(PlayRequest::new(item_id.clone())))
        }
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
        UiCommand::Action(_)
        | UiCommand::ApprovePairing { .. }
        | UiCommand::Retry
        | UiCommand::LoadLibrary { .. } => None,
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
) {
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
                        ui.apply_playback_update(playback_failure(
                            "jellyfin_unconfigured",
                            "Jellyfin is not configured",
                        ));
                        return;
                    }
                };
                let (cancel_handle, cancel) = tv_client::jellyfin::cancellation();
                match client.restore_session(&mut storage, &cancel).await {
                    Ok(true) => {}
                    Ok(false) => {
                        ui.apply_playback_update(playback_failure(
                            "jellyfin_sign_in",
                            "Jellyfin sign-in required",
                        ));
                        cancel_handle.cancel();
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Jellyfin session restore failed");
                        ui.apply_playback_update(playback_failure(
                            "jellyfin_unavailable",
                            "Jellyfin is unavailable",
                        ));
                        cancel_handle.cancel();
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
                        ui.apply_playback_update(playback_failure(
                            "player_unavailable",
                            "Native player is unavailable",
                        ));
                        cancel_handle.cancel();
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
                        ui.apply_playback_update(playback_failure(
                            "player_unavailable",
                            "Native player is unavailable",
                        ));
                        cancel_handle.cancel();
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
                        ui.apply_playback_update(playback_failure(
                            "controller_unavailable",
                            "Playback controller is unavailable",
                        ));
                        cancel_handle.cancel();
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
    if thread.is_err() {
        tracing::error!("cannot spawn the playback thread");
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
    #[cfg(feature = "native-ui")]
    #[error("native UI error: {0}")]
    Ui(#[from] tv_client::ui::NativeUiError),
}

impl CliError {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) | Self::MissingConfig { .. } => ExitCode::from(2),
            Self::Storage(_) | Self::MissingHome => ExitCode::FAILURE,
            #[cfg(feature = "native-ui")]
            Self::Ui(_) => ExitCode::FAILURE,
        }
    }
}
