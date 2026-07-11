//! Command dispatch for Fauna Scan.
//!
//! Keeps the process entry point thin and centralizes command routing.

use std::path::Path;

use crate::cli::Command;
use crate::configuration::Config;
use crate::error::{AppError, AppResult};

/// Execute the selected command.
///
/// `config_path` is the optional `--config` global option value.
/// Only `check-config` loads configuration in Phase 2; all other commands
/// continue returning not-yet-implemented errors.
pub async fn execute(command: Command, config_path: Option<&Path>) -> AppResult<()> {
    match command {
        Command::Run => Err(AppError::not_implemented("run")),
        Command::CheckConfig => handle_check_config(config_path),
        Command::Discover => Err(AppError::not_implemented("discover")),
        Command::Download(_) => Err(AppError::not_implemented("download")),
        Command::Scan(_) => Err(AppError::not_implemented("scan")),
        Command::Status => Err(AppError::not_implemented("status")),
    }
}

/// Handle the `check-config` command.
///
/// Loads, resolves, and validates the configuration without contacting
/// external services or creating directories.
fn handle_check_config(config_path: Option<&Path>) -> AppResult<()> {
    let config = Config::load(config_path)?;

    tracing::info!(
        config_path = %config.source_path.display(),
        "Configuration is valid"
    );
    println!("Configuration valid: {}", config.source_path.display());

    Ok(())
}
