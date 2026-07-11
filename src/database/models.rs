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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub processing_next_attempt_at: Option<Timestamp>,
    pub processing_lease_until: Option<Timestamp>,

    pub discovered_at: Timestamp,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
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
}

/// Data returned by a successful processing claim.
#[derive(Debug, Clone)]
pub struct ProcessingClaim {
    pub image_id: ImageId,
    pub image_key: ImageKey,
    pub local_path: PathBuf,
    pub processing_attempts: i64,
    pub lease_until: Timestamp,
}

// ── Failure dispositions ───────────────────────────────────────────────────

/// Disposition for a download failure transition.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct ClassificationInput {
    pub model: String,
    pub prompt_version: String,
    pub contains_wildlife: bool,
    pub is_interesting: bool,
    pub summary: Option<String>,
    pub species_json: Option<String>,
    pub confidence: Option<f64>,
    pub classification_json: Option<String>,
    pub raw_response: Option<String>,
    pub request_started_at: Timestamp,
    pub request_completed_at: Timestamp,
}

/// Persisted classification result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationRecord {
    pub id: ClassificationId,
    pub image_id: ImageId,
    pub model: String,
    pub prompt_version: String,
    pub contains_wildlife: bool,
    pub is_interesting: bool,
    pub summary: Option<String>,
    pub species_json: Option<String>,
    pub confidence: Option<f64>,
    pub classification_json: Option<String>,
    pub raw_response: Option<String>,
    pub request_started_at: Timestamp,
    pub request_completed_at: Timestamp,
    pub created_at: Timestamp,
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

// ── Lease recovery ─────────────────────────────────────────────────────────

/// Report of how many expired download and processing claims were recovered.
#[derive(Debug, Clone, Default)]
pub struct LeaseRecoveryCounts {
    pub downloads: u64,
    pub processing: u64,
}

// ── Helpers ────────────────────────────────────────────────────────────────

// All timestamp parsing is now fallible via camera_row_to_record and
// cursor_row_to_record.  The old parse_ts_or_panic helper has been removed
// so that malformed timestamps are reported as database errors rather than
// panicking.
