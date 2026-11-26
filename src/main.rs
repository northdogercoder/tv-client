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
    Ok(())
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
}

impl CliError {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) | Self::MissingConfig { .. } => ExitCode::from(2),
            Self::Storage(_) | Self::MissingHome => ExitCode::FAILURE,
        }
    }
}
