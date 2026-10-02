//! Concrete SQLite implementation of the `DataStore` trait.
//!
//! Wraps a `SqlitePool` with WAL mode, foreign keys, and busy timeout
//! configured. All repository operations are implemented here using the
//! existing SQLite schema.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sqlx::Row;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
};
use uuid::Uuid;

use super::models::*;
use super::repository::{
    DataStore, DatabaseOps, GcOperation, GcOutcome, RateLimitGrant, RateLimitReservation,
    RateLimitedProcessingClaim,
};
use super::web_models::*;
use super::{
    format_timestamp, map_sqlx_error, parse_download_status, parse_processing_status,
    parse_timestamp_col, parse_timestamp_col_opt,
};

use crate::domain::{
    CameraId, ClassificationId, DownloadStatus, ImageId, ImageKey, ProcessingStatus, Timestamp,
    TrackId,
};
use crate::error::{AppError, AppResult, ErrorCategory};

/// Concrete SQLite-backed DataStore.
#[derive(Clone)]
pub struct SqliteDataStore {
    pool: SqlitePool,
}

impl SqliteDataStore {
    /// Create a new SqliteDataStore, opening the pool with WAL mode,
    /// foreign keys, and busy timeout configured.
    pub async fn connect(path: &Path, max_connections: u32) -> AppResult<Self> {
        let db_path = path.to_str().ok_or_else(|| {
            AppError::new(
                ErrorCategory::Database,
                "sqlite_connect",
                "database path contains invalid UTF-8",
            )
        })?;

        let mut connect_options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));

        // WAL mode where supported (in-memory databases don't support it).
        connect_options = match path.to_str() {
            Some(p) if !p.starts_with("file::memory:") => {
                connect_options.journal_mode(SqliteJournalMode::Wal)
            }
            _ => connect_options,
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(connect_options)
            .await
            .map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "sqlite_connect",
                    format!("failed to open database at {}: {e}", path.display()),
                    e,
                )
            })?;

        // Run embedded migrations.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "sqlite_migrate",
                    format!("migration failed for database at {}: {e}", path.display()),
                    e,
                )
            })?;

        Ok(Self { pool })
    }

    /// Return the underlying pool for integration tests.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Create DatabaseOps from this store.
    pub fn ops(&self) -> DatabaseOps {
        DatabaseOps::new(Arc::new(self.clone()))
    }

    // ── Helper: fetch camera metadata ──────────────────────────────────

    async fn fetch_camera_metadata(&self, camera_id: CameraId) -> AppResult<(i64, Option<String>)> {
        let row = sqlx::query_as::<_, (i64, Option<String>)>(
            r#"SELECT channel_number, name FROM cameras WHERE id = ?"#,
        )
        .bind(camera_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("fetch_camera_metadata", e))?;

        match row {
            Some((channel_number, name)) => Ok((channel_number, name)),
            None => Err(AppError::new(
                ErrorCategory::Database,
                "fetch_camera_metadata",
                format!(
                    "camera {} not found after successful claim",
                    camera_id.get()
                ),
            )),
        }
    }

    // ── Helper: parse processing claim from row ────────────────────────

    fn processing_claim_from_row(
        row: SqliteRow,
        operation: &'static str,
    ) -> AppResult<ProcessingClaim> {
        let image_id: i64 = row.try_get(0).map_err(|e| map_sqlx_error(operation, e))?;
        let image_key: String = row.try_get(1).map_err(|e| map_sqlx_error(operation, e))?;
        let local_path: String = row.try_get(2).map_err(|e| map_sqlx_error(operation, e))?;
        let processing_attempts: i64 = row.try_get(3).map_err(|e| map_sqlx_error(operation, e))?;
        let generation: i64 = row.try_get(4).map_err(|e| map_sqlx_error(operation, e))?;
        let lease_until_str: String = row.try_get(5).map_err(|e| map_sqlx_error(operation, e))?;
        let lease_until = lease_until_str.parse::<Timestamp>().map_err(|e| {
            AppError::with_source(
                ErrorCategory::Database,
                operation,
                format!("invalid lease_until timestamp: {e}"),
                anyhow::Error::from(e),
            )
        })?;
        Ok(ProcessingClaim {
            image_id: ImageId::new(image_id),
            image_key: ImageKey::new(image_key),
            local_path: PathBuf::from(local_path),
            processing_attempts,
            generation,
            lease_until,
        })
    }

    // ── Helper: check for unreconciled wildlife references ─────────────

    async fn has_unreconciled_wildlife_file_reference(
        executor: &mut sqlx::SqliteConnection,
    ) -> Result<bool, sqlx::Error> {
        let exists: i64 = sqlx::query_scalar(
            r#"SELECT EXISTS(
                 SELECT 1 FROM images
                  WHERE local_path IS NOT NULL
                    AND (local_file_identity IS NULL
                         OR local_file_identity LIKE 'unresolved:%'
                         OR local_path != local_file_identity)
                    AND EXISTS (
                        SELECT 1 FROM classifications
                         WHERE classifications.image_id = images.id
                           AND classifications.contains_wildlife = 1
                    )
             )"#,
        )
        .fetch_one(&mut *executor)
        .await?;
        Ok(exists != 0)
    }

    // ── Web query helpers ──────────────────────────────────────────────

    async fn web_health(&self) -> AppResult<WebHealthSnapshot> {
        let last_discovery = self
            .get_metadata(&ServiceMetadataKey::LastSuccessfulCameraDiscovery)
            .await?;
        let last_downloader = self
            .get_metadata(&ServiceMetadataKey::LastSuccessfulDownloaderPoll)
            .await?;
        let last_scanner = self
            .get_metadata(&ServiceMetadataKey::LastSuccessfulScannerPass)
            .await?;

        let web_started_at_str = self
            .get_metadata(&ServiceMetadataKey::ApplicationVersion)
            .await?
            .unwrap_or_else(|| "unknown".to_string());

        let web_started_at: Timestamp = web_started_at_str
            .parse()
            .unwrap_or_else(|_| Timestamp::new(Utc::now()));

        let (active_downloads, active_classifications) = {
            let row = sqlx::query(
                r#"SELECT
                       COALESCE(SUM(CASE WHEN download_status = 'downloading' THEN 1 ELSE 0 END), 0),
                       COALESCE(SUM(CASE WHEN processing_status = 'processing' THEN 1 ELSE 0 END), 0)
                   FROM images"#,
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_health_counts", e))?;
            (row.get::<i64, _>(0), row.get::<i64, _>(1))
        };

        Ok(WebHealthSnapshot::new(
            last_discovery,
            last_downloader,
            last_scanner,
            active_downloads,
            active_classifications,
            &web_started_at,
        ))
    }

    async fn web_cameras(&self) -> AppResult<Vec<WebCameraRecord>> {
        let rows: Vec<CameraRow> = sqlx::query_as(
            r#"SELECT id, channel_number, primary_track_id, picture_track_id,
                      name, raw_discovery_identifier,
                      enabled, first_seen_at, last_seen_at, created_at, updated_at
                 FROM cameras
                 ORDER BY enabled DESC, channel_number ASC, id ASC"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("web_cameras", e))?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let record = camera_row_to_record(row)?;
            // Fetch cursor data for this camera
            let cursor_row = sqlx::query_as::<_, SearchCursorRow>(
                r#"SELECT camera_id, next_search_at, last_completed_window_start,
                          last_completed_window_end, last_poll_at, last_error, updated_at
                     FROM search_cursors WHERE camera_id = ?"#,
            )
            .bind(record.id.get())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_camera_cursor", e))?;

            let cursor = cursor_row.and_then(|c| cursor_row_to_record(c).ok());

            let last_completed_window_end = cursor
                .as_ref()
                .and_then(|c| c.last_completed_window_end)
                .map(|ts| format_timestamp(&ts));
            let last_poll_at = cursor
                .as_ref()
                .and_then(|c| c.last_poll_at)
                .map(|ts| format_timestamp(&ts));
            let next_search_at = cursor
                .as_ref()
                .and_then(|c| c.next_search_at)
                .map(|ts| format_timestamp(&ts));
            let last_error = cursor.as_ref().and_then(|c| c.last_error.clone());

            records.push(WebCameraRecord::new(
                record.id.get(),
                record.channel_number,
                record.name.clone(),
                record.enabled,
                record.primary_track_id.clone(),
                record.picture_track_id.clone(),
                format_timestamp(&record.last_seen_at),
                last_completed_window_end,
                last_poll_at,
                next_search_at,
                last_error,
            ));
        }
        Ok(records)
    }

    async fn web_query_images(
        &self,
        query: &WebImageQuery,
    ) -> AppResult<(Vec<WebImageSummaryRecord>, Option<(String, i64)>)> {
        let WebImageQuery {
            filter,
            order,
            limit,
            cursor,
        } = query;

        let (mut conditions, mut params) =
            super::web_query::predicates(filter, "classifications", false);
        let (sort_expression, direction) = super::web_query::ordering(order);
        if let Some((value, id)) = cursor {
            let comparison = if direction == "ASC" { ">" } else { "<" };
            conditions.push(format!(
                "({sort_expression}, images.id) {comparison} (?, ?)"
            ));
            if matches!(
                order,
                WebImageOrder::ConfidenceDescending | WebImageOrder::ConfidenceAscending
            ) {
                params.push(Box::new(value.parse::<f64>().map_err(|_| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "invalid_cursor",
                        "invalid confidence cursor",
                    )
                })?));
            } else {
                params.push(Box::new(value.clone()));
            }
            params.push(Box::new(*id));
        }
        let mut order_clause =
            format!("ORDER BY {sort_expression} {direction}, images.id {direction}");
        if matches!(
            order,
            WebImageOrder::CameraAscending | WebImageOrder::CameraDescending
        ) {
            let reverse = matches!(order, WebImageOrder::CameraDescending);
            order_clause = if reverse {
                "ORDER BY cameras.channel_number DESC, images.capture_start_at ASC, images.id ASC"
            } else {
                "ORDER BY cameras.channel_number ASC, images.capture_start_at DESC, images.id DESC"
            }
            .into();
            if let Some((value, id)) = cursor {
                conditions.pop();
                params.pop();
                params.pop();
                let (channel, timestamp): (i64, String) =
                    serde_json::from_str(value).map_err(|_| {
                        AppError::new(
                            ErrorCategory::Configuration,
                            "invalid_cursor",
                            "invalid camera cursor",
                        )
                    })?;
                conditions.push(if reverse {"(cameras.channel_number < ? OR (cameras.channel_number = ? AND (images.capture_start_at, images.id) > (?, ?)))"}else{"(cameras.channel_number > ? OR (cameras.channel_number = ? AND (images.capture_start_at, images.id) < (?, ?)))"}.into());
                params.push(Box::new(channel));
                params.push(Box::new(channel));
                params.push(Box::new(timestamp));
                params.push(Box::new(*id));
            }
        }
        let where_clause = format!("WHERE {}", conditions.join(" AND "));
        let query_str = format!(
            r#"SELECT images.id, images.image_key, images.capture_start_at, images.capture_end_at,
                      images.local_path, images.download_status, images.processing_status,
                      cameras.id as camera_id, cameras.channel_number, cameras.name,
                      classifications.id, classifications.contains_wildlife, classifications.is_interesting,
                      classifications.summary, classifications.species_json, classifications.confidence,
                      classifications.model, classifications.prompt_version, classifications.request_started_at,
                      classifications.request_completed_at, classifications.created_at,
                      {sort_expression} AS sort_value
               FROM images
               LEFT JOIN cameras ON cameras.id = images.camera_id
               LEFT JOIN classifications ON classifications.image_id = images.id
                   AND classifications.id = (
                       SELECT c2.id FROM classifications c2
                       WHERE c2.image_id = images.id
                       ORDER BY c2.request_completed_at DESC, c2.id DESC
                       LIMIT 1
                   )
               {where_clause} {order_clause}
               LIMIT ?"#,
        );

        let limit_i64 = (*limit as i64) + 1;
        let rows: Vec<_> = {
            let mut q = sqlx::query(&query_str);
            for param in &params {
                if let Some(s) = param.downcast_ref::<String>() {
                    q = q.bind(s);
                } else if let Some(i) = param.downcast_ref::<i64>() {
                    q = q.bind(i);
                } else if let Some(f) = param.downcast_ref::<f64>() {
                    q = q.bind(f);
                }
            }
            q.bind(limit_i64)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| map_sqlx_error("web_query_images", e))?
        };

        let has_more = rows.len() > *limit as usize;
        let row_count = if has_more {
            *limit as usize
        } else {
            rows.len()
        };
        let summaries = build_image_summaries(&rows[..row_count]).await?;
        let next_cursor = if has_more {
            let last = &rows[row_count - 1];
            let ts: String = match order {
                WebImageOrder::ConfidenceDescending | WebImageOrder::ConfidenceAscending => {
                    last.try_get::<f64, _>("sort_value").map(|v| v.to_string())
                }
                WebImageOrder::CameraAscending | WebImageOrder::CameraDescending => {
                    let channel: i64 = last
                        .try_get("sort_value")
                        .map_err(|e| map_sqlx_error("web_query_cursor", e))?;
                    let captured: String = last
                        .try_get(2)
                        .map_err(|e| map_sqlx_error("web_query_cursor", e))?;
                    Ok(serde_json::to_string(&(channel, captured)).expect("tuple serializes"))
                }
                _ => last.try_get("sort_value"),
            }
            .map_err(|e| map_sqlx_error("web_query_cursor", e))?;
            let id: i64 = last
                .try_get(0)
                .map_err(|e| map_sqlx_error("web_query_cursor", e))?;
            Some((ts, id))
        } else {
            None
        };
        Ok((summaries, next_cursor))
    }

    async fn web_overview(&self, filter: &WebImageFilter) -> AppResult<WebOverviewRecord> {
        let (conditions, params) = super::web_query::predicates(filter, "classifications", false);
        let where_clause = format!("WHERE {}", conditions.join(" AND "));
        let overview_sql = format!(
            r#"               SELECT
                   COUNT(*),
                   COALESCE(SUM(CASE WHEN images.download_status = 'downloaded' THEN 1 ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN classifications.id IS NOT NULL THEN 1 ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN classifications.contains_wildlife = 1 THEN 1 ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN classifications.is_interesting = 1 THEN 1 ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN images.download_status = 'retry_wait' OR images.processing_status = 'retry_wait' THEN 1 ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN images.download_status IN ('unavailable', 'failed') OR images.processing_status IN ('failed', 'missing') THEN 1 ELSE 0 END), 0)
               FROM images
               LEFT JOIN cameras ON cameras.id = images.camera_id
               LEFT JOIN classifications ON classifications.id = (
                   SELECT c.id FROM classifications c WHERE c.image_id = images.id
                   ORDER BY c.request_completed_at DESC, c.id DESC LIMIT 1
               )
 {where_clause}"#
        );
        let mut overview_query = sqlx::query(&overview_sql);
        for p in &params {
            if let Some(s) = p.downcast_ref::<String>() {
                overview_query = overview_query.bind(s);
            } else if let Some(i) = p.downcast_ref::<i64>() {
                overview_query = overview_query.bind(i);
            } else if let Some(f) = p.downcast_ref::<f64>() {
                overview_query = overview_query.bind(f);
            }
        }
        let row = overview_query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_overview", e))?;

        Ok(WebOverviewRecord {
            discovered: row
                .try_get(0)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
            downloaded: row
                .try_get(1)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
            classified: row
                .try_get(2)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
            wildlife: row
                .try_get(3)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
            interesting: row
                .try_get(4)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
            retryable_failures: row
                .try_get(5)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
            permanent_failures: row
                .try_get(6)
                .map_err(|e| map_sqlx_error("web_overview", e))?,
        })
    }

    async fn web_buckets(
        &self,
        filter: &WebImageFilter,
        seconds: i64,
    ) -> AppResult<Vec<WebActivityBucket>> {
        let (conditions, params) = super::web_query::predicates(filter, "classifications", false);
        let where_clause = conditions.join(" AND ");
        let sql = format!(
            r#"SELECT (CAST(strftime('%s', images.capture_start_at) AS INTEGER) / {seconds}) * {seconds} AS bucket,
            COUNT(*) AS discovered,
            COALESCE(SUM(CASE WHEN images.download_status = 'downloaded' THEN 1 ELSE 0 END), 0) AS downloaded,
            COALESCE(SUM(CASE WHEN classifications.id IS NOT NULL THEN 1 ELSE 0 END), 0) AS classified
            FROM images LEFT JOIN cameras ON cameras.id = images.camera_id
            LEFT JOIN classifications ON classifications.id = (SELECT c.id FROM classifications c WHERE c.image_id = images.id ORDER BY c.request_completed_at DESC, c.id DESC LIMIT 1)
            WHERE {where_clause} GROUP BY bucket ORDER BY bucket"#
        );

        let mut query = sqlx::query(&sql);
        for p in &params {
            if let Some(s) = p.downcast_ref::<String>() {
                query = query.bind(s);
            } else if let Some(i) = p.downcast_ref::<i64>() {
                query = query.bind(i);
            } else if let Some(f) = p.downcast_ref::<f64>() {
                query = query.bind(f);
            }
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_buckets", e))?;
        rows.into_iter()
            .map(|row| {
                let start: i64 = row
                    .try_get("bucket")
                    .map_err(|e| map_sqlx_error("web_buckets", e))?;
                Ok(WebActivityBucket {
                    start_at: chrono::DateTime::from_timestamp(start, 0)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                    end_at: chrono::DateTime::from_timestamp(start + seconds, 0)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                    discovered: row
                        .try_get("discovered")
                        .map_err(|e| map_sqlx_error("web_buckets", e))?,
                    downloaded: row
                        .try_get("downloaded")
                        .map_err(|e| map_sqlx_error("web_buckets", e))?,
                    classified: row
                        .try_get("classified")
                        .map_err(|e| map_sqlx_error("web_buckets", e))?,
                })
            })
            .collect()
    }

    async fn web_camera_counts(&self, filter: &WebImageFilter) -> AppResult<Vec<WebCameraCounts>> {
        let (conditions, params) = super::web_query::predicates(filter, "classifications", false);
        let sql = format!(
            "SELECT images.camera_id, COUNT(*) AS discovered, COUNT(classifications.id) AS classified
            FROM images LEFT JOIN cameras ON cameras.id = images.camera_id
            LEFT JOIN classifications ON classifications.id = (SELECT c.id FROM classifications c WHERE c.image_id = images.id ORDER BY c.request_completed_at DESC, c.id DESC LIMIT 1)
            WHERE {} GROUP BY images.camera_id", conditions.join(" AND ")
        );

        let mut query = sqlx::query(&sql);
        for p in &params {
            if let Some(s) = p.downcast_ref::<String>() {
                query = query.bind(s);
            } else if let Some(i) = p.downcast_ref::<i64>() {
                query = query.bind(i);
            } else if let Some(f) = p.downcast_ref::<f64>() {
                query = query.bind(f);
            }
        }
        query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_camera_counts", e))?
            .into_iter()
            .map(|row| {
                Ok(WebCameraCounts {
                    camera_id: row
                        .try_get("camera_id")
                        .map_err(|e| map_sqlx_error("web_camera_counts", e))?,
                    discovered: row
                        .try_get("discovered")
                        .map_err(|e| map_sqlx_error("web_camera_counts", e))?,
                    classified: row
                        .try_get("classified")
                        .map_err(|e| map_sqlx_error("web_camera_counts", e))?,
                })
            })
            .collect()
    }

    async fn web_image_detail(&self, image_id: ImageId) -> AppResult<Option<WebImageDetailRecord>> {
        let row = sqlx::query(
            r#"SELECT images.id, images.image_key, images.capture_start_at, images.capture_end_at,
                      images.discovered_at, images.local_path,
                      images.download_status, images.download_attempts, images.downloaded_at,
                      images.download_last_error, images.download_next_attempt_at, images.download_lease_until,
                      images.processing_status, images.processing_attempts, images.processing_started_at,
                      images.processing_completed_at, images.processing_last_error,
                      images.processing_next_attempt_at, images.processing_lease_until,
                      cameras.id as camera_id, cameras.channel_number, cameras.name,
                      cameras.primary_track_id, cameras.picture_track_id,
                      images.playback_uri, images.canonical_playback_uri
                 FROM images
                 LEFT JOIN cameras ON cameras.id = images.camera_id
                 WHERE images.id = ?"#,
        )
        .bind(image_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("web_image_detail", e))?;

        let row = match row {
            Some(r) => r,
            None => return Ok(None),
        };

        let id: i64 = row
            .try_get(0)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let image_key: String = row
            .try_get(1)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let captured_at = parse_timestamp_col(&row, 2)?;
        let capture_end_at = parse_timestamp_col_opt(&row, 3)?;
        let discovered_at = parse_timestamp_col(&row, 4)?;
        let local_path: Option<String> = row
            .try_get(5)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let download_status: String = row
            .try_get(6)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let download_attempts: i64 = row
            .try_get(7)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let downloaded_at: Option<String> = row
            .try_get(8)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let download_last_error: Option<String> = row
            .try_get(9)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let download_next_attempt_at: Option<String> = row
            .try_get(10)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let download_lease_until: Option<String> = row
            .try_get(11)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_status: String = row
            .try_get(12)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_attempts: i64 = row
            .try_get(13)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_started_at: Option<String> = row
            .try_get(14)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_completed_at: Option<String> = row
            .try_get(15)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_last_error: Option<String> = row
            .try_get(16)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_next_attempt_at: Option<String> = row
            .try_get(17)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let processing_lease_until: Option<String> = row
            .try_get(18)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let camera_id: i64 = row
            .try_get(19)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let camera_channel: i64 = row
            .try_get(20)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let camera_name: Option<String> = row
            .try_get(21)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let primary_track_id: String = row
            .try_get(22)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let picture_track_id: String = row
            .try_get(23)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let playback_uri: String = row
            .try_get(24)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;
        let canonical_nvr_url: Option<String> = row
            .try_get(25)
            .map_err(|e| map_sqlx_error("web_image_detail", e))?;

        // Fetch classifications for this image
        let class_rows: Vec<_> = sqlx::query(
            r#"SELECT id, model, prompt_version, contains_wildlife, is_interesting,
                      summary, species_json, confidence, classification_json,
                      request_started_at, request_completed_at, created_at, bounding_boxes_json
                 FROM classifications
                 WHERE image_id = ?
                 ORDER BY request_completed_at DESC, id DESC"#,
        )
        .bind(image_id.get())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("web_image_detail_classes", e))?;

        let mut classifications = Vec::with_capacity(class_rows.len());
        for cr in &class_rows {
            classifications.push(WebClassificationDetail::new(
                cr.try_get(0)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(1)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(2)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(3)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(4)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(5)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(6)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                serde_json::Value::Array(vec![]),
                cr.try_get(7)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                cr.try_get(8)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
                serde_json::Value::Object(serde_json::Map::new()),
                format_timestamp(&parse_timestamp_col(cr, 9)?),
                format_timestamp(&parse_timestamp_col(cr, 10)?),
                format_timestamp(&parse_timestamp_col(cr, 11)?),
                cr.try_get(12)
                    .map_err(|e| map_sqlx_error("web_image_detail_class", e))?,
            ));
        }

        let camera = WebDetailCamera::new(
            camera_id,
            camera_channel,
            camera_name.clone(),
            primary_track_id.clone(),
            picture_track_id,
        );

        Ok(Some(WebImageDetailRecord::new(
            id,
            image_key,
            format_timestamp(&captured_at),
            capture_end_at.map(|ts| format_timestamp(&ts)),
            format_timestamp(&discovered_at),
            camera,
            local_path,
            download_status,
            download_attempts,
            downloaded_at,
            download_last_error,
            download_next_attempt_at,
            download_lease_until,
            processing_status,
            processing_attempts,
            processing_started_at,
            processing_completed_at,
            processing_last_error,
            processing_next_attempt_at,
            processing_lease_until,
            canonical_nvr_url,
            Some(playback_uri),
            classifications,
        )))
    }

    async fn web_image_content_lookup(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebImageContentLookup>> {
        let row = sqlx::query(
            r#"SELECT images.local_path, images.download_status,
                      (SELECT c.bounding_boxes_json FROM classifications c
                       WHERE c.image_id = images.id
                       ORDER BY c.request_completed_at DESC, c.id DESC LIMIT 1)
               FROM images WHERE images.id = ?"#,
        )
        .bind(image_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("web_image_content_lookup", e))?;

        match row {
            Some(r) => {
                let local_path: Option<String> = r
                    .try_get(0)
                    .map_err(|e| map_sqlx_error("web_image_content_lookup", e))?;
                let download_status: String = r
                    .try_get(1)
                    .map_err(|e| map_sqlx_error("web_image_content_lookup", e))?;
                let bounding_boxes_json: Option<String> = r
                    .try_get(2)
                    .map_err(|e| map_sqlx_error("web_image_content_lookup", e))?;
                Ok(Some(WebImageContentLookup {
                    local_path,
                    download_status,
                    bounding_boxes_json,
                }))
            }
            None => Ok(None),
        }
    }

    async fn web_recording_target(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebRecordingTarget>> {
        let row = sqlx::query(
            r#"SELECT capture_start_at, primary_track_id
                 FROM images
                 LEFT JOIN cameras ON cameras.id = images.camera_id
                 WHERE images.id = ?"#,
        )
        .bind(image_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("web_recording_target", e))?;

        match row {
            Some(r) => {
                let capture_start_at: String = r
                    .try_get(0)
                    .map_err(|e| map_sqlx_error("web_recording_target", e))?;
                let primary_track_id: String = r
                    .try_get(1)
                    .map_err(|e| map_sqlx_error("web_recording_target", e))?;
                Ok(Some(WebRecordingTarget::new(
                    capture_start_at,
                    primary_track_id,
                )))
            }
            None => Ok(None),
        }
    }

    async fn web_activity(&self, filter: &WebImageFilter) -> AppResult<WebActivityRecord> {
        let WebImageFilter {
            scope,
            download_status: _,
            processing_status: _,
            classified: _,
            contains_wildlife: _,
            is_interesting: _,
            confidence_min: _,
            advanced: _,
        } = filter;

        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn std::any::Any + Send + Sync>> = Vec::new();

        if let Some(from) = &scope.from {
            conditions.push("capture_start_at >= ?".to_string());
            params.push(Box::new(format_timestamp(from)));
        }
        if let Some(to) = &scope.to {
            conditions.push("capture_start_at < ?".to_string());
            params.push(Box::new(format_timestamp(to)));
        }
        if !scope.camera_ids.is_empty() {
            let placeholders: Vec<String> = (0..scope.camera_ids.len())
                .map(|_| "?".to_string())
                .collect();
            conditions.push(format!("camera_id IN ({})", placeholders.join(",")));
            for cid in &scope.camera_ids {
                params.push(Box::new(*cid));
            }
        }
        let where_clause = if conditions.is_empty() {
            "WHERE 1=1".to_string()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // Separate GROUP BY queries: one per category to avoid paired-row duplication.
        // Download counts
        let dl_sql = format!(
            r#"SELECT download_status, COUNT(*), MIN(discovered_at)
               FROM images {where_clause}
               GROUP BY download_status"#,
        );
        let mut dl_query = sqlx::query_as::<_, (String, i64, Option<String>)>(&dl_sql);
        for p in &params {
            if let Some(s) = p.downcast_ref::<String>() {
                dl_query = dl_query.bind(s);
            } else if let Some(i) = p.downcast_ref::<i64>() {
                dl_query = dl_query.bind(i);
            } else if let Some(f) = p.downcast_ref::<f64>() {
                dl_query = dl_query.bind(f);
            }
        }
        let dl_rows: Vec<(String, i64, Option<String>)> = dl_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_activity_dl", e))?;

        // Processing counts
        let ps_sql = format!(
            r#"SELECT processing_status, COUNT(*), MIN(downloaded_at)
               FROM images {where_clause}
               GROUP BY processing_status"#,
        );
        let mut ps_query = sqlx::query_as::<_, (String, i64, Option<String>)>(&ps_sql);
        for p in &params {
            if let Some(s) = p.downcast_ref::<String>() {
                ps_query = ps_query.bind(s);
            } else if let Some(i) = p.downcast_ref::<i64>() {
                ps_query = ps_query.bind(i);
            } else if let Some(f) = p.downcast_ref::<f64>() {
                ps_query = ps_query.bind(f);
            }
        }
        let ps_rows: Vec<(String, i64, Option<String>)> = ps_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_activity_ps", e))?;

        let mut counts = Vec::new();
        for (status, count, oldest_at) in &dl_rows {
            counts.push(WebActivityCount {
                oldest_at: oldest_at.clone(),
                category: "download".to_string(),
                status: status.clone(),
                count: *count,
            });
        }
        for (status, count, oldest_at) in &ps_rows {
            counts.push(WebActivityCount {
                oldest_at: oldest_at.clone(),
                category: "processing".to_string(),
                status: status.clone(),
                count: *count,
            });
        }

        // Active work (downloading or processing) — ordered by updated_at DESC
        let active_sql = format!(
            r#"SELECT images.id, images.capture_start_at, cameras.name, cameras.channel_number,
                      images.download_status, images.download_attempts, images.download_lease_until,
                      images.processing_status, images.processing_attempts, images.processing_started_at,
                      images.processing_lease_until
                 FROM images
                 LEFT JOIN cameras ON cameras.id = images.camera_id
                 {where_clause}
                 AND (images.download_status = 'downloading' OR images.processing_status = 'processing')
                 ORDER BY images.updated_at DESC
                 LIMIT 100"#,
        );
        let mut active_query = sqlx::query(&active_sql);
        for p in &params {
            if let Some(s) = p.downcast_ref::<String>() {
                active_query = active_query.bind(s);
            } else if let Some(i) = p.downcast_ref::<i64>() {
                active_query = active_query.bind(i);
            } else if let Some(f) = p.downcast_ref::<f64>() {
                active_query = active_query.bind(f);
            }
        }
        let active_rows: Vec<_> = active_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("web_activity_active", e))?;

        let mut active = Vec::new();
        for row in active_rows {
            let id: i64 = row
                .try_get(0)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let captured_at: String = row
                .try_get(1)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let camera_name: Option<String> = row
                .try_get(2)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let channel: i64 = row
                .try_get(3)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let download_status: String = row
                .try_get(4)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let download_attempts: i64 = row
                .try_get(5)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let download_lease_until: Option<String> = row
                .try_get(6)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let processing_status: String = row
                .try_get(7)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let processing_attempts: i64 = row
                .try_get(8)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let processing_started_at: Option<String> = row
                .try_get(9)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;
            let processing_lease_until: Option<String> = row
                .try_get(10)
                .map_err(|e| map_sqlx_error("web_activity_active", e))?;

            active.push(WebActivityActive {
                id,
                captured_at,
                camera_name,
                channel,
                download_status,
                download_attempts,
                download_lease_until,
                processing_status,
                processing_attempts,
                processing_started_at,
                processing_lease_until,
            });
        }

        let now = Timestamp::new(Utc::now());
        Ok(WebActivityRecord {
            counts,
            active,
            generated_at: format_timestamp(&now),
        })
    }
}

/// Build image summary records from web query rows.
async fn build_image_summaries(
    rows: &[sqlx::sqlite::SqliteRow],
) -> AppResult<Vec<WebImageSummaryRecord>> {
    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row
            .try_get(0)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let _image_key: String = row
            .try_get(1)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let captured_at: String = row
            .try_get(2)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let capture_end_at: Option<String> = row
            .try_get(3)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let local_path: Option<String> = row
            .try_get(4)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let download_status: String = row
            .try_get(5)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let processing_status: String = row
            .try_get(6)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let camera_id: i64 = row
            .try_get(7)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let camera_channel: i64 = row
            .try_get(8)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let camera_name: Option<String> = row
            .try_get(9)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;

        // Optional classification fields
        let class_id: Option<i64> = row
            .try_get(10)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_contains_wildlife: Option<i64> = row
            .try_get(11)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_is_interesting: Option<i64> = row
            .try_get(12)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_summary: Option<String> = row
            .try_get(13)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_species_json: Option<String> = row
            .try_get(14)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_confidence: Option<f64> = row
            .try_get(15)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_model: Option<String> = row
            .try_get(16)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_prompt_version: Option<String> = row
            .try_get(17)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let _class_started: Option<String> = row
            .try_get(18)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let class_completed: Option<String> = row
            .try_get(19)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;
        let _class_created: Option<String> = row
            .try_get(20)
            .map_err(|e| map_sqlx_error("build_summaries", e))?;

        let classification = match class_id {
            Some(cid) => {
                let contains_wildlife = class_contains_wildlife.unwrap_or(0) != 0;
                let is_interesting = class_is_interesting.unwrap_or(0) != 0;
                let model = class_model.unwrap_or_default();
                let prompt_version = class_prompt_version.unwrap_or_default();
                let completed_at = class_completed.unwrap_or_default();

                Some(WebClassificationSummary::new(
                    cid,
                    contains_wildlife,
                    is_interesting,
                    class_summary,
                    class_species_json,
                    serde_json::Value::Array(vec![]),
                    class_confidence,
                    model,
                    prompt_version,
                    completed_at,
                ))
            }
            None => None,
        };

        let camera = WebCameraSummary {
            id: camera_id,
            name: camera_name.clone(),
            channel: camera_channel,
        };

        summaries.push(WebImageSummaryRecord::new(
            id,
            captured_at,
            capture_end_at,
            camera,
            local_path,
            download_status,
            processing_status,
            classification,
        ));
    }
    Ok(summaries)
}

// ── DataStore trait implementation for SqliteDataStore ──────────────────

#[async_trait]
impl DataStore for SqliteDataStore {
    // ── Camera operations ───────────────────────────────────────────────

    async fn list_active_cameras(&self) -> AppResult<Vec<CameraRecord>> {
        let rows: Vec<CameraRow> = sqlx::query_as(
            r#"SELECT id, channel_number, primary_track_id, picture_track_id,
                      name, raw_discovery_identifier,
                      enabled, first_seen_at, last_seen_at, created_at, updated_at
                 FROM cameras
                 WHERE enabled = 1
                 ORDER BY channel_number ASC, id ASC"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("list_active_cameras", e))?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            records.push(camera_row_to_record(row)?);
        }
        Ok(records)
    }

    async fn sync_cameras(
        &self,
        cameras: &[CameraDiscovery],
        observed_at: &Timestamp,
    ) -> AppResult<Vec<CameraRecord>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_sqlx_error("sync_cameras", e))?;

        let now = format_timestamp(observed_at);
        let mut records = Vec::with_capacity(cameras.len());

        for cam in cameras {
            let _result = sqlx::query(
                r#"INSERT INTO cameras (channel_number, primary_track_id, picture_track_id, name,
                                         raw_discovery_identifier, enabled, first_seen_at, last_seen_at,
                                         created_at, updated_at)
                   VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?)
                   ON CONFLICT(picture_track_id) DO UPDATE SET
                       channel_number = excluded.channel_number,
                       primary_track_id = excluded.primary_track_id,
                       name = excluded.name,
                       raw_discovery_identifier = excluded.raw_discovery_identifier,
                       enabled = 1,
                       last_seen_at = excluded.last_seen_at,
                       updated_at = excluded.updated_at"#,
            )
            .bind(cam.channel_number)
            .bind(&cam.primary_track_id)
            .bind(&cam.picture_track_id)
            .bind(&cam.name)
            .bind(&cam.raw_discovery_identifier)
            .bind(now.clone())
            .bind(now.clone())
            .bind(now.clone())
            .bind(now.clone())
            .execute(tx.as_mut())
            .await
            .map_err(|e| map_sqlx_error("sync_cameras_insert", e))?;

            let row = sqlx::query_as::<_, CameraRow>(
                r#"SELECT id, channel_number, primary_track_id, picture_track_id,
                          name, raw_discovery_identifier,
                          enabled, first_seen_at, last_seen_at, created_at, updated_at
                     FROM cameras WHERE picture_track_id = ?"#,
            )
            .bind(&cam.picture_track_id)
            .fetch_one(tx.as_mut())
            .await
            .map_err(|e| map_sqlx_error("sync_cameras_fetch", e))?;

            records.push(camera_row_to_record(row)?);
        }

        // Mark absent cameras inactive
        let picture_ids: Vec<&str> = cameras
            .iter()
            .map(|c| c.picture_track_id.as_str())
            .collect();
        if !picture_ids.is_empty() {
            let placeholders: Vec<String> =
                (0..picture_ids.len()).map(|_| "?".to_string()).collect();
            let query = format!(
                "UPDATE cameras SET enabled = 0, updated_at = ? WHERE enabled = 1 AND picture_track_id NOT IN ({})",
                placeholders.join(",")
            );
            let mut q = sqlx::query(&query).bind(&now);
            for pid in &picture_ids {
                q = q.bind(pid);
            }
            q.execute(tx.as_mut())
                .await
                .map_err(|e| map_sqlx_error("sync_cameras_deactivate", e))?;
        } else {
            sqlx::query("UPDATE cameras SET enabled = 0, updated_at = ? WHERE enabled = 1")
                .bind(&now)
                .execute(tx.as_mut())
                .await
                .map_err(|e| map_sqlx_error("sync_cameras_deactivate_all", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("sync_cameras_commit", e))?;
        Ok(records)
    }

    // ── Image discovery and cursor commit ───────────────────────────────

    async fn commit_search_window(
        &self,
        window: &SearchWindowCommit,
        images: &[DiscoveredImage],
    ) -> AppResult<u64> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_sqlx_error("commit_search_window", e))?;

        if images.is_empty() {
            let window_start = format_timestamp(&window.window_start);
            let window_end = format_timestamp(&window.window_end);
            let next_search = format_timestamp(&window.next_search_at);
            let polled = format_timestamp(&window.polled_at);
            let updated = format_timestamp(&window.updated_at);

            sqlx::query(
                r#"INSERT INTO search_cursors (camera_id, last_completed_window_start,
                                               last_completed_window_end, next_search_at,
                                               last_poll_at, updated_at)
                   VALUES (?, ?, ?, ?, ?, ?)
                   ON CONFLICT(camera_id) DO UPDATE SET
                       last_completed_window_start = CASE
                           WHEN search_cursors.last_completed_window_start IS NULL
                               THEN excluded.last_completed_window_start
                           WHEN excluded.last_completed_window_start IS NULL
                               THEN search_cursors.last_completed_window_start
                           WHEN excluded.last_completed_window_start > search_cursors.last_completed_window_start
                               THEN excluded.last_completed_window_start
                           ELSE search_cursors.last_completed_window_start
                       END,
                       last_completed_window_end = CASE
                           WHEN search_cursors.last_completed_window_end IS NULL
                               THEN excluded.last_completed_window_end
                           WHEN excluded.last_completed_window_end IS NULL
                               THEN search_cursors.last_completed_window_end
                           WHEN excluded.last_completed_window_end > search_cursors.last_completed_window_end
                               THEN excluded.last_completed_window_end
                           ELSE search_cursors.last_completed_window_end
                       END,
                       next_search_at = MAX(
                           COALESCE(search_cursors.next_search_at, '0000-00-00T00:00:00Z'),
                           excluded.next_search_at),
                       last_poll_at = excluded.last_poll_at,
                       last_error = NULL,
                       updated_at = excluded.updated_at"#,
            )
            .bind(window.camera_id.get())
            .bind(&window_start)
            .bind(&window_end)
            .bind(&next_search)
            .bind(&polled)
            .bind(&updated)
            .execute(tx.as_mut())
            .await
            .map_err(|e| map_sqlx_error("commit_search_window_cursor", e))?;

            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("commit_search_window_commit", e))?;
            return Ok(0);
        }

        let batch_discovered_at = format_timestamp(&images.first().unwrap().discovered_at);
        let mut new_count: u64 = 0;
        let mut was_insert = Vec::with_capacity(images.len());

        for img in images {
            let discovered_at = format_timestamp(&img.discovered_at);
            let capture_start = format_timestamp(&img.capture_start_at);
            let capture_end = img.capture_end_at.as_ref().map(format_timestamp);

            let result = sqlx::query(
                r#"INSERT OR IGNORE INTO images (
                       image_key, camera_id, track_id, capture_start_at, capture_end_at,
                       playback_uri, canonical_playback_uri, codec_type, content_type,
                       nvr_reported_size, discovered_at, created_at, updated_at,
                       download_status, download_attempts, downloaded_at,
                       download_last_error, download_next_attempt_at, download_lease_until,
                       processing_status, processing_attempts, processing_started_at,
                       processing_completed_at, processing_last_error,
                       processing_next_attempt_at, processing_lease_until
                   ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                             'pending', 0, NULL, NULL, NULL, NULL,
                             'new', 0, NULL, NULL, NULL, NULL, NULL)"#,
            )
            .bind(img.image_key.as_str())
            .bind(img.camera_id.get())
            .bind(img.track_id.as_str())
            .bind(&capture_start)
            .bind(capture_end)
            .bind(&img.playback_uri)
            .bind(&img.canonical_playback_uri)
            .bind(&img.codec_type)
            .bind(&img.content_type)
            .bind(img.nvr_reported_size)
            .bind(&discovered_at)
            .bind(&batch_discovered_at)
            .bind(&batch_discovered_at)
            .execute(tx.as_mut())
            .await
            .map_err(|e| map_sqlx_error("commit_search_window_insert", e))?;

            let inserted = result.rows_affected() > 0;
            was_insert.push(inserted);
            if inserted {
                new_count += 1;
            }
        }

        let batch_updated_at = format_timestamp(&window.updated_at);
        for (img, inserted) in images.iter().zip(was_insert.iter()) {
            if *inserted {
                continue;
            }
            let capture_start = format_timestamp(&img.capture_start_at);
            let capture_end = img.capture_end_at.as_ref().map(format_timestamp);

            sqlx::query(
                r#"UPDATE images SET
                       track_id = ?,
                       capture_start_at = ?,
                       capture_end_at = COALESCE(?, images.capture_end_at),
                       playback_uri = ?,
                       canonical_playback_uri = ?,
                       codec_type = COALESCE(?, images.codec_type),
                       content_type = COALESCE(?, images.content_type),
                       nvr_reported_size = COALESCE(?, images.nvr_reported_size),
                       updated_at = ?
                   WHERE image_key = ?"#,
            )
            .bind(img.track_id.as_str())
            .bind(&capture_start)
            .bind(capture_end)
            .bind(&img.playback_uri)
            .bind(&img.canonical_playback_uri)
            .bind(&img.codec_type)
            .bind(&img.content_type)
            .bind(img.nvr_reported_size)
            .bind(&batch_updated_at)
            .bind(img.image_key.as_str())
            .execute(tx.as_mut())
            .await
            .map_err(|e| map_sqlx_error("commit_search_window_update", e))?;
        }

        let window_start = format_timestamp(&window.window_start);
        let window_end = format_timestamp(&window.window_end);
        let next_search = format_timestamp(&window.next_search_at);
        let polled = format_timestamp(&window.polled_at);
        let updated = format_timestamp(&window.updated_at);

        sqlx::query(
            r#"INSERT INTO search_cursors (camera_id, last_completed_window_start,
                                           last_completed_window_end, next_search_at,
                                           last_poll_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?)
               ON CONFLICT(camera_id) DO UPDATE SET
                   last_completed_window_start = CASE
                       WHEN search_cursors.last_completed_window_start IS NULL
                           THEN excluded.last_completed_window_start
                       WHEN excluded.last_completed_window_start IS NULL
                           THEN search_cursors.last_completed_window_start
                       WHEN excluded.last_completed_window_start > search_cursors.last_completed_window_start
                           THEN excluded.last_completed_window_start
                       ELSE search_cursors.last_completed_window_start
                   END,
                   last_completed_window_end = CASE
                       WHEN search_cursors.last_completed_window_end IS NULL
                           THEN excluded.last_completed_window_end
                       WHEN excluded.last_completed_window_end IS NULL
                           THEN search_cursors.last_completed_window_end
                       WHEN excluded.last_completed_window_end > search_cursors.last_completed_window_end
                           THEN excluded.last_completed_window_end
                       ELSE search_cursors.last_completed_window_end
                   END,
                   next_search_at = MAX(
                       COALESCE(search_cursors.next_search_at, '0000-00-00T00:00:00Z'),
                       excluded.next_search_at),
                   last_poll_at = excluded.last_poll_at,
                   last_error = NULL,
                   updated_at = excluded.updated_at"#,
        )
        .bind(window.camera_id.get())
        .bind(&window_start)
        .bind(&window_end)
        .bind(&next_search)
        .bind(&polled)
        .bind(&updated)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("commit_search_window_cursor", e))?;

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("commit_search_window_commit", e))?;
        Ok(new_count)
    }

    async fn record_cursor_error(
        &self,
        camera_id: CameraId,
        error_msg: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        let updated = format_timestamp(updated_at);

        sqlx::query(
            r#"INSERT INTO search_cursors (camera_id, next_search_at,
                                           last_completed_window_start, last_completed_window_end,
                                           last_poll_at, last_error, updated_at)
               VALUES (?, NULL, NULL, NULL, NULL, ?, ?)
               ON CONFLICT(camera_id) DO UPDATE SET
                   last_error = excluded.last_error,
                   updated_at = excluded.updated_at"#,
        )
        .bind(camera_id.get())
        .bind(error_msg)
        .bind(&updated)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("record_cursor_error", e))?;
        Ok(())
    }

    async fn get_cursor(&self, camera_id: CameraId) -> AppResult<Option<SearchCursorRecord>> {
        let row = sqlx::query_as::<_, SearchCursorRow>(
            r#"SELECT camera_id, next_search_at, last_completed_window_start,
                      last_completed_window_end, last_poll_at, last_error, updated_at
                 FROM search_cursors WHERE camera_id = ?"#,
        )
        .bind(camera_id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("get_cursor", e))?;
        row.map(cursor_row_to_record).transpose()
    }

    // ── Download claiming and transitions ───────────────────────────────

    async fn claim_next_download(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<DownloadClaim>> {
        let now_str = format_timestamp(now);
        let lease_str = format_timestamp(lease_until);

        let row = sqlx::query(
            r#"UPDATE images
               SET download_status = 'downloading',
                   download_attempts = download_attempts + 1,
                   download_lease_until = ?,
                   download_next_attempt_at = NULL,
                   updated_at = ?
               WHERE id = (
                   SELECT id FROM images
                   WHERE download_status IN ('pending', 'retry_wait')
                     AND (download_status != 'retry_wait'
                          OR download_next_attempt_at IS NULL
                          OR download_next_attempt_at <= ?)
                     AND download_lease_until IS NULL
                   ORDER BY id ASC
                   LIMIT 1
               )
               RETURNING id, image_key, camera_id, track_id,
                         capture_start_at, playback_uri, canonical_playback_uri,
                         download_attempts, download_lease_until,
                         nvr_reported_size"#,
        )
        .bind(&lease_str)
        .bind(&now_str)
        .bind(&now_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("claim_next_download", e))?;

        match row {
            Some(row) => {
                let image_id: i64 = row
                    .try_get(0)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let image_key: String = row
                    .try_get(1)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let camera_id: i64 = row
                    .try_get(2)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let track_id: String = row
                    .try_get(3)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let capture_start_at = parse_timestamp_col(&row, 4)?;
                let playback_uri: String = row
                    .try_get(5)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let canonical_playback_uri: String = row
                    .try_get(6)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let download_attempts: i64 = row
                    .try_get(7)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let lease_until_str: String = row
                    .try_get(8)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;
                let lease_until_ts = lease_until_str.parse::<Timestamp>().map_err(|e| {
                    AppError::with_source(
                        ErrorCategory::Database,
                        "claim_next_download",
                        format!("invalid lease_until timestamp: {e}"),
                        anyhow::Error::from(e),
                    )
                })?;
                let nvr_reported_size: Option<i64> = row
                    .try_get(9)
                    .map_err(|e| map_sqlx_error("claim_next_download", e))?;

                let (channel_number, camera_name) =
                    self.fetch_camera_metadata(CameraId::new(camera_id)).await?;

                Ok(Some(DownloadClaim {
                    image_id: ImageId::new(image_id),
                    image_key: ImageKey::new(image_key),
                    camera_id: CameraId::new(camera_id),
                    track_id: TrackId::new(track_id),
                    capture_start_at,
                    playback_uri,
                    canonical_playback_uri,
                    download_attempts,
                    lease_until: lease_until_ts,
                    camera_channel_number: channel_number,
                    camera_name,
                    nvr_reported_size,
                }))
            }
            None => Ok(None),
        }
    }

    async fn complete_download(
        &self,
        image_id: ImageId,
        local_path: &Path,
        downloaded_at: &Timestamp,
    ) -> AppResult<()> {
        let (path_str, file_identity) = match tokio::fs::canonicalize(local_path).await {
            Ok(path) => {
                let canonical = path.to_string_lossy().to_string();
                (canonical.clone(), canonical)
            }
            Err(_) => {
                let path = local_path.to_string_lossy().to_string();
                (path.clone(), format!("unresolved:{path}"))
            }
        };
        let downloaded_str = format_timestamp(downloaded_at);
        let updated_at = format_timestamp(downloaded_at);

        let result = sqlx::query(
            r#"UPDATE images SET
                   download_status = 'downloaded',
                   local_path = ?,
                   local_file_identity = ?,
                   downloaded_at = ?,
                   download_lease_until = NULL,
                   download_next_attempt_at = NULL,
                   download_last_error = NULL,
                   updated_at = ?
               WHERE id = ? AND download_status = 'downloading'"#,
        )
        .bind(&path_str)
        .bind(&file_identity)
        .bind(&downloaded_str)
        .bind(&updated_at)
        .bind(image_id.get())
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("complete_download", e))?;

        if result.rows_affected() == 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "complete_download",
                format!("image {} is not in downloading state", image_id.get()),
            ));
        }
        Ok(())
    }

    async fn fail_download(
        &self,
        image_id: ImageId,
        error: &str,
        disposition: DownloadFailureDisposition,
        now: &Timestamp,
    ) -> AppResult<()> {
        let (status, next_attempt_at) = match &disposition {
            DownloadFailureDisposition::RetryWait { next_attempt_at } => {
                ("retry_wait", Some(next_attempt_at))
            }
            DownloadFailureDisposition::Unavailable => ("unavailable", None),
            DownloadFailureDisposition::Failed => ("failed", None),
        };

        let next_attempt_str = next_attempt_at.map(format_timestamp);
        let updated_at = format_timestamp(now);

        let result = sqlx::query(
            r#"UPDATE images SET
                   download_status = ?,
                   download_last_error = ?,
                   download_lease_until = NULL,
                   download_next_attempt_at = CASE ?
                       WHEN 'retry_wait' THEN ?
                       ELSE NULL
                   END,
                   updated_at = ?
               WHERE id = ? AND download_status = 'downloading'"#,
        )
        .bind(status)
        .bind(error)
        .bind(status)
        .bind(&next_attempt_str)
        .bind(&updated_at)
        .bind(image_id.get())
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("fail_download", e))?;

        if result.rows_affected() == 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "fail_download",
                format!("image {} is not in downloading state", image_id.get()),
            ));
        }
        Ok(())
    }

    // ── Processing claiming and transitions ─────────────────────────────

    async fn has_eligible_processing(&self, now: &Timestamp) -> AppResult<bool> {
        let now = format_timestamp(now);
        let row = sqlx::query("SELECT EXISTS(SELECT 1 FROM images WHERE processing_status IN ('new', 'retry_wait') AND (processing_status != 'retry_wait' OR processing_next_attempt_at IS NULL OR processing_next_attempt_at <= ?) AND local_path IS NOT NULL AND download_status = 'downloaded' AND processing_lease_until IS NULL)")
            .bind(now).fetch_one(&self.pool).await.map_err(|e| map_sqlx_error("has_eligible_processing", e))?;
        Ok(row.get::<i64, _>(0) != 0)
    }

    async fn claim_next_processing(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<ProcessingClaim>> {
        let now_str = format_timestamp(now);
        let lease_str = format_timestamp(lease_until);

        let row = sqlx::query(
            r#"UPDATE images
               SET processing_status = 'processing',
                   processing_attempts = processing_attempts + 1,
                   processing_generation = processing_generation + 1,
                   processing_started_at = ?,
                   processing_lease_until = ?,
                   updated_at = ?
               WHERE id = (
                   SELECT id FROM images
                   WHERE processing_status IN ('new', 'retry_wait')
                     AND (processing_status != 'retry_wait'
                          OR processing_next_attempt_at IS NULL
                          OR processing_next_attempt_at <= ?)
                     AND local_path IS NOT NULL
                     AND download_status = 'downloaded'
                     AND processing_lease_until IS NULL
                   ORDER BY id ASC
                   LIMIT 1
               )
               RETURNING id, image_key, local_path, processing_attempts,
                         processing_generation, processing_lease_until"#,
        )
        .bind(&now_str)
        .bind(&lease_str)
        .bind(&now_str)
        .bind(&now_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing", e))?;

        match row {
            Some(row) => Ok(Some(Self::processing_claim_from_row(
                row,
                "claim_next_processing",
            )?)),
            None => Ok(None),
        }
    }

    async fn classifier_cooldown_remaining(
        &self,
        cooldown_group: &str,
        now: &Timestamp,
    ) -> AppResult<Option<Duration>> {
        let now_str = format_timestamp(now);
        let cooldown_until: Option<String> = sqlx::query_scalar(
            "SELECT cooldown_until FROM classifier_cooldowns \
             WHERE cooldown_group = ? AND cooldown_until > ?",
        )
        .bind(cooldown_group)
        .bind(now_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("classifier_cooldown_remaining", e))?;
        let Some(cooldown_until) = cooldown_until else {
            return Ok(None);
        };
        let cooldown_until = cooldown_until.parse::<Timestamp>().map_err(|_| {
            AppError::new(
                ErrorCategory::Database,
                "classifier_cooldown_remaining",
                "stored classifier cooldown timestamp is invalid",
            )
        })?;
        let remaining = (*cooldown_until.as_datetime() - *now.as_datetime())
            .to_std()
            .unwrap_or(Duration::ZERO);
        Ok(Some(remaining.max(Duration::from_millis(1))))
    }

    async fn set_classifier_cooldown(
        &self,
        cooldown_group: &str,
        cooldown_until: &Timestamp,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO classifier_cooldowns (cooldown_group, cooldown_until, updated_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT(cooldown_group) DO UPDATE SET \
               cooldown_until = MAX(classifier_cooldowns.cooldown_until, excluded.cooldown_until), \
               updated_at = excluded.updated_at",
        )
        .bind(cooldown_group)
        .bind(format_timestamp(cooldown_until))
        .bind(format_timestamp(updated_at))
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("set_classifier_cooldown", e))?;
        Ok(())
    }

    async fn fail_processing(
        &self,
        image_id: ImageId,
        error: &str,
        raw_response: Option<String>,
        generation: i64,
        disposition: ProcessingFailureDisposition,
        now: &Timestamp,
    ) -> AppResult<()> {
        let (status, next_attempt_at) = match &disposition {
            ProcessingFailureDisposition::RetryWait { next_attempt_at } => {
                ("retry_wait", Some(next_attempt_at))
            }
            ProcessingFailureDisposition::Failed => ("failed", None),
            ProcessingFailureDisposition::Missing => ("missing", None),
        };

        let next_attempt_str = next_attempt_at.map(format_timestamp);
        let updated_at = format_timestamp(now);

        let result = sqlx::query(
            r#"UPDATE images SET
                   processing_status = ?,
                   processing_last_error = ?,
                   processing_last_raw_response = COALESCE(NULLIF(?, ''),
                                                          processing_last_raw_response),
                   processing_lease_until = NULL,
                   processing_next_attempt_at = CASE ?
                       WHEN 'retry_wait' THEN ?
                       ELSE NULL
                   END,
                   updated_at = ?
               WHERE id = ? AND processing_status = 'processing'
                 AND processing_generation = ?"#,
        )
        .bind(status)
        .bind(error)
        .bind(&raw_response)
        .bind(status)
        .bind(&next_attempt_str)
        .bind(&updated_at)
        .bind(image_id.get())
        .bind(generation)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("fail_processing", e))?;

        if result.rows_affected() == 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "fail_processing",
                format!(
                    "image {} is not in processing state or generation mismatch",
                    image_id.get()
                ),
            ));
        }
        Ok(())
    }

    async fn complete_classification(
        &self,
        image_id: ImageId,
        classification: &ClassificationInput,
        generation: i64,
        completed_at: &Timestamp,
    ) -> AppResult<ClassificationId> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("complete_classification", e))?;

        let row = sqlx::query(
            r#"SELECT processing_status, processing_generation
                 FROM images WHERE id = ?"#,
        )
        .bind(image_id.get())
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("complete_classification_check", e))?;

        let current_status: String = row
            .try_get(0)
            .map_err(|e| map_sqlx_error("complete_classification_check", e))?;
        if current_status != "processing" {
            return Err(AppError::new(
                ErrorCategory::Database,
                "complete_classification",
                format!(
                    "image {} is not in processing state (is '{}')",
                    image_id.get(),
                    current_status
                ),
            ));
        }

        let current_generation: i64 = row
            .try_get(1)
            .map_err(|e| map_sqlx_error("complete_classification_check", e))?;
        if current_generation != generation {
            return Err(AppError::new(
                ErrorCategory::Database,
                "complete_classification",
                format!(
                    "image {} generation mismatch (expected {}, got {})",
                    image_id.get(),
                    generation,
                    current_generation
                ),
            ));
        }

        let completed_str = format_timestamp(completed_at);
        let started_str = format_timestamp(&classification.request_started_at);
        let completed_req_str = format_timestamp(&classification.request_completed_at);
        let created_str = &completed_str;
        let updated_at = &completed_str;

        let _result = sqlx::query(
            r#"INSERT INTO classifications (
                   image_id, model, prompt_version, contains_wildlife, is_interesting,
                   summary, species_json, confidence, classification_json, raw_response,
                   request_started_at, request_completed_at, created_at, bounding_boxes_json
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(image_id.get())
        .bind(&classification.model)
        .bind(&classification.prompt_version)
        .bind(classification.contains_wildlife as i64)
        .bind(classification.is_interesting as i64)
        .bind(&classification.summary)
        .bind(&classification.species_json)
        .bind(classification.confidence)
        .bind(&classification.classification_json)
        .bind(&classification.raw_response)
        .bind(&started_str)
        .bind(&completed_req_str)
        .bind(created_str)
        .bind(&classification.bounding_boxes_json)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("complete_classification_insert", e))?;

        let update_result = sqlx::query(
            r#"UPDATE images SET
                   processing_status = 'done',
                   processing_completed_at = ?,
                   processing_lease_until = NULL,
                   processing_next_attempt_at = NULL,
                   processing_last_error = NULL,
                   processing_last_raw_response = NULL,
                   updated_at = ?
               WHERE id = ? AND processing_status = 'processing'
                 AND processing_generation = ?"#,
        )
        .bind(&completed_str)
        .bind(updated_at)
        .bind(image_id.get())
        .bind(generation)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("complete_classification_update", e))?;

        if update_result.rows_affected() != 1 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "complete_classification",
                format!(
                    "image {} did not transition to done (rows affected: {})",
                    image_id.get(),
                    update_result.rows_affected()
                ),
            ));
        }

        let class_id: i64 = sqlx::query_scalar(
            "SELECT id FROM classifications WHERE image_id = ? AND model = ? AND prompt_version = ?",
        )
        .bind(image_id.get())
        .bind(&classification.model)
        .bind(&classification.prompt_version)
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("complete_classification_fetch", e))?;

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("complete_classification_commit", e))?;
        Ok(ClassificationId::new(class_id))
    }

    // ── Lease management ────────────────────────────────────────────────

    async fn renew_processing_lease(
        &self,
        image_id: ImageId,
        generation: i64,
        new_lease_until: &Timestamp,
        renewal_at: &Timestamp,
    ) -> AppResult<()> {
        let lease_str = format_timestamp(new_lease_until);
        let updated_at = format_timestamp(renewal_at);

        let result = sqlx::query(
            r#"UPDATE images SET
                   processing_lease_until = ?,
                   updated_at = ?
               WHERE id = ? AND processing_status = 'processing'
                 AND processing_generation = ?"#,
        )
        .bind(&lease_str)
        .bind(&updated_at)
        .bind(image_id.get())
        .bind(generation)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("renew_processing_lease", e))?;

        if result.rows_affected() == 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "renew_processing_lease",
                format!(
                    "image {} is no longer in processing state or generation mismatch",
                    image_id.get()
                ),
            ));
        }
        Ok(())
    }

    async fn verify_processing_ownership(
        &self,
        image_id: ImageId,
        generation: i64,
    ) -> AppResult<()> {
        let row = sqlx::query(
            r#"SELECT processing_status, processing_lease_until, processing_generation
                 FROM images WHERE id = ?"#,
        )
        .bind(image_id.get())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("verify_processing_ownership", e))?;

        let status: String = row
            .try_get(0)
            .map_err(|e| map_sqlx_error("verify_processing_ownership", e))?;
        if status != "processing" {
            return Err(AppError::new(
                ErrorCategory::Database,
                "verify_processing_ownership",
                format!(
                    "image {} ownership lost: processing_status is '{}'",
                    image_id.get(),
                    status
                ),
            ));
        }

        let lease_until: Option<String> = row
            .try_get(1)
            .map_err(|e| map_sqlx_error("verify_processing_ownership", e))?;
        if lease_until.is_none() {
            return Err(AppError::new(
                ErrorCategory::Database,
                "verify_processing_ownership",
                format!(
                    "image {} ownership lost: processing_lease_until is NULL",
                    image_id.get()
                ),
            ));
        }

        let current_generation: i64 = row
            .try_get(2)
            .map_err(|e| map_sqlx_error("verify_processing_ownership", e))?;
        if current_generation != generation {
            return Err(AppError::new(
                ErrorCategory::Database,
                "verify_processing_ownership",
                format!(
                    "image {} ownership lost: generation mismatch (expected {}, got {})",
                    image_id.get(),
                    generation,
                    current_generation
                ),
            ));
        }

        Ok(())
    }

    async fn recover_expired_leases(&self, now: &Timestamp) -> AppResult<LeaseRecoveryCounts> {
        let now_str = format_timestamp(now);

        let download_changes = sqlx::query(
            r#"UPDATE images SET
                   download_status = 'retry_wait',
                   download_next_attempt_at = ?,
                   download_lease_until = NULL,
                   updated_at = ?
               WHERE download_status = 'downloading'
                 AND download_lease_until IS NOT NULL
                 AND download_lease_until <= ?"#,
        )
        .bind(&now_str)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("recover_expired_leases_download", e))?;

        let processing_changes = sqlx::query(
            r#"UPDATE images SET
                   processing_status = 'retry_wait',
                   processing_next_attempt_at = ?,
                   processing_lease_until = NULL,
                   updated_at = ?
               WHERE processing_status = 'processing'
                 AND processing_lease_until IS NOT NULL
                 AND processing_lease_until <= ?"#,
        )
        .bind(&now_str)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("recover_expired_leases_processing", e))?;

        Ok(LeaseRecoveryCounts {
            downloads: download_changes.rows_affected() as u64,
            processing: processing_changes.rows_affected() as u64,
        })
    }

    // ── Service metadata ────────────────────────────────────────────────

    async fn set_metadata(
        &self,
        key: &ServiceMetadataKey,
        value: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        let updated_str = format_timestamp(updated_at);
        sqlx::query(
            r#"INSERT INTO service_metadata (key, value, updated_at)
               VALUES (?, ?, ?)
               ON CONFLICT(key) DO UPDATE SET
                   value = excluded.value,
                   updated_at = excluded.updated_at"#,
        )
        .bind(key.as_str())
        .bind(value)
        .bind(&updated_str)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("set_metadata", e))?;
        Ok(())
    }

    async fn get_metadata(&self, key: &ServiceMetadataKey) -> AppResult<Option<String>> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT value FROM service_metadata WHERE key = ?")
                .bind(key.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| map_sqlx_error("get_metadata", e))?;
        Ok(value)
    }

    // ── Status counts ───────────────────────────────────────────────────

    async fn status_counts(&self) -> AppResult<StatusCounts> {
        let mut counts = StatusCounts::default();

        let download_rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT download_status, COUNT(*), MIN(discovered_at) FROM images GROUP BY download_status")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| map_sqlx_error("status_counts_download", e))?;

        for (status_str, count) in download_rows {
            let status = status_str.parse::<DownloadStatus>().map_err(|_| {
                AppError::new(
                    ErrorCategory::Database,
                    "status_counts_download",
                    format!("unknown download_status in count: {status_str}"),
                )
            })?;
            counts.download.insert(status, count);
        }

        let processing_rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT processing_status, COUNT(*), MIN(downloaded_at) FROM images GROUP BY processing_status",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("status_counts_processing", e))?;

        for (status_str, count) in processing_rows {
            let status = status_str.parse::<ProcessingStatus>().map_err(|_| {
                AppError::new(
                    ErrorCategory::Database,
                    "status_counts_processing",
                    format!("unknown processing_status in count: {status_str}"),
                )
            })?;
            counts.processing.insert(status, count);
        }

        Ok(counts)
    }

    async fn operational_summary(&self) -> AppResult<OperationalSummary> {
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT download_status, processing_status FROM images")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| map_sqlx_error("operational_summary_statuses", e))?;
        for (download, processing) in statuses {
            download.parse::<DownloadStatus>().map_err(|_| {
                AppError::new(
                    ErrorCategory::Database,
                    "operational_summary",
                    format!("unknown download_status in operational summary: {download}"),
                )
            })?;
            processing.parse::<ProcessingStatus>().map_err(|_| {
                AppError::new(
                    ErrorCategory::Database,
                    "operational_summary",
                    format!("unknown processing_status in operational summary: {processing}"),
                )
            })?;
        }

        let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64)>(
            r#"SELECT
                (SELECT COUNT(*) FROM cameras WHERE enabled = 1),
                COUNT(*),
                COALESCE(SUM(CASE WHEN download_status = 'downloaded' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN download_status = 'pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN download_status = 'downloaded'
                    AND processing_status IN ('new', 'processing', 'retry_wait') THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN processing_status = 'done' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN download_status = 'retry_wait' THEN 1 ELSE 0 END), 0)
                    + COALESCE(SUM(CASE WHEN processing_status = 'retry_wait' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN download_status IN ('unavailable', 'failed') THEN 1 ELSE 0 END), 0)
                    + COALESCE(SUM(CASE WHEN processing_status IN ('failed', 'missing') THEN 1 ELSE 0 END), 0)
             FROM images"#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("operational_summary", e))?;

        Ok(OperationalSummary {
            cameras_active: row.0,
            images_discovered: row.1,
            images_downloaded: row.2,
            downloads_pending: row.3,
            images_awaiting_classification: row.4,
            classifications_completed: row.5,
            retryable_failures: row.6,
            permanent_failures: row.7,
        })
    }

    // ── Row lookups ─────────────────────────────────────────────────────

    async fn get_image(&self, image_id: ImageId) -> AppResult<ImageRecord> {
        let row = sqlx::query(
            r#"SELECT id, image_key, camera_id, track_id,
                      capture_start_at, capture_end_at,
                      playback_uri, canonical_playback_uri, codec_type, content_type,
                      nvr_reported_size, local_path,
                      download_status, download_attempts, downloaded_at,
                      download_last_error, download_next_attempt_at, download_lease_until,
                      processing_status, processing_attempts, processing_started_at,
                      processing_completed_at, processing_last_error,
                      processing_last_raw_response, processing_generation,
                      processing_next_attempt_at, processing_lease_until,
                      discovered_at, created_at, updated_at
                 FROM images WHERE id = ?"#,
        )
        .bind(image_id.get())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("get_image", e))?;

        let id: i64 = row.try_get(0).map_err(|e| map_sqlx_error("get_image", e))?;
        let image_key: String = row.try_get(1).map_err(|e| map_sqlx_error("get_image", e))?;
        let camera_id: i64 = row.try_get(2).map_err(|e| map_sqlx_error("get_image", e))?;
        let track_id: String = row.try_get(3).map_err(|e| map_sqlx_error("get_image", e))?;
        let capture_start_at = parse_timestamp_col(&row, 4)?;
        let capture_end_at = parse_timestamp_col_opt(&row, 5)?;
        let playback_uri: String = row.try_get(6).map_err(|e| map_sqlx_error("get_image", e))?;
        let canonical_playback_uri: String =
            row.try_get(7).map_err(|e| map_sqlx_error("get_image", e))?;
        let codec_type: Option<String> =
            row.try_get(8).map_err(|e| map_sqlx_error("get_image", e))?;
        let content_type: Option<String> =
            row.try_get(9).map_err(|e| map_sqlx_error("get_image", e))?;
        let nvr_reported_size: Option<i64> = row
            .try_get(10)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let local_path: Option<PathBuf> = row
            .try_get::<Option<String>, _>(11)
            .map_err(|e| map_sqlx_error("get_image", e))?
            .map(PathBuf::from);
        let download_status = parse_download_status(&row, 12)?;
        let download_attempts: i64 = row
            .try_get(13)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let downloaded_at: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(14)
            .map_err(|e| map_sqlx_error("get_image", e))?
        {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid downloaded_at: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let download_last_error: Option<String> = row
            .try_get(15)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let download_next_attempt_at: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(16)
            .map_err(|e| map_sqlx_error("get_image", e))?
        {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid download_next_attempt_at: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let download_lease_until: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(17)
            .map_err(|e| map_sqlx_error("get_image", e))?
        {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid download_lease_until: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let processing_status = parse_processing_status(&row, 18)?;
        let processing_attempts: i64 = row
            .try_get(19)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let processing_started_at: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(20)
            .map_err(|e| map_sqlx_error("get_image", e))?
        {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid processing_started_at: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let processing_completed_at: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(21)
            .map_err(|e| map_sqlx_error("get_image", e))?
        {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid processing_completed_at: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let processing_last_error: Option<String> = row
            .try_get(22)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let processing_last_raw_response: Option<String> = row
            .try_get(23)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let processing_generation: i64 = row
            .try_get(24)
            .map_err(|e| map_sqlx_error("get_image", e))?;
        let processing_next_attempt_at: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(25)
            .map_err(|e| map_sqlx_error("get_image", e))?
        {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid processing_next_attempt_at: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let processing_lease_until: Option<Timestamp> = match row
            .try_get::<Option<String>, _>(26)
            .map_err(|e| {
            map_sqlx_error("get_image", e)
        })? {
            Some(s) => Some(s.parse().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "get_image",
                    format!("invalid processing_lease_until: {e}"),
                    anyhow::Error::from(e),
                )
            })?),
            None => None,
        };
        let discovered_at = parse_timestamp_col(&row, 27)?;
        let created_at = parse_timestamp_col(&row, 28)?;
        let updated_at = parse_timestamp_col(&row, 29)?;

        Ok(ImageRecord {
            id: ImageId::new(id),
            image_key: ImageKey::new(image_key),
            camera_id: CameraId::new(camera_id),
            track_id: TrackId::new(track_id),
            capture_start_at,
            capture_end_at,
            playback_uri,
            canonical_playback_uri,
            codec_type,
            content_type,
            nvr_reported_size,
            local_path,
            download_status,
            download_attempts,
            downloaded_at,
            download_last_error,
            download_next_attempt_at,
            download_lease_until,
            processing_status,
            processing_attempts,
            processing_started_at,
            processing_completed_at,
            processing_last_error,
            processing_last_raw_response,
            processing_generation,
            processing_next_attempt_at,
            processing_lease_until,
            discovered_at,
            created_at,
            updated_at,
        })
    }

    async fn get_classification(
        &self,
        image_id: ImageId,
        model: &str,
        prompt_version: &str,
    ) -> AppResult<ClassificationRecord> {
        let row = sqlx::query(
            r#"SELECT id, image_id, model, prompt_version, contains_wildlife, is_interesting,
                      summary, species_json, confidence, classification_json, raw_response,
                      request_started_at, request_completed_at, created_at, bounding_boxes_json
                 FROM classifications
                 WHERE image_id = ? AND model = ? AND prompt_version = ?"#,
        )
        .bind(image_id.get())
        .bind(model)
        .bind(prompt_version)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("get_classification", e))?;

        Ok(ClassificationRecord {
            id: ClassificationId::new(
                row.try_get(0)
                    .map_err(|e| map_sqlx_error("get_classification", e))?,
            ),
            image_id: ImageId::new(
                row.try_get(1)
                    .map_err(|e| map_sqlx_error("get_classification", e))?,
            ),
            model: row
                .try_get(2)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            prompt_version: row
                .try_get(3)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            contains_wildlife: row
                .try_get(4)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            is_interesting: row
                .try_get(5)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            summary: row
                .try_get(6)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            species_json: row
                .try_get(7)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            confidence: row
                .try_get(8)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            classification_json: row
                .try_get(9)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            bounding_boxes_json: row
                .try_get(14)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            raw_response: row
                .try_get(10)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            request_started_at: parse_timestamp_col(&row, 11)?,
            request_completed_at: parse_timestamp_col(&row, 12)?,
            created_at: parse_timestamp_col(&row, 13)?,
        })
    }

    // ── Garbage collection ──────────────────────────────────────────────

    async fn garbage_collection_candidates(
        &self,
        cutoff: &Timestamp,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<GarbageCollectionCandidate>> {
        let cutoff_str = format_timestamp(cutoff);
        let min_id = after_image_id.map(|id| id.get());

        let query = if let Some(min_id) = min_id {
            sqlx::query_as::<_, GarbageCollectionRow>(
                r#"SELECT id, local_path FROM images
                       WHERE processing_status = 'done'
                         AND local_path IS NOT NULL
                         AND capture_start_at < ?
                         AND id > ?
                         AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 0)
                         AND NOT EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)
                       ORDER BY images.id ASC LIMIT ?"#,
            )
            .bind(&cutoff_str)
            .bind(min_id)
            .bind(limit)
        } else {
            sqlx::query_as::<_, GarbageCollectionRow>(
                r#"SELECT id, local_path FROM images
                       WHERE processing_status = 'done'
                         AND local_path IS NOT NULL
                         AND capture_start_at < ?
                         AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 0)
                         AND NOT EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)
                       ORDER BY images.id ASC LIMIT ?"#,
            )
            .bind(&cutoff_str)
            .bind(limit)
        };

        let rows: Vec<GarbageCollectionRow> = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("garbage_collection_candidates", e))?;

        Ok(rows
            .into_iter()
            .map(|row| GarbageCollectionCandidate {
                image_id: ImageId::new(row.id),
                local_path: PathBuf::from(row.local_path),
            })
            .collect())
    }

    async fn wildlife_file_references_needing_reconciliation(
        &self,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<WildlifeFileReference>> {
        let min_id = after_image_id.map(|id| id.get());
        let query = if let Some(min_id) = min_id {
            sqlx::query_as::<_, (i64, String)>(
                r#"SELECT images.id, images.local_path FROM images
                    WHERE images.id > ?
                      AND images.local_path IS NOT NULL
                      AND (images.local_file_identity IS NULL OR images.local_file_identity LIKE 'unresolved:%' OR images.local_path != images.local_file_identity)
                      AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)
                    ORDER BY images.id ASC LIMIT ?"#,
            )
            .bind(min_id)
            .bind(limit)
        } else {
            sqlx::query_as::<_, (i64, String)>(
                r#"SELECT images.id, images.local_path FROM images
                    WHERE images.local_path IS NOT NULL
                      AND (images.local_file_identity IS NULL OR images.local_file_identity LIKE 'unresolved:%' OR images.local_path != images.local_file_identity)
                      AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)
                    ORDER BY images.id ASC LIMIT ?"#,
            )
            .bind(limit)
        };

        query
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|(id, path)| WildlifeFileReference {
                        image_id: ImageId::new(id),
                        local_path: PathBuf::from(path),
                    })
                    .collect()
            })
            .map_err(|e| map_sqlx_error("wildlife_file_references_needing_reconciliation", e))
    }

    async fn reconcile_wildlife_file_reference(
        &self,
        reference: &WildlifeFileReference,
        canonical_path: &Path,
    ) -> AppResult<()> {
        let expected = reference.local_path.to_string_lossy().to_string();
        let canonical = canonical_path.to_string_lossy().to_string();
        sqlx::query(
            r#"UPDATE images SET local_path = ?, local_file_identity = ?
                WHERE id = ? AND local_path = ?
                  AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)"#,
        )
        .bind(&canonical)
        .bind(&canonical)
        .bind(reference.image_id.get())
        .bind(&expected)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("reconcile_wildlife_file_reference", e))?;
        Ok(())
    }

    async fn mark_wildlife_file_reference_unresolved(
        &self,
        reference: &WildlifeFileReference,
    ) -> AppResult<()> {
        let expected = reference.local_path.to_string_lossy().to_string();
        let marker = format!("unresolved:{expected}");
        sqlx::query(
            r#"UPDATE images SET local_file_identity = ?
                WHERE id = ? AND local_path = ?
                  AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)"#,
        )
        .bind(&marker)
        .bind(reference.image_id.get())
        .bind(&expected)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("mark_wildlife_file_reference_unresolved", e))?;
        Ok(())
    }

    async fn with_gc_candidate(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        canonical_identity: Option<&Path>,
        operation: GcOperation,
    ) -> AppResult<GcOutcome> {
        let cutoff_str = format_timestamp(cutoff);
        let path_str = candidate.local_path.to_string_lossy().to_string();
        let identity_str = canonical_identity.map(|path| path.to_string_lossy().to_string());
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("with_gc_candidate", e))?;

        let eligible: i64 = sqlx::query_scalar(
            r#"SELECT EXISTS(
                 SELECT 1 FROM images
                  WHERE id = ? AND local_path = ? AND processing_status = 'done'
                    AND capture_start_at < ?
                    AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 0)
                    AND NOT EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)
             )"#,
        )
        .bind(candidate.image_id.get())
        .bind(&path_str)
        .bind(&cutoff_str)
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("with_gc_candidate_eligibility", e))?;
        if eligible == 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "with_gc_candidate",
                "candidate ineligible",
            ));
        }

        let shared_positive: i64 = sqlx::query_scalar(
            r#"SELECT EXISTS(
                 SELECT 1 FROM images AS positive_images
                  WHERE positive_images.local_path IS NOT NULL
                    AND (positive_images.local_path = ? OR positive_images.local_file_identity = ?)
                    AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = positive_images.id AND classifications.contains_wildlife = 1)
             )"#,
        )
        .bind(&path_str)
        .bind(&identity_str)
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("with_gc_candidate_shared_wildlife", e))?;
        if shared_positive != 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "with_gc_candidate",
                "shares wildlife-positive file",
            ));
        }

        if Self::has_unreconciled_wildlife_file_reference(&mut tx)
            .await
            .map_err(|e| map_sqlx_error("with_gc_candidate_unreconciled_wildlife", e))?
        {
            return Err(AppError::new(
                ErrorCategory::Database,
                "with_gc_candidate",
                "unreconciled wildlife reference",
            ));
        }

        let outcome = operation().await?;
        let collected_at = format_timestamp(&Timestamp::new(Utc::now()));
        let update = sqlx::query(
            r#"UPDATE images SET local_path = NULL, local_file_identity = NULL, updated_at = ?
                WHERE id = ? AND local_path = ? AND processing_status = 'done'
                  AND capture_start_at < ?
                  AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 0)
                  AND NOT EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)"#,
        )
        .bind(&collected_at)
        .bind(candidate.image_id.get())
        .bind(&path_str)
        .bind(&cutoff_str)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("with_gc_candidate_clear_path", e))?;
        if update.rows_affected() != 1 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "with_gc_candidate_clear_path",
                format!(
                    "image {} changed during garbage collection",
                    candidate.image_id.get()
                ),
            ));
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("with_gc_candidate_commit", e))?;
        Ok(outcome)
    }

    async fn mark_local_file_garbage_collected(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        collected_at: &Timestamp,
    ) -> AppResult<()> {
        let cutoff_str = format_timestamp(cutoff);
        let collected_str = format_timestamp(collected_at);
        let path_str = candidate.local_path.to_string_lossy().to_string();

        let result = sqlx::query(
            r#"UPDATE images SET local_path = NULL, local_file_identity = NULL, updated_at = ?
               WHERE id = ? AND local_path = ? AND processing_status = 'done'
                 AND capture_start_at < ?
                 AND EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 0)
                 AND NOT EXISTS (SELECT 1 FROM classifications WHERE classifications.image_id = images.id AND classifications.contains_wildlife = 1)"#,
        )
        .bind(&collected_str)
        .bind(candidate.image_id.get())
        .bind(&path_str)
        .bind(&cutoff_str)
        .execute(&self.pool)
        .await
        .map_err(|e| map_sqlx_error("mark_local_file_garbage_collected", e))?;

        if result.rows_affected() == 0 {
            let already_cleared: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM images WHERE id = ? AND local_path IS NULL",
            )
            .bind(candidate.image_id.get())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("mark_local_file_garbage_collected_check", e))?;

            if already_cleared > 0 {
                return Ok(());
            }

            return Err(AppError::new(
                ErrorCategory::Database,
                "mark_local_file_garbage_collected",
                format!(
                    "image {} eligibility changed or path mismatch during garbage collection",
                    candidate.image_id.get()
                ),
            ));
        }

        Ok(())
    }

    // ── Rate limiting ───────────────────────────────────────────────────

    async fn reserve_classifier_rate_limit(
        &self,
        limit: &crate::configuration::ClassifierRateLimitConfig,
        max_tokens: u32,
        now: &Timestamp,
    ) -> AppResult<RateLimitReservation> {
        let token_cost = limit
            .estimated_input_tokens_per_request
            .checked_add(max_tokens as u64)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Configuration,
                    "reserve_classifier_rate_limit",
                    "classifier token reservation overflow",
                )
            })?;
        let token_cost = i64::try_from(token_cost).map_err(|_| {
            AppError::new(
                ErrorCategory::Configuration,
                "reserve_classifier_rate_limit",
                "classifier token reservation exceeds SQLite integer range",
            )
        })?;
        let now_dt = now.as_datetime();
        let now_str = format_timestamp(now);
        let cutoff = Timestamp::new(*now_dt - chrono::Duration::seconds(60));
        let cutoff_str = format_timestamp(&cutoff);
        let day = now_dt.date_naive().to_string();

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
        let minute = sqlx::query("SELECT COUNT(*), COALESCE(SUM(token_cost), 0), MIN(reserved_at) FROM classifier_rate_limit_events WHERE quota_group = ? AND reserved_at > ?")
            .bind(&limit.quota_group).bind(&cutoff_str).fetch_one(tx.as_mut()).await
            .map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
        let minute_requests: i64 = minute.get(0);
        let minute_tokens: i64 = minute.get(1);
        let oldest: Option<String> = minute.get(2);
        let daily = sqlx::query("SELECT requests, tokens FROM classifier_rate_limit_daily_usage WHERE quota_group = ? AND day = ?")
            .bind(&limit.quota_group).bind(&day).fetch_optional(tx.as_mut()).await
            .map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
        let (daily_requests, daily_tokens) = daily
            .map(|row| (row.get::<i64, _>(0), row.get::<i64, _>(1)))
            .unwrap_or((0, 0));
        let rpm = i64::try_from(limit.requests_per_minute).unwrap_or(i64::MAX);
        let rpd = i64::try_from(limit.requests_per_day).unwrap_or(i64::MAX);
        let tpm = i64::try_from(limit.tokens_per_minute).unwrap_or(i64::MAX);
        let tpd = i64::try_from(limit.tokens_per_day).unwrap_or(i64::MAX);
        let permitted = minute_requests < rpm
            && minute_tokens.saturating_add(token_cost) <= tpm
            && daily_requests < rpd
            && daily_tokens.saturating_add(token_cost) <= tpd;
        if permitted {
            sqlx::query("INSERT INTO classifier_rate_limit_events (id, quota_group, reserved_at, token_cost) VALUES (?, ?, ?, ?)")
                .bind(Uuid::new_v4().to_string()).bind(&limit.quota_group).bind(&now_str).bind(token_cost)
                .execute(tx.as_mut()).await.map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
            sqlx::query("INSERT INTO classifier_rate_limit_daily_usage (quota_group, day, requests, tokens) VALUES (?, ?, 1, ?) ON CONFLICT(quota_group, day) DO UPDATE SET requests = requests + 1, tokens = tokens + excluded.tokens")
                .bind(&limit.quota_group).bind(&day).bind(token_cost)
                .execute(tx.as_mut()).await.map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
            return Ok(RateLimitReservation::Granted);
        }
        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
        if daily_requests >= rpd || daily_tokens.saturating_add(token_cost) > tpd {
            let tomorrow = now_dt
                .date_naive()
                .succ_opt()
                .expect("date has a successor");
            let reset = chrono::DateTime::<Utc>::from_naive_utc_and_offset(
                tomorrow.and_hms_opt(0, 0, 0).expect("midnight is valid"),
                Utc,
            );
            return Ok(RateLimitReservation::DailyExhausted(
                (reset - *now_dt).to_std().unwrap_or(Duration::from_secs(1)),
            ));
        }
        let wait = oldest
            .and_then(|value| value.parse::<Timestamp>().ok())
            .and_then(|oldest| {
                ((*oldest.as_datetime() + chrono::Duration::seconds(60)) - *now_dt)
                    .to_std()
                    .ok()
            })
            .unwrap_or(Duration::from_secs(1));
        Ok(RateLimitReservation::Wait(
            wait.max(Duration::from_millis(1)),
        ))
    }

    async fn claim_next_processing_with_rate_limit(
        &self,
        limit: &crate::configuration::ClassifierRateLimitConfig,
        max_tokens: u32,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<RateLimitedProcessingClaim> {
        let token_cost = limit
            .estimated_input_tokens_per_request
            .checked_add(max_tokens as u64)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Configuration,
                    "claim_next_processing_with_rate_limit",
                    "classifier token reservation overflow",
                )
            })?;
        let token_cost = i64::try_from(token_cost).map_err(|_| {
            AppError::new(
                ErrorCategory::Configuration,
                "claim_next_processing_with_rate_limit",
                "classifier token reservation exceeds SQLite integer range",
            )
        })?;
        let now_dt = now.as_datetime();
        let now_str = format_timestamp(now);
        let lease_str = format_timestamp(lease_until);
        let cutoff = Timestamp::new(*now_dt - chrono::Duration::seconds(60));
        let cutoff_str = format_timestamp(&cutoff);
        let day = now_dt.date_naive().to_string();

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;

        let image_id: Option<i64> = sqlx::query_scalar(
            r#"SELECT id FROM images WHERE processing_status IN ('new', 'retry_wait')
               AND (processing_status != 'retry_wait' OR processing_next_attempt_at IS NULL OR processing_next_attempt_at <= ?)
               AND local_path IS NOT NULL AND download_status = 'downloaded' AND processing_lease_until IS NULL
               ORDER BY id ASC LIMIT 1"#,
        )
        .bind(&now_str).fetch_optional(tx.as_mut()).await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        let Some(image_id) = image_id else {
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
            return Ok(RateLimitedProcessingClaim::NoWork);
        };

        let minute = sqlx::query("SELECT COUNT(*), COALESCE(SUM(token_cost), 0), MIN(reserved_at) FROM classifier_rate_limit_events WHERE quota_group = ? AND reserved_at > ?")
            .bind(&limit.quota_group).bind(&cutoff_str).fetch_one(tx.as_mut()).await
            .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        let minute_requests: i64 = minute.get(0);
        let minute_tokens: i64 = minute.get(1);
        let oldest: Option<String> = minute.get(2);
        let daily = sqlx::query("SELECT requests, tokens FROM classifier_rate_limit_daily_usage WHERE quota_group = ? AND day = ?")
            .bind(&limit.quota_group).bind(&day).fetch_optional(tx.as_mut()).await
            .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        let (daily_requests, daily_tokens) = daily
            .map(|row| (row.get::<i64, _>(0), row.get::<i64, _>(1)))
            .unwrap_or((0, 0));
        let rpm = i64::try_from(limit.requests_per_minute).unwrap_or(i64::MAX);
        let rpd = i64::try_from(limit.requests_per_day).unwrap_or(i64::MAX);
        let tpm = i64::try_from(limit.tokens_per_minute).unwrap_or(i64::MAX);
        let tpd = i64::try_from(limit.tokens_per_day).unwrap_or(i64::MAX);
        let permitted = minute_requests < rpm
            && minute_tokens.saturating_add(token_cost) <= tpm
            && daily_requests < rpd
            && daily_tokens.saturating_add(token_cost) <= tpd;
        if !permitted {
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
            if daily_requests >= rpd || daily_tokens.saturating_add(token_cost) > tpd {
                let tomorrow = now_dt
                    .date_naive()
                    .succ_opt()
                    .expect("date has a successor");
                let reset = chrono::DateTime::<Utc>::from_naive_utc_and_offset(
                    tomorrow.and_hms_opt(0, 0, 0).expect("midnight is valid"),
                    Utc,
                );
                return Ok(RateLimitedProcessingClaim::DailyExhausted(
                    (reset - *now_dt).to_std().unwrap_or(Duration::from_secs(1)),
                ));
            }
            let wait = oldest
                .and_then(|value| value.parse::<Timestamp>().ok())
                .and_then(|oldest| {
                    ((*oldest.as_datetime() + chrono::Duration::seconds(60)) - *now_dt)
                        .to_std()
                        .ok()
                })
                .unwrap_or(Duration::from_secs(1));
            return Ok(RateLimitedProcessingClaim::Wait(
                wait.max(Duration::from_millis(1)),
            ));
        }

        // Claim the image
        let claim_row = sqlx::query(
            r#"UPDATE images SET
                   processing_status = 'processing',
                   processing_attempts = processing_attempts + 1,
                   processing_generation = processing_generation + 1,
                   processing_started_at = ?,
                   processing_lease_until = ?,
                   updated_at = ?
               WHERE id = ?
               RETURNING id, image_key, local_path, processing_attempts, processing_generation, processing_lease_until"#,
        )
        .bind(&now_str).bind(&lease_str).bind(&now_str).bind(image_id)
        .fetch_optional(tx.as_mut()).await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;

        let claim = match claim_row {
            Some(row) => {
                Self::processing_claim_from_row(row, "claim_next_processing_with_rate_limit")?
            }
            None => {
                tx.commit()
                    .await
                    .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
                return Ok(RateLimitedProcessingClaim::NoWork);
            }
        };

        // Record rate limit event
        let grant_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO classifier_rate_limit_events (id, quota_group, reserved_at, token_cost) VALUES (?, ?, ?, ?)")
            .bind(&grant_id).bind(&limit.quota_group).bind(&now_str).bind(token_cost)
            .execute(tx.as_mut()).await.map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        sqlx::query("INSERT INTO classifier_rate_limit_daily_usage (quota_group, day, requests, tokens) VALUES (?, ?, 1, ?) ON CONFLICT(quota_group, day) DO UPDATE SET requests = requests + 1, tokens = tokens + excluded.tokens")
            .bind(&limit.quota_group).bind(&day).bind(token_cost)
            .execute(tx.as_mut()).await.map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;

        Ok(RateLimitedProcessingClaim::Claimed {
            claim,
            grant: RateLimitGrant {
                id: grant_id,
                quota_group: limit.quota_group.clone(),
                day,
                token_cost,
            },
        })
    }

    async fn cancel_classifier_rate_limit(&self, grant: &RateLimitGrant) -> AppResult<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;

        // Delete the event and decrement daily usage atomically
        let event = sqlx::query(
            r#"DELETE FROM classifier_rate_limit_events WHERE id = ? AND quota_group = ? AND daily_refunded_at IS NULL"#,
        )
        .bind(&grant.id)
        .bind(&grant.quota_group)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;
        if event.rows_affected() == 0 {
            return Ok(false);
        }

        // Decrement daily usage — require exactly one row to preserve invariant
        let usage = sqlx::query(
            r#"UPDATE classifier_rate_limit_daily_usage SET requests = MAX(requests - 1, 0),
               tokens = MAX(tokens - ?, 0)
               WHERE quota_group = ? AND day = ? AND requests > 0 AND tokens >= ?"#,
        )
        .bind(grant.token_cost)
        .bind(&grant.quota_group)
        .bind(&grant.day)
        .bind(grant.token_cost)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;

        if usage.rows_affected() != 1 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "cancel_classifier_rate_limit",
                "rate-limit event exists without matching daily usage",
            ));
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;
        Ok(true)
    }

    async fn refund_daily_classifier_rate_limit(
        &self,
        grant: &RateLimitGrant,
        now: &Timestamp,
    ) -> AppResult<bool> {
        let now_str = format_timestamp(now);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;

        // Mark the event as refunded
        let event = sqlx::query(
            r#"UPDATE classifier_rate_limit_events SET daily_refunded_at = ?
               WHERE id = ? AND quota_group = ? AND daily_refunded_at IS NULL"#,
        )
        .bind(&now_str)
        .bind(&grant.id)
        .bind(&grant.quota_group)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
        if event.rows_affected() == 0 {
            return Ok(false);
        }

        // Refund daily usage
        let usage = sqlx::query(
            r#"UPDATE classifier_rate_limit_daily_usage SET requests = MAX(requests - 1, 0),
               tokens = MAX(tokens - ?, 0)
               WHERE quota_group = ? AND day = ? AND requests > 0 AND tokens >= ?"#,
        )
        .bind(grant.token_cost)
        .bind(&grant.quota_group)
        .bind(&grant.day)
        .bind(grant.token_cost)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
        if usage.rows_affected() != 1 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "refund_daily_classifier_rate_limit",
                "rate-limit event exists without matching daily usage",
            ));
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
        Ok(true)
    }

    // ── Web query operations ────────────────────────────────────────────

    async fn web_health(&self) -> AppResult<WebHealthSnapshot> {
        self.web_health().await
    }

    async fn web_cameras(&self) -> AppResult<Vec<WebCameraRecord>> {
        self.web_cameras().await
    }

    async fn web_query_images(
        &self,
        query: &WebImageQuery,
    ) -> AppResult<(Vec<WebImageSummaryRecord>, Option<(String, i64)>)> {
        self.web_query_images(query).await
    }

    async fn web_overview(&self, filter: &WebImageFilter) -> AppResult<WebOverviewRecord> {
        self.web_overview(filter).await
    }

    async fn web_buckets(
        &self,
        filter: &WebImageFilter,
        seconds: i64,
    ) -> AppResult<Vec<WebActivityBucket>> {
        self.web_buckets(filter, seconds).await
    }

    async fn web_camera_counts(&self, filter: &WebImageFilter) -> AppResult<Vec<WebCameraCounts>> {
        self.web_camera_counts(filter).await
    }

    async fn web_image_detail(&self, image_id: ImageId) -> AppResult<Option<WebImageDetailRecord>> {
        self.web_image_detail(image_id).await
    }

    async fn web_image_content_lookup(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebImageContentLookup>> {
        self.web_image_content_lookup(image_id).await
    }

    async fn web_recording_target(
        &self,
        image_id: ImageId,
    ) -> AppResult<Option<WebRecordingTarget>> {
        self.web_recording_target(image_id).await
    }

    async fn web_activity(&self, filter: &WebImageFilter) -> AppResult<WebActivityRecord> {
        self.web_activity(filter).await
    }
}

/// Raw row for GarbageCollectionCandidate deserialization.
#[derive(sqlx::FromRow)]
struct GarbageCollectionRow {
    id: i64,
    local_path: String,
}
