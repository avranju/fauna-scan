//! Axum-powered read-only JSON API.
//!
//! Exposes a REST API for browsing images, cameras, health, and activity.
//! All data access goes through `DatabaseOps` — the backend-neutral façade —
//! so the web layer is
//! agnostic to whether the backing store is SQLite or PostgreSQL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use url::Url;

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
    #[allow(dead_code)]
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
    Router::new()
        // API endpoints
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

// ── API: config ────────────────────────────────────────────────────────────

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
}

async fn api_health(State(state): State<WebState>) -> Result<Json<HealthResponse>, WebError> {
    let snapshot = state.ops.web_health().await.map_err(db_error)?;
    Ok(Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        web_started_at: state.started_at.to_string(),
        last_camera_discovery: snapshot.last_camera_discovery.clone(),
        last_downloader_poll: snapshot.last_downloader_poll.clone(),
        last_scanner_pass: snapshot.last_scanner_pass.clone(),
        active_downloads: snapshot.active_downloads,
        active_classifications: snapshot.active_classifications,
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
            last_error: r.last_error,
        })
        .collect();
    Ok(Json(
        json!({ "data": data, "generated_at": Timestamp::new(Utc::now()).to_string() }),
    ))
}

// ── Query parameter parsing ────────────────────────────────────────────────

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
    let limit = params.limit.unwrap_or(60).clamp(1, 200);
    let filter = build_image_filter(params)?;
    let query = WebImageQuery {
        filter,
        order: if ascending {
            WebImageOrder::CapturedAscending
        } else {
            WebImageOrder::CapturedDescending
        },
        limit,
        cursor: params.cursor.as_deref().map(decode_cursor).transpose()?,
    };

    let (summaries, next_cursor) = state.ops.web_query_images(&query).await.map_err(db_error)?;

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

    let next_cursor = next_cursor.map(|(ts, id)| encode_cursor(&ts, id));
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

    Ok(WebImageFilter {
        scope,
        download_status,
        processing_status,
        classified,
        contains_wildlife,
        is_interesting,
        confidence_min,
    })
}

// ── API: overview ──────────────────────────────────────────────────────────

async fn api_overview(
    State(state): State<WebState>,
    Query(params): Query<ImageParams>,
) -> Result<Json<Value>, WebError> {
    validate_time_range(&params)?;
    let filter = build_image_filter(&params)?;
    let record = state.ops.web_overview(&filter).await.map_err(db_error)?;
    Ok(Json(json!({
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
            "last_error": detail.download.last_error,
            "next_attempt_at": detail.download.next_attempt_at,
            "lease_until": detail.download.lease_until
        },
        "processing": {
            "status": detail.processing.status,
            "attempts": detail.processing.attempts,
            "started_at": detail.processing.started_at,
            "completed_at": detail.processing.completed_at,
            "last_error": detail.processing.last_error,
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

// ── API: image content ─────────────────────────────────────────────────────

async fn api_image_content(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
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

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| WebError::not_found("Local image file could not be read"))?;

    // Full JPEG validation: check signature and trailer
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

// ── API: recording ─────────────────────────────────────────────────────────

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
            tracing::warn!(image_id = %id, "recording search failed: {e}");
            WebError {
                status: StatusCode::BAD_GATEWAY,
                code: "recording_search_failed",
                message: "Failed to search for recording".into(),
            }
        })?;

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

// ── API: activity ──────────────────────────────────────────────────────────

async fn api_activity(
    State(state): State<WebState>,
    Query(params): Query<ImageParams>,
) -> Result<Json<Value>, WebError> {
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

    async fn test_state() -> (tempfile::TempDir, WebState) {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("images");
        std::fs::create_dir(&output).unwrap();
        let db_path = root.path().join("web.sqlite3");
        let sqlite_store = crate::database::sqlite::SqliteDataStore::connect(&db_path, 1)
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
    async fn ui_routes_are_not_served() {
        let (_root, state) = test_state().await;
        let response = router(state)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
