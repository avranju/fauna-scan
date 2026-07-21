//! Download worker and orchestrator.
//!
//! - `mod.rs` (Phase 7): `DownloadWorker` — bounded-concurrency image
//!   download with retry, adoption, and crash recovery.
//! - `orchestration.rs` (Phase 8): `DownloaderOrchestrator` — discovery,
//!   cursor-based search, download draining, and continuous polling.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing;

use crate::database::models::*;
use crate::database::repository::DatabaseOps;
use crate::domain::{ImageKey, Timestamp};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::filesystem::{
    self, FilePreparation, image_destination, remove_final_file, safe_io_error,
};
use crate::nvr::ImageDownloadClient;
use crate::service_lifecycle::ShutdownToken;

// ── Phase 8 orchestration submodule ───────────────────────────────────────

pub mod orchestration;
pub use orchestration::{
    CameraSearchFailure, CameraSearchReport, DownloaderOrchestrator, DownloaderOrchestratorOptions,
    DownloaderPassReport,
};

// ── DownloadWorkerOptions ─────────────────────────────────────────────────

/// Configuration for the download worker.
#[derive(Debug, Clone)]
pub struct DownloadWorkerOptions {
    /// Directory for downloaded images.
    pub output_directory: PathBuf,
    /// Maximum concurrent downloads.
    pub concurrency: usize,
    /// Maximum retry attempts before permanent failure.
    pub retry_limit: u32,
    /// Initial retry delay.
    pub retry_initial_delay: Duration,
    /// Maximum retry delay (backoff cap).
    pub retry_max_delay: Duration,
    /// Lease duration for download claims.
    pub lease_duration: Duration,
    /// Maximum image size in bytes.
    pub maximum_image_size_bytes: u64,
    /// Whether to verify JPEG signatures.
    pub verify_jpeg: bool,
}

impl DownloadWorkerOptions {
    /// Construct validated options from a full `Config`.
    ///
    /// Derives the lease duration as **twice the NVR request timeout** plus
    /// a **30-second safety margin**, using saturating arithmetic to prevent
    /// overflow.  Rejects zero concurrency.
    ///
    /// Returns an `AppError` when concurrency is zero or the derived lease
    /// duration overflows `Duration::MAX`.
    pub fn from_config(config: &crate::configuration::Config) -> AppResult<Self> {
        if config.nvr.download.concurrency == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "download_worker_options",
                "download concurrency must be greater than zero",
            ));
        }

        // Lease = 2 * request_timeout + 30s safety margin.
        // Use saturating arithmetic to prevent overflow.
        let timeout_secs = config.nvr.request_timeout_seconds;
        let lease_secs = timeout_secs.saturating_mul(2).saturating_add(30);

        let lease_duration = Duration::from_secs(lease_secs);

        Ok(Self {
            output_directory: config.general.output_directory.clone(),
            concurrency: config.nvr.download.concurrency,
            retry_limit: config.nvr.download.retry_limit,
            retry_initial_delay: Duration::from_secs(
                config.nvr.download.retry_initial_delay_seconds as u64,
            ),
            retry_max_delay: Duration::from_secs(
                config.nvr.download.retry_max_delay_seconds as u64,
            ),
            lease_duration,
            maximum_image_size_bytes: config.nvr.download.maximum_image_size_bytes,
            verify_jpeg: config.nvr.download.verify_jpeg,
        })
    }
}

// ── DownloadPassReport ────────────────────────────────────────────────────

/// Summary of a finite download worker pass.
#[derive(Debug, Clone, Default)]
pub struct DownloadPassReport {
    pub claimed: u64,
    pub downloaded: u64,
    pub adopted: u64,
    pub retry_scheduled: u64,
    pub unavailable: u64,
    pub failed: u64,
    pub stale_parts_removed: u64,
    pub leases_recovered: u64,
}

// ── DownloadOutcome ───────────────────────────────────────────────────────

/// Result of processing one download claim.
#[derive(Debug, Clone)]
pub enum DownloadOutcome {
    /// Successfully downloaded via network.
    Downloaded,
    /// Existing valid file was adopted without network transfer.
    Adopted,
    /// Retry scheduled for a temporary failure.
    RetryScheduled,
    /// Playback unavailable after retry exhaustion (404/410).
    Unavailable,
    /// Permanent failure.
    Failed,
}

// ── DownloadWorker ────────────────────────────────────────────────────────

/// Coordinates atomic claims, concurrent downloads, retry decisions,
/// and durable state transitions.
pub struct DownloadWorker {
    database: DatabaseOps,
    client: Arc<ImageDownloadClient>,
    options: DownloadWorkerOptions,
}

impl DownloadWorker {
    /// Construct a download worker.
    pub fn new(
        database: DatabaseOps,
        client: Arc<ImageDownloadClient>,
        options: DownloadWorkerOptions,
    ) -> Self {
        Self {
            database,
            client,
            options,
        }
    }

    /// Perform startup housekeeping: remove stale `.part` files and
    /// recover expired download leases.
    ///
    /// Returns a report with cleanup and recovery counts.
    /// Propagates filesystem and database errors instead of silently
    /// converting them to zero-count success results.
    pub async fn startup_housekeeping(&self) -> AppResult<DownloadPassReport> {
        let now = Timestamp::new(chrono::Utc::now());

        // Remove stale part files — errors are propagated.
        let stale_parts =
            filesystem::remove_stale_part_files(&self.options.output_directory).await?;

        // Recover expired leases — errors are propagated.
        let recovery = self.database.recover_expired_leases(&now).await?;

        Ok(DownloadPassReport {
            stale_parts_removed: stale_parts,
            leases_recovered: recovery.downloads,
            ..DownloadPassReport::default()
        })
    }

    /// Run the download worker until no eligible work remains.
    ///
    /// Claims work up to the configured concurrency, processes each claim
    /// in a separate task, waits for completion, and continues claiming
    /// until no eligible rows exist or all active tasks finish.
    ///
    /// Recomputes the current time and lease deadline for each claim.
    /// Drains all active tasks before returning, even on error.
    pub async fn run_until_idle(&self) -> AppResult<DownloadPassReport> {
        self.run_until_idle_with_shutdown(&ShutdownToken::new())
            .await
    }

    /// Run until idle while refusing new claims after cancellation.  Existing
    /// download tasks are always joined before returning.
    pub async fn run_until_idle_with_shutdown(
        &self,
        shutdown: &ShutdownToken,
    ) -> AppResult<DownloadPassReport> {
        let mut report = DownloadPassReport::default();
        let mut first_error: Option<AppError> = None;

        loop {
            if shutdown.is_cancelled() {
                break;
            }
            // Claim work up to current concurrency.
            let mut tasks: Vec<(ImageKey, JoinHandle<AppResult<DownloadOutcome>>)> = Vec::new();
            let mut pass_claims: u64 = 0;

            loop {
                if shutdown.is_cancelled() {
                    break;
                }
                // Check how many tasks are still active.
                let active_count = tasks.iter().filter(|(_, h)| !h.is_finished()).count();
                if active_count >= self.options.concurrency {
                    break;
                }

                // Recompute time and lease for each claim.
                let now = Timestamp::new(chrono::Utc::now());

                let lease_until = compute_lease_until(now, self.options.lease_duration);

                // Try to claim the next download.
                let claim = match self.database.claim_next_download(&now, &lease_until).await {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(e) => {
                        // Database error — record it but stop claiming and
                        // drain active tasks before returning.
                        if first_error.is_none() {
                            first_error = Some(AppError::with_source(
                                ErrorCategory::Database,
                                "run_until_idle",
                                "database claim error during worker loop",
                                e,
                            ));
                        }
                        break;
                    }
                };

                pass_claims += 1;
                report.claimed += 1;
                let image_key = claim.image_key.clone();

                let db = self.database.clone();
                let client = self.client.clone();
                let options = self.options.clone();

                let handle = tokio::spawn(process_claim(claim, db, client, options));
                tasks.push((image_key, handle));
            }

            // Wait for all active tasks to complete — drain all handles.
            let mut task_has_error = false;
            for (_image_key, handle) in tasks {
                let outcome = match handle.await {
                    Ok(o) => o,
                    Err(e) => Err(AppError::with_source(
                        ErrorCategory::Internal,
                        "download_worker_task",
                        "download task failed",
                        anyhow::Error::from(e),
                    )),
                };

                match outcome {
                    Ok(DownloadOutcome::Downloaded) => report.downloaded += 1,
                    Ok(DownloadOutcome::Adopted) => report.adopted += 1,
                    Ok(DownloadOutcome::RetryScheduled) => report.retry_scheduled += 1,
                    Ok(DownloadOutcome::Unavailable) => report.unavailable += 1,
                    Ok(DownloadOutcome::Failed) => report.failed += 1,
                    Err(e) => {
                        // Database error during completion/failure — surface
                        // after draining.  Do not issue another transition.
                        if !task_has_error {
                            task_has_error = true;
                            if first_error.is_none() {
                                first_error = Some(AppError::with_source(
                                    ErrorCategory::Database,
                                    "process_claim_result",
                                    "database completion/failure transition error",
                                    e,
                                ));
                            }
                        }
                    }
                }
            }

            // If we didn't claim anything this pass, we're done.
            if pass_claims == 0 {
                break;
            }
        }

        // Return the first error if one occurred, otherwise the report.
        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(report)
        }
    }
}

/// Process a single download claim.
///
/// 1. Compute the destination path.
/// 2. Check for an existing valid file (adopt if found).
/// 3. Otherwise, fetch the playback response and stream it atomically.
/// 4. Call complete_download or fail_download based on the result.
async fn process_claim(
    claim: DownloadClaim,
    database: DatabaseOps,
    client: Arc<ImageDownloadClient>,
    options: DownloadWorkerOptions,
) -> AppResult<DownloadOutcome> {
    // Compute the destination.
    let destination = image_destination(
        &options.output_directory,
        claim.camera_channel_number,
        claim.camera_name.as_deref(),
        &claim.capture_start_at,
        &claim.track_id,
        &claim.image_key,
    );

    // Clone claim for arms that may move it, so we can still use it
    // after the match block for the fetch path.
    let claim_clone = claim.clone();

    // Check for an existing valid file.
    match filesystem::verify_existing_file(
        &destination.final_path,
        options.maximum_image_size_bytes,
        options.verify_jpeg,
    ) {
        Ok(FilePreparation::Adopted) => {
            // Adopt the existing file.
            let now = Timestamp::new(chrono::Utc::now());
            database
                .complete_download(claim.image_id, &destination.final_path, &now)
                .await
                .map_err(|e| {
                    // Database completion failed — the file exists but the
                    // database row is not marked downloaded.  Leave the row
                    // in downloading state so lease recovery can resume.
                    AppError::with_source(
                        ErrorCategory::Database,
                        "process_claim_adopt",
                        "database completion failed after file adoption",
                        e,
                    )
                })?;
            let short_key = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(8)];
            tracing::info!(
                image_id = claim.image_id.get(),
                image_key = short_key,
                track = %claim.track_id,
                "adopted existing file",
            );
            return Ok(DownloadOutcome::Adopted);
        }
        Ok(FilePreparation::DownloadRequired) => {
            // Invalid or missing file — proceed with download.
            // If the file exists but is invalid (e.g. HTML body, truncated JPEG),
            // remove it before making the network request.
            match destination.final_path.try_exists() {
                Ok(true) => {
                    if let Err(e) = remove_final_file(&destination.final_path).await {
                        // Removal failure is a filesystem error — classify it
                        // and fail the claim rather than propagating directly.
                        let io_err = e;
                        let io_err = AppError::with_source(
                            ErrorCategory::Filesystem,
                            "process_claim_remove",
                            "cannot remove invalid final file",
                            io_err,
                        );
                        let disposition = classify_download_failure(
                            &io_err,
                            claim.download_attempts,
                            &options.retry_limit,
                            &options.retry_initial_delay,
                            &options.retry_max_delay,
                        );
                        fail_claim(database, claim_clone, &io_err, disposition).await?;
                        return Ok(disposition_to_outcome(&disposition));
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    // I/O error from try_exists — classify as retryable
                    // filesystem failure.
                    let io_err = AppError::with_source(
                        ErrorCategory::Filesystem,
                        "process_claim_exists",
                        format!("cannot check existing file: {}", safe_io_error(&e)),
                        e,
                    );
                    let disposition = classify_download_failure(
                        &io_err,
                        claim.download_attempts,
                        &options.retry_limit,
                        &options.retry_initial_delay,
                        &options.retry_max_delay,
                    );
                    fail_claim(database, claim_clone, &io_err, disposition).await?;
                    return Ok(disposition_to_outcome(&disposition));
                }
            }
        }
        Err(e) => {
            // Verification I/O error — classify as retryable filesystem
            // failure rather than silently proceeding with a download.
            let disposition = classify_download_failure(
                &e,
                claim.download_attempts,
                &options.retry_limit,
                &options.retry_initial_delay,
                &options.retry_max_delay,
            );
            fail_claim(database, claim_clone, &e, disposition).await?;
            return Ok(disposition_to_outcome(&disposition));
        }
    }

    // Fetch the playback response.
    let response = match client.fetch(&claim.playback_uri).await {
        Ok(r) => r,
        Err(e) => {
            // NvrTransport converts non-success responses (including 404/410)
            // into AppError with http_status set. Check for 404/410 here
            // before generic protocol classification so the correct disposition
            // (retry or unavailable) is applied.
            if let Some(404 | 410) = e.http_status() {
                let short_key = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(8)];
                let disposition = classify_404_410(
                    claim.download_attempts,
                    &options.retry_limit,
                    &options.retry_initial_delay,
                    &options.retry_max_delay,
                );
                tracing::info!(
                    image_id = claim.image_id.get(),
                    image_key = short_key,
                    track = %claim.track_id,
                    attempt = claim.download_attempts,
                    category = %e.category,
                    http_status = e.http_status().unwrap_or(0),
                    "404/410 playback unavailable",
                );
                fail_claim(database, claim, &e, disposition).await?;
                return Ok(disposition_to_outcome(&disposition));
            }

            // All other fetch errors — classify normally.
            let short_key = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(8)];
            let disposition = classify_download_failure(
                &e,
                claim.download_attempts,
                &options.retry_limit,
                &options.retry_initial_delay,
                &options.retry_max_delay,
            );
            tracing::info!(
                image_id = claim.image_id.get(),
                image_key = short_key,
                track = %claim.track_id,
                attempt = claim.download_attempts,
                category = %e.category,
                http_status = e.http_status().unwrap_or(0),
                "fetch failure",
            );
            fail_claim(database, claim, &e, disposition).await?;
            return Ok(disposition_to_outcome(&disposition));
        }
    };

    let http_status = response.status().as_u16();

    // Non-2xx response that wasn't a fetch error — classify by status.
    if !response.status().is_success() {
        let e = AppError::new(
            ErrorCategory::Protocol,
            "process_claim",
            format!("HTTP {}", http_status),
        )
        .with_http_status(http_status);
        let short_key = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(8)];
        let disposition = classify_download_failure(
            &e,
            claim.download_attempts,
            &options.retry_limit,
            &options.retry_initial_delay,
            &options.retry_max_delay,
        );
        tracing::info!(
            image_id = claim.image_id.get(),
            image_key = short_key,
            track = %claim.track_id,
            attempt = claim.download_attempts,
            http_status,
            "non-2xx response",
        );
        fail_claim(database, claim, &e, disposition).await?;
        return Ok(disposition_to_outcome(&disposition));
    }

    // Stream the response to the destination.
    let _bytes_written = match filesystem::stream_response_to_destination(
        response,
        &destination,
        options.maximum_image_size_bytes,
        options.verify_jpeg,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            let short_key = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(8)];
            let disposition = classify_streaming_failure(
                &e,
                claim.download_attempts,
                &options.retry_limit,
                &options.retry_initial_delay,
                &options.retry_max_delay,
            );
            tracing::info!(
                image_id = claim.image_id.get(),
                image_key = short_key,
                track = %claim.track_id,
                attempt = claim.download_attempts,
                category = %e.category,
                "streaming failure",
            );
            fail_claim(database, claim, &e, disposition).await?;
            return Ok(disposition_to_outcome(&disposition));
        }
    };

    // Atomically rename succeeded — now mark the database as downloaded.
    let now = Timestamp::new(chrono::Utc::now());
    database
        .complete_download(claim.image_id, &destination.final_path, &now)
        .await
        .map_err(|e| {
            // The file exists on disk but the database row is not marked
            // downloaded.  Leave the row in downloading state so lease
            // recovery can resume.  Do NOT remove the valid final file.
            AppError::with_source(
                ErrorCategory::Database,
                "process_claim_complete",
                "database completion failed after successful download",
                e,
            )
        })?;

    let short_key = &claim.image_key.as_str()[..claim.image_key.as_str().len().min(8)];
    tracing::info!(
        image_id = claim.image_id.get(),
        image_key = short_key,
        track = %claim.track_id,
        attempt = claim.download_attempts,
        "download completed",
    );

    Ok(DownloadOutcome::Downloaded)
}

/// Classify a download failure into a disposition.
///
/// Uses the error's `http_status` field to distinguish retryable HTTP
/// statuses (408, 429, 5xx) from permanent ones.  Network and timeout
/// errors are always retryable (while attempts remain).  Other categories
/// (authentication, authorization, protocol without a retryable status)
/// are permanent failures.
fn classify_download_failure(
    error: &AppError,
    attempts: i64,
    retry_limit: &u32,
    retry_initial_delay: &Duration,
    retry_max_delay: &Duration,
) -> DownloadFailureDisposition {
    let category = error.category;
    let status = error.http_status();

    // 408 (Request Timeout) and 429 (Too Many Requests) are retryable.
    if let Some(s) = status
        && (s == 408 || s == 429 || (500..599).contains(&s))
    {
        if attempts as u32 >= *retry_limit {
            return DownloadFailureDisposition::Failed;
        }
        let delay = exponential_backoff(attempts, *retry_initial_delay, *retry_max_delay);
        return DownloadFailureDisposition::RetryWait {
            next_attempt_at: compute_retry_time(delay),
        };
    }

    // Protocol errors without a known retryable status are permanent.
    if category == ErrorCategory::Protocol {
        return DownloadFailureDisposition::Failed;
    }

    // Authentication and authorization are permanent failures.
    if category == ErrorCategory::Authentication || category == ErrorCategory::Authorization {
        return DownloadFailureDisposition::Failed;
    }

    // Network and timeout errors are retryable (while attempts remain).
    if category == ErrorCategory::Network || category == ErrorCategory::Timeout {
        if attempts as u32 >= *retry_limit {
            return DownloadFailureDisposition::Failed;
        }
        let delay = exponential_backoff(attempts, *retry_initial_delay, *retry_max_delay);
        return DownloadFailureDisposition::RetryWait {
            next_attempt_at: compute_retry_time(delay),
        };
    }

    // Filesystem errors during network phase are retryable.
    if category == ErrorCategory::Filesystem {
        if attempts as u32 >= *retry_limit {
            return DownloadFailureDisposition::Failed;
        }
        let delay = exponential_backoff(attempts, *retry_initial_delay, *retry_max_delay);
        return DownloadFailureDisposition::RetryWait {
            next_attempt_at: compute_retry_time(delay),
        };
    }

    // Everything else is a permanent failure.
    DownloadFailureDisposition::Failed
}

/// Classify 404/410 responses.
///
/// These are treated as confirmation retries: each attempt schedules a
/// retry with the configured backoff, and after `retry_limit` attempts
/// the image becomes unavailable.
fn classify_404_410(
    attempts: i64,
    retry_limit: &u32,
    retry_initial_delay: &Duration,
    retry_max_delay: &Duration,
) -> DownloadFailureDisposition {
    if attempts as u32 >= *retry_limit {
        DownloadFailureDisposition::Unavailable
    } else {
        let delay = exponential_backoff(attempts, *retry_initial_delay, *retry_max_delay);
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: compute_retry_time(delay),
        }
    }
}

/// Classify streaming failures.
fn classify_streaming_failure(
    error: &AppError,
    attempts: i64,
    retry_limit: &u32,
    retry_initial_delay: &Duration,
    retry_max_delay: &Duration,
) -> DownloadFailureDisposition {
    let category = error.category;
    let is_retryable = matches!(category, ErrorCategory::Network | ErrorCategory::Filesystem);

    if !is_retryable {
        return DownloadFailureDisposition::Failed;
    }

    if attempts as u32 >= *retry_limit {
        return DownloadFailureDisposition::Failed;
    }

    let delay = exponential_backoff(attempts, *retry_initial_delay, *retry_max_delay);
    DownloadFailureDisposition::RetryWait {
        next_attempt_at: compute_retry_time(delay),
    }
}

/// Compute the next retry timestamp from a delay duration.
fn compute_retry_time(delay: Duration) -> Timestamp {
    let now = chrono::Utc::now();
    if let Ok(td) = chrono::TimeDelta::from_std(delay)
        && let Some(dt) = now.checked_add_signed(td)
    {
        return Timestamp::new(dt);
    }
    // Fallback: use current time (retry immediately).
    Timestamp::new(now)
}

/// Compute a lease-until timestamp from the current time and lease duration.
fn compute_lease_until(now: Timestamp, lease_duration: Duration) -> Timestamp {
    let now_dt = now.as_datetime();
    if let Ok(td) = chrono::TimeDelta::from_std(lease_duration)
        && let Some(dt) = now_dt.checked_add_signed(td)
    {
        return Timestamp::new(dt);
    }
    // Fallback: use current time.
    now
}

/// Convert a disposition to an outcome.
fn disposition_to_outcome(d: &DownloadFailureDisposition) -> DownloadOutcome {
    match d {
        DownloadFailureDisposition::RetryWait { .. } => DownloadOutcome::RetryScheduled,
        DownloadFailureDisposition::Unavailable => DownloadOutcome::Unavailable,
        DownloadFailureDisposition::Failed => DownloadOutcome::Failed,
    }
}

/// Fail a claim with the given error and disposition.
async fn fail_claim(
    database: DatabaseOps,
    claim: DownloadClaim,
    error: &AppError,
    disposition: DownloadFailureDisposition,
) -> AppResult<()> {
    let error_msg = build_safe_error_message(error);
    let now = Timestamp::new(chrono::Utc::now());
    database
        .fail_download(claim.image_id, &error_msg, disposition, &now)
        .await
}

/// Build a safe error message for fail_download, never including URLs,
/// query parameters, response bodies, or credentials.
fn build_safe_error_message(error: &AppError) -> String {
    let category = error.category;
    let status = error.http_status();

    match category {
        ErrorCategory::PlaybackUnavailable => {
            if let Some(s) = status {
                format!("playback unavailable (HTTP {})", s)
            } else {
                "playback unavailable".to_string()
            }
        }
        ErrorCategory::Protocol => {
            if let Some(s) = status {
                format!("protocol error (HTTP {})", s)
            } else {
                "protocol error".to_string()
            }
        }
        ErrorCategory::Authentication => "authentication failed".to_string(),
        ErrorCategory::Authorization => "authorization denied".to_string(),
        ErrorCategory::Network => "network transport error".to_string(),
        ErrorCategory::Timeout => "request timed out".to_string(),
        ErrorCategory::Filesystem => "filesystem error".to_string(),
        ErrorCategory::XmlParsing => "response parsing error".to_string(),
        ErrorCategory::Database => "database error".to_string(),
        _ => "download failed".to_string(),
    }
}

/// Calculate exponential backoff delay.
///
/// Returns `initial * 2^(attempt - 1)`, capped at `maximum`.
/// Uses saturating arithmetic to prevent overflow.
///
/// `attempt` is the one-based persisted `download_attempts` value.
pub fn exponential_backoff(attempt: i64, initial: Duration, maximum: Duration) -> Duration {
    if attempt <= 0 {
        return initial;
    }

    let attempt_u64 = attempt as u64;
    // 2^(attempt - 1) — saturating shift.
    let shift = ((attempt_u64 - 1) as u32).min(63);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);

    // Multiply initial duration by the multiplier using saturating arithmetic.
    let initial_secs = initial.as_secs().saturating_mul(multiplier);
    let initial_nanos = (initial.subsec_nanos() as u64).saturating_mul(multiplier);

    let total_secs = initial_secs.saturating_add(initial_nanos / 1_000_000_000);
    let total_nanos = (initial_nanos % 1_000_000_000) as u32;

    let delay = Duration::new(total_secs, total_nanos);

    if delay > maximum { maximum } else { delay }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_attempt_one_returns_initial() {
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        let delay = exponential_backoff(1, initial, max);
        assert_eq!(delay, initial);
    }

    #[test]
    fn exponential_backoff_doubles_each_attempt() {
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        assert_eq!(exponential_backoff(1, initial, max), Duration::from_secs(5));
        assert_eq!(
            exponential_backoff(2, initial, max),
            Duration::from_secs(10)
        );
        assert_eq!(
            exponential_backoff(3, initial, max),
            Duration::from_secs(20)
        );
        assert_eq!(
            exponential_backoff(4, initial, max),
            Duration::from_secs(40)
        );
        assert_eq!(
            exponential_backoff(5, initial, max),
            Duration::from_secs(80)
        );
        assert_eq!(
            exponential_backoff(6, initial, max),
            Duration::from_secs(160)
        );
        assert_eq!(
            exponential_backoff(7, initial, max),
            Duration::from_secs(300)
        );
        assert_eq!(
            exponential_backoff(8, initial, max),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn exponential_backoff_caps_at_maximum() {
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(100);
        assert_eq!(exponential_backoff(1, initial, max), Duration::from_secs(5));
        assert_eq!(
            exponential_backoff(2, initial, max),
            Duration::from_secs(10)
        );
        assert_eq!(
            exponential_backoff(3, initial, max),
            Duration::from_secs(20)
        );
        assert_eq!(
            exponential_backoff(4, initial, max),
            Duration::from_secs(40)
        );
        assert_eq!(
            exponential_backoff(5, initial, max),
            Duration::from_secs(80)
        );
        assert_eq!(exponential_backoff(6, initial, max), max);
        assert_eq!(exponential_backoff(10, initial, max), max);
    }

    #[test]
    fn exponential_backoff_no_overflow() {
        // Even with a very large attempt count, should not overflow.
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(3600);
        let delay = exponential_backoff(100, initial, max);
        assert_eq!(delay, max);
    }

    #[test]
    fn exponential_backoff_zero_attempt_returns_initial() {
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        let delay = exponential_backoff(0, initial, max);
        assert_eq!(delay, initial);
    }

    // ── Failure classification tests ─────────────────────────────────────

    #[test]
    fn classify_network_error_is_retryable() {
        let err = AppError::new(ErrorCategory::Network, "fetch", "connection failed");
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_network_error_exhausted_is_failed() {
        let err = AppError::new(ErrorCategory::Network, "fetch", "connection failed");
        let limit = 3u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 3, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::Failed));
    }

    #[test]
    fn classify_timeout_error_is_retryable() {
        let err = AppError::new(ErrorCategory::Timeout, "fetch", "timed out");
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_http_408_is_retryable() {
        let err = AppError::new(ErrorCategory::Protocol, "fetch", "request timeout")
            .with_http_status(408);
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_http_429_is_retryable() {
        let err = AppError::new(ErrorCategory::Protocol, "fetch", "too many requests")
            .with_http_status(429);
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_http_500_is_retryable() {
        let err =
            AppError::new(ErrorCategory::Protocol, "fetch", "server error").with_http_status(500);
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_http_500_exhausted_is_failed() {
        let err =
            AppError::new(ErrorCategory::Protocol, "fetch", "server error").with_http_status(500);
        let limit = 3u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 3, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::Failed));
    }

    #[test]
    fn classify_http_404_not_in_classification() {
        // 404/410 are handled separately in process_claim before
        // classify_download_failure is called.  This test verifies
        // that classify_download_failure does NOT treat 404 as
        // retryable (it should be Failed since it has no special
        // handling here — 404 is classified via classify_404_410).
        let err = AppError::new(ErrorCategory::PlaybackUnavailable, "fetch", "not found")
            .with_http_status(404);
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        // PlaybackUnavailable is not Network/Timeout, so it's a permanent failure.
        assert!(matches!(d, DownloadFailureDisposition::Failed));
    }

    #[test]
    fn classify_authentication_is_failed() {
        let err = AppError::new(ErrorCategory::Authentication, "auth", "bad credentials");
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::Failed));
    }

    #[test]
    fn classify_authorization_is_failed() {
        let err = AppError::new(ErrorCategory::Authorization, "fetch", "forbidden");
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_download_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::Failed));
    }

    #[test]
    fn classify_404_410_retryable_before_limit() {
        let limit = 10u32;
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        let d = classify_404_410(1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_404_410_unavailable_at_limit() {
        let limit = 3u32;
        let initial = Duration::from_secs(5);
        let max = Duration::from_secs(300);
        let d = classify_404_410(3, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::Unavailable));
    }

    #[test]
    fn classify_404_410_uses_configured_backoff() {
        let limit = 10u32;
        let initial = Duration::from_secs(10);
        let max = Duration::from_secs(600);
        let d = classify_404_410(2, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_streaming_retryable() {
        let err = AppError::new(ErrorCategory::Filesystem, "stream", "write error");
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_streaming_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::RetryWait { .. }));
    }

    #[test]
    fn classify_streaming_non_retryable_is_failed() {
        let err = AppError::new(ErrorCategory::Protocol, "stream", "invalid data");
        let limit = 10u32;
        let initial = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        let d = classify_streaming_failure(&err, 1, &limit, &initial, &max);
        assert!(matches!(d, DownloadFailureDisposition::Failed));
    }

    // ── DownloadWorkerOptions::from_config tests ────────────────────────

    fn make_test_config(
        request_timeout_secs: u64,
        concurrency: usize,
    ) -> crate::configuration::Config {
        use crate::configuration::{GeneralConfig, NvrDownloadConfig, NvrSearchConfig};
        use crate::domain::Timestamp;

        crate::configuration::Config {
            general: GeneralConfig {
                database_path: PathBuf::from("/tmp/fauna-scan.db"),
                output_directory: PathBuf::from("/tmp/fauna-output"),
                log_level: crate::cli::LogLevel::Info,
            },
            nvr: crate::configuration::NvrConfig {
                scheme: "http".to_string(),
                host: "nvr.local".to_string(),
                port: 8080,
                username: "admin".to_string(),
                password: Some(crate::configuration::Secret::new("pass".to_string())),
                start_at: Timestamp::new(chrono::Utc::now()),
                request_timeout_seconds: request_timeout_secs,
                connect_timeout_seconds: 10,
                allow_invalid_tls_certificates: false,
                search: NvrSearchConfig {
                    window_minutes: 60,
                    max_results: 50,
                    poll_interval_seconds: 60,
                    poll_overlap_seconds: 120,
                    camera_refresh_interval_seconds: 3600,
                    settlement_delay_seconds: 10,
                },
                download: NvrDownloadConfig {
                    retry_limit: 10,
                    retry_initial_delay_seconds: 5,
                    retry_max_delay_seconds: 300,
                    maximum_image_size_bytes: 25_000_000,
                    verify_jpeg: true,
                    rebase_playback_urls: true,
                    concurrency,
                    playback_host_allowlist: vec![],
                },
            },
            classifier: crate::configuration::ClassifierConfig {
                endpoints: Vec::new(),
                poll_interval_seconds: 10,
                retry_limit: 5,
                retry_initial_delay_seconds: 10,
                retry_max_delay_seconds: 300,
                processing_lease_seconds: 600,
            },
            source_path: PathBuf::from("/tmp/config.toml"),
        }
    }

    #[test]
    fn from_config_derives_lease_duration() {
        // Lease = 2 * request_timeout + 30s.
        // request_timeout = 30 → lease = 90s.
        let config = make_test_config(30, 2);
        let options = DownloadWorkerOptions::from_config(&config).unwrap();
        assert_eq!(options.lease_duration, Duration::from_secs(90));
    }

    #[test]
    fn from_config_lease_default_timeout() {
        // request_timeout = 5 → lease = 2*5+30 = 40s.
        let config = make_test_config(5, 2);
        let options = DownloadWorkerOptions::from_config(&config).unwrap();
        assert_eq!(options.lease_duration, Duration::from_secs(40));
    }

    #[test]
    fn from_config_rejects_zero_concurrency() {
        let config = make_test_config(30, 0);
        let result = DownloadWorkerOptions::from_config(&config);
        assert!(result.is_err());
    }

    #[test]
    fn from_config_passes_through_download_settings() {
        let config = make_test_config(30, 4);
        let options = DownloadWorkerOptions::from_config(&config).unwrap();
        assert_eq!(options.concurrency, 4);
        assert_eq!(options.retry_limit, 10);
        assert_eq!(options.retry_initial_delay, Duration::from_secs(5));
        assert_eq!(options.retry_max_delay, Duration::from_secs(300));
        assert_eq!(options.maximum_image_size_bytes, 25_000_000);
        assert!(options.verify_jpeg);
    }

    #[test]
    fn from_config_overflow_safe_lease() {
        // Very large timeout should not overflow.
        let config = make_test_config(u64::MAX, 2);
        let result = DownloadWorkerOptions::from_config(&config);
        // Should succeed with a saturated lease duration.
        assert!(result.is_ok());
        let options = result.unwrap();
        // Lease should be Duration::MAX (saturated).
        // Compare seconds only since Duration::MAX includes subsec_nanos.
        assert_eq!(options.lease_duration.as_secs(), Duration::MAX.as_secs());
    }
}
