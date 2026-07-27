//! Command dispatch for Fauna Scan.
//!
//! Keeps the process entry point thin and centralizes command routing.

use std::path::Path;
use std::sync::Arc;

use crate::cli::Command;
use crate::configuration::Config;
use crate::database::Database;
use crate::database::models::ServiceMetadataKey;
use crate::domain::{DownloadStatus, ProcessingStatus};
use crate::downloader::orchestration::{
    CameraSearchFailure, DownloaderOrchestrator, DownloaderOrchestratorOptions,
};
use crate::downloader::{DownloadWorker, DownloadWorkerOptions};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::{CameraDiscoveryClient, ImageDownloadClient, NvrTransport};
use crate::scanner::{Scanner, ScannerOptions};
use crate::service_lifecycle::{
    ServiceLifecycleOptions, ShutdownToken, run_single_pipeline_until_signal, supervise_service,
    wait_for_shutdown_signal,
};

/// Execute the selected command.
///
/// `config_path` is the optional `--config` global option value.
pub async fn execute(command: Command, config_path: Option<&Path>) -> AppResult<()> {
    match command {
        Command::Run => handle_run(config_path)
            .await
            .map_err(|error| with_command_context("run", error)),
        Command::Web => handle_web(config_path)
            .await
            .map_err(|error| with_command_context("web", error)),
        Command::CheckConfig => handle_check_config(config_path),
        Command::Discover => handle_discover(config_path).await,
        Command::Download(args) => handle_download(args, config_path).await,
        Command::Scan(args) => handle_scan(args, config_path).await,
        Command::Status => handle_status(config_path)
            .await
            .map_err(|error| with_command_context("status", error)),
    }
}

/// Serve the dashboard against durable state without running worker pipelines.
async fn handle_web(config_path: Option<&Path>) -> AppResult<()> {
    let config = Config::load(config_path)?;
    create_runtime_directories(&config)?;
    let database = Database::open(&config.general.database_path).await?;
    let transport = Arc::new(NvrTransport::from_config(&config.nvr)?);
    let state = crate::web::WebState::from_config(database.ops(), &config, transport);
    let shutdown = ShutdownToken::new();
    let web_shutdown = shutdown.clone();
    run_single_pipeline_until_signal(
        "web",
        crate::web::serve(state, web_shutdown),
        shutdown,
        wait_for_shutdown_signal(),
        ServiceLifecycleOptions::default().shutdown_timeout,
    )
    .await
}

/// Handle the `check-config` command.
///
/// Loads, resolves, and validates the configuration without contacting
/// external services or creating directories. Shared classifier policy and
/// endpoint-specific lease relationships are validated during `Config::load`.
/// An empty endpoint list is valid here because classification is optional for
/// commands that do not run the scanner.
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
        // Continuous polling loop with bounded signal shutdown.
        let shutdown = ShutdownToken::new();
        let pipeline_shutdown = shutdown.clone();
        run_single_pipeline_until_signal(
            "downloader",
            async move {
                orchestrator
                    .run_continuous_with_shutdown(pipeline_shutdown)
                    .await
            },
            shutdown,
            wait_for_shutdown_signal(),
            ServiceLifecycleOptions::default().shutdown_timeout,
        )
        .await
    }
}

/// Run the complete service with downloader and scanner under one supervisor.
async fn handle_run(config_path: Option<&Path>) -> AppResult<()> {
    let config = Config::load(config_path)?;
    if !config
        .classifier
        .endpoints
        .iter()
        .any(|endpoint| endpoint.enabled)
    {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "run",
            "no classifier endpoints are configured or enabled; run requires downloader and scanner pipelines",
        ));
    }
    let scanner_options = ScannerOptions::from_config(&config)?;

    // Startup order: directories, database/migrations, lease recovery and
    // housekeeping, clients, initial discovery, then primary tasks.
    create_runtime_directories(&config)?;
    let database = Database::open(&config.general.database_path).await?;
    let ops = database.ops();
    let recovery = ops
        .recover_expired_leases(&crate::domain::Timestamp::new(chrono::Utc::now()))
        .await?;
    let stale_parts_removed =
        crate::filesystem::remove_stale_part_files(&config.general.output_directory).await?;
    let transport = Arc::new(NvrTransport::from_config(&config.nvr)?);
    let web_state = config
        .web
        .enabled
        .then(|| crate::web::WebState::from_config(ops.clone(), &config, transport.clone()));
    let download_client = Arc::new(ImageDownloadClient::from_config(
        transport.clone(),
        &config.nvr,
    ));
    let download_options = DownloadWorkerOptions::from_config(&config)?;
    let download_worker = DownloadWorker::new(ops.clone(), download_client, download_options);
    let orchestrator_options = DownloaderOrchestratorOptions::from_config(&config)?;
    orchestrator_options.validate()?;
    let mut orchestrator = DownloaderOrchestrator::new(
        ops.clone(),
        transport,
        download_worker,
        orchestrator_options,
    )?;

    let now = crate::domain::Timestamp::new(chrono::Utc::now());
    ops.set_metadata(
        &ServiceMetadataKey::ApplicationVersion,
        env!("CARGO_PKG_VERSION"),
        &now,
    )
    .await?;
    orchestrator.persist_nvr_identity().await?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        database = %config.general.database_path.display(),
        output_directory = %config.general.output_directory.display(),
        "Fauna Scan service starting"
    );
    tracing::info!(
        stale_parts_removed,
        download_leases_recovered = recovery.downloads,
        processing_leases_recovered = recovery.processing,
        "Startup housekeeping completed"
    );
    let effective_end = crate::downloader::orchestration::compute_effective_end(
        now,
        std::time::Duration::from_secs(config.nvr.search.settlement_delay_seconds),
    )?;
    tracing::info!(
        historical_start = %config.nvr.start_at,
        historical_end = %effective_end,
        "Historical backfill range"
    );

    // HTTP clients are fully constructed before discovery, but no primary
    // pipeline is spawned until discovery and synchronization finish.
    let classifiers = classifier_clients(&config.classifier)?;
    let rate_limits = config
        .classifier
        .endpoints
        .iter()
        .filter(|endpoint| endpoint.enabled)
        .map(|endpoint| endpoint.rate_limit.clone())
        .collect();
    let scanner = Scanner::with_classifiers_and_rate_limits(
        ops.clone(),
        classifiers,
        rate_limits,
        scanner_options,
    );

    // Discovery is deliberately completed before either task is spawned.
    match orchestrator.attempt_discovery().await {
        Ok(cameras) => {
            let records = orchestrator.sync_cameras(&cameras).await?;
            tracing::info!(
                camera_count = records.len(),
                "Initial camera discovery completed"
            );
        }
        Err(error) if is_recoverable_nvr_error(error.category) => {
            tracing::warn!(category = %error.category, "Initial camera discovery failed; using persisted active cameras");
            // Keep the failed attempt visible to the first continuous pass.
            // In particular, do not update last_successful_camera_refresh:
            // doing so would suppress the retry for the whole refresh interval.
            orchestrator.last_discovery_failure = Some(CameraSearchFailure {
                camera_id: crate::domain::CameraId::new(0),
                track_id: String::new(),
                category: error.category,
                operation: "discovery",
            });
            let cameras = orchestrator.reload_active_cameras().await?;
            tracing::info!(
                camera_count = cameras.len(),
                "Loaded persisted active cameras"
            );
        }
        Err(error) => return Err(error),
    }

    let cameras = orchestrator.active_cameras.len();
    tracing::info!(
        camera_count = cameras,
        "Starting downloader and scanner pipelines"
    );
    let shutdown = ShutdownToken::new();
    let downloader_shutdown = shutdown.clone();
    let web_shutdown = shutdown.clone();
    let downloader = tokio::spawn(async move {
        if let Some(web_state) = web_state {
            tokio::try_join!(
                orchestrator.run_continuous_with_shutdown(downloader_shutdown),
                crate::web::serve(web_state, web_shutdown),
            )
            .map(|_| ())
        } else {
            orchestrator
                .run_continuous_with_shutdown(downloader_shutdown)
                .await
        }
    });
    let scanner_shutdown = shutdown.clone();
    let scanner_task =
        tokio::spawn(async move { scanner.run_continuous_with_shutdown(scanner_shutdown).await });

    supervise_service(
        downloader,
        scanner_task,
        shutdown,
        ops,
        ServiceLifecycleOptions::default(),
        wait_for_shutdown_signal(),
    )
    .await
}

/// Print grouped database state in a stable, exhaustive order.
async fn handle_status(config_path: Option<&Path>) -> AppResult<()> {
    let config = Config::load(config_path)?;
    let database = Database::open(&config.general.database_path).await?;
    let counts = database.ops().status_counts().await?;

    println!("Download status:");
    for status in [
        DownloadStatus::Pending,
        DownloadStatus::Downloading,
        DownloadStatus::Downloaded,
        DownloadStatus::RetryWait,
        DownloadStatus::Unavailable,
        DownloadStatus::Failed,
    ] {
        println!(
            "{}: {}",
            status,
            counts.download.get(&status).copied().unwrap_or(0)
        );
    }
    println!("Processing status:");
    for status in [
        ProcessingStatus::New,
        ProcessingStatus::Processing,
        ProcessingStatus::Done,
        ProcessingStatus::RetryWait,
        ProcessingStatus::Failed,
        ProcessingStatus::Missing,
    ] {
        println!(
            "{}: {}",
            status,
            counts.processing.get(&status).copied().unwrap_or(0)
        );
    }
    Ok(())
}

fn with_command_context(operation: &'static str, error: AppError) -> AppError {
    AppError::with_source(error.category, operation, error.message.clone(), error)
}

/// Build one client for each enabled classifier endpoint.
fn classifier_clients(
    config: &crate::configuration::ClassifierConfig,
) -> AppResult<Vec<Arc<crate::classifier::ClassifierClient>>> {
    let mut clients = Vec::with_capacity(config.endpoints.len());
    for endpoint in config.endpoints.iter().filter(|endpoint| endpoint.enabled) {
        clients.push(Arc::new(
            crate::classifier::ClassifierClient::from_endpoint_config(endpoint)?,
        ));
    }
    Ok(clients)
}

fn is_recoverable_nvr_error(category: ErrorCategory) -> bool {
    matches!(
        category,
        ErrorCategory::Authentication
            | ErrorCategory::Authorization
            | ErrorCategory::Network
            | ErrorCategory::Timeout
            | ErrorCategory::Protocol
            | ErrorCategory::XmlParsing
            | ErrorCategory::InvalidNvrResponse
            | ErrorCategory::PlaybackUnavailable
    )
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
/// Loads configuration, requires at least one enabled classifier endpoint **before**
/// creating directories or opening the database, then constructs
/// ClassifierClient and ScannerOptions, and either executes one finite
/// pass (`--once`) or enters continuous polling.
async fn handle_scan(args: crate::cli::ScanArgs, config_path: Option<&Path>) -> AppResult<()> {
    // Load and validate configuration.
    let config = Config::load(config_path)?;

    // Require an enabled endpoint immediately, before any filesystem mutation or
    // database open, so the configuration error is not masked by downstream
    // failures and an endpoint-less invocation has no side effects.
    ScannerOptions::from_config(&config)?;

    // Create runtime directories (database parent and output directory).
    create_runtime_directories(&config)?;

    // Open the database (applies migrations).
    let database = Database::open(&config.general.database_path).await?;

    // Build one client per endpoint.
    let classifiers = classifier_clients(&config.classifier)?;

    // Build scanner options from shared pool policy.
    let options = ScannerOptions::from_config(&config)?;

    // Build one concurrent worker per enabled classifier endpoint.
    let rate_limits = config
        .classifier
        .endpoints
        .iter()
        .filter(|endpoint| endpoint.enabled)
        .map(|endpoint| endpoint.rate_limit.clone())
        .collect();
    let scanner = Scanner::with_classifiers_and_rate_limits(
        database.ops(),
        classifiers,
        rate_limits,
        options,
    );

    if args.once {
        // One finite pass: drain all currently eligible images.
        let report = scanner.execute_one_pass().await?;
        println!("{report}");
        Ok(())
    } else {
        // Continuous scanning loop with bounded signal shutdown.
        let shutdown = ShutdownToken::new();
        let pipeline_shutdown = shutdown.clone();
        run_single_pipeline_until_signal(
            "scanner",
            async move {
                scanner
                    .run_continuous_with_shutdown(pipeline_shutdown)
                    .await
            },
            shutdown,
            wait_for_shutdown_signal(),
            ServiceLifecycleOptions::default().shutdown_timeout,
        )
        .await
    }
}

/// Strip potentially sensitive details from an io::Error message.
fn safe_io_message(e: &std::io::Error) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::classifier_clients;
    use crate::configuration::{
        ClassifierConfig, ClassifierEndpointConfig, ClassifierGenerationConfig,
    };
    use url::Url;

    fn endpoint(enabled: bool, base_url: &str) -> ClassifierEndpointConfig {
        ClassifierEndpointConfig {
            enabled,
            rate_limit: None,
            base_url: Url::parse(base_url).unwrap(),
            endpoint: "/chat/completions".to_string(),
            model: "test-model".to_string(),
            api_key: None,
            username: String::new(),
            password: None,
            request_timeout_seconds: 10,
            prompt_version: "wildlife-v1".to_string(),
            generation: ClassifierGenerationConfig {
                temperature: 0.1,
                max_tokens: 1000,
            },
        }
    }

    #[test]
    fn classifier_clients_exclude_disabled_endpoints() {
        let config = ClassifierConfig {
            endpoints: vec![
                endpoint(false, "ftp://disabled.invalid/v1"),
                endpoint(true, "http://enabled.invalid/v1"),
            ],
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 600,
        };

        let clients = classifier_clients(&config).expect("enabled endpoint should build");
        assert_eq!(clients.len(), 1);
        assert_eq!(
            clients[0].endpoint_url().host_str(),
            Some("enabled.invalid")
        );
    }
}
