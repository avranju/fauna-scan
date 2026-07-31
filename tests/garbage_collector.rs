//! Integration tests for garbage collection of no-wildlife images.
//!
//! Uses temporary SQLite databases and filesystems with deterministic timestamps
//! to verify removal, reconciliation, safety, failure isolation, pagination,
//! and cancellation behavior.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use fauna_scan::database::repository::DatabaseOps;
use fauna_scan::database::sqlite::SqliteDataStore;
use fauna_scan::domain::*;
use fauna_scan::garbage_collector;
use fauna_scan::service_lifecycle::ShutdownToken;
use sqlx::SqlitePool;
use tempfile::TempDir;

fn now_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap())
}

fn past_ts(hours: i64) -> Timestamp {
    let base = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
    Timestamp::new(
        base.checked_sub_signed(chrono::Duration::hours(hours))
            .expect("past_ts should not underflow"),
    )
}

fn format_ts(ts: &Timestamp) -> String {
    ts.as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn insert_image(
    pool: &SqlitePool,
    image_id: i64,
    image_key: &str,
    capture_start: &Timestamp,
    local_path: &str,
    download_status: &str,
    processing_status: &str,
) {
    let now = now_ts();
    let now_str = format_ts(&now);
    let capture_str = format_ts(capture_start);

    // When download_status is 'downloaded' and processing_status is 'done',
    // also set the completion timestamps.
    let (downloaded_at_str, processing_completed_at_str) =
        if download_status == "downloaded" && processing_status == "done" {
            (Some(&now_str as &str), Some(&now_str as &str))
        } else {
            (None, None)
        };
    let file_identity = tokio::fs::canonicalize(local_path)
        .await
        .ok()
        .map(|path| path.to_string_lossy().to_string());

    sqlx::query(
        r#"INSERT OR REPLACE INTO images (
               id, image_key, camera_id, track_id, capture_start_at,
               playback_uri, canonical_playback_uri, local_path,
               local_file_identity, download_status, downloaded_at, processing_status,
               processing_completed_at, discovered_at, created_at, updated_at
           ) VALUES (?, ?, 1, '103', ?, 'http://nvr/img', 'http://nvr/img', ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(image_id)
    .bind(image_key)
    .bind(&capture_str)
    .bind(local_path)
    .bind(file_identity)
    .bind(download_status)
    .bind(downloaded_at_str)
    .bind(processing_status)
    .bind(processing_completed_at_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_classification(pool: &SqlitePool, image_id: i64, contains_wildlife: bool) {
    let now = now_ts();
    let now_str = format_ts(&now);
    let wildlife = if contains_wildlife { 1 } else { 0 };

    sqlx::query(
        r#"INSERT OR REPLACE INTO classifications (
               image_id, model, prompt_version, contains_wildlife, is_interesting,
               summary, species_json, confidence, classification_json, raw_response,
               request_started_at, request_completed_at, created_at
           ) VALUES (?, 'test-model', 'wildlife-v1', ?, 0, NULL, NULL, NULL, NULL, NULL, ?, ?, ?)"#,
    )
    .bind(image_id)
    .bind(wildlife)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(pool)
    .await
    .unwrap();
}

async fn setup_db(
    dir: &TempDir,
) -> (
    PathBuf,
    Arc<SqliteDataStore>,
    DatabaseOps,
    SqlitePool,
    PathBuf,
) {
    let db_path = dir.path().join("test.db");
    let output_dir = dir.path().join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Create a camera record
    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let now_str = format_ts(&now);

    sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id,
                                first_seen_at, last_seen_at, created_at, updated_at)
           VALUES (1, '101', '103', ?, ?, ?, ?)"#,
    )
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    (db_path, Arc::new(store), ops, pool, output_dir)
}

#[tokio::test]
async fn old_negative_image_is_collected() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24); // 5 days ago
    let file_path = output_dir.join("old.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "old-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400); // 4 days
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 1);
    assert_eq!(report.missing_files_reconciled, 0);
    assert_eq!(report.filesystem_failures, 0);
    assert!(!file_path.exists());

    // Verify local_path is cleared
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(img.local_path.is_none());
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

#[tokio::test]
async fn positive_wildlife_image_is_not_collected() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("wildlife.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "wildlife-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, true).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

#[tokio::test]
async fn any_positive_classification_prevents_collection() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("mixed.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "mixed-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    // Add both negative and positive classifications with distinct model keys
    // so they are separate rows (not overwritten by INSERT OR REPLACE).
    sqlx::query(
        r#"INSERT INTO classifications (
               image_id, model, prompt_version, contains_wildlife, is_interesting,
               summary, species_json, confidence, classification_json, raw_response,
               request_started_at, request_completed_at, created_at
           ) VALUES (?, 'test-model-a', 'wildlife-v1', 0, 0, NULL, NULL, NULL, NULL, NULL, ?, ?, ?)"#,
    )
    .bind(1)
    .bind(format_ts(&now_ts()))
    .bind(format_ts(&now_ts()))
    .bind(format_ts(&now_ts()))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO classifications (
               image_id, model, prompt_version, contains_wildlife, is_interesting,
               summary, species_json, confidence, classification_json, raw_response,
               request_started_at, request_completed_at, created_at
           ) VALUES (?, 'test-model-b', 'wildlife-v1', 1, 0, NULL, NULL, NULL, NULL, NULL, ?, ?, ?)"#,
    )
    .bind(1)
    .bind(format_ts(&now_ts()))
    .bind(format_ts(&now_ts()))
    .bind(format_ts(&now_ts()))
    .execute(&pool)
    .await
    .unwrap();

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

#[tokio::test]
async fn recent_image_is_not_collected() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let recent_capture = past_ts(2); // 2 hours ago
    let file_path = output_dir.join("recent.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "recent-img",
        &recent_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

#[tokio::test]
async fn boundary_capture_time_is_not_collected() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    // Exactly at the cutoff boundary (4 days ago) — should NOT be collected
    // because capture_start_at must be strictly less than cutoff.
    let cutoff = past_ts(4 * 24);
    let file_path = output_dir.join("boundary.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "boundary-img",
        &cutoff,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

#[tokio::test]
async fn non_done_processing_is_not_collected() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("unprocessed.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "unprocessed-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "new",
    )
    .await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

#[tokio::test]
async fn missing_file_is_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("missing.jpg");
    // Don't create the file — simulate already missing

    insert_image(
        &pool,
        1,
        "missing-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert_eq!(report.missing_files_reconciled, 1);

    // Verify local_path is cleared
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(img.local_path.is_none());
}

#[tokio::test]
async fn metadata_is_preserved_after_collection() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("metadata.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "metadata-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 1);

    // Verify all metadata is preserved
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.id, ImageId::new(1));
    assert_eq!(img.image_key, ImageKey::new("metadata-img"));
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert_eq!(img.processing_status, ProcessingStatus::Done);
    assert!(img.downloaded_at.is_some());
    assert!(img.processing_completed_at.is_some());
    assert!(img.local_path.is_none());

    // Classification should still exist
    let class = ops
        .get_classification(ImageId::new(1), "test-model", "wildlife-v1")
        .await
        .unwrap();
    assert!(!class.contains_wildlife);
}

#[tokio::test]
async fn idempotent_collection_clears_already_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("idempotent.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "idempotent-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    // First pass — should remove the file
    let report1 = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();
    assert_eq!(report1.files_removed, 1);

    // Second pass — should be idempotent (file already gone, path already cleared)
    let report2 = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();
    assert_eq!(report2.files_removed, 0);
    assert_eq!(report2.missing_files_reconciled, 0);
}

#[tokio::test]
async fn shutdown_prevents_collection() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("shutdown.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "shutdown-img",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();
    shutdown.cancel();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

#[tokio::test]
async fn collection_works_with_symlinked_output_root() {
    // Test that garbage collection works when the configured output root
    // is a symlink to a real directory.  The database stores paths under
    // the symlink alias.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let real_output = dir.path().join("real-output");
    let symlink_output = dir.path().join("symlink-output");
    std::fs::create_dir_all(&real_output).unwrap();
    std::os::unix::fs::symlink(&real_output, &symlink_output).unwrap();

    // Create a camera record
    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let now_str = format_ts(&now);

    sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id,
                                first_seen_at, last_seen_at, created_at, updated_at)
           VALUES (1, '101', '103', ?, ?, ?, ?)"#,
    )
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    let old_capture = past_ts(5 * 24);
    let file_path = real_output.join("old.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    // Store the path under the symlink alias (as the database would).
    let symlink_path = symlink_output.join("old.jpg");
    insert_image(
        &pool,
        1,
        "symlink-img",
        &old_capture,
        symlink_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let shutdown = ShutdownToken::new();

    // Pass the symlinked output root.
    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &symlink_output,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 1);
    assert_eq!(report.missing_files_reconciled, 0);
    assert!(!file_path.exists());

    // Verify local_path is cleared
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(img.local_path.is_none());
}

#[tokio::test]
async fn path_traversal_candidate_is_not_collected() {
    // Test that a candidate whose path contains `..` is rejected by
    // the filesystem safety checks and reported as a filesystem failure,
    // allowing other candidates to continue.
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);

    // Create a wildlife-positive file that path traversal would target.
    let positive_dir = output_dir.join("camera-1");
    let negative_dir = output_dir.join("negative");
    std::fs::create_dir_all(&positive_dir).unwrap();
    std::fs::create_dir_all(&negative_dir).unwrap();

    let wildlife_path = positive_dir.join("wildlife.jpg");
    std::fs::write(&wildlife_path, "wildlife-jpeg-data").unwrap();

    // Insert a candidate with a path traversal `..` in its local_path.
    // This simulates a malicious or corrupted database entry.
    let traversal_path = negative_dir.join("../camera-1/wildlife.jpg");
    insert_image(
        &pool,
        1,
        "traversal-img",
        &old_capture,
        traversal_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // The traversal candidate should be rejected as a filesystem failure.
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.filesystem_failures, 1);

    // The wildlife-positive file must still exist.
    assert!(
        wildlife_path.exists(),
        "wildlife-positive target must survive traversal rejection"
    );

    // The image row should still have its local_path intact.
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(img.local_path.is_some());
}

// ── Failure isolation ───────────────────────────────────────────────────

/// A failing candidate (path traversal) at a lower image ID does not
/// prevent a valid candidate at a higher ID from being collected.
#[tokio::test]
async fn failure_isolation_continues_after_filesystem_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);

    // Create directories for path traversal test.
    let positive_dir = output_dir.join("camera-1");
    let negative_dir = output_dir.join("negative");
    std::fs::create_dir_all(&positive_dir).unwrap();
    std::fs::create_dir_all(&negative_dir).unwrap();

    let wildlife_path = positive_dir.join("wildlife.jpg");
    std::fs::write(&wildlife_path, "wildlife-jpeg-data").unwrap();

    // Insert a failing candidate (path traversal) at ID 1.
    let traversal_path = negative_dir.join("../camera-1/wildlife.jpg");
    insert_image(
        &pool,
        1,
        "traversal-img",
        &old_capture,
        traversal_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    // Insert a valid candidate at ID 2.
    let valid_file_path = output_dir.join("valid.jpg");
    std::fs::write(&valid_file_path, "jpeg-data").unwrap();
    insert_image(
        &pool,
        2,
        "valid-img",
        &old_capture,
        valid_file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 2, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // The failing candidate is reported as a filesystem failure.
    assert_eq!(report.filesystem_failures, 1);
    // The valid candidate is collected despite the earlier failure.
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.missing_files_reconciled, 0);

    // The wildlife-positive file must still exist.
    assert!(wildlife_path.exists());
    // The valid file must be removed.
    assert!(!valid_file_path.exists());
    // The traversal candidate's local_path must be intact.
    let img1 = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(img1.local_path.is_some());
    // The valid candidate's local_path must be cleared.
    let img2 = ops.get_image(ImageId::new(2)).await.unwrap();
    assert!(img2.local_path.is_none());
}

// ── Collector pagination ────────────────────────────────────────────────

/// A sweep exceeding the internal batch size (256) processes all candidates
/// across multiple pages.
#[tokio::test]
async fn collector_pagination_exceeds_batch_size() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);

    // Insert 300 eligible images (exceeds the 256 batch size).
    for id in 1..=300 {
        let file_path = output_dir.join(format!("img-{}.jpg", id));
        std::fs::write(&file_path, "jpeg-data").unwrap();
        insert_image(
            &pool,
            id as i64,
            &format!("img-{}", id),
            &old_capture,
            file_path.to_str().unwrap(),
            "downloaded",
            "done",
        )
        .await;
        insert_classification(&pool, id as i64, false).await;
    }

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // All 300 images should be collected across multiple pages.
    assert_eq!(report.files_removed, 300);
    assert_eq!(report.missing_files_reconciled, 0);
    assert_eq!(report.filesystem_failures, 0);

    // Verify all local_paths are cleared.
    for id in 1..=300 {
        let img = ops.get_image(ImageId::new(id)).await.unwrap();
        assert!(
            img.local_path.is_none(),
            "image {} should have local_path cleared",
            id
        );
    }

    // Verify all files are removed.
    for id in 1..=300 {
        let file_path = output_dir.join(format!("img-{}.jpg", id));
        assert!(!file_path.exists(), "file {} should have been removed", id);
    }
}

// ── Deterministic cancellation during sweep ─────────────────────────────

/// Shutdown is triggered between batches, and the collector returns
/// a partial report without error.
///
/// Uses the CollectorHook to deterministically signal when the first batch
/// completes, avoiding timing-dependent polling.
#[tokio::test]
async fn cancellation_during_sweep_returns_partial_report() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);

    // Insert 300 eligible images — more than the 256-item batch size,
    // ensuring at least two batches are needed.
    for id in 1..=300 {
        let file_path = output_dir.join(format!("cancel-{}.jpg", id));
        std::fs::write(&file_path, "jpeg-data").unwrap();
        insert_image(
            &pool,
            id as i64,
            &format!("cancel-{}", id),
            &old_capture,
            file_path.to_str().unwrap(),
            "downloaded",
            "done",
        )
        .await;
        insert_classification(&pool, id as i64, false).await;
    }

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();
    let hook = garbage_collector::CollectorHook::new();

    // Clone the hook for use inside the spawned task.
    let hook_for_task = hook.clone();

    // Spawn the collector in a task.
    let ops_clone = ops.clone();
    let retention_clone = retention;
    let shutdown_clone = shutdown.clone();
    let output_dir_clone = output_dir.clone();
    let now_clone = now;

    let handle = tokio::spawn(async move {
        garbage_collector::collect_non_wildlife_images(
            &ops_clone,
            &output_dir_clone,
            retention_clone,
            &now_clone,
            &shutdown_clone,
            Some(&hook_for_task),
        )
        .await
    });

    // Wait for the hook to be notified (first batch complete), then cancel.
    hook.wait().await;
    shutdown.cancel();
    hook.release();

    // The collector should have already processed the first batch before
    // the hook was notified.  Now it will check shutdown and return.
    let report = handle.await.unwrap().unwrap();

    // The report should be partial — some collected, some not.
    assert!(
        report.files_removed < 300,
        "should be a partial report (removed {})",
        report.files_removed
    );
    assert!(
        report.files_removed >= 256,
        "should have collected the full first batch (removed {})",
        report.files_removed
    );

    // The remaining files should still exist.
    let remaining = 300 - report.files_removed;
    let remaining_count = (1..=300)
        .map(|id| output_dir.join(format!("cancel-{}.jpg", id)))
        .filter(|p| p.exists())
        .count();
    assert_eq!(
        remaining_count, remaining as usize,
        "{} files should still exist",
        remaining
    );
}

// ── Canonical path sharing tests ────────────────────────────────────────

/// A negative-image candidate whose canonical path resolves to the same
/// underlying file as a wildlife-positive image through a symlinked
/// output root must not be collected.  The wildlife file and the
/// positive image's local_path must remain intact.
#[tokio::test]
async fn canonical_path_sharing_symlink_root_preserves_wildlife() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let real_output = dir.path().join("real-output");
    let symlink_output = dir.path().join("symlink-output");
    std::fs::create_dir_all(&real_output).unwrap();
    std::os::unix::fs::symlink(&real_output, &symlink_output).unwrap();

    // Create a camera record
    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let now_str = format_ts(&now);

    sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id,
                                first_seen_at, last_seen_at, created_at, updated_at)
           VALUES (1, '101', '103', ?, ?, ?, ?)"#,
    )
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    let old_capture = past_ts(5 * 24);

    // Create a single file in the real output directory.
    let file_path = real_output.join("shared.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();
    assert!(file_path.exists());

    // Insert a wildlife-positive image that references the file via the
    // real output root.
    insert_image(
        &pool,
        1,
        "wildlife-shared",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, true).await;
    // Simulate a pre-0007 wildlife row with no cached identity. The
    // negative row below uses a distinct symlink-alias string.
    sqlx::query("UPDATE images SET local_file_identity = NULL WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();

    // Insert a no-wildlife image that references the same file via the
    // symlinked output root (different string path, same canonical file).
    let symlink_path = symlink_output.join("shared.jpg");
    insert_image(
        &pool,
        2,
        "negative-shared",
        &old_capture,
        symlink_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 2, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let shutdown = ShutdownToken::new();

    // Collect using the real output root.
    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &real_output,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // The negative candidate should be skipped because its canonical path
    // is shared with a wildlife-positive image.
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.missing_files_reconciled, 0);
    assert_eq!(report.filesystem_failures, 0);

    // The wildlife-positive file must still exist.
    assert!(
        file_path.exists(),
        "wildlife-positive file must survive canonical-path sharing check"
    );

    // Both image rows must retain their local_path.
    let img1 = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(
        img1.local_path.is_some(),
        "wildlife-positive image must retain local_path"
    );
    let img2 = ops.get_image(ImageId::new(2)).await.unwrap();
    assert!(
        img2.local_path.is_some(),
        "negative image sharing wildlife file must retain local_path"
    );
}

/// A negative-image candidate whose canonical path resolves to the same
/// underlying file as a wildlife-positive image through a symlinked
/// output root, in the reverse direction: the positive image uses the
/// symlink alias while the negative candidate uses the canonical path.
///
/// This test verifies that the collector handles both alias directions
/// — the SQL string comparison may match one direction but not the other,
/// so canonical filesystem identity comparison is required.
#[tokio::test]
async fn canonical_path_sharing_reverse_symlink_preserves_wildlife() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let real_output = dir.path().join("real-output");
    let symlink_output = dir.path().join("symlink-output");
    std::fs::create_dir_all(&real_output).unwrap();
    std::os::unix::fs::symlink(&real_output, &symlink_output).unwrap();

    // Create a camera record
    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let now_str = format_ts(&now);

    sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id,
                                first_seen_at, last_seen_at, created_at, updated_at)
           VALUES (1, '101', '103', ?, ?, ?, ?)"#,
    )
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    let old_capture = past_ts(5 * 24);

    // Create a single file in the real output directory.
    let file_path = real_output.join("shared.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();
    assert!(file_path.exists());

    // Insert a wildlife-positive image that references the file via the
    // SYMLINK alias (opposite direction from the previous test).
    let symlink_path = symlink_output.join("shared.jpg");
    insert_image(
        &pool,
        1,
        "wildlife-symlink",
        &old_capture,
        symlink_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, true).await;

    // Insert a no-wildlife image that references the same file via the
    // REAL (canonical) output root.
    let real_path = real_output.join("shared.jpg");
    insert_image(
        &pool,
        2,
        "negative-real",
        &old_capture,
        real_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 2, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let shutdown = ShutdownToken::new();

    // Collect using the real output root.
    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &real_output,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // The negative candidate should be skipped because its canonical path
    // is shared with a wildlife-positive image (even though the positive
    // image uses the symlink alias).
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.missing_files_reconciled, 0);
    assert_eq!(report.filesystem_failures, 0);

    // The wildlife-positive file must still exist.
    assert!(
        file_path.exists(),
        "wildlife-positive file must survive reverse-symlink sharing check"
    );

    // Both image rows must retain their local_path.
    let img1 = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(
        img1.local_path.is_some(),
        "wildlife-positive image (symlink alias) must retain local_path"
    );
    let img2 = ops.get_image(ImageId::new(2)).await.unwrap();
    assert!(
        img2.local_path.is_some(),
        "negative image (canonical path) sharing wildlife file must retain local_path"
    );
}

/// A negative-image candidate whose canonical path resolves to the same
/// underlying file as a wildlife-positive image through a relative vs
/// absolute path alias must not be collected.
#[tokio::test]
async fn canonical_path_sharing_relative_absolute_preserves_wildlife() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let output = dir.path().join("output");
    std::fs::create_dir_all(&output).unwrap();

    // Create a camera record
    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let now_str = format_ts(&now);

    sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id,
                                first_seen_at, last_seen_at, created_at, updated_at)
           VALUES (1, '101', '103', ?, ?, ?, ?)"#,
    )
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    let old_capture = past_ts(5 * 24);

    // Create a single file.
    let file_path = output.join("shared.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();
    assert!(file_path.exists());

    // Get the absolute path.
    let absolute_path = output.join("shared.jpg");

    // Insert a wildlife-positive image that references the file via the
    // absolute path.
    insert_image(
        &pool,
        1,
        "wildlife-abs",
        &old_capture,
        absolute_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    // Simulate a pre-0007 row: the positive image has no persisted identity,
    // so collection must reconcile it before trusting aliases.
    sqlx::query("UPDATE images SET local_file_identity = NULL WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    insert_classification(&pool, 1, true).await;

    // Insert a no-wildlife image that references the same file via a
    // relative path (relative to the parent of output).
    let parent = dir.path();
    let relative_path = parent.join("output/shared.jpg");

    // Insert with the relative path string.
    let now = now_ts();
    let now_str = format_ts(&now);
    let capture_str = format_ts(&old_capture);
    sqlx::query(
        r#"INSERT OR REPLACE INTO images (
               id, image_key, camera_id, track_id, capture_start_at,
               playback_uri, canonical_playback_uri, local_path,
               download_status, downloaded_at, processing_status,
               processing_completed_at, discovered_at, created_at, updated_at
           ) VALUES (?, ?, 1, '103', ?, 'http://nvr/img', 'http://nvr/img', ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(2_i64)
    .bind("negative-rel")
    .bind(&capture_str)
    .bind(relative_path.to_str().unwrap())
    .bind("downloaded")
    .bind(&now_str)
    .bind("done")
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT OR REPLACE INTO classifications (
               image_id, model, prompt_version, contains_wildlife, is_interesting,
               summary, species_json, confidence, classification_json, raw_response,
               request_started_at, request_completed_at, created_at
           ) VALUES (?, 'test-model', 'wildlife-v1', 0, 0, NULL, NULL, NULL, NULL, NULL, ?, ?, ?)"#,
    )
    .bind(2_i64)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    let retention = Duration::from_secs(4 * 86_400);
    let shutdown = ShutdownToken::new();

    // Collect using the absolute output path.
    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        absolute_path.parent().unwrap(),
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // The negative candidate should be skipped because its canonical path
    // (which resolves to the same file as the wildlife-positive image)
    // is shared.
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.missing_files_reconciled, 0);
    assert_eq!(report.filesystem_failures, 0);

    // The wildlife-positive file must still exist.
    assert!(
        file_path.exists(),
        "wildlife-positive file must survive canonical-path sharing check"
    );

    // Both image rows must retain their local_path.
    let img1 = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(
        img1.local_path.is_some(),
        "wildlife-positive image must retain local_path"
    );
    let img2 = ops.get_image(ImageId::new(2)).await.unwrap();
    assert!(
        img2.local_path.is_some(),
        "negative image sharing wildlife file must retain local_path"
    );
}

// ── Legacy symlink retargeting regression ─────────────────────────────────

/// A legacy positive row can retain an identity from before an output-root
/// symlink was retargeted. Reconcile the positive path before collection so
/// an aliased negative row cannot remove its current wildlife bytes.
#[tokio::test]
#[cfg(unix)]
async fn retargeted_output_alias_preserves_legacy_wildlife_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let disk1 = dir.path().join("disk1");
    let disk2 = dir.path().join("disk2");
    let output_alias = dir.path().join("output");
    std::fs::create_dir_all(&disk1).unwrap();
    std::fs::create_dir_all(&disk2).unwrap();
    std::os::unix::fs::symlink(&disk1, &output_alias).unwrap();

    let store = SqliteDataStore::connect(&db_path, 4).await.unwrap();
    let pool = store.pool().clone();
    let ops = store.ops();
    let now = now_ts();
    let now_str = format_ts(&now);
    sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id,
                                first_seen_at, last_seen_at, created_at, updated_at)
           VALUES (1, '101', '103', ?, ?, ?, ?)"#,
    )
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .bind(&now_str)
    .execute(&pool)
    .await
    .unwrap();

    let old_capture = past_ts(5 * 24);
    let old_path = disk1.join("shared.jpg");
    let wildlife_bytes = b"wildlife-bytes";
    std::fs::write(&old_path, wildlife_bytes).unwrap();
    let positive_alias_path = output_alias.join("shared.jpg");
    insert_image(
        &pool,
        1,
        "legacy-wildlife",
        &old_capture,
        positive_alias_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, true).await;

    // Reproduce the pre-0007 state: local_path is the alias while its
    // canonical identity points at the old target.
    let stale_identity = old_path.to_str().unwrap();
    sqlx::query("UPDATE images SET local_file_identity = ? WHERE id = 1")
        .bind(stale_identity)
        .execute(&pool)
        .await
        .unwrap();

    std::fs::rename(&old_path, disk2.join("shared.jpg")).unwrap();
    std::fs::remove_file(&output_alias).unwrap();
    std::os::unix::fs::symlink(&disk2, &output_alias).unwrap();

    let current_path = disk2.join("shared.jpg");
    insert_image(
        &pool,
        2,
        "aliased-negative",
        &old_capture,
        current_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 2, false).await;

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &disk2,
        Duration::from_secs(4 * 86_400),
        &now,
        &ShutdownToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(report.files_removed, 0);
    assert_eq!(std::fs::read(&current_path).unwrap(), wildlife_bytes);
    assert!(
        ops.get_image(ImageId::new(1))
            .await
            .unwrap()
            .local_path
            .is_some()
    );
    assert!(
        ops.get_image(ImageId::new(2))
            .await
            .unwrap()
            .local_path
            .is_some()
    );
}

// ── Shared wildlife file protection ────────────────────────────────────

/// Multiple negative rows sharing a file with a wildlife-positive row must
/// not cause that file to be collected.
#[tokio::test]
async fn shared_wildlife_file_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;

    let old_capture = past_ts(5 * 24);

    // Create a single file.
    let file_path = output_dir.join("shared.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();
    assert!(file_path.exists());

    // Insert a no-wildlife image (candidate) at ID 1.
    insert_image(
        &pool,
        1,
        "negative-candidate",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;

    // Insert a no-wildlife image (candidate) at ID 2.
    insert_image(
        &pool,
        2,
        "negative-candidate-2",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 2, false).await;

    let retention = Duration::from_secs(4 * 86_400);
    let now = now_ts();
    let shutdown = ShutdownToken::new();

    // Insert a wildlife-positive image at ID 3 that references the same file.
    insert_image(
        &pool,
        3,
        "wildlife-candidate",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 3, true).await;

    let report = garbage_collector::collect_non_wildlife_images(
        &ops,
        &output_dir,
        retention,
        &now,
        &shutdown,
        None,
    )
    .await
    .unwrap();

    // The negative candidates should be skipped because the wildlife-positive
    // image (ID 3) shares the same canonical path.
    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());

    // All image rows must retain their local_path.
    for id in 1..=3 {
        let img = ops.get_image(ImageId::new(id)).await.unwrap();
        assert!(
            img.local_path.is_some(),
            "image {} must retain local_path",
            id
        );
    }
}

/// A wildlife classification committed at the selection-to-unlink boundary
/// must win over collection. The collector's SQLite writer transaction keeps
/// the final check and unlink serialized with classification writes.
#[tokio::test]
async fn classification_committed_at_unlink_boundary_preserves_shared_file() {
    let dir = tempfile::tempdir().unwrap();
    let (_db_path, _store, ops, pool, output_dir) = setup_db(&dir).await;
    let old_capture = past_ts(5 * 24);
    let file_path = output_dir.join("boundary.jpg");
    std::fs::write(&file_path, "jpeg-data").unwrap();

    insert_image(
        &pool,
        1,
        "boundary-negative",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;
    insert_classification(&pool, 1, false).await;
    // This row shares the bytes but is not a candidate until it receives a
    // classification at the controlled boundary.
    insert_image(
        &pool,
        2,
        "boundary-wildlife",
        &old_capture,
        file_path.to_str().unwrap(),
        "downloaded",
        "done",
    )
    .await;

    let hook = garbage_collector::CollectorHook::new();
    hook.enable_candidate_barrier();
    let hook_for_collector = hook.clone();
    let ops_for_collector = ops.clone();
    let output_for_collector = output_dir.clone();
    let shutdown = ShutdownToken::new();
    let shutdown_for_collector = shutdown.clone();
    let task = tokio::spawn(async move {
        garbage_collector::collect_non_wildlife_images(
            &ops_for_collector,
            &output_for_collector,
            Duration::from_secs(4 * 86_400),
            &now_ts(),
            &shutdown_for_collector,
            Some(&hook_for_collector),
        )
        .await
    });

    hook.wait_for_candidate().await;
    insert_classification(&pool, 2, true).await;
    hook.release_candidate();
    // The collector will pause at the end of its short batch; release that
    // barrier as well so the task can finish.
    hook.wait().await;
    hook.release();

    let report = task.await.unwrap().unwrap();
    assert_eq!(report.files_removed, 0);
    assert!(file_path.exists());
}

// ── Directly constructed Config with zero retention ─────────────────────

/// ScannerOptions::from_config rejects a Config with zero retention days,
/// even when Config::load would have caught it.
#[test]
fn scanner_options_rejects_zero_retention() {
    let config = fauna_scan::configuration::Config {
        general: fauna_scan::configuration::GeneralConfig {
            output_directory: PathBuf::from("/tmp/output"),
            log_level: fauna_scan::cli::LogLevel::Error,
            non_wildlife_image_retention_days: 0,
        },
        database: fauna_scan::configuration::DatabaseConfig::Sqlite {
            path: PathBuf::from("/tmp/test.db"),
            max_connections: 4,
        },
        nvr: fauna_scan::configuration::NvrConfig {
            scheme: "http".to_string(),
            host: "test".to_string(),
            port: 80,
            username: "u".to_string(),
            password: Some(fauna_scan::configuration::Secret::new("x".to_string())),
            start_at: fauna_scan::domain::Timestamp::new(Utc::now()),
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
            endpoints: vec![fauna_scan::configuration::ClassifierEndpointConfig {
                enabled: true,
                base_url: url::Url::parse("http://localhost:8081/v1").unwrap(),
                endpoint: "/chat/completions".to_string(),
                model: "test".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 120,
                prompt_version: "wildlife-v1".to_string(),
                generation: fauna_scan::configuration::ClassifierGenerationConfig {
                    temperature: 0.1,
                    max_tokens: 1000,
                },
                rate_limit: None,
            }],
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 600,
        },
        web: fauna_scan::configuration::WebConfig::default(),
        source_path: PathBuf::from("/tmp/test.toml"),
    };
    let result = fauna_scan::scanner::ScannerOptions::from_config(&config);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().message.contains("greater than zero"),
        "expected zero-retention rejection"
    );
}
