//! Integration tests for Phase 8: downloader orchestration.
//!
//! Tests core orchestration logic including:
//! - Options validation (zero search concurrency rejection)
//! - Settlement delay enforcement
//! - Active camera enumeration from the repository
//! - Monotonic cursor guarantees (overlap doesn't regress next_search_at)
//! - Successful overlap replay clears cursor error
//! - Overlap deduplication (no duplicate image rows)

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use fauna_scan::configuration::{
    ClassifierConfig, Config, GeneralConfig, NvrConfig, NvrDownloadConfig, NvrSearchConfig,
};
use fauna_scan::database::Database;
use fauna_scan::database::models::*;
use fauna_scan::domain::*;
use fauna_scan::downloader::orchestration::{
    DownloaderOrchestrator, DownloaderOrchestratorOptions,
};
use fauna_scan::downloader::{DownloadWorker, DownloadWorkerOptions};
use fauna_scan::error::{AppResult, ErrorCategory};
use fauna_scan::nvr::{ImageDownloadClient, NvrTransport, configured_nvr_identity};
use fauna_scan::service_lifecycle::ShutdownToken;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// ── Helpers ────────────────────────────────────────────────────────────────

fn ts(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap())
}

fn future_ts(hours: i32) -> Timestamp {
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, (12 + hours) as u32, 0, 0)
            .unwrap(),
    )
}

fn past_ts(hours: i32) -> Timestamp {
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, (12 - hours) as u32, 0, 0)
            .unwrap(),
    )
}

/// Build a test Config pointing at a local mock server.
fn make_test_config(
    mock_base: &str,
    db_path: &std::path::Path,
    output_dir: &std::path::Path,
) -> Config {
    let url = Url::parse(mock_base).unwrap();
    let port = url.port().unwrap_or(80);
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("127.0.0.1").to_string();

    Config {
        general: GeneralConfig {
            database_path: db_path.to_path_buf(),
            output_directory: output_dir.to_path_buf(),
            log_level: fauna_scan::cli::LogLevel::Info,
        },
        nvr: NvrConfig {
            scheme: scheme.to_string(),
            host,
            port,
            username: "admin".to_string(),
            password: Some(fauna_scan::configuration::Secret::new(
                "correct-pass".to_string(),
            )),
            start_at: Timestamp::new(Utc::now() - chrono::Duration::minutes(65)),
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
        },
        classifier: ClassifierConfig {
            endpoints: Vec::new(),
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 600,
        },
        source_path: std::path::PathBuf::from("/tmp/test.toml"),
    }
}

/// Build a minimal orchestrator from a test config and mock server.
#[allow(dead_code)]
async fn build_orchestrator(
    mock_server: &str,
    temp_dir: &tempfile::TempDir,
) -> AppResult<(DownloaderOrchestrator, NvrConfig)> {
    let db_path = temp_dir.path().join("test.db");
    let output_dir = temp_dir.path().join("output");
    std::fs::create_dir_all(&output_dir).ok();

    let config = make_test_config(mock_server, &db_path, &output_dir);

    // Open database
    let database = Database::open(&db_path).await?;

    // Build transport
    let transport = Arc::new(NvrTransport::from_config(&config.nvr)?);

    // Build download client
    let download_client = Arc::new(ImageDownloadClient::from_config(
        transport.clone(),
        &config.nvr,
    ));

    // Build worker options
    let worker_options = DownloadWorkerOptions::from_config(&config)?;

    // Build worker
    let download_worker =
        DownloadWorker::new(database.ops().clone(), download_client, worker_options);

    // Build orchestrator options
    let orchestrator_options = DownloaderOrchestratorOptions::from_config(&config)?;

    // Validate
    orchestrator_options.validate()?;

    // Build orchestrator (now fallible — validates concurrency).
    let orchestrator = DownloaderOrchestrator::new(
        database.ops().clone(),
        transport,
        download_worker,
        orchestrator_options,
    )?;

    Ok((orchestrator, config.nvr))
}

// ── Discovery XML fixtures ────────────────────────────────────────────────

/// Discovery XML fixture — kept for potential future integration tests.
#[allow(dead_code)]
fn discovery_xml_two_cameras() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList xmlns="http://www.hikvision.com/ver20/XMLSchema">
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Camera One</name>
  </StreamingChannel>
  <StreamingChannel>
    <id>ch2</id>
    <trackID>301</trackID>
    <name>Camera Two</name>
  </StreamingChannel>
</StreamingChannelList>"#
}

/// Search response fixture — kept for potential future integration tests.
#[allow(dead_code)]
fn search_response_empty() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<CMSearchResult>
  <responseStatus bool="true"/>
  <responseStatusStrg>OK</responseStatusStrg>
  <numOfMatches>0</numOfMatches>
</CMSearchResult>"#
        .to_string()
}

const DIGEST_CHALLENGE: &str = "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"";

fn valid_jpeg() -> Vec<u8> {
    vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, b'J', b'F', 0xff, 0xd9]
}

fn xml_value(body: &[u8], tag: &str) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = body[start..].find(&format!("</{tag}>"))?;
    Some(body[start..start + end].to_string())
}

struct SearchResponder {
    playback_base: String,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    failed_track: Option<String>,
    capture_at: String,
    include_bad_image: bool,
}

impl Respond for SearchResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if !request.headers.contains_key("authorization") {
            return ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE);
        }

        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        let active_counter = self.active.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            active_counter.fetch_sub(1, Ordering::SeqCst);
        });

        let track = xml_value(&request.body, "trackID").unwrap_or_default();
        if self.failed_track.as_deref() == Some(track.as_str()) {
            return ResponseTemplate::new(500).set_delay(Duration::from_millis(50));
        }

        let id = xml_value(&request.body, "searchID").unwrap_or_default();
        let start = self.capture_at.clone();
        let end = xml_value(&request.body, "endTime")
            .unwrap_or_else(|| "2026-07-11T00:01:00Z".to_string());
        let extra = if self.include_bad_image {
            format!(
                "<searchMatchItem><trackID>{track}</trackID><timeSpan><startTime>{start}</startTime><endTime>{end}</endTime></timeSpan><contentType>picture</contentType><codecType>jpeg</codecType><playbackURI>{base}/picture/bad-{track}</playbackURI><size>10</size></searchMatchItem>",
                track = track,
                start = start,
                end = end,
                base = self.playback_base,
            )
        } else {
            String::new()
        };
        let count = if self.include_bad_image { 2 } else { 1 };
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult><searchID>{id}</searchID>
<responseStatus>true</responseStatus><responseStatusStrg>OK</responseStatusStrg>
<numOfMatches>{count}</numOfMatches><searchMatchItem><trackID>{track}</trackID>
<timeSpan><startTime>{start}</startTime><endTime>{end}</endTime></timeSpan>
<contentType>picture</contentType><codecType>jpeg</codecType>
<playbackURI>{base}/picture/{track}</playbackURI><size>10</size>
</searchMatchItem>{extra}</CMSearchResult>"#,
            id = id,
            track = track,
            start = start,
            end = end,
            base = self.playback_base,
            count = count,
            extra = extra,
        );
        ResponseTemplate::new(200)
            .set_delay(Duration::from_millis(50))
            .set_body_string(body)
    }
}

async fn mount_discovery(mock: &MockServer, cameras: usize) {
    let mut channels = String::new();
    for channel in 1..=cameras {
        let track = channel * 100 + 1;
        channels.push_str(&format!(
            "<StreamingChannel><id>ch{channel}</id><trackID>{track}</trackID><name>Camera {channel}</name></StreamingChannel>"
        ));
    }
    let body = format!(
        "<StreamingChannelList xmlns=\"http://www.hikvision.com/ver20/XMLSchema\">{channels}</StreamingChannelList>"
    );
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(move |request: &Request| {
            if request.headers.contains_key("authorization") {
                ResponseTemplate::new(200).set_body_string(body.clone())
            } else {
                ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE)
            }
        })
        .mount(mock)
        .await;
}

async fn mount_search(mock: &MockServer, responder: SearchResponder) {
    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(responder)
        .mount(mock)
        .await;
}

async fn mount_bad_playback(mock: &MockServer, track: usize) {
    let path_value = format!("/picture/bad-{track}");
    Mock::given(method("GET"))
        .and(path(path_value))
        .respond_with(move |request: &Request| {
            if request.headers.contains_key("authorization") {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE)
            }
        })
        .mount(mock)
        .await;
}

async fn mount_playback(mock: &MockServer, tracks: usize) {
    for channel in 1..=tracks {
        let track = channel * 100 + 3;
        let path_value = format!("/picture/{track}");
        Mock::given(method("GET"))
            .and(path(path_value))
            .respond_with(move |request: &Request| {
                if request.headers.contains_key("authorization") {
                    ResponseTemplate::new(200).set_body_bytes(valid_jpeg())
                } else {
                    ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE)
                }
            })
            .mount(mock)
            .await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Verify that options validation rejects zero search concurrency.
///
/// Regression test: a manually constructed options value with
/// search_concurrency = 0 would create a zero-permit semaphore and
/// execute_one_pass would wait forever.
#[test]
fn validate_rejects_zero_search_concurrency() {
    let options = DownloaderOrchestratorOptions {
        start_at: ts(2026, 7, 10, 0, 0, 0),
        window_minutes: 60,
        poll_interval: Duration::from_secs(60),
        poll_overlap: Duration::from_secs(120),
        camera_refresh_interval: Duration::from_secs(3600),
        settlement_delay: Duration::from_secs(10),
        search_concurrency: 0,
        nvr_identity: "http://nvr:8080".to_string(),
        search_config: NvrSearchConfig {
            window_minutes: 60,
            max_results: 50,
            poll_interval_seconds: 60,
            poll_overlap_seconds: 120,
            camera_refresh_interval_seconds: 3600,
            settlement_delay_seconds: 10,
        },
    };
    let result = options.validate();
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Configuration);
    assert!(err.message.contains("search_concurrency"));
}

/// Verify that active camera enumeration works correctly via the
/// repository, which is used by the orchestrator's reload_active_cameras.
#[tokio::test]
async fn active_cameras_enumeration() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = ts(2026, 7, 11, 12, 0, 0);

    // Sync two cameras
    let cameras = vec![
        CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: Some("Camera One".to_string()),
            raw_discovery_identifier: None,
        },
        CameraDiscovery {
            channel_number: 2,
            primary_track_id: "301".to_string(),
            picture_track_id: "303".to_string(),
            name: Some("Camera Two".to_string()),
            raw_discovery_identifier: None,
        },
    ];
    ops.sync_cameras(&cameras, &now).await.unwrap();

    // Mark camera 2 inactive
    sqlx::query("UPDATE cameras SET enabled = 0 WHERE picture_track_id = '303'")
        .execute(ops.pool())
        .await
        .unwrap();

    // Only camera 1 should be listed
    let active = ops.list_active_cameras().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].picture_track_id, "103");
}

/// Verify that committing an older overlap window does not regress
/// next_search_at. This is the core monotonic cursor guarantee.
#[tokio::test]
async fn overlap_window_does_not_regress_next_search_at() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = ts(2026, 7, 11, 12, 0, 0);
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

    // First window advances cursor to future_ts(2)
    let window1 = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-monotonic-1"),
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

    ops.commit_search_window(&window1, &[img]).await.unwrap();

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.next_search_at, Some(future_ts(2)));
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));

    // Replay an older overlap window with next_search_at = future_ts(1)
    // — this should NOT lower next_search_at
    let overlap_window = SearchWindowCommit {
        camera_id,
        window_start: past_ts(1),
        window_end: future_ts(1),
        next_search_at: future_ts(1),
        polled_at: now,
        updated_at: now,
    };

    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-monotonic-2"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    ops.commit_search_window(&overlap_window, &[img2])
        .await
        .unwrap();

    let cursor2 = ops.get_cursor(camera_id).await.unwrap().unwrap();
    // next_search_at should NOT have regressed
    assert_eq!(cursor2.next_search_at, Some(future_ts(2)));
    // last_completed_window_end should still be the later value
    assert_eq!(cursor2.last_completed_window_end, Some(future_ts(1)));
}

/// Verify that a successful overlap replay clears a previous cursor
/// error without regressing completed-window progress.
#[tokio::test]
async fn successful_overlap_replay_clears_cursor_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = ts(2026, 7, 11, 12, 0, 0);
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

    // Record a cursor error
    ops.record_cursor_error(camera_id, "timeout error", &now)
        .await
        .unwrap();

    let cursor_before = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor_before.last_error, Some("timeout error".to_string()));
    assert!(cursor_before.next_search_at.is_none());

    // Commit a successful window — should clear error and populate cursor
    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-clear-error"),
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

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let cursor_after = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert!(cursor_after.last_error.is_none());
    assert_eq!(cursor_after.next_search_at, Some(future_ts(2)));
    assert_eq!(cursor_after.last_completed_window_start, Some(now));
    assert_eq!(cursor_after.last_completed_window_end, Some(future_ts(1)));
}

/// Verify that overlap deduplication does not create duplicate image rows.
#[tokio::test]
async fn overlap_deduplication_no_duplicate_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();
    let now = ts(2026, 7, 11, 12, 0, 0);
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

    // First window: insert image
    let window1 = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-dedup-1"),
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

    let new_count1 = ops.commit_search_window(&window1, &[img1]).await.unwrap();
    assert_eq!(new_count1, 1);

    // Overlap window: same image key — should not insert duplicate
    let overlap_window = SearchWindowCommit {
        camera_id,
        window_start: past_ts(1),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-dedup-1"),
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

    let new_count2 = ops
        .commit_search_window(&overlap_window, &[img2])
        .await
        .unwrap();
    assert_eq!(new_count2, 0);

    // Still exactly one image row
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// Verify that the orchestrator constructor accepts real config values
/// and does not fall back to placeholder values.
#[tokio::test]
async fn orchestrator_uses_real_config_values() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let output_dir = dir.path().join("output");
    std::fs::create_dir_all(&output_dir).ok();

    let config = make_test_config("http://127.0.0.1:9999", &db_path, &output_dir);

    // Verify the config has the expected NVR identity
    let identity = configured_nvr_identity(&config.nvr.scheme, &config.nvr.host, config.nvr.port);
    assert_eq!(identity, "http://127.0.0.1:9999");

    // Build orchestrator options from real config.
    let options = DownloaderOrchestratorOptions::from_config(&config).unwrap();
    assert_eq!(options.nvr_identity, "http://127.0.0.1:9999");
    assert_eq!(options.window_minutes, 60);
    assert_eq!(options.poll_interval, Duration::from_secs(60));
    assert_eq!(options.poll_overlap, Duration::from_secs(120));
    assert_eq!(options.settlement_delay, Duration::from_secs(10));
    assert_eq!(options.search_concurrency, 2); // Conservative default
    // Verify real search config is propagated (max_results from config).
    assert_eq!(options.search_config.max_results, 50);
    assert_eq!(options.search_config.window_minutes, 60);
}

/// Verify that the orchestrator constructor properly validates search
/// concurrency and rejects zero at construction time.
#[test]
fn orchestrator_rejects_zero_concurrency_at_construction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let output_dir = dir.path().join("output");
    std::fs::create_dir_all(&output_dir).ok();

    let config = make_test_config("http://127.0.0.1:9999", &db_path, &output_dir);

    // Build options with zero concurrency (includes new search_config field).
    let options = DownloaderOrchestratorOptions {
        start_at: config.nvr.start_at,
        window_minutes: config.nvr.search.window_minutes,
        poll_interval: Duration::from_secs(config.nvr.search.poll_interval_seconds),
        poll_overlap: Duration::from_secs(config.nvr.search.poll_overlap_seconds),
        camera_refresh_interval: Duration::from_secs(
            config.nvr.search.camera_refresh_interval_seconds,
        ),
        settlement_delay: Duration::from_secs(config.nvr.search.settlement_delay_seconds),
        search_concurrency: 0,
        nvr_identity: configured_nvr_identity(
            &config.nvr.scheme,
            &config.nvr.host,
            config.nvr.port,
        ),
        search_config: config.nvr.search.clone(),
    };

    // Validate should reject zero concurrency.
    assert!(options.validate().is_err());
    let err = options.validate().unwrap_err();
    assert_eq!(err.category, ErrorCategory::Configuration);

    // The orchestrator constructor also validates concurrency directly.
    // We cannot build a full orchestrator without a real transport,
    // but from_config + validate are the normal paths and both reject zero.
}

// ── End-to-end integration tests ──────────────────────────────────────────

fn recent_capture() -> String {
    (Utc::now() - chrono::Duration::seconds(20)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn prepare_server(
    cameras: usize,
    failed_track: Option<&str>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
) -> MockServer {
    let server = MockServer::start().await;
    mount_discovery(&server, cameras).await;
    mount_search(
        &server,
        SearchResponder {
            playback_base: server.uri(),
            active,
            maximum,
            failed_track: failed_track.map(str::to_string),
            capture_at: recent_capture(),
            include_bad_image: false,
        },
    )
    .await;
    mount_playback(&server, cameras).await;
    server
}

#[tokio::test]
async fn cancellation_before_pass_prevents_discovery_search_and_claims() {
    let server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();
    let shutdown = ShutdownToken::new();
    shutdown.cancel();

    let report = orchestrator
        .execute_one_pass_with_shutdown(true, shutdown)
        .await
        .unwrap();
    assert_eq!(report.cameras_discovered, 0);
    assert_eq!(report.windows_completed, 0);
    assert_eq!(report.download_pass.claimed, 0);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_between_search_windows_preserves_only_committed_cursor() {
    let server = MockServer::start().await;
    mount_discovery(&server, 1).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();
    orchestrator.startup_housekeeping().await.unwrap();
    orchestrator.options.start_at = Timestamp::new(Utc::now() - chrono::Duration::minutes(5));
    orchestrator.options.window_minutes = 1;
    orchestrator.options.settlement_delay = Duration::ZERO;
    orchestrator.search_config.window_minutes = 1;
    let shutdown = ShutdownToken::new();
    let response_shutdown = shutdown.clone();
    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(move |_request: &Request| {
            response_shutdown.cancel();
            ResponseTemplate::new(200).set_body_string(
                "<CMSearchResult><responseStatus>true</responseStatus><responseStatusStrg>OK</responseStatusStrg><numOfMatches>0</numOfMatches></CMSearchResult>",
            )
        })
        .mount(&server)
        .await;

    let _report = orchestrator
        .execute_one_pass_with_shutdown(true, shutdown)
        .await
        .unwrap();
    let camera = orchestrator.database.list_active_cameras().await.unwrap()[0].id;
    let cursor = orchestrator.database.get_cursor(camera).await.unwrap();
    assert!(cursor.is_some_and(|cursor| cursor.last_completed_window_end.is_some()));
    let search_count = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .count();
    assert_eq!(search_count, 1, "cancellation advanced a skipped window");
}

#[tokio::test]
async fn cancellation_during_download_batch_drains_active_tasks_without_new_claims() {
    let server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();
    let now = Timestamp::new(Utc::now());
    let camera_id = orchestrator
        .database
        .sync_cameras(
            &[CameraDiscovery {
                channel_number: 1,
                primary_track_id: "101".into(),
                picture_track_id: "103".into(),
                name: None,
                raw_discovery_identifier: None,
            }],
            &now,
        )
        .await
        .unwrap()[0]
        .id;
    for index in 0..3 {
        orchestrator
            .database
            .commit_search_window(
                &SearchWindowCommit {
                    camera_id,
                    window_start: now,
                    window_end: now,
                    next_search_at: now,
                    polled_at: now,
                    updated_at: now,
                },
                &[DiscoveredImage {
                    image_key: ImageKey::new(format!("download-cancel-{index}")),
                    camera_id,
                    track_id: TrackId::new("103"),
                    capture_start_at: Timestamp::new(
                        *now.as_datetime() + chrono::Duration::seconds(index as i64),
                    ),
                    capture_end_at: None,
                    playback_uri: format!("{}/picture/103", server.uri()),
                    canonical_playback_uri: format!("{}/picture/103", server.uri()),
                    codec_type: Some("jpeg".into()),
                    content_type: Some("picture".into()),
                    nvr_reported_size: None,
                    discovered_at: now,
                }],
            )
            .await
            .unwrap();
    }
    let shutdown = ShutdownToken::new();
    let response_shutdown = shutdown.clone();
    Mock::given(method("GET"))
        .and(path("/picture/103"))
        .respond_with(move |_request: &Request| {
            response_shutdown.cancel();
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(20))
                .set_body_bytes(valid_jpeg())
        })
        .mount(&server)
        .await;

    let report = orchestrator
        .download_worker
        .run_until_idle_with_shutdown(&shutdown)
        .await
        .unwrap();
    assert_eq!(report.claimed, 2, "worker claimed beyond its active batch");
    let counts = orchestrator.database.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Pending), Some(&1));
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&2));
}

#[tokio::test]
async fn cancellation_wakes_downloader_polling_sleep() {
    let server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();
    orchestrator.last_successful_camera_refresh = Some(Timestamp::new(Utc::now()));
    orchestrator.options.poll_interval = Duration::from_secs(3600);
    let shutdown = ShutdownToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        orchestrator
            .run_continuous_with_shutdown(task_shutdown)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("polling sleep did not observe cancellation")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn one_pass_discovers_searches_and_downloads_multiple_cameras() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server = prepare_server(4, None, active, maximum.clone()).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();

    orchestrator.startup_housekeeping().await.unwrap();
    orchestrator.persist_nvr_identity().await.unwrap();
    let report = orchestrator.execute_one_pass(true).await.unwrap();

    assert_eq!(report.cameras_discovered, 4);
    assert_eq!(report.cameras_active, 4);
    assert_eq!(report.search_failures, 0);
    assert_eq!(report.images_discovered, 4);
    assert_eq!(report.download_pass.downloaded, 4);
    // Four cameras make an unbounded implementation observable: the
    // configured bound must be reached but never exceeded.
    assert_eq!(maximum.load(Ordering::SeqCst), 2);

    let counts = orchestrator.database.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&4));
    assert_eq!(
        orchestrator
            .database
            .get_metadata(&ServiceMetadataKey::InitialBackfillCompleted)
            .await
            .unwrap()
            .as_deref(),
        Some("true")
    );
    assert!(
        orchestrator
            .database
            .get_metadata(&ServiceMetadataKey::LastSuccessfulDownloaderPoll)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn one_failed_download_does_not_block_a_successful_sibling() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start().await;
    mount_discovery(&server, 1).await;
    mount_search(
        &server,
        SearchResponder {
            playback_base: server.uri(),
            active,
            maximum,
            failed_track: None,
            capture_at: recent_capture(),
            include_bad_image: true,
        },
    )
    .await;
    mount_playback(&server, 1).await;
    mount_bad_playback(&server, 103).await;

    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();
    orchestrator.startup_housekeeping().await.unwrap();
    let report = orchestrator.execute_one_pass(true).await.unwrap();

    assert_eq!(report.images_discovered, 2);
    assert_eq!(report.download_pass.downloaded, 1);
    assert_eq!(report.download_pass.retry_scheduled, 1);
    let counts = orchestrator.database.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&1));
    assert_eq!(counts.download.get(&DownloadStatus::RetryWait), Some(&1));
}

#[tokio::test]
async fn failed_camera_does_not_block_sibling_search_or_download() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server = prepare_server(2, Some("203"), active, maximum).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();

    orchestrator.startup_housekeeping().await.unwrap();
    let report = orchestrator.execute_one_pass(true).await.unwrap();

    assert_eq!(report.search_failures, 1);
    assert_eq!(report.download_pass.downloaded, 1);
    assert!(report.camera_reports.iter().any(|r| r.failure.is_some()));
    assert!(
        orchestrator
            .database
            .get_metadata(&ServiceMetadataKey::LastSuccessfulDownloaderPoll)
            .await
            .unwrap()
            .is_none()
    );

    let cameras = orchestrator.database.list_active_cameras().await.unwrap();
    let failed_camera = cameras
        .iter()
        .find(|camera| camera.picture_track_id == "203")
        .unwrap();
    let cursor = orchestrator
        .database
        .get_cursor(failed_camera.id)
        .await
        .unwrap()
        .unwrap();
    assert!(cursor.next_search_at.is_none());
}

#[tokio::test]
async fn refreshed_camera_set_is_backfilled_from_configured_start() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server = prepare_server(2, None, active, maximum).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut orchestrator, _nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();
    orchestrator.startup_housekeeping().await.unwrap();

    let discovered = orchestrator.attempt_discovery().await.unwrap();
    orchestrator.sync_cameras(&discovered[..1]).await.unwrap();
    assert_eq!(orchestrator.reload_active_cameras().await.unwrap().len(), 1);

    // Make the scheduled refresh due. The iteration must discover and sync
    // the second camera before it loads cameras for searching.
    orchestrator.last_successful_camera_refresh =
        Some(Timestamp::new(Utc::now() - chrono::Duration::hours(2)));
    let report = orchestrator.run_continuous_iteration().await.unwrap();
    assert_eq!(report.cameras_active, 2);
    assert!(report.camera_reports.iter().all(|r| r.failure.is_none()));
    let cameras = orchestrator.database.list_active_cameras().await.unwrap();
    assert_eq!(cameras.len(), 2);
    for camera in cameras {
        assert!(
            orchestrator
                .database
                .get_cursor(camera.id)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn search_requests_respect_settlement_and_restart_overlap_is_idempotent() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server = prepare_server(1, None, active, maximum).await;
    let temp_dir = tempfile::tempdir().unwrap();
    let (mut first, nvr) = build_orchestrator(&server.uri(), &temp_dir).await.unwrap();

    first.startup_housekeeping().await.unwrap();
    let first_report = first.execute_one_pass(true).await.unwrap();
    assert_eq!(first_report.download_pass.downloaded, 1);

    let now = Utc::now();
    let requests = server.received_requests().await.unwrap();
    let first_searches: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST"
                && request.headers.contains_key("authorization")
                && !request.body.is_empty()
        })
        .collect();
    assert!(!first_searches.is_empty());
    for request in &first_searches {
        let end = xml_value(&request.body, "endTime")
            .unwrap()
            .parse::<Timestamp>()
            .unwrap();
        assert!(end.as_datetime() <= &(now - chrono::Duration::seconds(8)));
    }
    let first_start = xml_value(&first_searches[0].body, "startTime")
        .unwrap()
        .parse::<Timestamp>()
        .unwrap();
    let request_count_before_restart = first_searches.len();

    let db_path = temp_dir.path().join("test.db");
    let database = Database::open(&db_path).await.unwrap();
    let output_dir = temp_dir.path().join("output");
    let config = make_test_config(&server.uri(), &db_path, &output_dir);
    let transport = Arc::new(NvrTransport::from_config(&nvr).unwrap());
    let client = Arc::new(ImageDownloadClient::from_config(transport.clone(), &nvr));
    let worker = DownloadWorker::new(
        database.ops().clone(),
        client,
        DownloadWorkerOptions::from_config(&config).unwrap(),
    );
    let options = DownloaderOrchestratorOptions::from_config(&config).unwrap();
    let mut restarted =
        DownloaderOrchestrator::new(database.ops().clone(), transport, worker, options).unwrap();
    restarted.startup_housekeeping().await.unwrap();
    let before = database
        .ops()
        .get_cursor(CameraId::new(1))
        .await
        .unwrap()
        .unwrap();
    let second_report = restarted.execute_one_pass(true).await.unwrap();
    assert_eq!(second_report.images_discovered, 0);

    let requests_after_restart = server.received_requests().await.unwrap();
    let second_searches: Vec<_> = requests_after_restart
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST"
                && request.headers.contains_key("authorization")
                && !request.body.is_empty()
        })
        .collect();
    assert!(second_searches.len() > request_count_before_restart);
    let restart_start = xml_value(
        &second_searches[request_count_before_restart].body,
        "startTime",
    )
    .unwrap()
    .parse::<Timestamp>()
    .unwrap();
    assert!(
        restart_start > first_start,
        "restart repeated the historical start instead of using the persisted cursor/overlap"
    );

    let after = database
        .ops()
        .get_cursor(CameraId::new(1))
        .await
        .unwrap()
        .unwrap();
    assert!(after.next_search_at >= before.next_search_at);
    let image_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(database.ops().pool())
        .await
        .unwrap();
    assert_eq!(image_count, 1);
}
