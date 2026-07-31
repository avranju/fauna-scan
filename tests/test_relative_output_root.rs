//! Subprocess test for relative-output-root handling.
//!
//! This test runs in a separate process to avoid changing the current working
//! directory during parallel test execution.  It verifies that
//! `remove_managed_image_file` correctly handles a relative output root
//! path.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use fauna_scan::database::sqlite::SqliteDataStore;
use fauna_scan::domain::Timestamp;

#[tokio::main]
async fn main() {
    // Create a temporary directory structure.
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    let sub = output.join("camera-1");
    std::fs::create_dir_all(&sub).unwrap();

    // Create a file inside the output directory.
    let file_path = sub.join("image.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();
    assert!(file_path.exists());

    // Change the current working directory to the output directory.
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&output).unwrap();

    // Use a relative path for the output root.
    let relative_output = PathBuf::from(".");

    // Call remove_managed_image_file with a relative output root.
    let result =
        fauna_scan::filesystem::remove_managed_image_file(&relative_output, &file_path).await;

    // Restore the original working directory.
    let _ = std::env::set_current_dir(&original_cwd);

    match result {
        Ok(fauna_scan::filesystem::LocalFileRemoval::Removed) => {
            println!("PASS: file removed successfully");
        }
        Ok(fauna_scan::filesystem::LocalFileRemoval::AlreadyMissing) => {
            eprintln!("FAIL: file was already missing");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("FAIL: unexpected error: {}", e);
            std::process::exit(1);
        }
    }

    // Verify the file is actually removed.
    if file_path.exists() {
        eprintln!("FAIL: file still exists after removal");
        std::process::exit(1);
    }

    // Also exercise a genuinely relative database and candidate path. This
    // target uses harness=false so changing cwd is isolated from other tests.
    std::env::set_current_dir(&dir).unwrap();
    let db_path = Path::new("relative.db");
    let store = SqliteDataStore::connect(db_path, 4).await.unwrap();
    let pool = store.pool();
    let ops = store.ops();
    let now = Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap());
    let now_str = now.to_string();
    sqlx::query(
        "INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, first_seen_at, last_seen_at, created_at, updated_at) VALUES (1, '101', '103', ?, ?, ?, ?)",
    )
    .bind(&now_str).bind(&now_str).bind(&now_str).bind(&now_str)
    .execute(pool)
    .await
    .unwrap();

    let shared = Path::new("output").join("shared.jpg");
    let absolute_shared = dir.path().join(&shared);
    std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
    std::fs::write(&shared, "jpeg-data").unwrap();
    let old = Timestamp::new(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap());
    let old_str = old.to_string();
    let identity = tokio::fs::canonicalize(&shared).await.unwrap();
    for (id, key, path) in [
        (
            1_i64,
            "positive",
            absolute_shared.to_string_lossy().to_string(),
        ),
        (2_i64, "negative", shared.to_string_lossy().to_string()),
    ] {
        sqlx::query(
            "INSERT INTO images (id, image_key, camera_id, track_id, capture_start_at, playback_uri, canonical_playback_uri, local_path, local_file_identity, download_status, downloaded_at, processing_status, processing_completed_at, discovered_at, created_at, updated_at) VALUES (?, ?, 1, '103', ?, 'http://nvr/img', 'http://nvr/img', ?, ?, 'downloaded', ?, 'done', ?, ?, ?, ?)",
        )
        .bind(id).bind(key).bind(&old_str).bind(&path)
        .bind(identity.to_string_lossy().to_string())
        .bind(&now_str).bind(&now_str).bind(&now_str).bind(&now_str).bind(&now_str)
        .execute(pool)
        .await
        .unwrap();
    }
    for (id, wildlife) in [(1_i64, 1_i64), (2_i64, 0_i64)] {
        sqlx::query(
            "INSERT INTO classifications (image_id, model, prompt_version, contains_wildlife, is_interesting, request_started_at, request_completed_at, created_at) VALUES (?, 'test', 'v1', ?, 0, ?, ?, ?)",
        )
        .bind(id).bind(wildlife).bind(&now_str).bind(&now_str).bind(&now_str)
        .execute(pool)
        .await
        .unwrap();
    }
    let shutdown = fauna_scan::service_lifecycle::ShutdownToken::new();
    let report = fauna_scan::garbage_collector::collect_non_wildlife_images(
        &ops,
        Path::new("output"),
        Duration::from_secs(86_400),
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.files_removed, 0);
    assert!(shared.exists());

    println!("PASS: relative-output-root test completed successfully");
}
