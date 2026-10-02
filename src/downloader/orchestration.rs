//! Downloader orchestration: discovery, search, and download coordination.
//!
//! Implemented in Phase 8.
//!
//! Provides `DownloaderOrchestrator` which coordinates camera discovery,
//! cursor-based search windows, bounded concurrent camera searches,
//! download draining, metadata persistence, scheduled refresh, and
//! continuous polling.
//!
//! The Phase 7 `DownloadWorker` (in this module's `mod.rs`) handles
//! individual download claims; this orchestrator drives the higher-level
//! pipeline.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing;

use crate::configuration::{Config, NvrSearchConfig};
use crate::database::models::*;
use crate::database::repository::DatabaseOps;
use crate::domain::{CameraId, DownloadStatus, Timestamp};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::{
    CameraDiscoveryClient, ImageSearchClient, NvrTransport, configured_nvr_identity,
    generate_search_windows,
};
use crate::service_lifecycle::ShutdownToken;

use super::{DownloadPassReport, DownloadWorker};

// ── DownloaderOrchestratorOptions ──────────────────────────────────────────

/// Configuration for the downloader orchestrator.
#[derive(Debug, Clone)]
pub struct DownloaderOrchestratorOptions {
    /// Configured start-at timestamp for initial backfill.
    pub start_at: Timestamp,
    /// Search window size in minutes.
    pub window_minutes: u64,
    /// Duration between polling passes.
    pub poll_interval: Duration,
    /// Duration of overlap between consecutive polls.
    pub poll_overlap: Duration,
    /// Interval between camera discovery refreshes.
    pub camera_refresh_interval: Duration,
    /// Settlement delay subtracted from the current time.
    pub settlement_delay: Duration,
    /// Maximum concurrent camera searches.
    pub search_concurrency: usize,
    /// NVR identity string used for image-key computation.
    pub nvr_identity: String,
    /// Full search configuration (max_results, etc.) from the real config.
    pub search_config: NvrSearchConfig,
}

impl DownloaderOrchestratorOptions {
    /// Construct validated options from a full `Config`.
    ///
    /// Uses a conservative search concurrency of **2** regardless of the
    /// download concurrency setting.  Rejects zero search concurrency.
    pub fn from_config(config: &Config) -> AppResult<Self> {
        if config.nvr.search.poll_interval_seconds == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "DownloaderOrchestratorOptions::from_config",
                "poll_interval_seconds must be greater than zero",
            ));
        }

        let window_minutes = config.nvr.search.window_minutes;
        if window_minutes == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "DownloaderOrchestratorOptions::from_config",
                "window_minutes must be greater than zero",
            ));
        }

        let poll_interval = checked_std_duration(
            config.nvr.search.poll_interval_seconds,
            "poll_interval_seconds",
        )?;
        let poll_overlap = checked_std_duration(
            config.nvr.search.poll_overlap_seconds,
            "poll_overlap_seconds",
        )?;
        let camera_refresh_interval = checked_std_duration(
            config.nvr.search.camera_refresh_interval_seconds,
            "camera_refresh_interval_seconds",
        )?;
        let settlement_delay = checked_std_duration(
            config.nvr.search.settlement_delay_seconds,
            "settlement_delay_seconds",
        )?;

        // Conservative default search concurrency.
        let search_concurrency = 2usize;

        let nvr_identity =
            configured_nvr_identity(&config.nvr.scheme, &config.nvr.host, config.nvr.port);

        Ok(Self {
            start_at: config.nvr.start_at,
            window_minutes,
            poll_interval,
            poll_overlap,
            camera_refresh_interval,
            settlement_delay,
            search_concurrency,
            nvr_identity,
            search_config: config.nvr.search.clone(),
        })
    }

    /// Validate that search concurrency is greater than zero.
    pub fn validate(&self) -> AppResult<()> {
        if self.search_concurrency == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "DownloaderOrchestratorOptions::validate",
                "search_concurrency must be greater than zero",
            ));
        }
        Ok(())
    }
}

fn checked_std_duration(seconds: u64, field: &'static str) -> AppResult<Duration> {
    let duration = Duration::from_secs(seconds);
    chrono::TimeDelta::from_std(duration).map_err(|_| {
        AppError::new(
            ErrorCategory::Configuration,
            "DownloaderOrchestratorOptions::from_config",
            format!("{field} is too large for timestamp arithmetic"),
        )
    })?;
    Ok(duration)
}

fn is_expected_camera_failure(category: ErrorCategory) -> bool {
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

// ── CameraSearchFailure ────────────────────────────────────────────────────

/// Safe failure context for a single camera search.
#[derive(Debug, Clone)]
pub struct CameraSearchFailure {
    pub camera_id: CameraId,
    pub track_id: String,
    pub category: ErrorCategory,
    pub operation: &'static str,
}

// ── CameraSearchReport ─────────────────────────────────────────────────────

/// Summary of one camera's attempted search windows.
#[derive(Debug, Clone, Default)]
pub struct CameraSearchReport {
    pub camera_id: CameraId,
    pub channel_number: i64,
    pub windows_completed: u64,
    pub pages_fetched: u64,
    pub records_found: u64,
    pub records_skipped: u64,
    pub records_inserted: u64,
    pub reached_effective_end: bool,
    pub failure: Option<CameraSearchFailure>,
}

// ── DownloaderPassReport ───────────────────────────────────────────────────

/// Complete summary of one downloader pass (discovery + search + drain).
#[derive(Debug, Clone, Default)]
pub struct DownloaderPassReport {
    pub discovery_attempted: bool,
    pub cameras_discovered: u64,
    pub cameras_active: u64,
    pub camera_reports: Vec<CameraSearchReport>,
    pub search_failures: u64,
    pub windows_completed: u64,
    pub images_discovered: u64,
    pub download_pass: DownloadPassReport,
    pub backfill_completed: bool,
    pub discovery_failure: Option<CameraSearchFailure>,
    /// Durable status counts queried after draining downloads.
    pub status_counts: crate::database::models::StatusCounts,
}

// ── Settlement and start-time helpers ──────────────────────────────────────

/// Subtract `settlement_delay` from `now` to compute the effective end of
/// the search window.
///
/// Returns an `Internal` error on checked arithmetic underflow.
pub fn compute_effective_end(now: Timestamp, settlement_delay: Duration) -> AppResult<Timestamp> {
    let now_dt = *now.as_datetime();
    let td = chrono::TimeDelta::from_std(settlement_delay).map_err(|_| {
        AppError::new(
            ErrorCategory::Internal,
            "compute_effective_end",
            "settlement delay duration conversion overflow",
        )
    })?;
    now_dt
        .checked_sub_signed(td)
        .map(Timestamp::new)
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::Internal,
                "compute_effective_end",
                "settlement delay subtraction underflow",
            )
        })
}

/// Determine the search start for a camera based on its cursor state.
///
/// Rules:
/// * A camera without a cursor starts at `configured_start_at`.
/// * During unfinished backfill (no cursor), start at `configured_start_at`.
/// * During unfinished backfill (cursor exists), start exactly at
///   `next_search_at` (resume from cursor).
/// * During polling (backfill complete), start at
///   `max(start_at, next_search_at - poll_overlap)`.
///
/// Returns an error if checked arithmetic underflows.
pub fn compute_camera_search_start(
    configured_start_at: Timestamp,
    cursor: Option<&SearchCursorRecord>,
    initial_backfill_completed: bool,
    overlap: Duration,
) -> AppResult<Timestamp> {
    let cursor = cursor.as_ref();

    if !initial_backfill_completed {
        if let Some(c) = cursor
            && let Some(next) = c.next_search_at
        {
            return Ok(next);
        }
        return Ok(configured_start_at);
    }

    // Polling mode: apply overlap.
    if let Some(c) = cursor
        && let Some(next) = c.next_search_at
    {
        let next_dt = *next.as_datetime();
        let td = chrono::TimeDelta::from_std(overlap).map_err(|_| {
            AppError::new(
                ErrorCategory::Internal,
                "compute_camera_search_start",
                "overlap duration conversion overflow",
            )
        })?;
        let overlapped = next_dt.checked_sub_signed(td).ok_or_else(|| {
            AppError::new(
                ErrorCategory::Internal,
                "compute_camera_search_start",
                "poll overlap subtraction underflow",
            )
        })?;
        let overlapped_ts = Timestamp::new(overlapped);
        if overlapped_ts < configured_start_at {
            return Ok(configured_start_at);
        }
        return Ok(overlapped_ts);
    }

    Ok(configured_start_at)
}

// ── DownloaderOrchestrator ─────────────────────────────────────────────────

/// Coordinates discovery, search, download draining, metadata, and
/// continuous polling.
pub struct DownloaderOrchestrator {
    pub database: DatabaseOps,
    pub transport: Arc<NvrTransport>,
    pub download_worker: DownloadWorker,
    pub search_config: NvrSearchConfig,
    pub options: DownloaderOrchestratorOptions,
    pub active_cameras: Vec<CameraRecord>,
    pub last_successful_camera_refresh: Option<Timestamp>,
    pub housekeeping_completed: bool,
    /// Carries a scheduled-refresh discovery failure into the next
    /// pass report when `with_discovery` is false.
    pub last_discovery_failure: Option<CameraSearchFailure>,
}

impl DownloaderOrchestrator {
    /// Construct a new orchestrator from shared dependencies.
    ///
    /// Accepts a pre-built `DownloadWorker` so that all download policy
    /// values (concurrency, retries, limits, JPEG verification) come from
    /// the real configuration rather than a fabricated placeholder.
    ///
    /// Validates that `search_concurrency` is greater than zero; returns
    /// a `Configuration` error when it is not.
    pub fn new(
        database: DatabaseOps,
        transport: Arc<NvrTransport>,
        download_worker: DownloadWorker,
        options: DownloaderOrchestratorOptions,
    ) -> AppResult<Self> {
        if options.search_concurrency == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "DownloaderOrchestrator::new",
                "search_concurrency must be greater than zero",
            ));
        }

        Ok(Self {
            database,
            transport,
            download_worker,
            search_config: options.search_config.clone(),
            options,
            active_cameras: Vec::new(),
            last_successful_camera_refresh: None,
            housekeeping_completed: false,
            last_discovery_failure: None,
        })
    }

    /// Perform startup housekeeping.
    pub async fn startup_housekeeping(&mut self) -> AppResult<DownloadPassReport> {
        let report = self.download_worker.startup_housekeeping().await?;
        self.housekeeping_completed = true;
        Ok(report)
    }

    /// Persist the NVR identity metadata.
    pub async fn persist_nvr_identity(&self) -> AppResult<()> {
        let now = Timestamp::new(chrono::Utc::now());
        self.database
            .set_metadata(
                &ServiceMetadataKey::NvrIdentity,
                &self.options.nvr_identity,
                &now,
            )
            .await
    }

    /// Attempt camera discovery and synchronize the result.
    pub async fn attempt_discovery(&self) -> Result<Vec<CameraDiscovery>, AppError> {
        let client = CameraDiscoveryClient::new(&self.transport);
        let cameras = client.discover().await?;
        Ok(cameras)
    }

    /// Synchronize discovered cameras and reload active camera list.
    pub async fn sync_cameras(
        &mut self,
        cameras: &[CameraDiscovery],
    ) -> AppResult<Vec<CameraRecord>> {
        let now = Timestamp::new(chrono::Utc::now());
        let records = self.database.sync_cameras(cameras, &now).await?;
        self.database
            .set_metadata(
                &ServiceMetadataKey::LastSuccessfulCameraDiscovery,
                &now.to_string(),
                &now,
            )
            .await?;
        self.last_successful_camera_refresh = Some(now);
        self.active_cameras = records.clone();
        Ok(records)
    }

    /// Reload the active camera list from SQLite.
    pub async fn reload_active_cameras(&mut self) -> AppResult<Vec<CameraRecord>> {
        let cameras = self.database.list_active_cameras().await?;
        self.active_cameras = cameras.clone();
        Ok(cameras)
    }

    /// Check whether initial backfill is complete.
    pub async fn check_backfill_complete(&self) -> AppResult<bool> {
        let value = self
            .database
            .get_metadata(&ServiceMetadataKey::InitialBackfillCompleted)
            .await?;
        Ok(value.is_some_and(|v| v == "true"))
    }

    /// Mark initial backfill as complete.
    pub async fn mark_backfill_complete(&self) -> AppResult<()> {
        let now = Timestamp::new(chrono::Utc::now());
        self.database
            .set_metadata(&ServiceMetadataKey::InitialBackfillCompleted, "true", &now)
            .await
    }

    /// Mark last successful downloader poll.
    pub async fn mark_poll_complete(&self) -> AppResult<()> {
        let now = Timestamp::new(chrono::Utc::now());
        self.database
            .set_metadata(
                &ServiceMetadataKey::LastSuccessfulDownloaderPoll,
                &now.to_string(),
                &now,
            )
            .await
    }

    /// Execute one finite pass: discovery, search, and download drain.
    ///
    /// When `with_discovery` is true, camera discovery is performed at the
    /// start of the pass.  When false, the pass relies on the previously
    /// loaded active-camera list (used by the continuous loop where
    /// discovery is handled separately by the refresh schedule).
    pub async fn execute_one_pass(
        &mut self,
        with_discovery: bool,
    ) -> AppResult<DownloaderPassReport> {
        self.execute_one_pass_internal(with_discovery, None).await
    }

    /// Execute one pass with cooperative cancellation boundaries.
    pub async fn execute_one_pass_with_shutdown(
        &mut self,
        with_discovery: bool,
        shutdown: ShutdownToken,
    ) -> AppResult<DownloaderPassReport> {
        self.execute_one_pass_internal(with_discovery, Some(shutdown))
            .await
    }

    async fn execute_one_pass_internal(
        &mut self,
        with_discovery: bool,
        shutdown: Option<ShutdownToken>,
    ) -> AppResult<DownloaderPassReport> {
        if shutdown.as_ref().is_some_and(ShutdownToken::is_cancelled) {
            return Ok(DownloaderPassReport::default());
        }
        let now = chrono::Utc::now();
        let effective_end =
            compute_effective_end(Timestamp::new(now), self.options.settlement_delay)?;

        // Step 1: Discovery (only when requested).
        let mut discovery_failure: Option<CameraSearchFailure> = None;
        let mut cameras_discovered: u64 = 0;

        if with_discovery {
            if shutdown.as_ref().is_some_and(ShutdownToken::is_cancelled) {
                return Ok(DownloaderPassReport::default());
            }
            let discovery_result = self.attempt_discovery().await;
            match &discovery_result {
                Ok(cameras) => {
                    cameras_discovered = cameras.len() as u64;
                    tracing::info!(camera_count = cameras.len(), "Discovery completed");
                }
                Err(err) => {
                    discovery_failure = Some(CameraSearchFailure {
                        camera_id: CameraId::new(0),
                        track_id: String::new(),
                        category: err.category,
                        operation: "discovery",
                    });
                    tracing::warn!(
                        category = %err.category,
                        "Discovery failed; will use previously persisted cameras"
                    );
                }
            }

            // Step 2: Synchronize cameras (only if discovery succeeded).
            // Synchronization is a database operation — failures are fatal
            // because durable coordination may be compromised.
            if let Ok(cameras) = &discovery_result
                && let Err(err) = self.sync_cameras(cameras).await
            {
                return Err(AppError::with_source(
                    ErrorCategory::Database,
                    "execute_one_pass",
                    "camera synchronization failed",
                    err,
                ));
            }
        }

        // Step 3: Reload active cameras.
        let active_cameras = match self.reload_active_cameras().await {
            Ok(cameras) => cameras,
            Err(err) => {
                return Err(AppError::with_source(
                    ErrorCategory::Database,
                    "execute_one_pass",
                    "failed to reload active cameras",
                    err,
                ));
            }
        };

        let cameras_active = active_cameras.len() as u64;

        // Step 4: Check backfill status.
        let initial_backfill_completed = self.check_backfill_complete().await?;

        // Step 5: Search each active camera.
        let semaphore = Arc::new(Semaphore::new(self.options.search_concurrency));
        let mut search_tasks: Vec<(CameraId, i64, JoinHandle<AppResult<CameraSearchReport>>)> =
            Vec::new();

        for cam in &active_cameras {
            if shutdown.as_ref().is_some_and(ShutdownToken::is_cancelled) {
                break;
            }
            let cam_id = cam.id;
            let channel = cam.channel_number;
            let picture_track = cam.picture_track_id.clone();

            let db = self.database.clone();
            let transport = self.transport.clone();
            let search_config = self.search_config.clone();
            let nvr_identity = self.options.nvr_identity.clone();
            let start_at = self.options.start_at;
            let window_minutes = self.options.window_minutes;

            let overlap = self.options.poll_overlap;
            let search_semaphore = semaphore.clone();
            let task_shutdown = shutdown.clone();

            let handle = tokio::spawn(async move {
                // Every camera has a task; the permit bounds only active
                // search work and lets the pass drain all tasks on failure.
                let permit = search_semaphore.acquire_owned().await.map_err(|err| {
                    AppError::with_source(
                        ErrorCategory::Internal,
                        "camera_search_task",
                        "search semaphore closed",
                        anyhow::Error::from(err),
                    )
                })?;

                // Cursor fetch is a database operation — failures are fatal.
                let cursor = db.get_cursor(cam_id).await.map_err(|err| {
                    AppError::with_source(
                        ErrorCategory::Database,
                        "camera_search_task",
                        format!("get_cursor failed for camera_id={}", cam_id),
                        err,
                    )
                })?;

                let search_start = compute_camera_search_start(
                    start_at,
                    cursor.as_ref(),
                    initial_backfill_completed,
                    overlap,
                )?;

                let windows = generate_search_windows(search_start, effective_end, window_minutes)?;

                if windows.is_empty() {
                    drop(permit);
                    return Ok(CameraSearchReport {
                        camera_id: cam_id,
                        channel_number: channel,
                        reached_effective_end: true,
                        ..CameraSearchReport::default()
                    });
                }

                let mut report = CameraSearchReport {
                    camera_id: cam_id,
                    channel_number: channel,
                    ..CameraSearchReport::default()
                };

                for window in &windows {
                    if task_shutdown
                        .as_ref()
                        .is_some_and(ShutdownToken::is_cancelled)
                    {
                        break;
                    }
                    let client = ImageSearchClient {
                        transport: &transport,
                        database: db.clone(),
                        search_config: search_config.clone(),
                        nvr_identity: nvr_identity.clone(),
                        camera_id: cam_id,
                        picture_track: picture_track.clone(),
                    };

                    match client.search_one_window(window).await {
                        Ok(outcome) => {
                            report.windows_completed += 1;
                            report.pages_fetched += outcome.pages_fetched;
                            report.records_found += outcome.records_found;
                            report.records_skipped += outcome.records_skipped;
                            report.records_inserted += outcome.records_inserted;
                            tracing::info!(
                                camera_id = %cam_id,
                                window_start = %window.start,
                                window_end = %window.end,
                                inserted = outcome.records_inserted,
                                "Search window completed"
                            );
                        }
                        Err(err) => {
                            if is_expected_camera_failure(err.category) {
                                report.failure = Some(CameraSearchFailure {
                                    camera_id: cam_id,
                                    track_id: picture_track.clone(),
                                    category: err.category,
                                    operation: "search_one_window",
                                });
                                tracing::warn!(
                                    camera_id = %cam_id,
                                    category = %err.category,
                                    "Camera search failed, stopping this camera"
                                );
                                break;
                            }

                            // Database, filesystem, and internal failures
                            // compromise durable coordination and are fatal
                            // to this pass rather than isolated camera errors.
                            return Err(err);
                        }
                    }
                }

                if report.failure.is_none() && !windows.is_empty() {
                    report.reached_effective_end = true;
                }

                drop(permit);
                Ok(report)
            });

            search_tasks.push((cam_id, channel, handle));
        }

        // Join all camera search tasks without early returns.
        // Collect results and identify any fatal errors.
        let mut camera_reports: Vec<CameraSearchReport> = Vec::new();
        let mut search_failures: u64 = 0;
        let mut all_windows_completed: u64 = 0;
        let mut all_images_discovered: u64 = 0;
        let mut first_fatal_error: Option<AppError> = None;

        for (cam_id, _channel, handle) in search_tasks {
            let result = match handle.await {
                Ok(r) => r,
                Err(_err) => {
                    // Task join failure is a fatal internal error.
                    Err(AppError::with_source(
                        ErrorCategory::Internal,
                        "execute_one_pass",
                        format!("camera search task join failed: camera_id={}", cam_id),
                        anyhow::Error::from(_err),
                    ))
                }
            };

            match result {
                Ok(report) => {
                    if report.failure.is_some() {
                        search_failures += 1;
                    }
                    all_windows_completed += report.windows_completed;
                    all_images_discovered += report.records_inserted;
                    camera_reports.push(report);
                }
                Err(err) => {
                    // Classify fatal vs non-fatal errors.
                    // Database, Filesystem, and Internal errors are fatal
                    // coordination failures — the durable state may be
                    // compromised and the pass cannot reliably continue.
                    // Network, Timeout, Authentication, Authorization,
                    // Protocol, XmlParsing, InvalidNvrResponse, and
                    // PlaybackUnavailable are expected NVR-facing failures
                    // that should be recorded as CameraSearchFailure while
                    // allowing other cameras and downloads to complete.
                    if !is_expected_camera_failure(err.category) {
                        if first_fatal_error.is_none() {
                            let category = err.category;
                            let diagnostic_chain = err.safe_diagnostic_chain();
                            tracing::error!(
                                camera_id = %cam_id,
                                error_category = %err.category,
                                error_operation = err.operation,
                                error_http_status = ?err.http_status(),
                                error_message = %err.message,
                                error_chain = %diagnostic_chain,
                                "Fatal camera search task failure"
                            );
                            first_fatal_error = Some(AppError::with_source(
                                category,
                                "execute_one_pass",
                                format!("camera search task failed: camera_id={}", cam_id),
                                err,
                            ));
                        }
                    } else {
                        // Expected NVR-facing failure — record it as a
                        // CameraSearchFailure so the pass report captures it,
                        // but continue joining remaining tasks.
                        search_failures += 1;
                        camera_reports.push(CameraSearchReport {
                            camera_id: cam_id,
                            channel_number: 0,
                            failure: Some(CameraSearchFailure {
                                camera_id: cam_id,
                                track_id: String::new(),
                                category: err.category,
                                operation: "search_one_window",
                            }),
                            ..CameraSearchReport::default()
                        });
                    }
                }
            }
        }

        // Step 6: Drain downloads before any metadata writes so that
        // successful sibling work is never blocked by metadata errors.
        let download_pass = match match &shutdown {
            Some(token) => {
                self.download_worker
                    .run_until_idle_with_shutdown(token)
                    .await
            }
            None => self.download_worker.run_until_idle().await,
        } {
            Ok(report) => report,
            Err(e) => {
                // Download worker database errors are also fatal coordination
                // failures — durable state may be compromised.
                if first_fatal_error.is_none() {
                    let category = e.category;
                    first_fatal_error = Some(AppError::with_source(
                        category,
                        "execute_one_pass",
                        "download worker failed during drain",
                        e,
                    ));
                }
                DownloadPassReport::default()
            }
        };

        // Step 7: If there was a fatal error during search or drain, return
        // immediately without writing metadata.  Successful sibling work has
        // already been drained above.
        if let Some(fatal) = first_fatal_error {
            return Err(fatal);
        }

        if shutdown.as_ref().is_some_and(ShutdownToken::is_cancelled) {
            return Ok(DownloaderPassReport::default());
        }

        // A scheduled refresh failure is carried into this pass after the
        // refresh itself has already fallen back to persisted cameras.  It
        // remains visible and prevents success metadata for this iteration.
        let pass_discovery_failure = if !with_discovery {
            self.last_discovery_failure.take()
        } else {
            discovery_failure
        };

        // Step 8: If every active camera reached effective_end, mark
        // backfill complete.  A valid empty discovery is also complete;
        // cameras discovered later have no cursor and will still start at
        // the configured start_at even after this global marker is set.
        let backfill_completed = if !initial_backfill_completed
            && search_failures == 0
            && pass_discovery_failure.is_none()
            && camera_reports.iter().all(|r| r.reached_effective_end)
        {
            self.mark_backfill_complete().await?;
            true
        } else {
            initial_backfill_completed
        };

        // Step 9: Persist LastSuccessfulDownloaderPoll only when
        // orchestration completed without pass-level failures.
        if search_failures == 0 && pass_discovery_failure.is_none() {
            self.mark_poll_complete().await?;
        }

        // Step 10: Query durable status counts after draining.
        let status_counts = match self.database.status_counts().await {
            Ok(counts) => counts,
            Err(err) => {
                // Status counts are read after durable work and are part of
                // the pass coordination contract; a repository failure is
                // fatal rather than being reported as an empty summary.
                return Err(AppError::with_source(
                    ErrorCategory::Database,
                    "execute_one_pass",
                    "failed to query status counts",
                    err,
                ));
            }
        };

        // Step 11: Build pass report.
        let report = DownloaderPassReport {
            discovery_attempted: with_discovery,
            cameras_discovered,
            cameras_active,
            camera_reports,
            search_failures,
            windows_completed: all_windows_completed,
            images_discovered: all_images_discovered,
            download_pass,
            backfill_completed,
            discovery_failure: pass_discovery_failure,
            status_counts,
        };

        tracing::info!(
            cameras_active = report.cameras_active,
            windows_completed = report.windows_completed,
            images_discovered = report.images_discovered,
            downloads_downloaded = report.download_pass.downloaded,
            downloads_retry_wait = report
                .status_counts
                .download
                .get(&DownloadStatus::RetryWait)
                .copied()
                .unwrap_or(0),
            downloads_unavailable = report
                .status_counts
                .download
                .get(&DownloadStatus::Unavailable)
                .copied()
                .unwrap_or(0),
            downloads_failed = report
                .status_counts
                .download
                .get(&DownloadStatus::Failed)
                .copied()
                .unwrap_or(0),
            downloads_pending = report
                .status_counts
                .download
                .get(&DownloadStatus::Pending)
                .copied()
                .unwrap_or(0),
            backfill_completed = report.backfill_completed,
            "Downloader pass completed"
        );

        Ok(report)
    }

    /// Execute one continuous-mode iteration.
    ///
    /// A due discovery refresh runs before the search pass, so a successful
    /// refresh can add cameras to the same iteration. Refresh failures keep
    /// the persisted camera set and are reported by the pass without being
    /// treated as fatal coordination errors.
    pub async fn run_continuous_iteration(&mut self) -> AppResult<DownloaderPassReport> {
        self.run_continuous_iteration_with_shutdown(None).await
    }

    async fn run_continuous_iteration_with_shutdown(
        &mut self,
        shutdown: Option<ShutdownToken>,
    ) -> AppResult<DownloaderPassReport> {
        if shutdown.as_ref().is_some_and(ShutdownToken::is_cancelled) {
            return Ok(DownloaderPassReport::default());
        }
        let refresh_due = match &self.last_successful_camera_refresh {
            None => true,
            Some(last_refresh) => {
                let now = Timestamp::new(chrono::Utc::now());
                let last_dt = *last_refresh.as_datetime();
                let now_dt = *now.as_datetime();
                match now_dt.signed_duration_since(last_dt).to_std() {
                    Ok(dur) => dur >= self.options.camera_refresh_interval,
                    Err(_) => false,
                }
            }
        };

        if refresh_due {
            if shutdown.as_ref().is_some_and(ShutdownToken::is_cancelled) {
                return Ok(DownloaderPassReport::default());
            }
            match self.attempt_discovery().await {
                Ok(cameras) => {
                    self.sync_cameras(&cameras).await.map_err(|err| {
                        AppError::with_source(
                            ErrorCategory::Database,
                            "run_continuous_iteration",
                            "scheduled camera refresh synchronization failed",
                            err,
                        )
                    })?;
                    // A successful retry supersedes any recoverable startup
                    // or scheduled discovery failure carried by the prior
                    // iteration.
                    self.last_discovery_failure = None;
                    tracing::info!(
                        camera_count = cameras.len(),
                        "Scheduled camera refresh completed"
                    );
                }
                Err(err) => {
                    self.last_discovery_failure = Some(CameraSearchFailure {
                        camera_id: CameraId::new(0),
                        track_id: String::new(),
                        category: err.category,
                        operation: "discovery",
                    });
                    tracing::warn!(
                        category = %err.category,
                        "Scheduled camera refresh failed; proceeding with previously persisted cameras"
                    );
                }
            }
        }

        match shutdown {
            Some(token) => self.execute_one_pass_with_shutdown(false, token).await,
            None => self.execute_one_pass(false).await,
        }
    }

    /// Execute the continuous downloader loop.
    ///
    /// This method assumes that startup housekeeping and NVR identity
    /// persistence have already been performed by the caller (e.g.
    /// `handle_download`).
    pub async fn run_continuous(&mut self) -> AppResult<()> {
        self.run_continuous_with_shutdown(ShutdownToken::new())
            .await
    }

    /// Execute continuous polling while observing cooperative cancellation.
    pub async fn run_continuous_with_shutdown(&mut self, shutdown: ShutdownToken) -> AppResult<()> {
        crate::service_lifecycle::with_pipeline_heartbeat(
            self.database.clone(),
            ServiceMetadataKey::DownloaderHeartbeat,
            self.options.poll_interval,
            self.run_continuous_inner(shutdown),
        )
        .await
    }

    async fn run_continuous_inner(&mut self, shutdown: ShutdownToken) -> AppResult<()> {
        let active = self.reload_active_cameras().await?;
        tracing::info!(
            active_cameras = active.len(),
            "Active cameras loaded for continuous loop"
        );
        tracing::info!("Entering continuous downloader polling");

        loop {
            if shutdown.is_cancelled() {
                tracing::info!("Downloader loop terminated orderly");
                return Ok(());
            }
            self.run_continuous_iteration_with_shutdown(Some(shutdown.clone()))
                .await?;
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("Downloader loop terminated orderly");
                    return Ok(());
                }
                _ = tokio::time::sleep(self.options.poll_interval) => {}
            }
        }
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn ts(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> Timestamp {
        Timestamp::new(Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap())
    }

    #[test]
    fn effective_end_subtracts_settlement_delay() {
        let now = ts(2026, 7, 11, 12, 0, 0);
        let delay = Duration::from_secs(10);
        let end = compute_effective_end(now, delay).unwrap();
        assert_eq!(
            *end.as_datetime(),
            Utc.with_ymd_and_hms(2026, 7, 11, 11, 59, 50).unwrap()
        );
    }

    #[test]
    fn effective_end_zero_delay_returns_now() {
        let now = ts(2026, 7, 11, 12, 0, 0);
        let end = compute_effective_end(now, Duration::from_secs(0)).unwrap();
        assert_eq!(*end.as_datetime(), *now.as_datetime());
    }

    #[test]
    fn effective_end_underflow_returns_error() {
        let minimum = Timestamp::new(DateTime::<Utc>::MIN_UTC);
        let result = compute_effective_end(minimum, Duration::from_secs(1));
        let error = result.unwrap_err();
        assert_eq!(error.category, ErrorCategory::Internal);
        assert!(error.message.contains("underflow"));
    }

    #[test]
    fn no_cursor_during_backfill_returns_start_at() {
        let start_at = ts(2026, 7, 11, 0, 0, 0);
        let result =
            compute_camera_search_start(start_at, None, false, Duration::from_secs(120)).unwrap();
        assert_eq!(result, start_at);
    }

    #[test]
    fn cursor_during_backfill_returns_next_search_at() {
        let start_at = ts(2026, 7, 11, 0, 0, 0);
        let cursor = SearchCursorRecord {
            camera_id: CameraId::new(1),
            next_search_at: Some(ts(2026, 7, 11, 6, 0, 0)),
            last_completed_window_start: None,
            last_completed_window_end: None,
            last_poll_at: None,
            last_error: None,
            updated_at: ts(2026, 7, 11, 5, 0, 0),
        };
        let result =
            compute_camera_search_start(start_at, Some(&cursor), false, Duration::from_secs(120))
                .unwrap();
        assert_eq!(result, ts(2026, 7, 11, 6, 0, 0));
    }

    #[test]
    fn polling_overlap_underflow_returns_error() {
        let minimum = Timestamp::new(DateTime::<Utc>::MIN_UTC);
        let cursor = SearchCursorRecord {
            camera_id: CameraId::new(1),
            next_search_at: Some(minimum),
            last_completed_window_start: None,
            last_completed_window_end: None,
            last_poll_at: None,
            last_error: None,
            updated_at: minimum,
        };

        let result =
            compute_camera_search_start(minimum, Some(&cursor), true, Duration::from_secs(1));
        let error = result.unwrap_err();
        assert_eq!(error.category, ErrorCategory::Internal);
        assert!(error.message.contains("underflow"));
    }

    #[test]
    fn polling_with_overlap_clamps_to_start_at() {
        let start_at = ts(2026, 7, 11, 0, 0, 0);
        let cursor = SearchCursorRecord {
            camera_id: CameraId::new(1),
            next_search_at: Some(ts(2026, 7, 11, 0, 5, 0)),
            last_completed_window_start: None,
            last_completed_window_end: None,
            last_poll_at: None,
            last_error: None,
            updated_at: ts(2026, 7, 11, 0, 0, 0),
        };
        let result =
            compute_camera_search_start(start_at, Some(&cursor), true, Duration::from_secs(300))
                .unwrap();
        assert_eq!(result, start_at);
    }

    #[test]
    fn polling_with_overlap_applies_overlap() {
        let start_at = ts(2026, 7, 11, 0, 0, 0);
        let cursor = SearchCursorRecord {
            camera_id: CameraId::new(1),
            next_search_at: Some(ts(2026, 7, 11, 2, 0, 0)),
            last_completed_window_start: None,
            last_completed_window_end: None,
            last_poll_at: None,
            last_error: None,
            updated_at: ts(2026, 7, 11, 1, 0, 0),
        };
        let result =
            compute_camera_search_start(start_at, Some(&cursor), true, Duration::from_secs(300))
                .unwrap();
        assert_eq!(
            *result.as_datetime(),
            Utc.with_ymd_and_hms(2026, 7, 11, 1, 55, 0).unwrap()
        );
    }

    #[test]
    fn polling_no_cursor_returns_start_at() {
        let start_at = ts(2026, 7, 11, 0, 0, 0);
        let cursor = SearchCursorRecord {
            camera_id: CameraId::new(1),
            next_search_at: None,
            last_completed_window_start: None,
            last_completed_window_end: None,
            last_poll_at: None,
            last_error: None,
            updated_at: ts(2026, 7, 11, 0, 0, 0),
        };
        let result =
            compute_camera_search_start(start_at, Some(&cursor), true, Duration::from_secs(120))
                .unwrap();
        assert_eq!(result, start_at);
    }

    #[test]
    fn zero_overlap_returns_next_search_at() {
        let start_at = ts(2026, 7, 11, 0, 0, 0);
        let cursor = SearchCursorRecord {
            camera_id: CameraId::new(1),
            next_search_at: Some(ts(2026, 7, 11, 2, 0, 0)),
            last_completed_window_start: None,
            last_completed_window_end: None,
            last_poll_at: None,
            last_error: None,
            updated_at: ts(2026, 7, 11, 1, 0, 0),
        };
        let result =
            compute_camera_search_start(start_at, Some(&cursor), true, Duration::from_secs(0))
                .unwrap();
        assert_eq!(result, ts(2026, 7, 11, 2, 0, 0));
    }
}
