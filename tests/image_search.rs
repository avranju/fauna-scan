//! Integration tests for Phase 6 image search.
//!
//! Uses wiremock for a reliable local mock server to simulate Digest
//! authentication and Hikvision search responses.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use fauna_scan::configuration::{NvrConfig, NvrDownloadConfig, NvrSearchConfig};
use fauna_scan::database::Database;
use fauna_scan::database::models::*;
use fauna_scan::database::repository::DatabaseOps;
use fauna_scan::domain::*;
use fauna_scan::error::{AppResult, ErrorCategory};
use fauna_scan::nvr::NvrTransport;
use fauna_scan::nvr::image_search::*;
use uuid::Uuid;
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

/// Build an `NvrConfig` pointing at the given mock server URL.
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
        start_at: Timestamp::new(chrono::Utc::now()),
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

/// Build an `NvrTransport` from an `NvrConfig`.
async fn build_transport(config: &NvrConfig) -> AppResult<NvrTransport> {
    NvrTransport::from_config(config)
}

/// Build an `ImageSearchClient` from a transport and database.
fn build_search_client(
    transport: NvrTransport,
    ops: DatabaseOps,
    camera_id: CameraId,
) -> ImageSearchClient<'static> {
    let transport_ref: &'static NvrTransport = Box::leak(Box::new(transport));

    ImageSearchClient {
        transport: transport_ref,
        database: ops,
        search_config: NvrSearchConfig {
            window_minutes: 60,
            max_results: 50,
            poll_interval_seconds: 60,
            poll_overlap_seconds: 120,
            camera_refresh_interval_seconds: 3600,
            settlement_delay_seconds: 10,
        },
        nvr_identity: configured_nvr_identity("http", "127.0.0.1", 0),
        camera_id,
        picture_track: "103".to_string(),
    }
}

/// Extract the searchID from a CMSearchDescription XML request body.
fn extract_request_search_id(body: &[u8]) -> Option<Uuid> {
    let xml = std::str::from_utf8(body).ok()?;
    let start = xml.find("<searchID>")?;
    let inner_start = start + 10;
    let end = xml[inner_start..].find("</searchID>")?;
    let id_str = &xml[inner_start..inner_start + end];
    Uuid::parse_str(id_str).ok()
}

/// Response builders that echo back the searchID from the request.
fn echo_success(playback_uri: &str) -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>OK</responseStatusStrg>
  <numOfMatches>1</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>{uri}</playbackURI>
    <size>12345</size>
  </searchMatchItem>
</CMSearchResult>"#,
            id = search_id,
            uri = playback_uri,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_empty() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>OK</responseStatusStrg>
  <numOfMatches>0</numOfMatches>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_failure() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Failure</responseStatusStrg>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_more_zero() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>MORE</responseStatusStrg>
  <numOfMatches>0</numOfMatches>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_mixed_items() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>2</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/1.jpg</playbackURI>
  </searchMatchItem>
  <searchMatchItem>
    <timeSpan>
      <startTime>2026-07-11T03:00:00Z</startTime>
      <endTime>2026-07-11T03:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/2.jpg</playbackURI>
  </searchMatchItem>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_mixed_media() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>2</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>video</contentType>
    <codecType>h264</codecType>
    <playbackURI>http://nvr/video/1.mp4</playbackURI>
  </searchMatchItem>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:01:00Z</startTime>
      <endTime>2026-07-11T02:01:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/1.jpg</playbackURI>
  </searchMatchItem>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_duplicate_timestamps() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>2</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/a.jpg?param=1</playbackURI>
  </searchMatchItem>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/b.jpg?param=2</playbackURI>
  </searchMatchItem>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

fn echo_repeated_page() -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let search_id = extract_request_search_id(&request.body)
            .unwrap_or_else(|| Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap());
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>MORE</responseStatusStrg>
  <numOfMatches>2</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/1.jpg</playbackURI>
    <size>1000</size>
  </searchMatchItem>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:01:00Z</startTime>
      <endTime>2026-07-11T02:01:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/2.jpg</playbackURI>
    <size>1000</size>
  </searchMatchItem>
</CMSearchResult>"#,
            id = search_id,
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

/// Capture request bodies for inspection.
#[derive(Clone)]
struct RequestCapture {
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RequestCapture {
    fn new() -> Self {
        Self {
            bodies: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    fn matcher(&self) -> RequestCaptureMatcher {
        RequestCaptureMatcher {
            bodies: self.bodies.clone(),
        }
    }
}

struct RequestCaptureMatcher {
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
}

impl wiremock::Match for RequestCaptureMatcher {
    fn matches(&self, request: &wiremock::Request) -> bool {
        if request.url.path() != "/ISAPI/ContentMgmt/search" {
            return false;
        }
        if let Ok(body_str) = String::from_utf8(request.body.to_vec()) {
            self.bodies.lock().unwrap().push(body_str);
        }
        true
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn single_page_search_commits_image_and_advances_cursor() {
    let mock_server = MockServer::start().await;
    let capture = RequestCapture::new();

    Mock::given(capture.matcher())
        .respond_with(echo_success("http://nvr/pic/1.jpg"))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(1),
    };

    let outcome = client.search_one_window(&window).await.unwrap();

    assert_eq!(outcome.pages_fetched, 1);
    assert_eq!(outcome.records_found, 1);
    assert_eq!(outcome.records_skipped, 0);
    assert_eq!(outcome.records_inserted, 1);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(cursor.next_search_at, Some(future_ts(1)));
    assert!(cursor.last_error.is_none());

    let bodies = capture.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("<searchResultPostion>0</searchResultPostion>"));
    assert!(bodies[0].contains("<searchID>"));
}

#[tokio::test]
async fn paginated_search_fetches_all_pages_and_commits() {
    let mock_server = MockServer::start().await;
    // Store each wire request once in the responder. The first request for
    // each logical page must be unauthenticated and receive a challenge;
    // only its authenticated replay is allowed to return the page.
    let requests: Arc<std::sync::Mutex<Vec<(String, bool)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with({
            let requests = requests.clone();
            move |request: &wiremock::Request| {
                let body = String::from_utf8(request.body.to_vec()).unwrap_or_default();
                let authenticated = request.headers.get("Authorization").is_some();
                requests
                    .lock()
                    .unwrap()
                    .push((body.clone(), authenticated));

                if !authenticated {
                    return ResponseTemplate::new(401)
                        .insert_header("WWW-Authenticate", DIGEST_CHALLENGE);
                }

                let search_id = extract_request_search_id(&request.body).unwrap_or_else(|| {
                    Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap()
                });
                // Use searchResultPostion in the authenticated request to
                // determine the response for this page.
                let is_first = body.contains("<searchResultPostion>0</searchResultPostion>");
                let (status, items) = if is_first {
                    let mut s = String::new();
                    for i in 0..2 {
                        s.push_str(&format!("<searchMatchItem><trackID>103</trackID><timeSpan><startTime>2026-07-11T02:{:02}:00Z</startTime><endTime>2026-07-11T02:{:02}:01Z</endTime></timeSpan><contentType>picture</contentType><codecType>jpeg</codecType><playbackURI>http://nvr/pic/{}.jpg</playbackURI><size>1000</size></searchMatchItem>", i, i, i));
                    }
                    ("MORE", s)
                } else {
                    let mut s = String::new();
                    for i in 0..2 {
                        let n = 2 + i;
                        s.push_str(&format!("<searchMatchItem><trackID>103</trackID><timeSpan><startTime>2026-07-11T02:{:02}:00Z</startTime><endTime>2026-07-11T02:{:02}:01Z</endTime></timeSpan><contentType>picture</contentType><codecType>jpeg</codecType><playbackURI>http://nvr/pic/{}.jpg</playbackURI><size>1000</size></searchMatchItem>", n, n, n));
                    }
                    ("Success", s)
                };
                let response = format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><CMSearchResult><searchID>{id}</searchID><responseStatus>true</responseStatus><responseStatusStrg>{status}</responseStatusStrg><numOfMatches>2</numOfMatches>{items}</CMSearchResult>"#,
                    id = search_id,
                    status = status,
                    items = items
                );
                ResponseTemplate::new(200).set_body_string(response)
            }
        })
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(1),
    };

    let outcome = client.search_one_window(&window).await.unwrap();

    assert_eq!(outcome.pages_fetched, 2);
    assert_eq!(outcome.records_found, 4);
    assert_eq!(outcome.records_skipped, 0);
    assert_eq!(outcome.records_inserted, 4);

    // Verify 4 images
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 4);

    // Digest authentication sends each logical request once before the
    // challenge and once as an identical authenticated replay.
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(!requests[0].1);
    assert!(requests[1].1);
    assert!(!requests[2].1);
    assert!(requests[3].1);
    assert_eq!(requests[0].0, requests[1].0);
    assert_eq!(requests[2].0, requests[3].0);
    assert!(
        requests[0]
            .0
            .contains("<searchResultPostion>0</searchResultPostion>")
    );
    assert!(
        requests[2]
            .0
            .contains("<searchResultPostion>2</searchResultPostion>")
    );

    let uuid1 = extract_uuid(&requests[0].0);
    let uuid2 = extract_uuid(&requests[2].0);
    assert_ne!(uuid1, uuid2);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 4);
}

#[tokio::test]
async fn empty_search_advances_cursor_without_images() {
    let mock_server = MockServer::start().await;
    let capture = RequestCapture::new();

    Mock::given(capture.matcher())
        .respond_with(echo_empty())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(1),
    };

    let outcome = client.search_one_window(&window).await.unwrap();

    assert_eq!(outcome.pages_fetched, 1);
    assert_eq!(outcome.records_found, 0);
    assert_eq!(outcome.records_skipped, 0);
    assert_eq!(outcome.records_inserted, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
}

#[tokio::test]
async fn malformed_xml_fails_and_retains_cursor() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"<?xml version=\"1.0\"?><CMSearchResult><broken>"),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let prior_window = SearchWindowCommit {
        camera_id,
        window_start: now_ts(),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now_ts(),
        updated_at: now_ts(),
    };
    ops.commit_search_window(&prior_window, &[]).await.unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(2),
    };

    let result = client.search_one_window(&window).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    // Malformed XML without required fields returns InvalidNvrResponse
    // (the parser recognizes the root but finds missing responseStatus).
    assert!(
        matches!(
            err.category,
            ErrorCategory::XmlParsing | ErrorCategory::InvalidNvrResponse
        ),
        "expected XmlParsing or InvalidNvrResponse, got {:?}",
        err.category
    );

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert!(cursor.last_error.is_some());
}

#[tokio::test]
async fn nvr_failure_fails_and_retains_cursor() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_failure())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let prior_window = SearchWindowCommit {
        camera_id,
        window_start: now_ts(),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now_ts(),
        updated_at: now_ts(),
    };
    ops.commit_search_window(&prior_window, &[]).await.unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(2),
    };

    let result = client.search_one_window(&window).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::InvalidNvrResponse);

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert!(cursor.last_error.is_some());
}

#[tokio::test]
async fn digest_authentication_failure_records_cursor_error() {
    let mock_server = MockServer::start().await;
    let mock = Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        );
    mock.up_to_n_times(2).mount(&mock_server).await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        password: Some(fauna_scan::configuration::Secret::new(
            "wrong-pass".to_string(),
        )),
        ..config
    };
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("test.db")).await.unwrap();
    let ops = db.ops();
    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);
    let result = client
        .search_one_window(&SearchWindow {
            start: now_ts(),
            end: future_ts(1),
        })
        .await;
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authentication);
    assert!(
        ops.get_cursor(camera_id)
            .await
            .unwrap()
            .unwrap()
            .last_error
            .is_some()
    );
}

#[tokio::test]
async fn timeout_fails_and_retains_cursor() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(10))
                .set_body_string("delayed"),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        request_timeout_seconds: 1,
        ..config
    };
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let prior_window = SearchWindowCommit {
        camera_id,
        window_start: now_ts(),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now_ts(),
        updated_at: now_ts(),
    };
    ops.commit_search_window(&prior_window, &[]).await.unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(2),
    };

    let result = client.search_one_window(&window).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(
        err.category,
        ErrorCategory::Timeout,
        "expected Timeout, got {:?}",
        err.category
    );

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert!(cursor.last_error.is_some());
}

#[tokio::test]
async fn more_zero_items_fails_and_retains_cursor() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_more_zero())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let prior_window = SearchWindowCommit {
        camera_id,
        window_start: now_ts(),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now_ts(),
        updated_at: now_ts(),
    };
    ops.commit_search_window(&prior_window, &[]).await.unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(2),
    };

    let result = client.search_one_window(&window).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::InvalidNvrResponse);

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert!(cursor.last_error.is_some());
}

#[tokio::test]
async fn repeated_page_fails_and_retains_cursor() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_repeated_page())
        .up_to_n_times(3)
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let prior_window = SearchWindowCommit {
        camera_id,
        window_start: now_ts(),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now_ts(),
        updated_at: now_ts(),
    };
    ops.commit_search_window(&prior_window, &[]).await.unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(2),
    };

    let result = client.search_one_window(&window).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::InvalidNvrResponse);

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert!(cursor.last_error.is_some());
}

#[tokio::test]
async fn malformed_sibling_retains_valid_items() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_mixed_items())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(1),
    };

    let outcome = client.search_one_window(&window).await.unwrap();

    assert_eq!(outcome.pages_fetched, 1);
    assert_eq!(outcome.records_found, 1);
    assert_eq!(outcome.records_skipped, 1);
    assert_eq!(outcome.records_inserted, 1);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn media_filtering_keeps_only_pictures() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_mixed_media())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(1),
    };

    let outcome = client.search_one_window(&window).await.unwrap();

    assert_eq!(outcome.pages_fetched, 1);
    assert_eq!(outcome.records_found, 2);
    assert_eq!(outcome.records_skipped, 1);
    assert_eq!(outcome.records_inserted, 1);
}

#[tokio::test]
async fn duplicate_timestamps_distinct_uris_produce_two_rows() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_duplicate_timestamps())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(1),
    };

    let outcome = client.search_one_window(&window).await.unwrap();

    assert_eq!(outcome.pages_fetched, 1);
    assert_eq!(outcome.records_found, 2);
    assert_eq!(outcome.records_skipped, 0);
    assert_eq!(outcome.records_inserted, 2);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn cursor_error_recorded_on_failure() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(echo_failure())
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    let camera_id = CameraId::new(1);
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now_ts(),
    )
    .await
    .unwrap();

    let prior_window = SearchWindowCommit {
        camera_id,
        window_start: now_ts(),
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now_ts(),
        updated_at: now_ts(),
    };
    ops.commit_search_window(&prior_window, &[]).await.unwrap();

    let client = build_search_client(transport, ops.clone(), camera_id);

    let window = SearchWindow {
        start: now_ts(),
        end: future_ts(2),
    };

    let _result = client.search_one_window(&window).await;

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert!(cursor.last_error.is_some());
    assert!(
        cursor
            .last_error
            .as_ref()
            .unwrap()
            .contains("search failed")
    );
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(cursor.next_search_at, Some(future_ts(2)));
}

fn extract_uuid(xml: &str) -> Uuid {
    let start = xml.find("<searchID>").unwrap() + 10;
    let end = xml[start..].find("</searchID>").unwrap();
    let uuid_str = &xml[start..start + end];
    uuid_str.parse().unwrap()
}
