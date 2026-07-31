//! Garbage collection of locally downloaded images classified as no-wildlife.
//!
//! Runs after classification workers finish draining a scanner pass.
//! Collects images whose capture time is older than the retention cutoff
//! and whose completed classifications contain no wildlife.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::database::repository::{DatabaseOps, GcOperation, GcOutcome};
use crate::domain::ImageId;
use crate::domain::Timestamp;
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::filesystem::{self, LocalFileRemoval};
use crate::service_lifecycle::ShutdownToken;

/// Maximum number of candidates to fetch per batch during pagination.
const CANDIDATE_BATCH_SIZE: u32 = 256;

/// Outcome report for a garbage collection pass.
#[derive(Debug, Clone, Default)]
pub struct GarbageCollectionReport {
    /// Files successfully removed.
    pub files_removed: u64,
    /// Files that were already missing and had their local_path reconciled.
    pub missing_files_reconciled: u64,
    /// Per-file filesystem failures (path outside root, permission error, etc.).
    /// These candidates remain eligible for retry on a later pass.
    pub filesystem_failures: u64,
}

/// Internal state shared between the collector and test for the hook.
struct HookState {
    batch_complete: Notify,
    resume: Notify,
    candidate_ready: Notify,
    candidate_resume: Notify,
    candidate_barrier: std::sync::atomic::AtomicBool,
}

/// Optional hook for testing cancellation at batch boundaries.
///
/// When provided, the collector waits for the hook to be notified after
/// processing each batch of candidates.  This allows tests to trigger
/// shutdown at deterministic points without timing-dependent polling.
#[derive(Clone)]
pub struct CollectorHook {
    state: Arc<HookState>,
}

impl CollectorHook {
    /// Create a new hook that is not initially notified.
    pub fn new() -> Self {
        Self {
            state: Arc::new(HookState {
                batch_complete: Notify::new(),
                resume: Notify::new(),
                candidate_ready: Notify::new(),
                candidate_resume: Notify::new(),
                candidate_barrier: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Signal that the current batch is complete.
    pub fn notify(&self) {
        self.state.batch_complete.notify_one();
    }

    /// Wait until the collector reports a completed batch.
    pub async fn wait(&self) {
        self.state.batch_complete.notified().await;
    }

    /// Release the collector to fetch the next batch.
    pub fn release(&self) {
        self.state.resume.notify_one();
    }

    async fn wait_for_resume(&self) {
        self.state.resume.notified().await;
    }

    /// Enable the optional candidate boundary used by race regression tests.
    pub fn enable_candidate_barrier(&self) {
        self.state
            .candidate_barrier
            .store(true, std::sync::atomic::Ordering::Release);
    }

    async fn wait_for_candidate_release(&self) {
        if self
            .state
            .candidate_barrier
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.state.candidate_ready.notify_one();
            self.state.candidate_resume.notified().await;
        }
    }

    /// Wait until the collector has selected and canonicalized a candidate.
    pub async fn wait_for_candidate(&self) {
        self.state.candidate_ready.notified().await;
    }

    /// Release a candidate paused by the optional race barrier.
    pub fn release_candidate(&self) {
        self.state.candidate_resume.notify_one();
    }
}

impl Default for CollectorHook {
    fn default() -> Self {
        Self::new()
    }
}

/// Run one bounded, cancellation-aware garbage collection sweep.
///
/// Computes the strict capture-time cutoff, pages DatabaseOps candidates,
/// canonicalizes the output root only when work exists, removes safe files,
/// clears local_path, continues after individual filesystem failures, and
/// propagates database or coordination errors.
///
/// Shared-positive path safety is checked by a transaction that remains held
/// while the file is unlinked. Canonical filesystem identities are indexed in
/// the images table, with an exact-path fallback for legacy rows.
///
/// # Hooks
///
/// When `hook` is provided, the collector waits for the hook to be notified
/// after processing each batch of candidates.  This allows tests to trigger
/// shutdown at deterministic points without timing-dependent polling.
pub async fn collect_non_wildlife_images(
    database: &DatabaseOps,
    output_directory: &Path,
    retention: Duration,
    now: &Timestamp,
    shutdown: &ShutdownToken,
    hook: Option<&CollectorHook>,
) -> AppResult<GarbageCollectionReport> {
    // Compute the strict cutoff: capture_start_at must be strictly before this.
    let cutoff = compute_cutoff(now, retention)?;

    let mut report = GarbageCollectionReport::default();

    // New downloads persist canonical local_path values. Reconcile legacy
    // wildlife-positive aliases before selecting anything for deletion. This
    // filesystem pass is bounded per query, runs outside SQLite write
    // transactions, and can be interrupted between every reference.
    let mut after_wildlife_id: Option<ImageId> = None;
    loop {
        if shutdown.is_cancelled() {
            return Ok(report);
        }
        let references = database
            .wildlife_file_references_needing_reconciliation(
                after_wildlife_id,
                CANDIDATE_BATCH_SIZE,
            )
            .await?;
        if references.is_empty() {
            break;
        }
        for reference in &references {
            if shutdown.is_cancelled() {
                return Ok(report);
            }
            match tokio::fs::canonicalize(&reference.local_path).await {
                Ok(canonical_path) => {
                    database
                        .reconcile_wildlife_file_reference(reference, &canonical_path)
                        .await?;
                }
                Err(error) => {
                    // Keep the reference marked unresolved. The final short
                    // SQL guard will refuse all deletion until this path can
                    // be reconciled, preserving fail-safe behavior.
                    database
                        .mark_wildlife_file_reference_unresolved(reference)
                        .await?;
                    tracing::warn!(
                        image_id = reference.image_id.get(),
                        error_category = ?ErrorCategory::Filesystem,
                        kind = ?error.kind(),
                        "Cannot reconcile wildlife-positive local path; collection deferred"
                    );
                    report.filesystem_failures += 1;
                    return Ok(report);
                }
            }
            after_wildlife_id = Some(reference.image_id);
        }
        if references.len() < CANDIDATE_BATCH_SIZE as usize {
            break;
        }
    }

    let mut after_image_id: Option<ImageId> = None;

    loop {
        // Check cancellation before fetching the next batch.
        if shutdown.is_cancelled() {
            tracing::info!(
                files_removed = report.files_removed,
                missing_reconciled = report.missing_files_reconciled,
                filesystem_failures = report.filesystem_failures,
                "Garbage collection interrupted by shutdown"
            );
            return Ok(report);
        }

        // Fetch the next batch of candidates.
        let candidates = database
            .garbage_collection_candidates(&cutoff, after_image_id, CANDIDATE_BATCH_SIZE)
            .await?;

        if candidates.is_empty() {
            break;
        }

        // Process each candidate.
        for candidate in &candidates {
            // Check cancellation between candidates.
            if shutdown.is_cancelled() {
                tracing::info!(
                    files_removed = report.files_removed,
                    missing_reconciled = report.missing_files_reconciled,
                    filesystem_failures = report.filesystem_failures,
                    candidate_image_id = candidate.image_id.get(),
                    "Garbage collection interrupted by shutdown"
                );
                return Ok(report);
            }

            // Canonicalize only the candidate being considered. The database
            // transaction below performs the shared-wildlife lookup and stays
            // held through unlinking, so classification writes cannot race it.
            let canonical_identity = match tokio::fs::canonicalize(&candidate.local_path).await {
                Ok(path) => Some(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => {
                    tracing::warn!(
                        image_id = candidate.image_id.get(),
                        error_category = ?ErrorCategory::Filesystem,
                        "Cannot canonicalize candidate path; skipping"
                    );
                    report.filesystem_failures += 1;
                    after_image_id = Some(candidate.image_id);
                    continue;
                }
            };

            if let Some(hook) = hook {
                hook.wait_for_candidate_release().await;
            }

            let candidate_path = candidate.local_path.clone();
            let output_root = output_directory.to_path_buf();
            let operation: GcOperation = Box::new(move || {
                Box::pin(async move {
                    match filesystem::remove_managed_image_file(&output_root, &candidate_path).await
                    {
                        Ok(LocalFileRemoval::Removed) => Ok(GcOutcome::Removed),
                        Ok(LocalFileRemoval::AlreadyMissing) => Ok(GcOutcome::AlreadyMissing),
                        Err(e) => Err(e),
                    }
                })
            });
            let collection_result = database
                .with_gc_candidate(candidate, &cutoff, canonical_identity.as_deref(), operation)
                .await;

            match collection_result {
                Err(e) if e.category == ErrorCategory::Database => {
                    // Ineligible or shared-wildlife — advance cursor and continue.
                    tracing::debug!(
                        image_id = candidate.image_id.get(),
                        "Candidate became ineligible or shares a wildlife-positive file"
                    );
                }
                Err(e) if e.category == ErrorCategory::Filesystem => {
                    tracing::warn!(
                        image_id = candidate.image_id.get(),
                        error_category = ?e.category,
                        "Filesystem failure during garbage collection; candidate remains eligible"
                    );
                    report.filesystem_failures += 1;
                }
                Ok(GcOutcome::Removed) => report.files_removed += 1,
                Ok(GcOutcome::AlreadyMissing) => report.missing_files_reconciled += 1,
                Err(e) => return Err(e),
            }

            // Advance pagination cursor regardless of per-file outcome.
            after_image_id = Some(candidate.image_id);
        }

        // Notify the hook that this batch is complete (if provided).
        if let Some(hook) = hook {
            hook.notify();
            // Wait for an explicit release. This two-way barrier prevents
            // observing the completion notification from releasing us too.
            hook.wait_for_resume().await;
        }

        // If we got fewer than the batch size, we've reached the end.
        if candidates.len() < CANDIDATE_BATCH_SIZE as usize {
            break;
        }
    }

    if report.files_removed > 0
        || report.missing_files_reconciled > 0
        || report.filesystem_failures > 0
    {
        tracing::info!(
            files_removed = report.files_removed,
            missing_reconciled = report.missing_files_reconciled,
            filesystem_failures = report.filesystem_failures,
            "Garbage collection pass completed"
        );
    }

    Ok(report)
}

/// Compute the strict capture-time cutoff from now and retention duration.
///
/// Returns an error if the retention is zero, if the subtraction would
/// underflow Chrono's DateTime bounds, or if the retention duration exceeds
/// i64::MAX seconds.
fn compute_cutoff(now: &Timestamp, retention: Duration) -> AppResult<Timestamp> {
    if retention.is_zero() {
        return Err(AppError::new(
            ErrorCategory::Internal,
            "collect_non_wildlife_images",
            "retention duration must be greater than zero",
        ));
    }

    let retention_secs = i64::try_from(retention.as_secs()).map_err(|_| {
        AppError::new(
            ErrorCategory::Internal,
            "collect_non_wildlife_images",
            "retention duration exceeds i64::MAX seconds; cannot compute cutoff",
        )
    })?;
    let now_dt = *now.as_datetime();

    let cutoff_dt = now_dt
        .checked_sub_signed(chrono::Duration::seconds(retention_secs))
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::Internal,
                "collect_non_wildlife_images",
                "cutoff computation underflowed Chrono DateTime bounds",
            )
        })?;

    Ok(Timestamp::new(cutoff_dt))
}
