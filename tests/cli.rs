//! Process-level integration tests for the fauna-scan CLI.
//!
//! Verifies the externally visible Phase 1/2 contract: help, version,
//! command listing, global options, argument validation, and
//! not-yet-implemented command results (except check-config which is
//! now operational in Phase 2).

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
            .failure(); // fails because run is not implemented, not because of the flag
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
enabled = false
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
enabled = false
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
enabled = true
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test-model"
request_timeout_seconds = 120
processing_lease_seconds = 60
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
enabled = true
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test-model"
request_timeout_seconds = 120
processing_lease_seconds = 120
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
enabled = false
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

// ── Other commands (still not-yet-implemented) ────────────────────────────

#[test]
fn run_exits_nonzero_with_message() {
    cmd()
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run"));
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
fn scan_once_exits_nonzero_with_disabled_classifier() {
    // scan --once with classifier.enabled=false should fail at
    // configuration validation (scanner_from_config rejects disabled).
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        minimal_valid_config(), // classifier.enabled = false
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
        .stderr(predicate::str::contains("classifier is disabled"));
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
enabled = true
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test-model"
poll_interval_seconds = 10
retry_limit = 3
retry_initial_delay_seconds = 5
retry_max_delay_seconds = 300
processing_lease_seconds = 600
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

#[test]
fn status_exits_nonzero_with_message() {
    cmd()
        .arg("status")
        .assert()
        .failure()
        .stderr(predicate::str::contains("status"));
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
