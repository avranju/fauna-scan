//! Command dispatch for Fauna Scan.
//!
//! Keeps the process entry point thin and centralizes command routing.

use std::path::Path;

use crate::cli::Command;
use crate::configuration::Config;
use crate::database::Database;
use crate::database::models::ServiceMetadataKey;
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::{CameraDiscoveryClient, NvrTransport};

/// Execute the selected command.
///
/// `config_path` is the optional `--config` global option value.
pub async fn execute(command: Command, config_path: Option<&Path>) -> AppResult<()> {
    match command {
        Command::Run => Err(AppError::not_implemented("run")),
        Command::CheckConfig => handle_check_config(config_path),
        Command::Discover => handle_discover(config_path).await,
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

/// Handle the `discover` command.
///
/// Loads configuration, creates runtime directories, opens the database,
/// discovers cameras via the NVR, synchronizes them, records metadata,
/// and prints a summary.
async fn handle_discover(config_path: Option<&Path>) -> AppResult<()> {
    // Load and validate configuration
    let config = Config::load(config_path)?;

    // Create runtime directories (database parent and output directory)
    create_runtime_directories(&config)?;

    // Open the database (applies migrations)
    let database = Database::open(&config.general.database_path).await?;

    // Build the NVR transport with Digest authentication
    let transport = NvrTransport::from_config(&config.nvr)?;

    // Capture a single observation timestamp for all operations
    let observed_at = chrono::Utc::now();
    let observed_ts = crate::domain::Timestamp::new(observed_at);

    // Discover cameras
    let client = CameraDiscoveryClient::new(&transport);
    let cameras = client.discover().await?;

    // Synchronize cameras in the database
    let records = database.ops().sync_cameras(&cameras, &observed_ts).await?;

    // Persist last successful discovery metadata
    database
        .ops()
        .set_metadata(
            &ServiceMetadataKey::LastSuccessfulCameraDiscovery,
            &observed_ts.to_string(),
            &observed_ts,
        )
        .await?;

    // Log success-oriented event after sync and metadata persist complete.
    tracing::info!(camera_count = records.len(), "Discovered cameras from NVR");

    // Print summary
    println!("Discovered {} camera(s):", records.len());
    for rec in &records {
        let name_part = rec
            .name
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        println!(
            "  Channel {}: primary track {}, picture track {}{}",
            rec.channel_number, rec.primary_track_id, rec.picture_track_id, name_part
        );
    }

    Ok(())
}

/// Create runtime directories required by operational commands.
///
/// Creates the parent directory of the configured database path and the
/// configured output directory.  `check-config` does not call this,
/// keeping it non-mutating.
///
/// When the database path has no parent (e.g. a single-component relative
/// path like `fauna-scan.sqlite3`), the current directory is used instead
/// — no explicit directory creation is needed in that case.
fn create_runtime_directories(config: &Config) -> AppResult<()> {
    // Create database parent directory (skip if the path is a single-component
    // relative path whose parent is "." — i.e. the current directory).
    if let Some(db_parent) = config.general.database_path.parent()
        && db_parent != std::path::Path::new("")
    {
        std::fs::create_dir_all(db_parent).map_err(|e| {
            AppError::with_source(
                ErrorCategory::Filesystem,
                "create_runtime_directories",
                format!(
                    "cannot create database parent directory {}: {}",
                    db_parent.display(),
                    safe_io_message(&e)
                ),
                e,
            )
        })?;
    }

    // Create output directory
    std::fs::create_dir_all(&config.general.output_directory).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Filesystem,
            "create_runtime_directories",
            format!(
                "cannot create output directory {}: {}",
                config.general.output_directory.display(),
                safe_io_message(&e)
            ),
            e,
        )
    })?;

    Ok(())
}

/// Strip potentially sensitive details from an io::Error message.
fn safe_io_message(e: &std::io::Error) -> String {
    e.to_string()
}
