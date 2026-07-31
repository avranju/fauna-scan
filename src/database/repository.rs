//! All durable state changes for Fauna Scan, centralized in short
//! parameterized transactions.
//!
//! Defines the backend-neutral `DataStore` trait and the `DatabaseOps`
//! façade that delegates to a selected implementation.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::{CameraId, ClassificationId, ImageId, Timestamp};
use crate::error::AppResult;

use super::models::*;
use super::web_models::*;

// Re-export model types used in the DataStore trait signature.
pub use super::models::GarbageCollectionCandidate;

// ── Rate limit types ──────────────────────────────────────────────────────

/// Result of attempting to reserve provider quota before dispatching an HTTP
/// classification request. A denied reservation has not claimed an image and
/// therefore must not consume a processing attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitReservation {
    /// The sliding per-minute limit is temporarily full. The worker can wait
    /// and try again without abandoning its scanner pass.
    Wait(Duration),
    /// The UTC daily request or token budget is exhausted. This can be many
    /// hours, so the worker should yield until a later scanner pass.
    DailyExhausted(Duration),
    Granted,
}

/// An individual provider-quota reservation. It is returned only together
/// with an atomically claimed image, so it can safely be refunded if Cerebras
/// rejects the request before charging tokens.
#[derive(Debug, Clone)]
pub struct RateLimitGrant {
    pub id: String,
    pub quota_group: String,
    pub day: String,
    pub token_cost: i64,
}

/// Result of atomically checking quota and claiming work for a rate-limited
/// classifier endpoint.
#[derive(Debug)]
pub enum RateLimitedProcessingClaim {
    Claimed {
        claim: ProcessingClaim,
        grant: RateLimitGrant,
    },
    NoWork,
    Wait(Duration),
    DailyExhausted(Duration),
}

/// Outcome of a garbage-collection filesystem operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcOutcome {
    /// The file was successfully removed.
    Removed,
    /// The file was already missing; its path was reconciled.
    AlreadyMissing,
}

/// Boxed garbage-collection operation for object-safe callbacks.
///
/// The callback performs filesystem I/O (typically unlinking) and returns
/// a `GcOutcome` describing the result.  This type is object-safe so the
/// `DataStore` trait can be used behind an `Arc<dyn DataStore>`.
pub type GcOperation =
    Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = AppResult<GcOutcome>> + Send>> + Send>;

// ── DataStore trait ───────────────────────────────────────────────────────

/// Backend-neutral contract for every durable read, write, claim, and
/// garbage-collection coordination operation.
///
/// Implementations may use SQLite, PostgreSQL, or any other backend.
#[async_trait]
pub trait DataStore: Send + Sync {
    // ── Camera operations ───────────────────────────────────────────────

    /// Return enabled cameras ordered deterministically by channel number then
    /// picture track ID.
    async fn list_active_cameras(&self) -> AppResult<Vec<CameraRecord>>;

    /// Upsert the current discovery set and mark absent known cameras inactive.
    async fn sync_cameras(
        &self,
        cameras: &[CameraDiscovery],
        observed_at: &Timestamp,
    ) -> AppResult<Vec<CameraRecord>>;

    // ── Image discovery and cursor commit ───────────────────────────────

    /// Idempotently insert discovered images and advance the cursor atomically.
    async fn commit_search_window(
        &self,
        window: &SearchWindowCommit,
        images: &[DiscoveredImage],
    ) -> AppResult<u64>;

    /// Record a cursor error without advancing the cursor.
    async fn record_cursor_error(
        &self,
        camera_id: CameraId,
        error_msg: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()>;

    /// Read a camera's cursor, if it exists.
    async fn get_cursor(&self, camera_id: CameraId) -> AppResult<Option<SearchCursorRecord>>;

    // ── Download claiming and transitions ───────────────────────────────

    /// Atomically claim one pending or due-retry download.
    async fn claim_next_download(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<DownloadClaim>>;

    /// Guard a download completion: downloading → downloaded.
    async fn complete_download(
        &self,
        image_id: ImageId,
        local_path: &Path,
        downloaded_at: &Timestamp,
    ) -> AppResult<()>;

    /// Guard a download failure transition.
    async fn fail_download(
        &self,
        image_id: ImageId,
        error: &str,
        disposition: DownloadFailureDisposition,
        now: &Timestamp,
    ) -> AppResult<()>;

    // ── Processing claiming and transitions ─────────────────────────────

    /// Whether any downloaded image is eligible for a processing claim.
    async fn has_eligible_processing(&self, now: &Timestamp) -> AppResult<bool>;

    /// Atomically claim one downloaded image for processing.
    async fn claim_next_processing(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<ProcessingClaim>>;

    /// Guard a processing failure transition.
    async fn fail_processing(
        &self,
        image_id: ImageId,
        error: &str,
        raw_response: Option<String>,
        generation: i64,
        disposition: ProcessingFailureDisposition,
        now: &Timestamp,
    ) -> AppResult<()>;

    /// Transactionally insert a classification and mark processing done.
    async fn complete_classification(
        &self,
        image_id: ImageId,
        classification: &ClassificationInput,
        generation: i64,
        completed_at: &Timestamp,
    ) -> AppResult<ClassificationId>;

    // ── Lease management ────────────────────────────────────────────────

    /// Renew the processing lease for a row that is currently being processed.
    async fn renew_processing_lease(
        &self,
        image_id: ImageId,
        generation: i64,
        new_lease_until: &Timestamp,
        renewal_at: &Timestamp,
    ) -> AppResult<()>;

    /// Verify that the image is still in processing status with an unexpired
    /// lease and a matching generation token.
    async fn verify_processing_ownership(
        &self,
        image_id: ImageId,
        generation: i64,
    ) -> AppResult<()>;

    /// Return expired downloading and processing rows to retry_wait.
    async fn recover_expired_leases(&self, now: &Timestamp) -> AppResult<LeaseRecoveryCounts>;

    // ── Service metadata ────────────────────────────────────────────────

    /// Upsert a service metadata value.
    async fn set_metadata(
        &self,
        key: &ServiceMetadataKey,
        value: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()>;

    /// Read a service metadata value.
    async fn get_metadata(&self, key: &ServiceMetadataKey) -> AppResult<Option<String>>;

    // ── Status counts ───────────────────────────────────────────────────

    /// Return counts grouped by download and processing status.
    async fn status_counts(&self) -> AppResult<StatusCounts>;

    /// Return aggregate operational counters.
    async fn operational_summary(&self) -> AppResult<OperationalSummary>;

    // ── Row lookups ─────────────────────────────────────────────────────

    /// Fetch a full image record by ID.
    async fn get_image(&self, image_id: ImageId) -> AppResult<ImageRecord>;

    /// Fetch a classification record by image_id, model, and prompt_version.
    async fn get_classification(
        &self,
        image_id: ImageId,
        model: &str,
        prompt_version: &str,
    ) -> AppResult<ClassificationRecord>;

    // ── Garbage collection ──────────────────────────────────────────────

    /// Page eligible no-wildlife image paths for garbage collection.
    async fn garbage_collection_candidates(
        &self,
        cutoff: &Timestamp,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<GarbageCollectionCandidate>>;

    /// Page wildlife-positive rows whose stored path is not canonical.
    async fn wildlife_file_references_needing_reconciliation(
        &self,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<WildlifeFileReference>>;

    /// Replace a legacy wildlife-positive path with its current canonical path.
    async fn reconcile_wildlife_file_reference(
        &self,
        reference: &WildlifeFileReference,
        canonical_path: &Path,
    ) -> AppResult<()>;

    /// Mark an inaccessible legacy wildlife path as unresolved.
    async fn mark_wildlife_file_reference_unresolved(
        &self,
        reference: &WildlifeFileReference,
    ) -> AppResult<()>;

    /// Serialize the final eligibility check, shared-file check, unlink, and
    /// path clearing against classification writes.
    ///
    /// The `operation` parameter is a boxed, object-safe callback that performs
    /// filesystem I/O while the database transaction is held.
    async fn with_gc_candidate(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        canonical_identity: Option<&Path>,
        operation: GcOperation,
    ) -> AppResult<GcOutcome>;

    /// Clear `local_path` for a garbage-collected image.
    async fn mark_local_file_garbage_collected(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        collected_at: &Timestamp,
    ) -> AppResult<()>;

    // ── Rate limiting ───────────────────────────────────────────────────

    /// Atomically reserve one classifier request against its shared provider quota.
    async fn reserve_classifier_rate_limit(
        &self,
        limit: &crate::configuration::ClassifierRateLimitConfig,
        max_tokens: u32,
        now: &Timestamp,
    ) -> AppResult<RateLimitReservation>;

    /// Atomically admit provider quota and claim an image.
    async fn claim_next_processing_with_rate_limit(
        &self,
        limit: &crate::configuration::ClassifierRateLimitConfig,
        max_tokens: u32,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<RateLimitedProcessingClaim>;

    /// Cancel a reservation when request preparation failed.
    async fn cancel_classifier_rate_limit(&self, grant: &RateLimitGrant) -> AppResult<bool>;

    /// Refund daily quota after an explicit provider rejection.
    async fn refund_daily_classifier_rate_limit(
        &self,
        grant: &RateLimitGrant,
        now: &Timestamp,
    ) -> AppResult<bool>;

    // ── Web query operations ────────────────────────────────────────────

    /// Fetch health metadata and active counts.
    async fn web_health(&self) -> AppResult<WebHealthSnapshot>;

    /// List cameras with cursor data.
    async fn web_cameras(&self) -> AppResult<Vec<WebCameraRecord>>;

    /// Paginated image listing with filters.
    async fn web_query_images(
        &self,
        query: &WebImageQuery,
    ) -> AppResult<(Vec<WebImageSummaryRecord>, Option<(String, i64)>)>;

    /// Overview counts with filters.
    async fn web_overview(&self, filter: &WebImageFilter) -> AppResult<WebOverviewRecord>;

    /// Image detail by ID.
    async fn web_image_detail(&self, image_id: ImageId) -> AppResult<Option<WebImageDetailRecord>>;

    /// Image content lookup (path and status).
    async fn web_image_content_lookup(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebImageContentLookup>>;

    /// Recording target lookup.
    async fn web_recording_target(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebRecordingTarget>>;

    /// Activity counts and active work.
    async fn web_activity(&self, filter: &WebImageFilter) -> AppResult<WebActivityRecord>;
}

// ── DatabaseOps façade ────────────────────────────────────────────────────

/// Cloneable application-facing façade that delegates to a selected DataStore.
///
/// Consumers depend only on `DatabaseOps` and never see the concrete backend.
#[derive(Clone)]
pub struct DatabaseOps {
    inner: Arc<dyn DataStore>,
}

impl DatabaseOps {
    /// Create a new DatabaseOps wrapping the given DataStore.
    pub fn new(store: Arc<dyn DataStore>) -> Self {
        Self { inner: store }
    }

    // ── Delegate to DataStore ───────────────────────────────────────────

    pub async fn list_active_cameras(&self) -> AppResult<Vec<CameraRecord>> {
        self.inner.list_active_cameras().await
    }

    pub async fn sync_cameras(
        &self,
        cameras: &[CameraDiscovery],
        observed_at: &Timestamp,
    ) -> AppResult<Vec<CameraRecord>> {
        self.inner.sync_cameras(cameras, observed_at).await
    }

    pub async fn commit_search_window(
        &self,
        window: &SearchWindowCommit,
        images: &[DiscoveredImage],
    ) -> AppResult<u64> {
        self.inner.commit_search_window(window, images).await
    }

    pub async fn record_cursor_error(
        &self,
        camera_id: CameraId,
        error_msg: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        self.inner
            .record_cursor_error(camera_id, error_msg, updated_at)
            .await
    }

    pub async fn get_cursor(&self, camera_id: CameraId) -> AppResult<Option<SearchCursorRecord>> {
        self.inner.get_cursor(camera_id).await
    }

    pub async fn claim_next_download(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<DownloadClaim>> {
        self.inner.claim_next_download(now, lease_until).await
    }

    pub async fn complete_download(
        &self,
        image_id: ImageId,
        local_path: &Path,
        downloaded_at: &Timestamp,
    ) -> AppResult<()> {
        self.inner
            .complete_download(image_id, local_path, downloaded_at)
            .await
    }

    pub async fn fail_download(
        &self,
        image_id: ImageId,
        error: &str,
        disposition: DownloadFailureDisposition,
        now: &Timestamp,
    ) -> AppResult<()> {
        self.inner
            .fail_download(image_id, error, disposition, now)
            .await
    }

    pub async fn has_eligible_processing(&self, now: &Timestamp) -> AppResult<bool> {
        self.inner.has_eligible_processing(now).await
    }

    pub async fn claim_next_processing(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<ProcessingClaim>> {
        self.inner.claim_next_processing(now, lease_until).await
    }

    pub async fn fail_processing(
        &self,
        image_id: ImageId,
        error: &str,
        raw_response: Option<String>,
        generation: i64,
        disposition: ProcessingFailureDisposition,
        now: &Timestamp,
    ) -> AppResult<()> {
        self.inner
            .fail_processing(image_id, error, raw_response, generation, disposition, now)
            .await
    }

    pub async fn complete_classification(
        &self,
        image_id: ImageId,
        classification: &ClassificationInput,
        generation: i64,
        completed_at: &Timestamp,
    ) -> AppResult<ClassificationId> {
        self.inner
            .complete_classification(image_id, classification, generation, completed_at)
            .await
    }

    pub async fn renew_processing_lease(
        &self,
        image_id: ImageId,
        generation: i64,
        new_lease_until: &Timestamp,
        renewal_at: &Timestamp,
    ) -> AppResult<()> {
        self.inner
            .renew_processing_lease(image_id, generation, new_lease_until, renewal_at)
            .await
    }

    pub async fn verify_processing_ownership(
        &self,
        image_id: ImageId,
        generation: i64,
    ) -> AppResult<()> {
        self.inner
            .verify_processing_ownership(image_id, generation)
            .await
    }

    pub async fn recover_expired_leases(&self, now: &Timestamp) -> AppResult<LeaseRecoveryCounts> {
        self.inner.recover_expired_leases(now).await
    }

    pub async fn set_metadata(
        &self,
        key: &ServiceMetadataKey,
        value: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        self.inner.set_metadata(key, value, updated_at).await
    }

    pub async fn get_metadata(&self, key: &ServiceMetadataKey) -> AppResult<Option<String>> {
        self.inner.get_metadata(key).await
    }

    pub async fn status_counts(&self) -> AppResult<StatusCounts> {
        self.inner.status_counts().await
    }

    pub async fn operational_summary(&self) -> AppResult<OperationalSummary> {
        self.inner.operational_summary().await
    }

    pub async fn get_image(&self, image_id: ImageId) -> AppResult<ImageRecord> {
        self.inner.get_image(image_id).await
    }

    pub async fn get_classification(
        &self,
        image_id: ImageId,
        model: &str,
        prompt_version: &str,
    ) -> AppResult<ClassificationRecord> {
        self.inner
            .get_classification(image_id, model, prompt_version)
            .await
    }

    pub async fn garbage_collection_candidates(
        &self,
        cutoff: &Timestamp,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<GarbageCollectionCandidate>> {
        self.inner
            .garbage_collection_candidates(cutoff, after_image_id, limit)
            .await
    }

    pub async fn wildlife_file_references_needing_reconciliation(
        &self,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<WildlifeFileReference>> {
        self.inner
            .wildlife_file_references_needing_reconciliation(after_image_id, limit)
            .await
    }

    pub async fn reconcile_wildlife_file_reference(
        &self,
        reference: &WildlifeFileReference,
        canonical_path: &Path,
    ) -> AppResult<()> {
        self.inner
            .reconcile_wildlife_file_reference(reference, canonical_path)
            .await
    }

    pub async fn mark_wildlife_file_reference_unresolved(
        &self,
        reference: &WildlifeFileReference,
    ) -> AppResult<()> {
        self.inner
            .mark_wildlife_file_reference_unresolved(reference)
            .await
    }

    pub async fn with_gc_candidate(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        canonical_identity: Option<&Path>,
        operation: GcOperation,
    ) -> AppResult<GcOutcome> {
        self.inner
            .with_gc_candidate(candidate, cutoff, canonical_identity, operation)
            .await
    }

    pub async fn mark_local_file_garbage_collected(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        collected_at: &Timestamp,
    ) -> AppResult<()> {
        self.inner
            .mark_local_file_garbage_collected(candidate, cutoff, collected_at)
            .await
    }

    pub async fn reserve_classifier_rate_limit(
        &self,
        limit: &crate::configuration::ClassifierRateLimitConfig,
        max_tokens: u32,
        now: &Timestamp,
    ) -> AppResult<RateLimitReservation> {
        self.inner
            .reserve_classifier_rate_limit(limit, max_tokens, now)
            .await
    }

    pub async fn claim_next_processing_with_rate_limit(
        &self,
        limit: &crate::configuration::ClassifierRateLimitConfig,
        max_tokens: u32,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<RateLimitedProcessingClaim> {
        self.inner
            .claim_next_processing_with_rate_limit(limit, max_tokens, now, lease_until)
            .await
    }

    pub async fn cancel_classifier_rate_limit(&self, grant: &RateLimitGrant) -> AppResult<bool> {
        self.inner.cancel_classifier_rate_limit(grant).await
    }

    pub async fn refund_daily_classifier_rate_limit(
        &self,
        grant: &RateLimitGrant,
        now: &Timestamp,
    ) -> AppResult<bool> {
        self.inner
            .refund_daily_classifier_rate_limit(grant, now)
            .await
    }

    // ── Web query delegates ─────────────────────────────────────────────

    pub async fn web_health(&self) -> AppResult<WebHealthSnapshot> {
        self.inner.web_health().await
    }

    pub async fn web_cameras(&self) -> AppResult<Vec<WebCameraRecord>> {
        self.inner.web_cameras().await
    }

    pub async fn web_query_images(
        &self,
        query: &WebImageQuery,
    ) -> AppResult<(Vec<WebImageSummaryRecord>, Option<(String, i64)>)> {
        self.inner.web_query_images(query).await
    }

    pub async fn web_overview(&self, filter: &WebImageFilter) -> AppResult<WebOverviewRecord> {
        self.inner.web_overview(filter).await
    }

    pub async fn web_image_detail(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebImageDetailRecord>> {
        self.inner.web_image_detail(image_id).await
    }

    pub async fn web_image_content_lookup(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebImageContentLookup>> {
        self.inner.web_image_content_lookup(image_id).await
    }

    pub async fn web_recording_target(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebRecordingTarget>> {
        self.inner.web_recording_target(image_id).await
    }

    pub async fn web_activity(&self, filter: &WebImageFilter) -> AppResult<WebActivityRecord> {
        self.inner.web_activity(filter).await
    }
}
