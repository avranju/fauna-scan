//! Typed web query criteria and result DTOs for the data abstraction.
//!
//! These types carry validated filter parameters and web-facing record data
//! between the web layer and the backend-neutral DataStore trait.  They
//! intentionally contain no HTTP types, no SQLx types, and no raw classifier
//! response bodies.

use chrono::Utc;
use serde::Serialize;

use crate::domain::Timestamp;

/// Scope filters for web image queries.
#[derive(Debug, Clone)]
pub struct WebScopeFilter {
    /// Lower bound (inclusive) on capture_start_at.
    pub from: Option<Timestamp>,
    /// Upper bound (exclusive) on capture_start_at.
    pub to: Option<Timestamp>,
    /// Camera IDs to include (empty means all cameras).
    pub camera_ids: Vec<i64>,
}

/// Download status filter for web queries.
#[derive(Debug, Clone)]
pub enum WebDownloadStatusFilter {
    /// Filter to one or more specific statuses.
    OneOf(Vec<String>),
}

/// Processing status filter for web queries.
#[derive(Debug, Clone)]
pub enum WebProcessingStatusFilter {
    /// Filter to one or more specific statuses.
    OneOf(Vec<String>),
}

/// Classification flag filter.
#[derive(Debug, Clone)]
pub enum WebClassifiedFilter {
    Classified,
    NotClassified,
}

/// Web image filter criteria.
#[derive(Debug, Clone)]
pub struct WebImageFilter {
    pub scope: WebScopeFilter,
    pub download_status: Option<WebDownloadStatusFilter>,
    pub processing_status: Option<WebProcessingStatusFilter>,
    pub classified: Option<WebClassifiedFilter>,
    pub contains_wildlife: Option<bool>,
    pub is_interesting: Option<bool>,
    pub confidence_min: Option<f64>,
}

/// Ordering direction for web image queries.
#[derive(Debug, Clone)]
pub enum WebImageOrder {
    CapturedAscending,
    CapturedDescending,
}

/// Paginated web image query with filters and cursor.
#[derive(Debug, Clone)]
pub struct WebImageQuery {
    pub filter: WebImageFilter,
    pub order: WebImageOrder,
    pub limit: u32,
    /// (capture_start_at, id) cursor for keyset pagination.
    pub cursor: Option<(String, i64)>,
}

/// Health snapshot returned by the health API.
#[derive(Debug, Clone, Serialize)]
pub struct WebHealthSnapshot {
    pub status: &'static str,
    pub version: &'static str,
    pub web_started_at: String,
    pub last_camera_discovery: Option<String>,
    pub last_downloader_poll: Option<String>,
    pub last_scanner_pass: Option<String>,
    pub active_downloads: i64,
    pub active_classifications: i64,
    pub generated_at: String,
}

/// Camera record for the cameras API.
#[derive(Debug, Clone, Serialize)]
pub struct WebCameraRecord {
    pub id: i64,
    pub channel_number: i64,
    pub name: Option<String>,
    pub enabled: bool,
    pub primary_track_id: String,
    pub picture_track_id: String,
    pub last_seen_at: String,
    pub last_completed_window_end: Option<String>,
    pub last_poll_at: Option<String>,
    pub next_search_at: Option<String>,
    pub last_error: Option<String>,
}

/// Classification summary for the image list API.
#[derive(Debug, Clone, Serialize)]
pub struct WebClassificationSummary {
    pub id: i64,
    pub contains_wildlife: bool,
    pub is_interesting: bool,
    pub summary: Option<String>,
    pub species: serde_json::Value,
    pub confidence: Option<f64>,
    pub model: String,
    pub prompt_version: String,
    pub completed_at: String,
}

/// Camera summary for the image list API.
#[derive(Debug, Clone, Serialize)]
pub struct WebCameraSummary {
    pub id: i64,
    pub name: Option<String>,
    pub channel: i64,
}

/// Image summary record for the image list API.
#[derive(Debug, Clone, Serialize)]
pub struct WebImageSummaryRecord {
    pub id: i64,
    pub captured_at: String,
    pub capture_end_at: Option<String>,
    pub camera: WebCameraSummary,
    pub content_url: Option<String>,
    pub thumbnail_url: Option<String>,
    pub download_status: String,
    pub processing_status: String,
    pub classification: Option<WebClassificationSummary>,
}

/// Overview counts for the overview API.
#[derive(Debug, Clone, Serialize)]
pub struct WebOverviewRecord {
    pub discovered: i64,
    pub downloaded: i64,
    pub classified: i64,
    pub wildlife: i64,
    pub interesting: i64,
    pub retryable_failures: i64,
    pub permanent_failures: i64,
}

/// Classification record for the detail API (without raw_response).
#[derive(Debug, Clone, Serialize)]
pub struct WebClassificationDetail {
    pub id: i64,
    pub model: String,
    pub prompt_version: String,
    pub contains_wildlife: bool,
    pub is_interesting: bool,
    pub summary: Option<String>,
    pub species: serde_json::Value,
    /// Normalized animal boxes; absent for historical classifications.
    pub bounding_boxes: Option<serde_json::Value>,
    pub confidence: Option<f64>,
    pub structured: serde_json::Value,
    pub request_started_at: String,
    pub request_completed_at: String,
    pub created_at: String,
}

/// Image detail record for the detail API.
#[derive(Debug, Clone, Serialize)]
pub struct WebImageDetailRecord {
    pub id: i64,
    pub image_key: String,
    pub captured_at: String,
    pub capture_end_at: Option<String>,
    pub discovered_at: String,
    pub camera: WebDetailCamera,
    pub content_url: Option<String>,
    pub download: WebDownloadDetail,
    pub processing: WebProcessingDetail,
    pub nvr: WebNvrUrls,
    pub classifications: Vec<WebClassificationDetail>,
}

/// Camera detail for the image detail API.
#[derive(Debug, Clone, Serialize)]
pub struct WebDetailCamera {
    pub id: i64,
    pub channel: i64,
    pub name: Option<String>,
    pub primary_track_id: String,
    pub picture_track_id: String,
}

/// Download detail for the image detail API.
#[derive(Debug, Clone, Serialize)]
pub struct WebDownloadDetail {
    pub status: String,
    pub attempts: i64,
    pub downloaded_at: Option<String>,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<String>,
    pub lease_until: Option<String>,
}

/// Processing detail for the image detail API.
#[derive(Debug, Clone, Serialize)]
pub struct WebProcessingDetail {
    pub status: String,
    pub attempts: i64,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<String>,
    pub lease_until: Option<String>,
}

/// NVR URLs for the image detail API.
#[derive(Debug, Clone, Serialize)]
pub struct WebNvrUrls {
    pub image_url: Option<String>,
    pub reported_image_url: Option<String>,
}

/// Image content lookup result.
#[derive(Debug, Clone)]
pub struct WebImageContentLookup {
    pub local_path: Option<String>,
    pub download_status: String,
    /// Boxes from the newest classification, if that classification has them.
    pub bounding_boxes_json: Option<String>,
}

/// Recording target lookup result.
#[derive(Debug, Clone, Serialize)]
pub struct WebRecordingTarget {
    pub capture_start_at: String,
    pub primary_track_id: String,
}

/// Activity count record.
#[derive(Debug, Clone, Serialize)]
pub struct WebActivityCount {
    pub category: String,
    pub status: String,
    pub count: i64,
}

/// Active work record for the activity API.
#[derive(Debug, Clone, Serialize)]
pub struct WebActivityActive {
    pub id: i64,
    pub captured_at: String,
    pub camera_name: Option<String>,
    pub channel: i64,
    pub download_status: String,
    pub download_attempts: i64,
    pub download_lease_until: Option<String>,
    pub processing_status: String,
    pub processing_attempts: i64,
    pub processing_started_at: Option<String>,
    pub processing_lease_until: Option<String>,
}

/// Activity response data.
#[derive(Debug, Clone, Serialize)]
pub struct WebActivityRecord {
    pub counts: Vec<WebActivityCount>,
    pub active: Vec<WebActivityActive>,
    pub generated_at: String,
}

impl WebHealthSnapshot {
    /// Create a health snapshot with the current timestamp.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        last_camera_discovery: Option<String>,
        last_downloader_poll: Option<String>,
        last_scanner_pass: Option<String>,
        active_downloads: i64,
        active_classifications: i64,
        web_started_at: &Timestamp,
    ) -> Self {
        let now = Timestamp::new(Utc::now());
        Self {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            web_started_at: web_started_at.to_string(),
            last_camera_discovery,
            last_downloader_poll,
            last_scanner_pass,
            active_downloads,
            active_classifications,
            generated_at: now.to_string(),
        }
    }
}

impl WebCameraRecord {
    /// Create a camera record from raw fields.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        channel_number: i64,
        name: Option<String>,
        enabled: bool,
        primary_track_id: String,
        picture_track_id: String,
        last_seen_at: String,
        last_completed_window_end: Option<String>,
        last_poll_at: Option<String>,
        next_search_at: Option<String>,
        last_error: Option<String>,
    ) -> Self {
        Self {
            id,
            channel_number,
            name,
            enabled,
            primary_track_id,
            picture_track_id,
            last_seen_at,
            last_completed_window_end,
            last_poll_at,
            next_search_at,
            last_error,
        }
    }
}

impl WebImageSummaryRecord {
    /// Create an image summary record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        captured_at: String,
        capture_end_at: Option<String>,
        camera: WebCameraSummary,
        local_path: Option<String>,
        download_status: String,
        processing_status: String,
        classification: Option<WebClassificationSummary>,
    ) -> Self {
        let has_content = download_status == "downloaded" && local_path.is_some();
        Self {
            id,
            captured_at,
            capture_end_at,
            camera,
            content_url: has_content.then(|| format!("/api/v1/images/{id}/content")),
            thumbnail_url: has_content.then(|| format!("/api/v1/images/{id}/thumbnail")),
            download_status,
            processing_status,
            classification,
        }
    }
}

impl WebClassificationSummary {
    /// Create a classification summary.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        contains_wildlife: bool,
        is_interesting: bool,
        summary: Option<String>,
        species_json: Option<String>,
        default_species: serde_json::Value,
        confidence: Option<f64>,
        model: String,
        prompt_version: String,
        completed_at: String,
    ) -> Self {
        let species = species_json
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or(default_species);
        Self {
            id,
            contains_wildlife,
            is_interesting,
            summary,
            species,
            confidence,
            model,
            prompt_version,
            completed_at,
        }
    }
}

impl WebImageDetailRecord {
    /// Create an image detail record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        image_key: String,
        captured_at: String,
        capture_end_at: Option<String>,
        discovered_at: String,
        camera: WebDetailCamera,
        local_path: Option<String>,
        download_status: String,
        download_attempts: i64,
        downloaded_at: Option<String>,
        download_last_error: Option<String>,
        download_next_attempt_at: Option<String>,
        download_lease_until: Option<String>,
        processing_status: String,
        processing_attempts: i64,
        processing_started_at: Option<String>,
        processing_completed_at: Option<String>,
        processing_last_error: Option<String>,
        processing_next_attempt_at: Option<String>,
        processing_lease_until: Option<String>,
        canonical_nvr_url: Option<String>,
        reported_nvr_url: Option<String>,
        classifications: Vec<WebClassificationDetail>,
    ) -> Self {
        let content_url = (download_status == "downloaded" && local_path.is_some())
            .then(|| format!("/api/v1/images/{id}/content"));
        Self {
            id,
            image_key,
            captured_at,
            capture_end_at,
            discovered_at,
            camera,
            content_url,
            download: WebDownloadDetail {
                status: download_status,
                attempts: download_attempts,
                downloaded_at,
                last_error: download_last_error,
                next_attempt_at: download_next_attempt_at,
                lease_until: download_lease_until,
            },
            processing: WebProcessingDetail {
                status: processing_status,
                attempts: processing_attempts,
                started_at: processing_started_at,
                completed_at: processing_completed_at,
                last_error: processing_last_error,
                next_attempt_at: processing_next_attempt_at,
                lease_until: processing_lease_until,
            },
            nvr: WebNvrUrls {
                image_url: canonical_nvr_url,
                reported_image_url: reported_nvr_url,
            },
            classifications,
        }
    }
}

impl WebDetailCamera {
    /// Create a detail camera record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        channel: i64,
        name: Option<String>,
        primary_track_id: String,
        picture_track_id: String,
    ) -> Self {
        Self {
            id,
            channel,
            name,
            primary_track_id,
            picture_track_id,
        }
    }
}

impl WebClassificationDetail {
    /// Create a classification detail without raw_response.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        model: String,
        prompt_version: String,
        contains_wildlife: bool,
        is_interesting: bool,
        summary: Option<String>,
        species_json: Option<String>,
        default_species: serde_json::Value,
        confidence: Option<f64>,
        classification_json: Option<String>,
        default_structured: serde_json::Value,
        request_started_at: String,
        request_completed_at: String,
        created_at: String,
        bounding_boxes_json: Option<String>,
    ) -> Self {
        let species = species_json
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or(default_species);
        let structured = classification_json
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or(default_structured);
        let bounding_boxes = bounding_boxes_json.and_then(|v| serde_json::from_str(&v).ok());
        Self {
            id,
            model,
            prompt_version,
            contains_wildlife,
            is_interesting,
            summary,
            species,
            bounding_boxes,
            confidence,
            structured,
            request_started_at,
            request_completed_at,
            created_at,
        }
    }
}

impl WebRecordingTarget {
    /// Create a recording target.
    #[allow(clippy::too_many_arguments)]
    pub fn new(capture_start_at: String, primary_track_id: String) -> Self {
        Self {
            capture_start_at,
            primary_track_id,
        }
    }
}
