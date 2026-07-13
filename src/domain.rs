//! Explicit domain types for Fauna Scan identifiers, timestamps, and states.
//!
//! These types prevent accidental confusion between different integer/string
//! identifiers and ensure UTC-normalized timestamps throughout the application.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Identifiers ────────────────────────────────────────────────────────────

/// Internal database identifier for a camera.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct CameraId(i64);

impl CameraId {
    /// Create a new camera ID.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Return the raw integer value.
    pub fn get(&self) -> i64 {
        self.0
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// NVR track identifier (e.g. "103" for a picture stream).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TrackId(String);

impl TrackId {
    /// Create a new track ID from a string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the track ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TrackId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

/// Stable unique key for an image, derived from NVR identity, track, time,
/// and canonical playback URI.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ImageKey(String);

impl ImageKey {
    /// Create a new image key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Return the key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ImageKey {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

// ── Timestamp ──────────────────────────────────────────────────────────────

/// UTC-normalized application timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    /// Create a new timestamp from a UTC DateTime.
    pub fn new(dt: DateTime<Utc>) -> Self {
        Self(dt)
    }

    /// Return the inner DateTime<Utc>.
    pub fn as_datetime(&self) -> &DateTime<Utc> {
        &self.0
    }

    /// Consume the timestamp and return the inner DateTime<Utc>.
    pub fn into_inner(self) -> DateTime<Utc> {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.0.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        )
    }
}

impl FromStr for Timestamp {
    type Err = chrono::format::ParseError;

    /// Parse an RFC 3339 timestamp and normalize it to UTC.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dt: DateTime<Utc> = DateTime::parse_from_rfc3339(s)?.into();
        Ok(Self(dt))
    }
}

// ── Download status ────────────────────────────────────────────────────────

/// The download lifecycle state for a discovered image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    /// Discovered but not yet downloaded.
    Pending,
    /// Currently being downloaded.
    Downloading,
    /// Successfully downloaded to disk.
    Downloaded,
    /// Waiting before the next retry attempt.
    RetryWait,
    /// Playback URI is unreachable.
    Unavailable,
    /// Download failed permanently.
    Failed,
}

impl DownloadStatus {
    /// Return the canonical snake_case string for the status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Downloading => "downloading",
            Self::Downloaded => "downloaded",
            Self::RetryWait => "retry_wait",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for DownloadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DownloadStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "downloading" => Ok(Self::Downloading),
            "downloaded" => Ok(Self::Downloaded),
            "retry_wait" => Ok(Self::RetryWait),
            "unavailable" => Ok(Self::Unavailable),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown download status: {other}")),
        }
    }
}

// ── Processing status ──────────────────────────────────────────────────────

/// The classification-processing lifecycle state for a downloaded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingStatus {
    /// Downloaded and ready for classification.
    New,
    /// Currently being classified.
    Processing,
    /// Classification completed successfully.
    Done,
    /// Waiting before the next retry attempt.
    RetryWait,
    /// Classification failed permanently.
    Failed,
    /// Local file is missing; will not be re-downloaded.
    Missing,
}

impl ProcessingStatus {
    /// Return the canonical snake_case string for the status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Processing => "processing",
            Self::Done => "done",
            Self::RetryWait => "retry_wait",
            Self::Failed => "failed",
            Self::Missing => "missing",
        }
    }
}

impl fmt::Display for ProcessingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProcessingStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "new" => Ok(Self::New),
            "processing" => Ok(Self::Processing),
            "done" => Ok(Self::Done),
            "retry_wait" => Ok(Self::RetryWait),
            "failed" => Ok(Self::Failed),
            "missing" => Ok(Self::Missing),
            other => Err(format!("unknown processing status: {other}")),
        }
    }
}

// ── Database row identifiers ──────────────────────────────────────────────

/// Internal database identifier for an image row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ImageId(i64);

impl ImageId {
    /// Create a new image ID.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Return the raw integer value.
    pub fn get(&self) -> i64 {
        self.0
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Internal database identifier for a classification row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClassificationId(i64);

impl ClassificationId {
    /// Create a new classification ID.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Return the raw integer value.
    pub fn get(&self) -> i64 {
        self.0
    }
}

impl fmt::Display for ClassificationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── Download status transitions ────────────────────────────────────────────

impl DownloadStatus {
    /// Return true if this status may legally transition to `next`.
    ///
    /// This encodes the valid lifecycle edges for download states:
    ///
    /// * `pending` → `downloading` (claim)
    /// * `downloading` → `downloaded`, `retry_wait`, `unavailable`, `failed`
    /// * `retry_wait` → `downloading` (retry after delay)
    pub fn can_transition_to(&self, next: DownloadStatus) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Downloading)
                | (Self::Downloading, Self::Downloaded)
                | (Self::Downloading, Self::RetryWait)
                | (Self::Downloading, Self::Unavailable)
                | (Self::Downloading, Self::Failed)
                | (Self::RetryWait, Self::Downloading)
        )
    }
}

// ── Processing status transitions ──────────────────────────────────────────

impl ProcessingStatus {
    /// Return true if this status may legally transition to `next`.
    ///
    /// This encodes the valid lifecycle edges for processing states:
    ///
    /// * `new` → `processing` (claim)
    /// * `processing` → `done`, `retry_wait`, `failed`, `missing`
    /// * `retry_wait` → `processing` (retry after delay)
    pub fn can_transition_to(&self, next: ProcessingStatus) -> bool {
        matches!(
            (self, next),
            (Self::New, Self::Processing)
                | (Self::Processing, Self::Done)
                | (Self::Processing, Self::RetryWait)
                | (Self::Processing, Self::Failed)
                | (Self::Processing, Self::Missing)
                | (Self::RetryWait, Self::Processing)
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    // Identifiers

    #[test]
    fn camera_id_round_trip() {
        let id = CameraId::new(42);
        assert_eq!(id.get(), 42);
        assert_eq!(format!("{id}"), "42");
    }

    #[test]
    fn track_id_round_trip() {
        let id = TrackId::new("103");
        assert_eq!(id.as_str(), "103");
        assert_eq!(format!("{id}"), "103");
        let parsed: TrackId = "203".parse().unwrap();
        assert_eq!(parsed.as_str(), "203");
    }

    #[test]
    fn image_key_round_trip() {
        let key = ImageKey::new("abc123");
        assert_eq!(key.as_str(), "abc123");
        assert_eq!(format!("{key}"), "abc123");
        let parsed: ImageKey = "def456".parse().unwrap();
        assert_eq!(parsed.as_str(), "def456");
    }

    // Timestamp

    #[test]
    fn timestamp_utc_display() {
        let dt = Utc.with_ymd_and_hms(2026, 7, 11, 2, 0, 0).unwrap();
        let ts = Timestamp::new(dt);
        let displayed = format!("{ts}");
        assert!(displayed.ends_with('Z'));
    }

    #[test]
    fn timestamp_from_str_normalizes_offset_to_utc() {
        // +05:30 offset should be normalized to UTC
        let ts: Timestamp = "2026-07-11T07:30:00+05:30".parse().unwrap();
        let utc = ts.as_datetime();
        assert_eq!(utc.hour(), 2);
        assert_eq!(utc.minute(), 0);
    }

    // Serde round-trips for identifiers and timestamp

    #[test]
    fn camera_id_serde_round_trip() {
        let id = CameraId::new(7);
        let json = serde_json::to_string(&id).unwrap();
        let back: CameraId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn track_id_serde_round_trip() {
        let id = TrackId::new("303");
        let json = serde_json::to_string(&id).unwrap();
        let back: TrackId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn image_key_serde_round_trip() {
        let key = ImageKey::new("sha256-hex");
        let json = serde_json::to_string(&key).unwrap();
        let back: ImageKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, back);
    }

    #[test]
    fn timestamp_serde_round_trip() {
        let dt = Utc.with_ymd_and_hms(2026, 7, 11, 2, 30, 45).unwrap();
        let ts = Timestamp::new(dt);
        let json = serde_json::to_string(&ts).unwrap();
        let back: Timestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(ts, back);
    }

    // DownloadStatus

    #[test]
    fn download_status_display_and_parse() {
        let statuses = [
            (DownloadStatus::Pending, "pending"),
            (DownloadStatus::Downloading, "downloading"),
            (DownloadStatus::Downloaded, "downloaded"),
            (DownloadStatus::RetryWait, "retry_wait"),
            (DownloadStatus::Unavailable, "unavailable"),
            (DownloadStatus::Failed, "failed"),
        ];
        for (status, expected) in statuses {
            assert_eq!(status.as_str(), expected);
            assert_eq!(format!("{status}"), expected);
            let parsed = DownloadStatus::from_str(expected).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn download_status_serde_round_trip() {
        let statuses = [
            DownloadStatus::Pending,
            DownloadStatus::Downloading,
            DownloadStatus::Downloaded,
            DownloadStatus::RetryWait,
            DownloadStatus::Unavailable,
            DownloadStatus::Failed,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let back: DownloadStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "round-trip failed for {status}");
        }
    }

    // ProcessingStatus

    #[test]
    fn processing_status_display_and_parse() {
        let statuses = [
            (ProcessingStatus::New, "new"),
            (ProcessingStatus::Processing, "processing"),
            (ProcessingStatus::Done, "done"),
            (ProcessingStatus::RetryWait, "retry_wait"),
            (ProcessingStatus::Failed, "failed"),
            (ProcessingStatus::Missing, "missing"),
        ];
        for (status, expected) in statuses {
            assert_eq!(status.as_str(), expected);
            assert_eq!(format!("{status}"), expected);
            let parsed = ProcessingStatus::from_str(expected).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn processing_status_serde_round_trip() {
        let statuses = [
            ProcessingStatus::New,
            ProcessingStatus::Processing,
            ProcessingStatus::Done,
            ProcessingStatus::RetryWait,
            ProcessingStatus::Failed,
            ProcessingStatus::Missing,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let back: ProcessingStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "round-trip failed for {status}");
        }
    }

    // ── ImageId and ClassificationId ──────────────────────────────────────

    #[test]
    fn image_id_round_trip() {
        let id = ImageId::new(99);
        assert_eq!(id.get(), 99);
        assert_eq!(format!("{id}"), "99");
    }

    #[test]
    fn classification_id_round_trip() {
        let id = ClassificationId::new(42);
        assert_eq!(id.get(), 42);
        assert_eq!(format!("{id}"), "42");
    }

    #[test]
    fn image_id_serde_round_trip() {
        let id = ImageId::new(7);
        let json = serde_json::to_string(&id).unwrap();
        let back: ImageId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn classification_id_serde_round_trip() {
        let id = ClassificationId::new(13);
        let json = serde_json::to_string(&id).unwrap();
        let back: ClassificationId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    // ── DownloadStatus transition validation ──────────────────────────────

    #[test]
    fn download_can_claim_pending() {
        assert!(DownloadStatus::Pending.can_transition_to(DownloadStatus::Downloading));
    }

    #[test]
    fn download_can_complete_downloading() {
        assert!(DownloadStatus::Downloading.can_transition_to(DownloadStatus::Downloaded));
    }

    #[test]
    fn download_can_retry_from_downloading() {
        assert!(DownloadStatus::Downloading.can_transition_to(DownloadStatus::RetryWait));
    }

    #[test]
    fn download_can_fail_from_downloading() {
        assert!(DownloadStatus::Downloading.can_transition_to(DownloadStatus::Failed));
        assert!(DownloadStatus::Downloading.can_transition_to(DownloadStatus::Unavailable));
    }

    #[test]
    fn download_can_retry_after_wait() {
        assert!(DownloadStatus::RetryWait.can_transition_to(DownloadStatus::Downloading));
    }

    #[test]
    fn download_cannot_go_downloaded_to_pending() {
        assert!(!DownloadStatus::Downloaded.can_transition_to(DownloadStatus::Pending));
        assert!(!DownloadStatus::Downloaded.can_transition_to(DownloadStatus::Downloading));
    }

    #[test]
    fn download_cannot_go_failed_to_downloading() {
        assert!(!DownloadStatus::Failed.can_transition_to(DownloadStatus::Downloading));
        assert!(!DownloadStatus::Unavailable.can_transition_to(DownloadStatus::Downloading));
    }

    #[test]
    fn download_cannot_go_pending_to_downloaded() {
        assert!(!DownloadStatus::Pending.can_transition_to(DownloadStatus::Downloaded));
    }

    #[test]
    fn download_cannot_go_downloading_to_pending() {
        assert!(!DownloadStatus::Downloading.can_transition_to(DownloadStatus::Pending));
    }

    // Exhaustive download transition matrix
    #[test]
    fn download_exhaustive_transition_matrix() {
        let all: [DownloadStatus; 6] = [
            DownloadStatus::Pending,
            DownloadStatus::Downloading,
            DownloadStatus::Downloaded,
            DownloadStatus::RetryWait,
            DownloadStatus::Unavailable,
            DownloadStatus::Failed,
        ];
        for from in &all {
            for to in &all {
                let allowed = from.can_transition_to(*to);
                let expected = matches!(
                    (from, to),
                    (DownloadStatus::Pending, DownloadStatus::Downloading)
                        | (DownloadStatus::Downloading, DownloadStatus::Downloaded)
                        | (DownloadStatus::Downloading, DownloadStatus::RetryWait)
                        | (DownloadStatus::Downloading, DownloadStatus::Unavailable)
                        | (DownloadStatus::Downloading, DownloadStatus::Failed)
                        | (DownloadStatus::RetryWait, DownloadStatus::Downloading)
                );
                assert_eq!(allowed, expected, "transition {from} -> {to}");
            }
        }
    }

    // ── ProcessingStatus transition validation ───────────────────────────

    #[test]
    fn processing_can_claim_new() {
        assert!(ProcessingStatus::New.can_transition_to(ProcessingStatus::Processing));
    }

    #[test]
    fn processing_can_complete_processing() {
        assert!(ProcessingStatus::Processing.can_transition_to(ProcessingStatus::Done));
    }

    #[test]
    fn processing_can_retry_from_processing() {
        assert!(ProcessingStatus::Processing.can_transition_to(ProcessingStatus::RetryWait));
    }

    #[test]
    fn processing_can_fail_from_processing() {
        assert!(ProcessingStatus::Processing.can_transition_to(ProcessingStatus::Failed));
        assert!(ProcessingStatus::Processing.can_transition_to(ProcessingStatus::Missing));
    }

    #[test]
    fn processing_can_retry_after_wait() {
        assert!(ProcessingStatus::RetryWait.can_transition_to(ProcessingStatus::Processing));
    }

    #[test]
    fn processing_cannot_go_done_to_processing() {
        assert!(!ProcessingStatus::Done.can_transition_to(ProcessingStatus::Processing));
    }

    #[test]
    fn processing_cannot_go_failed_to_processing() {
        assert!(!ProcessingStatus::Failed.can_transition_to(ProcessingStatus::Processing));
    }

    #[test]
    fn processing_cannot_go_new_to_done() {
        assert!(!ProcessingStatus::New.can_transition_to(ProcessingStatus::Done));
    }

    // Exhaustive processing transition matrix
    #[test]
    fn processing_exhaustive_transition_matrix() {
        let all: [ProcessingStatus; 6] = [
            ProcessingStatus::New,
            ProcessingStatus::Processing,
            ProcessingStatus::Done,
            ProcessingStatus::RetryWait,
            ProcessingStatus::Failed,
            ProcessingStatus::Missing,
        ];
        for from in &all {
            for to in &all {
                let allowed = from.can_transition_to(*to);
                let expected = matches!(
                    (from, to),
                    (ProcessingStatus::New, ProcessingStatus::Processing)
                        | (ProcessingStatus::Processing, ProcessingStatus::Done)
                        | (ProcessingStatus::Processing, ProcessingStatus::RetryWait)
                        | (ProcessingStatus::Processing, ProcessingStatus::Failed)
                        | (ProcessingStatus::Processing, ProcessingStatus::Missing)
                        | (ProcessingStatus::RetryWait, ProcessingStatus::Processing)
                );
                assert_eq!(allowed, expected, "transition {from} -> {to}");
            }
        }
    }
}
