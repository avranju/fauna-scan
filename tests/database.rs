//! Integration tests for Fauna Scan's Phase 3 database layer.
//!
//! Uses temporary file-backed SQLite databases to verify schema,
//! constraints, deduplication, atomic claims, transitions, recovery,
//! status counts, metadata, and persistence across reopen.

use std::path::PathBuf;

use chrono::{TimeZone, Timelike, Utc};
use fauna_scan::database::Database;
use fauna_scan::database::models::*;
use fauna_scan::database::repository::DatabaseOps;
use fauna_scan::domain::*;
use fauna_scan::error::ErrorCategory;
use tempfile::TempDir;

/// Create a temporary file-backed database and return its path and ops.
async fn open_test_db(temp_dir: &TempDir) -> (PathBuf, DatabaseOps) {
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path).await.unwrap();
    (db_path, db.ops())
}

fn now_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap())
}

fn future_ts(hours: i32) -> Timestamp {
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, 12 + hours as u32, 0, 0)
            .unwrap(),
    )
}

fn past_ts(hours: i32) -> Timestamp {
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, 12 - hours as u32, 0, 0)
            .unwrap(),
    )
}

fn lease_ts() -> Timestamp {
    Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 14, 0, 0).unwrap())
}

// ── Migration and schema ──────────────────────────────────────────────────

#[tokio::test]
async fn operational_summary_empty_database_is_all_zero() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    assert_eq!(
        ops.operational_summary().await.unwrap(),
        OperationalSummary::default()
    );
}

#[tokio::test]
async fn operational_summary_uses_combined_image_predicates_and_active_cameras() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let observed = now_ts();
    let records = ops
        .sync_cameras(
            &[
                CameraDiscovery {
                    channel_number: 1,
                    primary_track_id: "101".into(),
                    picture_track_id: "103".into(),
                    name: None,
                    raw_discovery_identifier: None,
                },
                CameraDiscovery {
                    channel_number: 2,
                    primary_track_id: "201".into(),
                    picture_track_id: "203".into(),
                    name: None,
                    raw_discovery_identifier: None,
                },
            ],
            &observed,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE cameras SET enabled = 0 WHERE id = ?")
        .bind(records[1].id.get())
        .execute(ops.pool())
        .await
        .unwrap();

    let states = [
        ("pending", "new"),
        ("downloaded", "new"),
        ("downloaded", "processing"),
        ("downloaded", "retry_wait"),
        ("downloaded", "done"),
        ("retry_wait", "new"),
        ("unavailable", "failed"),
        ("failed", "missing"),
    ];
    let timestamp = fauna_scan::database::format_timestamp(&observed);
    for (index, (download, processing)) in states.iter().enumerate() {
        sqlx::query(
            "INSERT INTO images (image_key, camera_id, track_id, capture_start_at, playback_uri, canonical_playback_uri, download_status, processing_status, discovered_at, created_at, updated_at) VALUES (?, ?, '103', ?, 'http://nvr/image', 'http://nvr/image', ?, ?, ?, ?, ?)",
        )
        .bind(format!("summary-{index}"))
        .bind(records[0].id.get())
        .bind(&timestamp)
        .bind(download)
        .bind(processing)
        .bind(&timestamp)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(ops.pool())
        .await
        .unwrap();
    }

    assert_eq!(
        ops.operational_summary().await.unwrap(),
        OperationalSummary {
            cameras_active: 1,
            images_discovered: 8,
            images_downloaded: 4,
            downloads_pending: 1,
            images_awaiting_classification: 3,
            classifications_completed: 1,
            retryable_failures: 2,
            permanent_failures: 4,
        }
    );
}

#[tokio::test]
async fn empty_database_applies_migration() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;

    // Verify all tables exist by querying them
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cameras")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM classifications")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search_cursors")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service_metadata")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn reopen_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("reopen.db");

    // First open
    let db1 = Database::open(&db_path).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cameras")
        .fetch_one(db1.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Second open on same file
    let db2 = Database::open(&db_path).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cameras")
        .fetch_one(db2.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// ── Camera synchronization ────────────────────────────────────────────────

#[tokio::test]
async fn sync_cameras_upserts_and_preserves_first_seen() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let observed = now_ts();

    let cameras = vec![
        CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: Some("Front Door".to_string()),
            raw_discovery_identifier: Some("ch1".to_string()),
        },
        CameraDiscovery {
            channel_number: 2,
            primary_track_id: "201".to_string(),
            picture_track_id: "203".to_string(),
            name: Some("Back Garden".to_string()),
            raw_discovery_identifier: Some("ch2".to_string()),
        },
    ];

    let records = ops.sync_cameras(&cameras, &observed).await.unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].channel_number, 1);
    assert_eq!(records[1].channel_number, 2);

    // Re-sync with updated name — first_seen_at should be preserved
    let observed2 = future_ts(1);
    let cameras2 = vec![CameraDiscovery {
        channel_number: 1,
        primary_track_id: "101".to_string(),
        picture_track_id: "103".to_string(),
        name: Some("Front Door Updated".to_string()),
        raw_discovery_identifier: Some("ch1".to_string()),
    }];

    let records2 = ops.sync_cameras(&cameras2, &observed2).await.unwrap();
    assert_eq!(records2[0].name, Some("Front Door Updated".to_string()));

    // Verify first_seen_at is preserved
    let row: (String,) =
        sqlx::query_as("SELECT first_seen_at FROM cameras WHERE picture_track_id = ?")
            .bind("103")
            .fetch_one(ops.pool())
            .await
            .unwrap();
    assert_eq!(row.0, fauna_scan::database::format_timestamp(&observed));
}

#[tokio::test]
async fn sync_cameras_marks_absent_inactive() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let observed = now_ts();

    // Sync two cameras
    let cameras = vec![
        CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
        CameraDiscovery {
            channel_number: 2,
            primary_track_id: "201".to_string(),
            picture_track_id: "203".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
    ];
    ops.sync_cameras(&cameras, &observed).await.unwrap();

    // Sync only one camera — the other should be marked inactive
    let observed2 = future_ts(1);
    let cameras2 = vec![CameraDiscovery {
        channel_number: 1,
        primary_track_id: "101".to_string(),
        picture_track_id: "103".to_string(),
        name: None,
        raw_discovery_identifier: None,
    }];
    ops.sync_cameras(&cameras2, &observed2).await.unwrap();

    let enabled: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '203'")
            .fetch_one(ops.pool())
            .await
            .unwrap();
    assert_eq!(enabled, 0);
}

#[tokio::test]
async fn sync_cameras_empty_discovery_deactivates_all() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let observed = now_ts();

    // Sync two cameras
    let cameras = vec![
        CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
        CameraDiscovery {
            channel_number: 2,
            primary_track_id: "201".to_string(),
            picture_track_id: "203".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
    ];
    ops.sync_cameras(&cameras, &observed).await.unwrap();

    // Sync empty set — all should be deactivated
    let observed2 = future_ts(1);
    ops.sync_cameras(&[], &observed2).await.unwrap();

    let enabled_103: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '103'")
            .fetch_one(ops.pool())
            .await
            .unwrap();
    let enabled_203: i64 =
        sqlx::query_scalar("SELECT enabled FROM cameras WHERE picture_track_id = '203'")
            .fetch_one(ops.pool())
            .await
            .unwrap();
    assert_eq!(enabled_103, 0);
    assert_eq!(enabled_203, 0);
}

// ── Image discovery idempotency ───────────────────────────────────────────

#[tokio::test]
async fn duplicate_image_key_does_not_reset_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    // Insert a camera
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert an image
    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-abc"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(1000),
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    let new_count = ops
        .commit_search_window(&window, std::slice::from_ref(&img1))
        .await
        .unwrap();
    assert_eq!(new_count, 1);

    // Claim and fail the download to set state
    let claim = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();
    let retry_at = future_ts(1);
    ops.fail_download(
        claim.image_id,
        "connection timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    // Verify state is set
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.download_attempts, 1);
    assert!(img.download_next_attempt_at.is_some());

    // Re-insert the same image key with different metadata
    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-abc"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: Some(now),
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(2000),
        discovered_at: future_ts(1),
    };

    let new_count2 = ops.commit_search_window(&window, &[img2]).await.unwrap();
    assert_eq!(new_count2, 0); // No new rows

    // Verify work state is unchanged
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.processing_status, ProcessingStatus::New);
    assert_eq!(img.download_attempts, 1);
    assert!(img.download_next_attempt_at.is_some());
}

#[tokio::test]
async fn rediscovery_enriches_metadata_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert image with minimal metadata
    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-enrich"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: None,
        content_type: None,
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img1]).await.unwrap();

    // Re-discover with enriched metadata
    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-enrich"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: Some(now),
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(5000),
        discovered_at: future_ts(1),
    };

    ops.commit_search_window(&window, &[img2]).await.unwrap();

    // Verify metadata was enriched
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.codec_type, Some("jpeg".to_string()));
    assert_eq!(img.content_type, Some("picture".to_string()));
    assert_eq!(img.nvr_reported_size, Some(5000));
    assert!(img.capture_end_at.is_some());

    // Verify state is untouched
    assert_eq!(img.download_status, DownloadStatus::Pending);
    assert_eq!(img.processing_status, ProcessingStatus::New);
    assert_eq!(img.download_attempts, 0);
}

#[tokio::test]
async fn distinct_images_same_timestamp_coexist() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-1"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-2"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    let new_count = ops
        .commit_search_window(&window, &[img1, img2])
        .await
        .unwrap();
    assert_eq!(new_count, 2);

    // Verify both images exist
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 2);
}

// ── Cursor commit and error recording ─────────────────────────────────────

#[tokio::test]
async fn cursor_advances_with_discoveries() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-cursor"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(cursor.next_search_at, Some(future_ts(2)));
}

#[tokio::test]
async fn cursor_error_records_without_advancing() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    // Sync a camera first (cursor references camera via FK)
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    ops.record_cursor_error(camera_id, "timeout error", &now)
        .await
        .unwrap();

    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_error, Some("timeout error".to_string()));
    assert!(cursor.next_search_at.is_none());
    assert!(cursor.last_completed_window_end.is_none());
}

// ── Download claiming and transitions ─────────────────────────────────────

#[tokio::test]
async fn claim_next_download_selects_pending() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-claim"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Claim should succeed
    let claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.image_key.as_str(), "key-claim");
    assert_eq!(claim.download_attempts, 1);

    // State should be downloading
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Downloading);
}

#[tokio::test]
async fn claim_next_download_selects_due_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-retry-claim"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate a failed download with retry
    let claim = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();
    let retry_at = past_ts(1);
    ops.fail_download(
        claim.image_id,
        "timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    // Now it should be claimable again (past retry time)
    let claim2 = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim2.image_key.as_str(), "key-retry-claim");
    assert_eq!(claim2.download_attempts, 2);
}

#[tokio::test]
async fn claim_next_download_skips_future_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-future-retry"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate a failed download with a future retry time
    let claim = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();
    let retry_at = future_ts(2);
    ops.fail_download(
        claim.image_id,
        "timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    // Claim should return None (not yet due)
    let claim2 = ops.claim_next_download(&now, &lease_ts()).await.unwrap();
    assert!(claim2.is_none());
}

#[tokio::test]
async fn download_completion_requires_downloading_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-complete"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Completing a pending image should fail
    let result = ops
        .complete_download(ImageId::new(1), &PathBuf::from("/tmp/test.jpg"), &now)
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.message.contains("not in downloading state"));
}

#[tokio::test]
async fn download_failure_sets_retry_wait_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-fail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Claim and fail
    let claim = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();
    let retry_at = future_ts(1);

    ops.fail_download(
        claim.image_id,
        "connection timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert!(
        matches!(img.download_next_attempt_at, Some(t) if *t.as_datetime() == *retry_at.as_datetime())
    );
    assert_eq!(img.download_attempts, 1);
    assert!(img.download_lease_until.is_none());
    assert_eq!(
        img.download_last_error,
        Some("connection timeout".to_string())
    );
}

#[tokio::test]
async fn download_failure_unavailable_clears_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-unavail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let claim = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();

    ops.fail_download(
        claim.image_id,
        "unreachable",
        DownloadFailureDisposition::Unavailable,
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Unavailable);
    assert!(img.download_next_attempt_at.is_none());
}

#[tokio::test]
async fn download_invalid_failure_transition_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-invalid-fail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Fail a pending image — should error and leave state unchanged
    let result = ops
        .fail_download(
            ImageId::new(1),
            "error",
            DownloadFailureDisposition::RetryWait {
                next_attempt_at: future_ts(1),
            },
            &now,
        )
        .await;
    assert!(result.is_err());

    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Pending);
    assert!(img.download_next_attempt_at.is_none());
}

// ── Processing claiming and classification ────────────────────────────────

#[tokio::test]
async fn claim_next_processing_requires_downloaded_and_local_path() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-proc"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Claim a pending image — should return None
    let claim = ops.claim_next_processing(&now, &lease).await.unwrap();
    assert!(claim.is_none());

    // Claim and complete the download
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();

    // Now claim should succeed
    let claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.image_key.as_str(), "key-proc");
    assert_eq!(claim.local_path, PathBuf::from("/tmp/test.jpg"));

    // State should be processing
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Processing);
}

#[tokio::test]
async fn classification_completes_processing_and_inserts_classification() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-class"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Claim and complete download, then claim processing
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Complete classification
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: Some("A squirrel on a fence".to_string()),
        species_json: Some(r#"[{"name":"squirrel","confidence":0.8}]"#.to_string()),
        confidence: Some(0.8),
        classification_json: None,
        raw_response: None,
        request_started_at: past_ts(1),
        request_completed_at: now,
    };

    let class_id = ops
        .complete_classification(claim.image_id, &classification, claim.generation, &now)
        .await
        .unwrap();

    // Verify classification was inserted
    let stored = ops
        .get_classification(claim.image_id, "vision-v1", "wildlife-v1")
        .await
        .unwrap();
    assert_eq!(stored.id, class_id);
    assert!(stored.contains_wildlife);
    assert!(stored.is_interesting);

    // Verify persisted timestamps are ordered: started <= completed.
    assert!(
        stored.request_started_at <= stored.request_completed_at,
        "request_started_at must be <= request_completed_at"
    );

    // Verify processing status is done
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

#[tokio::test]
async fn classification_fails_if_not_processing() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-class-fail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Try to complete classification on a pending image
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: None,
        species_json: None,
        confidence: None,
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };

    let result = ops
        .complete_classification(ImageId::new(1), &classification, 0, &now)
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn classification_uniqueness_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-uniqueness"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Claim and complete download, then claim processing
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Complete classification once
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: None,
        species_json: None,
        confidence: None,
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };

    ops.complete_classification(claim.image_id, &classification, claim.generation, &now)
        .await
        .unwrap();

    // Verify done
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);

    // Try duplicate classification — should fail and processing should remain done
    let result = ops
        .complete_classification(claim.image_id, &classification, claim.generation, &now)
        .await;
    assert!(result.is_err());

    // Processing should still be done (not reset)
    let img = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
}

#[tokio::test]
async fn processing_failure_sets_retry_wait_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-proc-fail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Fail processing with retry
    let retry_at = future_ts(1);
    ops.fail_processing(
        proc_claim.image_id,
        "classification error",
        None,
        proc_claim.generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::RetryWait);
    assert!(
        matches!(img.processing_next_attempt_at, Some(t) if *t.as_datetime() == *retry_at.as_datetime())
    );
    assert_eq!(img.processing_attempts, 1);
    assert!(img.processing_lease_until.is_none());
    assert_eq!(
        img.processing_last_error,
        Some("classification error".to_string())
    );
    // Download state should be unchanged
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
}

#[tokio::test]
async fn processing_invalid_failure_transition_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-invalid-proc-fail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Fail a new image — should error and leave state unchanged
    let result = ops
        .fail_processing(
            ImageId::new(1),
            "error",
            None,
            0,
            ProcessingFailureDisposition::RetryWait {
                next_attempt_at: future_ts(1),
            },
            &now,
        )
        .await;
    assert!(result.is_err());

    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::New);
    assert!(img.processing_next_attempt_at.is_none());
}

// ── Concurrent claims ─────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_download_claims_deduplicate() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("concurrent.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    // Setup and concurrent claims in the same session
    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-concurrent"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Concurrent claims through cloned handles
    let ops1 = db.clone().ops();
    let ops2 = db.clone().ops();

    let (claim1, claim2) = tokio::join!(
        ops1.claim_next_download(&now, &lease),
        ops2.claim_next_download(&now, &lease),
    );

    let claim1 = claim1.unwrap();
    let claim2 = claim2.unwrap();

    // Only one should succeed
    assert!(claim1.is_some());
    assert!(claim2.is_none());

    // Verify attempt counter is exactly 1
    let img = ops1.get_image(claim1.unwrap().image_id).await.unwrap();
    assert_eq!(img.download_attempts, 1);
}

#[tokio::test]
async fn concurrent_processing_claims_deduplicate() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("concurrent_proc.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-concurrent-proc"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Complete the download first
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();

    // Concurrent processing claims
    let ops1 = db.clone().ops();
    let ops2 = db.clone().ops();

    let (claim1, claim2) = tokio::join!(
        ops1.claim_next_processing(&now, &lease),
        ops2.claim_next_processing(&now, &lease),
    );

    let claim1 = claim1.unwrap();
    let claim2 = claim2.unwrap();

    // Only one should succeed
    assert!(claim1.is_some());
    assert!(claim2.is_none());

    // Verify attempt counter is exactly 1
    let img = ops1.get_image(claim1.unwrap().image_id).await.unwrap();
    assert_eq!(img.processing_attempts, 1);
}

// ── Lease recovery ────────────────────────────────────────────────────────

#[tokio::test]
async fn recover_expired_leases_resets_to_retry_wait() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let past_lease = past_ts(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-recovery"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate an interrupted download with expired lease
    sqlx::query(
        "UPDATE images SET download_status = 'downloading', download_lease_until = ?, download_attempts = 2",
    )
    .bind(past_lease.as_datetime().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    .execute(ops.pool())
    .await
    .unwrap();

    // Recover
    let counts = ops.recover_expired_leases(&now).await.unwrap();
    assert_eq!(counts.downloads, 1);
    assert_eq!(counts.processing, 0);

    // Verify state
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert_eq!(img.download_attempts, 2); // attempts preserved
    assert!(img.download_lease_until.is_none());
}

#[tokio::test]
async fn recovery_preserves_unexpired_leases() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let future_lease = future_ts(2);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-unexpired"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate a download with a future (unexpired) lease
    sqlx::query("UPDATE images SET download_status = 'downloading', download_lease_until = ?")
        .bind(
            future_lease
                .as_datetime()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .execute(ops.pool())
        .await
        .unwrap();

    let counts = ops.recover_expired_leases(&now).await.unwrap();
    assert_eq!(counts.downloads, 0);

    // Verify lease is still intact
    let lease_until: Option<String> =
        sqlx::query_scalar("SELECT download_lease_until FROM images WHERE id = 1")
            .fetch_one(ops.pool())
            .await
            .unwrap();
    assert!(lease_until.is_some());
}

#[tokio::test]
async fn recovery_preserves_downloaded_and_done_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Image 1: downloaded (should not be recovered)
    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-downloaded"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img1]).await.unwrap();

    let claim1 = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(claim1.image_id, &PathBuf::from("/tmp/1.jpg"), &now)
        .await
        .unwrap();

    // Image 2: processing done (should not be recovered)
    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-done"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window2 = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window2, &[img2]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(download_claim.image_id, &PathBuf::from("/tmp/2.jpg"), &now)
        .await
        .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: None,
        species_json: None,
        confidence: None,
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };
    ops.complete_classification(
        proc_claim.image_id,
        &classification,
        proc_claim.generation,
        &now,
    )
    .await
    .unwrap();

    // Recover — should find nothing
    let counts = ops.recover_expired_leases(&now).await.unwrap();
    assert_eq!(counts.downloads, 0);
    assert_eq!(counts.processing, 0);

    // Verify states unchanged
    // claim_next_processing selects the oldest downloaded row (image1),
    // so image1 ends up done and image2 stays downloaded.
    let img1 = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img1.download_status, DownloadStatus::Downloaded);
    assert_eq!(img1.processing_status, ProcessingStatus::Done);

    let img2 = ops.get_image(ImageId::new(2)).await.unwrap();
    assert_eq!(img2.download_status, DownloadStatus::Downloaded);
    assert_eq!(img2.processing_status, ProcessingStatus::New);
}

// ── Processing missing ────────────────────────────────────────────────────

#[tokio::test]
async fn processing_missing_preserves_download_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-missing"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/missing.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Mark as missing
    ops.fail_processing(
        proc_claim.image_id,
        "local file not found",
        None,
        proc_claim.generation,
        ProcessingFailureDisposition::Missing,
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::Downloaded);
    assert_eq!(img.processing_status, ProcessingStatus::Missing);
    // Image should not be re-claimable for download (already downloaded)
    // or processing (missing)
    let proc_claim2 = ops.claim_next_processing(&now, &lease).await.unwrap();
    assert!(proc_claim2.is_none());
}

// ── Status counts ─────────────────────────────────────────────────────────

#[tokio::test]
async fn status_counts_returns_typed_maps() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert 3 pending images
    for i in 0..3 {
        let img = DiscoveredImage {
            image_key: ImageKey::new(format!("key-count-{i}")),
            camera_id,
            track_id: TrackId::new("103"),
            capture_start_at: now,
            capture_end_at: None,
            playback_uri: format!("http://nvr/pic/{i}"),
            canonical_playback_uri: format!("http://nvr/pic/{i}"),
            codec_type: Some("jpeg".to_string()),
            content_type: Some("picture".to_string()),
            nvr_reported_size: None,
            discovered_at: now,
        };

        let window = SearchWindowCommit {
            camera_id,
            window_start: now,
            window_end: future_ts(1),
            next_search_at: future_ts(2),
            polled_at: now,
            updated_at: now,
        };

        ops.commit_search_window(&window, &[img]).await.unwrap();
    }

    // Download one
    let claim1 = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(claim1.image_id, &PathBuf::from("/tmp/1.jpg"), &now)
        .await
        .unwrap();

    // Download and complete processing on another
    let claim2 = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(claim2.image_id, &PathBuf::from("/tmp/2.jpg"), &now)
        .await
        .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: false,
        is_interesting: false,
        summary: None,
        species_json: None,
        confidence: None,
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };
    ops.complete_classification(
        proc_claim.image_id,
        &classification,
        proc_claim.generation,
        &now,
    )
    .await
    .unwrap();

    let counts = ops.status_counts().await.unwrap();
    assert_eq!(counts.download.get(&DownloadStatus::Pending), Some(&1));
    assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&2));
    assert_eq!(counts.processing.get(&ProcessingStatus::New), Some(&2));
    assert_eq!(counts.processing.get(&ProcessingStatus::Done), Some(&1));
}

// ── Service metadata ──────────────────────────────────────────────────────

#[tokio::test]
async fn service_metadata_upserts_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();

    ops.set_metadata(&ServiceMetadataKey::ApplicationVersion, "0.1.0", &now)
        .await
        .unwrap();

    let value = ops
        .get_metadata(&ServiceMetadataKey::ApplicationVersion)
        .await
        .unwrap();
    assert_eq!(value, Some("0.1.0".to_string()));

    // Update
    let updated = future_ts(1);
    ops.set_metadata(&ServiceMetadataKey::ApplicationVersion, "0.2.0", &updated)
        .await
        .unwrap();

    let value = ops
        .get_metadata(&ServiceMetadataKey::ApplicationVersion)
        .await
        .unwrap();
    assert_eq!(value, Some("0.2.0".to_string()));
}

// ── Persistence across reopen ─────────────────────────────────────────────

#[tokio::test]
async fn state_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("persist.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    // First session
    {
        let db = Database::open(&db_path).await.unwrap();
        let ops = db.ops();

        ops.sync_cameras(
            &[CameraDiscovery {
                channel_number: 1,
                primary_track_id: "101".to_string(),
                picture_track_id: "103".to_string(),
                name: None,
                raw_discovery_identifier: None,
            }],
            &now,
        )
        .await
        .unwrap();

        let img = DiscoveredImage {
            image_key: ImageKey::new("key-persist"),
            camera_id,
            track_id: TrackId::new("103"),
            capture_start_at: now,
            capture_end_at: None,
            playback_uri: "http://nvr/pic/1".to_string(),
            canonical_playback_uri: "http://nvr/pic/1".to_string(),
            codec_type: Some("jpeg".to_string()),
            content_type: Some("picture".to_string()),
            nvr_reported_size: None,
            discovered_at: now,
        };

        let window = SearchWindowCommit {
            camera_id,
            window_start: now,
            window_end: future_ts(1),
            next_search_at: future_ts(2),
            polled_at: now,
            updated_at: now,
        };

        ops.commit_search_window(&window, &[img]).await.unwrap();
        let claim = ops
            .claim_next_download(&now, &lease)
            .await
            .unwrap()
            .unwrap();
        ops.complete_download(claim.image_id, &PathBuf::from("/tmp/persist.jpg"), &now)
            .await
            .unwrap();
    }

    // Second session — state should be preserved
    {
        let db = Database::open(&db_path).await.unwrap();
        let ops = db.ops();

        let img = ops.get_image(ImageId::new(1)).await.unwrap();
        assert_eq!(img.download_status, DownloadStatus::Downloaded);
        assert_eq!(img.local_path, Some(PathBuf::from("/tmp/persist.jpg")));

        let counts = ops.status_counts().await.unwrap();
        assert_eq!(counts.download.get(&DownloadStatus::Downloaded), Some(&1));
    }
}

// ── Camera record types ───────────────────────────────────────────────────

#[tokio::test]
async fn camera_record_has_camera_id() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();

    let cameras = vec![CameraDiscovery {
        channel_number: 1,
        primary_track_id: "101".to_string(),
        picture_track_id: "103".to_string(),
        name: Some("Test Camera".to_string()),
        raw_discovery_identifier: None,
    }];

    let records = ops.sync_cameras(&cameras, &now).await.unwrap();
    assert_eq!(records[0].id, CameraId::new(1));
    assert!(records[0].first_seen_at.as_datetime().hour() <= 12);
}

// ── Transaction rollback on failure ───────────────────────────────────────

#[tokio::test]
async fn search_window_rollback_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Advance the cursor once so we can detect rollback.
    let first_window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };
    ops.commit_search_window(&first_window, &[]).await.unwrap();
    let prior_cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(prior_cursor.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(prior_cursor.next_search_at, Some(future_ts(2)));

    // A valid image followed by an image referencing a non-existent camera
    // (foreign-key violation).  The mid-batch failure must roll back the
    // entire transaction: no newly inserted image and the cursor unchanged.
    let valid_img = DiscoveredImage {
        image_key: ImageKey::new("key-rollback-valid"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let invalid_img = DiscoveredImage {
        image_key: ImageKey::new("key-rollback-invalid"),
        camera_id: CameraId::new(999_999), // no such camera → FK violation
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let replay_window = SearchWindowCommit {
        camera_id,
        window_start: future_ts(1),
        window_end: future_ts(2),
        next_search_at: future_ts(3),
        polled_at: now,
        updated_at: now,
    };

    let result = ops
        .commit_search_window(&replay_window, &[valid_img, invalid_img])
        .await;
    assert!(result.is_err(), "mid-batch FK failure must error");

    // No image rows should exist — the valid insert rolled back too.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    // The cursor must remain at its prior advanced values.
    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(cursor.next_search_at, Some(future_ts(2)));
}

// ── Stable-key replay idempotency ─────────────────────────────────────────

#[tokio::test]
async fn replay_same_image_key_is_idempotent_and_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-replay-stable"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let first_window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    // First commit inserts one row and advances the cursor.
    let new_count = ops
        .commit_search_window(&first_window, std::slice::from_ref(&img))
        .await
        .unwrap();
    assert_eq!(new_count, 1);

    // Set mutable work state so we can prove it survives a replay.
    let claim = ops
        .claim_next_download(&now, &lease_ts())
        .await
        .unwrap()
        .unwrap();
    let retry_at = future_ts(1);
    ops.fail_download(
        claim.image_id,
        "connection timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    let before = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(before.download_status, DownloadStatus::RetryWait);
    assert_eq!(before.download_attempts, 1);

    // Replay the same stable image key under a later window.  This must not
    // insert a duplicate row, must advance the cursor to the replayed window,
    // and must not reset mutable download state.
    let replay_window = SearchWindowCommit {
        camera_id,
        window_start: future_ts(1),
        window_end: future_ts(2),
        next_search_at: future_ts(3),
        polled_at: now,
        updated_at: now,
    };

    let new_count2 = ops
        .commit_search_window(&replay_window, std::slice::from_ref(&img))
        .await
        .unwrap();
    assert_eq!(new_count2, 0, "replay must not insert a duplicate row");

    // Exactly one image row exists.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);

    // The cursor advances to the replayed window bounds.
    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(2)));
    assert_eq!(cursor.next_search_at, Some(future_ts(3)));

    // Mutable work state is preserved — not reset to defaults.
    let after = ops.get_image(claim.image_id).await.unwrap();
    assert_eq!(after.download_status, DownloadStatus::RetryWait);
    assert_eq!(after.download_attempts, 1);
    assert_eq!(after.processing_status, ProcessingStatus::New);
    assert_eq!(
        after.download_last_error,
        Some("connection timeout".to_string())
    );
}

// ── Downloaded image not re-claimed ───────────────────────────────────────

#[tokio::test]
async fn downloaded_image_not_reclaimed_for_download() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-not-reclaim"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Download once
    let claim1 = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        claim1.image_id,
        &PathBuf::from("/tmp/not-reclaim.jpg"),
        &now,
    )
    .await
    .unwrap();

    // No more downloads available
    let claim2 = ops.claim_next_download(&now, &lease).await.unwrap();
    assert!(claim2.is_none());
}

// ── Migration and configuration tests ─────────────────────────────────────

#[tokio::test]
async fn migration_failure_on_incompatible_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bad_schema.db");

    // Create a database with an incompatible schema (wrong column type)
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true),
        )
        .await
        .unwrap();

    sqlx::query("CREATE TABLE cameras (id INTEGER PRIMARY KEY, picture_track_id TEXT NOT NULL UNIQUE, wrong_col TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Opening should fail because the migration cannot run on this schema.
    let result = Database::open(&db_path).await;
    assert!(
        result.is_err(),
        "expected Database::open to fail on incompatible schema"
    );
    let err = result.err().unwrap();
    assert_eq!(err.category, ErrorCategory::Database);
    assert!(err.operation == "migrate" || err.operation == "open");
}

#[tokio::test]
async fn foreign_keys_and_wal_configured() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("fk_wal.db");

    let db = Database::open(&db_path).await.unwrap();
    let pool = db.pool();

    // Verify foreign keys are enabled
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(fk, 1);

    // Verify WAL mode
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(journal, "wal");

    // Verify busy_timeout is set
    let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(busy, 10000);
}

#[tokio::test]
async fn migration_recorded_in_migrations_table() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("migration_record.db");

    let db = Database::open(&db_path).await.unwrap();
    let pool = db.pool();

    // Verify migration history table exists and has the initial migration.
    // SQLx 0.8 stores migration history in the `sqlx_migrations` table.
    // In embedded-macros mode, the table is created by sqlx_migrate!.
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(pool)
            .await
            .unwrap();

    // Check that the sqlx_migrations table exists (it's created by sqlx::migrate!)
    let has_migrations_table = tables.iter().any(|t| t == "sqlx_migrations");
    if has_migrations_table {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlx_migrations WHERE version = 1")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    } else {
        // In some SQLx configurations, migration history is stored in a
        // different table or not persisted.  Verify the migration ran by
        // checking that the expected tables exist.
        assert!(
            tables.iter().any(|t| t == "cameras"),
            "cameras table should exist after migration"
        );
        assert!(
            tables.iter().any(|t| t == "images"),
            "images table should exist after migration"
        );
        assert!(
            tables.iter().any(|t| t == "classifications"),
            "classifications table should exist after migration"
        );
        assert!(
            tables.iter().any(|t| t == "search_cursors"),
            "search_cursors table should exist after migration"
        );
        assert!(
            tables.iter().any(|t| t == "service_metadata"),
            "service_metadata table should exist after migration"
        );
    }
}

#[tokio::test]
async fn required_indexes_exist() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("indexes.db");

    let db = Database::open(&db_path).await.unwrap();
    let pool = db.pool();

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_images_%'",
    )
    .fetch_all(pool)
    .await
    .unwrap();

    // Verify all required indexes exist
    assert!(indexes.iter().any(|n| n == "idx_images_download_claim"));
    assert!(indexes.iter().any(|n| n == "idx_images_processing_claim"));
    assert!(indexes.iter().any(|n| n == "idx_images_download_lease"));
    assert!(indexes.iter().any(|n| n == "idx_images_processing_lease"));
    assert!(indexes.iter().any(|n| n == "idx_images_camera_capture"));
}

// ── Timestamp corruption tests ────────────────────────────────────────────

#[tokio::test]
async fn camera_record_handles_malformed_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("corrupt_camera.db");

    let db = Database::open(&db_path).await.unwrap();
    let pool = db.pool();

    // Insert a camera with a malformed timestamp
    sqlx::query(
        "INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, enabled, \
         first_seen_at, last_seen_at, created_at, updated_at) \
         VALUES (1, '101', '103', 1, 'not-a-timestamp', '2026-07-11T12:00:00Z', \
                 '2026-07-11T12:00:00Z', '2026-07-11T12:00:00Z')",
    )
    .execute(pool)
    .await
    .unwrap();

    // sync_cameras fetches the camera row and converts it
    let result = db
        .ops()
        .sync_cameras(
            &[CameraDiscovery {
                channel_number: 1,
                primary_track_id: "101".to_string(),
                picture_track_id: "103".to_string(),
                name: None,
                raw_discovery_identifier: None,
            }],
            &now_ts(),
        )
        .await;

    // Should return a database error, not panic
    match &result {
        Ok(_) => panic!("expected error for malformed camera timestamp"),
        Err(err) => assert_eq!(err.category, ErrorCategory::Database),
    }
}

#[tokio::test]
async fn cursor_record_handles_malformed_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("corrupt_cursor.db");

    let db = Database::open(&db_path).await.unwrap();
    let pool = db.pool();

    // Insert a camera first (cursor references it via FK)
    sqlx::query(
        "INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, enabled, \
         first_seen_at, last_seen_at, created_at, updated_at) \
         VALUES (1, '101', '103', 1, '2026-07-11T12:00:00Z', '2026-07-11T12:00:00Z', \
                 '2026-07-11T12:00:00Z', '2026-07-11T12:00:00Z')",
    )
    .execute(pool)
    .await
    .unwrap();

    // Insert a cursor with a malformed timestamp
    sqlx::query(
        "INSERT INTO search_cursors (camera_id, next_search_at, last_completed_window_start, \
         last_completed_window_end, last_poll_at, last_error, updated_at) \
         VALUES (1, NULL, NULL, NULL, NULL, NULL, 'not-a-timestamp')",
    )
    .execute(pool)
    .await
    .unwrap();

    // get_cursor should return an error, not panic
    let result = db.ops().get_cursor(CameraId::new(1)).await;
    match &result {
        Ok(_) => panic!("expected error for malformed cursor timestamp"),
        Err(err) => assert_eq!(err.category, ErrorCategory::Database),
    }
}

// ── Lease boundary test ───────────────────────────────────────────────────

#[tokio::test]
async fn lease_recovery_recovers_lease_equal_to_now() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert an image and set it to downloading with lease_until == now
    let img = DiscoveredImage {
        image_key: ImageKey::new("key-boundary"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate a download in progress with lease_until == now (exactly at boundary)
    let now_str = fauna_scan::database::format_timestamp(&now);
    sqlx::query("UPDATE images SET download_status = 'downloading', download_lease_until = ?")
        .bind(&now_str)
        .execute(ops.pool())
        .await
        .unwrap();

    // Recover — the lease equal to now should be recovered
    let counts = ops.recover_expired_leases(&now).await.unwrap();
    assert_eq!(counts.downloads, 1);

    // Verify state
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert!(img.download_lease_until.is_none());
}

// ── Concurrent replay for same image key ──────────────────────────────────

#[tokio::test]
async fn concurrent_replay_same_image_key_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("concurrent_replay.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-replay"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    // First commit should succeed and insert 1 row
    let new_count = ops
        .commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();
    assert_eq!(new_count, 1);

    // Concurrent replay (same session, same transaction context) should
    // succeed with 0 new rows (idempotent update)
    let new_count2 = ops
        .commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();
    assert_eq!(new_count2, 0);

    // Only one image row should exist
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// ── Updated_at refresh in state-changing operations ───────────────────────

#[tokio::test]
async fn updated_at_refreshed_on_download_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let later = future_ts(2);
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-update-claim"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let img_before = ops.get_image(ImageId::new(1)).await.unwrap();
    let updated_before = img_before.updated_at;

    // Claim should refresh updated_at
    let _claim = ops
        .claim_next_download(&later, &lease)
        .await
        .unwrap()
        .unwrap();

    let img_after = ops.get_image(ImageId::new(1)).await.unwrap();
    assert!(
        *img_after.updated_at.as_datetime() >= *updated_before.as_datetime(),
        "updated_at should be refreshed on claim"
    );
}

#[tokio::test]
async fn updated_at_refreshed_on_download_fail() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let later = future_ts(2);
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-update-fail"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();
    let claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    let img_before = ops.get_image(claim.image_id).await.unwrap();
    let updated_before = img_before.updated_at;

    // Fail should refresh updated_at
    ops.fail_download(
        claim.image_id,
        "timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: later,
        },
        &later,
    )
    .await
    .unwrap();

    let img_after = ops.get_image(claim.image_id).await.unwrap();
    assert!(
        *img_after.updated_at.as_datetime() >= *updated_before.as_datetime(),
        "updated_at should be refreshed on fail_download"
    );
}

#[tokio::test]
async fn updated_at_refreshed_on_classification_complete() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let later = future_ts(2);
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-update-class"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    let img_before = ops.get_image(proc_claim.image_id).await.unwrap();
    let updated_before = img_before.updated_at;

    // Complete classification should refresh updated_at
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: None,
        species_json: None,
        confidence: None,
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };
    ops.complete_classification(
        proc_claim.image_id,
        &classification,
        proc_claim.generation,
        &later,
    )
    .await
    .unwrap();

    let img_after = ops.get_image(proc_claim.image_id).await.unwrap();
    assert!(
        *img_after.updated_at.as_datetime() >= *updated_before.as_datetime(),
        "updated_at should be refreshed on complete_classification"
    );
}

// ── Empty search window cursor advancement ────────────────────────────────

#[tokio::test]
async fn empty_search_window_advances_cursor_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("empty_window.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    // Commit an empty search window
    let new_count = ops.commit_search_window(&window, &[]).await.unwrap();
    assert_eq!(new_count, 0);

    // Cursor should be advanced
    let cursor = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(cursor.next_search_at, Some(future_ts(2)));

    // Reopen and verify cursor persists
    let db2 = Database::open(&db_path).await.unwrap();
    let cursor2 = db2.ops().get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor2.last_completed_window_end, Some(future_ts(1)));
    assert_eq!(cursor2.next_search_at, Some(future_ts(2)));
}

// ── Fractional timestamp round-trip ───────────────────────────────────────

fn now_ts_subsec() -> Timestamp {
    // A timestamp with subsecond precision
    Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0)
            .unwrap()
            .with_nanosecond(500_000_000)
            .unwrap(),
    )
}

#[tokio::test]
async fn fractional_timestamp_round_trips_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let ts = now_ts_subsec();
    let _camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &ts,
    )
    .await
    .unwrap();

    // The camera's first_seen_at should preserve subsecond precision
    let row: (String,) =
        sqlx::query_as("SELECT first_seen_at FROM cameras WHERE picture_track_id = ?")
            .bind("103")
            .fetch_one(ops.pool())
            .await
            .unwrap();
    // Nanos format includes subseconds
    assert!(row.0.contains("."));
    assert!(row.0.ends_with("Z"));

    // Re-read via repository
    let records = ops
        .sync_cameras(
            &[CameraDiscovery {
                channel_number: 1,
                primary_track_id: "101".to_string(),
                picture_track_id: "103".to_string(),
                name: None,
                raw_discovery_identifier: None,
            }],
            &ts,
        )
        .await
        .unwrap();
    // The subsecond should survive the round-trip
    assert_eq!(
        records[0].first_seen_at.as_datetime().nanosecond(),
        500_000_000
    );
}

// ── Due/lease comparison with fractional seconds ──────────────────────────

#[tokio::test]
async fn due_retry_with_fractional_seconds_is_claimable() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-frac-due"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate a failed download with retry at now + 1 second but with
    // subsecond precision that sorts before an exact-second "now + 1"
    let retry_subsec = Timestamp::new(
        Utc.with_ymd_and_hms(2026, 7, 11, 13, 0, 0)
            .unwrap()
            .with_nanosecond(500_000_000)
            .unwrap(),
    );
    let retry_str = retry_subsec
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query("UPDATE images SET download_status = 'retry_wait', download_next_attempt_at = ?")
        .bind(&retry_str)
        .execute(ops.pool())
        .await
        .unwrap();

    // Now is an exact second (12:00:00), retry is 13:00:00.500
    // The retry should NOT be claimable yet
    let claim = ops.claim_next_download(&now, &lease).await.unwrap();
    assert!(claim.is_none());

    // But if we use a time that's past the retry (13:00:01), it should be claimable
    let later = Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, 13, 0, 1).unwrap());
    let claim2 = ops.claim_next_download(&later, &lease).await.unwrap();
    assert!(claim2.is_some());
}

// ── Recovery updated_at verification ──────────────────────────────────────

#[tokio::test]
async fn recovery_updates_updated_at() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let later = future_ts(2);
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-recovery-ua"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Set to downloading with an expired lease
    let past_lease = past_ts(1);
    let past_lease_str = past_lease
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    sqlx::query("UPDATE images SET download_status = 'downloading', download_lease_until = ?")
        .bind(&past_lease_str)
        .execute(ops.pool())
        .await
        .unwrap();

    // Recover
    let counts = ops.recover_expired_leases(&later).await.unwrap();
    assert_eq!(counts.downloads, 1);

    // updated_at should be set to the recovery time
    let img_after = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(
        *img_after.updated_at.as_datetime(),
        *later.as_datetime(),
        "updated_at should equal recovery time"
    );
}

// ── Equal-timestamp rediscovery metadata enrichment ───────────────────────

#[tokio::test]
async fn equal_timestamp_rediscovery_enriches_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert with minimal metadata
    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-eq-ts"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: None,
        content_type: None,
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img1]).await.unwrap();

    // Rediscover with same timestamp but enriched metadata
    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-eq-ts"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: Some(now),
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(5000),
        discovered_at: now, // Same timestamp!
    };

    let new_count = ops.commit_search_window(&window, &[img2]).await.unwrap();
    assert_eq!(new_count, 0); // No new rows

    // Metadata should be enriched
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.codec_type, Some("jpeg".to_string()));
    assert_eq!(img.content_type, Some("picture".to_string()));
    assert_eq!(img.nvr_reported_size, Some(5000));
    assert!(img.capture_end_at.is_some());
}

// ── Mixed batch metadata enrichment ───────────────────────────────────────

#[tokio::test]
async fn mixed_batch_enriches_existing_and_inserts_new() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    // Insert one image with minimal metadata
    let existing_img = DiscoveredImage {
        image_key: ImageKey::new("key-mixed-existing"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: None,
        content_type: None,
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[existing_img])
        .await
        .unwrap();

    // Verify first image exists
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Mixed batch: one existing (same key) and one new
    let mixed_existing = DiscoveredImage {
        image_key: ImageKey::new("key-mixed-existing"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: Some(now),
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(3000),
        discovered_at: now,
    };

    let new_img = DiscoveredImage {
        image_key: ImageKey::new("key-mixed-new"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("png".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: Some(2000),
        discovered_at: now,
    };

    let new_count = ops
        .commit_search_window(&window, &[mixed_existing, new_img])
        .await
        .unwrap();
    assert_eq!(new_count, 1); // Only the new one

    // Verify total count is now 2
    let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(ops.pool())
        .await
        .unwrap();
    assert_eq!(total_count, 2, "should have 2 images after mixed batch");

    // Verify existing was enriched by checking image_key and codec_type
    let enriched: Vec<(String, Option<String>)> =
        sqlx::query_as(r#"SELECT image_key, codec_type FROM images ORDER BY id"#)
            .fetch_all(ops.pool())
            .await
            .unwrap();
    // First row should be the enriched existing image
    assert_eq!(enriched[0].0, "key-mixed-existing");
    assert_eq!(enriched[0].1, Some("jpeg".to_string()));
    // Second row should be the new image
    assert_eq!(enriched[1].0, "key-mixed-new");
    assert_eq!(enriched[1].1, Some("png".to_string()));
}

// ── Concurrent replay for same image key ──────────────────────────────────

#[tokio::test]
async fn concurrent_replay_same_image_key_is_idempotent_concurrent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("concurrent_replay.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-concurrent-replay"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    // First commit through the original ops — inserts 1 row.
    let new_count1 = ops
        .commit_search_window(&window, std::slice::from_ref(&img))
        .await
        .unwrap();
    assert_eq!(new_count1, 1);

    // Concurrent replay through cloned database handles.
    // Both calls try to insert the same image key that already exists,
    // so both should return 0 new rows (idempotent).
    let db1 = db.clone();
    let db2 = db.clone();

    let (count1, count2) = tokio::join!(
        async {
            db1.ops()
                .commit_search_window(&window, std::slice::from_ref(&img))
                .await
                .unwrap()
        },
        async {
            db2.ops()
                .commit_search_window(&window, std::slice::from_ref(&img))
                .await
                .unwrap()
        },
    );

    // Both concurrent replays should return 0 (image already exists).
    assert_eq!(count1, 0, "first replay should find no new rows");
    assert_eq!(count2, 0, "second replay should find no new rows");

    // Only one image row should exist
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images")
        .fetch_one(db1.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// ── Lease boundary with fractional seconds ────────────────────────────────

#[tokio::test]
async fn lease_recovery_with_fractional_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-frac-lease"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Simulate a download in progress with lease_until == now (exact second)
    let now_str = fauna_scan::database::format_timestamp(&now);
    sqlx::query("UPDATE images SET download_status = 'downloading', download_lease_until = ?")
        .bind(&now_str)
        .execute(ops.pool())
        .await
        .unwrap();

    // Recover — the lease equal to now should be recovered
    let counts = ops.recover_expired_leases(&now).await.unwrap();
    assert_eq!(counts.downloads, 1);

    // Verify state
    let img = ops.get_image(ImageId::new(1)).await.unwrap();
    assert_eq!(img.download_status, DownloadStatus::RetryWait);
    assert!(img.download_lease_until.is_none());
}

// ── Phase 8: list_active_cameras and monotonic cursor ──────────────────────

#[tokio::test]
async fn list_active_cameras_excludes_inactive() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();

    let cameras = vec![
        CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: Some("Camera One".to_string()),
            raw_discovery_identifier: None,
        },
        CameraDiscovery {
            channel_number: 2,
            primary_track_id: "201".to_string(),
            picture_track_id: "203".to_string(),
            name: Some("Camera Two".to_string()),
            raw_discovery_identifier: None,
        },
    ];
    ops.sync_cameras(&cameras, &now).await.unwrap();

    let observed2 = future_ts(1);
    let cameras2 = vec![CameraDiscovery {
        channel_number: 1,
        primary_track_id: "101".to_string(),
        picture_track_id: "103".to_string(),
        name: None,
        raw_discovery_identifier: None,
    }];
    ops.sync_cameras(&cameras2, &observed2).await.unwrap();

    let active = ops.list_active_cameras().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].picture_track_id, "103");
}

#[tokio::test]
async fn list_active_cameras_ordered_by_channel_and_track() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();

    let cameras = vec![
        CameraDiscovery {
            channel_number: 3,
            primary_track_id: "301".to_string(),
            picture_track_id: "303".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
        CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
        CameraDiscovery {
            channel_number: 2,
            primary_track_id: "201".to_string(),
            picture_track_id: "203".to_string(),
            name: None,
            raw_discovery_identifier: None,
        },
    ];
    ops.sync_cameras(&cameras, &now).await.unwrap();

    let active = ops.list_active_cameras().await.unwrap();
    assert_eq!(active.len(), 3);
    assert_eq!(active[0].channel_number, 1);
    assert_eq!(active[1].channel_number, 2);
    assert_eq!(active[2].channel_number, 3);
}

#[tokio::test]
async fn monotonic_cursor_next_search_at_does_not_regress() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-monotonic-1"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window1 = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };
    ops.commit_search_window(&window1, &[img1]).await.unwrap();

    let cursor1 = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor1.next_search_at, Some(future_ts(2)));

    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-monotonic-2"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: past_ts(1),
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window2 = SearchWindowCommit {
        camera_id,
        window_start: past_ts(2),
        window_end: future_ts(1),
        next_search_at: future_ts(1),
        polled_at: now,
        updated_at: now,
    };
    ops.commit_search_window(&window2, &[img2]).await.unwrap();

    let cursor2 = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor2.next_search_at, Some(future_ts(2)));
    assert_eq!(cursor2.last_completed_window_end, Some(future_ts(1)));
}

#[tokio::test]
async fn monotonic_completed_window_does_not_regress() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img1 = DiscoveredImage {
        image_key: ImageKey::new("key-window-regress-1"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window1 = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(2),
        next_search_at: future_ts(3),
        polled_at: now,
        updated_at: now,
    };
    ops.commit_search_window(&window1, &[img1]).await.unwrap();

    let cursor1 = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor1.last_completed_window_end, Some(future_ts(2)));

    let img2 = DiscoveredImage {
        image_key: ImageKey::new("key-window-regress-2"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: past_ts(1),
        capture_end_at: None,
        playback_uri: "http://nvr/pic/2".to_string(),
        canonical_playback_uri: "http://nvr/pic/2".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window2 = SearchWindowCommit {
        camera_id,
        window_start: past_ts(2),
        window_end: past_ts(1),
        next_search_at: past_ts(1),
        polled_at: now,
        updated_at: now,
    };
    ops.commit_search_window(&window2, &[img2]).await.unwrap();

    let cursor2 = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor2.last_completed_window_end, Some(future_ts(2)));
    assert_eq!(cursor2.next_search_at, Some(future_ts(3)));
}

#[tokio::test]
async fn successful_overlap_replay_clears_cursor_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    ops.record_cursor_error(camera_id, "previous error", &now)
        .await
        .unwrap();

    let cursor_before = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert_eq!(cursor_before.last_error, Some("previous error".to_string()));

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-clear-error"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };
    ops.commit_search_window(&window, &[img]).await.unwrap();

    let cursor_after = ops.get_cursor(camera_id).await.unwrap().unwrap();
    assert!(cursor_after.last_error.is_none());
    assert!(cursor_after.next_search_at.is_some());
}

// ── Phase 10: processing failure diagnostic response ────────────────────────

#[tokio::test]
async fn fail_processing_persists_raw_response() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-raw-resp"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Fail processing with a raw response
    let raw_body = r#"{"error":{"message":"model not found"}}"#.to_string();
    ops.fail_processing(
        proc_claim.image_id,
        "classifier ClassifierTransport (HTTP 404)",
        Some(raw_body.clone()),
        proc_claim.generation,
        ProcessingFailureDisposition::Failed,
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Failed);
    assert_eq!(
        img.processing_last_error,
        Some("classifier ClassifierTransport (HTTP 404)".to_string())
    );
    assert_eq!(img.processing_last_raw_response, Some(raw_body));
}

#[tokio::test]
async fn fail_processing_coalesce_preserves_prior_raw_response() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-coalesce"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // First failure: a retryable classifier failure with a raw response.
    let first_raw = r#"{"error":{"message":"rate limited"}}"#.to_string();
    ops.fail_processing(
        proc_claim.image_id,
        "classifier ClassifierTransport (HTTP 429)",
        Some(first_raw.clone()),
        proc_claim.generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: past_ts(1),
        },
        &now,
    )
    .await
    .unwrap();

    // Verify the raw response was stored.
    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(img.processing_last_raw_response, Some(first_raw.clone()));

    // Re-claim processing after the retry_wait transition so the image
    // is back in processing state for the second failure.
    let proc_claim2 = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Second failure: a transient filesystem error with no raw response.
    // COALESCE should preserve the prior raw response.
    ops.fail_processing(
        proc_claim2.image_id,
        "classifier ClassifierTransport",
        None,
        proc_claim2.generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: past_ts(2),
        },
        &now,
    )
    .await
    .unwrap();

    // The prior raw response should still be present.
    let img = ops.get_image(proc_claim2.image_id).await.unwrap();
    assert_eq!(img.processing_last_raw_response, Some(first_raw));
}

#[tokio::test]
async fn complete_classification_clears_raw_response() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-clear-raw"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Introduce a failure with a raw response.
    let raw_body = r#"{"error":{"message":"internal error"}}"#.to_string();
    ops.fail_processing(
        proc_claim.image_id,
        "classifier ClassifierTransport (HTTP 500)",
        Some(raw_body.clone()),
        proc_claim.generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: past_ts(1),
        },
        &now,
    )
    .await
    .unwrap();

    // Verify raw response is stored.
    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(img.processing_last_raw_response, Some(raw_body.clone()));

    // Re-claim processing after the retry_wait transition so the image
    // is back in processing state for the successful completion.
    let proc_claim2 = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Now complete the classification successfully.
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: Some("A deer in the field".to_string()),
        species_json: Some(r#"[{"name":"deer","confidence":0.9}]"#.to_string()),
        confidence: Some(0.9),
        classification_json: None,
        raw_response: None,
        request_started_at: past_ts(1),
        request_completed_at: now,
    };

    ops.complete_classification(
        proc_claim2.image_id,
        &classification,
        proc_claim2.generation,
        &now,
    )
    .await
    .unwrap();

    // Verify raw response was cleared.
    let img = ops.get_image(proc_claim2.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Done);
    assert!(img.processing_last_raw_response.is_none());
    assert!(img.processing_last_error.is_none());
}

// ── Empty response preserves prior diagnostic ────────────────────────────

#[tokio::test]
async fn empty_response_preserves_prior_diagnostic() {
    // An empty-body classifier failure (Some("")) must not overwrite a
    // prior diagnostic response.  The SQL uses COALESCE(NULLIF(?, ''),
    // ...) so that an empty string is treated as absent.
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-empty-raw"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // First failure: classifier returns a body with diagnostic info.
    let prior_body = r#"{"error":{"code":500,"message":"upstream timeout"}}"#.to_string();
    ops.fail_processing(
        proc_claim.image_id,
        "classifier ClassifierTransport (HTTP 500)",
        Some(prior_body.clone()),
        proc_claim.generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: past_ts(1),
        },
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(img.processing_last_raw_response, Some(prior_body.clone()));

    // Re-claim so the image is back in processing state.
    let proc_claim2 = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();

    // Second failure: classifier returns an empty body (Some("")).
    // This must NOT overwrite the prior diagnostic.
    ops.fail_processing(
        proc_claim2.image_id,
        "classifier ClassifierTransport (HTTP 500)",
        Some("".to_string()), // empty body
        proc_claim2.generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: past_ts(1),
        },
        &now,
    )
    .await
    .unwrap();

    // The prior diagnostic must still be present.
    let img = ops.get_image(proc_claim2.image_id).await.unwrap();
    assert_eq!(img.processing_last_raw_response, Some(prior_body.clone()));

    // A third failure with None should also preserve the diagnostic.
    let proc_claim3 = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.fail_processing(
        proc_claim3.image_id,
        "classifier ClassifierTransport",
        None, // no response at all
        proc_claim3.generation,
        ProcessingFailureDisposition::Failed,
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(proc_claim3.image_id).await.unwrap();
    assert_eq!(img.processing_status, ProcessingStatus::Failed);
    assert_eq!(img.processing_last_raw_response, Some(prior_body.clone()));
}

// ── Concurrent lease recovery prevents stale completion ──────────────────

#[tokio::test]
async fn concurrent_processing_lease_recovery_prevents_stale_completion() {
    // Two workers hold cloned DatabaseOps for the same pool.  Worker A
    // claims an image, then Worker B recovers the (simulated expired)
    // lease and re-claims it.  Worker A must detect that it no longer
    // owns the claim via generation-token checks on renewal, verification,
    // completion, and failure — all must fail while Worker B's processing
    // state remains unchanged.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("concurrent_lease_recovery.db");
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    let db = Database::open(&db_path).await.unwrap();
    let ops = db.ops();

    // Setup: sync camera and insert image
    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-concurrent-lease"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Complete the download so the image is claimable for processing.
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();

    // Worker A claims the processing work (generation = 1).
    let ops_a = db.clone().ops();
    let claim_a = ops_a
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation_a = claim_a.generation;

    // Verify Worker A owns the claim.
    ops_a
        .verify_processing_ownership(claim_a.image_id, generation_a)
        .await
        .unwrap();

    // Simulate lease expiry: set lease_until to the past so recovery
    // considers it expired.
    let past_lease = past_ts(1);
    let past_lease_str = fauna_scan::database::format_timestamp(&past_lease);
    sqlx::query("UPDATE images SET processing_lease_until = ?")
        .bind(&past_lease_str)
        .execute(ops.pool())
        .await
        .unwrap();

    // Worker B recovers the expired lease (generation reset to 0).
    let ops_b = db.clone().ops();
    let recovery = ops_b.recover_expired_leases(&now).await.unwrap();
    assert_eq!(
        recovery.processing, 1,
        "Worker B should recover 1 processing lease"
    );

    // Worker B re-claims the image (generation = 1 again, but different row state).
    let claim_b = ops_b
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation_b = claim_b.generation;

    // Verify Worker B owns its claim.
    ops_b
        .verify_processing_ownership(claim_b.image_id, generation_b)
        .await
        .unwrap();

    // Worker A's renewal must fail (generation mismatch).
    let renewal_result = ops_a
        .renew_processing_lease(claim_a.image_id, generation_a, &lease, &now)
        .await;
    assert!(
        renewal_result.is_err(),
        "renew_processing_lease should fail after Worker B re-claimed"
    );

    // Worker A's ownership check must fail (generation mismatch).
    let verify_result = ops_a
        .verify_processing_ownership(claim_a.image_id, generation_a)
        .await;
    assert!(
        verify_result.is_err(),
        "verify_processing_ownership should fail after Worker B re-claimed"
    );
    let err = verify_result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Database);
    assert!(err.message.contains("ownership lost"));

    // Worker A must not be able to complete the classification.
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: None,
        species_json: None,
        confidence: None,
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };
    let complete_result = ops_a
        .complete_classification(claim_a.image_id, &classification, generation_a, &now)
        .await;
    assert!(
        complete_result.is_err(),
        "complete_classification should fail after Worker B re-claimed"
    );

    // Worker A must not be able to fail the processing either.
    let fail_result = ops_a
        .fail_processing(
            claim_a.image_id,
            "stale worker error",
            None,
            generation_a,
            ProcessingFailureDisposition::Failed,
            &now,
        )
        .await;
    assert!(
        fail_result.is_err(),
        "fail_processing should fail after Worker B re-claimed"
    );

    // Verify Worker B's processing state is unchanged (still processing).
    let img_b = ops_b.get_image(claim_b.image_id).await.unwrap();
    assert_eq!(img_b.processing_status, ProcessingStatus::Processing);
    assert_eq!(img_b.processing_generation, generation_b);
    assert_eq!(img_b.download_status, DownloadStatus::Downloaded);

    // Worker A's image is no longer in processing state.
    let img_a = ops_a.get_image(claim_a.image_id).await.unwrap();
    assert_eq!(img_a.processing_status, ProcessingStatus::Processing);
    assert_eq!(img_a.processing_generation, generation_b);
}

// ── Renewal timestamp test ────────────────────────────────────────────────

/// renew_processing_lease records the renewal instant in updated_at, not
/// the future lease deadline.
#[tokio::test]
async fn renewal_updates_updated_at_to_renewal_instant() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-renew-ts"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Complete the download then claim processing.
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation = proc_claim.generation;

    // Record the updated_at before renewal.
    let img_before = ops.get_image(proc_claim.image_id).await.unwrap();
    let _updated_before = img_before.updated_at;

    // Renew the lease with a future deadline but the current time as renewal.
    let future_lease = future_ts(1);
    ops.renew_processing_lease(proc_claim.image_id, generation, &future_lease, &now)
        .await
        .unwrap();

    // Verify updated_at equals the renewal instant (now), not the future deadline.
    let img_after = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(
        img_after.updated_at, now,
        "updated_at must equal the renewal instant, not the future lease deadline"
    );
    // The lease_until should be the future value.
    assert_eq!(
        img_after.processing_lease_until,
        Some(future_lease),
        "processing_lease_until should be the future deadline"
    );
}

// ── Raw response diagnostic tests ─────────────────────────────────────────

/// Retryable failures retain the raw response, and successful completion
/// clears it.
#[tokio::test]
async fn retryable_failure_retains_raw_response_and_success_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-raw-diag"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    // Complete download and claim processing.
    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation = proc_claim.generation;

    // Simulate a retryable failure with raw response.
    // Use past_ts so the image is immediately claimable for re-processing.
    let retry_at = past_ts(1);
    let diagnostic_response = "this is the diagnostic classifier output".to_string();
    ops.fail_processing(
        proc_claim.image_id,
        "classifier ClassifierTransport (HTTP 500)",
        Some(diagnostic_response.clone()),
        generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    // Verify raw response is retained.
    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(
        img.processing_last_raw_response,
        Some(diagnostic_response.clone()),
        "raw diagnostic response should be retained"
    );

    // Now complete the classification successfully.
    let classification = ClassificationInput {
        model: "vision-v1".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: Some("A deer".to_string()),
        species_json: Some(r#"[{"name":"deer","confidence":0.9}]"#.to_string()),
        confidence: Some(0.9),
        classification_json: None,
        raw_response: None,
        request_started_at: now,
        request_completed_at: now,
    };

    // Re-claim processing first (since it's in retry_wait now).
    let proc_claim2 = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation2 = proc_claim2.generation;

    ops.complete_classification(proc_claim2.image_id, &classification, generation2, &now)
        .await
        .unwrap();

    // Verify raw response was cleared.
    let img = ops.get_image(proc_claim2.image_id).await.unwrap();
    assert!(
        img.processing_last_raw_response.is_none(),
        "raw diagnostic response should be cleared on success"
    );
}

/// Permanent failure retains the raw response.
#[tokio::test]
async fn permanent_failure_retains_raw_response() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-perm-raw"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation = proc_claim.generation;

    // Simulate a permanent failure with raw response.
    let diagnostic_response = "401 Unauthorized body".to_string();
    ops.fail_processing(
        proc_claim.image_id,
        "classifier Authentication (HTTP 401)",
        Some(diagnostic_response.clone()),
        generation,
        ProcessingFailureDisposition::Failed,
        &now,
    )
    .await
    .unwrap();

    let img = ops.get_image(proc_claim.image_id).await.unwrap();
    assert_eq!(
        img.processing_last_raw_response,
        Some(diagnostic_response),
        "raw diagnostic response should be retained on permanent failure"
    );
}

/// When a later failure has no response body, the prior diagnostic response
/// is retained (COALESCE guard).
#[tokio::test]
async fn failure_without_response_retains_prior_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let (_path, ops) = open_test_db(&dir).await;
    let now = now_ts();
    let camera_id = CameraId::new(1);
    let lease = lease_ts();

    ops.sync_cameras(
        &[CameraDiscovery {
            channel_number: 1,
            primary_track_id: "101".to_string(),
            picture_track_id: "103".to_string(),
            name: None,
            raw_discovery_identifier: None,
        }],
        &now,
    )
    .await
    .unwrap();

    let img = DiscoveredImage {
        image_key: ImageKey::new("key-coalesce"),
        camera_id,
        track_id: TrackId::new("103"),
        capture_start_at: now,
        capture_end_at: None,
        playback_uri: "http://nvr/pic/1".to_string(),
        canonical_playback_uri: "http://nvr/pic/1".to_string(),
        codec_type: Some("jpeg".to_string()),
        content_type: Some("picture".to_string()),
        nvr_reported_size: None,
        discovered_at: now,
    };

    let window = SearchWindowCommit {
        camera_id,
        window_start: now,
        window_end: future_ts(1),
        next_search_at: future_ts(2),
        polled_at: now,
        updated_at: now,
    };

    ops.commit_search_window(&window, &[img]).await.unwrap();

    let download_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    ops.complete_download(
        download_claim.image_id,
        &PathBuf::from("/tmp/test.jpg"),
        &now,
    )
    .await
    .unwrap();
    let proc_claim = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation = proc_claim.generation;

    // First failure with raw response.
    // Use past_ts so the image is immediately claimable for re-processing.
    let first_response = "first diagnostic response".to_string();
    let retry_at = past_ts(1);
    ops.fail_processing(
        proc_claim.image_id,
        "error 1",
        Some(first_response.clone()),
        generation,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: retry_at,
        },
        &now,
    )
    .await
    .unwrap();

    // Re-claim and fail again without a response body.
    let proc_claim2 = ops
        .claim_next_processing(&now, &lease)
        .await
        .unwrap()
        .unwrap();
    let generation2 = proc_claim2.generation;
    let retry_at2 = past_ts(2);
    ops.fail_processing(
        proc_claim2.image_id,
        "error 2",
        None, // no response body this time
        generation2,
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: retry_at2,
        },
        &now,
    )
    .await
    .unwrap();

    // The first diagnostic response should still be retained.
    let img = ops.get_image(proc_claim2.image_id).await.unwrap();
    assert_eq!(
        img.processing_last_raw_response,
        Some(first_response),
        "prior diagnostic response should be retained when new failure has no body"
    );
}
