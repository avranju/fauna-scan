//! Integration tests for Phase 5: camera discovery.
//!
//! Uses wiremock for a reliable local mock server to simulate Digest
//! authentication and streaming-channel XML responses, plus temporary
//! SQLite databases to verify synchronization and metadata persistence.

use assert_cmd::Command;
use fauna_scan::configuration::{NvrConfig, NvrDownloadConfig, NvrSearchConfig};
use fauna_scan::database::{models::ServiceMetadataKey, sqlite::SqliteDataStore};
use fauna_scan::domain::Timestamp;
use fauna_scan::error::{AppResult, ErrorCategory};
use fauna_scan::nvr::{CameraDiscoveryClient, NvrTransport};
use predicates::prelude::*;

use chrono::{TimeZone, Utc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIGEST_CHALLENGE: &str = "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"";

// ── Helpers ────────────────────────────────────────────────────────────────

fn now_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap())
}

/// Build an `NvrConfig` pointing at the given mock server URL.
fn make_nvr_config(mock_base: &str) -> NvrConfig {
    use url::Url;
    let url = Url::parse(mock_base).unwrap();
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

/// Build a transport from config.
async fn build_transport(config: &NvrConfig) -> AppResult<NvrTransport> {
    NvrTransport::from_config(config)
}

/// Seed a database at the given path with an enabled camera and a metadata value.
async fn seed_discovered_camera_at(db_path: &std::path::Path) -> Timestamp {
    let db = SqliteDataStore::connect(db_path, 4).await.unwrap();
    let observed = now_ts();

    // Sync one camera so it exists as enabled.
    db.ops()
        .sync_cameras(
            &[fauna_scan::database::models::CameraDiscovery {
                channel_number: 1,
                primary_track_id: "101".to_string(),
                picture_track_id: "103".to_string(),
                name: Some("Existing Camera".to_string()),
                raw_discovery_identifier: Some("ch1".to_string()),
            }],
            &observed,
        )
        .await
        .unwrap();

    // Set an initial metadata value.
    let initial_meta = "2026-07-10T00:00:00.000000000Z";
    db.ops()
        .set_metadata(
            &ServiceMetadataKey::LastSuccessfulCameraDiscovery,
            initial_meta,
            &observed,
        )
        .await
        .unwrap();

    observed
}

// ── Successful authenticated discovery ─────────────────────────────────────

/// A successful channel-list response with two cameras.
fn success_channel_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
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

/// Mock server that returns a Digest challenge then success XML.
#[tokio::test]
async fn discover_authenticated_success() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request (authenticated): return channel list.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_string(success_channel_xml()))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = CameraDiscoveryClient::new(&transport);
    let cameras = client.discover().await.unwrap();

    assert_eq!(cameras.len(), 2);
    assert_eq!(cameras[0].channel_number, 1);
    assert_eq!(cameras[0].primary_track_id, "101");
    assert_eq!(cameras[0].picture_track_id, "103");
    assert_eq!(cameras[0].name, Some("Camera One".to_string()));
    assert_eq!(cameras[0].raw_discovery_identifier, Some("ch1".to_string()));
    assert_eq!(cameras[1].channel_number, 3);
    assert_eq!(cameras[1].primary_track_id, "301");
    assert_eq!(cameras[1].picture_track_id, "303");
}

/// Successful empty discovery returns an empty vector.
#[tokio::test]
async fn discover_empty_success() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<?xml version=\"1.0\"?><StreamingChannelList/>"),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = CameraDiscoveryClient::new(&transport);
    let cameras = client.discover().await.unwrap();

    assert!(cameras.is_empty());
}

// ── Malformed XML ──────────────────────────────────────────────────────────

/// Malformed XML (mismatched tags) that triggers XmlParsing.
fn malformed_xml() -> &'static [u8] {
    b"<?xml version=\"1.0\"?><tag><nested>text</tag>"
}

/// Malformed XML does not trigger persistence.
///
/// Exercises the operational discover flow through the CLI process:
/// seeds a configured database with an enabled camera and existing
/// metadata, runs `fauna-scan discover` against a Digest mock returning
/// malformed XML, asserts an `XmlParsing` failure, then reopens the
/// configured database and verifies the camera remains enabled and
/// metadata was not updated.
#[tokio::test]
async fn malformed_xml_does_not_trigger_persistence() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request (authenticated): return malformed XML.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(malformed_xml().to_vec()))
        .mount(&mock_server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let config_path = temp_dir.path().join("config.toml");

    // Parse the mock server URI to extract the port.
    let mock_url = url::Url::parse(&mock_server.uri()).unwrap();
    let mock_port = mock_url.port().unwrap_or(80);

    // Write a minimal config pointing at the mock server.
    let config_content = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"
log_level = "error"

[nvr]
scheme = "http"
host = "127.0.0.1"
port = {port}
username = "admin"
password = "correct-pass"
start_at = "2026-07-11T00:00:00Z"

[classifier]
"#,
        db = db_path.display(),
        output = temp_dir.path().join("output").display(),
        port = mock_port,
    );
    std::fs::write(&config_path, config_content).unwrap();

    // Seed a database with an enabled camera and metadata.
    let _observed = seed_discovered_camera_at(&db_path).await;

    // Verify initial state by reopening.
    let db = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let enabled_before: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '103'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(enabled_before, 1);

    let meta_before: Option<String> = db
        .ops()
        .get_metadata(&ServiceMetadataKey::LastSuccessfulCameraDiscovery)
        .await
        .unwrap();
    assert_eq!(
        meta_before,
        Some("2026-07-10T00:00:00.000000000Z".to_string())
    );

    // Drop the DB handle so the file is not locked.
    drop(db);

    // Run `fauna-scan discover` through the CLI process.
    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("discover")
        .assert()
        .failure()
        .stderr(predicate::str::contains("parse_camera_discovery_xml"));

    // Reopen the database and verify state is unchanged.
    let db2 = SqliteDataStore::connect(&db_path, 4).await.unwrap();

    let enabled_after: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '103'")
            .fetch_one(db2.pool())
            .await
            .unwrap();
    assert_eq!(
        enabled_after, 1,
        "camera should remain enabled after malformed XML"
    );

    let meta_after: Option<String> = db2
        .ops()
        .get_metadata(&ServiceMetadataKey::LastSuccessfulCameraDiscovery)
        .await
        .unwrap();
    assert_eq!(
        meta_after,
        Some("2026-07-10T00:00:00.000000000Z".to_string()),
        "metadata should not be updated after failed discovery"
    );
}

// ── Authentication failure ─────────────────────────────────────────────────

/// Digest challenge followed by 401 (bad credentials).
#[tokio::test]
async fn discover_auth_failure_no_leak() {
    let mock_server = MockServer::start().await;

    // Always return 401.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = CameraDiscoveryClient::new(&transport);

    let result = client.discover().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authentication);

    // Verify no password leak in error message.
    let display = format!("{err}");
    assert!(
        !display.contains("correct-pass"),
        "password leaked in error: {display}"
    );
}

// ── Synchronization and deactivation ───────────────────────────────────────

/// Discover cameras, sync them, then discover a reduced set and verify
/// absent cameras become inactive.
#[tokio::test]
async fn discover_sync_and_deactivation() {
    let mock_server = MockServer::start().await;

    // Each discover() makes 2 HTTP requests (401 challenge + authenticated 200).
    // We need 4 requests total for 2 discover calls.
    // Request 1: 401 challenge (first discover)
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Request 2: return 2-camera list (first discover authenticated)
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_string(success_channel_xml()))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Request 3: 401 challenge (second discover)
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Request 4: return 1-camera list (second discover authenticated)
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Camera One</name>
  </StreamingChannel>
</StreamingChannelList>"#,
        ))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = CameraDiscoveryClient::new(&transport);

    // Seed a database.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db = SqliteDataStore::connect(&db_path, 4).await.unwrap();

    // First sync: two cameras.
    let cameras1 = client.discover().await.unwrap();
    let observed = now_ts();
    let records1 = db.ops().sync_cameras(&cameras1, &observed).await.unwrap();
    assert_eq!(records1.len(), 2);

    // Verify both cameras are enabled.
    let enabled_103: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '103'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let enabled_303: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '303'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(enabled_103, 1);
    assert_eq!(enabled_303, 1);

    // Second sync: one camera (301 removed).
    let cameras2 = client.discover().await.unwrap();
    let observed2 = now_ts();
    let records2 = db.ops().sync_cameras(&cameras2, &observed2).await.unwrap();
    assert_eq!(records2.len(), 1);

    // Camera 1 should still be enabled.
    let enabled_103_after: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '103'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(enabled_103_after, 1);

    // Camera 3 should be inactive.
    let enabled_303_after: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '303'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(enabled_303_after, 0);
}

/// Successful discovery metadata is stored and readable.
#[tokio::test]
async fn discover_metadata_persistence() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_string(success_channel_xml()))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = CameraDiscoveryClient::new(&transport);

    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db = SqliteDataStore::connect(&db_path, 4).await.unwrap();

    // Discover and sync.
    let cameras = client.discover().await.unwrap();
    let observed = now_ts();
    let _records = db.ops().sync_cameras(&cameras, &observed).await.unwrap();

    // Store metadata.
    db.ops()
        .set_metadata(
            &ServiceMetadataKey::LastSuccessfulCameraDiscovery,
            &observed.to_string(),
            &observed,
        )
        .await
        .unwrap();

    // Read it back.
    let meta = db
        .ops()
        .get_metadata(&ServiceMetadataKey::LastSuccessfulCameraDiscovery)
        .await
        .unwrap();
    assert!(
        meta.is_some(),
        "metadata should be set after successful discovery"
    );
    assert!(meta.unwrap().contains("2026-07-11"));
}

// ── Deterministic multi-camera mapping ─────────────────────────────────────

/// A response with three cameras at different channels.
fn three_camera_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch5</id>
    <trackID>501</trackID>
    <name>Fifth Camera</name>
  </StreamingChannel>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>First Camera</name>
  </StreamingChannel>
  <StreamingChannel>
    <id>ch3</id>
    <trackID>301</trackID>
    <name>Third Camera</name>
  </StreamingChannel>
</StreamingChannelList>"#
}

/// Mock server returns three-camera XML on authenticated request.
#[tokio::test]
async fn discover_deterministic_ordering() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_string(three_camera_xml()))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let client = CameraDiscoveryClient::new(&transport);
    let cameras = client.discover().await.unwrap();

    // Should be sorted by channel number regardless of XML order.
    assert_eq!(cameras.len(), 3);
    assert_eq!(cameras[0].channel_number, 1);
    assert_eq!(cameras[0].picture_track_id, "103");
    assert_eq!(cameras[1].channel_number, 3);
    assert_eq!(cameras[1].picture_track_id, "303");
    assert_eq!(cameras[2].channel_number, 5);
    assert_eq!(cameras[2].picture_track_id, "503");
}
