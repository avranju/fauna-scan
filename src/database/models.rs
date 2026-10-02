//! Repository input/output and disposition types for Fauna Scan's database layer.
//!
//! These structures carry data between the repository methods and callers.
//! Secret-bearing configuration types are intentionally excluded.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::domain::{
    CameraId, ClassificationId, DownloadStatus, ImageId, ImageKey, ProcessingStatus, Timestamp,
    TrackId,
};

// ── Camera discovery ───────────────────────────────────────────────────────

/// One discovered camera carried into the synchronization transaction.
#[derive(Debug, Clone)]
pub struct CameraDiscovery {
    pub channel_number: i64,
    pub primary_track_id: String,
    pub picture_track_id: String,
    pub name: Option<String>,
    pub raw_discovery_identifier: Option<String>,
}

/// Persisted camera identity and discovery state returned by sync_cameras.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraRecord {
    pub id: CameraId,
    pub channel_number: i64,
    pub primary_track_id: String,
    pub picture_track_id: String,
    pub name: Option<String>,
    pub raw_discovery_identifier: Option<String>,
    pub enabled: bool,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Raw camera row for FromRow (uses raw types for sqlx compatibility).
#[derive(Debug, Clone, FromRow)]
pub(crate) struct CameraRow {
    pub id: i64,
    pub channel_number: i64,
    pub primary_track_id: String,
    pub picture_track_id: String,
    pub name: Option<String>,
    pub raw_discovery_identifier: Option<String>,
    pub enabled: i64,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Fallible conversion from a raw camera row to `CameraRecord`.
///
/// Returns an `AppError` with `ErrorCategory::Database` for any malformed
/// timestamp instead of panicking, so corrupted persisted data is reported
/// as a database error.
pub(crate) fn camera_row_to_record(row: CameraRow) -> crate::error::AppResult<CameraRecord> {
    use crate::error::{AppError, ErrorCategory};
    fn parse_ts(s: &str) -> crate::error::AppResult<Timestamp> {
        s.parse::<Timestamp>().map_err(|e| {
            AppError::with_source(
                ErrorCategory::Database,
                "parse_camera_timestamp",
                format!("invalid timestamp value '{s}': {e}"),
                anyhow::Error::from(e),
            )
        })
    }
    Ok(CameraRecord {
        id: CameraId::new(row.id),
        channel_number: row.channel_number,
        primary_track_id: row.primary_track_id,
        picture_track_id: row.picture_track_id,
        name: row.name,
        raw_discovery_identifier: row.raw_discovery_identifier,
        enabled: row.enabled != 0,
        first_seen_at: parse_ts(&row.first_seen_at)?,
        last_seen_at: parse_ts(&row.last_seen_at)?,
        created_at: parse_ts(&row.created_at)?,
        updated_at: parse_ts(&row.updated_at)?,
    })
}

// ── Image discovery ────────────────────────────────────────────────────────

/// Discovered image metadata without mutable work-state fields.
#[derive(Debug, Clone)]
pub struct DiscoveredImage {
    pub image_key: ImageKey,
    pub camera_id: CameraId,
    pub track_id: TrackId,
    pub capture_start_at: Timestamp,
    pub capture_end_at: Option<Timestamp>,
    pub playback_uri: String,
    pub canonical_playback_uri: String,
    pub codec_type: Option<String>,
    pub content_type: Option<String>,
    pub nvr_reported_size: Option<i64>,
    pub discovered_at: Timestamp,
}

/// Full durable image row including download and processing state.
///
/// This type intentionally does NOT derive `Debug` because
/// `processing_last_raw_response` may contain classifier output that
/// must never appear in logs or debug output.  A manual `Debug` impl
/// reports only the presence/length of the raw response.
#[derive(Clone, Serialize, Deserialize)]
pub struct ImageRecord {
    pub id: ImageId,
    pub image_key: ImageKey,
    pub camera_id: CameraId,
    pub track_id: TrackId,
    pub capture_start_at: Timestamp,
    pub capture_end_at: Option<Timestamp>,
    pub playback_uri: String,
    pub canonical_playback_uri: String,
    pub codec_type: Option<String>,
    pub content_type: Option<String>,
    pub nvr_reported_size: Option<i64>,
    pub local_path: Option<PathBuf>,

    pub download_status: DownloadStatus,
    pub download_attempts: i64,
    pub downloaded_at: Option<Timestamp>,
    pub download_last_error: Option<String>,
    pub download_next_attempt_at: Option<Timestamp>,
    pub download_lease_until: Option<Timestamp>,

    pub processing_status: ProcessingStatus,
    pub processing_attempts: i64,
    pub processing_started_at: Option<Timestamp>,
    pub processing_completed_at: Option<Timestamp>,
    pub processing_last_error: Option<String>,
    /// Latest available raw classifier response for failed processing attempts.
    ///
    /// Stored only for diagnostic purposes — never logged.
    pub processing_last_raw_response: Option<String>,
    pub processing_generation: i64,
    pub processing_next_attempt_at: Option<Timestamp>,
    pub processing_lease_until: Option<Timestamp>,

    pub discovered_at: Timestamp,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

// Manual Debug for ImageRecord — omits the raw classifier response body
// to prevent accidental logging of diagnostic payload data.
impl std::fmt::Debug for ImageRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageRecord")
            .field("id", &self.id)
            .field("image_key", &self.image_key)
            .field("camera_id", &self.camera_id)
            .field("track_id", &self.track_id)
            .field("capture_start_at", &self.capture_start_at)
            .field("capture_end_at", &self.capture_end_at)
            .field("local_path", &self.local_path)
            .field("download_status", &self.download_status)
            .field("download_attempts", &self.download_attempts)
            .field("downloaded_at", &self.downloaded_at)
            .field("download_last_error", &self.download_last_error)
            .field("download_next_attempt_at", &self.download_next_attempt_at)
            .field("download_lease_until", &self.download_lease_until)
            .field("processing_status", &self.processing_status)
            .field("processing_attempts", &self.processing_attempts)
            .field("processing_started_at", &self.processing_started_at)
            .field("processing_completed_at", &self.processing_completed_at)
            .field("processing_last_error", &self.processing_last_error)
            .field(
                "processing_last_raw_response",
                &format!(
                    "{} bytes",
                    self.processing_last_raw_response
                        .as_ref()
                        .map(|s| s.len())
                        .unwrap_or(0)
                ),
            )
            .field("processing_generation", &self.processing_generation)
            .field(
                "processing_next_attempt_at",
                &self.processing_next_attempt_at,
            )
            .field("processing_lease_until", &self.processing_lease_until)
            .field("discovered_at", &self.discovered_at)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

// ── Work claims ────────────────────────────────────────────────────────────

/// Data returned by a successful download claim.
#[derive(Debug, Clone)]
pub struct DownloadClaim {
    pub image_id: ImageId,
    pub image_key: ImageKey,
    pub camera_id: CameraId,
    pub track_id: TrackId,
    pub capture_start_at: Timestamp,
    pub playback_uri: String,
    pub canonical_playback_uri: String,
    pub download_attempts: i64,
    pub lease_until: Timestamp,
    /// Camera channel number (for destination path construction).
    pub camera_channel_number: i64,
    /// Optional camera name (for destination path construction).
    pub camera_name: Option<String>,
    /// NVR-reported size, when available.
    pub nvr_reported_size: Option<i64>,
}

/// Data returned by a successful processing claim.
#[derive(Debug, Clone)]
pub struct ProcessingClaim {
    pub image_id: ImageId,
    pub image_key: ImageKey,
    pub local_path: PathBuf,
    pub processing_attempts: i64,
    pub generation: i64,
    pub lease_until: Timestamp,
}

// ── Failure dispositions ───────────────────────────────────────────────────

/// Disposition for a download failure transition.
#[derive(Debug, Clone, Copy)]
pub enum DownloadFailureDisposition {
    RetryWait { next_attempt_at: Timestamp },
    Unavailable,
    Failed,
}

/// Disposition for a processing failure transition.
#[derive(Debug, Clone)]
pub enum ProcessingFailureDisposition {
    RetryWait { next_attempt_at: Timestamp },
    Failed,
    Missing,
}

// ── Classification ─────────────────────────────────────────────────────────

/// Validated classifier result carried into the atomic completion transaction.
///
/// Manual Debug impl omits `raw_response` to prevent accidental logging
/// of classifier output data.
#[derive(Clone)]
pub struct ClassificationInput {
    pub model: String,
    pub prompt_version: String,
    pub contains_wildlife: bool,
    pub is_interesting: bool,
    pub summary: Option<String>,
    pub species_json: Option<String>,
    pub bounding_boxes_json: Option<String>,
    pub confidence: Option<f64>,
    pub classification_json: Option<String>,
    pub raw_response: Option<String>,
    pub request_started_at: Timestamp,
    pub request_completed_at: Timestamp,
}

/// Persisted classification result.
///
/// Manual Debug impl omits `raw_response` to prevent accidental logging
/// of classifier output data.
#[derive(Clone, Serialize, Deserialize)]
pub struct ClassificationRecord {
    pub id: ClassificationId,
    pub image_id: ImageId,
    pub model: String,
    pub prompt_version: String,
    pub contains_wildlife: bool,
    pub is_interesting: bool,
    pub summary: Option<String>,
    pub species_json: Option<String>,
    pub bounding_boxes_json: Option<String>,
    pub confidence: Option<f64>,
    pub classification_json: Option<String>,
    pub raw_response: Option<String>,
    pub request_started_at: Timestamp,
    pub request_completed_at: Timestamp,
    pub created_at: Timestamp,
}

// Manual Debug for ClassificationInput — omits raw_response.
impl std::fmt::Debug for ClassificationInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassificationInput")
            .field("model", &self.model)
            .field("prompt_version", &self.prompt_version)
            .field("contains_wildlife", &self.contains_wildlife)
            .field("is_interesting", &self.is_interesting)
            .field("summary", &self.summary)
            .field(
                "species_json_len",
                &self.species_json.as_ref().map(|s| s.len()),
            )
            .field(
                "bounding_boxes_json_len",
                &self.bounding_boxes_json.as_ref().map(|s| s.len()),
            )
            .field("confidence", &self.confidence)
            .field(
                "classification_json_len",
                &self.classification_json.as_ref().map(|s| s.len()),
            )
            .field(
                "raw_response_len",
                &self.raw_response.as_ref().map(|s| s.len()),
            )
            .field("request_started_at", &self.request_started_at)
            .field("request_completed_at", &self.request_completed_at)
            .finish()
    }
}

// Manual Debug for ClassificationRecord — omits raw_response.
impl std::fmt::Debug for ClassificationRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassificationRecord")
            .field("id", &self.id)
            .field("image_id", &self.image_id)
            .field("model", &self.model)
            .field("prompt_version", &self.prompt_version)
            .field("contains_wildlife", &self.contains_wildlife)
            .field("is_interesting", &self.is_interesting)
            .field("summary", &self.summary)
            .field(
                "species_json_len",
                &self.species_json.as_ref().map(|s| s.len()),
            )
            .field(
                "bounding_boxes_json_len",
                &self.bounding_boxes_json.as_ref().map(|s| s.len()),
            )
            .field("confidence", &self.confidence)
            .field(
                "classification_json_len",
                &self.classification_json.as_ref().map(|s| s.len()),
            )
            .field(
                "raw_response_len",
                &self.raw_response.as_ref().map(|s| s.len()),
            )
            .field("request_started_at", &self.request_started_at)
            .field("request_completed_at", &self.request_completed_at)
            .field("created_at", &self.created_at)
            .finish()
    }
}

// ── Search cursor ──────────────────────────────────────────────────────────

/// Persisted per-camera search progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchCursorRecord {
    pub camera_id: CameraId,
    pub next_search_at: Option<Timestamp>,
    pub last_completed_window_start: Option<Timestamp>,
    pub last_completed_window_end: Option<Timestamp>,
    pub last_poll_at: Option<Timestamp>,
    pub last_error: Option<String>,
    pub updated_at: Timestamp,
}

/// Raw cursor row for FromRow (uses raw types for sqlx compatibility).
#[derive(Debug, Clone, FromRow)]
pub(crate) struct SearchCursorRow {
    pub camera_id: i64,
    pub next_search_at: Option<String>,
    pub last_completed_window_start: Option<String>,
    pub last_completed_window_end: Option<String>,
    pub last_poll_at: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// Fallible conversion from a raw cursor row to `SearchCursorRecord`.
///
/// Returns an `AppError` with `ErrorCategory::Database` for any malformed
/// timestamp instead of panicking.
pub(crate) fn cursor_row_to_record(
    row: SearchCursorRow,
) -> crate::error::AppResult<SearchCursorRecord> {
    use crate::error::{AppError, ErrorCategory};
    fn parse_ts(s: &str) -> crate::error::AppResult<Timestamp> {
        s.parse::<Timestamp>().map_err(|e| {
            AppError::with_source(
                ErrorCategory::Database,
                "parse_cursor_timestamp",
                format!("invalid timestamp value '{s}': {e}"),
                anyhow::Error::from(e),
            )
        })
    }
    Ok(SearchCursorRecord {
        camera_id: CameraId::new(row.camera_id),
        next_search_at: row.next_search_at.as_deref().map(parse_ts).transpose()?,
        last_completed_window_start: row
            .last_completed_window_start
            .as_deref()
            .map(parse_ts)
            .transpose()?,
        last_completed_window_end: row
            .last_completed_window_end
            .as_deref()
            .map(parse_ts)
            .transpose()?,
        last_poll_at: row.last_poll_at.as_deref().map(parse_ts).transpose()?,
        last_error: row.last_error,
        updated_at: parse_ts(&row.updated_at)?,
    })
}

/// All cursor values committed with a completed discovery window.
#[derive(Debug, Clone)]
pub struct SearchWindowCommit {
    pub camera_id: CameraId,
    pub window_start: Timestamp,
    pub window_end: Timestamp,
    pub next_search_at: Timestamp,
    pub polled_at: Timestamp,
    pub updated_at: Timestamp,
}

// ── Service metadata ───────────────────────────────────────────────────────

/// Required service metadata keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceMetadataKey {
    ApplicationVersion,
    NvrIdentity,
    InitialBackfillCompleted,
    LastSuccessfulCameraDiscovery,
    LastSuccessfulDownloaderPoll,
    LastSuccessfulScannerPass,
    DownloaderHeartbeat,
    ScannerHeartbeat,
}

impl ServiceMetadataKey {
    /// Return the canonical string key for database storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ApplicationVersion => "application_version",
            Self::NvrIdentity => "nvr_identity",
            Self::InitialBackfillCompleted => "initial_backfill_completed",
            Self::LastSuccessfulCameraDiscovery => "last_successful_camera_discovery",
            Self::LastSuccessfulDownloaderPoll => "last_successful_downloader_poll",
            Self::LastSuccessfulScannerPass => "last_successful_scanner_pass",
            Self::DownloaderHeartbeat => "downloader_heartbeat",
            Self::ScannerHeartbeat => "scanner_heartbeat",
        }
    }
}

// ── Status counts ──────────────────────────────────────────────────────────

/// Counts grouped by typed download and processing states.
#[derive(Debug, Clone, Default)]
pub struct StatusCounts {
    pub download: BTreeMap<DownloadStatus, i64>,
    pub processing: BTreeMap<ProcessingStatus, i64>,
}

/// Compact aggregate values used by the service health summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OperationalSummary {
    pub cameras_active: i64,
    pub images_discovered: i64,
    pub images_downloaded: i64,
    pub downloads_pending: i64,
    pub images_awaiting_classification: i64,
    pub classifications_completed: i64,
    pub retryable_failures: i64,
    pub permanent_failures: i64,
}

// ── Lease recovery ─────────────────────────────────────────────────────────

/// Report of how many expired download and processing claims were recovered.
#[derive(Debug, Clone, Default)]
pub struct LeaseRecoveryCounts {
    pub downloads: u64,
    pub processing: u64,
}

// ── Garbage collection candidates ──────────────────────────────────────────

/// A candidate image selected for local-file garbage collection.
///
/// Carries only the minimal durable identity and path needed to collect
/// one local image.  Classification payloads are intentionally excluded
/// so logging cannot expose raw classifier responses.
#[derive(Debug, Clone)]
pub struct GarbageCollectionCandidate {
    /// The image row identifier.
    pub image_id: ImageId,
    /// The local file path that may be removed.
    pub local_path: PathBuf,
}

/// A wildlife-positive path whose persisted filesystem identity is not known
/// to be current. These rows are reconciled before collection so an alias
/// cannot hide a permanently retained wildlife file.
#[derive(Debug, Clone)]
pub struct WildlifeFileReference {
    pub image_id: ImageId,
    pub local_path: PathBuf,
}

// ── Helpers ────────────────────────────────────────────────────────────────

// All timestamp parsing is now fallible via camera_row_to_record and
// cursor_row_to_record.  The old parse_ts_or_panic helper has been removed
// so that malformed timestamps are reported as database errors rather than
// panicking.

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DownloadStatus, ProcessingStatus, Timestamp};
    use chrono::Utc;

    fn sample_image_record(raw_response: Option<String>) -> ImageRecord {
        ImageRecord {
            id: ImageId::new(1),
            image_key: ImageKey::new("sentinel-key"),
            camera_id: CameraId::new(1),
            track_id: TrackId::new("103"),
            capture_start_at: Timestamp::new(Utc::now()),
            capture_end_at: None,
            playback_uri: "http://nvr/pic/1".to_string(),
            canonical_playback_uri: "http://nvr/pic/1".to_string(),
            codec_type: Some("jpeg".to_string()),
            content_type: Some("picture".to_string()),
            nvr_reported_size: Some(1000),
            local_path: Some(PathBuf::from("/tmp/test.jpg")),
            download_status: DownloadStatus::Downloaded,
            download_attempts: 1,
            downloaded_at: Some(Timestamp::new(Utc::now())),
            download_last_error: None,
            download_next_attempt_at: None,
            download_lease_until: None,
            processing_status: ProcessingStatus::Processing,
            processing_attempts: 1,
            processing_started_at: Some(Timestamp::new(Utc::now())),
            processing_completed_at: None,
            processing_last_error: Some("test error".to_string()),
            processing_last_raw_response: raw_response,
            processing_generation: 1,
            processing_next_attempt_at: None,
            processing_lease_until: Some(Timestamp::new(Utc::now())),
            discovered_at: Timestamp::new(Utc::now()),
            created_at: Timestamp::new(Utc::now()),
            updated_at: Timestamp::new(Utc::now()),
        }
    }

    #[test]
    fn image_record_debug_omits_raw_response() {
        let record = sample_image_record(Some(
            "{\"model_output\": \"SENTINEL-CLASSIFIER-OUTPUT\"}".to_string(),
        ));
        let debug_str = format!("{record:?}");
        // The raw response body must NOT appear in Debug output.
        assert!(
            !debug_str.contains("SENTINEL-CLASSIFIER-OUTPUT"),
            "Debug output leaked raw response body: {debug_str}"
        );
        // But the length should be reported.
        assert!(
            debug_str.contains("bytes"),
            "Debug output should report raw response length: {debug_str}"
        );
    }

    #[test]
    fn image_record_debug_no_raw_response() {
        let record = sample_image_record(None);
        let debug_str = format!("{record:?}");
        assert!(debug_str.contains("0 bytes"));
    }

    // ── ClassificationInput Debug ─────────────────────────────────────────

    #[test]
    fn classification_input_debug_omits_raw_response() {
        let input = ClassificationInput {
            model: "vision-v1".to_string(),
            prompt_version: "wildlife-v1".to_string(),
            contains_wildlife: true,
            is_interesting: true,
            summary: Some("A deer".to_string()),
            species_json: Some(r#"[{"name":"deer","confidence":0.9}]"#.to_string()),
            bounding_boxes_json: None,
            confidence: Some(0.9),
            classification_json: None,
            raw_response: Some("SENTINEL-CLASSIFIER-RAW-RESPONSE".to_string()),
            request_started_at: Timestamp::new(Utc::now()),
            request_completed_at: Timestamp::new(Utc::now()),
        };
        let debug_str = format!("{input:?}");
        // The raw response body must NOT appear.
        assert!(
            !debug_str.contains("SENTINEL-CLASSIFIER-RAW-RESPONSE"),
            "Debug output leaked raw response: {debug_str}"
        );
        // But length should be reported.
        assert!(
            debug_str.contains("raw_response_len"),
            "Debug should report raw_response_len: {debug_str}"
        );
    }

    #[test]
    fn classification_input_debug_no_raw_response() {
        let input = ClassificationInput {
            model: "vision-v1".to_string(),
            prompt_version: "wildlife-v1".to_string(),
            contains_wildlife: false,
            is_interesting: false,
            summary: None,
            species_json: None,
            bounding_boxes_json: None,
            confidence: None,
            classification_json: None,
            raw_response: None,
            request_started_at: Timestamp::new(Utc::now()),
            request_completed_at: Timestamp::new(Utc::now()),
        };
        let debug_str = format!("{input:?}");
        assert!(debug_str.contains("raw_response_len"));
        assert!(debug_str.contains("None"));
    }

    // ── ClassificationRecord Debug ────────────────────────────────────────

    #[test]
    fn classification_record_debug_omits_raw_response() {
        let record = ClassificationRecord {
            id: ClassificationId::new(1),
            image_id: ImageId::new(1),
            model: "vision-v1".to_string(),
            prompt_version: "wildlife-v1".to_string(),
            contains_wildlife: true,
            is_interesting: true,
            summary: Some("A deer".to_string()),
            species_json: Some(r#"[{"name":"deer","confidence":0.9}]"#.to_string()),
            bounding_boxes_json: None,
            confidence: Some(0.9),
            classification_json: None,
            raw_response: Some("SENTINEL-CLASSIFIER-RAW-RESPONSE".to_string()),
            request_started_at: Timestamp::new(Utc::now()),
            request_completed_at: Timestamp::new(Utc::now()),
            created_at: Timestamp::new(Utc::now()),
        };
        let debug_str = format!("{record:?}");
        // The raw response body must NOT appear.
        assert!(
            !debug_str.contains("SENTINEL-CLASSIFIER-RAW-RESPONSE"),
            "Debug output leaked raw response: {debug_str}"
        );
        // But length should be reported.
        assert!(
            debug_str.contains("raw_response_len"),
            "Debug should report raw_response_len: {debug_str}"
        );
    }

    #[test]
    fn classification_record_debug_no_raw_response() {
        let record = ClassificationRecord {
            id: ClassificationId::new(1),
            image_id: ImageId::new(1),
            model: "vision-v1".to_string(),
            prompt_version: "wildlife-v1".to_string(),
            contains_wildlife: false,
            is_interesting: false,
            summary: None,
            species_json: None,
            bounding_boxes_json: None,
            confidence: None,
            classification_json: None,
            raw_response: None,
            request_started_at: Timestamp::new(Utc::now()),
            request_completed_at: Timestamp::new(Utc::now()),
            created_at: Timestamp::new(Utc::now()),
        };
        let debug_str = format!("{record:?}");
        assert!(debug_str.contains("raw_response_len"));
        assert!(debug_str.contains("None"));
    }
}
