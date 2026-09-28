use anyhow::{Context, Result, bail};
use clap::Parser;
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, SqlitePool, Transaction};
use sqlx::{
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

const CURRENT_SQLITE_MIGRATION: i64 = 9;
const TABLES: &[&str] = &[
    "cameras",
    "images",
    "classifications",
    "search_cursors",
    "service_metadata",
    "classifier_rate_limit_events",
    "classifier_rate_limit_daily_usage",
];

#[derive(Parser, Debug)]
#[command(about = "Migrate Fauna Scan state from SQLite to PostgreSQL")]
struct Args {
    #[arg(long)]
    sqlite_path: String,

    #[arg(long)]
    pg_url: String,

    #[arg(long, default_value_t = 1000, value_parser = parse_batch_size)]
    batch_size: usize,

    /// Validate the source and destination without copying rows.
    #[arg(long)]
    dry_run: bool,
}

fn parse_batch_size(value: &str) -> Result<usize, String> {
    let size = value
        .parse::<usize>()
        .map_err(|_| "batch size must be a positive integer".to_string())?;
    if size == 0 {
        return Err("batch size must be greater than zero".to_string());
    }
    Ok(size)
}

#[derive(Debug, FromRow)]
struct Camera {
    id: i64,
    channel_number: i64,
    primary_track_id: String,
    picture_track_id: String,
    name: Option<String>,
    raw_discovery_identifier: Option<String>,
    enabled: i64,
    first_seen_at: String,
    last_seen_at: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, FromRow)]
struct Image {
    id: i64,
    image_key: String,
    camera_id: i64,
    track_id: String,
    capture_start_at: String,
    capture_end_at: Option<String>,
    playback_uri: String,
    canonical_playback_uri: String,
    codec_type: Option<String>,
    content_type: Option<String>,
    nvr_reported_size: Option<i64>,
    local_path: Option<String>,
    download_status: String,
    download_attempts: i64,
    downloaded_at: Option<String>,
    download_last_error: Option<String>,
    download_next_attempt_at: Option<String>,
    download_lease_until: Option<String>,
    processing_status: String,
    processing_attempts: i64,
    processing_started_at: Option<String>,
    processing_completed_at: Option<String>,
    processing_last_error: Option<String>,
    processing_next_attempt_at: Option<String>,
    processing_lease_until: Option<String>,
    discovered_at: String,
    created_at: String,
    updated_at: String,
    processing_last_raw_response: Option<String>,
    processing_generation: i64,
    local_file_identity: Option<String>,
}

#[derive(Debug, FromRow)]
struct Classification {
    id: i64,
    image_id: i64,
    model: String,
    prompt_version: String,
    contains_wildlife: i64,
    is_interesting: i64,
    summary: Option<String>,
    species_json: Option<String>,
    confidence: Option<f64>,
    classification_json: Option<String>,
    bounding_boxes_json: Option<String>,
    raw_response: Option<String>,
    request_started_at: String,
    request_completed_at: String,
    created_at: String,
}

#[derive(Debug, FromRow)]
struct SearchCursor {
    camera_id: i64,
    next_search_at: Option<String>,
    last_completed_window_start: Option<String>,
    last_completed_window_end: Option<String>,
    last_poll_at: Option<String>,
    last_error: Option<String>,
    updated_at: String,
}

#[derive(Debug, FromRow)]
struct ServiceMetadata {
    key: String,
    value: String,
    updated_at: String,
}

#[derive(Debug, FromRow)]
struct RateLimitEvent {
    id: String,
    quota_group: String,
    reserved_at: String,
    token_cost: i64,
    daily_refunded_at: Option<String>,
}

#[derive(Debug, FromRow)]
struct DailyUsage {
    quota_group: String,
    day: String,
    requests: i64,
    tokens: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let sqlite = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&args.sqlite_path)
                .read_only(true)
                .foreign_keys(true),
        )
        .await
        .with_context(|| format!("opening SQLite database {}", args.sqlite_path))?;

    verify_sqlite_schema(&sqlite).await?;
    let source_counts = sqlite_counts(&sqlite).await?;

    let pg = PgPoolOptions::new()
        .max_connections(4)
        .connect(&args.pg_url)
        .await
        .context("connecting to PostgreSQL")?;

    sqlx::migrate!("./migrations-postgres")
        .run(&pg)
        .await
        .context("running PostgreSQL migrations")?;

    ensure_destination_is_empty(&pg).await?;
    println!("SQLite source counts: {source_counts:?}");

    if args.dry_run {
        println!("Dry run complete; no rows were copied.");
        return Ok(());
    }

    let mut tx = pg
        .begin()
        .await
        .context("starting PostgreSQL transaction")?;
    copy_cameras(&sqlite, &mut tx, args.batch_size).await?;
    copy_images(&sqlite, &mut tx, args.batch_size).await?;
    copy_classifications(&sqlite, &mut tx, args.batch_size).await?;
    copy_search_cursors(&sqlite, &mut tx).await?;
    copy_service_metadata(&sqlite, &mut tx).await?;
    copy_rate_limit_events(&sqlite, &mut tx, args.batch_size).await?;
    copy_daily_usage(&sqlite, &mut tx).await?;

    verify_foreign_keys(&mut tx).await?;
    reset_sequences(&mut tx).await?;
    tx.commit()
        .await
        .context("committing PostgreSQL transaction")?;

    let destination_counts = pg_counts(&pg).await?;
    if source_counts != destination_counts {
        bail!(
            "row-count verification failed: source={source_counts:?}, destination={destination_counts:?}"
        );
    }

    println!("Migration complete. PostgreSQL counts: {destination_counts:?}");
    Ok(())
}

async fn verify_sqlite_schema(sqlite: &SqlitePool) -> Result<()> {
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_one(sqlite)
    .await
    .context("reading SQLite migration version")?;
    if version != CURRENT_SQLITE_MIGRATION {
        bail!(
            "SQLite migration version is {version}; expected {CURRENT_SQLITE_MIGRATION}. Upgrade the SQLite deployment first."
        );
    }
    Ok(())
}

async fn ensure_destination_is_empty(pg: &PgPool) -> Result<()> {
    for table in TABLES {
        let query = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = sqlx::query_scalar(&query).fetch_one(pg).await?;
        if count != 0 {
            bail!("destination table {table} is not empty ({count} rows)");
        }
    }
    Ok(())
}

async fn copy_cameras(
    sqlite: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    batch_size: usize,
) -> Result<()> {
    let mut last_id = 0_i64;
    loop {
        let rows: Vec<Camera> =
            sqlx::query_as("SELECT * FROM cameras WHERE id > ? ORDER BY id LIMIT ?")
                .bind(last_id)
                .bind(batch_size as i64)
                .fetch_all(sqlite)
                .await
                .context("reading cameras from SQLite")?;
        if rows.is_empty() {
            break;
        }
        last_id = rows.last().expect("non-empty batch").id;
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO cameras (id, channel_number, primary_track_id, picture_track_id, name, raw_discovery_identifier, enabled, first_seen_at, last_seen_at, created_at, updated_at) ",
        );
        query.push_values(&rows, |mut b, row| {
            b.push_bind(row.id)
                .push_bind(row.channel_number)
                .push_bind(&row.primary_track_id)
                .push_bind(&row.picture_track_id)
                .push_bind(&row.name)
                .push_bind(&row.raw_discovery_identifier)
                .push_bind(row.enabled)
                .push_bind(&row.first_seen_at)
                .push_bind(&row.last_seen_at)
                .push_bind(&row.created_at)
                .push_bind(&row.updated_at);
        });
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn copy_images(
    sqlite: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    batch_size: usize,
) -> Result<()> {
    let mut last_id = 0_i64;
    loop {
        let rows: Vec<Image> =
            sqlx::query_as("SELECT * FROM images WHERE id > ? ORDER BY id LIMIT ?")
                .bind(last_id)
                .bind(batch_size as i64)
                .fetch_all(sqlite)
                .await
                .context("reading images from SQLite")?;
        if rows.is_empty() {
            break;
        }
        last_id = rows.last().expect("non-empty batch").id;
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO images (id, image_key, camera_id, track_id, capture_start_at, capture_end_at, playback_uri, canonical_playback_uri, codec_type, content_type, nvr_reported_size, local_path, download_status, download_attempts, downloaded_at, download_last_error, download_next_attempt_at, download_lease_until, processing_status, processing_attempts, processing_started_at, processing_completed_at, processing_last_error, processing_next_attempt_at, processing_lease_until, discovered_at, created_at, updated_at, processing_last_raw_response, processing_generation, local_file_identity) ",
        );
        query.push_values(&rows, |mut b, row| {
            b.push_bind(row.id)
                .push_bind(&row.image_key)
                .push_bind(row.camera_id)
                .push_bind(&row.track_id)
                .push_bind(&row.capture_start_at)
                .push_bind(&row.capture_end_at)
                .push_bind(&row.playback_uri)
                .push_bind(&row.canonical_playback_uri)
                .push_bind(&row.codec_type)
                .push_bind(&row.content_type)
                .push_bind(row.nvr_reported_size)
                .push_bind(&row.local_path)
                .push_bind(&row.download_status)
                .push_bind(row.download_attempts)
                .push_bind(&row.downloaded_at)
                .push_bind(&row.download_last_error)
                .push_bind(&row.download_next_attempt_at)
                .push_bind(&row.download_lease_until)
                .push_bind(&row.processing_status)
                .push_bind(row.processing_attempts)
                .push_bind(&row.processing_started_at)
                .push_bind(&row.processing_completed_at)
                .push_bind(&row.processing_last_error)
                .push_bind(&row.processing_next_attempt_at)
                .push_bind(&row.processing_lease_until)
                .push_bind(&row.discovered_at)
                .push_bind(&row.created_at)
                .push_bind(&row.updated_at)
                .push_bind(&row.processing_last_raw_response)
                .push_bind(row.processing_generation)
                .push_bind(&row.local_file_identity);
        });
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn copy_classifications(
    sqlite: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    batch_size: usize,
) -> Result<()> {
    let mut last_id = 0_i64;
    loop {
        let rows: Vec<Classification> =
            sqlx::query_as("SELECT * FROM classifications WHERE id > ? ORDER BY id LIMIT ?")
                .bind(last_id)
                .bind(batch_size as i64)
                .fetch_all(sqlite)
                .await
                .context("reading classifications from SQLite")?;
        if rows.is_empty() {
            break;
        }
        last_id = rows.last().expect("non-empty batch").id;
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO classifications (id, image_id, model, prompt_version, contains_wildlife, is_interesting, summary, species_json, confidence, classification_json, raw_response, request_started_at, request_completed_at, created_at, bounding_boxes_json) ",
        );
        query.push_values(&rows, |mut b, row| {
            b.push_bind(row.id)
                .push_bind(row.image_id)
                .push_bind(&row.model)
                .push_bind(&row.prompt_version)
                .push_bind(row.contains_wildlife)
                .push_bind(row.is_interesting)
                .push_bind(&row.summary)
                .push_bind(&row.species_json)
                .push_bind(row.confidence)
                .push_bind(&row.classification_json)
                .push_bind(&row.raw_response)
                .push_bind(&row.request_started_at)
                .push_bind(&row.request_completed_at)
                .push_bind(&row.created_at)
                .push_bind(&row.bounding_boxes_json);
        });
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn copy_search_cursors(
    sqlite: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<()> {
    let rows: Vec<SearchCursor> = sqlx::query_as("SELECT * FROM search_cursors ORDER BY camera_id")
        .fetch_all(sqlite)
        .await
        .context("reading search_cursors")?;
    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO search_cursors (camera_id, next_search_at, last_completed_window_start, last_completed_window_end, last_poll_at, last_error, updated_at) ",
    );
    query.push_values(&rows, |mut b, row| {
        b.push_bind(row.camera_id)
            .push_bind(&row.next_search_at)
            .push_bind(&row.last_completed_window_start)
            .push_bind(&row.last_completed_window_end)
            .push_bind(&row.last_poll_at)
            .push_bind(&row.last_error)
            .push_bind(&row.updated_at);
    });
    if !rows.is_empty() {
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn copy_service_metadata(
    sqlite: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<()> {
    let rows: Vec<ServiceMetadata> = sqlx::query_as("SELECT * FROM service_metadata ORDER BY key")
        .fetch_all(sqlite)
        .await
        .context("reading service_metadata")?;
    let mut query =
        QueryBuilder::<Postgres>::new("INSERT INTO service_metadata (key, value, updated_at) ");
    query.push_values(&rows, |mut b, row| {
        b.push_bind(&row.key)
            .push_bind(&row.value)
            .push_bind(&row.updated_at);
    });
    if !rows.is_empty() {
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn copy_rate_limit_events(
    sqlite: &SqlitePool,
    tx: &mut Transaction<'_, Postgres>,
    batch_size: usize,
) -> Result<()> {
    let mut offset = 0_i64;
    loop {
        let rows: Vec<RateLimitEvent> = sqlx::query_as(
            "SELECT * FROM classifier_rate_limit_events ORDER BY id LIMIT ? OFFSET ?",
        )
        .bind(batch_size as i64)
        .bind(offset)
        .fetch_all(sqlite)
        .await
        .context("reading classifier_rate_limit_events")?;
        if rows.is_empty() {
            break;
        }
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO classifier_rate_limit_events (id, quota_group, reserved_at, token_cost, daily_refunded_at) ",
        );
        query.push_values(&rows, |mut b, row| {
            b.push_bind(&row.id)
                .push_bind(&row.quota_group)
                .push_bind(&row.reserved_at)
                .push_bind(row.token_cost)
                .push_bind(&row.daily_refunded_at);
        });
        query.build().execute(&mut **tx).await?;
        offset += rows.len() as i64;
    }
    Ok(())
}

async fn copy_daily_usage(sqlite: &SqlitePool, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let rows: Vec<DailyUsage> =
        sqlx::query_as("SELECT * FROM classifier_rate_limit_daily_usage ORDER BY quota_group, day")
            .fetch_all(sqlite)
            .await
            .context("reading classifier_rate_limit_daily_usage")?;
    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO classifier_rate_limit_daily_usage (quota_group, day, requests, tokens) ",
    );
    query.push_values(&rows, |mut b, row| {
        b.push_bind(&row.quota_group)
            .push_bind(&row.day)
            .push_bind(row.requests)
            .push_bind(row.tokens);
    });
    if !rows.is_empty() {
        query.build().execute(&mut **tx).await?;
    }
    Ok(())
}

async fn verify_foreign_keys(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    let orphan_images: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM images i LEFT JOIN cameras c ON c.id = i.camera_id WHERE c.id IS NULL")
        .fetch_one(&mut **tx).await?;
    let orphan_classifications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM classifications c LEFT JOIN images i ON i.id = c.image_id WHERE i.id IS NULL")
        .fetch_one(&mut **tx).await?;
    if orphan_images != 0 || orphan_classifications != 0 {
        bail!(
            "foreign-key verification failed: {orphan_images} orphan images, {orphan_classifications} orphan classifications"
        );
    }
    Ok(())
}

async fn reset_sequences(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    for table in ["cameras", "images", "classifications"] {
        let query = format!(
            "SELECT setval(pg_get_serial_sequence('{table}', 'id'), COALESCE(MAX(id), 1), MAX(id) IS NOT NULL) FROM {table}"
        );
        sqlx::query(&query).execute(&mut **tx).await?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Counts([i64; 7]);

async fn sqlite_counts(sqlite: &SqlitePool) -> Result<Counts> {
    Ok(Counts([
        sqlx::query_scalar("SELECT COUNT(*) FROM cameras")
            .fetch_one(sqlite)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM images")
            .fetch_one(sqlite)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM classifications")
            .fetch_one(sqlite)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM search_cursors")
            .fetch_one(sqlite)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM service_metadata")
            .fetch_one(sqlite)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM classifier_rate_limit_events")
            .fetch_one(sqlite)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM classifier_rate_limit_daily_usage")
            .fetch_one(sqlite)
            .await?,
    ]))
}

async fn pg_counts(pg: &PgPool) -> Result<Counts> {
    Ok(Counts([
        sqlx::query_scalar("SELECT COUNT(*) FROM cameras")
            .fetch_one(pg)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM images")
            .fetch_one(pg)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM classifications")
            .fetch_one(pg)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM search_cursors")
            .fetch_one(pg)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM service_metadata")
            .fetch_one(pg)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM classifier_rate_limit_events")
            .fetch_one(pg)
            .await?,
        sqlx::query_scalar("SELECT COUNT(*) FROM classifier_rate_limit_daily_usage")
            .fetch_one(pg)
            .await?,
    ]))
}
