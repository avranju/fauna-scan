//! Axum-powered read-only web interface and JSON API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{QueryBuilder, Row, Sqlite};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use url::Url;

use crate::configuration::{Config, NvrConfig, WebConfig};
use crate::database::repository::DatabaseOps;
use crate::domain::{ImageId, Timestamp, TrackId};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::{NvrTransport, RecordingSearchClient};
use crate::service_lifecycle::ShutdownToken;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_CSS: &str = include_str!("assets/app.css");
const APP_JS: &str = include_str!("assets/app.js");

#[derive(Clone)]
pub struct WebState {
    ops: DatabaseOps,
    output_directory: PathBuf,
    web: WebConfig,
    nvr: NvrConfig,
    recording_search: Arc<RecordingSearchClient>,
    started_at: Timestamp,
}

impl WebState {
    pub fn from_config(ops: DatabaseOps, config: &Config, transport: Arc<NvrTransport>) -> Self {
        Self {
            ops,
            output_directory: config.general.output_directory.clone(),
            web: config.web.clone(),
            nvr: config.nvr.clone(),
            recording_search: Arc::new(RecordingSearchClient::new(
                transport,
                config.nvr.search.max_results,
            )),
            started_at: Timestamp::new(Utc::now()),
        }
    }
}

/// Serve the UI until the shared service shutdown token is cancelled.
pub async fn serve(state: WebState, shutdown: ShutdownToken) -> AppResult<()> {
    let listen_address = state.web.listen_address;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen_address)
        .await
        .map_err(|error| {
            AppError::with_source(
                ErrorCategory::Configuration,
                "web_bind",
                format!("failed to bind web interface on {listen_address}"),
                error,
            )
        })?;
    tracing::info!(address = %listen_address, "Web interface listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .map_err(|error| {
            AppError::with_source(
                ErrorCategory::Network,
                "web_serve",
                "web interface server failed",
                error,
            )
        })
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/images", get(index))
        .route("/images/{id}", get(index))
        .route("/activity", get(index))
        .route("/about", get(index))
        .route("/assets/app.css", get(css))
        .route("/assets/app.js", get(javascript))
        .route("/api/v1/config", get(api_config))
        .route("/api/v1/health", get(api_health))
        .route("/api/v1/cameras", get(api_cameras))
        .route("/api/v1/overview", get(api_overview))
        .route("/api/v1/images", get(api_images))
        .route("/api/v1/images/{id}", get(api_image))
        .route("/api/v1/images/{id}/content", get(api_image_content))
        .route("/api/v1/images/{id}/thumbnail", get(api_image_content))
        .route("/api/v1/images/{id}/recording", get(api_recording))
        .route("/api/v1/activity", get(api_activity))
        .fallback(not_found)
        .with_state(state)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

async fn index() -> Response {
    let mut response = Html(INDEX_HTML).into_response();
    security_headers(response.headers_mut());
    response
}

async fn css() -> Response {
    static_asset(APP_CSS, "text/css; charset=utf-8")
}

async fn javascript() -> Response {
    static_asset(APP_JS, "text/javascript; charset=utf-8")
}

fn static_asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    security_headers(response.headers_mut());
    response
}

fn security_headers(headers: &mut HeaderMap) {
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
}

async fn not_found() -> WebError {
    WebError::not_found("The requested resource does not exist")
}

#[derive(Debug)]
struct WebError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl WebError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }
}

impl From<AppError> for WebError {
    fn from(error: AppError) -> Self {
        let status = match error.category {
            ErrorCategory::Authentication => StatusCode::UNAUTHORIZED,
            ErrorCategory::Authorization => StatusCode::FORBIDDEN,
            ErrorCategory::Configuration => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCategory::Network | ErrorCategory::Timeout => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCategory::Protocol
            | ErrorCategory::XmlParsing
            | ErrorCategory::InvalidNvrResponse => StatusCode::BAD_GATEWAY,
            ErrorCategory::PlaybackUnavailable => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            code: "request_failed",
            message: error.message,
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": { "code": self.code, "message": self.message }
            })),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ConfigResponse {
    version: &'static str,
    generated_at: String,
    default_clip_pre_roll_seconds: u64,
    default_clip_post_roll_seconds: u64,
    maximum_clip_duration_seconds: u64,
    capabilities: Value,
}

async fn api_config(State(state): State<WebState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        version: env!("CARGO_PKG_VERSION"),
        generated_at: Timestamp::new(Utc::now()).to_string(),
        default_clip_pre_roll_seconds: state.web.clip_pre_roll_seconds,
        default_clip_post_roll_seconds: state.web.clip_post_roll_seconds,
        maximum_clip_duration_seconds: state.web.maximum_clip_duration_seconds,
        capabilities: json!({
            "nvr_still_url": true,
            "nvr_recording_lookup": true,
            "browser_clip_playback": false,
            "clip_download": false
        }),
    })
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    web_started_at: String,
    last_camera_discovery: Option<String>,
    last_downloader_poll: Option<String>,
    last_scanner_pass: Option<String>,
    active_downloads: i64,
    active_classifications: i64,
    generated_at: String,
}

async fn api_health(State(state): State<WebState>) -> Result<Json<HealthResponse>, WebError> {
    let rows = sqlx::query("SELECT key, value FROM service_metadata")
        .fetch_all(state.ops.pool())
        .await
        .map_err(db_error)?;
    let mut metadata = BTreeMap::new();
    for row in rows {
        metadata.insert(row.get::<String, _>("key"), row.get::<String, _>("value"));
    }
    let (active_downloads, active_classifications): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE download_status = 'downloading'), COUNT(*) FILTER (WHERE processing_status = 'processing') FROM images",
    )
    .fetch_one(state.ops.pool())
    .await
    .map_err(db_error)?;
    Ok(Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        web_started_at: state.started_at.to_string(),
        last_camera_discovery: metadata.remove("last_successful_camera_discovery"),
        last_downloader_poll: metadata.remove("last_successful_downloader_poll"),
        last_scanner_pass: metadata.remove("last_successful_scanner_pass"),
        active_downloads,
        active_classifications,
        generated_at: Timestamp::new(Utc::now()).to_string(),
    }))
}

#[derive(Serialize)]
struct CameraDto {
    id: i64,
    channel_number: i64,
    name: Option<String>,
    enabled: bool,
    primary_track_id: String,
    picture_track_id: String,
    last_seen_at: String,
    last_completed_window_end: Option<String>,
    last_poll_at: Option<String>,
    next_search_at: Option<String>,
    last_error: Option<String>,
}

async fn api_cameras(State(state): State<WebState>) -> Result<Json<Value>, WebError> {
    let rows = sqlx::query(
        r#"SELECT c.id, c.channel_number, c.name, c.enabled, c.primary_track_id,
                  c.picture_track_id, c.last_seen_at, s.last_completed_window_end,
                  s.last_poll_at, s.next_search_at, s.last_error
             FROM cameras c
             LEFT JOIN search_cursors s ON s.camera_id = c.id
             ORDER BY c.enabled DESC, c.channel_number ASC, c.id ASC"#,
    )
    .fetch_all(state.ops.pool())
    .await
    .map_err(db_error)?;
    let data = rows
        .into_iter()
        .map(|row| CameraDto {
            id: row.get("id"),
            channel_number: row.get("channel_number"),
            name: row.get("name"),
            enabled: row.get::<i64, _>("enabled") != 0,
            primary_track_id: row.get("primary_track_id"),
            picture_track_id: row.get("picture_track_id"),
            last_seen_at: row.get("last_seen_at"),
            last_completed_window_end: row.get("last_completed_window_end"),
            last_poll_at: row.get("last_poll_at"),
            next_search_at: row.get("next_search_at"),
            last_error: row.get("last_error"),
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({ "data": data, "generated_at": Timestamp::new(Utc::now()).to_string() }),
    ))
}

#[derive(Debug, Default, Deserialize, Clone)]
struct ImageParams {
    from: Option<String>,
    to: Option<String>,
    camera: Option<String>,
    download_status: Option<String>,
    processing_status: Option<String>,
    contains_wildlife: Option<bool>,
    interesting: Option<bool>,
    classified: Option<bool>,
    confidence_min: Option<f64>,
    sort: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

#[derive(Serialize)]
struct CameraSummary {
    id: i64,
    name: Option<String>,
    channel: i64,
}

#[derive(Serialize)]
struct ClassificationSummary {
    id: i64,
    contains_wildlife: bool,
    interesting: bool,
    summary: Option<String>,
    species: Value,
    confidence: Option<f64>,
    model: String,
    prompt_version: String,
    completed_at: String,
}

#[derive(Serialize)]
struct ImageSummary {
    id: i64,
    captured_at: String,
    capture_end_at: Option<String>,
    camera: CameraSummary,
    content_url: Option<String>,
    thumbnail_url: Option<String>,
    download_status: String,
    processing_status: String,
    classification: Option<ClassificationSummary>,
}

async fn api_images(
    State(state): State<WebState>,
    Query(params): Query<ImageParams>,
) -> Result<Json<Value>, WebError> {
    let (data, next_cursor) = query_images(&state, &params).await?;
    Ok(Json(json!({
        "data": data,
        "page": { "next_cursor": next_cursor, "has_more": next_cursor.is_some() },
        "generated_at": Timestamp::new(Utc::now()).to_string()
    })))
}

async fn query_images(
    state: &WebState,
    params: &ImageParams,
) -> Result<(Vec<ImageSummary>, Option<String>), WebError> {
    validate_time_range(params)?;
    let ascending = match params.sort.as_deref().unwrap_or("captured_desc") {
        "captured_desc" => false,
        "captured_asc" => true,
        _ => return Err(WebError::bad_request("unsupported image sort")),
    };
    let limit = params.limit.unwrap_or(60).clamp(1, 200) as i64;
    let cursor = params.cursor.as_deref().map(decode_cursor).transpose()?;

    let mut builder = QueryBuilder::<Sqlite>::new(
        r#"WITH ranked_classifications AS (
               SELECT cl.*, ROW_NUMBER() OVER (
                   PARTITION BY cl.image_id
                   ORDER BY cl.request_completed_at DESC, cl.id DESC
               ) AS rn
               FROM classifications cl
           )
           SELECT i.id, i.capture_start_at, i.capture_end_at,
                  i.camera_id, c.name AS camera_name, c.channel_number,
                  i.local_path, i.download_status, i.processing_status,
                  lc.id AS classification_id, lc.contains_wildlife,
                  lc.is_interesting, lc.summary, lc.species_json,
                  lc.confidence, lc.model, lc.prompt_version,
                  lc.request_completed_at
             FROM images i
             JOIN cameras c ON c.id = i.camera_id
             LEFT JOIN ranked_classifications lc ON lc.image_id = i.id AND lc.rn = 1
            WHERE 1 = 1"#,
    );
    append_image_filters(&mut builder, params)?;
    if let Some((timestamp, id)) = cursor {
        if ascending {
            builder.push(" AND (i.capture_start_at > ");
        } else {
            builder.push(" AND (i.capture_start_at < ");
        }
        builder.push_bind(timestamp.clone());
        builder.push(" OR (i.capture_start_at = ");
        builder.push_bind(timestamp);
        if ascending {
            builder.push(" AND i.id > ");
        } else {
            builder.push(" AND i.id < ");
        }
        builder.push_bind(id);
        builder.push("))");
    }
    builder.push(if ascending {
        " ORDER BY i.capture_start_at ASC, i.id ASC LIMIT "
    } else {
        " ORDER BY i.capture_start_at DESC, i.id DESC LIMIT "
    });
    builder.push_bind(limit + 1);

    let rows = builder
        .build()
        .fetch_all(state.ops.pool())
        .await
        .map_err(db_error)?;
    let has_more = rows.len() as i64 > limit;
    let mut data = rows
        .into_iter()
        .take(limit as usize)
        .map(row_to_image_summary)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if has_more {
        data.last()
            .map(|item| encode_cursor(&item.captured_at, item.id))
    } else {
        None
    };
    Ok((std::mem::take(&mut data), next_cursor))
}

fn append_image_filters(
    builder: &mut QueryBuilder<'_, Sqlite>,
    params: &ImageParams,
) -> Result<(), WebError> {
    append_scope_filters(builder, params)?;
    append_string_list(
        builder,
        "i.download_status",
        params.download_status.as_deref(),
        &[
            "pending",
            "downloading",
            "downloaded",
            "retry_wait",
            "unavailable",
            "failed",
        ],
    )?;
    append_string_list(
        builder,
        "i.processing_status",
        params.processing_status.as_deref(),
        &[
            "new",
            "processing",
            "done",
            "retry_wait",
            "failed",
            "missing",
        ],
    )?;
    if let Some(value) = params.classified {
        builder.push(if value {
            " AND lc.id IS NOT NULL"
        } else {
            " AND lc.id IS NULL"
        });
    }
    if let Some(value) = params.contains_wildlife {
        builder
            .push(" AND lc.contains_wildlife = ")
            .push_bind(i64::from(value));
    }
    if let Some(value) = params.interesting {
        builder
            .push(" AND lc.is_interesting = ")
            .push_bind(i64::from(value));
    }
    if let Some(value) = params.confidence_min {
        if !(0.0..=1.0).contains(&value) {
            return Err(WebError::bad_request(
                "confidence_min must be between 0 and 1",
            ));
        }
        builder.push(" AND lc.confidence >= ").push_bind(value);
    }
    Ok(())
}

fn append_scope_filters(
    builder: &mut QueryBuilder<'_, Sqlite>,
    params: &ImageParams,
) -> Result<(), WebError> {
    if let Some(from) = &params.from {
        let from = crate::database::format_timestamp(&parse_timestamp(from, "from")?);
        builder.push(" AND i.capture_start_at >= ").push_bind(from);
    }
    if let Some(to) = &params.to {
        let to = crate::database::format_timestamp(&parse_timestamp(to, "to")?);
        builder.push(" AND i.capture_start_at < ").push_bind(to);
    }
    append_i64_list(builder, "i.camera_id", params.camera.as_deref())?;
    Ok(())
}

fn append_i64_list(
    builder: &mut QueryBuilder<'_, Sqlite>,
    column: &'static str,
    raw: Option<&str>,
) -> Result<(), WebError> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let values = raw
        .split(',')
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| WebError::bad_request("camera contains an invalid ID"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    builder.push(" AND ").push(column).push(" IN (");
    let mut separated = builder.separated(", ");
    for value in values {
        separated.push_bind(value);
    }
    separated.push_unseparated(")");
    Ok(())
}

fn append_string_list(
    builder: &mut QueryBuilder<'_, Sqlite>,
    column: &'static str,
    raw: Option<&str>,
    allowed: &[&str],
) -> Result<(), WebError> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let values = raw.split(',').collect::<Vec<_>>();
    if values.iter().any(|value| !allowed.contains(value)) {
        return Err(WebError::bad_request("filter contains an unknown status"));
    }
    builder.push(" AND ").push(column).push(" IN (");
    let mut separated = builder.separated(", ");
    for value in values {
        separated.push_bind(value.to_string());
    }
    separated.push_unseparated(")");
    Ok(())
}

fn row_to_image_summary(row: sqlx::sqlite::SqliteRow) -> Result<ImageSummary, WebError> {
    let id: i64 = row.get("id");
    let downloaded = row.get::<String, _>("download_status") == "downloaded";
    let has_path = row.get::<Option<String>, _>("local_path").is_some();
    let classification_id: Option<i64> = row.get("classification_id");
    let classification = classification_id.map(|classification_id| ClassificationSummary {
        id: classification_id,
        contains_wildlife: row.get::<i64, _>("contains_wildlife") != 0,
        interesting: row.get::<i64, _>("is_interesting") != 0,
        summary: row.get("summary"),
        species: parse_json_value(row.get::<Option<String>, _>("species_json"), json!([])),
        confidence: row.get("confidence"),
        model: row.get("model"),
        prompt_version: row.get("prompt_version"),
        completed_at: row.get("request_completed_at"),
    });
    Ok(ImageSummary {
        id,
        captured_at: row.get("capture_start_at"),
        capture_end_at: row.get("capture_end_at"),
        camera: CameraSummary {
            id: row.get("camera_id"),
            name: row.get("camera_name"),
            channel: row.get("channel_number"),
        },
        content_url: (downloaded && has_path).then(|| format!("/api/v1/images/{id}/content")),
        thumbnail_url: (downloaded && has_path).then(|| format!("/api/v1/images/{id}/thumbnail")),
        download_status: row.get("download_status"),
        processing_status: row.get("processing_status"),
        classification,
    })
}

async fn api_overview(
    State(state): State<WebState>,
    Query(params): Query<ImageParams>,
) -> Result<Json<Value>, WebError> {
    validate_time_range(&params)?;
    let mut builder = QueryBuilder::<Sqlite>::new(
        r#"WITH ranked_classifications AS (
               SELECT cl.*, ROW_NUMBER() OVER (
                   PARTITION BY cl.image_id ORDER BY cl.request_completed_at DESC, cl.id DESC
               ) AS rn FROM classifications cl
           )
           SELECT COUNT(*) AS discovered,
                  COALESCE(SUM(i.download_status = 'downloaded'), 0) AS downloaded,
                  COALESCE(SUM(i.processing_status = 'done'), 0) AS classified,
                  COALESCE(SUM(lc.contains_wildlife = 1), 0) AS wildlife,
                  COALESCE(SUM(lc.is_interesting = 1), 0) AS interesting,
                  COALESCE(SUM(i.download_status = 'retry_wait'), 0)
                    + COALESCE(SUM(i.processing_status = 'retry_wait'), 0) AS retryable,
                  COALESCE(SUM(i.download_status IN ('unavailable','failed')), 0)
                    + COALESCE(SUM(i.processing_status IN ('failed','missing')), 0) AS permanent
             FROM images i
             JOIN cameras c ON c.id = i.camera_id
             LEFT JOIN ranked_classifications lc ON lc.image_id = i.id AND lc.rn = 1
            WHERE 1 = 1"#,
    );
    append_image_filters(&mut builder, &params)?;
    let row = builder
        .build()
        .fetch_one(state.ops.pool())
        .await
        .map_err(db_error)?;
    Ok(Json(json!({
        "counts": {
            "discovered": row.get::<i64, _>("discovered"),
            "downloaded": row.get::<i64, _>("downloaded"),
            "classified": row.get::<i64, _>("classified"),
            "wildlife": row.get::<i64, _>("wildlife"),
            "interesting": row.get::<i64, _>("interesting"),
            "retryable_failures": row.get::<i64, _>("retryable"),
            "permanent_failures": row.get::<i64, _>("permanent")
        },
        "generated_at": Timestamp::new(Utc::now()).to_string()
    })))
}

async fn api_image(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
) -> Result<Json<Value>, WebError> {
    let image = state
        .ops
        .get_image(ImageId::new(id))
        .await
        .map_err(|error| match error.message.as_str() {
            "row not found" => WebError::not_found("Image not found"),
            _ => error.into(),
        })?;
    let camera = sqlx::query(
        "SELECT id, channel_number, name, primary_track_id, picture_track_id FROM cameras WHERE id = ?",
    )
    .bind(image.camera_id.get())
    .fetch_one(state.ops.pool())
    .await
    .map_err(db_error)?;
    let classification_rows = sqlx::query(
        r#"SELECT id, model, prompt_version, contains_wildlife, is_interesting,
                  summary, species_json, confidence, classification_json,
                  request_started_at, request_completed_at, created_at
             FROM classifications WHERE image_id = ?
             ORDER BY request_completed_at DESC, id DESC"#,
    )
    .bind(id)
    .fetch_all(state.ops.pool())
    .await
    .map_err(db_error)?;
    let classifications = classification_rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.get::<i64, _>("id"),
                "model": row.get::<String, _>("model"),
                "prompt_version": row.get::<String, _>("prompt_version"),
                "contains_wildlife": row.get::<i64, _>("contains_wildlife") != 0,
                "interesting": row.get::<i64, _>("is_interesting") != 0,
                "summary": row.get::<Option<String>, _>("summary"),
                "species": parse_json_value(row.get::<Option<String>, _>("species_json"), json!([])),
                "confidence": row.get::<Option<f64>, _>("confidence"),
                "structured": parse_json_value(row.get::<Option<String>, _>("classification_json"), Value::Null),
                "request_started_at": row.get::<String, _>("request_started_at"),
                "request_completed_at": row.get::<String, _>("request_completed_at"),
                "created_at": row.get::<String, _>("created_at")
            })
        })
        .collect::<Vec<_>>();
    let canonical_nvr_url = validated_still_url(&image.canonical_playback_uri, &state.nvr);
    let reported_nvr_url = validated_still_url(&image.playback_uri, &state.nvr);
    Ok(Json(json!({
        "id": id,
        "image_key": image.image_key.as_str(),
        "captured_at": image.capture_start_at.to_string(),
        "capture_end_at": image.capture_end_at.map(|value| value.to_string()),
        "discovered_at": image.discovered_at.to_string(),
        "camera": {
            "id": camera.get::<i64, _>("id"),
            "channel": camera.get::<i64, _>("channel_number"),
            "name": camera.get::<Option<String>, _>("name"),
            "primary_track_id": camera.get::<String, _>("primary_track_id"),
            "picture_track_id": camera.get::<String, _>("picture_track_id")
        },
        "content_url": (image.local_path.is_some() && image.download_status.as_str() == "downloaded")
            .then(|| format!("/api/v1/images/{id}/content")),
        "download": {
            "status": image.download_status.as_str(),
            "attempts": image.download_attempts,
            "downloaded_at": image.downloaded_at.map(|value| value.to_string()),
            "last_error": image.download_last_error,
            "next_attempt_at": image.download_next_attempt_at.map(|value| value.to_string()),
            "lease_until": image.download_lease_until.map(|value| value.to_string())
        },
        "processing": {
            "status": image.processing_status.as_str(),
            "attempts": image.processing_attempts,
            "started_at": image.processing_started_at.map(|value| value.to_string()),
            "completed_at": image.processing_completed_at.map(|value| value.to_string()),
            "last_error": image.processing_last_error,
            "next_attempt_at": image.processing_next_attempt_at.map(|value| value.to_string()),
            "lease_until": image.processing_lease_until.map(|value| value.to_string())
        },
        "nvr": {
            "image_url": canonical_nvr_url,
            "reported_image_url": reported_nvr_url
        },
        "classifications": classifications
    })))
}

async fn api_image_content(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
) -> Result<Response, WebError> {
    let row = sqlx::query("SELECT local_path, download_status FROM images WHERE id = ?")
        .bind(id)
        .fetch_optional(state.ops.pool())
        .await
        .map_err(db_error)?
        .ok_or_else(|| WebError::not_found("Image not found"))?;
    if row.get::<String, _>("download_status") != "downloaded" {
        return Err(WebError::not_found("Image content has not been downloaded"));
    }
    let path: String = row
        .get::<Option<String>, _>("local_path")
        .ok_or_else(|| WebError::not_found("Local image file is unavailable"))?;
    let root = tokio::fs::canonicalize(&state.output_directory)
        .await
        .map_err(|_| WebError::not_found("Image directory is unavailable"))?;
    let path = tokio::fs::canonicalize(Path::new(&path))
        .await
        .map_err(|_| WebError::not_found("Local image file is missing"))?;
    if !path.starts_with(&root) {
        return Err(WebError {
            status: StatusCode::FORBIDDEN,
            code: "unsafe_image_path",
            message: "The image path is outside the configured output directory".into(),
        });
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| WebError::not_found("Local image file could not be read"))?;
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(WebError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_image",
            message: "The local file is not a valid JPEG".into(),
        });
    }
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"fauna-scan-{id}.jpg\""))
            .unwrap_or_else(|_| HeaderValue::from_static("inline")),
    );
    security_headers(response.headers_mut());
    Ok(response)
}

#[derive(Deserialize)]
struct RecordingParams {
    pre_roll_seconds: Option<u64>,
    post_roll_seconds: Option<u64>,
}

async fn api_recording(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
    Query(params): Query<RecordingParams>,
) -> Result<Json<Value>, WebError> {
    let pre = params
        .pre_roll_seconds
        .unwrap_or(state.web.clip_pre_roll_seconds);
    let post = params
        .post_roll_seconds
        .unwrap_or(state.web.clip_post_roll_seconds);
    let duration = pre
        .checked_add(post)
        .ok_or_else(|| WebError::bad_request("clip interval is too large"))?;
    if duration == 0 || duration > state.web.maximum_clip_duration_seconds {
        return Err(WebError::bad_request(format!(
            "clip interval must be between 1 and {} seconds",
            state.web.maximum_clip_duration_seconds
        )));
    }
    let row = sqlx::query(
        r#"SELECT i.capture_start_at, c.primary_track_id
             FROM images i JOIN cameras c ON c.id = i.camera_id
            WHERE i.id = ?"#,
    )
    .bind(id)
    .fetch_optional(state.ops.pool())
    .await
    .map_err(db_error)?
    .ok_or_else(|| WebError::not_found("Image not found"))?;
    let target = row
        .get::<String, _>("capture_start_at")
        .parse::<Timestamp>()
        .map_err(|_| WebError::bad_request("image capture timestamp is invalid"))?;
    let start = Timestamp::new(
        target
            .as_datetime()
            .checked_sub_signed(Duration::seconds(pre as i64))
            .ok_or_else(|| WebError::bad_request("clip start is out of range"))?,
    );
    let end = Timestamp::new(
        target
            .as_datetime()
            .checked_add_signed(Duration::seconds(post as i64))
            .ok_or_else(|| WebError::bad_request("clip end is out of range"))?,
    );
    let track = TrackId::new(row.get::<String, _>("primary_track_id"));
    let found = state
        .recording_search
        .find(&track, start, end, target)
        .await?;
    let Some(recording) = found else {
        return Ok(Json(json!({
            "status": "not_found",
            "requested_start_at": start.to_string(),
            "requested_end_at": end.to_string()
        })));
    };
    let playback_uri = validate_recording_uri(&recording.playback_uri, &state.nvr)?;
    Ok(Json(json!({
        "status": "found",
        "requested_start_at": start.to_string(),
        "requested_end_at": end.to_string(),
        "recording_start_at": recording.start_at.to_string(),
        "recording_end_at": recording.end_at.to_string(),
        "track_id": recording.track_id,
        "nvr_playback_uri": playback_uri,
        "capabilities": {
            "open_external": true,
            "browser_playback": false,
            "download": false
        }
    })))
}

async fn api_activity(
    State(state): State<WebState>,
    Query(params): Query<ImageParams>,
) -> Result<Json<Value>, WebError> {
    validate_time_range(&params)?;
    let mut counts = Vec::new();
    for (category, column) in [
        ("download", "i.download_status"),
        ("processing", "i.processing_status"),
    ] {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
        builder
            .push(column)
            .push(" AS status, COUNT(*) AS count FROM images i WHERE 1 = 1");
        append_scope_filters(&mut builder, &params)?;
        builder.push(" GROUP BY ").push(column);
        let rows = builder
            .build()
            .fetch_all(state.ops.pool())
            .await
            .map_err(db_error)?;
        counts.extend(rows.into_iter().map(|row| {
            json!({
                "category": category,
                "status": row.get::<String, _>("status"),
                "count": row.get::<i64, _>("count")
            })
        }));
    }
    let mut active_builder = QueryBuilder::<Sqlite>::new(
        r#"SELECT i.id, i.capture_start_at, c.name AS camera_name, c.channel_number,
                  i.download_status, i.download_attempts, i.download_lease_until,
                  i.processing_status, i.processing_attempts, i.processing_started_at,
                  i.processing_lease_until
             FROM images i JOIN cameras c ON c.id = i.camera_id
            WHERE (i.download_status = 'downloading' OR i.processing_status = 'processing')"#,
    );
    append_scope_filters(&mut active_builder, &params)?;
    active_builder.push(" ORDER BY i.updated_at DESC LIMIT 100");
    let active_rows = active_builder
        .build()
        .fetch_all(state.ops.pool())
        .await
        .map_err(db_error)?;
    let active = active_rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.get::<i64, _>("id"),
                "captured_at": row.get::<String, _>("capture_start_at"),
                "camera_name": row.get::<Option<String>, _>("camera_name"),
                "channel": row.get::<i64, _>("channel_number"),
                "download_status": row.get::<String, _>("download_status"),
                "download_attempts": row.get::<i64, _>("download_attempts"),
                "download_lease_until": row.get::<Option<String>, _>("download_lease_until"),
                "processing_status": row.get::<String, _>("processing_status"),
                "processing_attempts": row.get::<i64, _>("processing_attempts"),
                "processing_started_at": row.get::<Option<String>, _>("processing_started_at"),
                "processing_lease_until": row.get::<Option<String>, _>("processing_lease_until")
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "counts": counts,
        "active": active,
        "generated_at": Timestamp::new(Utc::now()).to_string()
    })))
}

fn validate_time_range(params: &ImageParams) -> Result<(), WebError> {
    let from = params
        .from
        .as_deref()
        .map(|value| parse_timestamp(value, "from"))
        .transpose()?;
    let to = params
        .to
        .as_deref()
        .map(|value| parse_timestamp(value, "to"))
        .transpose()?;
    if let (Some(from), Some(to)) = (from, to)
        && from >= to
    {
        return Err(WebError::bad_request("from must be earlier than to"));
    }
    Ok(())
}

fn parse_timestamp(value: &str, field: &'static str) -> Result<Timestamp, WebError> {
    value
        .parse::<Timestamp>()
        .map_err(|_| WebError::bad_request(format!("{field} must be an RFC 3339 timestamp")))
}

fn encode_cursor(timestamp: &str, id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("{timestamp}\n{id}"))
}

fn decode_cursor(value: &str) -> Result<(String, i64), WebError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| WebError::bad_request("cursor is invalid"))?;
    let value = String::from_utf8(bytes).map_err(|_| WebError::bad_request("cursor is invalid"))?;
    let (timestamp, id) = value
        .rsplit_once('\n')
        .ok_or_else(|| WebError::bad_request("cursor is invalid"))?;
    parse_timestamp(timestamp, "cursor timestamp")?;
    let id = id
        .parse::<i64>()
        .map_err(|_| WebError::bad_request("cursor is invalid"))?;
    Ok((timestamp.to_string(), id))
}

fn parse_json_value(raw: Option<String>, default: Value) -> Value {
    raw.and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or(default)
}

fn validated_still_url(raw: &str, config: &NvrConfig) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    if !playback_host_allowed(url.host_str()?, config) {
        return None;
    }
    Some(url.to_string())
}

fn validate_recording_uri(raw: &str, config: &NvrConfig) -> Result<String, WebError> {
    let url = Url::parse(raw)
        .map_err(|_| WebError::bad_request("NVR returned an invalid playback URI"))?;
    if !matches!(url.scheme(), "rtsp" | "rtsps" | "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(WebError::bad_request("NVR returned an unsafe playback URI"));
    }
    if !playback_host_allowed(url.host_str().unwrap_or_default(), config) {
        return Err(WebError {
            status: StatusCode::BAD_GATEWAY,
            code: "playback_host_rejected",
            message: "NVR returned a playback URI for an unauthorized host".into(),
        });
    }
    Ok(url.to_string())
}

fn playback_host_allowed(host: &str, config: &NvrConfig) -> bool {
    host.eq_ignore_ascii_case(config.host.trim_matches(['[', ']']))
        || config
            .download
            .playback_host_allowlist
            .iter()
            .any(|allowed| host.eq_ignore_ascii_case(allowed.trim_matches(['[', ']'])))
}

fn db_error(error: sqlx::Error) -> WebError {
    tracing::warn!(operation = "web_query", "Web database query failed");
    WebError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "database_error",
        message: match error {
            sqlx::Error::RowNotFound => "The requested record was not found".into(),
            _ => "The database query failed".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn test_state() -> (tempfile::TempDir, WebState) {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("images");
        std::fs::create_dir(&output).unwrap();
        let database = crate::database::Database::open(&root.path().join("web.sqlite3"))
            .await
            .unwrap();
        let now = "2026-07-21T10:00:00.000000000Z";
        sqlx::query(
            r#"INSERT INTO cameras (
                   id, channel_number, primary_track_id, picture_track_id, name,
                   enabled, first_seen_at, last_seen_at, created_at, updated_at
               ) VALUES (1, 3, '301', '303', 'Back garden', 1, ?, ?, ?, ?)"#,
        )
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(database.pool())
        .await
        .unwrap();
        let image_path = output.join("one.jpg");
        std::fs::write(&image_path, [0xff, 0xd8, 0xff, 0xd9]).unwrap();
        sqlx::query(
            r#"INSERT INTO images (
                   id, image_key, camera_id, track_id, capture_start_at,
                   playback_uri, canonical_playback_uri, local_path,
                   download_status, downloaded_at, processing_status,
                   processing_completed_at, discovered_at, created_at, updated_at
               ) VALUES (
                   1, 'image-key', 1, '303', ?, 'http://nvr/image/1',
                   'http://nvr/image/1', ?, 'downloaded', ?, 'done', ?, ?, ?, ?
               )"#,
        )
        .bind(now)
        .bind(image_path.to_string_lossy().to_string())
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO classifications (
                   image_id, model, prompt_version, contains_wildlife,
                   is_interesting, summary, species_json, confidence,
                   classification_json, raw_response, request_started_at,
                   request_completed_at, created_at
               ) VALUES (1, 'vision', 'wildlife-v1', 1, 1, 'A squirrel.',
                   '[{"name":"squirrel","confidence":0.9}]', 0.9,
                   '{"summary":"A squirrel."}', 'SECRET-RAW-RESPONSE', ?, ?, ?)"#,
        )
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(database.pool())
        .await
        .unwrap();
        let nvr = NvrConfig {
            scheme: "http".into(),
            host: "nvr".into(),
            port: 80,
            username: "user".into(),
            password: Some(crate::configuration::Secret::new("password".into())),
            start_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            request_timeout_seconds: 5,
            connect_timeout_seconds: 5,
            allow_invalid_tls_certificates: false,
            search: crate::configuration::NvrSearchConfig {
                window_minutes: 60,
                max_results: 50,
                poll_interval_seconds: 60,
                poll_overlap_seconds: 120,
                camera_refresh_interval_seconds: 3600,
                settlement_delay_seconds: 10,
            },
            download: crate::configuration::NvrDownloadConfig {
                retry_limit: 3,
                retry_initial_delay_seconds: 1,
                retry_max_delay_seconds: 3,
                maximum_image_size_bytes: 1_000_000,
                verify_jpeg: true,
                rebase_playback_urls: true,
                concurrency: 1,
                playback_host_allowlist: Vec::new(),
            },
        };
        let transport = Arc::new(NvrTransport::from_config(&nvr).unwrap());
        let state = WebState {
            ops: database.ops(),
            output_directory: output,
            web: WebConfig::default(),
            nvr,
            recording_search: Arc::new(RecordingSearchClient::new(transport, 50)),
            started_at: "2026-07-21T10:00:00Z".parse().unwrap(),
        };
        (root, state)
    }

    #[test]
    fn cursor_round_trip() {
        let encoded = encode_cursor("2026-07-21T00:00:00Z", 42);
        assert_eq!(
            decode_cursor(&encoded).unwrap(),
            ("2026-07-21T00:00:00Z".to_string(), 42)
        );
    }

    #[tokio::test]
    async fn image_api_filters_and_returns_classification() {
        let (_root, state) = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images?from=2026-07-21T09%3A00%3A00.000Z&to=2026-07-21T11%3A00%3A00.000Z&camera=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["data"][0]["classification"]["summary"], "A squirrel.");
        assert_eq!(value["data"][0]["camera"]["name"], "Back garden");
    }

    #[tokio::test]
    async fn detail_api_omits_paths_and_raw_responses() {
        let (_root, state) = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("A squirrel."));
        assert!(!text.contains("SECRET-RAW-RESPONSE"));
        assert!(!text.contains("one.jpg"));
    }

    #[tokio::test]
    async fn image_content_is_served_as_jpeg() {
        let (_root, state) = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images/1/content")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "image/jpeg");
        let bytes = to_bytes(response.into_body(), 32).await.unwrap();
        assert_eq!(&bytes[..], &[0xff, 0xd8, 0xff, 0xd9]);
    }

    #[tokio::test]
    async fn index_has_security_headers() {
        let (_root, state) = test_state().await;
        let response = router(state)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("content-security-policy"));
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    }

    #[tokio::test]
    async fn image_api_rejects_reversed_time_range() {
        let (_root, state) = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images?from=2026-07-22T00%3A00%3A00Z&to=2026-07-21T00%3A00%3A00Z")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
