//! Process-level integration tests for the fauna-scan CLI.
//!
//! Verifies the externally visible CLI contract: help, version, command
//! listing, configuration validation, lifecycle dispatch, and durable status
//! output.

use assert_cmd::Command;
use predicates::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cmd() -> Command {
    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    // Disable color output so assertions match stable substrings.
    cmd.env("CLICOLOR_FORCE", "0");
    cmd
}

// ── Help and version ──────────────────────────────────────────────────────

#[test]
fn help_exits_successfully() {
    cmd().arg("--help").assert().success();
}

#[test]
fn help_lists_all_subcommands() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("run")
            .and(predicate::str::contains("check-config"))
            .and(predicate::str::contains("discover"))
            .and(predicate::str::contains("download"))
            .and(predicate::str::contains("scan"))
            .and(predicate::str::contains("status")),
    );
}

#[test]
fn help_lists_global_options() {
    cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config").and(predicate::str::contains("--log-level")));
}

#[test]
fn version_exits_successfully() {
    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("fauna-scan").and(predicate::str::contains("0.1.0")));
}

// ── Global options ────────────────────────────────────────────────────────

#[test]
fn config_option_before_subcommand() {
    // --config before subcommand is accepted (fails at config load, not parse)
    cmd()
        .arg("--config")
        .arg("/tmp/nonexistent-phase2.toml")
        .arg("check-config")
        .assert()
        .failure();
}

#[test]
fn log_level_option_accepted() {
    for level in ["error", "warn", "info", "debug", "trace"] {
        cmd()
            .arg("--log-level")
            .arg(level)
            .arg("run")
            .assert()
            .failure(); // no default configuration is installed in this test
    }
}

#[test]
fn invalid_log_level_exits_nonzero() {
    cmd()
        .arg("--log-level")
        .arg("verbose")
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("verbose"));
}

// ── check-config (Phase 2 — operational) ──────────────────────────────────

fn minimal_valid_config() -> &'static str {
    r#"[general]
output_directory = "/tmp/fauna-scan-test-output"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#
}

#[test]
fn check_config_with_valid_file_exits_success() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, minimal_valid_config()).unwrap();

    // Create the output directory so filesystem validation passes
    std::fs::create_dir_all("/tmp/fauna-scan-test-output").ok();

    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("check-config")
        .assert()
        .success()
        .stdout(predicate::str::contains("Configuration valid"));
}

#[test]
fn check_config_after_subcommand_position() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, minimal_valid_config()).unwrap();
    std::fs::create_dir_all("/tmp/fauna-scan-test-output").ok();

    // --config after subcommand (global option)
    cmd()
        .arg("check-config")
        .arg("--config")
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("Configuration valid"));
}

#[test]
fn check_config_missing_default_config_fails() {
    // Ensure no default config exists by unsetting XDG_CONFIG_HOME
    // and using a non-existent HOME path
    cmd()
        .arg("check-config")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", "/nonexistent-home-for-test-12345")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot read configuration file"));
}

#[test]
fn check_config_does_not_create_directories() {
    let dir = tempfile::tempdir().unwrap();
    let output_dir = dir.path().join("new_output_dir");
    let db_dir = dir.path().join("new_db_dir");
    assert!(!output_dir.exists());
    assert!(!db_dir.exists());

    let config_content = format!(
        r#"[general]
database_path = "{db}/fauna-scan.sqlite3"
output_directory = "{output}"
log_level = "error"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db_dir.display(),
        output = output_dir.display(),
    );

    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_content).unwrap();

    // check-config validates the config and parent directories exist,
    // but does NOT create absent output or database directories.
    // The temp dir is the parent, so validation passes.
    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("check-config")
        .assert()
        .success();

    // Verify no directories were created by check-config
    assert!(
        !output_dir.exists(),
        "output directory should not have been created"
    );
    assert!(
        !db_dir.exists(),
        "database parent directory should not have been created"
    );
}

#[test]
fn check_config_rejects_lease_not_exceeding_timeout() {
    // check-config must reject a processing lease that does not exceed
    // the classifier request timeout — the same validation that scan
    // performs via ScannerOptions::from_config.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"[general]
output_directory = "/tmp/fauna-scan-test-output"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
processing_lease_seconds = 60

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test-model"
request_timeout_seconds = 120
prompt_version = "wildlife-v1"
"#,
    )
    .unwrap();
    std::fs::create_dir_all("/tmp/fauna-scan-test-output").ok();

    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("check-config")
        .assert()
        .failure()
        .stderr(predicate::str::contains("lease"));
}

#[test]
fn check_config_rejects_lease_equal_to_timeout() {
    // Lease equal to request timeout must also be rejected.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"[general]
output_directory = "/tmp/fauna-scan-test-output"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
processing_lease_seconds = 120

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test-model"
request_timeout_seconds = 120
prompt_version = "wildlife-v1"
"#,
    )
    .unwrap();
    std::fs::create_dir_all("/tmp/fauna-scan-test-output").ok();

    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("check-config")
        .assert()
        .failure()
        .stderr(predicate::str::contains("lease"));
}

#[tokio::test]
async fn download_once_honors_explicit_config_path_and_reaches_dispatch() {
    const DIGEST_CHALLENGE: &str = "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"";
    let server = MockServer::start().await;
    let discovery = r#"<StreamingChannelList xmlns="http://www.hikvision.com/ver20/XMLSchema"><StreamingChannel><id>ch1</id><trackID>101</trackID></StreamingChannel></StreamingChannelList>"#;
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(move |request: &wiremock::Request| {
            if request.headers.contains_key("authorization") {
                ResponseTemplate::new(200).set_body_string(discovery)
            } else {
                ResponseTemplate::new(401).insert_header("WWW-Authenticate", DIGEST_CHALLENGE)
            }
        })
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("selected.sqlite3");
    let output_dir = dir.path().join("images");
    let config_path = dir.path().join("selected.toml");
    let url = url::Url::parse(&server.uri()).unwrap();
    let config = format!(
        r#"[general]
database_path = "{}"
output_directory = "{}"
log_level = "error"

[nvr]
scheme = "{}"
host = "{}"
port = {}
username = "admin"
password = "correct-pass"
start_at = "{}"

[nvr.search]
window_minutes = 60
max_results = 50
poll_interval_seconds = 1
poll_overlap_seconds = 1
camera_refresh_interval_seconds = 60
settlement_delay_seconds = 1

[classifier]
"#,
        db_path.display(),
        output_dir.display(),
        url.scheme(),
        url.host_str().unwrap(),
        url.port().unwrap(),
        (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
    );
    std::fs::write(&config_path, config).unwrap();

    // No search response is mounted intentionally: discovery succeeds and
    // the command reaches operational search dispatch before failing.
    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("download")
        .arg("--once")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cannot read configuration file")
                .not()
                .and(predicate::str::contains("search")),
        );

    let database = fauna_scan::database::Database::open(&db_path)
        .await
        .unwrap();
    assert_eq!(database.ops().list_active_cameras().await.unwrap().len(), 1);
}

// ── Operational commands ──────────────────────────────────────────────────

#[test]
fn run_rejects_missing_classifier_endpoints_before_startup() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, minimal_valid_config()).unwrap();

    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("run")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no classifier endpoints are configured")
                .and(predicate::str::contains("Starting downloader and scanner pipelines").not()),
        );
}

#[test]
fn discover_no_longer_not_implemented() {
    // discover is now operational; without a valid config it fails at
    // configuration loading, not at the "not implemented" stage.
    cmd()
        .arg("discover")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", "/nonexistent-home-for-test-12345")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cannot read configuration file")
                .or(predicate::str::contains("configuration file")),
        );
}

#[test]
fn download_once_exits_nonzero_with_message() {
    // download --once is now operational; without a valid config it
    // fails at configuration loading, not at the "not implemented" stage.
    cmd()
        .arg("download")
        .arg("--once")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", "/nonexistent-home-for-test-12345")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cannot read configuration file")
                .or(predicate::str::contains("configuration file")),
        );
}

#[test]
fn scan_once_exits_nonzero_without_classifier_endpoints() {
    // scan --once with no classifier endpoints should fail before database
    // or network work begins.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        minimal_valid_config(), // no classifier endpoints
    )
    .unwrap();
    std::fs::create_dir_all("/tmp/fauna-scan-test-output").ok();

    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("scan")
        .arg("--once")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no classifier endpoints are configured",
        ));
}

#[test]
fn scan_once_without_config_exits_nonzero() {
    cmd()
        .arg("scan")
        .arg("--once")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", "/nonexistent-home-for-scan-test-12345")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot read configuration file"));
}

#[test]
fn scan_once_enabled_classifier_empty_queue_succeeds() {
    // scan --once with an enabled classifier and no eligible images
    // should exit successfully and print a scanner-pass summary.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("fauna-scan.sqlite3");
    let output_dir = dir.path().join("output");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"[general]
database_path = "{}"
output_directory = "{}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
poll_interval_seconds = 10
retry_limit = 3
retry_initial_delay_seconds = 5
retry_max_delay_seconds = 300
processing_lease_seconds = 600

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test-model"
prompt_version = "wildlife-v1"
"#,
            db_path.display(),
            output_dir.display(),
        ),
    )
    .unwrap();

    cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("scan")
        .arg("--once")
        .assert()
        .success()
        .stdout(predicate::str::contains("Scanner pass"));
}

#[tokio::test]
async fn status_empty_database_prints_all_states_and_zeroes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("status.db");
    let output_dir = dir.path().join("images");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"[general]
database_path = "{}"
output_directory = "{}"

[nvr]
scheme = "http"
host = "127.0.0.1"
port = 1
username = "u"
password = "p"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
            db_path.display(),
            output_dir.display(),
        ),
    )
    .unwrap();
    fauna_scan::database::Database::open(&db_path)
        .await
        .unwrap();

    let output = cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    for state in [
        "pending: 0",
        "downloading: 0",
        "downloaded: 0",
        "retry_wait: 0",
        "unavailable: 0",
        "failed: 0",
        "new: 0",
        "processing: 0",
        "done: 0",
        "missing: 0",
    ] {
        assert!(output.contains(state), "missing {state} in {output}");
    }
    assert!(output.find("pending: 0").unwrap() < output.find("failed: 0").unwrap());
    assert!(output.find("new: 0").unwrap() < output.find("missing: 0").unwrap());
}

#[tokio::test]
async fn status_populated_database_prints_grouped_counts_in_domain_order() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("status.db");
    let output_dir = dir.path().join("images");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"[general]
database_path = "{}"
output_directory = "{}"

[nvr]
scheme = "http"
host = "127.0.0.1"
port = 1
username = "u"
password = "p"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
            db_path.display(),
            output_dir.display(),
        ),
    )
    .unwrap();
    let database = fauna_scan::database::Database::open(&db_path)
        .await
        .unwrap();
    let timestamp = "2026-01-01T00:00:00Z";
    sqlx::query(
        "INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, first_seen_at, last_seen_at, created_at, updated_at) VALUES (1, '101', '103', ?, ?, ?, ?)",
    )
    .bind(timestamp)
    .bind(timestamp)
    .bind(timestamp)
    .bind(timestamp)
    .execute(database.pool())
    .await
    .unwrap();
    let download_states = [
        "pending",
        "downloading",
        "downloaded",
        "retry_wait",
        "unavailable",
        "failed",
    ];
    let processing_states = [
        "new",
        "processing",
        "done",
        "retry_wait",
        "failed",
        "missing",
    ];
    for (index, (download, processing)) in download_states.iter().zip(processing_states).enumerate()
    {
        sqlx::query(
            "INSERT INTO images (image_key, camera_id, track_id, capture_start_at, playback_uri, canonical_playback_uri, download_status, processing_status, discovered_at, created_at, updated_at) VALUES (?, 1, '103', ?, 'http://nvr/image', 'http://nvr/image', ?, ?, ?, ?, ?)",
        )
        .bind(format!("key-{index}"))
        .bind(timestamp)
        .bind(download)
        .bind(processing)
        .bind(timestamp)
        .bind(timestamp)
        .bind(timestamp)
        .execute(database.pool())
        .await
        .unwrap();
    }

    let output = cmd()
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    for state in download_states.iter().chain(processing_states.iter()) {
        assert!(output.contains(&format!("{state}: 1")));
    }
    let order = [
        "pending: 1",
        "downloading: 1",
        "downloaded: 1",
        "retry_wait: 1",
        "unavailable: 1",
        "failed: 1",
        "new: 1",
        "processing: 1",
        "done: 1",
        "retry_wait: 1",
        "failed: 1",
        "missing: 1",
    ];
    // The repeated state names are checked within their respective sections.
    let download_section = output.split("Processing status:").next().unwrap();
    let processing_section = output.split("Processing status:").nth(1).unwrap();
    for pair in order[..6].windows(2) {
        assert!(download_section.find(pair[0]).unwrap() < download_section.find(pair[1]).unwrap());
    }
    for pair in order[6..].windows(2) {
        assert!(
            processing_section.find(pair[0]).unwrap() < processing_section.find(pair[1]).unwrap()
        );
    }
}

// ── Invalid invocations ───────────────────────────────────────────────────

#[test]
fn unknown_command_exits_nonzero() {
    cmd()
        .arg("foobar")
        .assert()
        .failure()
        .stderr(predicate::str::contains("foobar"));
}

#[test]
fn unknown_flag_exits_nonzero() {
    cmd()
        .arg("--unknown-flag")
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown-flag"));
}

#[test]
fn missing_config_value_exits_nonzero() {
    cmd().arg("--config").arg("run").assert().failure();
}

#[test]
fn missing_subcommand_exits_nonzero() {
    cmd().assert().failure();
}
