//! Process-level integration tests for the fauna-scan CLI.
//!
//! Verifies the externally visible Phase 1/2 contract: help, version,
//! command listing, global options, argument validation, and
//! not-yet-implemented command results (except check-config which is
//! now operational in Phase 2).

use assert_cmd::Command;
use predicates::prelude::*;

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
fn discover_exits_nonzero_with_message() {
    cmd()
        .arg("discover")
        .assert()
        .failure()
        .stderr(predicate::str::contains("discover"));
}

#[test]
fn download_once_exits_nonzero_with_message() {
    cmd()
        .arg("download")
        .arg("--once")
        .assert()
        .failure()
        .stderr(predicate::str::contains("download"));
}

#[test]
fn scan_once_exits_nonzero_with_message() {
    cmd()
        .arg("scan")
        .arg("--once")
        .assert()
        .failure()
        .stderr(predicate::str::contains("scan"));
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
