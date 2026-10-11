//! Durable token contract shared by SQLite and PostgreSQL.

use fauna_scan::authentication::{fingerprint, new_api_token};
use fauna_scan::database::repository::DatabaseOps;

async fn token_contract(ops: &DatabaseOps) {
    let never = new_api_token();
    let expiring = new_api_token();
    assert_ne!(never, expiring);
    let never_hash = fingerprint(&never);
    let expiring_hash = fingerprint(&expiring);
    assert!(
        ops.create_api_token("never ' स", &never_hash, None)
            .await
            .unwrap()
    );
    assert!(
        ops.create_api_token("expiring", &expiring_hash, Some(1000))
            .await
            .unwrap()
    );
    assert!(
        !ops.create_api_token("expiring", &fingerprint(&new_api_token()), None)
            .await
            .unwrap()
    );
    assert_eq!(
        ops.find_api_token(&expiring_hash, 999)
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        Some(1000)
    );
    assert!(
        ops.find_api_token(&expiring_hash, 1000)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ops.find_api_token(&never_hash, i64::MAX)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        ops.find_api_token(&fingerprint(&new_api_token()), 0)
            .await
            .unwrap()
            .is_none()
    );
    assert!(ops.find_api_token(&never, 0).await.unwrap().is_none());
    let metadata = ops.list_api_tokens().await.unwrap();
    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata[0].name, "expiring");
    assert_eq!(metadata[1].name, "never ' स");
    assert!(ops.revoke_api_token("never ' स").await.unwrap());
    assert!(!ops.revoke_api_token("never ' स").await.unwrap());
    assert!(ops.find_api_token(&never_hash, 0).await.unwrap().is_none());
    assert!(
        ops.find_api_token(&expiring_hash, 999)
            .await
            .unwrap()
            .is_some()
    );
    // A recreated name cannot bring back its previous secret.
    assert!(
        ops.create_api_token("never ' स", &fingerprint(&new_api_token()), None)
            .await
            .unwrap()
    );
    assert!(ops.find_api_token(&never_hash, 0).await.unwrap().is_none());
}

#[tokio::test]
async fn sqlite_api_tokens_are_durable_and_store_only_hashes() {
    use fauna_scan::database::sqlite::SqliteDataStore;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tokens.sqlite3");
    let store = SqliteDataStore::connect(&path, 1).await.unwrap();
    token_contract(&store.ops()).await;
    let token = new_api_token();
    let hash = fingerprint(&token);
    store
        .ops()
        .create_api_token("durable", &hash, None)
        .await
        .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT token_hash FROM api_tokens WHERE name = 'durable'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(stored, hash);
    assert_ne!(stored, token);
    store.pool().close().await;
    let reopened = SqliteDataStore::connect(&path, 1).await.unwrap();
    assert!(
        reopened
            .ops()
            .find_api_token(&hash, i64::MAX)
            .await
            .unwrap()
            .is_some()
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires FAUNA_SCAN_TEST_POSTGRES_URL"]
async fn postgres_api_token_contract() {
    use fauna_scan::configuration::Secret;
    use fauna_scan::database::postgres::PostgresDataStore;
    let base = std::env::var("FAUNA_SCAN_TEST_POSTGRES_URL").unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let schema = format!("fs_tokens_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    let mut url = url::Url::parse(&base).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("--search_path={schema}"));
    let store = PostgresDataStore::connect(&Secret::new(url.into()), 1)
        .await
        .unwrap();
    token_contract(&store.ops()).await;
    let token = new_api_token();
    let hash = fingerprint(&token);
    store
        .ops()
        .create_api_token("stored", &hash, None)
        .await
        .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT token_hash FROM api_tokens WHERE name = 'stored'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(stored, hash);
    assert_ne!(stored, token);
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
}
