use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use fauna_scan::database::Database;
use fauna_scan::database::models::{CameraDiscovery, DiscoveredImage, SearchWindowCommit};
use fauna_scan::domain::{ImageKey, ProcessingStatus, Timestamp, TrackId};
use fauna_scan::error::{AppError, AppResult, ErrorCategory};
use fauna_scan::service_lifecycle::{
    ServiceLifecycleOptions, ShutdownReason, ShutdownToken, install_sanitized_panic_hook,
    run_single_pipeline_until_signal, supervise_service,
};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn test_database() -> (Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::open(&dir.path().join("lifecycle.db"))
        .await
        .unwrap();
    (database, dir)
}

#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn options() -> ServiceLifecycleOptions {
    ServiceLifecycleOptions {
        shutdown_timeout: Duration::from_secs(1),
        summary_interval: Duration::from_secs(60),
    }
}

fn service_command(config_path: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_fauna-scan"));
    command
        .env("CLICOLOR_FORCE", "0")
        .arg("--log-level")
        .arg("info")
        .arg("--config")
        .arg(config_path)
        .arg("run");
    command
}

fn service_config(
    dir: &std::path::Path,
    server: &MockServer,
    classifier: bool,
) -> std::path::PathBuf {
    let server_url = url::Url::parse(&server.uri()).unwrap();
    let config_path = dir.join("config.toml");
    let classifier_config = if classifier {
        r#"[classifier]
enabled = true
base_url = "http://127.0.0.1:9/v1"
endpoint = "/chat/completions"
model = "test-model"
request_timeout_seconds = 1
processing_lease_seconds = 5
prompt_version = "test"
"#
    } else {
        "[classifier]\nenabled = false\n"
    };
    let config = format!(
        r#"[general]
database_path = "{}"
output_directory = "{}"

[nvr]
scheme = "{}"
host = "{}"
port = {}
username = "test-user"
password = "test-password"
start_at = "2026-01-01T00:00:00Z"

[nvr.search]
poll_interval_seconds = 1
camera_refresh_interval_seconds = 3600
settlement_delay_seconds = 1

{}
"#,
        dir.join("service.db").display(),
        dir.join("images").display(),
        server_url.scheme(),
        server_url.host_str().unwrap(),
        server_url.port().unwrap(),
        classifier_config,
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

fn config_for_classifier(
    dir: &std::path::Path,
    nvr_server: &MockServer,
    classifier_server: &MockServer,
    api_key: Option<&str>,
) -> std::path::PathBuf {
    let path = service_config(dir, nvr_server, true);
    let mut config = std::fs::read_to_string(&path).unwrap();
    config = config.replace(
        "base_url = \"http://127.0.0.1:9/v1\"",
        &format!(
            "base_url = \"{}/v1\"{}",
            classifier_server.uri(),
            api_key
                .map(|key| format!("\napi_key = \"{key}\""))
                .unwrap_or_default()
        ),
    );
    std::fs::write(&path, config).unwrap();
    path
}

async fn wait_for_application_metadata(db_path: &std::path::Path) {
    for _ in 0..200 {
        if db_path.exists()
            && let Ok(db) = Database::open(db_path).await
            && db
                .ops()
                .get_metadata(&fauna_scan::database::models::ServiceMetadataKey::ApplicationVersion)
                .await
                .ok()
                .flatten()
                .is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("service did not reach startup metadata");
}

async fn seed_downloaded_image(db_path: &std::path::Path, output_dir: &std::path::Path) {
    let database = Database::open(db_path).await.unwrap();
    let now = Timestamp::new(Utc::now());
    let camera_id = database
        .ops()
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
    database
        .ops()
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
                image_key: ImageKey::new("process-recovery-image"),
                camera_id,
                track_id: TrackId::new("103"),
                capture_start_at: now,
                capture_end_at: None,
                playback_uri: "http://nvr/image".into(),
                canonical_playback_uri: "http://nvr/image".into(),
                codec_type: Some("jpeg".into()),
                content_type: Some("picture".into()),
                nvr_reported_size: None,
                discovered_at: now,
            }],
        )
        .await
        .unwrap();
    let claim = database
        .ops()
        .claim_next_download(
            &now,
            &Timestamp::new(Utc::now() + chrono::Duration::minutes(5)),
        )
        .await
        .unwrap()
        .unwrap();
    std::fs::create_dir_all(output_dir).unwrap();
    let image_path = output_dir.join("recovery.jpg");
    std::fs::write(&image_path, [0xff, 0xd8, 0xff, 0xe0, 0xff, 0xd9]).unwrap();
    database
        .ops()
        .complete_download(claim.image_id, &image_path, &now)
        .await
        .unwrap();
}

fn scan_command(config_path: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_fauna-scan"));
    command
        .env("CLICOLOR_FORCE", "0")
        .arg("--log-level")
        .arg("info")
        .arg("--config")
        .arg(config_path)
        .arg("scan");
    command
}

async fn wait_for_processing_status(db_path: &std::path::Path, status: ProcessingStatus) {
    for _ in 0..300 {
        if let Ok(database) = Database::open(db_path).await
            && let Ok(image) = database
                .ops()
                .get_image(fauna_scan::domain::ImageId::new(1))
                .await
            && image.processing_status == status
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("image did not reach processing status {status}");
}

fn classifier_success_body() -> String {
    serde_json::json!({
        "choices": [{
            "message": {
                "content": r#"{"contains_animal":true,"contains_wildlife":true,"is_interesting":true,"species":[],"overall_confidence":0.9,"summary":"ok","uncertainties":[]}"#
            }
        }]
    })
    .to_string()
}

async fn stop_process(
    child: std::process::Child,
    signal_kind: Signal,
) -> (std::process::ExitStatus, String) {
    signal::kill(Pid::from_raw(child.id() as i32), signal_kind).unwrap();
    let output = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status, logs)
}

#[tokio::test]
async fn signal_shutdown_cancels_both_primary_pipelines() {
    let (database, _dir) = test_database().await;
    let token = ShutdownToken::new();
    let downloader_token = token.clone();
    let scanner_token = token.clone();
    let downloader = tokio::spawn(async move {
        downloader_token.cancelled().await;
        Ok::<(), AppError>(())
    });
    let scanner = tokio::spawn(async move {
        scanner_token.cancelled().await;
        Ok::<(), AppError>(())
    });

    supervise_service(
        downloader,
        scanner,
        token,
        database.ops(),
        options(),
        async { Ok(ShutdownReason::SigInt) },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn unexpected_pipeline_success_is_fatal_and_cancels_sibling() {
    let (database, _dir) = test_database().await;
    let token = ShutdownToken::new();
    let sibling_token = token.clone();
    let downloader = tokio::spawn(async { Ok::<(), AppError>(()) });
    let scanner = tokio::spawn(async move {
        sibling_token.cancelled().await;
        Ok::<(), AppError>(())
    });

    let error = supervise_service(
        downloader,
        scanner,
        token,
        database.ops(),
        options(),
        futures::future::pending::<AppResult<ShutdownReason>>(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Internal);
    assert!(error.message.contains("downloader"));
}

#[tokio::test]
async fn single_pipeline_signal_shutdown_is_successful() {
    let token = ShutdownToken::new();
    let pipeline_token = token.clone();
    run_single_pipeline_until_signal(
        "scanner",
        async move {
            pipeline_token.cancelled().await;
            Ok::<(), AppError>(())
        },
        token,
        async { Ok(ShutdownReason::SigTerm) },
        Duration::from_secs(1),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn pipeline_error_is_propagated_and_cancels_sibling() {
    let (database, _dir) = test_database().await;
    let token = ShutdownToken::new();
    let sibling_token = token.clone();
    let downloader = tokio::spawn(async {
        Err::<(), AppError>(AppError::new(
            ErrorCategory::Database,
            "test_pipeline",
            "database failed",
        ))
    });
    let scanner = tokio::spawn(async move {
        sibling_token.cancelled().await;
        Ok::<(), AppError>(())
    });

    let error = supervise_service(
        downloader,
        scanner,
        token,
        database.ops(),
        options(),
        futures::future::pending::<AppResult<ShutdownReason>>(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Database);
    assert_eq!(error.message, "database failed");
}

#[tokio::test]
async fn panicking_pipeline_is_a_safe_internal_error() {
    let (database, _dir) = test_database().await;
    let token = ShutdownToken::new();
    let sibling_token = token.clone();
    let downloader = tokio::spawn(async move {
        sibling_token.cancelled().await;
        Ok::<(), AppError>(())
    });
    let scanner = tokio::spawn(async {
        panic!("classifier response must not be logged");
        #[allow(unreachable_code)]
        Ok::<(), AppError>(())
    });

    let error = supervise_service(
        downloader,
        scanner,
        token,
        database.ops(),
        options(),
        futures::future::pending::<AppResult<ShutdownReason>>(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.category, ErrorCategory::Internal);
    assert_eq!(error.message, "scanner pipeline task failed");
    assert!(!error.message.contains("classifier response"));
}

#[tokio::test]
async fn panic_hook_suppresses_payload_in_child_process() {
    if std::env::var_os("FAUNA_SCAN_PANIC_HOOK_CHILD").is_some() {
        install_sanitized_panic_hook();
        panic!("panic-secret-classifier-response");
    }

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "panic_hook_suppresses_payload_in_child_process",
            "--nocapture",
        ])
        .env("FAUNA_SCAN_PANIC_HOOK_CHILD", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let mut output_text = String::from_utf8_lossy(&output.stdout).into_owned();
    output_text.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(!output_text.contains("panic-secret-classifier-response"));
    assert!(output_text.contains("panic details suppressed"));
}

#[tokio::test]
async fn interrupted_processing_work_is_recovered_after_lease_expiry() {
    let (database, _dir) = test_database().await;
    let observed = Timestamp::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    let camera = database
        .ops()
        .sync_cameras(
            &[CameraDiscovery {
                channel_number: 1,
                primary_track_id: "101".into(),
                picture_track_id: "103".into(),
                name: None,
                raw_discovery_identifier: None,
            }],
            &observed,
        )
        .await
        .unwrap()[0]
        .id;
    sqlx::query(
        "INSERT INTO images (image_key, camera_id, track_id, capture_start_at, playback_uri, canonical_playback_uri, local_path, download_status, processing_status, discovered_at, created_at, updated_at) VALUES ('interrupted', ?, '103', ?, 'http://nvr/image', 'http://nvr/image', '/tmp/interrupted.jpg', 'downloaded', 'new', ?, ?, ?)",
    )
    .bind(camera.get())
    .bind(fauna_scan::database::format_timestamp(&observed))
    .bind(fauna_scan::database::format_timestamp(&observed))
    .bind(fauna_scan::database::format_timestamp(&observed))
    .bind(fauna_scan::database::format_timestamp(&observed))
    .execute(database.pool())
    .await
    .unwrap();
    let lease_until = Timestamp::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap());
    let claim = database
        .ops()
        .claim_next_processing(&observed, &lease_until)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        database
            .ops()
            .get_image(claim.image_id)
            .await
            .unwrap()
            .processing_status,
        ProcessingStatus::Processing
    );

    let recovered_at = Timestamp::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 2, 0).unwrap());
    let recovery = database
        .ops()
        .recover_expired_leases(&recovered_at)
        .await
        .unwrap();
    assert_eq!(recovery.processing, 1);
    let image = database.ops().get_image(claim.image_id).await.unwrap();
    assert_eq!(image.processing_status, ProcessingStatus::RetryWait);
    assert!(image.processing_lease_until.is_none());
}

#[tokio::test]
async fn periodic_summary_logs_all_operational_fields() {
    let (database, _dir) = test_database().await;
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer({
            let buffer = buffer.clone();
            move || BufferWriter(buffer.clone())
        })
        .finish();
    let token = ShutdownToken::new();
    let downloader_token = token.clone();
    let scanner_token = token.clone();
    let _guard = tracing::subscriber::set_default(subscriber);
    let result = supervise_service(
        tokio::spawn(async move {
            downloader_token.cancelled().await;
            Ok::<(), AppError>(())
        }),
        tokio::spawn(async move {
            scanner_token.cancelled().await;
            Ok::<(), AppError>(())
        }),
        token,
        database.ops(),
        ServiceLifecycleOptions {
            shutdown_timeout: Duration::from_secs(1),
            summary_interval: Duration::from_millis(1),
        },
        async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(ShutdownReason::SigInt)
        },
    )
    .await;
    result.unwrap();
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    for field in [
        "cameras_active=0",
        "images_discovered=0",
        "images_downloaded=0",
        "downloads_pending=0",
        "images_awaiting_classification=0",
        "classifications_completed=0",
        "retryable_failures=0",
        "permanent_failures=0",
    ] {
        assert!(logs.contains(field), "missing {field} in {logs}");
    }
}

#[tokio::test]
async fn shutdown_deadline_aborts_uncooperative_pipelines() {
    let (database, _dir) = test_database().await;
    let token = ShutdownToken::new();
    let downloader = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok::<(), AppError>(())
    });
    let scanner = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok::<(), AppError>(())
    });
    let started = Instant::now();
    supervise_service(
        downloader,
        scanner,
        token,
        database.ops(),
        ServiceLifecycleOptions {
            shutdown_timeout: Duration::from_millis(20),
            summary_interval: Duration::from_secs(60),
        },
        async { Ok(ShutdownReason::SigInt) },
    )
    .await
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn sigint_process_shuts_down_gracefully() {
    run_process_signal_test(Signal::SIGINT, "SIGINT").await;
}

#[tokio::test]
async fn sigterm_process_shuts_down_gracefully() {
    run_process_signal_test(Signal::SIGTERM, "SIGTERM").await;
}

async fn run_process_signal_test(signal_kind: Signal, signal_name: &str) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<StreamingChannelList></StreamingChannelList>"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = service_config(dir.path(), &server, true);
    let db_path = dir.path().join("service.db");
    let mut command = service_command(&config_path);
    command.stderr(Stdio::piped()).stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_application_metadata(&db_path).await;
    let (status, stderr) = stop_process(child, signal_kind).await;
    assert!(status.success(), "service stderr: {stderr}");
    assert!(stderr.contains("Starting downloader and scanner pipelines"));
    assert!(stderr.contains("Shutdown signal received"));
    assert!(stderr.contains(signal_name));
    assert!(stderr.contains("Graceful shutdown completed"));
}

#[tokio::test]
async fn process_redaction_covers_digest_classifier_and_image_payloads() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(|request: &wiremock::Request| {
            if request.headers.contains_key("authorization") {
                ResponseTemplate::new(200).set_body_string(
                    "<StreamingChannelList><StreamingChannel><id>ch1</id><trackID>101</trackID></StreamingChannel></StreamingChannelList>",
                )
            } else {
                ResponseTemplate::new(401).insert_header(
                    "WWW-Authenticate",
                    "Digest realm=\"sentinel-realm\", nonce=\"sentinel-nonce\", algorithm=MD5, qop=\"auth\"",
                )
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("classifier-raw-response-sentinel"),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let config_path = config_for_classifier(
        dir.path(),
        &server,
        &server,
        Some("classifier-api-key-sentinel"),
    );
    let config = std::fs::read_to_string(&config_path).unwrap().replace(
        "password = \"test-password\"",
        "password = \"nvr-password-sentinel\"",
    );
    std::fs::write(&config_path, config).unwrap();
    seed_downloaded_image(&dir.path().join("service.db"), &dir.path().join("images")).await;

    let mut command = service_command(&config_path);
    command.stderr(Stdio::piped()).stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_application_metadata(&dir.path().join("service.db")).await;

    let mut classifier_request = None;
    for _ in 0..300 {
        let requests = server.received_requests().await.unwrap();
        if let Some(request) = requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
        {
            classifier_request = Some(request.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let request = classifier_request.expect("scanner did not submit the image");
    assert!(
        request
            .headers
            .get("authorization")
            .is_some_and(|value| value.to_str().unwrap().starts_with("Bearer "))
    );
    assert!(String::from_utf8_lossy(&request.body).contains("data:image/jpeg;base64"));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| {
                request.url.path() == "/ISAPI/Streaming/channels"
                    && request
                        .headers
                        .get("authorization")
                        .is_some_and(|value| value.to_str().unwrap().starts_with("Digest "))
            })
    );

    let (status, logs) = stop_process(child, Signal::SIGINT).await;
    assert!(status.success(), "service logs: {logs}");
    for secret in [
        "nvr-password-sentinel",
        "classifier-api-key-sentinel",
        "sentinel-realm",
        "sentinel-nonce",
        "classifier-raw-response-sentinel",
        "Authorization",
        "data:image/jpeg;base64",
    ] {
        assert!(!logs.contains(secret), "sensitive value leaked: {secret}");
    }
}

#[tokio::test]
async fn interrupted_processing_work_is_recovered_by_a_restarted_process() {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let requests = requests.clone();
            move |_request: &wiremock::Request| {
                if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_secs(10))
                        .set_body_string(classifier_success_body())
                } else {
                    ResponseTemplate::new(200).set_body_string(classifier_success_body())
                }
            }
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = config_for_classifier(dir.path(), &server, &server, None);
    let config = std::fs::read_to_string(&config_path).unwrap().replace(
        "processing_lease_seconds = 5",
        "processing_lease_seconds = 2",
    );
    std::fs::write(&config_path, config).unwrap();
    let db_path = dir.path().join("service.db");
    seed_downloaded_image(&db_path, &dir.path().join("images")).await;

    let mut first = scan_command(&config_path);
    first.stderr(Stdio::piped()).stdout(Stdio::piped());
    let first_child = first.spawn().unwrap();
    wait_for_processing_status(&db_path, ProcessingStatus::Processing).await;
    let (first_status, _) = stop_process(first_child, Signal::SIGKILL).await;
    assert!(!first_status.success());

    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut second = scan_command(&config_path);
    second.stderr(Stdio::piped()).stdout(Stdio::piped());
    let second_child = second.spawn().unwrap();
    wait_for_processing_status(&db_path, ProcessingStatus::Done).await;
    let (second_status, logs) = stop_process(second_child, Signal::SIGINT).await;
    assert!(second_status.success(), "restarted scanner logs: {logs}");
    assert!(requests.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn startup_discovery_failure_is_retried_without_waiting_for_refresh_interval() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<StreamingChannelList></StreamingChannelList>"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = service_config(dir.path(), &server, true);
    let db_path = dir.path().join("service.db");
    let mut command = service_command(&config_path);
    command.stderr(Stdio::piped()).stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_application_metadata(&db_path).await;

    let mut retried = false;
    for _ in 0..100 {
        if server.received_requests().await.unwrap().len() >= 2 {
            retried = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_status, _stderr) = stop_process(child, Signal::SIGINT).await;
    assert!(retried, "initial discovery was not retried promptly");
}

#[tokio::test]
async fn fatal_database_failure_propagates_nonzero_and_stops_both_pipelines() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<StreamingChannelList></StreamingChannelList>"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = service_config(dir.path(), &server, true);
    let db_path = dir.path().join("service.db");
    let mut command = service_command(&config_path);
    command.stderr(Stdio::piped()).stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for_application_metadata(&db_path).await;

    let db = Database::open(&db_path).await.unwrap();
    sqlx::query("DROP TABLE images")
        .execute(db.pool())
        .await
        .unwrap();
    let output = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(logs.contains("Primary pipeline terminated"));
    for secret in [
        "test-password",
        "Authorization",
        "classifier-response-sentinel",
        "data:image/jpeg;base64",
    ] {
        assert!(!logs.contains(secret), "sensitive value leaked: {secret}");
    }
}

#[tokio::test]
async fn database_startup_failure_precedes_pipeline_start_and_external_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = service_config(dir.path(), &server, true);
    std::fs::create_dir(dir.path().join("service.db")).unwrap();
    let output = service_command(&config_path).output().unwrap();
    assert!(!output.status.success());
    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(!logs.contains("Starting downloader and scanner pipelines"));
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn disabled_classifier_fails_before_pipeline_start_or_external_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config_path = service_config(dir.path(), &server, false);
    let output = service_command(&config_path).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("classifier is disabled"));
    assert!(!stderr.contains("Starting downloader and scanner pipelines"));
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}
