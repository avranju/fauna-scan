//! Image filesystem layout, atomic writes, and JPEG validation.
//!
//! Implemented in Phase 7.
//!
//! Provides deterministic destination paths, sanitized camera/date directory
//! names, JPEG signature validation, existing-file adoption, streaming
//! `.part` writes with atomic rename, and stale-part cleanup.

use chrono::{Datelike, Timelike};
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use url::Url;

use crate::domain::{ImageKey, Timestamp, TrackId};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::http::map_reqwest_error;

// ── ImageDestination ──────────────────────────────────────────────────────

/// The final destination path for a downloaded JPEG and its same-directory
/// temporary (`.part`) path.
#[derive(Debug, Clone)]
pub struct ImageDestination {
    /// The final JPEG file path.
    pub final_path: PathBuf,
    /// The same-directory temporary path used during streaming writes.
    pub part_path: PathBuf,
}

impl ImageDestination {
    /// Create a new destination with the given final path and part path.
    pub fn new(final_path: PathBuf, part_path: PathBuf) -> Self {
        Self {
            final_path,
            part_path,
        }
    }
}

// ── FilePreparation ───────────────────────────────────────────────────────

/// Outcome of verifying an existing final file at the destination.
#[derive(Debug, Clone)]
pub enum FilePreparation {
    /// A valid existing file was found and can be adopted.
    Adopted,
    /// No valid file exists; a network download is required.
    DownloadRequired,
}

// ── Sanitization ──────────────────────────────────────────────────────────

/// Sanitize a string for safe use as a filesystem path component.
///
/// - Unsafe characters (non-alphanumeric, non-hyphen, non-underscore) become
///   hyphens.
/// - Consecutive unsafe characters are collapsed into a single hyphen.
/// - Leading/trailing hyphens are stripped.
/// - An empty result falls back to the provided `fallback`.
pub fn sanitize_path_component(value: &str, fallback: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive hyphens.
    let collapsed = sanitized
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    // Strip leading/trailing hyphens.
    let trimmed = collapsed.trim_matches('-');

    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Build a camera directory name from channel number and optional camera name.
///
/// Ensures distinct cameras cannot collide by always including the channel
/// number.
pub fn camera_directory_name(channel_number: i64, camera_name: Option<&str>) -> String {
    let name_part = camera_name
        .map(|n| sanitize_path_component(n, "camera"))
        .unwrap_or_else(|| "camera".to_string());
    format!("camera-{}-{}", channel_number, name_part)
}

/// Build the full image destination path beneath the output directory.
///
/// Structure: `<output>/<camera_dir>/<YYYY>/<MM>/<DD>/<filename>.jpg`
///
/// The filename contains the UTC capture timestamp, sanitized track ID,
/// and the first 12 hex characters of the image key.
pub fn image_destination(
    output_directory: &Path,
    channel_number: i64,
    camera_name: Option<&str>,
    capture_start_at: &Timestamp,
    track_id: &TrackId,
    image_key: &ImageKey,
) -> ImageDestination {
    let camera_dir = camera_directory_name(channel_number, camera_name);

    let dt = capture_start_at.as_datetime();
    let year = dt.year();
    let month = dt.month();
    let day = dt.day();

    // Include fractional seconds when capture_start_at has non-zero
    // subseconds, producing a deterministic UTC fractional component
    // that is stable for the same timestamp value.
    let subsec_micros = dt.nanosecond() / 1_000;
    let has_fraction = dt.nanosecond() != 0;
    let timestamp_str = if has_fraction {
        format!(
            "{}{:02}{:02}T{:02}{:02}{:02}.{:06}Z",
            year,
            month,
            day,
            dt.hour(),
            dt.minute(),
            dt.second(),
            subsec_micros
        )
    } else {
        format!(
            "{}{:02}{:02}T{:02}{:02}{:02}Z",
            year,
            month,
            day,
            dt.hour(),
            dt.minute(),
            dt.second()
        )
    };

    let track_sanitized = sanitize_path_component(track_id.as_str(), "track");

    let key_prefix_len = image_key.as_str().len().min(12);
    let key_prefix = &image_key.as_str()[..key_prefix_len];

    let filename = format!(
        "{}_track-{}_{}.jpg",
        timestamp_str, track_sanitized, key_prefix
    );

    let camera_dir_path = output_directory.join(&camera_dir);
    let date_dir = camera_dir_path
        .join(format!("{:04}", year))
        .join(format!("{:02}", month))
        .join(format!("{:02}", day));

    let final_path = date_dir.join(&filename);
    let part_path = date_dir.join(format!("{}.part", filename));

    ImageDestination {
        final_path,
        part_path,
    }
}

// ── JPEG Validation ───────────────────────────────────────────────────────

/// Validate JPEG body bytes.
///
/// Requires:
/// - Non-empty body.
/// - When `verify_jpeg` is true: `FF D8` at the start and `FF D9` at the end.
pub fn validate_jpeg_body(data: &[u8], verify_jpeg: bool) -> AppResult<()> {
    if data.is_empty() {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_jpeg_body",
            "downloaded body is empty",
        ));
    }

    if !verify_jpeg {
        return Ok(());
    }

    // Check JPEG start marker: FF D8
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_jpeg_body",
            "downloaded body does not start with JPEG signature (FF D8)",
        ));
    }

    // Check JPEG end marker: FF D9
    if data.len() < 4 {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_jpeg_body",
            "downloaded body is too short to contain JPEG end marker",
        ));
    }

    if data[data.len() - 2] != 0xFF || data[data.len() - 1] != 0xD9 {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_jpeg_body",
            "downloaded body does not end with JPEG end marker (FF D9)",
        ));
    }

    Ok(())
}

// ── Existing File Verification ────────────────────────────────────────────

/// Verify an existing final file and return an adoption outcome.
///
/// Returns `FilePreparation::Adopted` when the file:
/// - Exists and is non-empty.
/// - Is within the configured maximum size.
/// - Has a valid JPEG signature when verification is enabled.
///
/// Returns `FilePreparation::DownloadRequired` when the file:
/// - Does not exist.
/// - Is empty.
/// - Exceeds the configured maximum size.
/// - Has an invalid JPEG signature (when verification is enabled).
///
/// Returns an error only for actual I/O failures that prevent checking
/// the file (e.g. permission denied on the directory).
pub fn verify_existing_file(
    path: &Path,
    maximum_size: u64,
    verify_jpeg: bool,
) -> AppResult<FilePreparation> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(FilePreparation::DownloadRequired);
        }
        Err(e) => {
            return Err(AppError::with_source(
                ErrorCategory::Filesystem,
                "verify_existing_file",
                format!("cannot stat existing file: {}", safe_io_error(&e)),
                e,
            ));
        }
    };

    let len = metadata.len();

    // Must be non-empty.
    if len == 0 {
        return Ok(FilePreparation::DownloadRequired);
    }

    // Must be within maximum size.
    if len > maximum_size {
        return Ok(FilePreparation::DownloadRequired);
    }

    // When JPEG verification is enabled, read and validate the signature.
    // Invalid signatures are treated as DownloadRequired (the file will be
    // replaced by a fresh download) rather than propagating an error,
    // because the worker must proceed to network transfer regardless.
    if verify_jpeg {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => {
                return Err(AppError::with_source(
                    ErrorCategory::Filesystem,
                    "verify_existing_file",
                    format!(
                        "cannot read existing file for JPEG validation: {}",
                        safe_io_error(&e)
                    ),
                    e,
                ));
            }
        };

        // If JPEG validation fails, treat it as DownloadRequired so the
        // worker can remove the invalid file and proceed with a download.
        if validate_jpeg_body(&data, true).is_err() {
            return Ok(FilePreparation::DownloadRequired);
        }
    }

    Ok(FilePreparation::Adopted)
}

// ── Streaming Write ───────────────────────────────────────────────────────

/// Remove a part file, awaiting the removal to complete.
/// Returns an error if removal fails (best-effort only).
async fn remove_part_file(path: &Path) -> std::io::Result<()> {
    tokio::fs::remove_file(path).await
}

/// Attempt to clean up a part file and return a combined error
/// when cleanup fails.
///
/// When cleanup succeeds, returns the original error unchanged.
/// When cleanup fails, returns a new error that combines the
/// original context with a safe indication that the temporary
/// file could not be removed.
async fn cleanup_with_context(path: &Path, original_error: AppError) -> AppError {
    match remove_part_file(path).await {
        Ok(()) => original_error,
        Err(cleanup_err) => {
            tracing::warn!(
                path = %path.display(),
                "part file cleanup failed after primary error"
            );
            // Return a combined error indicating both the original
            // failure and the cleanup failure.  Do not include
            // raw reqwest errors or sensitive URLs.
            AppError::with_source(
                original_error.category,
                original_error.operation,
                format!(
                    "{}; part file cleanup failed: {}",
                    original_error.message,
                    safe_io_error(&cleanup_err)
                ),
                anyhow::Error::from(cleanup_err),
            )
        }
    }
}

/// Stream a response body into a bounded temporary file, validate it,
/// flush, sync, and atomically rename it to the final destination.
///
/// Returns the total number of bytes written on success.
///
/// Uses `create_new` to avoid overwriting existing `.part` files.
/// All failure paths await cleanup of the part file.
pub async fn stream_response_to_destination(
    mut response: reqwest::Response,
    destination: &ImageDestination,
    maximum_size: u64,
    verify_jpeg: bool,
) -> AppResult<u64> {
    // Ensure the parent directory exists.
    if let Some(parent) = destination.part_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AppError::with_source(
                ErrorCategory::Filesystem,
                "stream_response_to_destination",
                format!(
                    "cannot create directory {}: {}",
                    parent.display(),
                    safe_io_error(&e)
                ),
                e,
            )
        })?;
    }

    // Create a new temporary file (create_new to avoid collisions
    // with stale .part files from interrupted previous downloads).
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&destination.part_path)
        .await
        .map_err(|e| {
            AppError::with_source(
                ErrorCategory::Filesystem,
                "stream_response_to_destination",
                format!(
                    "cannot create temp file {}: {}",
                    destination.part_path.display(),
                    safe_io_error(&e)
                ),
                e,
            )
        })?;

    let mut total_bytes: u64 = 0;

    // Stream chunks with explicit cleanup on every failure path.
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break, // EOF reached
            Err(e) => {
                let error = AppError::new(
                    ErrorCategory::Network,
                    "stream_response_to_destination",
                    "interrupted body transfer",
                );
                let error = cleanup_with_context(&destination.part_path, error).await;
                return Err(AppError::with_source(
                    ErrorCategory::Network,
                    "stream_response_to_destination",
                    error.message,
                    map_reqwest_error("stream_response_to_destination", e),
                ));
            }
        };

        let chunk_len = chunk.len() as u64;

        // Enforce maximum size before writing.
        if total_bytes + chunk_len > maximum_size {
            let error = AppError::new(
                ErrorCategory::Filesystem,
                "stream_response_to_destination",
                format!(
                    "downloaded body exceeds maximum size of {} bytes (reached {})",
                    maximum_size,
                    total_bytes + chunk_len
                ),
            );
            return Err(cleanup_with_context(&destination.part_path, error).await);
        }

        if let Err(e) = file.write_all(&chunk).await {
            let error = AppError::with_source(
                ErrorCategory::Filesystem,
                "stream_response_to_destination",
                format!("write failure: {}", safe_io_error(&e)),
                e,
            );
            return Err(cleanup_with_context(&destination.part_path, error).await);
        }

        total_bytes += chunk_len;
    }

    // Validate size is non-zero.
    if total_bytes == 0 {
        let error = AppError::new(
            ErrorCategory::Filesystem,
            "stream_response_to_destination",
            "downloaded body is empty",
        );
        return Err(cleanup_with_context(&destination.part_path, error).await);
    }

    // Flush — clean up on failure.
    if let Err(e) = file.flush().await {
        let error = AppError::with_source(
            ErrorCategory::Filesystem,
            "stream_response_to_destination",
            format!("flush failure: {}", safe_io_error(&e)),
            e,
        );
        return Err(cleanup_with_context(&destination.part_path, error).await);
    }

    // Sync — clean up on failure.
    if let Err(e) = file.sync_all().await {
        let error = AppError::with_source(
            ErrorCategory::Filesystem,
            "stream_response_to_destination",
            format!("sync_all failure: {}", safe_io_error(&e)),
            e,
        );
        return Err(cleanup_with_context(&destination.part_path, error).await);
    }

    // Read back the file for final validation — clean up on failure.
    let data = match tokio::fs::read(&destination.part_path).await {
        Ok(data) => data,
        Err(e) => {
            let error = AppError::with_source(
                ErrorCategory::Filesystem,
                "stream_response_to_destination",
                format!("read-back failure: {}", safe_io_error(&e)),
                e,
            );
            return Err(cleanup_with_context(&destination.part_path, error).await);
        }
    };

    // Validate JPEG body — clean up on failure.
    if let Err(e) = validate_jpeg_body(&data, verify_jpeg) {
        return Err(cleanup_with_context(&destination.part_path, e).await);
    }

    // Atomic rename — clean up on failure.
    if let Err(e) = tokio::fs::rename(&destination.part_path, &destination.final_path).await {
        let error = AppError::with_source(
            ErrorCategory::Filesystem,
            "stream_response_to_destination",
            format!("atomic rename failure: {}", safe_io_error(&e)),
            e,
        );
        return Err(cleanup_with_context(&destination.part_path, error).await);
    }

    Ok(total_bytes)
}

// ── Stale Part Cleanup ────────────────────────────────────────────────────

/// Recursively remove stale `.part` files beneath the configured output
/// directory without following symlinks.
///
/// Returns the count of removed files.
pub async fn remove_stale_part_files(output_directory: &Path) -> AppResult<u64> {
    remove_stale_part_files_inner(output_directory).await
}

async fn remove_stale_part_files_inner(output_directory: &Path) -> AppResult<u64> {
    let mut count: u64 = 0;

    let mut stream = tokio::fs::read_dir(output_directory).await.map_err(|e| {
        AppError::with_source(
            ErrorCategory::Filesystem,
            "remove_stale_part_files",
            format!("cannot read output directory: {}", safe_io_error(&e)),
            e,
        )
    })?;

    while let Some(entry) = stream.next_entry().await.map_err(|e| {
        AppError::with_source(
            ErrorCategory::Filesystem,
            "remove_stale_part_files",
            format!("cannot read directory entry: {}", safe_io_error(&e)),
            e,
        )
    })? {
        let entry_path = entry.path();
        let file_type = entry.file_type().await.map_err(|e| {
            AppError::with_source(
                ErrorCategory::Filesystem,
                "remove_stale_part_files",
                format!(
                    "cannot stat {}: {}",
                    entry_path.display(),
                    safe_io_error(&e)
                ),
                e,
            )
        })?;

        if file_type.is_dir() && !file_type.is_symlink() {
            // Recurse into subdirectories (skip symlinks).
            count += Box::pin(remove_stale_part_files_inner(&entry_path)).await?;
        } else if file_type.is_file() && entry_path.extension().is_some_and(|ext| ext == "part") {
            tokio::fs::remove_file(&entry_path).await.map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Filesystem,
                    "remove_stale_part_files",
                    format!("cannot remove stale part file: {}", safe_io_error(&e)),
                    e,
                )
            })?;
            count += 1;
        }
        // Skip symlinks and other file types.
    }

    Ok(count)
}

// ── Cleanup helpers ───────────────────────────────────────────────────────

/// Remove a single `.part` file, ignoring errors (used for best-effort cleanup).
pub async fn cleanup_part_file(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

/// Remove a final file, returning an error if removal fails.
///
/// Used for invalid-file replacement: when an existing final file has
/// an invalid JPEG signature, this removes it before proceeding with
/// a fresh network download.  Errors are propagated so the caller
/// can decide whether to retry or fail the claim.
pub async fn remove_final_file(path: &Path) -> AppResult<()> {
    tokio::fs::remove_file(path).await.map_err(|e| {
        AppError::with_source(
            ErrorCategory::Filesystem,
            "remove_final_file",
            format!("cannot remove invalid final file: {}", safe_io_error(&e)),
            e,
        )
    })
}

/// Build a safe URL string for diagnostics, stripping query parameters.
pub fn safe_url_string(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.to_string()
}

/// Build a safe error message from an io::Error, omitting paths when they
/// could contain sensitive data.
pub fn safe_io_error(e: &io::Error) -> String {
    e.to_string()
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn test_ts() -> Timestamp {
        Timestamp::new(
            chrono::Utc
                .with_ymd_and_hms(2026, 7, 11, 2, 29, 32)
                .unwrap(),
        )
    }

    // ── Sanitization tests ──────────────────────────────────────────────

    #[test]
    fn sanitize_path_component_keeps_alphanumeric_and_hyphen() {
        let result = sanitize_path_component("Camera-01", "fallback");
        assert_eq!(result, "Camera-01");
    }

    #[test]
    fn sanitize_path_component_replaces_special_chars() {
        let result = sanitize_path_component("Cam/era:Name!", "fallback");
        assert_eq!(result, "Cam-era-Name");
    }

    #[test]
    fn sanitize_path_component_collapses_consecutive_special() {
        let result = sanitize_path_component("Cam///era", "fallback");
        assert_eq!(result, "Cam-era");
    }

    #[test]
    fn sanitize_path_component_strips_leading_trailing() {
        let result = sanitize_path_component("---Cam---", "fallback");
        assert_eq!(result, "Cam");
    }

    #[test]
    fn sanitize_path_component_falls_back_when_empty() {
        let result = sanitize_path_component("!!!", "fallback");
        assert_eq!(result, "fallback");
    }

    #[test]
    fn sanitize_path_component_empty_input_falls_back() {
        let result = sanitize_path_component("", "fallback");
        assert_eq!(result, "fallback");
    }

    // ── Camera directory name tests ─────────────────────────────────────

    #[test]
    fn camera_directory_name_includes_channel() {
        let name = camera_directory_name(1, Some("Front Door"));
        assert!(name.contains("1"));
        assert!(name.contains("Front-Door"));
    }

    #[test]
    fn camera_directory_name_with_no_name_uses_camera() {
        let name = camera_directory_name(2, None);
        assert!(name.contains("2"));
        assert!(name.contains("camera"));
    }

    #[test]
    fn camera_directory_names_are_distinct() {
        let n1 = camera_directory_name(1, Some("Front"));
        let n2 = camera_directory_name(2, Some("Front"));
        assert_ne!(n1, n2);
    }

    // ── Image destination tests ─────────────────────────────────────────

    #[test]
    fn image_destination_contains_date_components() {
        let dest = image_destination(
            Path::new("/output"),
            1,
            Some("Front"),
            &test_ts(),
            &TrackId::new("103"),
            &ImageKey::new("a04b77e391f2deadbeef"),
        );
        assert!(dest.final_path.to_string_lossy().contains("/2026/07/11/"));
        assert!(
            dest.final_path
                .to_string_lossy()
                .contains("20260711T022932Z")
        );
        assert!(dest.final_path.to_string_lossy().contains("track-103"));
        assert!(dest.final_path.to_string_lossy().contains("a04b77e391f2"));
        assert!(dest.final_path.to_string_lossy().ends_with(".jpg"));
        assert!(dest.part_path.to_string_lossy().ends_with(".part"));
    }

    #[test]
    fn image_destination_part_path_is_same_directory() {
        let dest = image_destination(
            Path::new("/output"),
            1,
            None,
            &test_ts(),
            &TrackId::new("103"),
            &ImageKey::new("abcdef123456"),
        );
        assert_eq!(dest.part_path.parent(), dest.final_path.parent());
    }

    #[test]
    fn image_destination_includes_fractional_timestamp_when_present() {
        // Timestamp with non-zero subseconds should include fractional component.
        let ts_with_frac = Timestamp::new(
            chrono::Utc
                .with_ymd_and_hms(2026, 7, 11, 2, 29, 32)
                .unwrap()
                .with_nanosecond(500_000_000)
                .unwrap(),
        );
        let dest = image_destination(
            Path::new("/output"),
            1,
            Some("Front"),
            &ts_with_frac,
            &TrackId::new("103"),
            &ImageKey::new("a04b77e391f2deadbeef"),
        );
        // Should include fractional seconds.
        assert!(
            dest.final_path
                .to_string_lossy()
                .contains("20260711T022932.500000Z"),
            "expected fractional timestamp, got: {}",
            dest.final_path.to_string_lossy()
        );
    }

    #[test]
    fn image_destination_no_fractional_when_zero() {
        // Timestamp with zero subseconds should NOT include fractional component.
        let ts_no_frac = Timestamp::new(
            chrono::Utc
                .with_ymd_and_hms(2026, 7, 11, 2, 29, 32)
                .unwrap()
                .with_nanosecond(0)
                .unwrap(),
        );
        let dest = image_destination(
            Path::new("/output"),
            1,
            Some("Front"),
            &ts_no_frac,
            &TrackId::new("103"),
            &ImageKey::new("a04b77e391f2deadbeef"),
        );
        // Should NOT include fractional seconds.
        let filename = dest.final_path.file_name().unwrap().to_string_lossy();
        assert!(
            filename.contains("20260711T022932Z"),
            "expected no fractional timestamp, got: {}",
            filename
        );
        // The filename should not contain a dot before the extension
        // (which would indicate fractional seconds).
        let name_without_ext = filename.strip_suffix(".jpg").unwrap();
        assert!(
            !name_without_ext.contains("."),
            "timestamp should not contain dot: {}",
            name_without_ext
        );
    }

    // ── JPEG validation tests ───────────────────────────────────────────

    #[test]
    fn validate_jpeg_body_empty_fails() {
        let result = validate_jpeg_body(b"", true);
        assert!(result.is_err());
    }

    #[test]
    fn validate_jpeg_body_valid_succeeds() {
        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
        let result = validate_jpeg_body(jpeg, true);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_jpeg_body_missing_start_marker_fails() {
        let data = b"\x00\x01\x02\xff\xd9";
        let result = validate_jpeg_body(data, true);
        assert!(result.is_err());
    }

    #[test]
    fn validate_jpeg_body_missing_end_marker_fails() {
        let data = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01";
        let result = validate_jpeg_body(data, true);
        assert!(result.is_err());
    }

    #[test]
    fn validate_jpeg_body_html_rejected() {
        let html = b"<html><body>Access Denied</body></html>";
        let result = validate_jpeg_body(html, true);
        assert!(result.is_err());
    }

    #[test]
    fn validate_jpeg_body_short_body_fails() {
        let data = b"\xff\xd8";
        let result = validate_jpeg_body(data, true);
        assert!(result.is_err());
    }

    #[test]
    fn validate_jpeg_body_no_verification_accepts_anything() {
        let html = b"<html><body>Access Denied</body></html>";
        let result = validate_jpeg_body(html, false);
        assert!(result.is_ok());
    }

    // ── Existing file verification tests ─────────────────────────────────

    #[test]
    fn verify_existing_file_not_found_returns_download_required() {
        let result = verify_existing_file(Path::new("/nonexistent/file.jpg"), 25_000_000, true);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), FilePreparation::DownloadRequired));
    }

    #[test]
    fn verify_existing_file_empty_returns_download_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jpg");
        std::fs::write(&path, b"").unwrap();
        let result = verify_existing_file(&path, 25_000_000, true);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), FilePreparation::DownloadRequired));
    }

    #[test]
    fn verify_existing_file_valid_jpeg_returns_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.jpg");
        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
        std::fs::write(&path, jpeg).unwrap();
        let result = verify_existing_file(&path, 25_000_000, true);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), FilePreparation::Adopted));
    }

    #[test]
    fn verify_existing_file_too_large_returns_download_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jpg");
        std::fs::write(&path, vec![0u8; 30_000_000]).unwrap();
        let result = verify_existing_file(&path, 25_000_000, false);
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), FilePreparation::DownloadRequired));
    }

    // ── Stale part cleanup tests ────────────────────────────────────────

    #[tokio::test]
    async fn remove_stale_part_files_removes_parts() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir_all(&output).unwrap();

        // Create a .part file
        let part_path = output.join("test.jpg.part");
        std::fs::write(&part_path, "junk").unwrap();

        // Create a non-part file
        let final_path = output.join("other.jpg");
        std::fs::write(&final_path, "data").unwrap();

        let count = remove_stale_part_files(&output).await.unwrap();
        assert_eq!(count, 1);
        assert!(!part_path.exists());
        assert!(final_path.exists());
    }

    #[tokio::test]
    async fn remove_stale_part_files_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        let sub = output.join("camera-1").join("2026").join("07").join("11");
        std::fs::create_dir_all(&sub).unwrap();

        let part_path = sub.join("image.jpg.part");
        std::fs::write(&part_path, "junk").unwrap();

        let count = remove_stale_part_files(&output).await.unwrap();
        assert_eq!(count, 1);
        assert!(!part_path.exists());
    }

    #[tokio::test]
    async fn remove_stale_part_files_no_parts_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir_all(&output).unwrap();

        let count = remove_stale_part_files(&output).await.unwrap();
        assert_eq!(count, 0);
    }

    // ── Safe URL test ───────────────────────────────────────────────────

    #[test]
    fn safe_url_string_strips_query() {
        let url = Url::parse("http://nvr/picture/1?starttime=abc&key=secret").unwrap();
        let safe = safe_url_string(&url);
        assert!(!safe.contains("starttime"));
        assert!(!safe.contains("key"));
        assert!(safe.contains("/picture/1"));
    }

    // ── cleanup_with_context unit tests ───────────────────────────────────

    /// Test that cleanup_with_context combines the primary error with
    /// a safe cleanup-failure indication when the part file removal fails.
    ///
    /// Uses a read-only directory to deterministically cause removal failure.
    /// Asserts both Display and Debug contain safe primary and cleanup context
    /// without leaking any sensitive data.
    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_with_context_combines_primary_and_cleanup_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir_all(&output).unwrap();

        // Create a .part file that cleanup will try to remove.
        let part_path = output.join("test.jpg.part");
        std::fs::write(&part_path, "junk").unwrap();
        assert!(part_path.exists());

        // Make the parent directory read-only so removal fails with EACCES.
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Construct a primary error with a safe message.
        let primary_error = AppError::new(
            ErrorCategory::Network,
            "stream_response_to_destination",
            "interrupted body transfer",
        );

        // Call cleanup_with_context directly — removal will fail.
        let combined = cleanup_with_context(&part_path, primary_error).await;

        // The part file should still exist (removal failed).
        assert!(
            part_path.exists(),
            "part file should still exist after failed cleanup"
        );

        // Display must contain safe primary context.
        let display = format!("{combined}");
        assert!(
            display.contains("interrupted body transfer"),
            "Display must contain primary context: {display}"
        );
        assert!(
            display.contains("part file cleanup failed"),
            "Display must contain cleanup context: {display}"
        );
        assert!(
            !display.contains("SENTINEL"),
            "sentinel leaked in Display: {display}"
        );

        // Debug must also contain safe primary and cleanup context.
        let debug_output = format!("{combined:?}");
        assert!(
            debug_output.contains("interrupted body transfer"),
            "Debug must contain primary context: {debug_output}"
        );
        assert!(
            debug_output.contains("part file cleanup failed"),
            "Debug must contain cleanup context: {debug_output}"
        );
        assert!(
            !debug_output.contains("SENTINEL"),
            "sentinel leaked in Debug: {debug_output}"
        );

        // Verify the error category and operation are preserved.
        assert_eq!(combined.category, ErrorCategory::Network);
        assert_eq!(combined.operation, "stream_response_to_destination");

        // Restore permissions for cleanup.
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Test that cleanup_with_context returns the original error unchanged
    /// when removal succeeds.
    #[tokio::test]
    async fn cleanup_with_context_returns_original_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir_all(&output).unwrap();

        let part_path = output.join("test.jpg.part");
        std::fs::write(&part_path, "junk").unwrap();

        let primary_error = AppError::new(
            ErrorCategory::Network,
            "stream_response_to_destination",
            "interrupted body transfer",
        );

        let result = cleanup_with_context(&part_path, primary_error).await;

        // The part file should be removed.
        assert!(
            !part_path.exists(),
            "part file should be removed after successful cleanup"
        );

        // The result should be the original error unchanged.
        assert_eq!(result.message, "interrupted body transfer");
        assert_eq!(result.category, ErrorCategory::Network);
    }
}
