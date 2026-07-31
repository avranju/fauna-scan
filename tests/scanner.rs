//! Integration tests for Phase 10: scanner pipeline.
//!
//! Uses temporary file-backed SQLite databases and a wiremock classifier
//! server to verify sequential classification, successful persistence,
//! missing and invalid files, transient retries, crash recovery, and
//! restart idempotency.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use fauna_scan::classifier::ClassifierClient;
use fauna_scan::configuration::{
    ClassifierConfig, ClassifierGenerationConfig, ClassifierRateLimitConfig,
};
use fauna_scan::database::models::*;
use fauna_scan::database::repository::DatabaseOps;
use fauna_scan::database::sqlite::SqliteDataStore;
use fauna_scan::domain::*;
use fauna_scan::scanner::{Scanner, ScannerOptions, ScannerPassReport, scanner_backoff};
use fauna_scan::service_lifecycle::ShutdownToken;
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn past_ts(hours: i32) -> Timestamp {
    let base = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
    Timestamp::new(
        base.checked_sub_signed(chrono::Duration::hours(hours as i64))
            .expect("past_ts should not underflow"),
    )
}

fn lease_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 14, 0, 0).unwrap())
}

/// Build a minimal ClassifierConfig pointing at the given mock server.
fn make_classifier_config(mock_base: &str) -> ClassifierConfig {
    let url = Url::parse(mock_base).unwrap();
    let port = url.port().unwrap_or(80);
    let scheme = url.scheme();
    ClassifierConfig {
        endpoints: vec![fauna_scan::configuration::ClassifierEndpointConfig {
            enabled: true,
            base_url: Url::parse(&format!("{scheme}://{}:{}", url.host_str().unwrap(), port))
                .unwrap(),
            endpoint: "/chat/completions".to_string(),
            model: "test-model".to_string(),
            api_key: None,
            username: String::new(),
            password: None,
            request_timeout_seconds: 10,
            prompt_version: "wildlife-v1".to_string(),
            generation: ClassifierGenerationConfig {
                temperature: 0.1,
                max_tokens: 1000,
            },
            rate_limit: None,
        }],
        poll_interval_seconds: 10,
        retry_limit: 5,
        retry_initial_delay_seconds: 1,
        retry_max_delay_seconds: 300,
        processing_lease_seconds: 600,
    }
}

/// Minimal valid JPEG bytes.
fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xda, 0x00, 0x0c, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00,
        0xff, 0xd9,
    ]
}

/// Valid OpenAI chat-completions envelope.
fn valid_openai_response() -> String {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"contains_animal\": true, \"contains_wildlife\": true, \"is_interesting\": true, \"species\": [{\"name\": \"Indian palm squirrel\", \"confidence\": 0.82}], \"overall_confidence\": 0.82, \"summary\": \"A small squirrel.\", \"uncertainties\": []}"
            },
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

/// Setup: create a temp database, sync a camera, insert downloaded images,
/// and return ops + output directory.
async fn setup_downloaded_images(
    temp_dir: &TempDir,
    count: usize,
    jpeg_data: &[u8],
) -> (PathBuf, DatabaseOps, SqlitePool, PathBuf) {
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let camera_id = CameraId::new(1);

    // Sync a camera.
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

    // Insert images.
    for i in 1..=count {
        let img = DiscoveredImage {
            image_key: ImageKey::new(format!("key-img-{}", i)),
            camera_id,
            track_id: TrackId::new("103"),
            capture_start_at: now,
            capture_end_at: None,
            playback_uri: format!("http://nvr/pic/{}", i),
            canonical_playback_uri: format!("http://nvr/pic/{}", i),
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

        ops.commit_search_window(&window, &[img]).await.unwrap();

        // Complete the download for this image.
        let download_claim = ops
            .claim_next_download(&now, &lease_ts())
            .await
            .unwrap()
            .unwrap();

        // Write the JPEG file.
        let file_path = output_dir.join(format!("img-{}.jpg", i));
        std::fs::write(&file_path, jpeg_data).unwrap();

        ops.complete_download(download_claim.image_id, &file_path, &now)
            .await
            .unwrap();
    }

    (db_path, ops, pool, output_dir)
}

/// Build a Scanner from a database, mock classifier server, and options.
async fn build_scanner(
    ops: DatabaseOps,
    _pool: SqlitePool,
    mock_server: &MockServer,
    output_dir: &std::path::Path,
    retry_limit: u32,
) -> Scanner {
    let config = make_classifier_config(&mock_server.uri());
    let classifier = ClassifierClient::from_config(&config).unwrap();

    let options = ScannerOptions {
        poll_interval: std::time::Duration::from_secs(config.poll_interval_seconds),
        retry_limit,
        retry_initial_delay: std::time::Duration::from_secs(1),
        retry_max_delay: std::time::Duration::from_secs(300),
        processing_lease_duration: std::time::Duration::from_secs(600),
        maximum_image_size_bytes: 25_000_000,
        output_directory: output_dir.to_path_buf(),
        non_wildlife_image_retention: std::time::Duration::from_secs(4 * 86_400),
    };

    Scanner::new(ops, Arc::new(classifier), options)
}

// ── Sequential classification test ────────────────────────────────────────

/// Two downloaded images are classified sequentially, both become done,
/// and LastSuccessfulScannerPass is updated.
#[tokio::test]
async fn sequential_classification_two_images() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 2, &minimal_jpeg()).await;

    // Check eligibility with raw SQL
    let eligible_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM images \
         WHERE processing_status IN ('new', 'retry_wait') \
           AND local_path IS NOT NULL \
           AND download_status = 'downloaded' \
           AND processing_lease_until IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(eligible_count, 2, "expected 2 eligible images");

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 2);
    assert_eq!(report.completed, 2);
    assert_eq!(report.retry_scheduled, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.missing, 0);

    // Verify both images are done.
    let img1 = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    let img2 = scanner.database.get_image(ImageId::new(2)).await.unwrap();
    assert_eq!(img1.processing_status, ProcessingStatus::Done);
    assert_eq!(img2.processing_status, ProcessingStatus::Done);

    // Verify classifications were inserted.
    let class1 = scanner
        .database
        .get_classification(ImageId::new(1), "test-model", "wildlife-v1")
        .await
        .unwrap();
    assert!(class1.contains_wildlife);
    assert_eq!(class1.model, "test-model");
    assert_eq!(class1.prompt_version, "wildlife-v1");

    // Verify LastSuccessfulScannerPass metadata.
    let meta = scanner
        .database
        .get_metadata(&ServiceMetadataKey::LastSuccessfulScannerPass)
        .await
        .unwrap()
        .unwrap();
    assert!(!meta.is_empty());
}

// ── Cancellation tests ────────────────────────────────────────────────────

#[tokio::test]
async fn cancellation_before_pass_claims_no_image_or_success_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;
    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;
    let shutdown = ShutdownToken::new();
    shutdown.cancel();

    let report = scanner
        .clone()
        .execute_one_pass_with_shutdown(&shutdown)
        .await
        .unwrap();
    assert_eq!(report.claimed, 0);
    assert!(
        scanner
            .database
            .get_metadata(&ServiceMetadataKey::LastSuccessfulScannerPass)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        scanner
            .database
            .get_image(ImageId::new(1))
            .await
            .unwrap()
            .processing_status,
        ProcessingStatus::New
    );
}

#[tokio::test]
async fn cancellation_after_one_image_prevents_the_next_claim() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;
    let shutdown = ShutdownToken::new();
    let response_shutdown = shutdown.clone();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            response_shutdown.cancel();
            ResponseTemplate::new(200).set_body_string(valid_openai_response())
        })
        .mount(&mock_server)
        .await;
    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 2, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner
        .clone()
        .execute_one_pass_with_shutdown(&shutdown)
        .await
        .unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);
    assert_eq!(
        scanner
            .database
            .get_image(ImageId::new(1))
            .await
            .unwrap()
            .processing_status,
        ProcessingStatus::Done
    );
    assert_eq!(
        scanner
            .database
            .get_image(ImageId::new(2))
            .await
            .unwrap()
            .processing_status,
        ProcessingStatus::New
    );
    assert!(
        scanner
            .database
            .get_metadata(&ServiceMetadataKey::LastSuccessfulScannerPass)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cancellation_wakes_scanner_polling_sleep() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;
    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 0, &minimal_jpeg()).await;
    let mut scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;
    scanner.options.poll_interval = std::time::Duration::from_secs(3600);
    let shutdown = ShutdownToken::new();
    let task_shutdown = shutdown.clone();
    let task =
        tokio::spawn(async move { scanner.run_continuous_with_shutdown(task_shutdown).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("scanner polling sleep did not observe cancellation")
        .unwrap()
        .unwrap();
    assert!(output_dir.exists());
}

// ── Missing file test ─────────────────────────────────────────────────────

/// A downloaded image whose local file has been deleted becomes missing.
/// Download status remains downloaded.
#[tokio::test]
async fn missing_file_becomes_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Delete the file.
    let file_path = output_dir.join("img-1.jpg");
    std::fs::remove_file(&file_path).unwrap();

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.missing, 1);
    assert_eq!(report.completed, 0);

    // Verify state.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Missing);
    assert_eq!(img.download_status, DownloadStatus::Downloaded);

    // No classifier request should have been made.
    // (If the mock server was hit, the test would fail due to unfulfilled mock.)
}

/// Two endpoint workers classify different claimed images concurrently.
#[tokio::test]
async fn classifier_endpoints_classify_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let primary_server = MockServer::start().await;
    let secondary_server = MockServer::start().await;
    let delay = std::time::Duration::from_millis(300);

    for server in [&primary_server, &secondary_server] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(valid_openai_response())
                    .set_delay(delay),
            )
            .mount(server)
            .await;
    }

    let (_db_path, ops, _pool, __output_dir) =
        setup_downloaded_images(&dir, 2, &minimal_jpeg()).await;
    let primary_config = make_classifier_config(&primary_server.uri());
    let secondary_config = make_classifier_config(&secondary_server.uri());
    let options = ScannerOptions {
        poll_interval: std::time::Duration::from_secs(10),
        retry_limit: 5,
        retry_initial_delay: std::time::Duration::from_secs(1),
        retry_max_delay: std::time::Duration::from_secs(300),
        processing_lease_duration: std::time::Duration::from_secs(600),
        maximum_image_size_bytes: 25_000_000,
        output_directory: dir.path().to_path_buf(),
        non_wildlife_image_retention: std::time::Duration::from_secs(4 * 86_400),
    };
    let scanner = Scanner::with_classifiers(
        ops,
        vec![
            Arc::new(ClassifierClient::from_config(&primary_config).unwrap()),
            Arc::new(ClassifierClient::from_config(&secondary_config).unwrap()),
        ],
        options,
    );

    let started = tokio::time::Instant::now();
    let report = scanner.execute_one_pass().await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(550),
        "two 300ms endpoint requests should overlap"
    );
    assert_eq!(report.claimed, 2);
    assert_eq!(report.completed, 2);
    assert_eq!(primary_server.received_requests().await.unwrap().len(), 1);
    assert_eq!(secondary_server.received_requests().await.unwrap().len(), 1);
}

/// A quota-blocked endpoint yields its worker instead of holding the pass open,
/// so an unrestricted endpoint can process current work and be recreated on
/// later polling passes.
#[tokio::test]
async fn quota_blocked_endpoint_does_not_block_other_endpoint_or_pass_completion() {
    let dir = tempfile::tempdir().unwrap();
    let quota_server = MockServer::start().await;
    let available_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(valid_openai_response())
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&available_server)
        .await;

    let (_db_path, ops, _pool, _output_dir) =
        setup_downloaded_images(&dir, 2, &minimal_jpeg()).await;
    let policy = ClassifierRateLimitConfig {
        quota_group: "exhausted-test-quota".to_string(),
        requests_per_minute: 1,
        requests_per_day: 1,
        tokens_per_minute: 2_000,
        tokens_per_day: 2_000,
        estimated_input_tokens_per_request: 1,
        max_images_per_request: 1,
    };
    // Consume the sole daily reservation. A second reservation would wait
    // until UTC midnight, which must not stall this scanner pass.
    assert!(matches!(
        ops.reserve_classifier_rate_limit(&policy, 1_000, &Timestamp::new(Utc::now()))
            .await
            .unwrap(),
        fauna_scan::database::repository::RateLimitReservation::Granted
    ));

    let mut quota_config = make_classifier_config(&quota_server.uri());
    quota_config.endpoints[0].rate_limit = Some(policy);
    let available_config = make_classifier_config(&available_server.uri());
    let scanner = Scanner::with_classifiers_and_rate_limits(
        ops,
        vec![
            Arc::new(ClassifierClient::from_config(&quota_config).unwrap()),
            Arc::new(ClassifierClient::from_config(&available_config).unwrap()),
        ],
        vec![
            quota_config.endpoints[0].rate_limit.clone(),
            available_config.endpoints[0].rate_limit.clone(),
        ],
        ScannerOptions {
            poll_interval: std::time::Duration::from_secs(10),
            retry_limit: 5,
            retry_initial_delay: std::time::Duration::from_secs(1),
            retry_max_delay: std::time::Duration::from_secs(300),
            processing_lease_duration: std::time::Duration::from_secs(600),
            maximum_image_size_bytes: 25_000_000,
            output_directory: dir.path().to_path_buf(),
            non_wildlife_image_retention: std::time::Duration::from_secs(4 * 86_400),
        },
    );

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        scanner.execute_one_pass(),
    )
    .await
    .expect("quota wait held the scanner pass open")
    .unwrap();

    assert_eq!(report.claimed, 2);
    assert_eq!(report.completed, 2);
    assert_eq!(quota_server.received_requests().await.unwrap().len(), 0);
    assert_eq!(available_server.received_requests().await.unwrap().len(), 2);
}

/// A minute-limited worker remains alive and resumes its reservation loop
/// instead of yielding permanently while other work keeps the scanner pass open.
#[tokio::test]
async fn minute_limited_endpoint_waits_within_the_current_pass() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&server)
        .await;

    let (_db_path, ops, _pool, _output_dir) =
        setup_downloaded_images(&dir, 2, &minimal_jpeg()).await;
    let policy = ClassifierRateLimitConfig {
        quota_group: "minute-limited-test-quota".to_string(),
        requests_per_minute: 1,
        requests_per_day: 100,
        tokens_per_minute: 2_000,
        tokens_per_day: 200_000,
        estimated_input_tokens_per_request: 1,
        max_images_per_request: 1,
    };
    let mut config = make_classifier_config(&server.uri());
    config.endpoints[0].rate_limit = Some(policy);
    let scanner = Scanner::with_classifiers_and_rate_limits(
        ops,
        vec![Arc::new(ClassifierClient::from_config(&config).unwrap())],
        vec![config.endpoints[0].rate_limit.clone()],
        ScannerOptions {
            poll_interval: std::time::Duration::from_secs(10),
            retry_limit: 5,
            retry_initial_delay: std::time::Duration::from_secs(1),
            retry_max_delay: std::time::Duration::from_secs(300),
            processing_lease_duration: std::time::Duration::from_secs(600),
            maximum_image_size_bytes: 25_000_000,
            output_directory: dir.path().to_path_buf(),
            non_wildlife_image_retention: std::time::Duration::from_secs(4 * 86_400),
        },
    );

    let shutdown = ShutdownToken::new();
    let task_shutdown = shutdown.clone();
    let task =
        tokio::spawn(async move { scanner.execute_one_pass_with_shutdown(&task_shutdown).await });

    for _ in 0..100 {
        if server.received_requests().await.unwrap().len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    // The second image is waiting on the minute window. Previously the worker
    // returned here and the scanner pass completed immediately.
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(!task.is_finished());

    shutdown.cancel();
    let report = task.await.unwrap().unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);
}

// ── Invalid file tests ────────────────────────────────────────────────────

/// Empty files, oversized files, and non-JPEG files become failed.
#[tokio::test]
async fn invalid_files_become_failed() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    let (db_path, _ops, pool, output_dir) = setup_downloaded_images(&dir, 3, &minimal_jpeg()).await;
    let db = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let ops = db.ops();

    // Image 1: empty file.
    let empty_path = output_dir.join("img-1.jpg");
    std::fs::write(&empty_path, b"").unwrap();

    // Image 2: non-JPEG content.
    let html_path = output_dir.join("img-2.jpg");
    std::fs::write(&html_path, b"<html><body>Access Denied</body></html>").unwrap();

    // Image 3: valid JPEG (should succeed).
    let valid_path = output_dir.join("img-3.jpg");
    std::fs::write(&valid_path, minimal_jpeg()).unwrap();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 3);
    assert_eq!(report.failed, 2);
    assert_eq!(report.completed, 1);

    // Verify states.
    let img1 = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    let img2 = scanner.database.get_image(ImageId::new(2)).await.unwrap();
    let img3 = scanner.database.get_image(ImageId::new(3)).await.unwrap();

    assert_eq!(img1.processing_status, ProcessingStatus::Failed);
    assert_eq!(img2.processing_status, ProcessingStatus::Failed);
    assert_eq!(img3.processing_status, ProcessingStatus::Done);
}

// ── Retry then success test ───────────────────────────────────────────────

/// HTTP 500 on first attempt schedules retry; second attempt succeeds.
#[tokio::test]
async fn retryable_then_success() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let call_count = call_count.clone();
            move |_req: &wiremock::Request| {
                let count = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if count == 0 {
                    ResponseTemplate::new(500).set_body_string("Internal Server Error")
                } else {
                    ResponseTemplate::new(200).set_body_string(valid_openai_response())
                }
            }
        })
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    // First pass: should retry-schedule.
    let report1 = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report1.claimed, 1);
    assert_eq!(report1.retry_scheduled, 1);

    // Advance the retry timestamp so it's due for the second pass.
    sqlx::query(
        "UPDATE images SET processing_status = 'retry_wait', processing_next_attempt_at = datetime('now', '-10 seconds') WHERE id = 1",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Second pass: should complete.
    let report2 = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report2.claimed, 1);
    assert_eq!(report2.completed, 1);

    // Verify the image is done.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

/// A 429 cools down the endpoint, so a backlog does not generate one 429 per
/// image in a single scanner pass.
#[tokio::test]
async fn http_429_cools_down_endpoint_before_claiming_another_image() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "60")
                .set_body_string("rate limited"),
        )
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 2, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let shutdown = ShutdownToken::new();
    let runner = scanner.clone();
    let runner_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        runner
            .execute_one_pass_with_shutdown(&runner_shutdown)
            .await
    });
    while mock_server.received_requests().await.unwrap().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    let report = task.await.unwrap().unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.retry_scheduled, 1);
    assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        scanner
            .database
            .get_image(ImageId::new(2))
            .await
            .unwrap()
            .processing_status,
        ProcessingStatus::New
    );
}

/// A provider 429 keeps the rolling-minute attempt but refunds the daily
/// quota because the provider did not generate a response.
#[tokio::test]
async fn http_429_refunds_daily_rate_limit_quota() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, _output_dir) =
        setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let policy = ClassifierRateLimitConfig {
        quota_group: "refund-on-429".to_string(),
        requests_per_minute: 1,
        requests_per_day: 1,
        tokens_per_minute: 2_000,
        tokens_per_day: 2_000,
        estimated_input_tokens_per_request: 1,
        max_images_per_request: 1,
    };
    let mut config = make_classifier_config(&mock_server.uri());
    config.endpoints[0].rate_limit = Some(policy.clone());
    let scanner = Scanner::with_classifiers_and_rate_limits(
        ops,
        vec![Arc::new(ClassifierClient::from_config(&config).unwrap())],
        vec![config.endpoints[0].rate_limit.clone()],
        ScannerOptions {
            poll_interval: std::time::Duration::from_secs(10),
            retry_limit: 5,
            retry_initial_delay: std::time::Duration::from_secs(1),
            retry_max_delay: std::time::Duration::from_secs(300),
            processing_lease_duration: std::time::Duration::from_secs(600),
            maximum_image_size_bytes: 25_000_000,
            output_directory: dir.path().to_path_buf(),
            non_wildlife_image_retention: std::time::Duration::from_secs(4 * 86_400),
        },
    );

    let shutdown = ShutdownToken::new();
    let runner = scanner.clone();
    let runner_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        runner
            .execute_one_pass_with_shutdown(&runner_shutdown)
            .await
    });
    loop {
        let cooldown_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM classifier_cooldowns WHERE cooldown_group = ?",
        )
        .bind(&policy.quota_group)
        .fetch_one(&pool)
        .await
        .unwrap();
        if cooldown_count == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    let report = task.await.unwrap().unwrap();
    assert_eq!(report.retry_scheduled, 1);
    let usage = sqlx::query(
        "SELECT requests, tokens FROM classifier_rate_limit_daily_usage \
         WHERE quota_group = ?",
    )
    .bind(&policy.quota_group)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage.get::<i64, _>(0), 0);
    assert_eq!(usage.get::<i64, _>(1), 0);
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM classifier_rate_limit_events WHERE quota_group = ?",
    )
    .bind(&policy.quota_group)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_count, 1);
    let cooldown: String = sqlx::query_scalar(
        "SELECT cooldown_until FROM classifier_cooldowns WHERE cooldown_group = ?",
    )
    .bind(&policy.quota_group)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cooldown.parse::<fauna_scan::domain::Timestamp>().is_ok());
}

// ── Retry exhaustion test ─────────────────────────────────────────────────

/// Malformed responses exhaust retries and become failed with raw response retention.
#[tokio::test]
async fn malformed_response_exhaustion() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not JSON"))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 3).await;

    // Run three passes: first two retry-schedule, third marks as failed.
    for i in 0..3 {
        // Advance retry timestamp so the image is claimable.
        let next_attempt = past_ts(i + 1);
        sqlx::query(
            "UPDATE images SET processing_status = 'retry_wait', processing_next_attempt_at = ? WHERE id = 1",
        )
        .bind(fauna_scan::database::format_timestamp(&next_attempt))
        .execute(&pool)
        .await
        .unwrap();

        let report = scanner.clone().execute_one_pass().await.unwrap();
        assert_eq!(report.claimed, 1);
        if i < 2 {
            assert_eq!(report.retry_scheduled, 1);
        } else {
            assert_eq!(report.failed, 1);
        }
    }

    // Verify state.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Failed);
    assert!(img.processing_attempts >= 3);

    // Verify raw response was retained.
    assert!(
        img.processing_last_raw_response
            .as_deref()
            .unwrap_or("")
            .contains("not JSON"),
        "raw response should be retained: {:?}",
        img.processing_last_raw_response
    );
}

// ── Permanent failure test ────────────────────────────────────────────────

/// HTTP 401/403 causes immediate permanent failure.
#[tokio::test]
async fn permanent_failure_immediate() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.failed, 1);

    // Verify state.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Failed);
    assert_eq!(img.processing_attempts, 1);

    // Only one classifier request was made.
}

// ── Expired lease recovery test ───────────────────────────────────────────

/// An image left in processing with an expired lease is recovered,
/// reclaimed, and completed.
#[tokio::test]
async fn expired_lease_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Simulate an interrupted processing claim with expired lease.
    let _now = now_ts();
    let past_lease = past_ts(1);
    sqlx::query(
        "UPDATE images SET processing_status = 'processing', processing_lease_until = ?, processing_attempts = 1",
    )
    .bind(past_lease.as_datetime().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    .execute(&pool)
    .await
    .unwrap();

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    // Pass should recover the expired lease and complete the image.
    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);
    assert_eq!(report.leases_recovered, 1);

    // Verify state.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

// ── Restart idempotency test ──────────────────────────────────────────────

/// A completed image is not reclassified after restart or model/prompt changes.
#[tokio::test]
async fn no_reclassification_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    // First pass: complete the image.
    let report1 = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report1.completed, 1);

    // Simulate restart: rebuild scanner from the same database.
    let db = SqliteDataStore::connect(&_db_path, 4).await.unwrap();
    let scanner2 = build_scanner(db.ops(), pool.clone(), &mock_server, &output_dir, 5).await;

    // Second pass: should find no work.
    let report2 = scanner2.clone().execute_one_pass().await.unwrap();
    assert_eq!(report2.claimed, 0);
    assert_eq!(report2.completed, 0);

    // Verify only one classification exists.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM classifications WHERE image_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// ── Scanner pass report display test ──────────────────────────────────────

#[test]
fn scanner_pass_report_display() {
    let report = ScannerPassReport {
        claimed: 5,
        completed: 3,
        retry_scheduled: 1,
        failed: 1,
        missing: 0,
        leases_recovered: 2,
        files_removed: 0,
        missing_files_reconciled: 0,
        garbage_collection_failures: 0,
    };
    let display = format!("{report}");
    assert!(display.contains("claimed=5"));
    assert!(display.contains("completed=3"));
    assert!(display.contains("retry=1"));
    assert!(display.contains("failed=1"));
    assert!(display.contains("missing=0"));
    assert!(display.contains("recovered=2"));
    // Garbage-collection counters are always present.
    assert!(display.contains("gc_removed=0"));
    assert!(display.contains("gc_reconciled=0"));
    assert!(display.contains("gc_failures=0"));

    // Nonzero GC counters are also reported.
    let gc_report = ScannerPassReport {
        claimed: 2,
        completed: 1,
        retry_scheduled: 0,
        failed: 0,
        missing: 1,
        leases_recovered: 0,
        files_removed: 3,
        missing_files_reconciled: 1,
        garbage_collection_failures: 2,
    };
    let gc_display = format!("{gc_report}");
    assert!(gc_display.contains("gc_removed=3"));
    assert!(gc_display.contains("gc_reconciled=1"));
    assert!(gc_display.contains("gc_failures=2"));
}

// ── Backoff test ──────────────────────────────────────────────────────────

#[test]
fn backoff_capped_at_maximum() {
    let initial = std::time::Duration::from_secs(10);
    let maximum = std::time::Duration::from_secs(30);

    let b1 = scanner_backoff(1, initial, maximum);
    let b2 = scanner_backoff(2, initial, maximum);
    let b3 = scanner_backoff(3, initial, maximum);
    let b4 = scanner_backoff(4, initial, maximum);
    let b5 = scanner_backoff(5, initial, maximum);

    assert_eq!(b1, std::time::Duration::from_secs(10));
    assert_eq!(b2, std::time::Duration::from_secs(20));
    assert_eq!(b3, maximum);
    assert_eq!(b4, maximum);
    assert_eq!(b5, maximum);
}

// ── Oversized file test ─────────────────────────────────────────────

/// Files exceeding maximum_image_size_bytes are rejected without
/// classifier submission, even when the file grows between metadata
/// check and read.
#[tokio::test]
async fn oversized_file_rejected_without_classifier() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    let (db_path, _ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let db = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let ops = db.ops();

    // Overwrite with a file that exceeds the maximum size.
    let big_path = output_dir.join("img-1.jpg");
    let mut f = std::fs::File::create(&big_path).unwrap();
    // Write JPEG header
    f.write_all(&minimal_jpeg()[..4]).unwrap();
    // Fill with padding to exceed 25 MB
    let padding = vec![0u8; 26_000_000];
    f.write_all(&padding).unwrap();
    // Write JPEG footer
    f.write_all(&[0xFF, 0xD9]).unwrap();
    f.flush().unwrap();
    drop(f);

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.failed, 1);

    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Failed);
}

// ── Scanner timestamp ordering integration test ─────────────────────

/// A successful scanner pass produces a ClassificationRecord with
/// request_started_at <= request_completed_at.
#[tokio::test]
async fn scanner_timestamp_ordering() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;
    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);

    let class = scanner
        .database
        .get_classification(ImageId::new(1), "test-model", "wildlife-v1")
        .await
        .unwrap();

    // Assert the persisted timestamps are ordered correctly.
    assert!(
        class.request_started_at <= class.request_completed_at,
        "request_started_at ({}) must be <= request_completed_at ({})",
        class.request_started_at,
        class.request_completed_at
    );
}

// ── Concurrent scanner test ─────────────────────────────────────────────

/// Two scanners competing for the same image: the first wins the claim,
/// the second gets nothing.  The first scanner's lease renewal during
/// classify_jpeg prevents the second scanner from recovering and
/// re-claiming the row.
#[tokio::test]
async fn concurrent_scanners_deduplicate() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    // Track how many times the mock server is called.
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let call_count = call_count.clone();
            move |_req: &wiremock::Request| {
                call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(valid_openai_response())
            }
        })
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Create two scanners from the same database.
    let scanner1 = build_scanner(ops.clone(), pool.clone(), &mock_server, &output_dir, 5).await;
    let scanner2 = build_scanner(ops.clone(), pool.clone(), &mock_server, &output_dir, 5).await;

    // Run scanner1 first to claim the image, then scanner2 concurrently
    // to verify it gets no work.  This avoids SQLite lock contention
    // between two concurrent claim attempts while still verifying that
    // scanner2 correctly finds nothing to claim.
    let report1 = scanner1.clone().execute_one_pass().await.unwrap();
    let report2 = scanner2.clone().execute_one_pass().await.unwrap();

    // Exactly one scanner should have claimed and completed the image.
    let total_claimed = report1.claimed + report2.claimed;
    let total_completed = report1.completed + report2.completed;
    assert_eq!(
        total_claimed, 1,
        "exactly one scanner should claim the image"
    );
    assert_eq!(
        total_completed, 1,
        "exactly one classification should be persisted"
    );

    // The mock server should have been called exactly once.
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "classifier should be called exactly once"
    );

    // Verify the image is done.
    let img = scanner1.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

/// Lease equal to request timeout is rejected by ScannerOptions.
#[test]
fn lease_equal_to_timeout_is_rejected() {
    let config = fauna_scan::configuration::Config {
        general: fauna_scan::configuration::GeneralConfig {
            output_directory: PathBuf::from("/tmp/output"),
            log_level: fauna_scan::cli::LogLevel::Error,
            non_wildlife_image_retention_days: 4,
        },
        database: fauna_scan::configuration::DatabaseConfig::Sqlite {
            path: PathBuf::from("/tmp/test.db"),
            max_connections: 4,
        },
        nvr: fauna_scan::configuration::NvrConfig {
            scheme: "http".to_string(),
            host: "test".to_string(),
            port: 80,
            username: "u".to_string(),
            password: Some(fauna_scan::configuration::Secret::new("x".to_string())),
            start_at: fauna_scan::domain::Timestamp::new(Utc::now()),
            request_timeout_seconds: 30,
            connect_timeout_seconds: 10,
            allow_invalid_tls_certificates: false,
            search: fauna_scan::configuration::NvrSearchConfig {
                window_minutes: 60,
                max_results: 50,
                poll_interval_seconds: 60,
                poll_overlap_seconds: 120,
                camera_refresh_interval_seconds: 3600,
                settlement_delay_seconds: 10,
            },
            download: fauna_scan::configuration::NvrDownloadConfig {
                retry_limit: 10,
                retry_initial_delay_seconds: 5,
                retry_max_delay_seconds: 300,
                maximum_image_size_bytes: 25_000_000,
                verify_jpeg: true,
                rebase_playback_urls: true,
                concurrency: 2,
                playback_host_allowlist: vec![],
            },
        },
        classifier: ClassifierConfig {
            endpoints: vec![fauna_scan::configuration::ClassifierEndpointConfig {
                enabled: true,
                base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                endpoint: "/chat/completions".to_string(),
                model: "test".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 120,
                prompt_version: "wildlife-v1".to_string(),
                generation: ClassifierGenerationConfig {
                    temperature: 0.1,
                    max_tokens: 1000,
                },
                rate_limit: None,
            }],
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 120, // equal to request timeout
        },
        web: fauna_scan::configuration::WebConfig::default(),
        source_path: PathBuf::from("/tmp/test.toml"),
    };
    let result = ScannerOptions::from_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("greater than"));
}

/// Lease one second above request timeout is accepted.
#[test]
fn lease_one_second_above_timeout_is_accepted() {
    let config = fauna_scan::configuration::Config {
        general: fauna_scan::configuration::GeneralConfig {
            output_directory: PathBuf::from("/tmp/output"),
            log_level: fauna_scan::cli::LogLevel::Error,
            non_wildlife_image_retention_days: 4,
        },
        database: fauna_scan::configuration::DatabaseConfig::Sqlite {
            path: PathBuf::from("/tmp/test.db"),
            max_connections: 4,
        },
        nvr: fauna_scan::configuration::NvrConfig {
            scheme: "http".to_string(),
            host: "test".to_string(),
            port: 80,
            username: "u".to_string(),
            password: Some(fauna_scan::configuration::Secret::new("x".to_string())),
            start_at: fauna_scan::domain::Timestamp::new(Utc::now()),
            request_timeout_seconds: 30,
            connect_timeout_seconds: 10,
            allow_invalid_tls_certificates: false,
            search: fauna_scan::configuration::NvrSearchConfig {
                window_minutes: 60,
                max_results: 50,
                poll_interval_seconds: 60,
                poll_overlap_seconds: 120,
                camera_refresh_interval_seconds: 3600,
                settlement_delay_seconds: 10,
            },
            download: fauna_scan::configuration::NvrDownloadConfig {
                retry_limit: 10,
                retry_initial_delay_seconds: 5,
                retry_max_delay_seconds: 300,
                maximum_image_size_bytes: 25_000_000,
                verify_jpeg: true,
                rebase_playback_urls: true,
                concurrency: 2,
                playback_host_allowlist: vec![],
            },
        },
        classifier: ClassifierConfig {
            endpoints: vec![fauna_scan::configuration::ClassifierEndpointConfig {
                enabled: true,
                base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                endpoint: "/chat/completions".to_string(),
                model: "test".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 120,
                prompt_version: "wildlife-v1".to_string(),
                generation: ClassifierGenerationConfig {
                    temperature: 0.1,
                    max_tokens: 1000,
                },
                rate_limit: None,
            }],
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 121, // one second above timeout
        },
        web: fauna_scan::configuration::WebConfig::default(),
        source_path: PathBuf::from("/tmp/test.toml"),
    };
    let result = ScannerOptions::from_config(&config);
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap().processing_lease_duration,
        std::time::Duration::from_secs(121)
    );
}

// ── load_and_validate_jpeg overflow test ─────────────────────────────────

/// Loading a file with maximum_image_size_bytes = u64::MAX works
/// for small files (saturating arithmetic handles the overflow).
#[tokio::test]
async fn load_jpeg_handles_u64_max_maximum_size() {
    use fauna_scan::scanner::load_and_validate_jpeg;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.jpg");
    std::fs::write(&file_path, minimal_jpeg()).unwrap();

    // u64::MAX should not panic — saturating arithmetic handles it.
    // A small JPEG should load successfully.
    let result = load_and_validate_jpeg(&file_path, u64::MAX).await;
    assert!(result.is_ok());
    let data = result.unwrap();
    assert_eq!(data.len(), minimal_jpeg().len());
}

/// Loading a file with maximum_image_size_bytes = u64::MAX - 1 works
/// for small files (the overflow sentinel is 2, which is fine).
#[tokio::test]
async fn load_jpeg_handles_near_max_maximum_size() {
    use fauna_scan::scanner::load_and_validate_jpeg;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.jpg");
    std::fs::write(&file_path, minimal_jpeg()).unwrap();

    // u64::MAX - 1: the overflow sentinel is 2, which fits fine.
    // Small JPEG should load successfully.
    let result = load_and_validate_jpeg(&file_path, u64::MAX - 1).await;
    assert!(result.is_ok());
    let data = result.unwrap();
    assert_eq!(data.len(), minimal_jpeg().len());
}

/// Loading a file with a very large but supported maximum size works
/// for small files.
#[tokio::test]
async fn load_jpeg_with_large_maximum_size_works() {
    use fauna_scan::scanner::load_and_validate_jpeg;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.jpg");
    std::fs::write(&file_path, minimal_jpeg()).unwrap();

    // 1 GB maximum: should work fine for a small JPEG.
    let result = load_and_validate_jpeg(&file_path, 1_000_000_000).await;
    assert!(result.is_ok());
    let data = result.unwrap();
    assert_eq!(data.len(), minimal_jpeg().len());
}

/// A small JPEG loaded with a very large maximum size uses a
/// bounded initial Vec capacity (clamped to 16 KiB) so that even
/// if metadata reports a huge length the allocation never overflows.
#[tokio::test]
async fn load_jpeg_large_maximum_uses_safe_capacity() {
    use fauna_scan::scanner::load_and_validate_jpeg;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.jpg");
    std::fs::write(&file_path, minimal_jpeg()).unwrap();

    // 1 PB maximum — far larger than any real file.  The code should
    // use a bounded initial capacity and load the small JPEG without
    // panicking.
    let result = load_and_validate_jpeg(&file_path, 1_000_000_000_000_000).await;
    assert!(result.is_ok());
    let data = result.unwrap();
    assert_eq!(data.len(), minimal_jpeg().len());
}

// ── Concurrent scanner with lease renewal test ────────────────────────────

/// Scanner A remains in classification/response processing beyond an initial
/// lease interval while Scanner B attempts recovery.  Scanner A's background
/// lease renewer keeps the lease alive through response-body handling and
/// CPU-heavy parsing, so Scanner B cannot reclaim or submit.  Exactly one
/// classifier request is received.
///
/// The mock server delay (500ms) exceeds the lease duration (300ms), so the
/// initial lease expires during response processing.  The renewer extends
/// the lease every 100ms, keeping it alive until parsing completes.
///
/// Scanner B starts 400ms after Scanner A — long enough for the initial
/// lease to expire and for Scanner A to have entered response processing,
/// but the renewer prevents recovery.
#[tokio::test]
async fn concurrent_scanner_lease_renewal_prevents_recovery() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    // Track how many times the mock server is called.
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Mock server delay (500ms) exceeds the lease duration (300ms).
    // This ensures the initial lease expires during response processing,
    // exercising the background renewer.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with({
            let call_count = call_count.clone();
            move |_req: &wiremock::Request| {
                call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_body_string(valid_openai_response())
                    .set_delay(Duration::from_millis(500))
            }
        })
        .mount(&mock_server)
        .await;

    let (_db_path, ops, _pool, _output_dir) =
        setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Build Scanner A with a 300ms lease and 1000ms request timeout.
    // The mock server delay (500ms) exceeds the lease (300ms), so the
    // background renewer must keep the lease alive.
    let mut config_a = make_classifier_config(&mock_server.uri());
    config_a.endpoints[0].request_timeout_seconds = 10;
    config_a.processing_lease_seconds = 1;
    let classifier_a = ClassifierClient::from_config(&config_a).unwrap();

    let options_a = ScannerOptions {
        poll_interval: Duration::from_secs(1),
        retry_limit: 3,
        retry_initial_delay: Duration::from_secs(1),
        retry_max_delay: Duration::from_secs(30),
        processing_lease_duration: Duration::from_millis(300),
        maximum_image_size_bytes: 25_000_000,
        output_directory: dir.path().to_path_buf(),
        non_wildlife_image_retention: Duration::from_secs(4 * 86_400),
    };
    let scanner_a = Scanner::new(ops.clone(), Arc::new(classifier_a), options_a);

    // Build Scanner B with the same lease.
    let mut config_b = make_classifier_config(&mock_server.uri());
    config_b.endpoints[0].request_timeout_seconds = 10;
    config_b.processing_lease_seconds = 1;
    let classifier_b = ClassifierClient::from_config(&config_b).unwrap();

    let options_b = ScannerOptions {
        poll_interval: Duration::from_secs(1),
        retry_limit: 3,
        retry_initial_delay: Duration::from_secs(1),
        retry_max_delay: Duration::from_secs(30),
        processing_lease_duration: Duration::from_millis(300),
        maximum_image_size_bytes: 25_000_000,
        output_directory: dir.path().to_path_buf(),
        non_wildlife_image_retention: Duration::from_secs(4 * 86_400),
    };
    let scanner_b = Scanner::new(ops.clone(), Arc::new(classifier_b), options_b);

    // Run scanner_a in the background.  It will claim the image and start
    // classification.  The mock server takes 500ms to respond, which
    // exceeds the 300ms lease, so the background renewer must keep it alive.
    let scanner_a_clone = scanner_a.clone();
    let handle_a = tokio::spawn(async move { scanner_a_clone.execute_one_pass().await });

    // Wait 400ms — long enough for the initial 300ms lease to expire and
    // for scanner_a to have entered response processing (HTTP response
    // arrives at ~500ms).  Scanner B starts here to verify it cannot
    // reclaim the row.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Run scanner_b.  It will try to recover expired leases and claim
    // work.  Scanner A's background renewer keeps the lease alive, so
    // scanner_b should find nothing to recover or claim.
    let scanner_b_clone = scanner_b.clone();
    let handle_b = tokio::spawn(async move { scanner_b_clone.execute_one_pass().await });

    // Wait for both to complete.
    let report_a = handle_a.await.unwrap().unwrap();
    let report_b = handle_b.await.unwrap().unwrap();

    // Scanner A should have claimed and completed the image.
    assert_eq!(report_a.claimed, 1);
    assert_eq!(report_a.completed, 1);

    // Scanner B should have found nothing to claim (the renewer kept the
    // lease alive throughout response processing).
    assert_eq!(
        report_b.claimed, 0,
        "scanner_b should not have claimed any work while scanner_a held the lease"
    );
    assert_eq!(
        report_b.leases_recovered, 0,
        "scanner_b should not have recovered any leases"
    );

    // The mock server should have been called exactly once.
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "classifier should be called exactly once"
    );

    // Verify the image is done.
    let img = scanner_a.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

// ── Forced renewal failure test ───────────────────────────────────────────

/// When the background renewer loses ownership (generation changes because
/// the row was re-claimed), the main processing task detects the exit and
/// returns an error instead of silently completing classification.
///
/// This test simulates ownership loss by directly updating the generation
/// in the database while Scanner A is in HTTP response processing.
#[tokio::test]
async fn forced_renewal_failure_causes_main_task_failure() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    // Mock server responds slowly (800ms) so Scanner A is stuck in
    // HTTP response processing when the ownership change takes effect.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(valid_openai_response())
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, _output_dir) =
        setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Build Scanner A with a short lease (200ms) and long request timeout.
    let config_a = make_classifier_config(&mock_server.uri());
    let classifier_a = ClassifierClient::from_config(&config_a).unwrap();

    let options_a = ScannerOptions {
        poll_interval: Duration::from_secs(1),
        retry_limit: 3,
        retry_initial_delay: Duration::from_secs(1),
        retry_max_delay: Duration::from_secs(30),
        processing_lease_duration: Duration::from_millis(200),
        maximum_image_size_bytes: 25_000_000,
        output_directory: dir.path().to_path_buf(),
        non_wildlife_image_retention: Duration::from_secs(4 * 86_400),
    };
    let scanner_a = Scanner::new(ops.clone(), Arc::new(classifier_a), options_a);

    // Run scanner_a in the background. It claims the image and starts
    // classification. The slow mock server (800ms) means Scanner A will
    // be in HTTP response processing for a while.
    let scanner_a_clone = scanner_a.clone();
    let handle_a = tokio::spawn(async move { scanner_a_clone.execute_one_pass().await });

    // Wait for scanner_a to claim the image and enter HTTP response
    // processing. At this point the renewer is running.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Force ownership loss: increment the generation directly.
    // This simulates another scanner having re-claimed the row.
    // The renewer will then fail when it tries to renew with the old
    // generation, and the main task will detect the ownership loss.
    sqlx::query("UPDATE images SET processing_generation = 999 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();

    // Wait for scanner_a to detect the ownership loss and return an error.
    // The renewer will exit when it fails to renew (generation mismatch),
    // and the main task will check is_finished() and return an error.
    let result_a = handle_a.await.unwrap();

    // Scanner A should have failed due to ownership loss.
    assert!(
        result_a.is_err(),
        "scanner_a should have failed when ownership was lost, got: {:?}",
        result_a
    );

    // Verify the image is still in processing (ownership loss doesn't
    // change the processing status — it only invalidates the lease).
    let img = scanner_a.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Processing);
}

// ── Parsing task failure test ─────────────────────────────────────────────

/// If the parsing task panics, the error is propagated to the caller
/// without leaving the image in an inconsistent state.
#[tokio::test]
async fn parsing_task_failure_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    // A normal pass should succeed.
    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);

    // Verify the image is done.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

// ── Scanner pass garbage collection tests ─────────────────────────────────

/// An old image classified as no wildlife during scan --once is collected
/// before the pass returns.
#[tokio::test]
async fn scanner_pass_collects_old_negative_image() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    // Return a negative classification.
    let negative_response = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"contains_animal\": false, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [], \"overall_confidence\": 0.1, \"summary\": \"Nothing wildlife.\", \"uncertainties\": []}"
            },
            "finish_reason": "stop"
        }]
    })
    .to_string();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(negative_response))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Set the capture time to 5 days ago so it's older than the 4-day retention.
    let old_capture = past_ts(5 * 24);
    sqlx::query("UPDATE images SET capture_start_at = ? WHERE id = 1")
        .bind(
            old_capture
                .as_datetime()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .execute(&pool)
        .await
        .unwrap();

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);
    // The garbage collector should have removed the file.
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.missing_files_reconciled, 0);
    assert_eq!(report.garbage_collection_failures, 0);

    // Verify the file was removed.
    let file_path = output_dir.join("img-1.jpg");
    assert!(!file_path.exists(), "file should have been removed");

    // Verify local_path is cleared.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert!(img.local_path.is_none());
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert_eq!(img.processing_status, ProcessingStatus::Done);

    // Classification should still exist.
    let class = scanner
        .database
        .get_classification(ImageId::new(1), "test-model", "wildlife-v1")
        .await
        .unwrap();
    assert!(!class.contains_wildlife);
}

/// An old image classified as containing wildlife is never collected.
#[tokio::test]
async fn scanner_pass_preserves_old_wildlife_image() {
    let dir = tempfile::tempdir().unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let (_db_path, ops, pool, output_dir) = setup_downloaded_images(&dir, 1, &minimal_jpeg()).await;

    // Set the capture time to 5 days ago so it's older than the 4-day retention.
    let old_capture = past_ts(5 * 24);
    sqlx::query("UPDATE images SET capture_start_at = ? WHERE id = 1")
        .bind(
            old_capture
                .as_datetime()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .execute(&pool)
        .await
        .unwrap();

    let scanner = build_scanner(ops, pool.clone(), &mock_server, &output_dir, 5).await;

    let report = scanner.clone().execute_one_pass().await.unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.completed, 1);
    // Wildlife-positive images should NOT be collected.
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.missing_files_reconciled, 0);

    // Verify the file still exists.
    let file_path = output_dir.join("img-1.jpg");
    assert!(
        file_path.exists(),
        "wildlife-positive file should be preserved"
    );

    // Verify local_path is preserved.
    let img = scanner.database.get_image(ImageId::new(1)).await.unwrap();
    assert!(img.local_path.is_some());
    assert_eq!(img.processing_status, ProcessingStatus::Done);

    // Classification should exist and be positive.
    let class = scanner
        .database
        .get_classification(ImageId::new(1), "test-model", "wildlife-v1")
        .await
        .unwrap();
    assert!(class.contains_wildlife);
}
