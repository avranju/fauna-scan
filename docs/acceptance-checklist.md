# Phase 12 acceptance checklist

Run from the repository root. Automated evidence is intentionally named so a
release can be audited without relying on this document's prose.

| # | Section 24 criterion | Evidence / command | Result |
|---:|---|---|---|
| 1 | Stable Rust/Linux build | `cargo build` and `cargo build --release` | ☐ |
| 2 | User-level systemd service | `fauna-scan.service`; manual check below | ☐ |
| 3 | Dynamic Hikvision discovery | `tests/camera_discovery.rs` and `tests/end_to_end.rs` | ☐ |
| 4 | 101→103, 301→303 mapping | discovery unit tests; end-to-end camera assertions | ☐ |
| 5 | Search from configured start | `tests/image_search.rs`, `tests/end_to_end.rs` | ☐ |
| 6 | Pagination over 50 | `tests/image_search.rs`; end-to-end 53-image fixture | ☐ |
| 7 | UUID v4 per search request | `tests/image_search.rs`; end-to-end captured IDs | ☐ |
| 8 | Exact `searchResultPostion` tag | `tests/image_search.rs` request serialization tests | ☐ |
| 9 | Durable SQLite metadata | `tests/database.rs`, `tests/end_to_end.rs` | ☐ |
| 10 | Same timestamp remains distinct | `tests/image_search.rs`; end-to-end playback-path fixtures | ☐ |
| 11 | Atomic downloads | `tests/image_download.rs` and downloader worker tests | ☐ |
| 12 | No redownload after completion | `tests/image_download.rs`, `tests/end_to_end.rs` | ☐ |
| 13 | No redownload after local deletion | `tests/database.rs` durable-state tests | ☐ |
| 14 | Pending work resumes after restart | `tests/database.rs`, `tests/service_lifecycle.rs`, end-to-end restart | ☐ |
| 15 | Continuous polling | `tests/downloader_orchestration.rs`, end-to-end overlap/new-image pass | ☐ |
| 16 | Sequential classifier submission | `tests/scanner.rs` and classifier integration tests | ☐ |
| 17 | Structured classifier results | `tests/classifier.rs`, `tests/scanner.rs`, end-to-end | ☐ |
| 18 | Successful rows become `done` | `tests/scanner.rs`, end-to-end durable status query | ☐ |
| 19 | Temporary NVR/classifier retry | `tests/image_download.rs`, `tests/scanner.rs` | ☐ |
| 20 | Crash lease recovery | `tests/database.rs`, `tests/service_lifecycle.rs` | ☐ |
| 21 | Clean SIGTERM shutdown | `tests/service_lifecycle.rs`; manual journal check below | ☐ |
| 22 | Useful redacted logging | `tests/service_lifecycle.rs`, auth/classifier redaction tests | ☐ |
| 23 | Broad automated coverage | `cargo test --all-targets --no-fail-fast` | ☐ |

## Required release commands

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --no-fail-fast
cargo build
cargo build --release
systemd-analyze --user verify ./fauna-scan.service
```

## Live user-systemd check

On a Linux host with a usable user manager, install the release binary and
configuration, first aligning both `ReadWritePaths` entries with the actual
database and output paths:

```bash
mkdir -p ~/.config/systemd/user
cp fauna-scan.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now fauna-scan.service
systemctl --user is-active fauna-scan.service
systemctl --user status fauna-scan.service
journalctl --user -u fauna-scan.service --no-pager
systemctl --user restart fauna-scan.service
journalctl --user -u fauna-scan.service --since '1 minute ago' --no-pager
systemctl --user stop fauna-scan.service
journalctl --user -u fauna-scan.service --since '1 minute ago' --no-pager
```

Expected results: `is-active` prints `active`; startup, discovery/polling,
and scanner events appear in the journal; restart keeps the same SQLite state
and does not duplicate completed work; stop exits within 30 seconds and logs
orderly SIGTERM shutdown. If no user manager or login session exists, record
that limitation and perform this check on the target Linux host. `ProtectHome`
and `ReadWritePaths` must agree with the selected paths.
