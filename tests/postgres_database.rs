//! PostgreSQL integration tests.
//!
//! These tests are gated by the `FAUNA_SCAN_TEST_POSTGRES_URL` environment
//! variable.  When set, each test creates an isolated schema, runs the
//! PostgreSQL migrations, and exercises the repository contract including
//! concurrent claims, rate-limit serialization, pagination, and
//! classification-versus-GC coordination.

use chrono::Utc;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

use fauna_scan::configuration::{ClassifierRateLimitConfig, Secret};
use fauna_scan::database::models::{
    DownloadFailureDisposition, ProcessingFailureDisposition, ServiceMetadataKey,
};
use fauna_scan::database::postgres::PostgresDataStore;
use fauna_scan::database::repository::{
    GarbageCollectionCandidate, GcOutcome, RateLimitReservation, RateLimitedProcessingClaim,
};
use fauna_scan::database::web_models::*;
use fauna_scan::domain::{CameraId, ImageId, ImageKey, ProcessingStatus, Timestamp, TrackId};

/// Build a unique schema name from the test name using a UUID suffix
/// to avoid collisions even when multiple tests start in the same second.
/// Uses a non-reserved prefix (`fs_`) and places the UUID first so the
/// 63-byte PostgreSQL identifier limit is not hit by long test names.
fn schema_name(test_name: &str) -> String {
    let uuid = Uuid::new_v4().simple().to_string();
    let sanitized: String = test_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    // Truncate if needed to stay within PostgreSQL's 63-byte identifier limit
    let max_len = 63 - uuid.len() - 3; // 3 for "fs_" prefix
    let sanitized = if sanitized.len() > max_len {
        &sanitized[..max_len]
    } else {
        &sanitized
    };
    format!("fs_{uuid}_{sanitized}")
}

/// Build a connection URL that targets a specific schema via `search_path`.
///
/// Appends `options=--search_path%3D{schema}` to the base PostgreSQL URL so
/// every connection in the pool operates in the target schema.  Preserves
/// any existing query parameters in the base URL.
fn schema_url(base_url: &str, schema: &str) -> String {
    let encoded = urlencoding_encode(schema);
    let sep = if base_url.contains('?') { "&" } else { "?" };
    format!("{base_url}{sep}options=--search_path%3D{encoded}")
}

fn urlencoding_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u8)
            }
        })
        .collect()
}

/// Create an isolated schema, configure a pool to use it, run migrations,
/// and return a `(pool, schema)` tuple for cleanup.
async fn setup_test_schema(test_name: &str, max_connections: u32) -> (PgPool, String) {
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL")
        .expect("FAUNA_SCAN_TEST_POSTGRES_URL must be set");
    let schema = schema_name(test_name);

    // Create the schema using the base connection (before scoping to the schema).
    let base_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_url)
        .await
        .expect("connect to test PostgreSQL for schema creation");
    // Schema identifiers are generated locally from test names and UUIDs.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        schema
    )))
    .execute(&base_pool)
    .await
    .expect("create test schema");
    drop(base_pool);

    // Now open a schema-scoped pool.
    let url = schema_url(&base_url, &schema);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(&url)
        .await
        .expect("connect to test PostgreSQL with schema");

    // Run migrations in the pool that already has search_path set.
    sqlx::migrate!("./migrations-postgres")
        .run(&pool)
        .await
        .expect("run PostgreSQL migrations in schema");

    (pool, schema)
}

/// Drop the test schema.
async fn drop_schema(pool: &PgPool, schema: &str) {
    // The schema identifier comes from schema_name(), never external input.
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {schema} CASCADE"
    )))
    .execute(pool)
    .await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_web_authentication_and_session_revocation() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use fauna_scan::authentication::{fingerprint, hash_password, new_session_token};
    use std::sync::Arc;
    use tower::ServiceExt;

    let (pool, schema) = setup_test_schema("web_authentication", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .unwrap();
    let ops = store.ops();
    let username = "Test User ' स";
    let hash = hash_password("test password").unwrap();
    assert!(ops.add_user(username, &hash).await.unwrap());
    assert!(!ops.add_user(username, "replacement").await.unwrap());
    assert_eq!(ops.list_users().await.unwrap(), vec![username]);
    assert_eq!(
        ops.find_user(username)
            .await
            .unwrap()
            .unwrap()
            .password_hash,
        hash
    );
    let user = ops.find_user(username).await.unwrap().unwrap();
    let expired = new_session_token();
    let now = Utc::now().timestamp();
    assert!(
        ops.create_session(&fingerprint(&expired), &user, Some(now))
            .await
            .unwrap()
    );
    assert!(
        ops.find_session(&fingerprint(&expired), now)
            .await
            .unwrap()
            .is_none()
    );

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            r#"[database]
backend = "postgres"
url = "postgres://localhost/test"
[general]
output_directory = {:?}
[nvr]
host = "nvr"
port = 80
username = "nvr-user"
password = "nvr-password"
start_at = "2026-01-01T00:00:00Z"
"#,
            root.path().join("images")
        ),
    )
    .unwrap();
    let config = fauna_scan::configuration::Config::load(Some(&path)).unwrap();
    let transport = Arc::new(fauna_scan::nvr::NvrTransport::from_config(&config.nvr).unwrap());
    let state = fauna_scan::web::WebState::from_config(ops.clone(), &config, transport);
    let app = fauna_scan::web::router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("x-fauna-scan-request", "1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"username": username, "password": "test password"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let token = cookie.split_once('=').unwrap().1;
    let stored = ops
        .find_session(&fingerprint(token), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.username, username);
    assert_eq!(stored.expires_at, None);
    assert_eq!(stored.credential_fingerprint, fingerprint(&hash));
    let check = || {
        Request::builder()
            .uri("/api/v1/auth/session")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(check()).await.unwrap().status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE users SET password_hash = $1 WHERE username = $2")
        .bind(hash_password("changed").unwrap())
        .bind(username)
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(
        app.clone().oneshot(check()).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // Password changes during verification must prevent session insertion.
    assert!(
        !ops.create_session(&fingerprint(&new_session_token()), &user, None)
            .await
            .unwrap()
    );
    assert!(ops.remove_user(username).await.unwrap());
    assert!(!ops.remove_user(username).await.unwrap());
    assert!(
        ops.find_session(&fingerprint(token), now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(ops.list_users().await.unwrap().is_empty());
    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_sqlite_to_pg_preserves_users_and_sessions() {
    use fauna_scan::authentication::{fingerprint, hash_password, new_session_token};

    let (pool, schema) = setup_test_schema("authentication_migration", 1).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let url = schema_url(&base_url, &schema);
    let root = tempfile::tempdir().unwrap();
    let database_path = root.path().join("source.sqlite3");
    let sqlite = fauna_scan::database::sqlite::SqliteDataStore::connect(&database_path, 1)
        .await
        .unwrap();
    let ops = sqlite.ops();
    let hash = hash_password("migration password").unwrap();
    let mut sessions = Vec::new();
    for index in 0..3 {
        let username = format!("Migration User {index}");
        ops.add_user(&username, &hash).await.unwrap();
        let user = ops.find_user(&username).await.unwrap().unwrap();
        let token = new_session_token();
        let expires_at = if index == 0 {
            None
        } else {
            Some(Utc::now().timestamp() + 3600)
        };
        ops.create_session(&fingerprint(&token), &user, expires_at)
            .await
            .unwrap();
        sessions.push((username, token, expires_at));
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sqlite-to-pg"))
        .args([
            "--sqlite-path",
            database_path.to_str().unwrap(),
            "--pg-url",
            &url,
            "--batch-size",
            "2",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let postgres = PostgresDataStore::connect(&Secret::new(url), 1)
        .await
        .unwrap();
    let ops = postgres.ops();
    assert_eq!(ops.list_users().await.unwrap().len(), 3);
    for (username, token, expires_at) in sessions {
        assert_eq!(
            ops.find_user(&username)
                .await
                .unwrap()
                .unwrap()
                .password_hash,
            hash
        );
        let session = ops
            .find_session(&fingerprint(&token), Utc::now().timestamp())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.username, username);
        assert_eq!(session.expires_at, expires_at);
        assert_eq!(session.credential_fingerprint, fingerprint(&hash));
    }
    drop_schema(&pool, &schema).await;
}

/// Poll `pg_locks` until at least `expected_waiters` backends are confirmed
/// waiting on the specific advisory lock held by `lock_pool`.
///
/// The complete self-join is performed in SQL so no lock identifiers need
/// to be decoded and rebound at the Rust layer.  OID columns are cast to
/// bigint and the database field uses IS NOT DISTINCT FROM for null-safe
/// equality.
///
/// Returns the number of distinct waiters observed within 5 seconds,
/// or 0 if fewer than expected appear.
async fn poll_lock_waiters(lock_pool: &PgPool, _lock_name: &str, expected_waiters: u32) -> u32 {
    let lock_holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(lock_pool)
        .await
        .expect("lock holder PID");

    // Count distinct backends waiting on the same advisory lock as the
    // holder.  The complete self-join runs in SQL using casts for OID
    // (→ bigint) and smallint (→ integer) columns, and IS NOT DISTINCT
    // FROM for null-safe database matching.
    let count_query = r#"SELECT COUNT(DISTINCT w.pid) FROM pg_locks h
               JOIN pg_locks w
                 ON h.locktype  = w.locktype
                AND h.database IS NOT DISTINCT FROM w.database
                AND h.classid::bigint = w.classid::bigint
                AND h.objid::bigint   = w.objid::bigint
                AND h.objsubid::integer = w.objsubid::integer
               WHERE h.locktype = 'advisory'
                 AND h.granted  = true
                 AND h.pid      = $1
                 AND w.granted  = false
                 AND w.mode     = 'ExclusiveLock'"#;

    let waiter_count: i64 = sqlx::query_scalar(count_query)
        .bind(lock_holder_pid)
        .fetch_one(lock_pool)
        .await
        .expect("count advisory-lock waiters");

    if waiter_count >= expected_waiters as i64 {
        return waiter_count as u32;
    }

    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let waiter_count: i64 = sqlx::query_scalar(count_query)
            .bind(lock_holder_pid)
            .fetch_one(lock_pool)
            .await
            .expect("count advisory-lock waiters");

        if waiter_count >= expected_waiters as i64 {
            return waiter_count as u32;
        }
    }

    0
}

/// Insert a test camera into the database.
async fn insert_camera(pool: &PgPool, channel: i64, track_id: &str) -> i64 {
    let now = "2026-07-21T10:00:00.000000000Z";
    let row = sqlx::query(
        r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, name,
                                 raw_discovery_identifier, enabled, first_seen_at, last_seen_at,
                                 created_at, updated_at)
           VALUES ($1, $2, $3, $3, $3, 1, $4, $4, $4, $4)
           RETURNING id"#,
    )
    .bind(channel)
    .bind(track_id)
    .bind(track_id)
    .bind(now)
    .fetch_one(pool)
    .await
    .expect("insert camera");
    row.try_get::<i64, _>(0).expect("camera id")
}

/// Insert a test image into the database.
async fn insert_image(
    pool: &sqlx::postgres::PgPool,
    camera_id: i64,
    track_id: &str,
    capture_start: &str,
    download_status: &str,
    processing_status: &str,
) -> i64 {
    let image_key = format!("img-{camera_id}-{capture_start}");
    let row = sqlx::query(
        r#"INSERT INTO images (image_key, camera_id, track_id, capture_start_at,
                   playback_uri, canonical_playback_uri, codec_type, content_type,
                   nvr_reported_size, discovered_at, created_at, updated_at,
                   download_status, download_attempts, downloaded_at,
                   download_last_error, download_next_attempt_at, download_lease_until,
                   processing_status, processing_attempts, processing_started_at,
                   processing_completed_at, processing_last_error,
                   processing_next_attempt_at, processing_lease_until)
           VALUES ($1, $2, $3, $4, 'http://nvr/image/1', 'http://nvr/image/1',
                   'h264', 'image/jpeg', 100000, $4, $4, $4,
                   $5, 1, $4, NULL, NULL, NULL,
                   $6, 0, NULL, NULL, NULL, NULL, NULL)
           RETURNING id"#,
    )
    .bind(image_key)
    .bind(camera_id)
    .bind(track_id)
    .bind(capture_start)
    .bind(download_status)
    .bind(processing_status)
    .fetch_one(pool)
    .await
    .expect("insert image");
    row.try_get::<i64, _>(0).expect("image id")
}

/// Insert a classification for an image.
async fn insert_classification(
    pool: &sqlx::postgres::PgPool,
    image_id: i64,
    model: &str,
    prompt_version: &str,
    completed_at: &str,
) -> i64 {
    insert_classification_with_flags(
        pool,
        image_id,
        model,
        prompt_version,
        completed_at,
        true,
        true,
    )
    .await
}

/// Insert a classification for an image with explicit wildlife/interesting flags.
async fn insert_classification_with_flags(
    pool: &sqlx::postgres::PgPool,
    image_id: i64,
    model: &str,
    prompt_version: &str,
    completed_at: &str,
    contains_wildlife: bool,
    is_interesting: bool,
) -> i64 {
    let now = "2026-07-21T10:00:00.000000000Z";
    let wl = if contains_wildlife { 1i64 } else { 0i64 };
    let int_flag = if is_interesting { 1i64 } else { 0i64 };
    let summary = if contains_wildlife {
        "A deer."
    } else {
        "No wildlife."
    };
    let species_json = if contains_wildlife {
        r#"[{"name":"deer","confidence":0.95}]"#
    } else {
        "[]"
    };
    let confidence = if contains_wildlife { 0.95f64 } else { 0.3f64 };
    let classification_json = if contains_wildlife {
        r#"{"summary":"A deer."}"#
    } else {
        "{}"
    };
    let row = sqlx::query(
        r#"INSERT INTO classifications (image_id, model, prompt_version, contains_wildlife,
                   is_interesting, summary, species_json, confidence, classification_json,
                   raw_response, request_started_at, request_completed_at, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'SECRET', $10, $11, $10)
           RETURNING id"#,
    )
    .bind(image_id)
    .bind(model)
    .bind(prompt_version)
    .bind(wl)
    .bind(int_flag)
    .bind(summary)
    .bind(species_json)
    .bind(confidence)
    .bind(classification_json)
    .bind(now)
    .bind(completed_at)
    .fetch_one(pool)
    .await
    .expect("insert classification");
    row.try_get::<i64, _>(0).expect("classification id")
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_migrations_and_cameras() {
    let (pool, schema) = setup_test_schema("test_postgres_migrations_and_cameras", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    // Sync cameras
    let cameras = vec![
        fauna_scan::database::models::CameraDiscovery {
            channel_number: 1,
            primary_track_id: "track-1".to_string(),
            picture_track_id: "pic-1".to_string(),
            name: Some("Camera One".to_string()),
            raw_discovery_identifier: Some("disc-1".to_string()),
        },
        fauna_scan::database::models::CameraDiscovery {
            channel_number: 2,
            primary_track_id: "track-2".to_string(),
            picture_track_id: "pic-2".to_string(),
            name: Some("Camera Two".to_string()),
            raw_discovery_identifier: Some("disc-2".to_string()),
        },
    ];
    let now = Timestamp::new(Utc::now());
    let synced = ops
        .sync_cameras(&cameras, &now)
        .await
        .expect("sync cameras");
    assert_eq!(synced.len(), 2);
    assert_eq!(synced[0].channel_number, 1);
    assert_eq!(synced[1].channel_number, 2);

    // List active cameras
    let active = ops.list_active_cameras().await.expect("list cameras");
    assert_eq!(active.len(), 2);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_image_discovery_and_cursors() {
    let (pool, schema) = setup_test_schema("test_postgres_image_discovery_and_cursors", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    // Insert camera
    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Commit search window
    let images = vec![fauna_scan::database::models::DiscoveredImage {
        image_key: ImageKey::new("img-1".to_string()),
        camera_id: CameraId::new(camera_id),
        track_id: TrackId::new("track-1".to_string()),
        capture_start_at: "2026-07-21T10:00:00.000000000Z".parse().unwrap(),
        capture_end_at: Some("2026-07-21T10:00:05.000000000Z".parse().unwrap()),
        playback_uri: "http://nvr/image/1".to_string(),
        canonical_playback_uri: "http://nvr/image/1".to_string(),
        codec_type: Some("h264".to_string()),
        content_type: Some("image/jpeg".to_string()),
        nvr_reported_size: Some(100000),
        discovered_at: "2026-07-21T10:00:00.000000000Z".parse().unwrap(),
    }];
    let window = fauna_scan::database::models::SearchWindowCommit {
        camera_id: CameraId::new(camera_id),
        window_start: "2026-07-21T09:00:00.000000000Z".parse().unwrap(),
        window_end: "2026-07-21T10:00:00.000000000Z".parse().unwrap(),
        next_search_at: "2026-07-21T10:01:00.000000000Z".parse().unwrap(),
        polled_at: "2026-07-21T10:00:00.000000000Z".parse().unwrap(),
        updated_at: "2026-07-21T10:00:00.000000000Z".parse().unwrap(),
    };
    let new_count = ops
        .commit_search_window(&window, &images)
        .await
        .expect("commit search window");
    assert_eq!(new_count, 1);

    // Get cursor
    let cursor = ops
        .get_cursor(CameraId::new(camera_id))
        .await
        .expect("get cursor")
        .expect("cursor exists");
    assert!(cursor.next_search_at.is_some());

    // Get image
    let image = ops.get_image(ImageId::new(1)).await.expect("get image");
    assert_eq!(image.image_key.as_str(), "img-1");

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_download_claims() {
    let (pool, schema) = setup_test_schema("test_postgres_download_claims", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "pending",
        "new",
    )
    .await;

    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));

    // Claim download
    let claim = ops
        .claim_next_download(&now, &lease)
        .await
        .expect("claim download")
        .expect("got claim");
    assert_eq!(claim.image_id.get(), image_id);

    // No more pending images
    let no_claim = ops
        .claim_next_download(&now, &lease)
        .await
        .expect("claim again");
    assert!(no_claim.is_none());

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_processing_claims() {
    let (pool, schema) = setup_test_schema("test_postgres_processing_claims", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Mark as downloaded with local path
    let now = "2026-07-21T10:00:00.000000000Z";
    sqlx::query(
        r#"UPDATE images SET local_path = '/tmp/test-image.jpg', local_file_identity = '/tmp/test-image.jpg',
                   downloaded_at = $1, download_status = 'downloaded', download_lease_until = NULL
           WHERE id = $2"#,
    )
    .bind(now)
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("mark downloaded");

    let now_ts = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now_ts.as_datetime()) + chrono::Duration::seconds(300));

    // Claim processing
    let claim = ops
        .claim_next_processing(&now_ts, &lease)
        .await
        .expect("claim processing")
        .expect("got claim");
    assert_eq!(claim.image_id.get(), image_id);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_stale_generation_rejection() {
    let (pool, schema) = setup_test_schema("test_postgres_stale_generation_rejection", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Manually set generation to 100
    sqlx::query(
        r#"UPDATE images SET processing_status = 'processing', processing_generation = 100,
                   processing_started_at = '2026-07-21T10:00:00.000000000Z',
                   processing_lease_until = '2026-07-21T10:10:00.000000000Z'
           WHERE id = $1"#,
    )
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("set generation");

    let classification_input = fauna_scan::database::models::ClassificationInput {
        model: "vision".to_string(),
        prompt_version: "wildlife-v1".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: Some("A deer".to_string()),
        species_json: Some("[{\"name\":\"deer\",\"confidence\":0.95}]".to_string()),
        bounding_boxes_json: None,
        confidence: Some(0.95),
        classification_json: Some("{\"summary\":\"A deer.\"}".to_string()),
        raw_response: Some("secret".to_string()),
        request_started_at: Timestamp::new(Utc::now()),
        request_completed_at: Timestamp::new(Utc::now()),
    };

    // Complete with wrong generation should fail
    let result = ops
        .complete_classification(
            ImageId::new(image_id),
            &classification_input,
            99, // wrong generation
            &Timestamp::new(Utc::now()),
        )
        .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("generation mismatch"));

    // Complete with correct generation should succeed
    let class_id = ops
        .complete_classification(
            ImageId::new(image_id),
            &classification_input,
            100, // correct generation
            &Timestamp::new(Utc::now()),
        )
        .await
        .expect("complete classification");
    assert!(class_id.get() > 0);

    // Verify image is done
    let image = ops
        .get_image(ImageId::new(image_id))
        .await
        .expect("get image");
    assert_eq!(image.processing_status, ProcessingStatus::Done);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_concurrent_download_claims() {
    use futures::stream::{self, StreamExt};

    let (pool, schema) = setup_test_schema("test_postgres_concurrent_download_claims", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert 5 pending images
    let mut image_ids = Vec::new();
    for i in 0..5 {
        let id = insert_image(
            &pool,
            camera_id,
            "track-1",
            &format!("2026-07-21T10:00:{i:02}.000000000Z"),
            "pending",
            "new",
        )
        .await;
        image_ids.push(id);
    }

    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));

    // Launch 10 concurrent claimers
    let ops_clone = ops.clone();
    let claims = stream::iter(0..10)
        .map(move |_| {
            let ops = ops_clone.clone();
            async move {
                ops.claim_next_download(&now, &lease)
                    .await
                    .expect("claim")
                    .map(|c| c.image_id.get())
            }
        })
        .buffer_unordered(10)
        .collect::<Vec<_>>()
        .await;

    let claimed_ids: Vec<i64> = claims.into_iter().flatten().collect();
    assert_eq!(claimed_ids.len(), 5, "exactly 5 images should be claimed");

    // Each image claimed exactly once
    let mut unique_ids = claimed_ids.clone();
    unique_ids.sort();
    unique_ids.dedup();
    assert_eq!(unique_ids.len(), 5, "each image claimed exactly once");

    // Verify no more claims available
    let remaining = ops
        .claim_next_download(&now, &lease)
        .await
        .expect("claim remaining");
    assert!(remaining.is_none());

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_rate_limit_serialization() {
    let (pool, schema) = setup_test_schema("test_postgres_rate_limit_serialization", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    // Create a rate limit config with 1 request per minute budget
    let limit = ClassifierRateLimitConfig {
        quota_group: "test-quota".to_string(),
        requests_per_minute: 1,
        requests_per_day: 10,
        tokens_per_minute: 1000,
        tokens_per_day: 10000,
        estimated_input_tokens_per_request: 500,
        max_images_per_request: 5,
    };

    let now = Timestamp::new(Utc::now());

    // First reservation should succeed
    let r1 = ops
        .reserve_classifier_rate_limit(&limit, 500, &now)
        .await
        .expect("first reservation");
    assert!(matches!(r1, RateLimitReservation::Granted));

    // Second reservation should wait (minute budget exhausted)
    let r2 = ops
        .reserve_classifier_rate_limit(&limit, 500, &now)
        .await
        .expect("second reservation");
    assert!(matches!(r2, RateLimitReservation::Wait(_)));

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_web_query_images() {
    let (pool, schema) = setup_test_schema("test_postgres_web_query_images", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    insert_classification(
        &pool,
        image_id,
        "vision",
        "wildlife-v1",
        "2026-07-21T10:05:00.000000000Z",
    )
    .await;

    // Query images
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: Some("2026-07-21T09:00:00.000000000Z".parse().unwrap()),
            to: Some("2026-07-21T11:00:00.000000000Z".parse().unwrap()),
            camera_ids: vec![camera_id],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: None,
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let query = WebImageQuery {
        filter,
        order: WebImageOrder::CapturedDescending,
        limit: 10,
        cursor: None,
    };

    let (summaries, next_cursor) = ops.web_query_images(&query).await.expect("query images");
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].classification.is_some());
    assert!(next_cursor.is_none());

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_classification_ranking_by_completed_at() {
    let (pool, schema) =
        setup_test_schema("test_postgres_classification_ranking_by_completed_at", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Insert two classifications with different completed_at timestamps
    // The later one should be selected as the "latest"
    insert_classification(
        &pool,
        image_id,
        "vision",
        "wildlife-v1",
        "2026-07-21T10:01:00.000000000Z",
    )
    .await;
    let class2_id = insert_classification(
        &pool,
        image_id,
        "vision",
        "wildlife-v2",
        "2026-07-21T10:05:00.000000000Z",
    )
    .await;

    // Query images - the latest by completed_at should be in the summary
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: None,
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let query = WebImageQuery {
        filter,
        order: WebImageOrder::CapturedDescending,
        limit: 10,
        cursor: None,
    };

    let (summaries, _) = ops.web_query_images(&query).await.expect("query images");
    assert_eq!(summaries.len(), 1);
    let classification = summaries[0]
        .classification
        .as_ref()
        .expect("has classification");
    assert_eq!(
        classification.id, class2_id,
        "latest classification by completed_at should be selected"
    );
    assert_eq!(classification.prompt_version, "wildlife-v2");

    // Detail should show both classifications in completed_at DESC order
    let detail = ops
        .web_image_detail(ImageId::new(image_id))
        .await
        .expect("image detail")
        .expect("detail exists");
    assert_eq!(detail.classifications.len(), 2);
    // First should be the later completed_at
    assert_eq!(
        detail.classifications[0].prompt_version, "wildlife-v2",
        "classifications should be ordered by request_completed_at DESC"
    );

    drop_schema(&pool, &schema).await;
}

/// Verify that overview and image-list filters reference the latest
/// classification only, not any historical classification.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_latest_classification_only() {
    let (pool, schema) = setup_test_schema("test_postgres_latest_classification_only", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Insert a non-wildlife classification first (older)
    insert_classification_with_flags(
        &pool,
        image_id,
        "vision",
        "v1",
        "2026-07-21T10:01:00.000000000Z",
        false, // not wildlife
        false, // not interesting
    )
    .await;

    // Insert a wildlife classification later (this should be the "latest")
    let _wildlife_class_id = insert_classification_with_flags(
        &pool,
        image_id,
        "vision",
        "v2",
        "2026-07-21T10:05:00.000000000Z",
        true, // wildlife!
        true, // interesting
    )
    .await;

    // Query images - should show wildlife because latest is wildlife
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(true),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let query = WebImageQuery {
        filter,
        order: WebImageOrder::CapturedDescending,
        limit: 10,
        cursor: None,
    };

    let (summaries, _) = ops.web_query_images(&query).await.expect("query images");
    assert_eq!(
        summaries.len(),
        1,
        "image with latest wildlife classification should match"
    );
    assert!(
        summaries[0]
            .classification
            .as_ref()
            .unwrap()
            .contains_wildlife,
        "summary should show latest classification as wildlife"
    );

    // Overview with wildlife filter should count this image
    // The filter should match because latest classification is wildlife
    let filter2 = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(true),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let overview = ops.web_overview(&filter2).await.expect("overview wildlife");
    assert_eq!(overview.wildlife, 1, "overview wildlife count should be 1");

    // Overview with no-wildlife filter should NOT count this image
    let filter3 = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(false),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let overview = ops
        .web_overview(&filter3)
        .await
        .expect("overview no wildlife");
    assert_eq!(
        overview.wildlife, 0,
        "overview no-wildlife count should be 0"
    );

    drop_schema(&pool, &schema).await;
}

/// Verify that classified=false (NotClassified) filter works correctly.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_not_classified_filter() {
    let (pool, schema) = setup_test_schema("test_postgres_not_classified_filter", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert a downloaded image without any classification
    let unclassified_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Insert a downloaded image with classification
    let classified_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    insert_classification(
        &pool,
        classified_id,
        "vision",
        "v1",
        "2026-07-21T10:02:00.000000000Z",
    )
    .await;

    // Query for unclassified images
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::NotClassified),
        contains_wildlife: None,
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let query = WebImageQuery {
        filter: filter.clone(),
        order: WebImageOrder::CapturedDescending,
        limit: 10,
        cursor: None,
    };

    let (summaries, _) = ops
        .web_query_images(&query)
        .await
        .expect("query unclassified");
    assert_eq!(
        summaries.len(),
        1,
        "only one unclassified image should be returned"
    );
    assert_eq!(
        summaries[0].id, unclassified_id,
        "the unclassified image should be returned"
    );
    assert!(
        summaries[0].classification.is_none(),
        "unclassified image should have no classification"
    );

    // Overview with NotClassified filter
    let overview = ops
        .web_overview(&filter)
        .await
        .expect("overview not classified");
    assert_eq!(
        overview.discovered, 1,
        "overview should show 1 unclassified image"
    );

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_web_activity_counts() {
    let (pool, schema) = setup_test_schema("test_postgres_web_activity_counts", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert images with mixed statuses
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloading",
        "new",
    )
    .await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "processing",
    )
    .await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:02:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: None,
        contains_wildlife: None,
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };

    let record = ops.web_activity(&filter).await.expect("web activity");

    // Count download statuses independently
    let download_counts: std::collections::HashMap<&str, i64> = record
        .counts
        .iter()
        .filter(|c| c.category == "download")
        .map(|c| (c.status.as_str(), c.count))
        .collect();

    assert_eq!(download_counts.get("downloading"), Some(&1));
    assert_eq!(download_counts.get("downloaded"), Some(&2));

    // Count processing statuses independently
    let processing_counts: std::collections::HashMap<&str, i64> = record
        .counts
        .iter()
        .filter(|c| c.category == "processing")
        .map(|c| (c.status.as_str(), c.count))
        .collect();

    assert_eq!(processing_counts.get("new"), Some(&1));
    assert_eq!(processing_counts.get("processing"), Some(&1));
    assert_eq!(processing_counts.get("done"), Some(&1));

    // Total download entries should be 2 (downloading + downloaded)
    let download_entries: Vec<_> = record
        .counts
        .iter()
        .filter(|c| c.category == "download")
        .collect();
    assert_eq!(download_entries.len(), 2);

    // Total processing entries should be 3 (new + processing + done)
    let processing_entries: Vec<_> = record
        .counts
        .iter()
        .filter(|c| c.category == "processing")
        .collect();
    assert_eq!(processing_entries.len(), 3);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_playback_urls_in_detail() {
    let (pool, schema) = setup_test_schema("test_postgres_playback_urls_in_detail", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Set both playback_uri and canonical_playback_uri
    sqlx::query(
        r#"UPDATE images SET playback_uri = 'http://nvr/playback/1',
                   canonical_playback_uri = 'http://nvr/canonical/1'
           WHERE id = $1"#,
    )
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("set playback URIs");

    let detail = ops
        .web_image_detail(ImageId::new(image_id))
        .await
        .expect("image detail")
        .expect("detail exists");

    // Both URLs should be present
    assert_eq!(
        detail.nvr.image_url,
        Some("http://nvr/canonical/1".to_string())
    );
    assert_eq!(
        detail.nvr.reported_image_url,
        Some("http://nvr/playback/1".to_string())
    );

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_lease_recovery() {
    let (pool, schema) = setup_test_schema("test_postgres_lease_recovery", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Set an expired lease
    let expired = "2026-07-21T09:00:00.000000000Z";
    sqlx::query(
        r#"UPDATE images SET processing_status = 'processing', processing_generation = 1,
                   processing_started_at = '2026-07-21T09:00:00.000000000Z',
                   processing_lease_until = $1
           WHERE id = $2"#,
    )
    .bind(expired)
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("set expired lease");

    let now = Timestamp::new(Utc::now());
    let counts = ops
        .recover_expired_leases(&now)
        .await
        .expect("recover leases");
    assert_eq!(counts.processing, 1);

    // Verify status changed to retry_wait
    let image = ops
        .get_image(ImageId::new(image_id))
        .await
        .expect("get image");
    assert_eq!(image.processing_status, ProcessingStatus::RetryWait);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_overview_counts() {
    let (pool, schema) = setup_test_schema("test_postgres_overview_counts", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert images with various statuses
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "pending",
        "new",
    )
    .await;
    let classified_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:02:00.000000000Z",
        "failed",
        "failed",
    )
    .await;

    insert_classification(
        &pool,
        classified_id,
        "vision",
        "v1",
        "2026-07-21T10:02:00.000000000Z",
    )
    .await;

    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: None,
        contains_wildlife: None,
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };

    let overview = ops.web_overview(&filter).await.expect("overview");
    assert_eq!(overview.discovered, 3);
    assert_eq!(overview.downloaded, 1);
    assert_eq!(overview.classified, 1);
    // Summary cards count images, so a double failure matches one list row.
    assert_eq!(overview.permanent_failures, 1);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_metadata() {
    let (pool, schema) = setup_test_schema("test_postgres_metadata", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let now = Timestamp::new(Utc::now());

    ops.set_metadata(&ServiceMetadataKey::ApplicationVersion, "1.0.0", &now)
        .await
        .expect("set metadata");

    let value = ops
        .get_metadata(&ServiceMetadataKey::ApplicationVersion)
        .await
        .expect("get metadata");
    assert_eq!(value, Some("1.0.0".to_string()));

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_operational_summary() {
    let (pool, schema) = setup_test_schema("test_postgres_operational_summary", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    let summary = ops
        .operational_summary()
        .await
        .expect("operational summary");
    assert_eq!(summary.cameras_active, 1);
    assert_eq!(summary.images_discovered, 1);
    assert_eq!(summary.classifications_completed, 1);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_web_health() {
    let (pool, schema) = setup_test_schema("test_postgres_web_health", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloading",
        "processing",
    )
    .await;

    let health = ops.web_health().await.expect("web health");
    assert_eq!(health.active_downloads, 1);
    assert_eq!(health.active_classifications, 1);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_web_cameras() {
    let (pool, schema) = setup_test_schema("test_postgres_web_cameras", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    let cameras = ops.web_cameras().await.expect("web cameras");
    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0].id, camera_id);
    assert_eq!(cameras[0].channel_number, 1);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_recording_target() {
    let (pool, schema) = setup_test_schema("test_postgres_recording_target", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    let target = ops
        .web_recording_target(ImageId::new(image_id))
        .await
        .expect("recording target")
        .expect("target exists");
    assert_eq!(target.primary_track_id, "track-1");
    assert_eq!(target.capture_start_at, "2026-07-21T10:00:00.000000000Z");

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_image_content_lookup() {
    let (pool, schema) = setup_test_schema("test_postgres_image_content_lookup", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    let lookup = ops
        .web_image_content_lookup(ImageId::new(image_id))
        .await
        .expect("content lookup")
        .expect("lookup exists");
    assert_eq!(lookup.download_status, "downloaded");

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_fail_download() {
    let (pool, schema) = setup_test_schema("test_postgres_fail_download", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "pending",
        "new",
    )
    .await;

    // Claim it first
    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));
    ops.claim_next_download(&now, &lease)
        .await
        .expect("claim")
        .expect("got claim");

    // Fail the download
    ops.fail_download(
        ImageId::new(image_id),
        "connection timeout",
        DownloadFailureDisposition::RetryWait {
            next_attempt_at: Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(60)),
        },
        &now,
    )
    .await
    .expect("fail download");

    let image = ops
        .get_image(ImageId::new(image_id))
        .await
        .expect("get image");
    assert_eq!(
        image.download_status,
        fauna_scan::domain::DownloadStatus::RetryWait
    );

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_fail_processing() {
    let (pool, schema) = setup_test_schema("test_postgres_fail_processing", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Set as processing with generation 1
    let now = "2026-07-21T10:00:00.000000000Z";
    sqlx::query(
        r#"UPDATE images SET processing_status = 'processing', processing_generation = 1,
                   processing_started_at = $1, processing_lease_until = '2026-07-21T10:10:00.000000000Z'
           WHERE id = $2"#,
    )
    .bind(now)
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("set processing");

    let now_ts = Timestamp::new(Utc::now());

    // Fail processing
    ops.fail_processing(
        ImageId::new(image_id),
        "classification error",
        Some("error response".to_string()),
        1, // generation
        ProcessingFailureDisposition::RetryWait {
            next_attempt_at: Timestamp::new(
                (*now_ts.as_datetime()) + chrono::Duration::seconds(60),
            ),
        },
        &now_ts,
    )
    .await
    .expect("fail processing");

    let image = ops
        .get_image(ImageId::new(image_id))
        .await
        .expect("get image");
    assert_eq!(image.processing_status, ProcessingStatus::RetryWait);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_verify_processing_ownership() {
    let (pool, schema) = setup_test_schema("test_postgres_verify_processing_ownership", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Set as processing with generation 1
    let now = "2026-07-21T10:00:00.000000000Z";
    sqlx::query(
        r#"UPDATE images SET processing_status = 'processing', processing_generation = 1,
                   processing_started_at = $1, processing_lease_until = '2026-07-21T10:10:00.000000000Z'
           WHERE id = $2"#,
    )
    .bind(now)
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("set processing");

    // Verify ownership
    ops.verify_processing_ownership(ImageId::new(image_id), 1)
        .await
        .expect("verify ownership");

    // Wrong generation should fail
    let result = ops
        .verify_processing_ownership(ImageId::new(image_id), 2)
        .await;
    assert!(result.is_err());

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_renew_processing_lease() {
    let (pool, schema) = setup_test_schema("test_postgres_renew_processing_lease", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Set as processing with generation 1
    let now = "2026-07-21T10:00:00.000000000Z";
    sqlx::query(
        r#"UPDATE images SET processing_status = 'processing', processing_generation = 1,
                   processing_started_at = $1, processing_lease_until = '2026-07-21T10:05:00.000000000Z'
           WHERE id = $2"#,
    )
    .bind(now)
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("set processing");

    let renewal_at = Timestamp::new(Utc::now());
    let new_lease = Timestamp::new((*renewal_at.as_datetime()) + chrono::Duration::seconds(300));

    ops.renew_processing_lease(ImageId::new(image_id), 1, &new_lease, &renewal_at)
        .await
        .expect("renew lease");

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_status_counts() {
    let (pool, schema) = setup_test_schema("test_postgres_status_counts", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "pending",
        "new",
    )
    .await;

    let counts = ops.status_counts().await.expect("status counts");
    assert_eq!(
        counts
            .download
            .get(&fauna_scan::domain::DownloadStatus::Downloaded),
        Some(&1)
    );
    assert_eq!(
        counts
            .download
            .get(&fauna_scan::domain::DownloadStatus::Pending),
        Some(&1)
    );
    assert_eq!(counts.processing.get(&ProcessingStatus::Done), Some(&1));
    assert_eq!(counts.processing.get(&ProcessingStatus::New), Some(&1));

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_classification_detail_ordering() {
    let (pool, schema) = setup_test_schema("test_postgres_classification_detail_ordering", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Insert three classifications with different completed_at timestamps
    insert_classification(
        &pool,
        image_id,
        "vision",
        "v1",
        "2026-07-21T10:01:00.000000000Z",
    )
    .await;
    insert_classification(
        &pool,
        image_id,
        "vision",
        "v2",
        "2026-07-21T10:03:00.000000000Z",
    )
    .await;
    insert_classification(
        &pool,
        image_id,
        "vision",
        "v3",
        "2026-07-21T10:02:00.000000000Z",
    )
    .await;

    let detail = ops
        .web_image_detail(ImageId::new(image_id))
        .await
        .expect("detail")
        .expect("exists");

    assert_eq!(detail.classifications.len(), 3);
    // Should be ordered by request_completed_at DESC
    assert_eq!(detail.classifications[0].prompt_version, "v2");
    assert_eq!(detail.classifications[1].prompt_version, "v3");
    assert_eq!(detail.classifications[2].prompt_version, "v1");

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_overview_wildlife_filter() {
    let (pool, schema) = setup_test_schema("test_postgres_overview_wildlife_filter", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id_1 = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    let image_id_2 = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Image 1: wildlife (using flag-parameterized helper)
    insert_classification_with_flags(
        &pool,
        image_id_1,
        "vision",
        "v1",
        "2026-07-21T10:01:00.000000000Z",
        true, // wildlife
        true, // interesting
    )
    .await;
    // Image 2: no wildlife (using flag-parameterized helper)
    insert_classification_with_flags(
        &pool,
        image_id_2,
        "vision",
        "v1",
        "2026-07-21T10:01:00.000000000Z",
        false, // not wildlife
        false, // not interesting
    )
    .await;

    // Filter by wildlife
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(true),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };

    let overview = ops.web_overview(&filter).await.expect("overview wildlife");
    assert_eq!(overview.discovered, 1);
    assert_eq!(overview.wildlife, 1);

    // Filter by no wildlife
    let filter2 = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(false),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };

    let overview2 = ops
        .web_overview(&filter2)
        .await
        .expect("overview no wildlife");
    assert_eq!(overview2.discovered, 1);

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_concurrent_processing_claims() {
    use futures::stream::{self, StreamExt};

    let (pool, schema) = setup_test_schema("test_postgres_concurrent_processing_claims", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert 5 downloaded images eligible for processing, seeding local_path
    let mut image_ids = Vec::new();
    for i in 0..5 {
        let id = insert_image(
            &pool,
            camera_id,
            "track-1",
            &format!("2026-07-21T10:00:{i:02}.000000000Z"),
            "downloaded",
            "new",
        )
        .await;
        // Seed local_path so the image is eligible for processing claims
        sqlx::query(
            r#"UPDATE images SET local_path = $1, local_file_identity = $1
               WHERE id = $2"#,
        )
        .bind(format!("/tmp/test_proc_{i}.jpg"))
        .bind(id)
        .execute(&pool)
        .await
        .expect("seed local_path");
        image_ids.push(id);
    }

    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));

    // Launch 10 concurrent claimers
    let ops_clone = ops.clone();
    let claims = stream::iter(0..10)
        .map(move |_| {
            let ops = ops_clone.clone();
            async move {
                ops.claim_next_processing(&now, &lease)
                    .await
                    .expect("claim")
                    .map(|c| c.image_id.get())
            }
        })
        .buffer_unordered(10)
        .collect::<Vec<_>>()
        .await;

    let claimed_ids: Vec<i64> = claims.into_iter().flatten().collect();
    assert_eq!(claimed_ids.len(), 5, "exactly 5 images should be claimed");

    // Each image claimed exactly once
    let mut unique_ids = claimed_ids.clone();
    unique_ids.sort();
    unique_ids.dedup();
    assert_eq!(unique_ids.len(), 5, "each image claimed exactly once");

    // Verify no more claims available
    let remaining = ops
        .claim_next_processing(&now, &lease)
        .await
        .expect("claim remaining");
    assert!(remaining.is_none());

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_concurrent_quota_admission() {
    use futures::stream::{self, StreamExt};

    let (pool, schema) = setup_test_schema("test_postgres_concurrent_quota_admission", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    // Create a rate limit config with 2 requests per minute budget.
    // Each reservation costs estimated_input_tokens_per_request (500) + max_tokens (500) = 1000.
    // tokens_per_minute = 2000 allows exactly 2 concurrent grants.
    let limit = ClassifierRateLimitConfig {
        quota_group: "test-quota-concurrent".to_string(),
        requests_per_minute: 2,
        requests_per_day: 100,
        tokens_per_minute: 2000,
        tokens_per_day: 10000,
        estimated_input_tokens_per_request: 500,
        max_images_per_request: 5,
    };

    let now = Timestamp::new(Utc::now());

    // Launch 5 concurrent reservations sharing one quota group
    let ops_clone = ops.clone();
    let results = stream::iter(0..5)
        .map(move |_| {
            let ops = ops_clone.clone();
            let limit = limit.clone();
            async move {
                ops.reserve_classifier_rate_limit(&limit, 500, &now)
                    .await
                    .expect("reservation")
            }
        })
        .buffer_unordered(5)
        .collect::<Vec<_>>()
        .await;

    // Exactly 2 should be granted, the rest should wait or be daily exhausted
    let granted_count = results
        .iter()
        .filter(|r| matches!(r, RateLimitReservation::Granted))
        .count();
    assert_eq!(
        granted_count, 2,
        "exactly 2 reservations should be granted from 5 concurrent"
    );

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_classification_vs_gc_race() {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    let (pool, schema) = setup_test_schema("test_postgres_classification_vs_gc_race", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Use a scoped temporary directory so the shared file is cleaned up
    // after the test completes, avoiding leftover artifacts in /tmp.
    let tmp_dir = TempDir::new().expect("create temp directory for shared file");
    let shared_path = tmp_dir.path().join("test_race_wildlife.jpg");
    let shared_path_str = shared_path.to_string_lossy().to_string();
    std::fs::write(&shared_path, b"shared wildlife image").expect("create shared file");

    // Image A: GC candidate — downloaded, processed, negative classification, has local_path.
    let image_a = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    sqlx::query(
        r#"UPDATE images SET local_path = $1, local_file_identity = $1
           WHERE id = $2"#,
    )
    .bind(&shared_path_str)
    .bind(image_a)
    .execute(&pool)
    .await
    .expect("set image A path");

    // Insert negative classification for image A so it passes GC eligibility.
    let neg_classified_at = "2026-07-21T10:00:01.000000000Z";
    sqlx::query(
        r#"INSERT INTO classifications (image_id, model, prompt_version, contains_wildlife, is_interesting,
               summary, species_json, confidence, classification_json, raw_response,
               request_started_at, request_completed_at, created_at)
           VALUES ($1, 'vision', 'wildlife-v1', 0, 0, 'No wildlife', NULL, NULL,
                   '{"summary":"No wildlife."}', NULL, $2, $2, $2)"#,
    )
    .bind(image_a)
    .bind(neg_classified_at)
    .execute(&pool)
    .await
    .expect("insert negative classification for A");

    // Image B: processing, same file identity — will be classified as wildlife.
    let image_b = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;
    sqlx::query(
        r#"UPDATE images SET processing_status = 'processing', processing_generation = 1,
                   processing_started_at = '2026-07-21T10:01:00.000000000Z',
                   processing_lease_until = '2026-07-21T10:10:00.000000000Z',
                   local_path = $1, local_file_identity = $1
           WHERE id = $2"#,
    )
    .bind(&shared_path_str)
    .bind(image_b)
    .execute(&pool)
    .await
    .expect("set image B processing state");

    let completed_at = Timestamp::new(Utc::now());
    let classification_input = fauna_scan::database::models::ClassificationInput {
        model: "vision".to_string(),
        prompt_version: "wildlife-v2".to_string(),
        contains_wildlife: true,
        is_interesting: true,
        summary: Some("A deer".to_string()),
        species_json: Some("[{\"name\":\"deer\",\"confidence\":0.95}]".to_string()),
        bounding_boxes_json: None,
        confidence: Some(0.95),
        classification_json: Some("{\"summary\":\"A deer.\"}".to_string()),
        raw_response: Some("secret".to_string()),
        request_started_at: Timestamp::new(Utc::now()),
        request_completed_at: completed_at,
    };

    // Shared state to track whether the GC callback was executed.
    let callback_executed = Arc::new(AtomicBool::new(false));
    let callback_executed_clone = callback_executed.clone();

    // ── Create a dedicated connection that will hold the advisory lock ──
    // This lock blocks both complete_classification and with_gc_candidate
    // until we explicitly release it, guaranteeing deterministic ordering.
    let lock_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base_url)
        .await
        .expect("connect to PostgreSQL for advisory lock");

    // Hold the advisory lock in an explicit transaction that we control.
    // The lock name must match the constant used in postgres.rs:
    //   hashtext('fauna_scan_classification_gc')
    sqlx::query("BEGIN")
        .execute(&lock_pool)
        .await
        .expect("begin lock transaction");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('fauna_scan_classification_gc'))")
        .execute(&lock_pool)
        .await
        .expect("acquire advisory lock");

    let ops_for_verify = ops.clone();
    let ops_for_gc = ops.clone();
    let ops_for_class = ops.clone();

    // ── Spawn complete_classification for image B.  It will block on the
    // advisory lock held by lock_pool. ──
    let class_task = tokio::spawn(async move {
        ops_for_class
            .complete_classification(
                ImageId::new(image_b),
                &classification_input,
                1,
                &completed_at,
            )
            .await
    });

    // ── Poll pg_locks until classification is confirmed waiting on the
    // advisory lock.  The self-join on pg_locks matches the holder
    // against waiters sharing the exact same lock identity, so unrelated
    // advisory locks cannot satisfy this check. ──
    let class_waiters = poll_lock_waiters(&lock_pool, "fauna_scan_classification_gc", 1).await;
    assert_eq!(
        class_waiters, 1,
        "classification must be the sole waiter on the advisory lock before GC starts"
    );

    // ── Spawn GC candidate check for image A.  It also blocks on the lock. ──
    let gc_task = tokio::spawn(async move {
        let candidate = GarbageCollectionCandidate {
            image_id: ImageId::new(image_a),
            local_path: PathBuf::from(&shared_path_str),
        };
        ops_for_gc
            .with_gc_candidate(
                &candidate,
                &Timestamp::new(Utc::now()),
                Some(PathBuf::from(&shared_path_str).as_path()),
                Box::new(move || {
                    let executed = callback_executed_clone.clone();
                    Box::pin(async move {
                        executed.store(true, Ordering::SeqCst);
                        Ok(GcOutcome::Removed)
                    })
                }),
            )
            .await
    });

    // ── Poll pg_locks until GC is also confirmed waiting on the lock.
    // After both tasks have reached the lock we expect exactly two
    // distinct waiters (classification + GC). ──
    let gc_waiters = poll_lock_waiters(&lock_pool, "fauna_scan_classification_gc", 2).await;
    assert_eq!(
        gc_waiters, 2,
        "both classification and GC must be waiting on the advisory lock before release"
    );

    // ── Verify neither operation completed before releasing the lock. ──
    let class_is_pending = class_task.is_finished();
    let gc_is_pending = gc_task.is_finished();
    assert!(
        !class_is_pending && !gc_is_pending,
        "neither classification nor GC should have completed before lock release"
    );

    // ── Release the advisory lock.  PostgreSQL FIFO ordering means
    // complete_classification (started first) will acquire it next. ──
    sqlx::query("COMMIT")
        .execute(&lock_pool)
        .await
        .expect("release advisory lock");
    drop(lock_pool);

    // Wait for both tasks to complete.
    let gc_result = gc_task.await.expect("gc task should not panic");
    let class_result = class_task
        .await
        .expect("classification task should not panic");

    // Classification must succeed — it transitions B to done with wildlife.
    assert!(
        class_result.is_ok(),
        "complete_classification should succeed: {:?}",
        class_result.err()
    );
    let class_id = class_result.expect("classification completed");
    assert!(class_id.get() > 0, "classification should have an ID");

    // The shared advisory lock ensures classification commits wildlife before
    // GC can check eligibility.  Therefore GC must refuse to delete.
    assert!(
        !callback_executed.load(Ordering::SeqCst),
        "GC callback should not have been executed because wildlife was detected"
    );

    // The file must still exist on disk — GC refused to unlink it.
    assert!(
        std::path::Path::new(&shared_path).exists(),
        "shared file should still exist after GC refused to delete"
    );

    // GC should have returned an error about shared wildlife.
    match &gc_result {
        Ok(_) => {
            panic!("GC should have failed due to shared wildlife detection");
        }
        Err(e) => {
            assert!(
                e.message.contains("shares wildlife") || e.message.contains("candidate ineligible"),
                "GC failure should be due to wildlife detection: {}",
                e.message
            );
        }
    }

    // Verify the core invariant: no single image has both a cleared path AND a
    // wildlife-positive classification.  Image A has a negative classification
    // so it can be cleared safely.  Image B has wildlife so it must never be
    // cleared.
    let detail_b = ops_for_verify
        .web_image_detail(ImageId::new(image_b))
        .await
        .expect("image B detail")
        .expect("image B exists");

    let b_is_wildlife = detail_b
        .classifications
        .first()
        .map(|c| c.contains_wildlife)
        .unwrap_or(false);
    assert!(b_is_wildlife, "image B should have wildlife classification");

    drop_schema(&pool, &schema).await;
}

/// Verify that wildlife/interesting/confidence filters reference the latest
/// classification only, not any historical classification.
///
/// This is the inverse of `test_postgres_latest_classification_only`:
/// an image whose older classification is wildlife-positive but latest is
/// negative must NOT match positive filters.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_inverse_history_wildlife_filter() {
    let (pool, schema) =
        setup_test_schema("test_postgres_inverse_history_wildlife_filter", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Insert a wildlife classification first (older)
    insert_classification_with_flags(
        &pool,
        image_id,
        "vision",
        "v1",
        "2026-07-21T10:01:00.000000000Z",
        true, // wildlife!
        true, // interesting
    )
    .await;

    // Insert a non-wildlife classification later (this is the "latest")
    let _latest_class_id = insert_classification_with_flags(
        &pool,
        image_id,
        "vision",
        "v2",
        "2026-07-21T10:05:00.000000000Z",
        false, // not wildlife
        false, // not interesting
    )
    .await;

    // Query images - should NOT show wildlife because latest is NOT wildlife
    // (the old wildlife classification should NOT match)
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(true),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let query = WebImageQuery {
        filter,
        order: WebImageOrder::CapturedDescending,
        limit: 10,
        cursor: None,
    };

    let (summaries, _) = ops.web_query_images(&query).await.expect("query images");
    assert_eq!(
        summaries.len(),
        0,
        "image with latest non-wildlife should NOT match wildlife filter, even if older was wildlife"
    );

    // Overview with wildlife filter should NOT count this image
    let filter2 = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(true),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let overview = ops.web_overview(&filter2).await.expect("overview wildlife");
    assert_eq!(overview.wildlife, 0, "overview wildlife count should be 0");

    // But it SHOULD match the no-wildlife filter
    let filter3 = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: Some(false),
        is_interesting: None,
        confidence_min: None,
        advanced: Default::default(),
    };
    let overview2 = ops
        .web_overview(&filter3)
        .await
        .expect("overview no wildlife");
    assert_eq!(overview2.wildlife, 0);
    assert_eq!(
        overview2.discovered, 1,
        "image should match no-wildlife filter"
    );

    // Verify detail shows latest classification as non-wildlife
    let detail = ops
        .web_image_detail(ImageId::new(image_id))
        .await
        .expect("image detail")
        .expect("image exists");
    assert_eq!(
        detail.classifications[0].prompt_version, "v2",
        "latest classification should be v2"
    );
    assert!(
        !detail.classifications[0].contains_wildlife,
        "latest classification should be non-wildlife"
    );

    drop_schema(&pool, &schema).await;
}

/// Verify that confidence filters reference the latest classification only.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_inverse_history_confidence_filter() {
    let (pool, schema) =
        setup_test_schema("test_postgres_inverse_history_confidence_filter", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "done",
    )
    .await;

    // Insert a high-confidence classification first (older)
    insert_classification_with_flags(
        &pool,
        image_id,
        "vision",
        "v1",
        "2026-07-21T10:01:00.000000000Z",
        true, // wildlife
        true, // interesting
    )
    .await;

    // Insert a low-confidence classification later (this is the "latest")
    let _latest_class_id = insert_classification_with_flags(
        &pool,
        image_id,
        "vision",
        "v2",
        "2026-07-21T10:05:00.000000000Z",
        false, // not wildlife
        false, // not interesting
    )
    .await;

    // Query with high confidence filter - should NOT match because latest confidence is low
    let filter = WebImageFilter {
        scope: WebScopeFilter {
            from: None,
            to: None,
            camera_ids: vec![],
        },
        download_status: None,
        processing_status: None,
        classified: Some(WebClassifiedFilter::Classified),
        contains_wildlife: None,
        is_interesting: None,
        confidence_min: Some(0.9),
        advanced: Default::default(),
    };
    let query = WebImageQuery {
        filter,
        order: WebImageOrder::CapturedDescending,
        limit: 10,
        cursor: None,
    };

    let (summaries, _) = ops.web_query_images(&query).await.expect("query images");
    assert_eq!(
        summaries.len(),
        0,
        "image with latest low-confidence should NOT match high-confidence filter"
    );

    drop_schema(&pool, &schema).await;
}

/// Verify that `claim_next_processing_with_rate_limit` atomically admits
/// provider quota and claims eligible processing work.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_rate_limited_processing_claim() {
    let (pool, schema) = setup_test_schema("test_postgres_rate_limited_processing_claim", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert downloaded images with local_path set
    let image_id_1 = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;
    let image_id_2 = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;

    // Seed local_path so images are eligible for processing
    for id in [image_id_1, image_id_2] {
        sqlx::query(
            r#"UPDATE images SET local_path = $1, local_file_identity = $1
               WHERE id = $2"#,
        )
        .bind(format!("/tmp/test_rate_limit_{id}.jpg"))
        .bind(id)
        .execute(&pool)
        .await
        .expect("seed local_path");
    }

    let limit = ClassifierRateLimitConfig {
        quota_group: "test-rate-limit-claim".to_string(),
        requests_per_minute: 10,
        requests_per_day: 100,
        tokens_per_minute: 10000,
        tokens_per_day: 100000,
        estimated_input_tokens_per_request: 500,
        max_images_per_request: 5,
    };

    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));

    // First claim should succeed
    let claim1 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("first rate-limited claim");
    match &claim1 {
        RateLimitedProcessingClaim::Claimed { claim, grant } => {
            assert_eq!(claim.image_id.get(), image_id_1);
            assert!(!grant.id.is_empty(), "grant should have an ID");
            assert_eq!(grant.quota_group, "test-rate-limit-claim");
        }
        other => panic!("expected Claimed, got {:?}", other),
    }

    // Second claim should also succeed (within budget)
    let claim2 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("second rate-limited claim");
    match &claim2 {
        RateLimitedProcessingClaim::Claimed { claim, grant } => {
            assert_eq!(claim.image_id.get(), image_id_2);
            assert!(!grant.id.is_empty(), "grant should have an ID");
        }
        other => panic!("expected Claimed, got {:?}", other),
    }

    // Third claim should return NoWork (no more eligible images)
    let claim3 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("third rate-limited claim");
    assert!(matches!(claim3, RateLimitedProcessingClaim::NoWork));

    // Verify the events were recorded
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM classifier_rate_limit_events WHERE quota_group = $1",
    )
    .bind("test-rate-limit-claim")
    .fetch_one(&pool)
    .await
    .expect("count events");
    assert_eq!(event_count, 2, "should have 2 rate-limit events");

    drop_schema(&pool, &schema).await;
}

/// Verify that `cancel_classifier_rate_limit` correctly undoes a reservation.
///
/// This tests the pre-dispatch cancellation path: when request preparation
/// fails (e.g., image decoding error), the worker cancels the rate-limit
/// grant so the quota is not wasted.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_rate_limit_cancel() {
    let (pool, schema) = setup_test_schema("test_postgres_rate_limit_cancel", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Insert a downloaded image with local_path
    let image_id = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:00:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;
    sqlx::query(
        r#"UPDATE images SET local_path = '/tmp/test_cancel.jpg', local_file_identity = '/tmp/test_cancel.jpg'
           WHERE id = $1"#,
    )
    .bind(image_id)
    .execute(&pool)
    .await
    .expect("seed local_path");

    let limit = ClassifierRateLimitConfig {
        quota_group: "test-rate-limit-cancel".to_string(),
        requests_per_minute: 1,
        requests_per_day: 100,
        tokens_per_minute: 1000,
        tokens_per_day: 10000,
        estimated_input_tokens_per_request: 500,
        max_images_per_request: 5,
    };

    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));

    // Claim with rate limit - should succeed
    let claim = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("claim with rate limit");
    let grant = match claim {
        RateLimitedProcessingClaim::Claimed { grant, .. } => grant,
        other => panic!("expected Claimed, got {:?}", other),
    };

    // Verify the event was recorded
    let event_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM classifier_rate_limit_events WHERE id = $1")
            .bind(&grant.id)
            .fetch_one(&pool)
            .await
            .expect("count event");
    assert_eq!(event_count, 1);

    // Now cancel the grant (simulating pre-dispatch failure)
    let cancelled = ops
        .cancel_classifier_rate_limit(&grant)
        .await
        .expect("cancel rate limit");
    assert!(cancelled, "grant should be cancellable");

    // Verify the event was deleted
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM classifier_rate_limit_events WHERE id = $1 AND daily_refunded_at IS NULL",
    )
    .bind(&grant.id)
    .fetch_one(&pool)
    .await
    .expect("count event after cancel");
    assert_eq!(event_count, 0, "event should be deleted after cancel");

    // Verify daily usage was decremented
    let daily: Option<(i64, i64)> = sqlx::query_as(
        "SELECT requests, tokens FROM classifier_rate_limit_daily_usage WHERE quota_group = $1 AND day = $2",
    )
    .bind("test-rate-limit-cancel")
    .bind(now.as_datetime().date_naive().to_string())
    .fetch_optional(&pool)
    .await
    .expect("get daily usage");
    assert_eq!(daily, Some((0, 0)), "daily usage should be 0 after cancel");

    // Canceling again should return false (idempotent)
    let cancelled2 = ops
        .cancel_classifier_rate_limit(&grant)
        .await
        .expect("cancel again");
    assert!(!cancelled2, "second cancel should return false");

    // Cancellation only releases quota; the image remains in 'processing' state.
    // Insert a second eligible image to verify the quota was actually freed.
    let image_id_2 = insert_image(
        &pool,
        camera_id,
        "track-1",
        "2026-07-21T10:01:00.000000000Z",
        "downloaded",
        "new",
    )
    .await;
    sqlx::query(
        r#"UPDATE images SET local_path = '/tmp/test_cancel_2.jpg', local_file_identity = '/tmp/test_cancel_2.jpg'
           WHERE id = $1"#,
    )
    .bind(image_id_2)
    .execute(&pool)
    .await
    .expect("seed second local_path");

    // Now the second image should be reclaimable since quota was freed
    let claim2 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("re-claim after cancel");
    match claim2 {
        RateLimitedProcessingClaim::Claimed { claim, .. } => {
            assert_eq!(claim.image_id.get(), image_id_2);
        }
        other => panic!("expected Claimed after cancel, got {:?}", other),
    }

    drop_schema(&pool, &schema).await;
}

/// Verify that `refund_daily_classifier_rate_limit` correctly refunds
/// daily quota after an explicit provider rejection.
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_rate_limit_daily_refund() {
    let (pool, schema) = setup_test_schema("test_postgres_rate_limit_daily_refund", 4).await;
    let base_url = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base_url, &schema)), 4)
        .await
        .expect("create PostgresDataStore");
    let ops = store.ops();

    let camera_id = insert_camera(&pool, 1, "track-1").await;

    // Seed three eligible images so we can exhaust the daily budget of 2,
    // then refund one and verify the third image becomes claimable.
    for i in 1..=3 {
        let img_id = insert_image(
            &pool,
            camera_id,
            "track-1",
            &format!("2026-07-21T10:0{i}:00.000000000Z"),
            "downloaded",
            "new",
        )
        .await;
        sqlx::query(r#"UPDATE images SET local_path = $1, local_file_identity = $1 WHERE id = $2"#)
            .bind(format!("/tmp/test_refund_{i}.jpg"))
            .bind(img_id)
            .execute(&pool)
            .await
            .expect("seed local_path");
    }

    // Tight daily limit to test exhaustion and refund
    let limit = ClassifierRateLimitConfig {
        quota_group: "test-rate-limit-refund".to_string(),
        requests_per_minute: 100,
        requests_per_day: 2,
        tokens_per_minute: 10000,
        tokens_per_day: 2000,
        estimated_input_tokens_per_request: 500,
        max_images_per_request: 5,
    };

    let now = Timestamp::new(Utc::now());
    let lease = Timestamp::new((*now.as_datetime()) + chrono::Duration::seconds(300));
    let day = now.as_datetime().date_naive().to_string();

    // First claim - should succeed
    let claim1 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("first claim");
    let grant1 = match claim1 {
        RateLimitedProcessingClaim::Claimed { grant, .. } => grant,
        other => panic!("expected Claimed, got {:?}", other),
    };

    // Second claim - should succeed (within daily budget of 2)
    let claim2 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("second claim");
    let _grant2 = match claim2 {
        RateLimitedProcessingClaim::Claimed { grant, .. } => grant,
        other => panic!("expected Claimed, got {:?}", other),
    };

    // Third claim - should be DailyExhausted (daily budget of 2 exhausted)
    let claim3 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("third claim");
    assert!(
        matches!(claim3, RateLimitedProcessingClaim::DailyExhausted(_)),
        "third claim should be DailyExhausted"
    );

    // Verify daily usage shows 2 requests
    let daily: (i64, i64) = sqlx::query_as(
        "SELECT requests, tokens FROM classifier_rate_limit_daily_usage WHERE quota_group = $1 AND day = $2",
    )
    .bind("test-rate-limit-refund")
    .bind(&day)
    .fetch_one(&pool)
    .await
    .expect("get daily usage");
    assert_eq!(daily.0, 2, "daily requests should be 2");
    assert_eq!(daily.1, 2000, "daily tokens should be 2000");

    // Refund grant1 (simulating provider rejection)
    let refunded = ops
        .refund_daily_classifier_rate_limit(&grant1, &now)
        .await
        .expect("refund daily");
    assert!(refunded, "grant1 should be refundable");

    // Verify the event has daily_refunded_at set
    let refunded_at: Option<String> = sqlx::query_scalar(
        "SELECT daily_refunded_at FROM classifier_rate_limit_events WHERE id = $1",
    )
    .bind(&grant1.id)
    .fetch_optional(&pool)
    .await
    .expect("get refund timestamp");
    assert!(
        refunded_at.is_some(),
        "grant1 should have daily_refunded_at set"
    );

    // Verify daily usage is decremented
    let daily: (i64, i64) = sqlx::query_as(
        "SELECT requests, tokens FROM classifier_rate_limit_daily_usage WHERE quota_group = $1 AND day = $2",
    )
    .bind("test-rate-limit-refund")
    .bind(&day)
    .fetch_one(&pool)
    .await
    .expect("get daily usage after refund");
    assert_eq!(daily.0, 1, "daily requests should be 1 after refund");
    assert_eq!(daily.1, 1000, "daily tokens should be 1000 after refund");

    // The third image was already in the database and eligible, so it should now be claimable
    // since the daily budget was partially freed by the refund.
    let claim4 = ops
        .claim_next_processing_with_rate_limit(&limit, 500, &now, &lease)
        .await
        .expect("claim after refund");
    match claim4 {
        RateLimitedProcessingClaim::Claimed { claim, .. } => {
            // claim4 should be the third image (the one that was previously exhausted)
            assert!(claim.image_id.get() > 0, "should have claimed an image");
        }
        other => panic!("expected Claimed after refund, got {:?}", other),
    }

    // Refunding grant1 again should return false (idempotent)
    let refunded2 = ops
        .refund_daily_classifier_rate_limit(&grant1, &now)
        .await
        .expect("refund again");
    assert!(!refunded2, "second refund should return false");

    drop_schema(&pool, &schema).await;
}

#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn test_postgres_explorer_filters_sorts_and_buckets() {
    let (pool, schema) = setup_test_schema("explorer_filters_sorts_buckets", 4).await;
    let base = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let store = PostgresDataStore::connect(&Secret::new(schema_url(&base, &schema)), 4)
        .await
        .unwrap();
    let ops = store.ops();
    let camera = insert_camera(&pool, 1, "track-1").await;
    let mut ids = Vec::new();
    for seconds in ["00", "01", "02"] {
        let id = insert_image(
            &pool,
            camera,
            "track-1",
            &format!("2026-07-21T10:00:{seconds}.000000000Z"),
            "downloaded",
            "done",
        )
        .await;
        insert_classification(&pool, id, "vision", "v1", "2026-07-21T10:01:00.000000000Z").await;
        ids.push(id);
    }
    let mut filter = WebImageFilter {
        scope: WebScopeFilter {
            from: Some("2026-07-21T10:00:00Z".parse().unwrap()),
            to: Some("2026-07-21T10:00:03Z".parse().unwrap()),
            camera_ids: vec![camera],
        },
        download_status: None,
        processing_status: None,
        classified: None,
        contains_wildlife: Some(true),
        is_interesting: None,
        confidence_min: Some(0.9),
        advanced: WebAdvancedFilter {
            species: vec!["DEER".into()],
            model: Some("vision".into()),
            prompt_version: Some("v1".into()),
            text: Some("deer".into()),
            ..Default::default()
        },
    };
    assert_eq!(ops.web_overview(&filter).await.unwrap().discovered, 3);
    for order in [
        WebImageOrder::ConfidenceDescending,
        WebImageOrder::ClassifiedDescending,
        WebImageOrder::CameraAscending,
    ] {
        let mut query = WebImageQuery {
            filter: filter.clone(),
            order,
            limit: 1,
            cursor: None,
        };
        let mut found = Vec::new();
        loop {
            let (rows, cursor) = ops.web_query_images(&query).await.unwrap();
            found.extend(rows.into_iter().map(|i| i.id));
            if cursor.is_none() {
                break;
            }
            query.cursor = cursor;
            assert!(found.len() < 4);
        }
        found.sort_unstable();
        assert_eq!(found, ids);
    }
    let camera_counts = ops.web_camera_counts(&filter).await.unwrap();
    assert_eq!(camera_counts.len(), 1);
    assert_eq!(camera_counts[0].discovered, 3);
    assert_eq!(camera_counts[0].classified, 3);
    let buckets = ops.web_buckets(&filter, 300).await.unwrap();
    assert_eq!(buckets.len(), 1);
    assert_eq!(
        (
            buckets[0].discovered,
            buckets[0].downloaded,
            buckets[0].classified
        ),
        (3, 3, 3)
    );
    filter.advanced.time_field = "classified".into();
    assert_eq!(ops.web_overview(&filter).await.unwrap().discovered, 0);
    drop_schema(&pool, &schema).await;
}
