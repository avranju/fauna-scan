//! Command dispatch for Fauna Scan.
//!
//! Keeps the process entry point thin and centralizes command routing.

use std::path::Path;
use std::sync::Arc;

use crate::cli::Command;
use crate::configuration::Config;
use crate::database::Database;
use crate::database::models::ServiceMetadataKey;
use crate::domain::DownloadStatus;
use crate::downloader::orchestration::{DownloaderOrchestrator, DownloaderOrchestratorOptions};
use crate::downloader::{DownloadWorker, DownloadWorkerOptions};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::{CameraDiscoveryClient, ImageDownloadClient, NvrTransport};
use crate::scanner::{Scanner, ScannerOptions};

/// Execute the selected command.
///
/// `config_path` is the optional `--config` global option value.
pub async fn execute(command: Command, config_path: Option<&Path>) -> AppResult<()> {
    match command {
        Command::Run => Err(AppError::not_implemented("run")),
        Command::CheckConfig => handle_check_config(config_path),
        Command::Discover => handle_discover(config_path).await,
        Command::Download(args) => handle_download(args, config_path).await,
        Command::Scan(args) => handle_scan(args, config_path).await,
        Command::Status => Err(AppError::not_implemented("status")),
    }
}

/// Handle the `check-config` command.
///
/// Loads, resolves, and validates the configuration without contacting
/// external services or creating directories.  Scanner-specific policy
/// (disabled classifier, lease relationships, maximum duration) is
/// validated during `Config::load` via `validate_config` for shared
/// classifier settings, and in `ScannerOptions::from_config` for
/// scanner-only checks.  This allows `check-config` to accept a
/// disabled classifier (valid for other commands) while still
/// rejecting invalid lease configurations.
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

/// Handle the `download` subcommand.
///
/// Loads configuration, creates runtime directories, opens/migrates
/// SQLite, builds the shared NVR transport and download client,
/// constructs the worker and orchestrator, runs startup housekeeping,
/// then either executes one finite pass (`--once`) or enters the
/// continuous polling loop.
async fn handle_download(
    args: crate::cli::DownloadArgs,
    config_path: Option<&Path>,
) -> AppResult<()> {
    // Load and validate configuration using the caller-supplied path.
    let config = Config::load(config_path)?;

    // Create runtime directories (database parent and output directory)
    create_runtime_directories(&config)?;

    // Open the database (applies migrations)
    let database = Database::open(&config.general.database_path).await?;

    // Build the NVR transport with Digest authentication
    let transport = NvrTransport::from_config(&config.nvr)?;

    // Build the image download client (wraps transport in Arc internally)
    let download_client = Arc::new(ImageDownloadClient::from_config(
        Arc::new(transport),
        &config.nvr,
    ));

    // Build download worker options from the real config
    let worker_options = DownloadWorkerOptions::from_config(&config)?;

    // Build download worker
    let download_worker = DownloadWorker::new(
        database.ops().clone(),
        download_client.clone(),
        worker_options,
    );

    // Build orchestrator options from the real config
    let orchestrator_options = DownloaderOrchestratorOptions::from_config(&config)?;

    // Validate search concurrency before starting work
    orchestrator_options.validate()?;

    // Build orchestrator (construction is now fallible — validates concurrency).
    let mut orchestrator = DownloaderOrchestrator::new(
        database.ops().clone(),
        download_client.transport.clone(),
        download_worker,
        orchestrator_options,
    )?;

    // Run startup housekeeping
    let housekeeping_report = orchestrator.startup_housekeeping().await?;
    tracing::info!(
        stale_parts_removed = housekeeping_report.stale_parts_removed,
        leases_recovered = housekeeping_report.leases_recovered,
        "Startup housekeeping completed"
    );

    // Persist NVR identity metadata — fatal because durable identity
    // tracking may be compromised if the write fails.
    orchestrator.persist_nvr_identity().await?;

    if args.once {
        // One finite pass: discovery + search + download drain
        let report = orchestrator.execute_one_pass(true).await?;

        // Print compact summary with real durable status counts.
        let pending = report
            .status_counts
            .download
            .get(&DownloadStatus::Pending)
            .copied()
            .unwrap_or(0);
        let retry_wait = report
            .status_counts
            .download
            .get(&DownloadStatus::RetryWait)
            .copied()
            .unwrap_or(0);
        let unavailable = report
            .status_counts
            .download
            .get(&DownloadStatus::Unavailable)
            .copied()
            .unwrap_or(0);
        let failed = report
            .status_counts
            .download
            .get(&DownloadStatus::Failed)
            .copied()
            .unwrap_or(0);

        println!(
            "Downloader pass: {} active camera(s), {} window(s) completed, \
             {} image(s) discovered, {} downloaded, {} pending, {} retry-wait, \
             {} unavailable, {} failed",
            report.cameras_active,
            report.windows_completed,
            report.images_discovered,
            report.download_pass.downloaded,
            pending,
            retry_wait,
            unavailable,
            failed,
        );

        // Return non-zero error if discovery or camera searches failed
        // (after draining work, which has already happened).
        if report.search_failures > 0 || report.discovery_failure.is_some() {
            // Prefer a camera failure when one is available so the command's
            // structured category and message identify the affected camera
            // and track without exposing request details or response bodies.
            let first_search_failure = report
                .camera_reports
                .iter()
                .find_map(|camera| camera.failure.as_ref());
            let first_failure = first_search_failure.or(report.discovery_failure.as_ref());

            if let Some(failure) = first_failure {
                let track = if failure.track_id.is_empty() {
                    "none"
                } else {
                    failure.track_id.as_str()
                };
                return Err(AppError::new(
                    failure.category,
                    "download_once",
                    format!(
                        "pass completed with {} search failure(s), discovery_failure={}; first failure camera_id={} track_id={} operation={} category={}",
                        report.search_failures,
                        report.discovery_failure.is_some(),
                        failure.camera_id,
                        track,
                        failure.operation,
                        failure.category,
                    ),
                ));
            }
        }

        Ok(())
    } else {
        // Continuous polling loop
        orchestrator.run_continuous().await
    }
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

/// Handle the `scan` subcommand.
///
/// Loads configuration, rejects a disabled classifier **before** creating
/// directories or opening the database (so the classifier-disabled error
/// is always the first and clearest failure), then constructs
/// ClassifierClient and ScannerOptions, and either executes one finite
/// pass (`--once`) or enters continuous polling.
async fn handle_scan(args: crate::cli::ScanArgs, config_path: Option<&Path>) -> AppResult<()> {
    // Load and validate configuration.
    let config = Config::load(config_path)?;

    // Reject a disabled classifier immediately — before any filesystem
    // mutation or database open.  This guarantees the classifier-disabled
    // error is never masked by downstream errors and that a disabled
    // invocation has no side effects.
    ScannerOptions::from_config(&config)?;

    // Create runtime directories (database parent and output directory).
    create_runtime_directories(&config)?;

    // Open the database (applies migrations).
    let database = Database::open(&config.general.database_path).await?;

    // Build the classifier client — also rejects disabled classifier.
    let classifier = crate::classifier::ClassifierClient::from_config(&config.classifier)?;

    // Build scanner options from config (also rejects disabled classifier).
    let options = ScannerOptions::from_config(&config)?;

    // Build scanner.
    let scanner = Scanner::new(database.ops(), Arc::new(classifier), options);

    if args.once {
        // One finite pass: drain all currently eligible images.
        let report = scanner.execute_one_pass().await?;
        println!("{report}");
        Ok(())
    } else {
        // Continuous scanning loop.
        scanner.run_continuous().await
    }
}

/// Strip potentially sensitive details from an io::Error message.
fn safe_io_message(e: &std::io::Error) -> String {
    e.to_string()
}
