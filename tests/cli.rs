//! Process-level integration tests for the fauna-scan CLI.
//!
//! Verifies the externally visible Phase 1 contract: help, version,
//! command listing, global options, argument validation, and
//! not-yet-implemented command results.

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
    cmd()
        .arg("--config")
        .arg("/tmp/nonexistent.toml")
        .arg("run")
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

// ── Valid commands exit non-zero with not-yet-implemented ─────────────────

#[test]
fn run_exits_nonzero_with_message() {
    cmd()
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run"));
}

#[test]
fn check_config_exits_nonzero_with_message() {
    cmd()
        .arg("check-config")
        .assert()
        .failure()
        .stderr(predicate::str::contains("check-config"));
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
