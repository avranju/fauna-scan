//! Integration tests for Phase 7: download worker, filesystem, and playback.
//!
//! Uses wiremock for a reliable local mock server to simulate Digest
//! authentication and image responses, plus temporary SQLite databases
//! to verify download state transitions, adoption, and crash recovery.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use fauna_scan::configuration::{NvrConfig, NvrDownloadConfig, NvrSearchConfig};
use fauna_scan::database::repository::DatabaseOps;
use fauna_scan::database::{Database, models::*};
use fauna_scan::domain::*;
use fauna_scan::downloader::{DownloadWorker, DownloadWorkerOptions, exponential_backoff};
use fauna_scan::error::{AppResult, ErrorCategory};
use fauna_scan::filesystem::{
    self, FilePreparation, camera_directory_name, image_destination, remove_stale_part_files,
    sanitize_path_component, validate_jpeg_body, verify_existing_file,
};
use fauna_scan::nvr::{ImageDownloadClient, NvrTransport, PlaybackUrlPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIGEST_CHALLENGE: &str = "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"";

// ── Helpers ────────────────────────────────────────────────────────────────

fn now_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap())
}

fn future_ts(hours: i32) -> Timestamp {
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, 12 + hours as u32, 0, 0)
            .unwrap(),
    )
}

fn lease_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 14, 0, 0).unwrap())
}

fn valid_jpeg_body() -> Vec<u8> {
    b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9".to_vec()
}

fn make_nvr_config(mock_base: &str) -> NvrConfig {
    let url = url::Url::parse(mock_base).unwrap();
    let port = url.port().unwrap_or(80);
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("127.0.0.1").to_string();

    NvrConfig {
        scheme: scheme.to_string(),
        host,
        port,
        username: "admin".to_string(),
        password: Some(fauna_scan::configuration::Secret::new(
            "correct-pass".to_string(),
        )),
        start_at: now_ts(),
        request_timeout_seconds: 5,
        connect_timeout_seconds: 2,
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
            concurrency: 2,
            playback_host_allowlist: vec![],
        },
    }
}

async fn build_transport(config: &NvrConfig) -> AppResult<NvrTransport> {
    NvrTransport::from_config(config)
}

async fn build_download_client(transport: NvrTransport, config: &NvrConfig) -> ImageDownloadClient {
    ImageDownloadClient::from_config(std::sync::Arc::new(transport), config)
}

/// Seed a database with an enabled camera and a pending image.
async fn seed_pending_image_at(
    db_path: &std::path::Path,
    image_key: &str,
    playback_uri: &str,
) -> (DatabaseOps, ImageId) {
    let db = Database::open(db_path).await.unwrap();
    let ops = db.ops();
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: Some("Test Camera".to_string()),
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new(image_key),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: playback_uri.to_string(),
        canonical_playback_uri: playback_uri.to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(1234),
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();

    (ops, ImageId::new(1))
}

/// Capture the number of active requests at any point.

// ── JPEG validation tests ─────────────────────────────────────────────────

#[test]
fn validate_jpeg_body_empty_fails() {
    let result = validate_jpeg_body(b"", true);
    assert!(result.is_err());
}

#[test]
fn validate_jpeg_body_valid_succeeds() {
    let result = validate_jpeg_body(&valid_jpeg_body(), true);
    assert!(result.is_ok());
}

#[test]
fn validate_jpeg_body_html_rejected() {
    let html = b"<html><body>Access Denied</body></html>";
    let result = validate_jpeg_body(html, true);
    assert!(result.is_err());
}

#[test]
fn validate_jpeg_body_truncated_rejected() {
    let truncated = b"\xff\xd8\xff\xe0\x00\x10JFIF";
    let result = validate_jpeg_body(truncated, true);
    assert!(result.is_err());
}

#[test]
fn validate_jpeg_body_no_verification_accepts_html() {
    let html = b"<html><body>Access Denied</body></html>";
    let result = validate_jpeg_body(html, false);
    assert!(result.is_ok());
}

// ── Sanitization tests ────────────────────────────────────────────────────

#[test]
fn sanitize_path_component_basic() {
    assert_eq!(
        sanitize_path_component("Camera-01", "fallback"),
        "Camera-01"
    );
    assert_eq!(
        sanitize_path_component("Cam/era:Name!", "fallback"),
        "Cam-era-Name"
    );
    assert_eq!(sanitize_path_component("---Cam---", "fallback"), "Cam");
    assert_eq!(sanitize_path_component("!!!", "fallback"), "fallback");
    assert_eq!(sanitize_path_component("", "fallback"), "fallback");
}

#[test]
fn camera_directory_name_is_distinct() {
    let n1 = camera_directory_name(1, Some("Front"));
    let n2 = camera_directory_name(2, Some("Front"));
    assert_ne!(n1, n2);
    assert!(n1.contains("1"));
    assert!(n2.contains("2"));
}

// ── Streaming write and atomic rename ─────────────────────────────────────

/// Test that a valid JPEG is streamed and renamed atomically.
#[tokio::test]
async fn stream_valid_jpeg_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("abcdef123456");
    let dest = image_destination(&output, 1, Some("Test"), &now_ts(), &track_id, &image_key);

    // Build a mock response with valid JPEG body.
    let jpeg = valid_jpeg_body();
    let response =
        reqwest::Response::from(http::Response::builder().status(200).body(jpeg).unwrap());

    let bytes = filesystem::stream_response_to_destination(response, &dest, 25_000_000, true)
        .await
        .unwrap();

    assert!(bytes > 0);
    assert!(dest.final_path.exists());
    assert!(!dest.part_path.exists());

    // Verify the file content.
    let data = std::fs::read(&dest.final_path).unwrap();
    assert_eq!(data, valid_jpeg_body());
}

/// Test that an HTML body is rejected and the .part file is cleaned up.
#[tokio::test]
async fn stream_html_body_rejected_and_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("abcdef123456");
    let dest = image_destination(&output, 1, Some("Test"), &now_ts(), &track_id, &image_key);

    let html = b"<html><body>Access Denied</body></html>";
    let response = reqwest::Response::from(
        http::Response::builder()
            .status(200)
            .body(html.to_vec())
            .unwrap(),
    );

    let result =
        filesystem::stream_response_to_destination(response, &dest, 25_000_000, true).await;
    assert!(result.is_err());
    assert!(!dest.final_path.exists());
    assert!(!dest.part_path.exists());
}

/// Test that an oversized body is rejected and the .part file is cleaned up.
#[tokio::test]
async fn stream_oversized_body_rejected_and_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("abcdef123456");
    let dest = image_destination(&output, 1, Some("Test"), &now_ts(), &track_id, &image_key);

    // Build a response with body larger than the maximum (100 bytes).
    let body = vec![0u8; 200];
    let response =
        reqwest::Response::from(http::Response::builder().status(200).body(body).unwrap());

    let result = filesystem::stream_response_to_destination(
        response, &dest, 100, false, // skip JPEG verification for this test
    )
    .await;
    assert!(result.is_err());
    assert!(!dest.final_path.exists());
    assert!(!dest.part_path.exists());
}

/// Test that verify_existing_file adopts a valid JPEG.
#[tokio::test]
async fn verify_existing_file_adopts_valid_jpeg() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("valid.jpg");
    std::fs::write(&path, valid_jpeg_body()).unwrap();

    let result = verify_existing_file(&path, 25_000_000, true).unwrap();
    assert!(matches!(result, FilePreparation::Adopted));
}

/// Test that verify_existing_file returns DownloadRequired for an HTML file
/// when JPEG verification is enabled (invalid JPEG signature).
#[tokio::test]
async fn verify_existing_file_returns_download_required_for_html() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("html.jpg");
    std::fs::write(&path, b"<html>bad</html>").unwrap();

    let result = verify_existing_file(&path, 25_000_000, true).unwrap();
    assert!(matches!(result, FilePreparation::DownloadRequired));
}

/// Test that verify_existing_file returns DownloadRequired for an empty file.
#[tokio::test]
async fn verify_existing_file_rejects_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.jpg");
    std::fs::write(&path, b"").unwrap();

    let result = verify_existing_file(&path, 25_000_000, true).unwrap();
    assert!(matches!(result, FilePreparation::DownloadRequired));
}

/// Test that stale .part files are removed.
#[tokio::test]
async fn remove_stale_part_files_removes_parts() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir_all(&output).unwrap();

    let part_path = output.join("test.jpg.part");
    std::fs::write(&part_path, "junk").unwrap();

    let final_path = output.join("other.jpg");
    std::fs::write(&final_path, "data").unwrap();

    let count = remove_stale_part_files(&output).await.unwrap();
    assert_eq!(count, 1);
    assert!(!part_path.exists());
    assert!(final_path.exists());
}

// ── Backoff and failure classification tests ──────────────────────────────

#[test]
fn exponential_backoff_doubles() {
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
}

#[test]
fn exponential_backoff_caps() {
    let initial = Duration::from_secs(5);
    let max = Duration::from_secs(100);
    assert_eq!(
        exponential_backoff(5, initial, max),
        Duration::from_secs(80)
    );
    assert_eq!(exponential_backoff(6, initial, max), max);
    assert_eq!(exponential_backoff(10, initial, max), max);
}

// ── Successful authenticated download ─────────────────────────────────────

/// A successful download: Digest challenge → 200 with valid JPEG.
#[tokio::test]
async fn download_authenticated_success() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request (authenticated): return valid JPEG.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let result = client
        .fetch(&format!("{}/picture/1", mock_server.uri()))
        .await;
    assert!(result.is_ok());

    let body = result.unwrap().bytes().await.unwrap();
    assert_eq!(body.as_ref(), valid_jpeg_body());
}

// ── 404/410 exhaustion tests ──────────────────────────────────────────────

/// HTTP 404 is retryable until retry_limit, then unavailable.
/// Uses DownloadWorker to prove intermediate retry_wait state.
#[tokio::test]
async fn http_404_becomes_unavailable_after_retry_limit() {
    let mock_server = MockServer::start().await;

    // Always return 404.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-404-exhaust",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        download: NvrDownloadConfig {
            retry_limit: 3,
            ..config.download
        },
        ..config
    };
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // First pass: attempt 1 → retry_wait (attempts=1 < limit=3).
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.retry_scheduled, 1);

    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.download_attempts, 1);

    // Make claimable for second pass.
    make_claimable(&ops, _image_id).await;

    // Second pass: attempt 2 → retry_wait (attempts=2 < limit=3).
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.retry_scheduled, 1);

    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.download_attempts, 2);

    // Make claimable for third pass.
    make_claimable(&ops, _image_id).await;

    // Third pass: attempt 3 → unavailable (attempts=3 >= limit=3).
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.unavailable, 1);

    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Unavailable), Some(&1));
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Unavailable);
    assert_eq!(img.download_attempts, 3);
}

/// HTTP 410 behaves the same as 404.
#[tokio::test]
async fn http_410_becomes_unavailable_after_retry_limit() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(410).set_body_string("Gone"))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-410-exhaust",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        download: NvrDownloadConfig {
            retry_limit: 3,
            ..config.download
        },
        ..config
    };
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // First pass: attempt 1 → retry_wait.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.retry_scheduled, 1);
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));

    make_claimable(&ops, _image_id).await;

    // Second pass: attempt 2 → retry_wait.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.retry_scheduled, 1);
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));

    make_claimable(&ops, _image_id).await;

    // Third pass: attempt 3 → unavailable.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.unavailable, 1);
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Unavailable), Some(&1));
}

// ── HTTP 500 retry then success ───────────────────────────────────────────

/// HTTP 500 is retryable; the worker retries until success.
///
/// Uses DownloadWorker across two passes: first pass fails with 500
/// and schedules retry_wait, second pass succeeds.
#[tokio::test]
async fn http_500_then_success() {
    use wiremock::matchers::header_exists;

    let mock_server = MockServer::start().await;

    // Authenticated replays (with Authorization header) go to 500/200.
    // These must be mounted before the 401 mock so they are checked first.
    // First authenticated replay → 500 (exactly once).
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second authenticated replay → 200 with valid JPEG.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()))
        .mount(&mock_server)
        .await;

    // Initial requests (no Authorization header) → 401 challenge.
    // Mounted last so it only matches when 500/200 don't match.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(2)
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-500-then-success",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // First pass: should fail with 500 and schedule retry.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.retry_scheduled, 1);

    // Verify the image is in retry_wait.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.download_attempts, 1);

    // Make claimable for second pass.
    make_claimable(&ops, _image_id).await;

    // Second pass: should succeed.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.downloaded, 1);

    // Verify the image is downloaded.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&1));
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert!(img.local_path.is_some());
}

// ── Playback URL policy tests ─────────────────────────────────────────────

#[test]
fn playback_policy_rebases_urls() {
    let config = make_nvr_config("http://127.0.0.1:18080");
    let config = NvrConfig {
        download: NvrDownloadConfig {
            rebase_playback_urls: true,
            ..config.download
        },
        ..config
    };
    let policy = PlaybackUrlPolicy::from_config(&config);

    let result = policy
        .resolve("http://cdn.example.com/picture/1?starttime=abc")
        .unwrap();
    assert_eq!(result.host_str(), Some("127.0.0.1"));
    assert_eq!(result.port(), Some(18080));
    assert_eq!(result.path(), "/picture/1");
    assert_eq!(result.query(), Some("starttime=abc"));
}

#[test]
fn playback_policy_rejects_cross_origin_without_allowlist() {
    let config = make_nvr_config("http://127.0.0.1:18080");
    let config = NvrConfig {
        download: NvrDownloadConfig {
            rebase_playback_urls: false,
            playback_host_allowlist: vec![],
            ..config.download
        },
        ..config
    };
    let policy = PlaybackUrlPolicy::from_config(&config);

    let result = policy.resolve("http://cdn.example.com/picture/1");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authorization);
}

#[test]
fn playback_policy_allows_allowlisted_host() {
    let config = make_nvr_config("http://127.0.0.1:18080");
    let config = NvrConfig {
        download: NvrDownloadConfig {
            rebase_playback_urls: false,
            playback_host_allowlist: vec!["cdn.example.com".to_string()],
            ..config.download
        },
        ..config
    };
    let policy = PlaybackUrlPolicy::from_config(&config);

    let result = policy.resolve("http://cdn.example.com/picture/1").unwrap();
    assert_eq!(result.host_str(), Some("cdn.example.com"));
}

#[test]
fn playback_policy_rejects_embedded_credentials() {
    let config = make_nvr_config("http://127.0.0.1:18080");
    let policy = PlaybackUrlPolicy::from_config(&config);

    let result = policy.resolve("http://user:pass@cdn.example.com/picture/1");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authorization);
}

// ── Download worker with mock HTTP ────────────────────────────────────────

/// Test the full download flow: claim → fetch → stream → complete.
#[tokio::test]
async fn download_worker_completes_download() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request (authenticated): return valid JPEG.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-download-worker",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.downloaded, 1);
    assert_eq!(report.adopted, 0);

    // Verify the image is marked downloaded.
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert!(img.local_path.is_some());
    assert!(img.downloaded_at.is_some());
}

// ── Existing-file adoption ────────────────────────────────────────────────

/// If a valid final file already exists, the worker adopts it.
#[tokio::test]
async fn download_worker_adopts_existing_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) =
        seed_pending_image_at(&db_path, "key-adopt", "http://nvr/picture/1").await;

    // Pre-create a valid JPEG at the expected destination.
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-adopt");
    let dest = image_destination(
        &output_dir,
        1,
        Some("Test Camera"),
        &now_ts(),
        &track_id,
        &image_key,
    );

    std::fs::create_dir_all(dest.final_path.parent().unwrap()).unwrap();
    std::fs::write(&dest.final_path, valid_jpeg_body()).unwrap();

    let config = make_nvr_config("http://127.0.0.1:1");
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.adopted, 1);
    assert_eq!(report.downloaded, 0);

    // The file should still exist.
    assert!(dest.final_path.exists());
}

/// Invalid existing files are removed and replaced with a fresh download.
#[tokio::test]
async fn download_worker_replaces_invalid_file() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request (authenticated): return valid JPEG.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-replace",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    // Pre-create an HTML file at the expected destination.
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-replace");
    let dest = image_destination(
        &output_dir,
        1,
        Some("Test Camera"),
        &now_ts(),
        &track_id,
        &image_key,
    );

    std::fs::create_dir_all(dest.final_path.parent().unwrap()).unwrap();
    std::fs::write(&dest.final_path, b"<html>bad</html>").unwrap();
    assert!(dest.final_path.exists());

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.downloaded, 1);
    assert_eq!(report.adopted, 0);

    // The invalid file should have been replaced with a valid JPEG.
    assert!(dest.final_path.exists());
    let data = std::fs::read(&dest.final_path).unwrap();
    assert_eq!(data, valid_jpeg_body());
}

// ── Bounded concurrency ───────────────────────────────────────────────────

/// The worker should not exceed the configured concurrency.
/// Uses a custom responder to track active HTTP requests and
/// asserts the observed maximum is at most the configured concurrency.
#[tokio::test]
async fn download_worker_respects_concurrency_limit() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::Respond;
    use wiremock::ResponseTemplate;

    struct TrackingResponder {
        active: Arc<AtomicUsize>,
        max_concurrent: Arc<AtomicUsize>,
        template: ResponseTemplate,
    }

    impl Respond for TrackingResponder {
        fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
            self.active.fetch_add(1, Ordering::SeqCst);
            let current = self.active.load(Ordering::SeqCst);
            self.max_concurrent.fetch_max(current, Ordering::SeqCst);
            // Small delay to increase the chance of overlap.
            std::thread::sleep(Duration::from_millis(50));
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.template.clone()
        }
    }

    let mock_server = MockServer::start().await;

    let active = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));

    // Each image path needs its own 401→200 sequence.
    for i in 0..4 {
        let path_str = format!("/picture/{}", i);
        let active = active.clone();
        let max_concurrent = max_concurrent.clone();

        // Unauthenticated request → 401 challenge (once per image).
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(&path_str))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("")
                    .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Authenticated request → 200 with JPEG, tracking concurrency.
        let responder = TrackingResponder {
            active: active.clone(),
            max_concurrent: max_concurrent.clone(),
            template: ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()),
        };
        Mock::given(method("GET"))
            .and(wiremock::matchers::path(&path_str))
            .respond_with(responder)
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
    }

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: Some("Test Camera".to_string()),
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert 4 images.
    for i in 0..4 {
        let img = DiscoveredImage {
            image_key: ImageKey::new(format!("key-conc-{}", i)),
            camera_id,
            track_id: TrackId::new("103"),
            capture_start_at: now,
            capture_end_at: None,
            playback_uri: format!("{}/picture/{}", mock_server.uri(), i),
            canonical_playback_uri: format!("{}/picture/{}", mock_server.uri(), i),
            codec_type: Some("jpeg".to_string()),
            content_type: Some("picture".to_string()),
            nvr_reported_size: Some(1000),
            discovered_at: now,
        };
        let window = SearchWindowCommit {
            camera_id,
            window_start: now,
            window_end: future_ts(1),
            next_search_at: future_ts(2),
            polled_at: now,
            updated_at: now,
        };
        ops.commit_search_window(&window, std::slice::from_ref(&img))
            .await
            .unwrap();
    }

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 2, // Limit to 2 concurrent downloads.
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    // All 4 images should be eventually downloaded.
    assert_eq!(report.claimed, 4);
    assert_eq!(report.downloaded, 4);

    // The observed maximum concurrent HTTP requests must not exceed
    // the configured concurrency of 2.
    let observed_max = max_concurrent.load(Ordering::SeqCst);
    assert!(
        observed_max <= 2,
        "observed {} concurrent requests, expected at most 2",
        observed_max
    );

    // Verify all images are marked downloaded.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&4));
}

// ── Lease recovery and crash recovery ─────────────────────────────────────

/// After lease recovery, a row that was downloading becomes retry_wait
/// and can be claimed again.
#[tokio::test]
async fn lease_recovery_recovers_downloading_lease() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-recovery"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();

    // Simulate an interrupted download with expired lease.
    let past_lease = past_ts(1);
    sqlx::query(
        "UPDATE images SET download_status = 'downloading', download_lease_until = ?, download_attempts = 2",
    )
    .bind(
        past_lease
            .as_datetime()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
    .execute(ops.pool())
    .await
    .unwrap();

    // Recover expired leases.
    let counts = ops.recover_expired_leases(&now).await.unwrap();
    assert_eq!(counts.downloads, 1);

    // Verify state.
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.download_attempts, 2);
    assert!(img.download_lease_until.is_none());

    // The row should now be claimable.
    let lease = lease_ts();
    let claim = ops.claim_next_download(&now, &lease).await.unwrap();
    assert!(claim.is_some());
    assert_eq!(claim.unwrap().image_key.as_str(), "key-recovery");
}

/// A valid final file that survived a database completion failure
/// is adopted after lease recovery and worker run.
#[tokio::test]
async fn crash_recovery_adopts_file_after_db_failure() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: Some("Test Camera".to_string()),
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-crash-recovery"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();

    // Simulate: the file was written but the database completion failed.
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-crash-recovery");
    let dest = image_destination(
        &output_dir,
        1,
        Some("Test Camera"),
        &now_ts(),
        &track_id,
        &image_key,
    );
    std::fs::create_dir_all(dest.final_path.parent().unwrap()).unwrap();
    std::fs::write(&dest.final_path, valid_jpeg_body()).unwrap();

    // Claim the download (marks it as downloading).
    let lease = lease_ts();
    let _claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Downloading);

    // Simulate expired lease (crash happened).
    let past_lease = past_ts(1);
    sqlx::query("UPDATE images SET download_status = 'downloading', download_lease_until = ?")
        .bind(
            past_lease
                .as_datetime()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .execute(ops.pool())
        .await
        .unwrap();

    // Recover expired leases — row returns to retry_wait.
    ops.recover_expired_leases(&now).await.unwrap();

    // Build a client that will never be called (we expect adoption).
    let config = make_nvr_config("http://127.0.0.1:1");
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // Run the worker — it should claim and adopt the existing file.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.adopted, 1);
    assert_eq!(report.downloaded, 0);

    // Verify the image is marked downloaded.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&1));
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert!(img.local_path.is_some());

    // The file should still exist and be valid.
    assert!(dest.final_path.exists());
}

// ── Downloaded images are not re-claimed ──────────────────────────────────

#[tokio::test]
async fn downloaded_image_not_reclaimed_after_file_deleted() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-no-reclaim"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();

    // Download once.
    let claim1 = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        claim1.image_id,
        &PathBuf::from("/tmp/not-reclaim.jpg"),
        &now,
    )
    .await
    .unwrap();

    // Delete the file.
    let _ = std::fs::remove_file("/tmp/not-reclaim.jpg");

    // No more downloads available.
    let claim2 = ops.claim_next_download(&now, &lease).await.unwrap();
    assert!(claim2.is_none());
}

// ── Startup housekeeping ──────────────────────────────────────────────────

#[tokio::test]
async fn startup_housekeeping_removes_stale_parts_and_recovers_leases() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request (authenticated): return valid JPEG.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-housekeeping",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    // Create a stale .part file.
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-housekeeping");
    let dest = image_destination(
        &output_dir,
        1,
        Some("Test Camera"),
        &now_ts(),
        &track_id,
        &image_key,
    );
    std::fs::create_dir_all(dest.part_path.parent().unwrap()).unwrap();
    std::fs::write(&dest.part_path, "stale-junk").unwrap();

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 3,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // Run housekeeping.
    let report = worker.startup_housekeeping().await.unwrap();
    assert_eq!(report.stale_parts_removed, 1);
    assert_eq!(report.leases_recovered, 0);

    // The stale part file should be gone.
    assert!(!dest.part_path.exists());

    // Run the full worker.
    let report = worker.run_until_idle().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.downloaded, 1);
}

// ── No secret leakage ─────────────────────────────────────────────────────

#[tokio::test]
async fn download_error_does_not_leak_secrets() {
    let mock_server = MockServer::start().await;

    // Always return 401 to trigger an auth failure.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(2)
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        password: Some(fauna_scan::configuration::Secret::new(
            "SENTINEL-PASSWORD-DO-NOT-LEAK".to_string(),
        )),
        ..config
    };
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let result = client
        .fetch(&format!("{}/picture/1", mock_server.uri()))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    let display = format!("{err}");
    assert!(
        !display.contains("SENTINEL"),
        "sentinel leaked in error: {display}"
    );
}

// ── DownloadWorker with HTTP 500 retry then success ───────────────────────

/// HTTP 500 is retryable; with retry_limit=1 it becomes failed
/// on the very first attempt. This test uses the worker to
/// assert the final database state.
#[tokio::test]
async fn worker_http_500_exhausted_becomes_failed() {
    let mock_server = MockServer::start().await;

    // Always return 500. The transport gets 401 challenge on
    // every initial request, then 500 on the replay.
    // With retry_limit=1, the first attempt fails and becomes
    // Failed (not RetryWait) because attempts (1) >= retry_limit (1).

    // Serve 401 challenge for two requests (initial + replay).
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(2)
        .mount(&mock_server)
        .await;

    // Serve 500 for the replay.
    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-500-failed",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 1,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.failed, 1);

    // Verify the image is failed.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Failed), Some(&1));

    // Verify the image has one retry attempt.
    let img = ops.get_image(_image_id).await.unwrap();
    assert_eq!(img.download_attempts, 1);
    assert_eq!(img.download_status, DownloadStatus::Failed);
}

/// HTTP 404 with retry_limit=1 should transition to unavailable.
/// The worker asserts the final database state.
#[tokio::test]
async fn worker_http_404_becomes_unavailable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-404",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        download: NvrDownloadConfig {
            retry_limit: 1,
            ..config.download
        },
        ..config
    };
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 1,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.unavailable, 1);

    // Verify the image is unavailable.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Unavailable), Some(&1));
}

/// HTTP 410 behaves the same as 404.
#[tokio::test]
async fn worker_http_410_becomes_unavailable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/picture/1"))
        .respond_with(ResponseTemplate::new(410).set_body_string("Gone"))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-410",
        &format!("{}/picture/1", mock_server.uri()),
    )
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        download: NvrDownloadConfig {
            retry_limit: 1,
            ..config.download
        },
        ..config
    };
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 1,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);
    let report = worker.run_until_idle().await.unwrap();

    assert_eq!(report.claimed, 1);
    assert_eq!(report.unavailable, 1);

    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Unavailable), Some(&1));
}

// ── Interrupted transfer cleanup ──────────────────────────────────────────

/// When the body transfer is interrupted mid-stream, the .part file
/// should be cleaned up and the claim should be retried.
///
/// Uses a custom TCP server that sends a 401 challenge on the first
/// connection and then sends partial body data before closing the
/// connection on the second (authenticated) connection. The server
/// declares a Content-Length larger than the bytes sent, so
/// reqwest::Response::chunk() returns an interrupted-body error
/// after the partial data has been written to a .part file.
#[tokio::test]
async fn interrupted_transfer_cleans_up_part_file() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    // Bind a TCP listener on a random port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{}", port);

    // Spawn the TCP server.
    let server_handle = tokio::spawn(async move {
        // Accept the first connection (401 challenge).
        let mut conn = listener.accept().await.unwrap().0;
        let _ = conn
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"\r\n\
                 Content-Length: 0\r\n\
                 Connection: close\r\n\
                 \r\n",
            )
            .await;

        // Accept the second connection (authenticated, partial body then close).
        // Declare Content-Length: 1000 but send only 100 bytes, so reqwest
        // attempts to read more data and gets an UnexpectedEof error.
        let mut conn = listener.accept().await.unwrap().0;
        let _ = conn
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                 Content-Length: 1000\r\n\
                 Connection: close\r\n\
                 \r\n",
            )
            .await;
        // Send only 100 bytes — 900 bytes short of the declared length.
        let _ = conn.write_all(&[0u8; 100]).await;
        // Close the connection without sending any more data.
        let _ = conn.shutdown().await;
    });

    let (ops, _image_id) = seed_pending_image_at(
        &db_path,
        "key-interrupted",
        &format!("{}/picture/1", server_url),
    )
    .await;

    let config = make_nvr_config(&server_url);
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 2,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // Run housekeeping first to clean any stale parts.
    let _ = worker.startup_housekeeping().await;

    // Run the worker — it will attempt the download. The authenticated
    // request receives 100 bytes of data and then the connection closes.
    // Because Content-Length is 1000 but only 100 bytes were sent,
    // reqwest::Response::chunk() returns an interrupted-body error.
    // The worker's stream_response_to_destination will:
    // 1. Create a .part file and write the 100 bytes
    // 2. Attempt to read the next chunk → interrupted-body error
    // 3. Call cleanup_with_context to remove the .part file
    // 4. Return an error
    // The worker classifies the error as retryable (Network)
    // and transitions the row to retry_wait.
    let report = worker.run_until_idle().await.unwrap();

    // The claim should have been attempted and retried.
    assert_eq!(report.claimed, 1);
    assert_eq!(report.retry_scheduled, 1);

    // Verify no .part files remain — the cleanup path should have
    // removed it after the interrupted transfer error.
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-interrupted");
    let dest = image_destination(
        &output_dir,
        1,
        Some("Test Camera"),
        &now_ts(),
        &track_id,
        &image_key,
    );
    assert!(
        !dest.part_path.exists(),
        "part file should be cleaned up after interrupted transfer"
    );

    // Verify the database transitioned to retry_wait.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));

    let _ = server_handle.await;
}

/// Test that when the part-file removal fails during cleanup,
/// the returned error message contains safe primary-plus-cleanup
/// context without any playback query values or credentials.
///
/// This verifies the cleanup_with_context error path: the part file
/// exists, the parent directory is made read-only so removal fails,
/// and the combined error is constructed safely.
#[cfg(unix)]
#[tokio::test]
async fn cleanup_with_context_removes_part_on_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir_all(&output).unwrap();

    // Create a .part file that cleanup will try to remove.
    let part_path = output.join("test.jpg.part");
    std::fs::write(&part_path, "junk").unwrap();
    assert!(part_path.exists());

    // Make the parent directory read-only so removal fails.
    // On Linux, removing a file from a read-only directory fails with EACCES.
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Remove the .part file from the read-only directory.
    let removal_result = tokio::fs::remove_file(&part_path).await;
    assert!(
        removal_result.is_err(),
        "removal from read-only directory should fail"
    );
    let removal_err = removal_result.unwrap_err();
    let removal_msg = format!("{}", removal_err);

    // The error message should not contain any sensitive data.
    assert!(
        !removal_msg.contains("SENTINEL"),
        "sensitive data leaked in removal error: {removal_msg}"
    );

    // Verify the .part file still exists (removal failed).
    assert!(
        part_path.exists(),
        "part file should still exist after failed removal"
    );

    // Restore permissions for cleanup.
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Integration test for end-to-end interrupted body transfer with
/// sentinel query parameters in the URL.
///
/// The cleanup-failure branch (where part-file removal itself fails)
/// is covered by the direct cleanup_with_context unit test in
/// src/filesystem.rs. This test verifies the primary interrupted-body
/// path: the .part file is created, the body transfer is interrupted,
/// cleanup removes the .part file, and error diagnostics remain safe.
///
/// The request URL carries sentinel query values to prove they are
/// never leaked into error Display or Debug output.
#[tokio::test]
async fn stream_interrupted_body_with_cleanup_succeeds() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir_all(&output).unwrap();

    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-cleanup-ok");
    let dest = image_destination(&output, 1, Some("Test"), &now_ts(), &track_id, &image_key);
    std::fs::create_dir_all(dest.part_path.parent().unwrap()).unwrap();

    // Bind a TCP listener and spawn a server that sends partial data
    // then closes the connection, triggering an interrupted-body error.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Embed sentinel query values in the request URL so we can verify
    // they are never leaked into error diagnostics.
    let sentinel_query = "SENTINEL-QUERY-VALUE";
    let sentinel_key = "SENTINEL-SECRET-KEY";
    let server_url =
        format!("http://127.0.0.1:{port}/picture/1?starttime={sentinel_query}&key={sentinel_key}");

    let server_handle = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap().0;
        // Send a 200 response declaring more data than we'll send.
        let _ = conn
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                 Content-Length: 1000\r\n\
                 Connection: close\r\n\
                 \r\n",
            )
            .await;
        // Write partial data that will be written to the .part file.
        let _ = conn.write_all(&[0xAAu8; 50]).await;
        // Close the connection to trigger an interrupted-body error.
        let _ = conn.shutdown().await;
    });

    // Make a real HTTP request to get a reqwest::Response with
    // an interrupted body.
    let client = reqwest::Client::new();
    let response = client.get(&server_url).send().await.unwrap();

    let result =
        filesystem::stream_response_to_destination(response, &dest, 25_000_000, true).await;

    // The operation should fail (body transfer interrupted).
    assert!(result.is_err());
    let err = result.unwrap_err();

    // The error message should contain safe primary context.
    let display = format!("{err}");
    assert!(
        display.contains("interrupted body transfer"),
        "Display must contain primary context: {display}"
    );
    // Verify every sentinel query value is omitted from Display.
    assert!(
        !display.contains(sentinel_query),
        "sentinel query value leaked in Display: {display}"
    );
    assert!(
        !display.contains(sentinel_key),
        "sentinel key value leaked in Display: {display}"
    );

    // Debug output should also be safe.
    let debug_output = format!("{err:?}");
    assert!(
        debug_output.contains("interrupted body transfer"),
        "Debug must contain primary context: {debug_output}"
    );
    // Verify every sentinel query value is omitted from Debug.
    assert!(
        !debug_output.contains(sentinel_query),
        "sentinel query value leaked in Debug: {debug_output}"
    );
    assert!(
        !debug_output.contains(sentinel_key),
        "sentinel key value leaked in Debug: {debug_output}"
    );

    // The part file should be cleaned up after the error.
    assert!(
        !dest.part_path.exists(),
        "part file should be cleaned up after interrupted transfer"
    );

    let _ = server_handle.await;
}

/// Test that a genuine body-transfer error (Content-Length larger
/// than bytes sent) produces safe Display and Debug output when the
/// request URL contains sentinel query parameters.
///
/// Uses a raw TCP server that sends a 200 response declaring more
/// data than it actually sends, then closes the connection. The
/// sentinel query value is embedded in the request URL to verify
/// it never leaks into error diagnostics.
///
/// Asserts that response.chunk() failure is reached, Display and
/// Debug omit the sentinel query values, and the .part file is
/// absent immediately afterward (cleaned up by the worker).
#[tokio::test]
async fn interrupted_body_transfer_url_safety() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    // Bind a TCP listener on a random port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");

    // Spawn the TCP server that sends partial data then closes.
    let sentinel_query = "SENTINEL-QUERY-VALUE";
    let secret_key = "SECRET";
    let server_handle = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap().0;
        // Send a 200 response declaring more data than we'll send.
        let _ = conn
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                 Content-Length: 1000\r\n\
                 Connection: close\r\n\
                 \r\n",
            )
            .await;
        // Write partial data — 100 bytes out of declared 1000.
        let _ = conn.write_all(&[0xBBu8; 100]).await;
        // Close the connection to trigger an interrupted-body error.
        let _ = conn.shutdown().await;
    });

    // Seed a pending image with a URL containing sentinel query values.
    let sentinel_playback = format!(
        "{}/picture/1?starttime={sentinel_query}&key={secret_key}",
        server_url
    );

    let (ops, _image_id) =
        seed_pending_image_at(&db_path, "key-interrupted-url-safety", &sentinel_playback).await;

    let config = make_nvr_config(&server_url);
    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    let options = DownloadWorkerOptions {
        output_directory: output_dir.clone(),
        concurrency: 1,
        retry_limit: 2,
        retry_initial_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(50),
        lease_duration: Duration::from_secs(30),
        maximum_image_size_bytes: 25_000_000,
        verify_jpeg: true,
    };

    let worker = DownloadWorker::new(ops.clone(), std::sync::Arc::new(client), options);

    // Run the worker — it will attempt the download. The authenticated
    // request receives 100 bytes of data and then the connection closes.
    // Because Content-Length is 1000 but only 100 bytes were sent,
    // reqwest::Response::chunk() returns an interrupted-body error.
    let report = worker.run_until_idle().await.unwrap();

    // The claim should have been attempted and retried.
    assert_eq!(report.claimed, 1);
    assert_eq!(report.retry_scheduled, 1);

    // Verify no .part files remain — the cleanup path should have
    // removed it after the interrupted transfer error.
    let track_id = TrackId::new("103");
    let image_key = ImageKey::new("key-interrupted-url-safety");
    let dest = image_destination(
        &output_dir,
        1,
        Some("Test Camera"),
        &now_ts(),
        &track_id,
        &image_key,
    );
    assert!(
        !dest.part_path.exists(),
        "part file should be cleaned up after interrupted transfer"
    );

    // Verify the database transitioned to retry_wait.
    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));

    let _ = server_handle.await;
}

// ── Cross-origin allowlisted transport test ───────────────────────────────

/// When rebasing is disabled and a host is in the allowlist, the
/// transport should authorize requests to that host.
#[tokio::test]
async fn cross_origin_allowlisted_transport_succeeds() {
    // NVR origin mock server.
    let nvr_server = MockServer::start().await;
    // CDN mock server (different host, same port).
    let cdn_server = MockServer::start().await;

    // NVR server: 401 challenge for normal NVR paths.
    Mock::given(method("GET"))
        .and(path("/nvr/path"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .mount(&nvr_server)
        .await;

    // CDN server: 401 challenge for playback.
    Mock::given(method("GET"))
        .and(path("/cdn/pic.jpg"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&cdn_server)
        .await;

    // CDN server: return valid JPEG after auth.
    Mock::given(method("GET"))
        .and(path("/cdn/pic.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(valid_jpeg_body()))
        .mount(&cdn_server)
        .await;

    let nvr_url = url::Url::parse(&nvr_server.uri()).unwrap();
    let nvr_host = nvr_url.host_str().unwrap().to_string();
    let nvr_port = nvr_url.port().unwrap_or(80);
    let nvr_scheme = nvr_url.scheme().to_string();

    let config = NvrConfig {
        scheme: nvr_scheme.clone(),
        host: nvr_host,
        port: nvr_port,
        username: "admin".to_string(),
        password: Some(fauna_scan::configuration::Secret::new(
            "correct-pass".to_string(),
        )),
        start_at: now_ts(),
        request_timeout_seconds: 5,
        connect_timeout_seconds: 2,
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
            rebase_playback_urls: false, // Disable rebasing
            concurrency: 2,
            playback_host_allowlist: vec![
                url::Url::parse(&cdn_server.uri())
                    .unwrap()
                    .host_str()
                    .unwrap()
                    .to_string(),
            ],
        },
    };

    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    // Fetch from the allowlisted CDN host.
    let result = client
        .fetch(&format!("{}/cdn/pic.jpg", cdn_server.uri()))
        .await;
    assert!(result.is_ok());
    let body = result.unwrap().bytes().await.unwrap();
    assert_eq!(body.as_ref(), valid_jpeg_body());
}

/// When rebasing is disabled and the host is NOT in the allowlist,
/// the transport should refuse the request.
#[tokio::test]
async fn cross_origin_not_allowlisted_transport_refused() {
    let nvr_server = MockServer::start().await;
    let other_server = MockServer::start().await;

    // NVR server: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/nvr/path"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .mount(&nvr_server)
        .await;

    let nvr_url = url::Url::parse(&nvr_server.uri()).unwrap();
    let nvr_host = nvr_url.host_str().unwrap().to_string();
    let nvr_port = nvr_url.port().unwrap_or(80);
    let nvr_scheme = nvr_url.scheme().to_string();

    let config = NvrConfig {
        scheme: nvr_scheme,
        host: nvr_host,
        port: nvr_port,
        username: "admin".to_string(),
        password: Some(fauna_scan::configuration::Secret::new(
            "correct-pass".to_string(),
        )),
        start_at: now_ts(),
        request_timeout_seconds: 5,
        connect_timeout_seconds: 2,
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
            rebase_playback_urls: false,
            concurrency: 2,
            playback_host_allowlist: vec![], // Empty allowlist
        },
    };

    let transport = build_transport(&config).await.unwrap();
    let client = build_download_client(transport, &config).await;

    // Fetch from a non-allowlisted host — should be refused.
    let result = client
        .fetch(&format!("{}/other/pic.jpg", other_server.uri()))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authorization);
}

// ── Helper: past timestamp ────────────────────────────────────────────────

fn past_ts(hours: i32) -> Timestamp {
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, 12 - hours as u32, 0, 0)
            .unwrap(),
    )
}

/// Make a retry_wait image immediately claimable by setting
/// download_next_attempt_at to a past timestamp.
async fn make_claimable(ops: &DatabaseOps, image_id: ImageId) {
    let past = past_ts(1);
    let dt = past
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("UPDATE images SET download_next_attempt_at = ?")
        .bind(&dt)
        .bind(image_id.get())
        .execute(ops.pool())
        .await
        .unwrap();
}
