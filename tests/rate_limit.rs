use std::time::Duration;

use fauna_scan::configuration::ClassifierRateLimitConfig;
use fauna_scan::database::Database;
use fauna_scan::database::repository::RateLimitReservation;
use fauna_scan::domain::Timestamp;
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

#[tokio::test]
async fn provider_quota_is_sliding_per_minute_and_persistent_per_day() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let start: Timestamp = "2026-07-25T12:00:00Z".parse().unwrap();
    let database = Database::open(&path).await.unwrap();
    let ops = database.ops();
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
    drop(database);

    // A fresh process/database handle must retain the already consumed daily
    // quota even after the minute window has expired.
    let database = Database::open(&path).await.unwrap();
    let after_minute: Timestamp = "2026-07-25T12:01:01Z".parse().unwrap();
    assert!(matches!(
        database.ops().reserve_classifier_rate_limit(&policy, 10, &after_minute).await.unwrap(),
        RateLimitReservation::DailyExhausted(wait) if wait > Duration::from_secs(10 * 60 * 60)
    ));
}
