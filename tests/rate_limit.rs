use std::time::Duration;

use fauna_scan::configuration::ClassifierRateLimitConfig;
use fauna_scan::database::repository::{
    DatabaseOps, RateLimitReservation, RateLimitedProcessingClaim,
};
use fauna_scan::database::sqlite::SqliteDataStore;
use fauna_scan::domain::Timestamp;
use sqlx::Row;
use tempfile::tempdir;

fn policy() -> ClassifierRateLimitConfig {
    ClassifierRateLimitConfig {
        quota_group: "test-cerebras-gemma".to_string(),
        requests_per_minute: 2,
        requests_per_day: 2,
        tokens_per_minute: 100,
        tokens_per_day: 100,
        estimated_input_tokens_per_request: 10,
        max_images_per_request: 1,
    }
}

async fn insert_downloaded_image(_ops: &DatabaseOps, pool: &sqlx::SqlitePool, now: &Timestamp) {
    let now = now.to_string();
    sqlx::query(
        "INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, \
         first_seen_at, last_seen_at, created_at, updated_at) \
         VALUES (1, 'primary', 'picture', ?, ?, ?, ?)",
    )
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images (image_key, camera_id, track_id, capture_start_at, \
         playback_uri, canonical_playback_uri, local_path, download_status, \
         downloaded_at, discovered_at, created_at, updated_at) \
         VALUES ('image-key', 1, 'track', ?, '/playback', '/playback', \
         '/tmp/image.jpg', 'downloaded', ?, ?, ?, ?)",
    )
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn provider_quota_is_sliding_per_minute_and_persistent_per_day() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let start: Timestamp = "2026-07-25T12:00:00Z".parse().unwrap();
    let store = SqliteDataStore::connect(&path, 4).await.unwrap();
    let _pool = store.pool();
    let ops = store.ops();
    let policy = policy();

    assert_eq!(
        ops.reserve_classifier_rate_limit(&policy, 10, &start)
            .await
            .unwrap(),
        RateLimitReservation::Granted
    );
    assert_eq!(
        ops.reserve_classifier_rate_limit(&policy, 10, &start)
            .await
            .unwrap(),
        RateLimitReservation::Granted
    );
    assert!(matches!(
        ops.reserve_classifier_rate_limit(&policy, 10, &start).await.unwrap(),
        RateLimitReservation::DailyExhausted(wait) if wait >= Duration::from_secs(60)
    ));
    drop(ops);
    drop(store);

    // A fresh process/database handle must retain the already consumed daily
    // quota even after the minute window has expired.
    let store2 = SqliteDataStore::connect(&path, 4).await.unwrap();
    let ops2 = store2.ops();
    let after_minute: Timestamp = "2026-07-25T12:01:01Z".parse().unwrap();
    assert!(matches!(
        ops2.reserve_classifier_rate_limit(&policy, 10, &after_minute).await.unwrap(),
        RateLimitReservation::DailyExhausted(wait) if wait > Duration::from_secs(10 * 60 * 60)
    ));
}

#[tokio::test]
async fn rate_limited_claim_reserves_only_when_it_claims_work() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let now: Timestamp = "2026-07-25T12:00:00Z".parse().unwrap();
    let lease: Timestamp = "2026-07-25T12:10:00Z".parse().unwrap();
    let store = SqliteDataStore::connect(&path, 4).await.unwrap();
    let pool = store.pool();
    let ops = store.ops();
    insert_downloaded_image(&ops, pool, &now).await;
    let policy = policy();

    let grant = match ops
        .claim_next_processing_with_rate_limit(&policy, 10, &now, &lease)
        .await
        .unwrap()
    {
        RateLimitedProcessingClaim::Claimed { claim, grant } => {
            assert_eq!(claim.image_id.get(), 1);
            grant
        }
        other => panic!("expected a claim, got {other:?}"),
    };
    assert!(matches!(
        ops.claim_next_processing_with_rate_limit(&policy, 10, &now, &lease)
            .await
            .unwrap(),
        RateLimitedProcessingClaim::NoWork
    ));

    let usage = sqlx::query(
        "SELECT requests, tokens FROM classifier_rate_limit_daily_usage \
         WHERE quota_group = ? AND day = '2026-07-25'",
    )
    .bind(&policy.quota_group)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(usage.get::<i64, _>(0), 1);
    assert_eq!(usage.get::<i64, _>(1), 20);

    assert!(
        ops.refund_daily_classifier_rate_limit(&grant, &now)
            .await
            .unwrap()
    );
    assert!(
        !ops.refund_daily_classifier_rate_limit(&grant, &now)
            .await
            .unwrap()
    );
    let usage = sqlx::query(
        "SELECT requests, tokens FROM classifier_rate_limit_daily_usage \
         WHERE quota_group = ? AND day = '2026-07-25'",
    )
    .bind(&policy.quota_group)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(usage.get::<i64, _>(0), 0);
    assert_eq!(usage.get::<i64, _>(1), 0);
}
