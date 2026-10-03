//! Axum-powered read-only JSON API.
//!
//! Exposes a REST API for browsing images, cameras, health, and activity.
//! All data access goes through `DatabaseOps` — the backend-neutral façade —
//! so the web layer is
//! agnostic to whether the backing store is SQLite or PostgreSQL.

mod assets;
mod auth;
mod bounding_boxes;
mod clips;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, RawQuery, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tower_http::compression::CompressionLayer;
use url::Url;

use crate::classifier::BoundingBox;
use crate::configuration::{Config, NvrConfig, WebConfig};
use crate::database::repository::DatabaseOps;
use crate::database::web_models::*;
use crate::domain::{ImageId, Timestamp, TrackId};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::{NvrTransport, RecordingSearchClient};
use crate::service_lifecycle::ShutdownToken;

// ── Web state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WebState {
    ops: DatabaseOps,
    output_directory: PathBuf,
    web: WebConfig,
    nvr: NvrConfig,
    recording_search: Arc<RecordingSearchClient>,
    clips: clips::ClipStore,
    bounding_box_render_limit: Arc<Semaphore>,
    #[allow(dead_code)]
    started_at: Timestamp,
    cursor_key: [u8; 32],
    event_limit: Arc<Semaphore>,
    thumbnails: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, bytes::Bytes>>>,
    login_guard: auth::LoginGuard,
}

impl WebState {
    pub fn from_config(ops: DatabaseOps, config: &Config, transport: Arc<NvrTransport>) -> Self {
        Self {
            ops,
            output_directory: config.general.output_directory.clone(),
            web: config.web.clone(),
            nvr: config.nvr.clone(),
            clips: clips::ClipStore::new(&config.web),
            recording_search: Arc::new(RecordingSearchClient::new(
                transport,
                config.nvr.search.max_results,
            )),
            bounding_box_render_limit: Arc::new(Semaphore::new(
                config.web.max_concurrent_bounding_box_renders,
            )),
            started_at: Timestamp::new(Utc::now()),
            event_limit: Arc::new(Semaphore::new(32)),
            thumbnails: Default::default(),
            login_guard: Default::default(),
            cursor_key: Sha256::digest(uuid::Uuid::new_v4().as_bytes()).into(),
        }
    }
}

// ── Server startup ─────────────────────────────────────────────────────────

/// Serve the API until the shared service shutdown token is cancelled.
pub async fn serve(state: WebState, shutdown: ShutdownToken) -> AppResult<()> {
    let listen_address = state.web.listen_address;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen_address)
        .await
        .map_err(|error| {
            AppError::with_source(
                ErrorCategory::Configuration,
                "web_bind",
                format!("failed to bind API server on {listen_address}"),
                error,
            )
        })?;
    tracing::info!(address = %listen_address, "API server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .map_err(|error| {
            AppError::with_source(
                ErrorCategory::Network,
                "web_serve",
                "API server failed",
                error,
            )
        })
}

pub fn router(state: WebState) -> Router {
    let timeout =
        std::time::Duration::from_secs(state.nvr.request_timeout_seconds.saturating_add(5).max(30));
    let clip_timeout = std::time::Duration::from_secs(
        state
            .web
            .maximum_clip_duration_seconds
            .saturating_mul(2)
            .saturating_add(60)
            .max(120),
    );
    Router::new()
        .route(
            "/api/v1/auth/login",
            post(auth::login).layer(axum::extract::DefaultBodyLimit::max(8192)),
        )
        .route("/api/v1/auth/session", get(auth::session))
        .route("/api/v1/auth/logout", post(auth::logout))
        // API endpoints
        .route("/api/v1/config", get(api_config))
        .route("/api/v1/health", get(api_health))
        .route("/api/v1/cameras", get(api_cameras))
        .route("/api/v1/overview", get(api_overview))
        .route("/api/v1/images", get(api_images))
        .route("/api/v1/images/facets", get(api_facets))
        .route("/api/v1/images/{id}", get(api_image))
        .route("/api/v1/images/{id}/neighbors", get(api_neighbors))
        .route("/api/v1/images/{id}/content", get(api_image_content))
        .route("/api/v1/images/{id}/thumbnail", get(api_image_thumbnail))
        .route("/api/v1/images/{id}/recording", get(api_recording))
        .route("/api/v1/images/{id}/clip", post(clips::prepare))
        .route("/api/v1/clips/{token}", get(clips::serve))
        .route("/api/v1/activity", get(api_activity))
        .route("/api/v1/events", get(api_events))
        .fallback(assets::serve)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_login,
        ))
        .with_state(state)
        .layer(CompressionLayer::new())
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| async move {
                let preparing_clip = request.method() == axum::http::Method::POST
                    && request.uri().path().ends_with("/clip");
                let sensitive = request.uri().path().starts_with("/api/v1/auth/")
                    || request.uri().path() == "/login"
                    || request.uri().path().contains("/clips/")
                    || preparing_clip
                    || request.uri().path().ends_with("/recording")
                    || request
                        .uri()
                        .path()
                        .strip_prefix("/api/v1/images/")
                        .is_some_and(|tail| !tail.contains('/'));
                let mut response = match tokio::time::timeout(
                    if preparing_clip {
                        clip_timeout
                    } else {
                        timeout
                    },
                    next.run(request),
                )
                .await
                {
                    Ok(response) => response,
                    Err(_) => WebError {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        code: "request_timeout",
                        message: "The request timed out. Try a narrower filter or try again."
                            .into(),
                    }
                    .into_response(),
                };
                if sensitive {
                    response
                        .headers_mut()
                        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
                }
                security_headers(response.headers_mut());
                response
            },
        ))
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

// ── Web error type ─────────────────────────────────────────────────────────

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
            message:
                "The requested operation could not be completed. Check the service diagnostics."
                    .into(),
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

// ── API: config ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ConfigResponse {
    version: &'static str,
    generated_at: String,
    default_clip_pre_roll_seconds: u64,
    default_clip_post_roll_seconds: u64,
    maximum_clip_duration_seconds: u64,
    capabilities: Value,
    nvr_identity: String,
}

async fn api_config(State(state): State<WebState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        version: env!("CARGO_PKG_VERSION"),
        nvr_identity: format!("{}:{}", state.nvr.host, state.nvr.port),
        generated_at: Timestamp::new(Utc::now()).to_string(),
        default_clip_pre_roll_seconds: state.web.clip_pre_roll_seconds,
        default_clip_post_roll_seconds: state.web.clip_post_roll_seconds,
        maximum_clip_duration_seconds: state.web.maximum_clip_duration_seconds,
        capabilities: json!({
            "nvr_still_url": true,
            "nvr_recording_lookup": true,
            "browser_clip_playback": state.clips.available(),
            "clip_download": state.clips.available()
        }),
    })
}

// ── API: health ────────────────────────────────────────────────────────────

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
    pipelines: Value,
}

async fn api_health(State(state): State<WebState>) -> Result<Json<HealthResponse>, WebError> {
    let snapshot = state.ops.web_health().await.map_err(db_error)?;
    let filter = build_image_filter(&ImageParams::default())?;
    let activity = state.ops.web_activity(&filter).await.map_err(db_error)?;
    let mut pipelines = serde_json::Map::new();
    for (name, key, category, success) in [
        (
            "downloader",
            crate::database::models::ServiceMetadataKey::DownloaderHeartbeat,
            "download",
            &snapshot.last_downloader_poll,
        ),
        (
            "classifier",
            crate::database::models::ServiceMetadataKey::ScannerHeartbeat,
            "processing",
            &snapshot.last_scanner_pass,
        ),
    ] {
        let raw = state.ops.get_metadata(&key).await.map_err(db_error)?;
        let heartbeat: Option<Value> = raw.and_then(|v| serde_json::from_str(&v).ok());
        let age = heartbeat
            .as_ref()
            .and_then(|v| v["heartbeat_at"].as_str())
            .and_then(|v| v.parse::<Timestamp>().ok())
            .map(|v| (Utc::now() - *v.as_datetime()).num_seconds());
        let fresh = age.is_some_and(|age| (0..=15).contains(&age));
        let active = activity
            .active
            .iter()
            .filter(|a| {
                let lease = if category == "download" {
                    &a.download_lease_until
                } else {
                    &a.processing_lease_until
                };
                let status = if category == "download" {
                    &a.download_status
                } else {
                    &a.processing_status
                };
                fresh
                    && ["downloading", "processing"].contains(&status.as_str())
                    && lease
                        .as_ref()
                        .and_then(|v| v.parse::<Timestamp>().ok())
                        .is_some_and(|v| *v.as_datetime() > Utc::now())
            })
            .count();
        let count = |statuses: &[&str]| {
            activity
                .counts
                .iter()
                .filter(|c| c.category == category && statuses.contains(&c.status.as_str()))
                .map(|c| c.count)
                .sum::<i64>()
        };
        let retry = count(&["retry_wait"]);
        let failures = count(&["failed", "unavailable", "missing"]);
        let poll = heartbeat
            .as_ref()
            .and_then(|v| v["poll_interval_seconds"].as_u64())
            .unwrap_or(5);
        let status = if heartbeat.is_none() {
            "unknown"
        } else if heartbeat.as_ref().is_some_and(|v| v["state"] == "stopped")
            || age.is_some_and(|age| age > 15.max(poll.saturating_mul(2) as i64))
        {
            "stopped"
        } else if !fresh
            || failures > 0
            || heartbeat.as_ref().is_some_and(|v| v["state"] == "degraded")
        {
            "degraded"
        } else if active > 0 {
            "active"
        } else if retry > 0 {
            "retrying"
        } else {
            "idle"
        };
        pipelines.insert(name.into(), json!({"state":status,"active":active,"queue_depth":count(&["pending","new","retry_wait"]),"oldest_queued_at":activity.counts.iter().filter(|c|c.category==category && ["pending","new","retry_wait"].contains(&c.status.as_str())).filter_map(|c|c.oldest_at.as_ref()).min(),"heartbeat_at":heartbeat.as_ref().map(|v|v["heartbeat_at"].clone()),"last_success_at":success,"heartbeat_fresh":fresh,"stale_after_seconds":15.max(poll.saturating_mul(2)),"last_error":if failures>0 {Some("Permanent failures are present in the queue.")}else{None}}));
    }
    let active_downloads = pipelines["downloader"]["active"].as_i64().unwrap_or(0);
    let active_classifications = pipelines["classifier"]["active"].as_i64().unwrap_or(0);
    Ok(Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        web_started_at: state.started_at.to_string(),
        last_camera_discovery: snapshot.last_camera_discovery.clone(),
        last_downloader_poll: snapshot.last_downloader_poll.clone(),
        last_scanner_pass: snapshot.last_scanner_pass.clone(),
        active_downloads,
        active_classifications,
        pipelines: Value::Object(pipelines),
        generated_at: snapshot.generated_at.clone(),
    }))
}

// ── API: cameras ───────────────────────────────────────────────────────────

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
    let records = state.ops.web_cameras().await.map_err(db_error)?;
    let data: Vec<CameraDto> = records
        .into_iter()
        .map(|r| CameraDto {
            id: r.id,
            channel_number: r.channel_number,
            name: r.name,
            enabled: r.enabled,
            primary_track_id: r.primary_track_id,
            picture_track_id: r.picture_track_id,
            last_seen_at: r.last_seen_at,
            last_completed_window_end: r.last_completed_window_end,
            last_poll_at: r.last_poll_at,
            next_search_at: r.next_search_at,
            last_error: r.last_error.map(|_| {
                "The last NVR search failed. Check connectivity and service diagnostics.".into()
            }),
        })
        .collect();
    Ok(Json(
        json!({ "data": data, "generated_at": Timestamp::new(Utc::now()).to_string() }),
    ))
}

// ── Query parameter parsing ────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
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
    species: Option<String>,
    model: Option<String>,
    prompt_version: Option<String>,
    time_field: Option<String>,
    q: Option<String>,
    failure: Option<String>,
    local_file: Option<String>,
}

// ── API: images ────────────────────────────────────────────────────────────

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
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, WebError> {
    let params = parse_image_params(raw)?;
    let (data, next_cursor) = query_images(&state, &params).await?;
    Ok(Json(json!({
        "data": data,
        "page": { "next_cursor": next_cursor, "has_more": next_cursor.is_some() },
        "generated_at": Timestamp::new(Utc::now()).to_string()
    })))
}

async fn local_file_present(state: &WebState, path: Option<&str>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let Ok(root) = tokio::fs::canonicalize(&state.output_directory).await else {
        return false;
    };
    let Ok(path) = tokio::fs::canonicalize(path).await else {
        return false;
    };
    path.starts_with(root) && tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file())
}

async fn query_images(
    state: &WebState,
    params: &ImageParams,
) -> Result<(Vec<ImageSummary>, Option<String>), WebError> {
    let mut current = params.clone();
    let limit = params.limit.unwrap_or(60);
    let mut results = Vec::new();
    let mut next = None;
    // A missing-file filter requires filesystem checks. Bound each response's
    // scan to eight database pages; the continuation still covers every row.
    for _ in 0..8 {
        current.limit = Some(limit - results.len() as u32);
        let (page, cursor) = query_images_page(state, &current).await?;
        results.extend(page);
        next = cursor;
        if results.len() >= limit as usize || next.is_none() || params.local_file.is_none() {
            break;
        }
        current.cursor = next.clone();
    }
    Ok((results, next))
}

async fn api_facets(
    State(state): State<WebState>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, WebError> {
    let params = parse_image_params(raw)?;
    validate_time_range(&params)?;
    if params.local_file.is_some() {
        return Ok(Json(json!({"download_status":{},"processing_status":{}})));
    }
    let mut result = serde_json::Map::new();
    for (facet, statuses) in [
        (
            "download_status",
            vec![
                "pending",
                "downloading",
                "downloaded",
                "retry_wait",
                "unavailable",
                "failed",
            ],
        ),
        (
            "processing_status",
            vec![
                "new",
                "processing",
                "done",
                "retry_wait",
                "failed",
                "missing",
            ],
        ),
    ] {
        let mut counts = serde_json::Map::new();
        for status in statuses {
            let mut candidate = params.clone();
            if facet == "download_status" {
                candidate.download_status = Some(status.into());
            } else {
                candidate.processing_status = Some(status.into());
            }
            let filter = build_image_filter(&candidate)?;
            let count = state
                .ops
                .web_overview(&filter)
                .await
                .map_err(db_error)?
                .discovered;
            counts.insert(status.into(), json!(count));
        }
        result.insert(facet.into(), Value::Object(counts));
    }
    Ok(Json(Value::Object(result)))
}

async fn query_images_page(
    state: &WebState,
    params: &ImageParams,
) -> Result<(Vec<ImageSummary>, Option<String>), WebError> {
    validate_time_range(params)?;
    let order = match params.sort.as_deref().unwrap_or("captured_desc") {
        "captured_desc" => WebImageOrder::CapturedDescending,
        "captured_asc" => WebImageOrder::CapturedAscending,
        "confidence_desc" => WebImageOrder::ConfidenceDescending,
        "classified_desc" => WebImageOrder::ClassifiedDescending,
        "camera_asc" => WebImageOrder::CameraAscending,
        _ => return Err(WebError::bad_request("unsupported image sort")),
    };
    let limit = params.limit.unwrap_or(60);
    if !(1..=200).contains(&limit) {
        return Err(WebError::bad_request("limit must be between 1 and 200"));
    }
    let filter = build_image_filter(params)?;
    let query = WebImageQuery {
        filter,
        order,
        limit,
        cursor: params
            .cursor
            .as_deref()
            .map(|v| decode_cursor(v, params, &state.cursor_key))
            .transpose()?,
    };

    let (summaries, next_cursor) = state.ops.web_query_images(&query).await.map_err(db_error)?;
    let mut matching = Vec::with_capacity(summaries.len());
    for summary in summaries {
        if let Some(presence) = &params.local_file {
            let present = summary.download_status == "downloaded"
                && local_file_present(state, summary.local_path.as_deref()).await;
            if present != (presence == "present") {
                continue;
            }
        }
        matching.push(summary);
    }
    let summaries = matching;

    let data: Vec<ImageSummary> = summaries
        .into_iter()
        .map(|s| ImageSummary {
            id: s.id,
            captured_at: s.captured_at,
            capture_end_at: s.capture_end_at,
            camera: CameraSummary {
                id: s.camera.id,
                name: s.camera.name,
                channel: s.camera.channel,
            },
            content_url: s.content_url,
            thumbnail_url: s.thumbnail_url,
            download_status: s.download_status,
            processing_status: s.processing_status,
            classification: s.classification.map(|c| ClassificationSummary {
                id: c.id,
                contains_wildlife: c.contains_wildlife,
                interesting: c.is_interesting,
                summary: c.summary,
                species: c.species,
                confidence: c.confidence,
                model: c.model,
                prompt_version: c.prompt_version,
                completed_at: c.completed_at,
            }),
        })
        .collect();

    let next_cursor = next_cursor.map(|(ts, id)| encode_cursor(&ts, id, params, &state.cursor_key));
    Ok((data, next_cursor))
}

fn build_image_filter(params: &ImageParams) -> Result<WebImageFilter, WebError> {
    // Valid download and processing statuses for strict validation
    const VALID_DOWNLOAD_STATUSES: &[&str] = &[
        "pending",
        "downloading",
        "downloaded",
        "retry_wait",
        "unavailable",
        "failed",
    ];
    const VALID_PROCESSING_STATUSES: &[&str] = &[
        "new",
        "processing",
        "done",
        "retry_wait",
        "failed",
        "missing",
    ];

    let scope = WebScopeFilter {
        from: params
            .from
            .as_deref()
            .map(|s| {
                s.parse::<Timestamp>()
                    .map_err(|_| WebError::bad_request("from must be an RFC 3339 timestamp"))
            })
            .transpose()?,
        to: params
            .to
            .as_deref()
            .map(|s| {
                s.parse::<Timestamp>()
                    .map_err(|_| WebError::bad_request("to must be an RFC 3339 timestamp"))
            })
            .transpose()?,
        camera_ids: params
            .camera
            .as_ref()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim())
                    .map(|s| {
                        s.parse::<i64>()
                            .map_err(|_| WebError::bad_request(format!("invalid camera ID: {s}")))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default(),
    };

    let download_status = params.download_status.as_ref().map(|v| {
        let statuses: Vec<String> = v.split(',').map(|s| s.trim().to_string()).collect();
        WebDownloadStatusFilter::OneOf(statuses)
    });

    let processing_status = params.processing_status.as_ref().map(|v| {
        let statuses: Vec<String> = v.split(',').map(|s| s.trim().to_string()).collect();
        WebProcessingStatusFilter::OneOf(statuses)
    });

    // Validate download_status values
    if let Some(WebDownloadStatusFilter::OneOf(statuses)) = &download_status {
        for status in statuses {
            if !VALID_DOWNLOAD_STATUSES.contains(&status.as_str()) {
                return Err(WebError::bad_request(format!(
                    "unknown download_status: {status}"
                )));
            }
        }
    }

    // Validate processing_status values
    if let Some(WebProcessingStatusFilter::OneOf(statuses)) = &processing_status {
        for status in statuses {
            if !VALID_PROCESSING_STATUSES.contains(&status.as_str()) {
                return Err(WebError::bad_request(format!(
                    "unknown processing_status: {status}"
                )));
            }
        }
    }

    let classified = params.classified.map(|v| match v {
        true => WebClassifiedFilter::Classified,
        false => WebClassifiedFilter::NotClassified,
    });

    let contains_wildlife = params.contains_wildlife;
    let is_interesting = params.interesting;

    let confidence_min = match params.confidence_min {
        Some(v) if (0.0..=1.0).contains(&v) => Some(v),
        Some(_) => {
            return Err(WebError::bad_request(
                "confidence_min must be between 0 and 1",
            ));
        }
        None => None,
    };

    if params
        .local_file
        .as_deref()
        .is_some_and(|v| !["present", "missing"].contains(&v))
    {
        return Err(WebError::bad_request(
            "local_file must be present or missing",
        ));
    }
    let time_field = params
        .time_field
        .clone()
        .unwrap_or_else(|| "captured".into());
    if !["captured", "discovered", "downloaded", "classified"].contains(&time_field.as_str()) {
        return Err(WebError::bad_request("unsupported time_field"));
    }
    if params
        .failure
        .as_deref()
        .is_some_and(|v| !["retryable", "permanent"].contains(&v))
    {
        return Err(WebError::bad_request("unsupported failure filter"));
    }
    Ok(WebImageFilter {
        scope,
        download_status,
        processing_status,
        classified,
        contains_wildlife,
        is_interesting,
        confidence_min,
        advanced: WebAdvancedFilter {
            time_field,
            species: params
                .species
                .as_ref()
                .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            model: params.model.clone(),
            prompt_version: params.prompt_version.clone(),
            text: params.q.clone(),
            failure: params.failure.clone(),
        },
    })
}

// ── API: overview ──────────────────────────────────────────────────────────

async fn api_overview(
    State(state): State<WebState>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, WebError> {
    let params = parse_image_params(raw)?;
    validate_time_range(&params)?;
    let filter = build_image_filter(&params)?;
    let record = state.ops.web_overview(&filter).await.map_err(db_error)?;
    let camera_counts = state
        .ops
        .web_camera_counts(&filter)
        .await
        .map_err(db_error)?;
    let range = match (&filter.scope.from, &filter.scope.to) {
        (Some(from), Some(to)) => (*to.as_datetime() - *from.as_datetime()).num_seconds(),
        _ => i64::MAX,
    };
    let seconds = if range <= 21600 {
        300
    } else if range <= 172800 {
        3600
    } else if range <= 7776000 {
        86400
    } else {
        604800
    };
    let buckets = state
        .ops
        .web_buckets(&filter, seconds)
        .await
        .map_err(db_error)?;
    Ok(Json(json!({
        "buckets": buckets,
        "camera_counts": camera_counts,
        "counts": {
            "discovered": record.discovered,
            "downloaded": record.downloaded,
            "classified": record.classified,
            "wildlife": record.wildlife,
            "interesting": record.interesting,
            "retryable_failures": record.retryable_failures,
            "permanent_failures": record.permanent_failures
        },
        "generated_at": Timestamp::new(Utc::now()).to_string()
    })))
}

// ── API: image detail ─────────────────────────────────────────────────────

async fn api_image(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
) -> Result<Json<Value>, WebError> {
    let detail = state
        .ops
        .web_image_detail(ImageId::new(id))
        .await
        .map_err(|error| match error.message.as_str() {
            "row not found" => WebError::not_found("Image not found"),
            _ => error.into(),
        })?;
    let detail = detail.ok_or_else(|| WebError::not_found("Image not found"))?;

    let lookup = state
        .ops
        .web_image_content_lookup(ImageId::new(id))
        .await
        .map_err(db_error)?;
    let mut file = json!({"present":false});
    if let Some(lookup) = lookup
        && lookup.download_status == "downloaded"
        && local_file_present(&state, lookup.local_path.as_deref()).await
        && let Some(path) = lookup.local_path
        && let Ok(metadata) = tokio::fs::metadata(path).await
    {
        file = json!({"present":true,"name":format!("image-{id}.jpg"),"size_bytes":metadata.len()});
    }
    // Validate NVR URLs from the detail record
    let validated_image_url = detail
        .nvr
        .image_url
        .as_deref()
        .and_then(|url| validated_still_url(url, &state.nvr));
    let validated_reported_image_url = detail
        .nvr
        .reported_image_url
        .as_deref()
        .and_then(|url| validated_still_url(url, &state.nvr));

    let classifications: Vec<Value> = detail
        .classifications
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "model": c.model,
                "prompt_version": c.prompt_version,
                "contains_wildlife": c.contains_wildlife,
                "interesting": c.is_interesting,
                "summary": c.summary,
                "species": c.species,
                "bounding_boxes": c.bounding_boxes,
                "confidence": c.confidence,
                "structured": c.structured,
                "request_started_at": c.request_started_at,
                "request_completed_at": c.request_completed_at,
                "created_at": c.created_at
            })
        })
        .collect();

    Ok(Json(json!({
        "id": detail.id,
        "image_key": detail.image_key,
        "file": file,
        "captured_at": detail.captured_at,
        "capture_end_at": detail.capture_end_at,
        "discovered_at": detail.discovered_at,
        "camera": {
            "id": detail.camera.id,
            "channel": detail.camera.channel,
            "name": detail.camera.name,
            "primary_track_id": detail.camera.primary_track_id,
            "picture_track_id": detail.camera.picture_track_id
        },
        "content_url": detail.content_url,
        "download": {
            "status": detail.download.status,
            "attempts": detail.download.attempts,
            "downloaded_at": detail.download.downloaded_at,
            "last_error": detail.download.last_error.map(|_|"The image download failed. Check the NVR connection and service diagnostics."),
            "next_attempt_at": detail.download.next_attempt_at,
            "lease_until": detail.download.lease_until
        },
        "processing": {
            "status": detail.processing.status,
            "attempts": detail.processing.attempts,
            "started_at": detail.processing.started_at,
            "completed_at": detail.processing.completed_at,
            "last_error": detail.processing.last_error.map(|_|"Classification failed. Check the model endpoint and service diagnostics."),
            "next_attempt_at": detail.processing.next_attempt_at,
            "lease_until": detail.processing.lease_until
        },
        "nvr": {
            "image_url": validated_image_url,
            "reported_image_url": validated_reported_image_url
        },
        "classifications": classifications
    })))
}

async fn api_neighbors(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, WebError> {
    let mut params = parse_image_params(raw)?;
    validate_time_range(&params)?;
    let detail = state
        .ops
        .web_image_detail(ImageId::new(id))
        .await
        .map_err(db_error)?
        .ok_or_else(|| WebError::not_found("Image not found"))?;
    if params.camera.is_none() && params.from.is_none() && params.to.is_none() {
        params.camera = Some(detail.camera.id.to_string());
    }
    let filter = build_image_filter(&params)?;
    let latest = detail.classifications.first();
    let (forward, backward, value) = match params.sort.as_deref().unwrap_or("captured_desc") {
        "captured_desc" => (
            WebImageOrder::CapturedDescending,
            WebImageOrder::CapturedAscending,
            detail.captured_at.clone(),
        ),
        "captured_asc" => (
            WebImageOrder::CapturedAscending,
            WebImageOrder::CapturedDescending,
            detail.captured_at.clone(),
        ),
        "confidence_desc" => (
            WebImageOrder::ConfidenceDescending,
            WebImageOrder::ConfidenceAscending,
            latest
                .and_then(|c| c.confidence)
                .unwrap_or(-1.0)
                .to_string(),
        ),
        "classified_desc" => (
            WebImageOrder::ClassifiedDescending,
            WebImageOrder::ClassifiedAscending,
            latest
                .map(|c| c.request_completed_at.clone())
                .unwrap_or_default(),
        ),
        "camera_asc" => (
            WebImageOrder::CameraAscending,
            WebImageOrder::CameraDescending,
            serde_json::to_string(&(detail.camera.channel, &detail.captured_at))
                .expect("tuple serializes"),
        ),
        _ => return Err(WebError::bad_request("unsupported image sort")),
    };
    async fn find_neighbor(
        state: &WebState,
        mut query: WebImageQuery,
        presence: Option<&str>,
    ) -> Result<Option<i64>, WebError> {
        query.limit = if presence.is_some() { 200 } else { 1 };
        loop {
            let (rows, cursor) = state.ops.web_query_images(&query).await.map_err(db_error)?;
            for row in rows {
                if let Some(presence) = presence {
                    let exists = row.download_status == "downloaded"
                        && local_file_present(state, row.local_path.as_deref()).await;
                    if exists != (presence == "present") {
                        continue;
                    }
                }
                return Ok(Some(row.id));
            }
            if cursor.is_none() {
                return Ok(None);
            }
            query.cursor = cursor;
        }
    }
    let next = find_neighbor(
        &state,
        WebImageQuery {
            filter: filter.clone(),
            order: forward,
            limit: 1,
            cursor: Some((value.clone(), id)),
        },
        params.local_file.as_deref(),
    )
    .await?;
    let previous = find_neighbor(
        &state,
        WebImageQuery {
            filter,
            order: backward,
            limit: 1,
            cursor: Some((value, id)),
        },
        params.local_file.as_deref(),
    )
    .await?;
    Ok(Json(json!({"previous":previous,"next":next})))
}

// ── API: image content ─────────────────────────────────────────────────────

#[derive(Default, Deserialize)]
struct ImageContentParams {
    #[serde(rename = "draw-bounding-box")]
    draw_bounding_box: Option<bool>,
}

async fn api_image_content(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
    Query(params): Query<ImageContentParams>,
) -> Result<Response, WebError> {
    serve_image_content(state, id, params.draw_bounding_box == Some(true), false).await
}

async fn api_image_thumbnail(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
) -> Result<Response, WebError> {
    serve_image_content(state, id, false, true).await
}

/// Open from a directory descriptor, refusing symlinks in every component.
/// Anchoring traversal prevents a replaced-path race between containment checks
/// and the actual read. O_NONBLOCK avoids blocking on a substituted FIFO.
async fn open_local_image(root: PathBuf, path: PathBuf) -> Result<tokio::fs::File, WebError> {
    let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        use nix::fcntl::{OFlag, open, openat};
        use nix::sys::stat::Mode;
        let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
        let mut directory = open(&root, flags | OFlag::O_DIRECTORY, Mode::empty())?;
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
        let components = relative.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let std::path::Component::Normal(name) = component else {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            };
            let final_component = index + 1 == components.len();
            directory = openat(
                &directory,
                Path::new(name),
                flags
                    | if final_component {
                        OFlag::O_NONBLOCK
                    } else {
                        OFlag::O_DIRECTORY
                    },
                Mode::empty(),
            )?;
        }
        let file = std::fs::File::from(directory);
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        Ok(file)
    })
    .await
    .map_err(|_| WebError::not_found("Local image file could not be opened"))?
    .map_err(|_| WebError::not_found("Local image file is missing or unsafe"))?;
    Ok(tokio::fs::File::from_std(file))
}

async fn serve_image_content(
    state: WebState,
    id: i64,
    draw_bounding_box: bool,
    thumbnail: bool,
) -> Result<Response, WebError> {
    let lookup = state
        .ops
        .web_image_content_lookup(ImageId::new(id))
        .await
        .map_err(db_error)?
        .ok_or_else(|| WebError::not_found("Image not found"))?;

    if lookup.download_status != "downloaded" {
        return Err(WebError::not_found("Image content has not been downloaded"));
    }

    let path: String = lookup
        .local_path
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

    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|_| WebError::not_found("Local image file is missing"))?;
    if !metadata.is_file() || metadata.len() > state.nvr.download.maximum_image_size_bytes {
        return Err(WebError::bad_request(
            "The local image is not a regular file within the configured size limit",
        ));
    }
    let cache_key = format!("{id}:{}:{:?}", metadata.len(), metadata.modified().ok());
    if thumbnail && let Some(bytes) = state.thumbnails.lock().await.get(&cache_key).cloned() {
        return Ok((
            [
                (CONTENT_TYPE, "image/jpeg"),
                (CACHE_CONTROL, "private, max-age=300"),
            ],
            bytes,
        )
            .into_response());
    }
    let boxes = if draw_bounding_box {
        lookup
            .bounding_boxes_json
            .as_deref()
            .map(|json| {
                serde_json::from_str::<Vec<BoundingBox>>(json).map_err(|_| WebError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    code: "invalid_bounding_boxes",
                    message: "The image's bounding box data is invalid".into(),
                })
            })
            .transpose()?
            .filter(|boxes| !boxes.is_empty())
    } else {
        None
    };

    // Acquire before reading the JPEG so waiting requests do not retain image
    // bytes. Move the permit into the blocking task so cancellation of the
    // HTTP request cannot release it while a render is still running.
    let render = if boxes.is_some() || thumbnail {
        let permit = state
            .bounding_box_render_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WebError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "image_rendering_unavailable",
                message: "Image rendering is unavailable".into(),
            })?;
        Some((boxes.unwrap_or_default(), permit))
    } else {
        None
    };

    use tokio::io::AsyncReadExt;
    let file = open_local_image(root, path).await?;
    let mut bytes = Vec::new();
    file.take(state.nvr.download.maximum_image_size_bytes + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| WebError::not_found("Local image file could not be read"))?;
    if bytes.len() as u64 > state.nvr.download.maximum_image_size_bytes {
        return Err(WebError::bad_request(
            "Image exceeds the configured size limit",
        ));
    }

    // Full JPEG validation: check signature and trailer
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(WebError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_image",
            message: "The local file is not a valid JPEG".into(),
        });
    }

    if let Some((boxes, permit)) = render {
        let color = state.web.bounding_box_color;
        let stroke_width = state.web.bounding_box_width_pixels;
        bytes = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if thumbnail {
                bounding_boxes::thumbnail(&bytes)
            } else {
                bounding_boxes::render_jpeg_with_boxes(&bytes, &boxes, color, stroke_width)
            }
        })
        .await
        .map_err(|_| WebError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "image_processing_failed",
            message: "Could not draw image bounding boxes".into(),
        })?
        .map_err(|_| WebError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_image",
            message: "The local JPEG could not be drawn".into(),
        })?;
    }

    let bytes = bytes::Bytes::from(bytes);
    if thumbnail {
        let mut cache = state.thumbnails.lock().await;
        // Hard memory/entry caps; an evicted thumbnail is safely regenerated.
        if cache.len() >= 512
            || cache.values().map(|v| v.len()).sum::<usize>() + bytes.len() > 64 * 1024 * 1024
        {
            cache.clear();
        }
        cache.insert(cache_key, bytes.clone());
    }
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if draw_bounding_box {
            "private, no-store"
        } else {
            "private, max-age=300"
        }),
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"fauna-scan-{id}.jpg\""))
            .unwrap_or_else(|_| HeaderValue::from_static("inline")),
    );
    security_headers(response.headers_mut());
    Ok(response)
}

// ── API: recording ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RecordingParams {
    pre_roll_seconds: Option<u64>,
    post_roll_seconds: Option<u64>,
}

struct RecordingLookup {
    start: Timestamp,
    end: Timestamp,
    duration: u64,
    recording: Option<crate::nvr::RecordingMatch>,
    playback_uri: Option<String>,
}

async fn lookup_recording(
    state: &WebState,
    id: i64,
    params: RecordingParams,
) -> Result<RecordingLookup, WebError> {
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

    let target = state
        .ops
        .web_recording_target(ImageId::new(id))
        .await
        .map_err(db_error)?
        .ok_or_else(|| WebError::not_found("Image not found"))?;

    let capture_start_at = target
        .capture_start_at
        .parse::<Timestamp>()
        .map_err(|_| WebError::bad_request("image capture timestamp is invalid"))?;
    let start = Timestamp::new(
        capture_start_at
            .as_datetime()
            .checked_sub_signed(Duration::seconds(pre as i64))
            .ok_or_else(|| WebError::bad_request("clip start is out of range"))?,
    );
    let end = Timestamp::new(
        capture_start_at
            .as_datetime()
            .checked_add_signed(Duration::seconds(post as i64))
            .ok_or_else(|| WebError::bad_request("clip end is out of range"))?,
    );

    let track = TrackId::new(target.primary_track_id);
    let found = state
        .recording_search
        .find(&track, start, end, capture_start_at)
        .await
        .map_err(|e| {
            tracing::warn!(image_id = %id, category = ?e.category, "Recording search failed");
            WebError {
                status: StatusCode::BAD_GATEWAY,
                code: "recording_search_failed",
                message: "Failed to search for recording".into(),
            }
        })?;

    let playback_uri = found
        .as_ref()
        .map(|recording| {
            bounded_recording_uri(
                &recording.playback_uri,
                &state.nvr,
                state.web.rtsp_port,
                &track,
                start,
                end,
            )
        })
        .transpose()?;
    Ok(RecordingLookup {
        start,
        end,
        duration,
        recording: found,
        playback_uri,
    })
}

async fn api_recording(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
    Query(params): Query<RecordingParams>,
) -> Result<Json<Value>, WebError> {
    let lookup = lookup_recording(&state, id, params).await?;
    let Some(recording) = lookup.recording else {
        return Ok(Json(json!({
            "status": "not_found",
            "requested_start_at": lookup.start.to_string(),
            "requested_end_at": lookup.end.to_string()
        })));
    };
    Ok(Json(json!({
        "status": "found",
        "requested_start_at": lookup.start.to_string(),
        "requested_end_at": lookup.end.to_string(),
        "recording_start_at": recording.start_at.to_string(),
        "recording_end_at": recording.end_at.to_string(),
        "track_id": recording.track_id,
        "nvr_playback_uri": lookup.playback_uri,
        "capabilities": {
            "open_external": true,
            "browser_playback": state.clips.available(),
            "download": state.clips.available()
        }
    })))
}

// The NVR search URI describes a whole storage segment and can even contain
// the ISAPI port. Validate its source, then construct a bounded RTSP request.
fn bounded_recording_uri(
    raw: &str,
    config: &NvrConfig,
    port: u16,
    track: &TrackId,
    start: Timestamp,
    end: Timestamp,
) -> Result<String, WebError> {
    validate_recording_uri(raw, config)?;
    let mut url = Url::parse("rtsp://localhost/").expect("static URL");
    url.set_host(Some(&config.host))
        .map_err(|_| WebError::bad_request("Invalid NVR host"))?;
    url.set_port(Some(port))
        .map_err(|_| WebError::bad_request("Invalid RTSP port"))?;
    url.set_path(&format!("/Streaming/tracks/{}/", track.as_str()));
    url.query_pairs_mut()
        .append_pair(
            "starttime",
            &start.as_datetime().format("%Y%m%dT%H%M%SZ").to_string(),
        )
        .append_pair(
            "endtime",
            &end.as_datetime().format("%Y%m%dT%H%M%SZ").to_string(),
        );
    Ok(url.to_string())
}

// ── API: activity ──────────────────────────────────────────────────────────

async fn api_activity(
    State(state): State<WebState>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, WebError> {
    let params = parse_image_params(raw)?;
    validate_time_range(&params)?;
    let filter = build_image_filter(&params)?;
    let record = state.ops.web_activity(&filter).await.map_err(db_error)?;

    let counts: Vec<Value> = record
        .counts
        .into_iter()
        .map(|c| {
            json!({
                "category": c.category,
                "status": c.status,
                "count": c.count
            })
        })
        .collect();

    let active: Vec<Value> = record
        .active
        .into_iter()
        .map(|a| {
            json!({
                "id": a.id,
                "captured_at": a.captured_at,
                "camera_name": a.camera_name,
                "channel": a.channel,
                "download_status": a.download_status,
                "download_attempts": a.download_attempts,
                "download_lease_until": a.download_lease_until,
                "processing_status": a.processing_status,
                "processing_attempts": a.processing_attempts,
                "processing_started_at": a.processing_started_at,
                "processing_lease_until": a.processing_lease_until
            })
        })
        .collect();

    Ok(Json(json!({
        "counts": counts,
        "active": active,
        "generated_at": record.generated_at
    })))
}

// Every event is a full invalidation, including the first after reconnect/restart.
// No replay buffer is needed because no entity state is carried in the stream.
async fn api_events(State(state): State<WebState>) -> Result<Response, WebError> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let permit = state
        .event_limit
        .clone()
        .try_acquire_owned()
        .map_err(|_| WebError {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "event_limit",
            message: "Too many live-update connections. Polling remains available.".into(),
        })?;
    let interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let stream =
        futures_util::stream::unfold((interval, permit), |(mut interval, permit)| async move {
            interval.tick().await;
            let now = Utc::now().to_rfc3339();
            let event = Event::default().event("invalidate").id(&now).data(
                json!({"resources":["health","activity","images","overview","cameras"],"at":now})
                    .to_string(),
            );
            Some((Ok::<_, std::convert::Infallible>(event), (interval, permit)))
        });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default().interval(std::time::Duration::from_secs(15)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

// ── Validation helpers ─────────────────────────────────────────────────────

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

fn parse_image_params(raw: Option<String>) -> Result<ImageParams, WebError> {
    let mut values = std::collections::BTreeMap::<String, String>::new();
    for (key, value) in url::form_urlencoded::parse(raw.as_deref().unwrap_or("").as_bytes()) {
        if let Some(existing) = values.get_mut(key.as_ref()) {
            if !["camera", "species", "download_status", "processing_status"]
                .contains(&key.as_ref())
            {
                return Err(WebError::bad_request("duplicate query parameter"));
            }
            existing.push(',');
            existing.push_str(&value);
        } else {
            values.insert(key.into_owned(), value.into_owned());
        }
    }
    let mut object = serde_json::Map::new();
    for (key, value) in values {
        let parsed = match key.as_str() {
            "contains_wildlife" | "interesting" | "classified" => json!(
                value
                    .parse::<bool>()
                    .map_err(|_| WebError::bad_request("invalid boolean filter"))?
            ),
            "limit" => json!(
                value
                    .parse::<u32>()
                    .map_err(|_| WebError::bad_request("invalid limit"))?
            ),
            "confidence_min" => {
                let confidence = value
                    .parse::<f64>()
                    .map_err(|_| WebError::bad_request("invalid confidence"))?;
                if !confidence.is_finite() {
                    return Err(WebError::bad_request("invalid confidence"));
                }
                json!(confidence)
            }
            _ => json!(value),
        };
        object.insert(key, parsed);
    }
    serde_json::from_value(Value::Object(object))
        .map_err(|_| WebError::bad_request("unknown or invalid query parameter"))
}

fn cursor_scope(params: &ImageParams) -> String {
    let mut scope = params.clone();
    scope.cursor = None;
    scope.limit = None;
    // Canonicalize unordered multi-value filters.
    for raw in [
        &mut scope.camera,
        &mut scope.download_status,
        &mut scope.processing_status,
        &mut scope.species,
    ]
    .into_iter()
    .flatten()
    {
        let mut values: Vec<_> = raw.split(',').map(str::trim).collect();
        values.sort_unstable();
        values.dedup();
        *raw = values.join(",");
    }
    serde_json::to_string(&scope).expect("filter serializes")
}

// HMAC-SHA256 with a per-process random key. Restarted servers reject old cursors.
fn cursor_mac(payload: &[u8], key: &[u8; 32]) -> Vec<u8> {
    let mut inner = [0x36; 64];
    let mut outer = [0x5c; 64];
    for i in 0..32 {
        inner[i] ^= key[i];
        outer[i] ^= key[i];
    }
    let mut hash = Sha256::new();
    hash.update(inner);
    hash.update(payload);
    let digest = hash.finalize();
    let mut hash = Sha256::new();
    hash.update(outer);
    hash.update(digest);
    hash.finalize().to_vec()
}
fn encode_cursor(value: &str, id: i64, params: &ImageParams, key: &[u8; 32]) -> String {
    let payload =
        serde_json::to_vec(&(value, id, cursor_scope(params))).expect("cursor serializes");
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(cursor_mac(&payload, key))
    )
}
fn decode_cursor(
    value: &str,
    params: &ImageParams,
    key: &[u8; 32],
) -> Result<(String, i64), WebError> {
    let invalid = || WebError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_cursor",
        message: "This image cursor expired or belongs to different filters. Reload the list."
            .into(),
    };
    let (payload, signature) = value.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
    let expected = cursor_mac(&payload, key);
    if signature.len() != expected.len()
        || signature
            .iter()
            .zip(&expected)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        return Err(invalid());
    }
    let (sort_value, id, scope): (String, i64, String) =
        serde_json::from_slice(&payload).map_err(|_| invalid())?;
    if scope != cursor_scope(params) {
        return Err(invalid());
    }
    Ok((sort_value, id))
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

fn db_error(error: AppError) -> WebError {
    tracing::warn!(operation = "web_query", "Web database query failed");
    WebError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "database_error",
        message: match error.message.as_str() {
            "row not found" => "The requested record was not found".into(),
            _ => "The database query failed".into(),
        },
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    pub(super) const TEST_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    /// Exercise existing API/media tests through the real authentication layer.
    pub(super) fn test_router(state: WebState) -> Router {
        super::router(state).layer(axum::middleware::from_fn(
            |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
                request.headers_mut().insert(
                    "cookie",
                    HeaderValue::from_static(
                        "fauna_scan_session=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                    ),
                );
                request
                    .headers_mut()
                    .insert("x-fauna-scan-request", HeaderValue::from_static("1"));
                next.run(request).await
            },
        ))
    }

    pub(super) async fn test_state() -> (tempfile::TempDir, WebState) {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("images");
        std::fs::create_dir(&output).unwrap();
        let db_path = root.path().join("web.sqlite3");
        let sqlite_store = crate::database::sqlite::SqliteDataStore::connect(&db_path, 1)
            .await
            .unwrap();
        let ops = sqlite_store.ops();
        ops.add_user("fixture", "fixture-credential").await.unwrap();
        let user = ops.find_user("fixture").await.unwrap().unwrap();
        ops.create_session(&crate::authentication::fingerprint(TEST_TOKEN), &user, None)
            .await
            .unwrap();
        let now = "2026-07-21T10:00:00.000000000Z";
        // Insert camera
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
        .execute(sqlite_store.pool())
        .await
        .unwrap();
        // Insert image
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
        .execute(sqlite_store.pool())
        .await
        .unwrap();
        // Insert classification
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
        .execute(sqlite_store.pool())
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
                capture_time_window: None,
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
            ops: sqlite_store.ops(),
            output_directory: output,
            web: WebConfig::default(),
            nvr,
            recording_search: Arc::new(RecordingSearchClient::new(transport, 50)),
            clips: clips::ClipStore::new(&WebConfig::default()),
            bounding_box_render_limit: Arc::new(Semaphore::new(2)),
            started_at: "2026-07-21T10:00:00Z".parse().unwrap(),
            event_limit: Arc::new(Semaphore::new(32)),
            thumbnails: Default::default(),
            cursor_key: [7; 32],
            login_guard: Default::default(),
        };
        (root, state)
    }

    #[tokio::test]
    async fn playback_uri_uses_requested_times_and_rtsp_port() {
        let (_root, state) = test_state().await;
        let start = "2026-10-03T05:40:00Z".parse().unwrap();
        let end = "2026-10-03T05:40:30Z".parse().unwrap();
        let uri = bounded_recording_uri(
            "rtsp://nvr:8080/Streaming/tracks/301/?starttime=20261003T053317Z&endtime=20261003T060304Z&name=segment&size=123",
            &state.nvr, 554, &TrackId::new("301"), start, end).unwrap();
        assert_eq!(
            uri,
            "rtsp://nvr:554/Streaming/tracks/301/?starttime=20261003T054000Z&endtime=20261003T054030Z"
        );
        assert!(
            bounded_recording_uri(
                "rtsp://evil.example/Streaming/tracks/301/",
                &state.nvr,
                554,
                &TrackId::new("301"),
                start,
                end
            )
            .is_err()
        );
    }

    #[test]
    fn cursor_round_trip() {
        let encoded = encode_cursor(
            "2026-07-21T00:00:00Z",
            42,
            &ImageParams::default(),
            &[7; 32],
        );
        assert_eq!(
            decode_cursor(&encoded, &ImageParams::default(), &[7; 32]).unwrap(),
            ("2026-07-21T00:00:00Z".to_string(), 42)
        );
    }

    #[tokio::test]
    async fn image_api_filters_and_returns_classification() {
        let (_root, state) = test_state().await;
        let response = test_router(state)
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
        let response = test_router(state)
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
        let response = test_router(state)
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
    async fn ui_routes_serve_embedded_application() {
        let (_root, state) = test_state().await;
        let response = test_router(state)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if assets::available() {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
    }

    #[tokio::test]
    async fn image_api_rejects_reversed_time_range() {
        let (_root, state) = test_state().await;
        let response = test_router(state)
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
    async fn get_json(state: &WebState, uri: &str) -> (StatusCode, Value) {
        let response = test_router(state.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn add_images(root: &tempfile::TempDir) {
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            root.path().join("web.sqlite3").display()
        ))
        .await
        .unwrap();
        for id in 2..=4 {
            sqlx::query("INSERT INTO images (id,image_key,camera_id,track_id,capture_start_at,playback_uri,canonical_playback_uri,download_status,processing_status,discovered_at,created_at,updated_at) VALUES (?, ?, 1, '303', '2026-07-21T10:00:00.000000000Z', 'http://nvr/still', 'http://nvr/still', 'pending', 'new', '2026-07-21T11:00:00.000000000Z', '2026-07-21T11:00:00.000000000Z', '2026-07-21T11:00:00.000000000Z')")
                .bind(id).bind(format!("image-{id}")).execute(&pool).await.unwrap();
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn explorer_filters_boundaries_and_aggregates_agree() {
        let (root, state) = test_state().await;
        add_images(&root).await;
        for (query, expected) in [
            ("from=2026-07-21T10:00:00Z&to=2026-07-21T10:00:01Z", 4),
            ("from=2026-07-21T09:00:00Z&to=2026-07-21T10:00:00Z", 0),
            ("camera=1&camera=99", 4),
            (
                "species=SQUIRREL&confidence_min=0.8&model=vision&prompt_version=wildlife-v1",
                1,
            ),
            ("species=dog", 0),
            ("q=garden", 4),
            ("q=squirrel", 1),
            ("time_field=discovered&from=2026-07-21T11:00:00Z", 3),
            ("time_field=classified&from=2026-07-21T10:00:00Z", 1),
        ] {
            let (status, images) = get_json(&state, &format!("/api/v1/images?{query}")).await;
            assert_eq!(status, StatusCode::OK, "{query}: {images}");
            assert_eq!(
                images["data"].as_array().unwrap().len(),
                expected,
                "{query}"
            );
            let (status, overview) = get_json(&state, &format!("/api/v1/overview?{query}")).await;
            assert_eq!(status, StatusCode::OK, "{query}: {overview}");
            assert_eq!(overview["counts"]["discovered"], expected, "{query}");
            let camera_total: i64 = overview["camera_counts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["discovered"].as_i64().unwrap())
                .sum();
            assert_eq!(camera_total, expected as i64, "{query}");
        }
        assert_eq!(
            get_json(&state, "/api/v1/images?typo=true").await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn all_sorts_have_stable_signed_pagination_and_neighbors() {
        let (root, state) = test_state().await;
        add_images(&root).await;
        for sort in [
            "captured_desc",
            "captured_asc",
            "confidence_desc",
            "classified_desc",
            "camera_asc",
        ] {
            let mut cursor = None::<String>;
            let mut ids = Vec::new();
            loop {
                let suffix = cursor
                    .as_ref()
                    .map(|c| format!("&cursor={c}"))
                    .unwrap_or_default();
                let (status, page) = get_json(
                    &state,
                    &format!("/api/v1/images?sort={sort}&limit=1{suffix}"),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{sort}: {page}");
                ids.push(page["data"][0]["id"].as_i64().unwrap());
                if let Some(next) = page["page"]["next_cursor"].as_str() {
                    cursor = Some(next.into());
                } else {
                    break;
                }
                assert!(ids.len() < 5);
            }
            let mut unique = ids.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique, vec![1, 2, 3, 4]);
            let (_, neighbor) = get_json(
                &state,
                &format!("/api/v1/images/{}/neighbors?sort={sort}", ids[1]),
            )
            .await;
            assert_eq!(neighbor["previous"], ids[0], "{sort}: {neighbor}");
            assert_eq!(neighbor["next"], ids[2], "{sort}: {neighbor}");
            let cursor = cursor.unwrap();
            assert_eq!(
                get_json(
                    &state,
                    &format!("/api/v1/images?sort={sort}&species=fox&cursor={cursor}")
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                get_json(
                    &state,
                    &format!("/api/v1/images?sort={sort}&cursor={cursor}x")
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[tokio::test]
    async fn thumbnail_is_resized_and_security_headers_cover_all_routes() {
        let (root, state) = test_state().await;
        let image = image::RgbImage::new(1600, 1200);
        image.save(root.path().join("images/one.jpg")).unwrap();
        let response = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images/1/thumbnail")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        let thumbnail = image::load_from_memory(&bytes).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (480, 360));
        assert_eq!(state.thumbnails.lock().await.len(), 1);
        for path in ["/api/v1/config", "/api/v1/does-not-exist", "/images/1"] {
            let response = test_router(state.clone())
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(response.headers().contains_key("content-security-policy"));
        }
    }

    #[tokio::test]
    async fn health_distinguishes_missing_fresh_and_stopped_heartbeats() {
        let (_root, state) = test_state().await;
        assert_eq!(
            get_json(&state, "/api/v1/health").await.1["pipelines"]["downloader"]["state"],
            "unknown"
        );
        let now = Timestamp::new(Utc::now());
        state.ops.set_metadata(&crate::database::models::ServiceMetadataKey::DownloaderHeartbeat,&json!({"state":"running","heartbeat_at":now.to_string(),"poll_interval_seconds":60}).to_string(),&now).await.unwrap();
        assert_eq!(
            get_json(&state, "/api/v1/health").await.1["pipelines"]["downloader"]["state"],
            "idle"
        );
        state
            .ops
            .set_metadata(
                &crate::database::models::ServiceMetadataKey::DownloaderHeartbeat,
                &json!({"state":"stopped","heartbeat_at":now.to_string()}).to_string(),
                &now,
            )
            .await
            .unwrap();
        assert_eq!(
            get_json(&state, "/api/v1/health").await.1["pipelines"]["downloader"]["state"],
            "stopped"
        );
    }
    #[tokio::test]
    async fn filesystem_filters_facets_and_descriptor_privacy() {
        let (root, state) = test_state().await;
        add_images(&root).await;
        assert_eq!(
            get_json(&state, "/api/v1/images?local_file=present")
                .await
                .1["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (status, facets) =
            get_json(&state, "/api/v1/images/facets?download_status=downloaded").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(facets["download_status"]["pending"], 3);
        assert_eq!(facets["processing_status"]["done"], 1);
        assert_eq!(
            get_json(&state, "/api/v1/images?confidence_min=NaN")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        tokio::fs::remove_file(root.path().join("images/one.jpg"))
            .await
            .unwrap();
        let (_, page) = get_json(
            &state,
            "/api/v1/images?download_status=downloaded&local_file=missing",
        )
        .await;
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        let (_, detail) = get_json(&state, "/api/v1/images/1").await;
        assert_eq!(detail["file"]["present"], false);
        let response = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
        let response = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/images/1/clip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn image_open_rejects_replaced_symlink_components() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.jpg"), b"private").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("replaced")).unwrap();
        assert!(
            open_local_image(root.path().into(), root.path().join("replaced/secret.jpg"))
                .await
                .is_err()
        );
        std::os::unix::fs::symlink(
            outside.path().join("secret.jpg"),
            root.path().join("file.jpg"),
        )
        .unwrap();
        assert!(
            open_local_image(root.path().into(), root.path().join("file.jpg"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn embedded_assets_and_stream_cover_the_production_contract() {
        let (_root, state) = test_state().await;
        if assets::available() {
            let response = test_router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/images/1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let html = String::from_utf8(
                to_bytes(response.into_body(), 1_000_000)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            let script = html
                .split("src=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert!(script.starts_with("/assets/"));
            let response = test_router(state.clone())
                .oneshot(Request::builder().uri(script).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "text/javascript; charset=utf-8"
            );
        }
        let response = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let mut stream = response.into_body().into_data_stream();
        use futures_util::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&chunk).contains("event: invalidate"));
        drop(stream);
        assert_eq!(state.event_limit.available_permits(), 32);
    }
}
