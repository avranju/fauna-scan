//! Integration tests for Phase 2 configuration loading, XDG paths,
//! secret resolution, and validation.
//!
//! Uses temporary configuration trees and isolated command environments
//! to verify behavior without depending on the host environment.

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::prelude::*;
use fauna_scan::configuration::Config;
use predicates::prelude::*;
use tempfile::TempDir;

/// Build a temporary config file with the given content.
fn write_config(dir: &TempDir, content: &str) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, content).unwrap();
    path
}

/// Write a secret file with the given content.
fn write_secret_file(dir: &TempDir, name: &str, content: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// Run fauna-scan with the given args and return the Command assertion.
fn cmd_with_config(config_path: &std::path::Path) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(config_path).arg("check-config");
    cmd.assert()
}

// ── Valid configurations ──────────────────────────────────────────────────

#[test]
fn literal_secret_loads_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "literal-password"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn web_configuration_loads_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[web]
enabled = true
listen_address = "127.0.0.1:9876"
clip_pre_roll_seconds = 5
clip_post_roll_seconds = 15
maximum_clip_duration_seconds = 60

[nvr]
host = "nvr"
port = 80
username = "user"
password = "password"
start_at = "2026-01-01T00:00:00Z"
"#,
        output = output.display(),
    );
    let config = Config::load(Some(&write_config(&dir, &toml))).unwrap();
    assert!(config.web.enabled);
    assert_eq!(config.web.listen_address.to_string(), "127.0.0.1:9876");
    assert_eq!(config.web.clip_pre_roll_seconds, 5);
    assert_eq!(config.web.clip_post_roll_seconds, 15);
    assert_eq!(config.web.maximum_clip_duration_seconds, 60);
}

#[test]
fn web_default_clip_must_fit_maximum() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[web]
clip_pre_roll_seconds = 40
clip_post_roll_seconds = 30
maximum_clip_duration_seconds = 60

[nvr]
host = "nvr"
port = 80
username = "user"
password = "password"
start_at = "2026-01-01T00:00:00Z"
"#,
        output = output.display(),
    );
    let error = Config::load(Some(&write_config(&dir, &toml)))
        .err()
        .unwrap();
    assert_eq!(
        error.category,
        fauna_scan::error::ErrorCategory::Configuration
    );
    assert!(error.message.contains("clip"));
}

#[test]
fn web_clip_maximum_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[web]
maximum_clip_duration_seconds = 86401

[nvr]
host = "nvr"
port = 80
username = "user"
password = "password"
start_at = "2026-01-01T00:00:00Z"
"#,
        output = output.display(),
    );
    let error = Config::load(Some(&write_config(&dir, &toml)))
        .err()
        .unwrap();
    assert!(error.message.contains("86400"));
}

#[test]
fn file_secret_loads_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let secret_path = write_secret_file(&dir, "nvr-pass", "file-password\n");
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password_file = "{secret}"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
        secret = secret_path.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn env_secret_loads_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password_env = "FAUNA_SCAN_NVR_PASSWORD"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.env("FAUNA_SCAN_NVR_PASSWORD", "env-password");
    cmd.arg("--config").arg(&path).arg("check-config");
    cmd.assert().success();
}

#[test]
fn classifier_enabled_with_file_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let api_key_path = write_secret_file(&dir, "api-key", "file-api-key\n");
    let pass_path = write_secret_file(&dir, "cls-pass", "cls-password\n");

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "nvr-pass"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test-model"
api_key_file = "{api_key}"
username = "cls-user"
password_file = "{pass}"

[classifier.endpoints.generation]
temperature = 0.5
max_tokens = 500
"#,
        output = output.display(),
        api_key = api_key_path.display(),
        pass = pass_path.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn classifier_endpoints_are_resolved_independently() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
processing_lease_seconds = 600

[[classifier.endpoints]]
base_url = "http://classifier-one:8081/v1"
model = "primary-model"
api_key = "primary-key"
request_timeout_seconds = 120

[[classifier.endpoints]]
enabled = false
base_url = "http://classifier-two:8082/v1"
model = "secondary-model"
api_key = "secondary-key"
request_timeout_seconds = 60

[classifier.endpoints.generation]
temperature = 0.4
max_tokens = 500

[[classifier.endpoints]]
base_url = "http://classifier-three:8083/v1"
model = "tertiary-model"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    let config = Config::load(Some(&path)).unwrap();

    assert_eq!(config.classifier.endpoints.len(), 3);
    assert!(config.classifier.endpoints[0].enabled);
    let endpoint = &config.classifier.endpoints[1];
    assert!(!endpoint.enabled);
    assert_eq!(endpoint.base_url.host_str(), Some("classifier-two"));
    assert_eq!(endpoint.endpoint, "/chat/completions");
    assert_eq!(endpoint.model, "secondary-model");
    assert_eq!(endpoint.request_timeout_seconds, 60);
    assert_eq!(endpoint.prompt_version, "wildlife-v1");
    assert_eq!(endpoint.generation.temperature, 0.4);
    assert_eq!(endpoint.generation.max_tokens, 500);
    assert_eq!(endpoint.api_key.as_ref().unwrap().expose(), "secondary-key");
    assert!(
        config.classifier.endpoints[2].api_key.is_none(),
        "primary credentials must not be forwarded to another endpoint"
    );
}

#[test]
fn empty_classifier_endpoint_list_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Classification is optional when no endpoint tables are configured.
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn nvr_scheme_defaults_to_http() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // No scheme specified — should default to http
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn invalid_log_level_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"
log_level = "verbose"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with invalid log_level"
    );
    assert!(
        stderr.contains("log_level"),
        "expected log_level in stderr: {}",
        stderr
    );
    // Error must NOT echo the invalid value
    assert!(
        !stderr.contains("verbose"),
        "invalid log_level value echoed in stderr: {}",
        stderr
    );
}

// ── Secret source conflicts ───────────────────────────────────────────────

#[test]
fn nvr_password_literal_and_file_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let secret_path = write_secret_file(&dir, "pass", "x\n");
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "literal"
password_file = "{secret}"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
        secret = secret_path.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("multiple sources"));
}

#[test]
fn classifier_api_key_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "literal-key"
api_key_env = "FAUNA_SCAN_API_KEY"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("multiple sources"));
}

#[test]
fn legacy_top_level_classifier_api_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
api_key = "legacy-key"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn legacy_top_level_classifier_password_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
password = "legacy-password"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("unknown field"));
}

// ── Missing secrets ───────────────────────────────────────────────────────

#[test]
fn missing_secret_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password_file = "/nonexistent/secret-file"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).failure();
}

#[test]
fn missing_env_var_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password_env = "FAUNA_SCAN_NONEXISTENT_VAR_12345"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("FAUNA_SCAN_NONEXISTENT_VAR_12345"));
}

// ── Validation failures ───────────────────────────────────────────────────

#[test]
fn invalid_timestamp_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "not-a-timestamp"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("start_at"));
}

#[test]
fn invalid_scheme_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "ftp"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("scheme"));
}

#[test]
fn zero_port_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 0
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("port"));
}

#[test]
fn zero_max_results_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.search]
max_results = 0

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("max_results"));
}

#[test]
fn unknown_field_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"
unknown_field = true

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).failure();
}

// ── Filesystem validation (non-mutating) ──────────────────────────────────

#[test]
fn absent_output_directory_with_usable_parent_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("new_output");
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn absent_output_with_no_existing_parent_fails() {
    // Use a path under /nonexistent so there is truly no existing parent
    let dir = tempfile::tempdir().unwrap();
    let output = PathBuf::from("/nonexistent-fs-test/output");
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).failure();
}

// ── Secret leakage assertions ─────────────────────────────────────────────

#[test]
fn secret_not_in_stdout_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-PASSWORD-DO-NOT-LEAK"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("SENTINEL-PASSWORD-DO-NOT-LEAK"),
        "secret leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-PASSWORD-DO-NOT-LEAK"),
        "secret leaked in stderr: {}",
        stderr
    );
}

#[test]
fn secret_not_in_stderr_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Invalid timestamp causes failure, but the password should not appear
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-PASSWORD-DO-NOT-LEAK"
start_at = "invalid-timestamp"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("SENTINEL-PASSWORD-DO-NOT-LEAK"),
        "secret leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-PASSWORD-DO-NOT-LEAK"),
        "secret leaked in stderr: {}",
        stderr
    );
}

#[test]
fn classifier_api_key_not_in_stderr_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Missing classifier model causes failure when enabled
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "nvr-pass"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
api_key = "SENTINEL-API-KEY-DO-NOT-LEAK"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("SENTINEL-API-KEY-DO-NOT-LEAK"),
        "api key leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-API-KEY-DO-NOT-LEAK"),
        "api key leaked in stderr: {}",
        stderr
    );
}

#[test]
fn malformed_toml_with_sentinel_secret_does_not_leak() {
    // A malformed TOML line containing a sentinel secret should not
    // appear in stdout or stderr.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-MALFORMED-TOML" trailing-garbage
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("SENTINEL-MALFORMED-TOML"),
        "secret leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-MALFORMED-TOML"),
        "secret leaked in stderr: {}",
        stderr
    );
    // Should still fail with a TOML parse error
    assert!(
        stderr.contains("TOML parse error") || stderr.contains("load_config"),
        "should report a TOML parse error: {}",
        stderr
    );
}

// ── Timestamp normalization ────────────────────────────────────────────────

#[test]
fn rfc3339_offset_normalized_to_utc() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // +05:30 offset should normalize to UTC
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-07-11T05:30:00+05:30"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    // Should succeed (05:30+05:30 = 00:00 UTC)
    cmd_with_config(&path).success();
}

// ── Full example configuration ────────────────────────────────────────────

#[test]
fn full_example_configuration_loads() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let db = dir.path().join("fauna-scan.sqlite3");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"
log_level = "info"

[nvr]
scheme = "http"
host = "pigate"
port = 8080
username = "admin"
password = "test-pass"
start_at = "2026-07-11T00:00:00+05:30"
request_timeout_seconds = 30
connect_timeout_seconds = 10
allow_invalid_tls_certificates = false

[nvr.search]
window_minutes = 60
max_results = 50
poll_interval_seconds = 60
poll_overlap_seconds = 120
camera_refresh_interval_seconds = 3600
settlement_delay_seconds = 10

[nvr.download]
retry_limit = 10
retry_initial_delay_seconds = 5
retry_max_delay_seconds = 300
maximum_image_size_bytes = 25000000
verify_jpeg = true
rebase_playback_urls = true

[classifier]
poll_interval_seconds = 10
retry_limit = 5
retry_initial_delay_seconds = 10
retry_max_delay_seconds = 300
processing_lease_seconds = 600

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "vision-model"
api_key = "test-key"
username = ""
request_timeout_seconds = 120
prompt_version = "wildlife-v1"

[classifier.endpoints.generation]
temperature = 0.1
max_tokens = 1000
"#,
        db = db.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

// ── Reviewer feedback: classifier URL credentials ─────────────────────────

#[test]
fn classifier_url_with_credentials_rejected_at_process_level() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://user:SENTINEL-URL-CRED@localhost:8081/v1"
model = "test"
api_key = "k"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with credential-bearing URL"
    );
    assert!(
        !stdout.contains("SENTINEL-URL-CRED"),
        "credential leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-URL-CRED"),
        "credential leaked in stderr: {}",
        stderr
    );
}

#[test]
fn classifier_invalid_url_error_does_not_leak_raw_url_at_process_level() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "SENTINEL-INVALID-URL-VALUE"
model = "test"
api_key = "k"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail with invalid URL");
    assert!(
        !stdout.contains("SENTINEL-INVALID-URL-VALUE"),
        "raw URL leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-INVALID-URL-VALUE"),
        "raw URL leaked in stderr: {}",
        stderr
    );
}

// ── Reviewer feedback: classifier scheme and auth pairing ─────────────────

#[test]
fn classifier_enabled_ftp_scheme_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "ftp://localhost:8081/v1"
model = "test"
api_key = "k"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("http").and(predicate::str::contains("https")));
}

#[test]
fn classifier_enabled_username_without_password_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
username = "cls-user"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("username").or(predicate::str::contains("password")));
}

#[test]
fn classifier_enabled_password_without_username_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
password = "cls-pass"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("username").or(predicate::str::contains("password")));
}

#[test]
fn classifier_enabled_no_auth_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn classifier_enabled_basic_auth_pair_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
username = "cls-user"
password = "cls-pass"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn classifier_enabled_api_key_and_basic_auth_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "test-key"
username = "cls-user"
password = "cls-pass"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn classifier_enabled_https_scheme_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "https://localhost:8081/v1"
model = "test"
api_key = "k"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
fn secret_source_debug_never_exposes_literal() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-DEBUG-LEAK"
start_at = "not-a-timestamp"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("SENTINEL-DEBUG-LEAK"),
        "secret leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-DEBUG-LEAK"),
        "secret leaked in stderr: {}",
        stderr
    );
}

// ── Reviewer feedback: classifier endpoint validation ─────────────────────

#[test]
fn classifier_enabled_endpoint_with_absolute_url_rejected_at_process_level() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "http://user:SENTINEL-EP-CRED@host/path"
model = "test"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with absolute-URL endpoint"
    );
    assert!(
        !stdout.contains("SENTINEL-EP-CRED"),
        "credential leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-EP-CRED"),
        "credential leaked in stderr: {}",
        stderr
    );
}

#[test]
fn classifier_enabled_endpoint_with_at_sign_rejected_at_process_level() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/user:SENTINEL-AT-CRED@/path"
model = "test"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with credential-bearing endpoint"
    );
    assert!(
        !stdout.contains("SENTINEL-AT-CRED"),
        "credential leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-AT-CRED"),
        "credential leaked in stderr: {}",
        stderr
    );
}

#[test]
fn default_xdg_config_path_loads() {
    let dir = tempfile::tempdir().unwrap();
    let xdg_config = dir.path().join("xdg_config");
    let xdg_state = dir.path().join("xdg_state");
    let output_dir = dir.path().join("output");
    std::fs::create_dir_all(xdg_config.join("fauna-scan")).unwrap();
    std::fs::create_dir_all(&xdg_state).unwrap();
    std::fs::create_dir_all(&output_dir).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output_dir.display(),
    );
    std::fs::write(xdg_config.join("fauna-scan/config.toml"), toml).unwrap();

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.env_remove("HOME");
    cmd.env("XDG_CONFIG_HOME", &xdg_config);
    cmd.env("XDG_STATE_HOME", &xdg_state);
    cmd.arg("check-config");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Configuration valid"));
}

#[test]
fn explicit_config_path_precedence_over_xdg_default() {
    let dir = tempfile::tempdir().unwrap();
    let xdg_config = dir.path().join("xdg_config");
    let xdg_state = dir.path().join("xdg_state");
    let output_dir = dir.path().join("output");
    std::fs::create_dir_all(xdg_config.join("fauna-scan")).unwrap();
    std::fs::create_dir_all(&xdg_state).unwrap();
    std::fs::create_dir_all(&output_dir).unwrap();

    // Write an invalid default XDG config (missing password)
    let invalid_toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
    std::fs::write(xdg_config.join("fauna-scan/config.toml"), invalid_toml).unwrap();

    // Write a valid explicit config
    let valid_toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "explicit-pass"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output_dir.display(),
    );
    let explicit_path = dir.path().join("explicit-config.toml");
    std::fs::write(&explicit_path, valid_toml).unwrap();

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.env("XDG_CONFIG_HOME", &xdg_config);
    cmd.env("XDG_STATE_HOME", &xdg_state);
    cmd.arg("--config").arg(&explicit_path).arg("check-config");
    // Should succeed because --config takes precedence
    cmd.assert().success();
}

// ── Missing NVR password ──────────────────────────────────────────────────

#[test]
fn missing_nvr_password_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("nvr.password"));
}

// ── Whitespace-only required strings ──────────────────────────────────────

#[test]
fn whitespace_only_nvr_host_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "   "
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("nvr.host"));
}

#[test]
fn whitespace_only_classifier_model_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "   "
api_key = "k"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("classifier.endpoints[0].model"));
}

#[test]
fn whitespace_only_prompt_version_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "k"
prompt_version = "   "
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains(
            "classifier.endpoints[0].prompt_version",
        ));
}

// ── Invalid classifier temperature ────────────────────────────────────────

#[test]
fn classifier_temperature_negative_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "k"

[classifier.endpoints.generation]
temperature = -0.5
max_tokens = 1000
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("temperature"));
}

// ── Classifier retry-delay ordering ───────────────────────────────────────

#[test]
fn classifier_retry_initial_exceeds_max_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
retry_initial_delay_seconds = 500
retry_max_delay_seconds = 100

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "k"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("retry_initial_delay_seconds"));
}

// ── Filesystem: database path is a directory ──────────────────────────────

#[test]
fn database_path_is_directory_fails() {
    let dir = tempfile::tempdir().unwrap();
    let db_dir = dir.path().join("fauna-scan.sqlite3");
    std::fs::create_dir(&db_dir).unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

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
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("directory"));
}

// ── Filesystem: read-only database file ───────────────────────────────────

#[test]
fn read_only_database_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("fauna-scan.sqlite3");
    std::fs::write(&db_path, "").unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&db_path).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&db_path, perms).unwrap();
    }

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db_path.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    #[cfg(unix)]
    {
        cmd_with_config(&path)
            .failure()
            .stderr(predicate::str::contains("not writable"));
    }

    #[cfg(not(unix))]
    {
        // On non-Unix, read-only may not be enforced; skip.
        let _ = cmd_with_config(&path);
    }
}

// ── Filesystem: non-directory output path ─────────────────────────────────

#[test]
fn output_path_is_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("output_file");
    std::fs::write(&output_path, "not a directory").unwrap();
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output_path.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("not a directory"));
}

// ── Malformed TOML diagnostics ────────────────────────────────────────────

#[test]
fn malformed_toml_diagnostic_is_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Malformed TOML with a sentinel value that must not leak
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-DIAG-LEAK" trailing-junk
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail with malformed TOML");
    // Error should mention TOML parse error with an actionable reason.
    // Trailing junk after a value produces "expected newline" from the parser.
    assert!(
        stderr.contains("TOML parse error") || stderr.contains("load_config"),
        "expected TOML parse error in stderr: {}",
        stderr
    );
    // The diagnostic should include a useful reason beyond the generic
    // fallback. Trailing content should produce "expected newline" or
    // "unexpected trailing content" rather than just "TOML parse error".
    assert!(
        stderr.contains("expected newline")
            || stderr.contains("expected `#`")
            || stderr.contains("unexpected"),
        "diagnostic should include actionable reason, not just generic error: {}",
        stderr
    );
    // Sentinel must not leak
    assert!(
        !stderr.contains("SENTINEL-DIAG-LEAK"),
        "sentinel leaked in stderr: {}",
        stderr
    );
}

#[test]
fn unterminated_string_toml_diagnostic_is_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Unterminated string — should produce "unterminated string" reason
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-UNTERM
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with unterminated string"
    );
    // Should include an actionable reason for unterminated string
    assert!(
        stderr.contains("unterminated string")
            || stderr.contains("invalid basic string")
            || stderr.contains("TOML syntax error"),
        "expected actionable unterminated-string diagnostic: {}",
        stderr
    );
    // Sentinel must not leak
    assert!(
        !stderr.contains("SENTINEL-UNTERM"),
        "sentinel leaked in stderr: {}",
        stderr
    );
}

#[test]
fn invalid_table_header_toml_diagnostic_is_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Invalid table header (missing closing bracket)
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[invalid_section
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with invalid table header"
    );
    // Should include an actionable reason for the table header error
    assert!(
        stderr.contains("invalid table header")
            || stderr.contains("expected `]`")
            || stderr.contains("expected `.`"),
        "expected actionable table-header diagnostic: {}",
        stderr
    );
}

#[test]
fn type_invalid_toml_diagnostic_is_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Invalid type: port should be integer, not string
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = "not-a-number"
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "should fail with type-invalid TOML"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TOML parse error") || stderr.contains("load_config"),
        "expected TOML parse error in stderr: {}",
        stderr
    );
}

// ── Single-quoted TOML sentinel leak tests ────────────────────────────────

#[test]
fn single_quoted_toml_sentinel_not_leaked() {
    // Single-quoted TOML values must also be sanitized in diagnostics
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = '{output}'

[nvr]
scheme = 'http'
host = 'p'
port = 80
username = 'u'
password = 'SENTINEL-SINGLE-QUOTE-LEAK' trailing-junk
start_at = '2026-01-01T00:00:00Z'

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail with malformed TOML");
    assert!(
        !stdout.contains("SENTINEL-SINGLE-QUOTE-LEAK"),
        "single-quoted sentinel leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-SINGLE-QUOTE-LEAK"),
        "single-quoted sentinel leaked in stderr: {}",
        stderr
    );
}

#[test]
fn multiline_syntax_error_sentinel_not_leaked() {
    // A multiline TOML syntax error with a sentinel must not leak
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-MULTILINE-LEAK"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[invalid_section
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail with malformed TOML");
    assert!(
        !stdout.contains("SENTINEL-MULTILINE-LEAK"),
        "sentinel leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL-MULTILINE-LEAK"),
        "sentinel leaked in stderr: {}",
        stderr
    );
}

// ── nvr.username validation ───────────────────────────────────────────────

#[test]
fn omitted_nvr_username_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("nvr.username"));
}

#[test]
fn empty_nvr_username_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = ""
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("nvr.username"));
}

#[test]
fn whitespace_only_nvr_username_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "   "
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path)
        .failure()
        .stderr(predicate::str::contains("nvr.username"));
}

// ── Scheme-relative endpoint validation ───────────────────────────────────

#[test]
fn classifier_endpoint_scheme_relative_rejected_at_process_level() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "//evil.invalid/path"
model = "test"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with scheme-relative endpoint"
    );
    assert!(
        stderr.contains("scheme-relative") || stderr.contains("absolute path"),
        "expected scheme-relative error in stderr: {}",
        stderr
    );
}

// ── Sentinel safety in semantic validation errors ─────────────────────────

#[test]
fn invalid_start_at_sentinel_not_in_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "SENTINEL-BAD-TIMESTAMP-VALUE"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with invalid timestamp"
    );
    assert!(
        !stderr.contains("SENTINEL-BAD-TIMESTAMP-VALUE"),
        "invalid timestamp value echoed in stderr: {}",
        stderr
    );
}

#[test]
fn invalid_scheme_sentinel_not_in_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "SENTINEL-BAD-SCHEME-VALUE"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail with invalid scheme");
    assert!(
        !stderr.contains("SENTINEL-BAD-SCHEME-VALUE"),
        "invalid scheme value echoed in stderr: {}",
        stderr
    );
}

#[test]
fn invalid_log_level_sentinel_not_in_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"
log_level = "SENTINEL-BAD-LEVEL-VALUE"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with invalid log_level"
    );
    assert!(
        !stderr.contains("SENTINEL-BAD-LEVEL-VALUE"),
        "invalid log_level value echoed in stderr: {}",
        stderr
    );
}

// ── Actionable TOML diagnostics (expected type) ───────────────────────────

#[test]
fn type_invalid_toml_diagnostic_mentions_expected_type() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Invalid type: port should be integer, not string
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = "not-a-number"
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with type-invalid TOML"
    );
    // The diagnostic should mention the expected type or field, not just
    // "TOML parse error" without context.
    assert!(
        stderr.contains("expected")
            || stderr.contains("integer")
            || stderr.contains("number")
            || stderr.contains("port"),
        "diagnostic should be actionable (mention expected type or field): {}",
        stderr
    );
}

// ── Broken symlink tests ──────────────────────────────────────────────────

#[test]
#[cfg(unix)]
fn broken_database_symlink_fails() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("fauna-scan.sqlite3");
    // Create a symlink pointing to a nonexistent target
    symlink("/nonexistent-broken-db-target-12345", &db_path).unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db_path.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "broken database symlink should fail validation"
    );
}

#[test]
#[cfg(unix)]
fn broken_output_symlink_fails() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("output_link");
    // Create a symlink pointing to a nonexistent target
    symlink("/nonexistent-broken-output-target-12345", &output_path).unwrap();
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output_path.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "broken output symlink should fail validation"
    );
}

// ── Reviewer feedback: backslash endpoint host replacement ────────────────

#[test]
fn classifier_endpoint_double_backslash_host_replacement_fails() {
    // \/\/evil.invalid/path should be rejected because the url crate
    // resolves it as //evil.invalid/path, replacing the configured host.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/\\evil.invalid/path"
model = "test"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "backslash endpoint should be rejected"
    );
    assert!(
        stderr.contains("backslash"),
        "expected backslash error in stderr: {}",
        stderr
    );
}

#[test]
fn classifier_endpoint_single_backslash_fails() {
    // A single backslash in the endpoint should also be rejected.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/path\\with\\backslash"
model = "test"
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "backslash endpoint should be rejected"
    );
    assert!(
        stderr.contains("backslash"),
        "expected backslash error in stderr: {}",
        stderr
    );
}

// ── Reviewer feedback: broken symlink ancestor detection ──────────────────

#[test]
#[cfg(unix)]
fn broken_symlink_ancestor_of_database_parent_fails() {
    // When the database parent is a broken symlink, check-config should fail
    // rather than silently accepting a higher writable ancestor.
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    // Create: dir/real_dir/broken_link/child/test.db
    let real_dir = dir.path().join("real_dir");
    std::fs::create_dir(&real_dir).unwrap();
    let broken_link = real_dir.join("broken_link");
    symlink("/nonexistent-broken-db-ancestor-target-11111", &broken_link).unwrap();
    let db = broken_link.join("child").join("test.db");
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "broken symlink ancestor of database parent should fail"
    );
}

#[test]
#[cfg(unix)]
fn broken_symlink_ancestor_of_output_fails() {
    // When an ancestor of output_directory is a broken symlink, check-config
    // should fail.
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    // Create: dir/real_dir/broken_link/child/output
    let real_dir = dir.path().join("real_dir");
    std::fs::create_dir(&real_dir).unwrap();
    let broken_link = real_dir.join("broken_link");
    symlink(
        "/nonexistent-broken-output-ancestor-target-22222",
        &broken_link,
    )
    .unwrap();
    let output = broken_link.join("child").join("output");
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "broken symlink ancestor of output should fail"
    );
}

// ── Reviewer feedback: unknown field sentinel redaction ───────────────────

#[test]
fn unknown_field_sentinel_not_leaked_in_stderr() {
    // An unknown field with a sentinel name should be redacted in the
    // diagnostic, while the error should still mention "unknown field".
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
output_directory = "{output}"
SENTINEL_UNKNOWN_FIELD_NAME = true

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "unknown field should cause a TOML parse error"
    );
    // The sentinel field name must NOT appear in stderr
    assert!(
        !stderr.contains("SENTINEL_UNKNOWN_FIELD_NAME"),
        "unknown field sentinel leaked in stderr: {}",
        stderr
    );
    // The diagnostic should still be actionable (mention unknown field)
    assert!(
        stderr.contains("unknown field") || stderr.contains("TOML parse error"),
        "expected actionable unknown-field diagnostic in stderr: {}",
        stderr
    );
}

// ── Reviewer feedback: type-invalid strings with embedded delimiters ──────

#[test]
fn type_invalid_string_with_embedded_double_quote_not_leaked() {
    // A type-invalid string containing an embedded double quote should not
    // cause the TOML diagnostic sanitizer to stop at the wrong delimiter and
    // leak the suffix.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Use a literal single-quoted string containing a double quote, assigned
    // to an integer field. This forces a type error with an embedded quote.
    let toml = format!(
        r#"[general]
output_directory = '{output}'

[nvr]
scheme = 'http'
host = 'p'
port = 'SENTINEL"TAIL'
username = 'u'
password = 'x'
start_at = '2026-01-01T00:00:00Z'

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with type-invalid TOML"
    );
    assert!(
        !stdout.contains("SENTINEL"),
        "embedded-quote sentinel leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL"),
        "embedded-quote sentinel leaked in stderr: {}",
        stderr
    );
    assert!(
        !stdout.contains("TAIL"),
        "suffix after embedded quote leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("TAIL"),
        "suffix after embedded quote leaked in stderr: {}",
        stderr
    );
}

#[test]
fn type_invalid_string_with_embedded_backtick_not_leaked() {
    // A type-invalid string containing an embedded backtick should not leak.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Use a basic double-quoted string containing a backtick
    let toml = format!(
        r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = "SENTINEL`TAIL"
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with type-invalid TOML"
    );
    assert!(
        !stdout.contains("SENTINEL"),
        "embedded-backtick sentinel leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL"),
        "embedded-backtick sentinel leaked in stderr: {}",
        stderr
    );
    assert!(
        !stdout.contains("TAIL"),
        "suffix after embedded backtick leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("TAIL"),
        "suffix after embedded backtick leaked in stderr: {}",
        stderr
    );
}

#[test]
fn unknown_quoted_key_with_embedded_backtick_sentinel_not_leaked() {
    // An unknown quoted key containing a backtick: the toml crate renders
    // field names in backticks without escaping embedded backticks. This
    // means a key like "SENTINEL`KEY" produces an error message like
    // `unknown field `SENTINEL`KEY`` where the embedded backtick causes a
    // delimiter-based scanner to stop at the wrong position and leak `KEY`.
    //
    // Classification-based sanitization omits the unknown field name entirely,
    // so both the sentinel prefix AND the suffix after the embedded backtick
    // are absent from the output.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    // Use a key name that contains a backtick (valid TOML in basic strings)
    let toml = format!(
        r#"[general]
output_directory = "{output}"
"SENTINEL_EMBEDDED_BACKTICK`KEY" = true

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        output = output.display(),
    );
    let path = write_config(&dir, &toml);

    let mut cmd = Command::cargo_bin("fauna-scan").unwrap();
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.arg("--config").arg(&path).arg("check-config");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "should fail with unknown field");
    // Both the sentinel prefix AND the suffix must be absent
    assert!(
        !stdout.contains("SENTINEL_EMBEDDED_BACKTICK"),
        "sentinel prefix leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("SENTINEL_EMBEDDED_BACKTICK"),
        "sentinel prefix leaked in stderr: {}",
        stderr
    );
    assert!(
        !stdout.contains("`KEY"),
        "suffix after embedded backtick leaked in stdout: {}",
        stdout
    );
    assert!(
        !stderr.contains("`KEY"),
        "suffix after embedded backtick leaked in stderr: {}",
        stderr
    );
    // The error should still be actionable
    assert!(
        stderr.contains("unknown field") || stderr.contains("TOML parse error"),
        "expected actionable unknown-field diagnostic in stderr: {}",
        stderr
    );
}

// ── Reviewer feedback: valid symlink acceptance ───────────────────────────

#[test]
#[cfg(unix)]
fn valid_database_file_symlink_succeeds() {
    // A symlink at database_path pointing to a valid writable regular file
    // should be accepted (not rejected like broken symlinks).
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    // Create the real file
    let real_db = dir.path().join("real.db");
    std::fs::write(&real_db, "").unwrap();
    // Create a symlink to the real file
    let db_link = dir.path().join("fauna-scan.sqlite3");
    symlink(&real_db, &db_link).unwrap();
    let output = dir.path().join("output");
    std::fs::create_dir(&output).unwrap();

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db_link.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
#[cfg(unix)]
fn valid_output_directory_symlink_succeeds() {
    // A symlink at output_directory pointing to a valid writable directory
    // should be accepted.
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    // Create the real directory
    let real_output = dir.path().join("real_output");
    std::fs::create_dir(&real_output).unwrap();
    // Create a symlink to the real directory
    let output_link = dir.path().join("output_link");
    symlink(&real_output, &output_link).unwrap();
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output_link.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}

#[test]
#[cfg(unix)]
fn valid_symlink_ancestor_of_output_succeeds() {
    // When an ancestor of output_directory is a valid symlink to a writable
    // directory, check-config should succeed.
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    // Create: dir/real_dir/ -> dir/link_dir/
    let real_dir = dir.path().join("real_dir");
    std::fs::create_dir(&real_dir).unwrap();
    let link_dir = dir.path().join("link_dir");
    symlink(&real_dir, &link_dir).unwrap();
    // output is under the symlinked ancestor (does not exist yet)
    let output = link_dir.join("new_output");
    let db = dir.path().join("test.db");

    let toml = format!(
        r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
        db = db.display(),
        output = output.display(),
    );
    let path = write_config(&dir, &toml);
    cmd_with_config(&path).success();
}
