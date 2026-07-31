//! Shared test utilities for database-backed tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fauna_scan::cli::LogLevel;
use fauna_scan::configuration::{DatabaseConfig, GeneralConfig, Secret};
use fauna_scan::database::repository::DatabaseOps;
use fauna_scan::database::sqlite::SqliteDataStore;

/// Open a temporary SQLite database and return the path, pool, and ops.
pub async fn open_test_db(dir: &tempfile::TempDir) -> (PathBuf, Arc<SqliteDataStore>, DatabaseOps) {
    let db_path = dir.path().join("test.db");
    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let _pool = store.pool().clone();
    let ops = store.ops();
    (db_path, Arc::new(store), ops)
}

/// Build a minimal Config for tests that need DatabaseConfig.
pub fn build_test_config(
    output_dir: &Path,
    db_config: DatabaseConfig,
    nvr_host: String,
    nvr_port: u16,
) -> fauna_scan::configuration::Config {
    fauna_scan::configuration::Config {
        general: GeneralConfig {
            output_directory: output_dir.to_path_buf(),
            log_level: LogLevel::Info,
            non_wildlife_image_retention_days: 4,
        },
        database: db_config,
        nvr: fauna_scan::configuration::NvrConfig {
            scheme: "http".to_string(),
            host: nvr_host,
            port: nvr_port,
            username: "admin".to_string(),
            password: Some(Secret::new("test-password".to_string())),
            start_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            request_timeout_seconds: 30,
            connect_timeout_seconds: 10,
            allow_invalid_tls_certificates: false,
            search: fauna_scan::configuration::NvrSearchConfig {
                window_minutes: 60,
                max_results: 50,
                poll_interval_seconds: 60,
                poll_overlap_seconds: 120,
                camera_refresh_interval_seconds: 3600,
                settlement_delay_seconds: 10,
            },
            download: fauna_scan::configuration::NvrDownloadConfig {
                retry_limit: 10,
                retry_initial_delay_seconds: 5,
                retry_max_delay_seconds: 300,
                maximum_image_size_bytes: 25_000_000,
                verify_jpeg: true,
                rebase_playback_urls: true,
                concurrency: 2,
                playback_host_allowlist: vec![],
            },
        },
        classifier: fauna_scan::configuration::ClassifierConfig {
            endpoints: vec![],
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 600,
        },
        web: fauna_scan::configuration::WebConfig::default(),
        source_path: PathBuf::from("test-config.toml"),
    }
}
