//! Full Phase 12 acceptance harness: authenticated NVR discovery/search/playback,
//! durable downloading, classification, restart replay, and overlap polling.

use chrono::Utc;
use fauna_scan::classifier::ClassifierClient;
use fauna_scan::configuration::{
    ClassifierConfig, ClassifierEndpointConfig, ClassifierGenerationConfig, Config, GeneralConfig,
    NvrConfig, NvrDownloadConfig, NvrSearchConfig, Secret,
};
use fauna_scan::database::Database;
use fauna_scan::domain::{DownloadStatus, ProcessingStatus, Timestamp};
use fauna_scan::downloader::orchestration::{
    DownloaderOrchestrator, DownloaderOrchestratorOptions,
};
use fauna_scan::downloader::{DownloadWorker, DownloadWorkerOptions};
use fauna_scan::error::{AppResult, ErrorCategory};
use fauna_scan::nvr::{ImageDownloadClient, NvrTransport};
use fauna_scan::scanner::{Scanner, ScannerOptions};
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DIGEST_CHALLENGE: &str =
    "Digest realm=\"Hikvision\", nonce=\"phase12-nonce\", algorithm=MD5, qop=\"auth\"";

#[derive(Clone, Debug)]
struct MockImage {
    track_id: String,
    capture_start_at: Timestamp,
    capture_end_at: Timestamp,
    playback_path: String,
}

struct HarnessState {
    images: Arc<RwLock<Vec<MockImage>>>,
    search_successes: AtomicUsize,
    playback_successes: AtomicUsize,
    classifier_submissions: AtomicUsize,
    search_requests: Mutex<Vec<(Uuid, u64, String)>>,
}

impl HarnessState {
    fn new(images: Vec<MockImage>) -> Arc<Self> {
        Arc::new(Self {
            images: Arc::new(RwLock::new(images)),
            search_successes: AtomicUsize::new(0),
            playback_successes: AtomicUsize::new(0),
            classifier_submissions: AtomicUsize::new(0),
            search_requests: Mutex::new(Vec::new()),
        })
    }
}

struct DiscoveryResponder;

impl Respond for DiscoveryResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if !request.headers.contains_key("authorization") {
            return ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE);
        }
        ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0"?><StreamingChannelList xmlns="http://www.hikvision.com/ver20/XMLSchema">
<StreamingChannel><id>camera-101</id><trackID>101</trackID><name>Front</name></StreamingChannel>
<StreamingChannel><id>camera-301</id><trackID>301</trackID><name>Garden</name></StreamingChannel>
</StreamingChannelList>"#,
        )
    }
}

struct SearchResponder {
    state: Arc<HarnessState>,
}

fn xml_value(body: &[u8], tag: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let begin = text.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = text[begin..].find(&format!("</{tag}>"))?;
    Some(text[begin..begin + end].to_string())
}

impl Respond for SearchResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if !request.headers.contains_key("authorization") {
            return ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE);
        }

        let Some(search_id) = xml_value(&request.body, "searchID")
            .and_then(|id| Uuid::parse_str(id.trim_matches(['{', '}'])).ok())
        else {
            return ResponseTemplate::new(400);
        };
        let track = xml_value(&request.body, "trackID").unwrap_or_default();
        let start =
            xml_value(&request.body, "startTime").and_then(|value| value.parse::<Timestamp>().ok());
        let end =
            xml_value(&request.body, "endTime").and_then(|value| value.parse::<Timestamp>().ok());
        let position = xml_value(&request.body, "searchResultPostion")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        let max_results = xml_value(&request.body, "maxResults")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(50);
        let (Some(start), Some(end)) = (start, end) else {
            return ResponseTemplate::new(400);
        };

        let mut matches: Vec<MockImage> = self
            .state
            .images
            .read()
            .expect("fixture lock")
            .iter()
            .filter(|image| {
                image.track_id == track
                    && image.capture_start_at >= start
                    && image.capture_start_at < end
            })
            .cloned()
            .collect();
        matches.sort_by(|a, b| a.playback_path.cmp(&b.playback_path));

        let start_index = usize::try_from(position).unwrap_or(usize::MAX);
        let page: Vec<MockImage> = matches
            .iter()
            .skip(start_index)
            .take(max_results)
            .cloned()
            .collect();
        let status = if start_index.saturating_add(page.len()) < matches.len() {
            "MORE"
        } else {
            "OK"
        };

        self.state.search_successes.fetch_add(1, Ordering::SeqCst);
        self.state
            .search_requests
            .lock()
            .expect("request lock")
            .push((search_id, position, track.clone()));

        let items = page
            .iter()
            .map(|image| {
                format!(
                    "<searchMatchItem><trackID>{}</trackID><timeSpan><startTime>{}</startTime><endTime>{}</endTime></timeSpan><contentType>picture</contentType><codecType>jpeg</codecType><playbackURI>http://fixture.invalid{}</playbackURI><size>32</size></searchMatchItem>",
                    image.track_id,
                    image.capture_start_at,
                    image.capture_end_at,
                    image.playback_path,
                )
            })
            .collect::<String>();
        let body = format!(
            "<?xml version=\"1.0\"?><CMSearchResult><searchID>{search_id}</searchID><responseStatus>true</responseStatus><responseStatusStrg>{status}</responseStatusStrg><numOfMatches>{total}</numOfMatches>{items}</CMSearchResult>",
            total = matches.len(),
        );
        ResponseTemplate::new(200).set_body_string(body)
    }
}

struct PlaybackResponder {
    state: Arc<HarnessState>,
}

fn minimal_jpeg() -> Vec<u8> {
    vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, b'J', b'F', 0xff, 0xd9]
}

impl Respond for PlaybackResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if !request.headers.contains_key("authorization") {
            return ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE);
        }
        self.state.playback_successes.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_bytes(minimal_jpeg())
    }
}

struct ClassifierResponder {
    state: Arc<HarnessState>,
}

impl Respond for ClassifierResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.state
            .classifier_submissions
            .fetch_add(1, Ordering::SeqCst);
        let content = json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"name": "small mammal", "confidence": 0.8}],
            "overall_confidence": 0.8,
            "summary": "A small wild animal is visible.",
            "uncertainties": []
        })
        .to_string();
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "phase12-classification",
            "choices": [{"message": {"role": "assistant", "content": content}}]
        }))
    }
}

struct RuntimeHarness {
    _database: Database,
    orchestrator: DownloaderOrchestrator,
    scanner: Scanner,
}

fn build_test_config(server: &MockServer, root: &Path, start_at: Timestamp) -> Config {
    let uri = Url::parse(&server.uri()).expect("mock URI");
    let host = uri.host_str().expect("mock host").to_string();
    let port = uri.port().expect("mock port");
    Config {
        general: GeneralConfig {
            database_path: root.join("state/fauna-scan.sqlite3"),
            output_directory: root.join("Pictures/fauna-scan"),
            log_level: fauna_scan::cli::LogLevel::Info,
        },
        nvr: NvrConfig {
            scheme: "http".to_string(),
            host,
            port,
            username: "admin".to_string(),
            password: Some(Secret::new("test-password".to_string())),
            start_at,
            request_timeout_seconds: 5,
            connect_timeout_seconds: 2,
            allow_invalid_tls_certificates: false,
            search: NvrSearchConfig {
                window_minutes: 60,
                max_results: 50,
                poll_interval_seconds: 60,
                poll_overlap_seconds: 120,
                camera_refresh_interval_seconds: 3600,
                settlement_delay_seconds: 0,
            },
            download: NvrDownloadConfig {
                retry_limit: 2,
                retry_initial_delay_seconds: 1,
                retry_max_delay_seconds: 2,
                maximum_image_size_bytes: 1_000_000,
                verify_jpeg: true,
                rebase_playback_urls: true,
                concurrency: 4,
                playback_host_allowlist: vec![],
            },
        },
        classifier: ClassifierConfig {
            endpoints: vec![ClassifierEndpointConfig {
                base_url: Url::parse(&format!("{}/v1", server.uri())).expect("classifier URI"),
                endpoint: "/chat/completions".to_string(),
                model: "phase12-model".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 5,
                prompt_version: "phase12-test".to_string(),
                generation: ClassifierGenerationConfig {
                    temperature: 0.0,
                    max_tokens: 100,
                },
            }],
            poll_interval_seconds: 1,
            retry_limit: 2,
            retry_initial_delay_seconds: 1,
            retry_max_delay_seconds: 2,
            processing_lease_seconds: 60,
        },
        source_path: root.join("config.toml"),
    }
}

async fn build_runtime(config: &Config) -> AppResult<RuntimeHarness> {
    std::fs::create_dir_all(config.general.database_path.parent().unwrap()).map_err(|e| {
        fauna_scan::error::AppError::new(ErrorCategory::Filesystem, "test", e.to_string())
    })?;
    std::fs::create_dir_all(&config.general.output_directory).map_err(|e| {
        fauna_scan::error::AppError::new(ErrorCategory::Filesystem, "test", e.to_string())
    })?;
    let database = Database::open(&config.general.database_path).await?;
    let transport = Arc::new(NvrTransport::from_config(&config.nvr)?);
    let download_client = Arc::new(ImageDownloadClient::from_config(
        transport.clone(),
        &config.nvr,
    ));
    let worker = DownloadWorker::new(
        database.ops().clone(),
        download_client,
        DownloadWorkerOptions::from_config(config)?,
    );
    let options = DownloaderOrchestratorOptions::from_config(config)?;
    let mut orchestrator =
        DownloaderOrchestrator::new(database.ops().clone(), transport, worker, options)?;
    orchestrator.startup_housekeeping().await?;
    orchestrator.persist_nvr_identity().await?;
    let classifier = ClassifierClient::from_config(&config.classifier)?;
    let scanner = Scanner::new(
        database.ops(),
        Arc::new(classifier),
        ScannerOptions::from_config(config)?,
    );
    Ok(RuntimeHarness {
        _database: database,
        orchestrator,
        scanner,
    })
}

async fn scalar_i64(pool: &sqlx::SqlitePool, sql: &str) -> AppResult<i64> {
    sqlx::query_scalar(sql).fetch_one(pool).await.map_err(|e| {
        fauna_scan::error::AppError::new(ErrorCategory::Database, "test_query", e.to_string())
    })
}

#[tokio::test]
async fn full_pipeline_is_restart_safe_and_polls_only_new_images() -> AppResult<()> {
    let server = MockServer::start().await;
    let root = tempfile::tempdir().expect("temporary root");
    let start_at = Timestamp::new(Utc::now() - chrono::Duration::seconds(60));
    let capture = Timestamp::new(Utc::now() - chrono::Duration::seconds(20));
    let capture_end = Timestamp::new(*capture.as_datetime() + chrono::Duration::seconds(1));
    let mut images = Vec::new();
    for index in 0..51 {
        images.push(MockImage {
            track_id: "103".to_string(),
            capture_start_at: capture,
            capture_end_at: capture_end,
            playback_path: format!("/picture/103-{index}"),
        });
    }
    for index in 0..2 {
        images.push(MockImage {
            track_id: "303".to_string(),
            capture_start_at: capture,
            capture_end_at: capture_end,
            playback_path: format!("/picture/303-{index}"),
        });
    }
    let state = HarnessState::new(images);

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(DiscoveryResponder)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(SearchResponder {
            state: state.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/picture/.*"))
        .respond_with(PlaybackResponder {
            state: state.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ClassifierResponder {
            state: state.clone(),
        })
        .mount(&server)
        .await;

    let config = build_test_config(&server, root.path(), start_at);
    let mut runtime = build_runtime(&config).await?;
    let first = runtime.orchestrator.execute_one_pass(true).await?;
    assert_eq!(first.cameras_discovered, 2);
    assert_eq!(first.images_discovered, 53);
    assert_eq!(first.download_pass.downloaded, 53);
    let first_scan = runtime.scanner.clone().execute_one_pass().await?;
    assert_eq!(first_scan.completed, 53);

    let pool = runtime.orchestrator.database.pool();
    assert_eq!(
        scalar_i64(pool, "SELECT COUNT(*) FROM cameras WHERE enabled = 1").await?,
        2
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*) FROM cameras WHERE primary_track_id = '101' AND picture_track_id = '103'"
        )
        .await?,
        1
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*) FROM cameras WHERE primary_track_id = '301' AND picture_track_id = '303'"
        )
        .await?,
        1
    );
    assert_eq!(scalar_i64(pool, "SELECT COUNT(*) FROM images").await?, 53);
    assert_eq!(
        scalar_i64(pool, "SELECT COUNT(*) FROM classifications").await?,
        53
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*) FROM images WHERE download_status = 'downloaded'"
        )
        .await?,
        53
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*) FROM images WHERE processing_status = 'done'"
        )
        .await?,
        53
    );
    assert!(scalar_i64(pool, "SELECT COUNT(*) FROM images WHERE capture_start_at = (SELECT capture_start_at FROM images LIMIT 1)").await? >= 2);
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(DISTINCT canonical_playback_uri) FROM images"
        )
        .await?,
        53
    );

    let requests = state.search_requests.lock().expect("request lock").clone();
    assert!(
        requests
            .iter()
            .any(|(_, position, track)| *position == 0 && track == "103")
    );
    assert!(
        requests
            .iter()
            .any(|(_, position, track)| *position == 50 && track == "103")
    );
    let unique_ids: std::collections::HashSet<Uuid> =
        requests.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(unique_ids.len(), requests.len());
    assert!(requests.iter().all(|(id, _, _)| id.get_version_num() == 4));
    let initial_playbacks = state.playback_successes.load(Ordering::SeqCst);
    let initial_classifications = state.classifier_submissions.load(Ordering::SeqCst);
    assert_eq!(initial_playbacks, 53);
    assert_eq!(initial_classifications, 53);

    drop(runtime);
    let mut restarted = build_runtime(&config).await?;
    let restart = restarted.orchestrator.execute_one_pass(true).await?;
    assert_eq!(restart.images_discovered, 0);
    assert_eq!(restart.download_pass.claimed, 0);
    let restart_scan = restarted.scanner.clone().execute_one_pass().await?;
    assert_eq!(restart_scan.claimed, 0);
    assert_eq!(
        state.playback_successes.load(Ordering::SeqCst),
        initial_playbacks
    );
    assert_eq!(
        state.classifier_submissions.load(Ordering::SeqCst),
        initial_classifications
    );

    let new_capture = Timestamp::new(Utc::now() - chrono::Duration::seconds(1));
    state.images.write().expect("fixture lock").push(MockImage {
        track_id: "103".to_string(),
        capture_start_at: new_capture,
        capture_end_at: Timestamp::new(*new_capture.as_datetime() + chrono::Duration::seconds(1)),
        playback_path: "/picture/103-new".to_string(),
    });
    let poll = restarted.orchestrator.run_continuous_iteration().await?;
    assert_eq!(poll.images_discovered, 1);
    assert_eq!(poll.download_pass.downloaded, 1);
    let poll_scan = restarted.scanner.clone().execute_one_pass().await?;
    assert_eq!(poll_scan.completed, 1);
    assert_eq!(
        scalar_i64(
            restarted.orchestrator.database.pool(),
            "SELECT COUNT(*) FROM images"
        )
        .await?,
        54
    );
    assert_eq!(
        scalar_i64(
            restarted.orchestrator.database.pool(),
            "SELECT COUNT(*) FROM classifications"
        )
        .await?,
        54
    );
    assert_eq!(
        state.playback_successes.load(Ordering::SeqCst),
        initial_playbacks + 1
    );
    assert_eq!(
        state.classifier_submissions.load(Ordering::SeqCst),
        initial_classifications + 1
    );

    let counts = restarted.orchestrator.database.status_counts().await?;
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&54));
    assert_eq!(counts.processing.get(&ProcessingStatus::Done), Some(&54));
    Ok(())
}
