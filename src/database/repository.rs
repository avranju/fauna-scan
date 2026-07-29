//! All durable state changes for Fauna Scan, centralized in short
//! parameterized transactions.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use sqlx::Row;
use uuid::Uuid;

use crate::domain::{
    CameraId, ClassificationId, DownloadStatus, ImageId, ImageKey, ProcessingStatus, Timestamp,
    TrackId,
};
use crate::error::{AppError, AppResult, ErrorCategory};

use super::models::*;
use super::{
    map_sqlx_error, parse_download_status, parse_processing_status, parse_timestamp_col,
    parse_timestamp_col_opt,
};

/// Repository operations backed by a shared SqlitePool.
///
/// Each method opens a short transaction, performs its work, and commits
/// or rolls back automatically.
#[derive(Clone)]
pub struct DatabaseOps(pub sqlx::SqlitePool);

impl DatabaseOps {
    /// Expose the underlying pool for integration tests.
    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.0
    }
}

/// Result of attempting to reserve provider quota before dispatching an HTTP
/// classification request. A denied reservation has not claimed an image and
/// therefore must not consume a processing attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitReservation {
    Granted,
    /// The sliding per-minute limit is temporarily full. The worker can wait
    /// and try again without abandoning its scanner pass.
    Wait(Duration),
    /// The UTC daily request or token budget is exhausted. This can be many
    /// hours, so the worker should yield until a later scanner pass.
    DailyExhausted(Duration),
}

/// An individual provider-quota reservation. It is returned only together
/// with an atomically claimed image, so it can safely be refunded if Cerebras
/// rejects the request before charging tokens.
#[derive(Debug, Clone)]
pub struct RateLimitGrant {
    id: String,
    quota_group: String,
    day: String,
    token_cost: i64,
}

/// Result of atomically checking quota and claiming work for a rate-limited
/// classifier endpoint.
#[derive(Debug)]
pub enum RateLimitedProcessingClaim {
    Claimed {
        claim: ProcessingClaim,
        grant: RateLimitGrant,
    },
    NoWork,
    Wait(Duration),
    DailyExhausted(Duration),
}

impl DatabaseOps {
    /// Atomically reserve one classifier request against its shared provider
    /// quota. Minute accounting uses a sliding 60-second window; daily
    /// accounting is persisted by UTC day so a process restart cannot reset a
    /// provider quota.
    pub async fn reserve_classifier_rate_limit(
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
        let now_str = super::format_timestamp(now);
        let cutoff = Timestamp::new(*now_dt - chrono::Duration::seconds(60));
        let cutoff_str = super::format_timestamp(&cutoff);
        let day = now_dt.date_naive().to_string();

        let mut tx = self
            .0
            .begin()
            .await
            .map_err(|e| map_sqlx_error("reserve_classifier_rate_limit", e))?;
        // Keep events beyond the rolling minute window. A later provider 429
        // may need to refund its daily charge, while the event must still
        // remain visible to preserve the original attempt's minute throttle.
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
}

impl DatabaseOps {
    /// Atomically admit provider quota and claim an image. Keeping these two
    /// writes in one `BEGIN IMMEDIATE` transaction prevents an endpoint worker
    /// from spending quota after another worker has claimed the final image.
    pub async fn claim_next_processing_with_rate_limit(
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
        let now_str = super::format_timestamp(now);
        let lease_str = super::format_timestamp(lease_until);
        let cutoff = Timestamp::new(*now_dt - chrono::Duration::seconds(60));
        let cutoff_str = super::format_timestamp(&cutoff);
        let day = now_dt.date_naive().to_string();

        let mut tx = self
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;

        let image_id: Option<i64> = sqlx::query_scalar(
            r#"SELECT id FROM images
               WHERE processing_status IN ('new', 'retry_wait')
                 AND (processing_status != 'retry_wait'
                      OR processing_next_attempt_at IS NULL
                      OR processing_next_attempt_at <= ?)
                 AND local_path IS NOT NULL
                 AND download_status = 'downloaded'
                 AND processing_lease_until IS NULL
               ORDER BY id ASC
               LIMIT 1"#,
        )
        .bind(&now_str)
        .fetch_optional(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        let Some(image_id) = image_id else {
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
            return Ok(RateLimitedProcessingClaim::NoWork);
        };

        let minute = sqlx::query(
            "SELECT COUNT(*), COALESCE(SUM(token_cost), 0), MIN(reserved_at) \
             FROM classifier_rate_limit_events WHERE quota_group = ? AND reserved_at > ?",
        )
        .bind(&limit.quota_group)
        .bind(&cutoff_str)
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        let minute_requests: i64 = minute.get(0);
        let minute_tokens: i64 = minute.get(1);
        let oldest: Option<String> = minute.get(2);
        let daily = sqlx::query(
            "SELECT requests, tokens FROM classifier_rate_limit_daily_usage \
             WHERE quota_group = ? AND day = ?",
        )
        .bind(&limit.quota_group)
        .bind(&day)
        .fetch_optional(tx.as_mut())
        .await
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

        let row = sqlx::query(
            r#"UPDATE images
               SET processing_status = 'processing',
                   processing_attempts = processing_attempts + 1,
                   processing_generation = processing_generation + 1,
                   processing_started_at = ?,
                   processing_lease_until = ?,
                   updated_at = ?
               WHERE id = ? AND processing_status IN ('new', 'retry_wait')
                 AND (processing_status != 'retry_wait'
                      OR processing_next_attempt_at IS NULL
                      OR processing_next_attempt_at <= ?)
                 AND local_path IS NOT NULL
                 AND download_status = 'downloaded'
                 AND processing_lease_until IS NULL
               RETURNING id, image_key, local_path, processing_attempts,
                         processing_generation, processing_lease_until"#,
        )
        .bind(&now_str)
        .bind(&lease_str)
        .bind(&now_str)
        .bind(image_id)
        .bind(&now_str)
        .fetch_optional(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        let Some(row) = row else {
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
            return Ok(RateLimitedProcessingClaim::NoWork);
        };

        let reservation_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO classifier_rate_limit_events (id, quota_group, reserved_at, token_cost) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&reservation_id)
        .bind(&limit.quota_group)
        .bind(&now_str)
        .bind(token_cost)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        sqlx::query(
            "INSERT INTO classifier_rate_limit_daily_usage (quota_group, day, requests, tokens) \
             VALUES (?, ?, 1, ?) ON CONFLICT(quota_group, day) DO UPDATE SET \
             requests = requests + 1, tokens = tokens + excluded.tokens",
        )
        .bind(&limit.quota_group)
        .bind(&day)
        .bind(token_cost)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;
        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("claim_next_processing_with_rate_limit", e))?;

        let claim = processing_claim_from_row(row, "claim_next_processing_with_rate_limit")?;
        Ok(RateLimitedProcessingClaim::Claimed {
            claim,
            grant: RateLimitGrant {
                id: reservation_id,
                quota_group: limit.quota_group.clone(),
                day,
                token_cost,
            },
        })
    }

    /// Cancel a reservation when request preparation failed before any HTTP
    /// submission. Both daily and minute accounting are released because the
    /// provider never observed an attempt.
    pub async fn cancel_classifier_rate_limit(&self, grant: &RateLimitGrant) -> AppResult<bool> {
        let mut tx = self
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;
        let removed = sqlx::query(
            "DELETE FROM classifier_rate_limit_events WHERE id = ? AND daily_refunded_at IS NULL",
        )
        .bind(&grant.id)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;
        if removed.rows_affected() == 0 {
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;
            return Ok(false);
        }
        decrement_daily_rate_limit_usage(tx.as_mut(), grant, "cancel_classifier_rate_limit")
            .await?;
        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("cancel_classifier_rate_limit", e))?;
        Ok(true)
    }

    /// Refund daily quota after an explicit provider rejection. The event is
    /// retained so the failed attempt continues to count in the rolling
    /// minute window and cannot turn retries into a request burst.
    pub async fn refund_daily_classifier_rate_limit(
        &self,
        grant: &RateLimitGrant,
        now: &Timestamp,
    ) -> AppResult<bool> {
        let mut tx = self
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
        let marked = sqlx::query(
            "UPDATE classifier_rate_limit_events SET daily_refunded_at = ? \
             WHERE id = ? AND daily_refunded_at IS NULL",
        )
        .bind(super::format_timestamp(now))
        .bind(&grant.id)
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
        if marked.rows_affected() == 0 {
            tx.commit()
                .await
                .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
            return Ok(false);
        }
        decrement_daily_rate_limit_usage(tx.as_mut(), grant, "refund_daily_classifier_rate_limit")
            .await?;
        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("refund_daily_classifier_rate_limit", e))?;
        Ok(true)
    }
}

async fn decrement_daily_rate_limit_usage(
    connection: &mut sqlx::SqliteConnection,
    grant: &RateLimitGrant,
    operation: &'static str,
) -> AppResult<()> {
    let usage = sqlx::query(
        "UPDATE classifier_rate_limit_daily_usage SET requests = requests - 1, \
         tokens = tokens - ? WHERE quota_group = ? AND day = ? \
         AND requests > 0 AND tokens >= ?",
    )
    .bind(grant.token_cost)
    .bind(&grant.quota_group)
    .bind(&grant.day)
    .bind(grant.token_cost)
    .execute(connection)
    .await
    .map_err(|e| map_sqlx_error(operation, e))?;
    if usage.rows_affected() != 1 {
        return Err(AppError::new(
            ErrorCategory::Database,
            operation,
            "rate-limit event exists without matching daily usage",
        ));
    }
    Ok(())
}

fn processing_claim_from_row(
    row: sqlx::sqlite::SqliteRow,
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

// ── Active camera enumeration ───────────────────────────────────────────

impl DatabaseOps {
    /// Return enabled cameras ordered deterministically by channel number then
    /// picture track ID.
    pub async fn list_active_cameras(&self) -> AppResult<Vec<CameraRecord>> {
        let rows: Vec<CameraRow> = sqlx::query_as(
            r#"SELECT id, channel_number, primary_track_id, picture_track_id,
                      name, raw_discovery_identifier,
                      enabled, first_seen_at, last_seen_at, created_at, updated_at
                 FROM cameras
                 WHERE enabled = 1
                 ORDER BY channel_number ASC, id ASC"#,
        )
        .fetch_all(&self.0)
        .await
        .map_err(|e| map_sqlx_error("list_active_cameras", e))?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            records.push(super::models::camera_row_to_record(row)?);
        }
        Ok(records)
    }
}

// ── Camera synchronization ────────────────────────────────────────────────

impl DatabaseOps {
    /// Upsert the current discovery set and mark absent known cameras inactive.
    ///
    /// When `cameras` is empty, every enabled camera is marked inactive in the
    /// same transaction. Returns the upserted camera records.
    pub async fn sync_cameras(
        &self,
        cameras: &[CameraDiscovery],
        observed_at: &Timestamp,
    ) -> AppResult<Vec<CameraRecord>> {
        let mut tx = self
            .0
            .begin()
            .await
            .map_err(|e| map_sqlx_error("sync_cameras", e))?;

        let now = super::format_timestamp(observed_at);
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

            // Fetch the row
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

            records.push(super::models::camera_row_to_record(row)?);
        }

        // Mark absent cameras inactive — always execute, even when cameras is empty.
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
            // Empty discovery set: deactivate all enabled cameras.
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

    // ── Image discovery and cursor commit ──────────────────────────────────

    /// Idempotently insert discovered images and advance the cursor atomically.
    ///
    /// Two-phase approach within a single transaction:
    ///
    /// 1. `INSERT OR IGNORE` for all images — silently skips duplicates.
    ///    `changes()` returns the exact count of newly inserted rows.
    ///
    /// 2. `UPDATE` for existing rows — uses `created_at < batch_discovered_at`
    ///    to exclude rows just inserted in phase 1, updating only metadata
    ///    for pre-existing rows while preserving work-state, attempts, leases,
    ///    paths, and errors.
    ///
    /// Returns the count of *newly inserted* image rows.
    pub async fn commit_search_window(
        &self,
        window: &SearchWindowCommit,
        images: &[DiscoveredImage],
    ) -> AppResult<u64> {
        let mut tx = self
            .0
            .begin()
            .await
            .map_err(|e| map_sqlx_error("commit_search_window", e))?;

        if images.is_empty() {
            // Upsert cursor only — must commit within the transaction.
            let window_start = super::format_timestamp(&window.window_start);
            let window_end = super::format_timestamp(&window.window_end);
            let next_search = super::format_timestamp(&window.next_search_at);
            let polled = super::format_timestamp(&window.polled_at);
            let updated = super::format_timestamp(&window.updated_at);

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

        // Phase 1: INSERT OR IGNORE — silently skips duplicate image_keys.
        let batch_discovered_at = super::format_timestamp(&images.first().unwrap().discovered_at);

        let mut new_count: u64 = 0;
        // Track which images were inserts vs ignored so we can update
        // metadata for pre-existing rows regardless of timestamp ordering.
        let mut was_insert = Vec::with_capacity(images.len());

        for img in images {
            let discovered_at = super::format_timestamp(&img.discovered_at);
            let capture_start = super::format_timestamp(&img.capture_start_at);
            let capture_end = img.capture_end_at.as_ref().map(super::format_timestamp);

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

            // INSERT OR IGNORE returns 1 for a new insert and 0 for a skipped
            // duplicate.  Accumulate to get the total new-count and track
            // which rows were inserts so Phase 2 can target the right ones.
            let inserted = result.rows_affected() > 0;
            was_insert.push(inserted);
            if inserted {
                new_count += 1;
            }
        }

        // Phase 2: UPDATE metadata for pre-existing rows.
        // We only update metadata for rows whose INSERT was ignored
        // (i.e. the image_key already existed), preserving work-state,
        // attempts, leases, paths, and errors for those rows.
        let batch_updated_at = super::format_timestamp(&window.updated_at);

        for (img, inserted) in images.iter().zip(was_insert.iter()) {
            if *inserted {
                continue;
            }
            let capture_start = super::format_timestamp(&img.capture_start_at);
            let capture_end = img.capture_end_at.as_ref().map(super::format_timestamp);

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

        // Upsert cursor
        let window_start = super::format_timestamp(&window.window_start);
        let window_end = super::format_timestamp(&window.window_end);
        let next_search = super::format_timestamp(&window.next_search_at);
        let polled = super::format_timestamp(&window.polled_at);
        let updated = super::format_timestamp(&window.updated_at);

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

    /// Record a cursor error without advancing the cursor.
    pub async fn record_cursor_error(
        &self,
        camera_id: CameraId,
        error_msg: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        let updated = super::format_timestamp(updated_at);

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
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("record_cursor_error", e))?;
        Ok(())
    }

    /// Read a camera's cursor, if it exists.
    pub async fn get_cursor(&self, camera_id: CameraId) -> AppResult<Option<SearchCursorRecord>> {
        let row = sqlx::query_as::<_, SearchCursorRow>(
            r#"SELECT camera_id, next_search_at, last_completed_window_start,
                      last_completed_window_end, last_poll_at, last_error, updated_at
                 FROM search_cursors WHERE camera_id = ?"#,
        )
        .bind(camera_id.get())
        .fetch_optional(&self.0)
        .await
        .map_err(|e| map_sqlx_error("get_cursor", e))?;
        row.map(super::models::cursor_row_to_record).transpose()
    }

    // ── Download claiming and transitions ──────────────────────────────────

    /// Atomically claim one pending or due-retry download using a single
    /// guarded UPDATE with a subquery and RETURNING.
    ///
    /// After the guarded atomic UPDATE succeeds, fetches the referenced
    /// camera's channel number and name to populate the extended
    /// `DownloadClaim`.
    ///
    /// Returns None when no eligible rows exist.
    pub async fn claim_next_download(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<DownloadClaim>> {
        let now_str = super::format_timestamp(now);
        let lease_str = super::format_timestamp(lease_until);

        // Single atomic UPDATE with subquery: selects the oldest eligible row
        // and transitions it to downloading in one statement.
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
        .fetch_optional(&self.0)
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

                // Fetch camera metadata for destination path construction.
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

    /// Fetch a camera's channel number and name by camera ID.
    ///
    /// Returns `(channel_number, name)` or an error if the camera row
    /// cannot be found.
    async fn fetch_camera_metadata(&self, camera_id: CameraId) -> AppResult<(i64, Option<String>)> {
        let row = sqlx::query_as::<_, (i64, Option<String>)>(
            r#"SELECT channel_number, name FROM cameras WHERE id = ?"#,
        )
        .bind(camera_id.get())
        .fetch_optional(&self.0)
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

    /// Guard a download completion: downloading → downloaded.
    pub async fn complete_download(
        &self,
        image_id: ImageId,
        local_path: &Path,
        downloaded_at: &Timestamp,
    ) -> AppResult<()> {
        // Persist the canonical path for successful downloads.  Keeping the
        // database reference independent of a supported output-root symlink
        // means that retargeting that symlink cannot make a wildlife path
        // silently refer to a different file later.
        let (path_str, file_identity) = match tokio::fs::canonicalize(local_path).await {
            Ok(path) => {
                let canonical = path.to_string_lossy().to_string();
                (canonical.clone(), canonical)
            }
            Err(_) => {
                // A few crash-recovery/test paths complete a row after the
                // bytes have disappeared. Preserve that compatibility, but
                // mark the identity unresolved so collection remains fail-safe.
                let path = local_path.to_string_lossy().to_string();
                (path.clone(), format!("unresolved:{path}"))
            }
        };
        let downloaded_str = super::format_timestamp(downloaded_at);
        let updated_at = super::format_timestamp(downloaded_at);

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
        .execute(&self.0)
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

    /// Guard a download failure transition in a single guarded UPDATE.
    ///
    /// Sets status, error, lease, next-attempt timestamp, and updated_at atomically.
    pub async fn fail_download(
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

        let next_attempt_str = next_attempt_at.map(super::format_timestamp);
        let updated_at = super::format_timestamp(now);

        // Single UPDATE: status, error, lease cleared, next-attempt set, updated_at refreshed.
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
        .execute(&self.0)
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

    // ── Processing claiming and transitions ────────────────────────────────

    /// Whether any downloaded image is eligible for a processing claim.
    /// This is intentionally only a preflight check; `claim_next_processing`
    /// remains the authoritative atomic claim.
    pub async fn has_eligible_processing(&self, now: &Timestamp) -> AppResult<bool> {
        let now = super::format_timestamp(now);
        let row = sqlx::query("SELECT EXISTS(SELECT 1 FROM images WHERE processing_status IN ('new', 'retry_wait') AND (processing_status != 'retry_wait' OR processing_next_attempt_at IS NULL OR processing_next_attempt_at <= ?) AND local_path IS NOT NULL AND download_status = 'downloaded' AND processing_lease_until IS NULL)")
            .bind(now).fetch_one(&self.0).await.map_err(|e| map_sqlx_error("has_eligible_processing", e))?;
        Ok(row.get::<i64, _>(0) != 0)
    }

    /// Atomically claim one downloaded image with a local path for processing
    /// using a single guarded UPDATE with a subquery and RETURNING.
    ///
    /// Increments `processing_generation` to produce a durable claim token
    /// that is returned in `ProcessingClaim`.  Renewal, failure, and
    /// completion operations verify this token to prevent stale workers
    /// from mutating a row that has been re-claimed by another scanner.
    pub async fn claim_next_processing(
        &self,
        now: &Timestamp,
        lease_until: &Timestamp,
    ) -> AppResult<Option<ProcessingClaim>> {
        let now_str = super::format_timestamp(now);
        let lease_str = super::format_timestamp(lease_until);

        // Single atomic UPDATE with subquery.
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
        .fetch_optional(&self.0)
        .await
        .map_err(|e| map_sqlx_error("claim_next_processing", e))?;

        match row {
            Some(row) => {
                let image_id: i64 = row
                    .try_get(0)
                    .map_err(|e| map_sqlx_error("claim_next_processing", e))?;
                let image_key: String = row
                    .try_get(1)
                    .map_err(|e| map_sqlx_error("claim_next_processing", e))?;
                let local_path: String = row
                    .try_get(2)
                    .map_err(|e| map_sqlx_error("claim_next_processing", e))?;
                let processing_attempts: i64 = row
                    .try_get(3)
                    .map_err(|e| map_sqlx_error("claim_next_processing", e))?;
                let generation: i64 = row
                    .try_get(4)
                    .map_err(|e| map_sqlx_error("claim_next_processing", e))?;
                let lease_until_str: String = row
                    .try_get(5)
                    .map_err(|e| map_sqlx_error("claim_next_processing", e))?;
                let lease_until_ts = lease_until_str.parse::<Timestamp>().map_err(|e| {
                    AppError::with_source(
                        ErrorCategory::Database,
                        "claim_next_processing",
                        format!("invalid lease_until timestamp: {e}"),
                        anyhow::Error::from(e),
                    )
                })?;
                Ok(Some(ProcessingClaim {
                    image_id: ImageId::new(image_id),
                    image_key: ImageKey::new(image_key),
                    local_path: PathBuf::from(local_path),
                    processing_attempts,
                    generation,
                    lease_until: lease_until_ts,
                }))
            }
            None => Ok(None),
        }
    }

    /// Guard a processing failure transition in a single guarded UPDATE.
    ///
    /// Persists the latest available raw classifier response (if provided)
    /// for diagnostic purposes while keeping the safe error message separate.
    ///
    /// Verifies the generation token to prevent stale workers from mutating
    /// a row that has been re-claimed by another scanner.
    pub async fn fail_processing(
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

        let next_attempt_str = next_attempt_at.map(super::format_timestamp);
        let updated_at = super::format_timestamp(now);

        // Single UPDATE: status, error, raw response (coalesced with
        // existing to preserve prior diagnostic data), lease cleared,
        // next-attempt set, updated_at refreshed.
        //
        // COALESCE(NULLIF(?, ''), ...) preserves the existing value when
        // raw_response is None or an empty string.  An empty response from
        // the classifier is treated as absent — it does not overwrite a
        // prior diagnostic response.
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
        .execute(&self.0)
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

    /// Transactionally insert a classification and mark processing done.
    ///
    /// If the insert fails (e.g. uniqueness conflict) or the state update
    /// fails, neither the classification nor the state change survives.
    /// The guarded done update verifies exactly one row is affected and
    /// that the generation token matches to prevent stale workers from
    /// completing a row that has been re-claimed by another scanner.
    pub async fn complete_classification(
        &self,
        image_id: ImageId,
        classification: &ClassificationInput,
        generation: i64,
        completed_at: &Timestamp,
    ) -> AppResult<ClassificationId> {
        // Acquire SQLite's writer reservation before reading the image row.
        // A deferred transaction would first establish a read snapshot below
        // and could then fail immediately with SQLITE_BUSY when upgrading to a
        // writer while a downloader completion is active. BEGIN IMMEDIATE
        // instead waits according to the configured busy timeout before the
        // snapshot is established, making downloader/scanner contention safe.
        let mut tx = self
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("complete_classification", e))?;

        // Verify the image is currently processing with the matching generation
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

        let completed_str = super::format_timestamp(completed_at);
        let started_str = super::format_timestamp(&classification.request_started_at);
        let completed_req_str = super::format_timestamp(&classification.request_completed_at);
        let created_str = &completed_str;
        let updated_at = &completed_str;

        // Insert classification
        let _result = sqlx::query(
            r#"INSERT INTO classifications (
                   image_id, model, prompt_version, contains_wildlife, is_interesting,
                   summary, species_json, confidence, classification_json, raw_response,
                   request_started_at, request_completed_at, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        .execute(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("complete_classification_insert", e))?;

        // Update processing status to done, clearing lease/retry/error/raw-response
        // and updating timestamp. Guard: exactly one row must be affected and
        // generation must match to prevent stale completion.
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

        // Fetch the classification id
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

    // ── Lease renewal and ownership ───────────────────────────────────────

    /// Renew the processing lease for a row that is currently being processed.
    ///
    /// Extends the lease deadline by the given duration so that a long-running
    /// classification cannot be interrupted by lease recovery.  Only succeeds
    /// when the row is still in `processing` status **and** the generation
    /// matches the one held by the claiming worker — if another scanner has
    /// already recovered and re-claimed the row, this update affects zero
    /// rows and returns an ownership error.
    ///
    /// `new_lease_until` must be in the future (typically `now + lease_duration`).
    /// `renewal_at` records the actual instant of this renewal and is persisted
    /// in `updated_at` so the timestamp reflects the real mutation time rather
    /// than the future lease deadline.
    pub async fn renew_processing_lease(
        &self,
        image_id: ImageId,
        generation: i64,
        new_lease_until: &Timestamp,
        renewal_at: &Timestamp,
    ) -> AppResult<()> {
        let lease_str = super::format_timestamp(new_lease_until);
        let updated_at = super::format_timestamp(renewal_at);

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
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("renew_processing_lease", e))?;

        if result.rows_affected() == 0 {
            return Err(AppError::new(
                ErrorCategory::Database,
                "renew_processing_lease",
                format!(
                    "image {} is no longer in processing state or generation mismatch (lease may have been recovered)",
                    image_id.get()
                ),
            ));
        }
        Ok(())
    }

    /// Verify that the image is still in processing status with an unexpired
    /// lease and a matching generation token.
    ///
    /// Returns `OwnershipLost` when the row has been recovered by another scanner
    /// (status changed, lease expired, lease cleared, or generation changed).
    /// This check is performed before attempting to complete or fail a claim
    /// to prevent a stale worker from mutating a row that another worker has
    /// already reclaimed.
    pub async fn verify_processing_ownership(
        &self,
        image_id: ImageId,
        generation: i64,
    ) -> AppResult<()> {
        let row = sqlx::query(
            r#"SELECT processing_status, processing_lease_until, processing_generation
                 FROM images WHERE id = ?"#,
        )
        .bind(image_id.get())
        .fetch_one(&self.0)
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

    // ── Lease recovery ─────────────────────────────────────────────────────

    /// Return expired downloading and processing rows to retry_wait.
    ///
    /// Treats `lease_until <= now` as expired, so a lease equal to the
    /// current time is also recovered.
    pub async fn recover_expired_leases(&self, now: &Timestamp) -> AppResult<LeaseRecoveryCounts> {
        let now_str = super::format_timestamp(now);

        // Recover expired download leases (lease_until <= now is expired).
        // Refresh updated_at so recovery is visible in audit trails.
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
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("recover_expired_leases_download", e))?;

        // Recover expired processing leases (lease_until <= now is expired).
        // Do NOT reset generation — it is a monotonically increasing token
        // that distinguishes claims.  On recovery the generation is preserved
        // so that the next claim increments it further, ensuring that stale
        // workers with an old generation value are rejected.
        // Refresh updated_at so recovery is visible in audit trails.
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
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("recover_expired_leases_processing", e))?;

        Ok(LeaseRecoveryCounts {
            downloads: download_changes.rows_affected() as u64,
            processing: processing_changes.rows_affected() as u64,
        })
    }

    // ── Service metadata ───────────────────────────────────────────────────

    /// Upsert a service metadata value.
    pub async fn set_metadata(
        &self,
        key: &ServiceMetadataKey,
        value: &str,
        updated_at: &Timestamp,
    ) -> AppResult<()> {
        let updated_str = super::format_timestamp(updated_at);
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
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("set_metadata", e))?;
        Ok(())
    }

    /// Read a service metadata value.
    pub async fn get_metadata(&self, key: &ServiceMetadataKey) -> AppResult<Option<String>> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT value FROM service_metadata WHERE key = ?")
                .bind(key.as_str())
                .fetch_optional(&self.0)
                .await
                .map_err(|e| map_sqlx_error("get_metadata", e))?;
        Ok(value)
    }

    // ── Status counts ──────────────────────────────────────────────────────

    /// Return counts grouped by download and processing status.
    pub async fn status_counts(&self) -> AppResult<StatusCounts> {
        let mut counts = StatusCounts::default();

        // Download counts
        let download_rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT download_status, COUNT(*) FROM images GROUP BY download_status")
                .fetch_all(&self.0)
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

        // Processing counts
        let processing_rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT processing_status, COUNT(*) FROM images GROUP BY processing_status",
        )
        .fetch_all(&self.0)
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

    /// Return aggregate operational counters using durable image predicates.
    pub async fn operational_summary(&self) -> AppResult<OperationalSummary> {
        // Validate persisted enum values before aggregating.  SQLite CHECK
        // constraints protect normal writes, but this keeps diagnostics safe
        // for databases altered by external tools or older migrations.
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT download_status, processing_status FROM images")
                .fetch_all(&self.0)
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
        .fetch_one(&self.0)
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

    // ── Row lookup helpers (for tests) ─────────────────────────────────────

    /// Fetch a full image record by ID.
    pub async fn get_image(&self, image_id: ImageId) -> AppResult<ImageRecord> {
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
        .fetch_one(&self.0)
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

    /// Fetch a classification record by image_id, model, and prompt_version.
    pub async fn get_classification(
        &self,
        image_id: ImageId,
        model: &str,
        prompt_version: &str,
    ) -> AppResult<ClassificationRecord> {
        let row = sqlx::query(
            r#"SELECT id, image_id, model, prompt_version, contains_wildlife, is_interesting,
                      summary, species_json, confidence, classification_json, raw_response,
                      request_started_at, request_completed_at, created_at
                 FROM classifications
                 WHERE image_id = ? AND model = ? AND prompt_version = ?"#,
        )
        .bind(image_id.get())
        .bind(model)
        .bind(prompt_version)
        .fetch_one(&self.0)
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
            raw_response: row
                .try_get(10)
                .map_err(|e| map_sqlx_error("get_classification", e))?,
            request_started_at: parse_timestamp_col(&row, 11)?,
            request_completed_at: parse_timestamp_col(&row, 12)?,
            created_at: parse_timestamp_col(&row, 13)?,
        })
    }

    // ── Garbage collection candidate selection ───────────────────────────

    /// Page eligible no-wildlife image paths for garbage collection.
    ///
    /// Returns at most `limit` candidates whose `capture_start_at` is
    /// strictly less than `cutoff`, ordered by image ID for stable pagination.
    ///
    /// Eligibility requires:
    /// - `processing_status = 'done'`
    /// - `local_path IS NOT NULL`
    /// - At least one classification with `contains_wildlife = 0`
    /// - No classification with `contains_wildlife = 1`
    ///
    /// Shared-positive path safety is enforced at the filesystem level by the
    /// garbage collector (which canonicalizes paths), so this query does not
    /// filter by shared paths.  That prevents alias-direction bugs where the
    /// SQL string comparison matches one direction but not the other.
    ///
    /// When `after_image_id` is provided, only images with a greater ID are
    /// returned, enabling bounded pagination across passes.
    pub async fn garbage_collection_candidates(
        &self,
        cutoff: &Timestamp,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<GarbageCollectionCandidate>> {
        let cutoff_str = super::format_timestamp(cutoff);
        let min_id = after_image_id.map(|id| id.get());

        let query = if let Some(min_id) = min_id {
            sqlx::query_as::<_, GarbageCollectionRow>(
                r#"SELECT id, local_path
                       FROM images
                       WHERE processing_status = 'done'
                         AND local_path IS NOT NULL
                         AND capture_start_at < ?
                         AND id > ?
                         AND EXISTS (
                             SELECT 1 FROM classifications
                             WHERE classifications.image_id = images.id
                               AND classifications.contains_wildlife = 0
                         )
                         AND NOT EXISTS (
                             SELECT 1 FROM classifications
                             WHERE classifications.image_id = images.id
                               AND classifications.contains_wildlife = 1
                         )
                       ORDER BY images.id ASC
                       LIMIT ?"#,
            )
            .bind(&cutoff_str)
            .bind(min_id)
            .bind(limit)
        } else {
            sqlx::query_as::<_, GarbageCollectionRow>(
                r#"SELECT id, local_path
                       FROM images
                       WHERE processing_status = 'done'
                         AND local_path IS NOT NULL
                         AND capture_start_at < ?
                         AND EXISTS (
                             SELECT 1 FROM classifications
                             WHERE classifications.image_id = images.id
                               AND classifications.contains_wildlife = 0
                         )
                         AND NOT EXISTS (
                             SELECT 1 FROM classifications
                             WHERE classifications.image_id = images.id
                               AND classifications.contains_wildlife = 1
                         )
                       ORDER BY images.id ASC
                       LIMIT ?"#,
            )
            .bind(&cutoff_str)
            .bind(limit)
        };

        let rows: Vec<GarbageCollectionRow> = query
            .fetch_all(&self.0)
            .await
            .map_err(|e| map_sqlx_error("garbage_collection_candidates", e))?;

        let candidates = rows
            .into_iter()
            .map(|row| {
                let image_id = ImageId::new(row.id);
                let local_path = PathBuf::from(row.local_path);
                GarbageCollectionCandidate {
                    image_id,
                    local_path,
                }
            })
            .collect();

        Ok(candidates)
    }

    /// Page wildlife-positive rows whose stored path is not canonical and
    /// therefore needs filesystem reconciliation before collection.
    pub async fn wildlife_file_references_needing_reconciliation(
        &self,
        after_image_id: Option<ImageId>,
        limit: u32,
    ) -> AppResult<Vec<WildlifeFileReference>> {
        let min_id = after_image_id.map(|id| id.get());
        let query = if let Some(min_id) = min_id {
            sqlx::query_as::<_, (i64, String)>(
                r#"SELECT images.id, images.local_path
                     FROM images
                    WHERE images.id > ?
                      AND images.local_path IS NOT NULL
                      AND (images.local_file_identity IS NULL
                           OR images.local_file_identity LIKE 'unresolved:%'
                           OR images.local_path != images.local_file_identity)
                      AND EXISTS (
                          SELECT 1 FROM classifications
                           WHERE classifications.image_id = images.id
                             AND classifications.contains_wildlife = 1
                      )
                    ORDER BY images.id ASC
                    LIMIT ?"#,
            )
            .bind(min_id)
            .bind(limit)
        } else {
            sqlx::query_as::<_, (i64, String)>(
                r#"SELECT images.id, images.local_path
                     FROM images
                    WHERE images.local_path IS NOT NULL
                      AND (images.local_file_identity IS NULL
                           OR images.local_file_identity LIKE 'unresolved:%'
                           OR images.local_path != images.local_file_identity)
                      AND EXISTS (
                          SELECT 1 FROM classifications
                           WHERE classifications.image_id = images.id
                             AND classifications.contains_wildlife = 1
                      )
                    ORDER BY images.id ASC
                    LIMIT ?"#,
            )
            .bind(limit)
        };

        query
            .fetch_all(&self.0)
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

    /// Replace a legacy wildlife-positive path with its current canonical
    /// filesystem path. The guard prevents a stale reconciliation from
    /// overwriting a path changed by another operation.
    pub async fn reconcile_wildlife_file_reference(
        &self,
        reference: &WildlifeFileReference,
        canonical_path: &Path,
    ) -> AppResult<()> {
        let expected = reference.local_path.to_string_lossy().to_string();
        let canonical = canonical_path.to_string_lossy().to_string();
        sqlx::query(
            r#"UPDATE images
                  SET local_path = ?, local_file_identity = ?
                WHERE id = ? AND local_path = ?
                  AND EXISTS (
                      SELECT 1 FROM classifications
                       WHERE classifications.image_id = images.id
                         AND classifications.contains_wildlife = 1
                  )"#,
        )
        .bind(&canonical)
        .bind(&canonical)
        .bind(reference.image_id.get())
        .bind(&expected)
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("reconcile_wildlife_file_reference", e))?;
        Ok(())
    }

    /// Mark an inaccessible legacy wildlife path as unresolved. It remains
    /// fail-safe for collection, without requiring the filesystem scan to be
    /// repeated once for every candidate.
    pub async fn mark_wildlife_file_reference_unresolved(
        &self,
        reference: &WildlifeFileReference,
    ) -> AppResult<()> {
        let expected = reference.local_path.to_string_lossy().to_string();
        let marker = format!("unresolved:{expected}");
        sqlx::query(
            r#"UPDATE images
                  SET local_file_identity = ?
                WHERE id = ? AND local_path = ?
                  AND EXISTS (
                      SELECT 1 FROM classifications
                       WHERE classifications.image_id = images.id
                         AND classifications.contains_wildlife = 1
                  )"#,
        )
        .bind(&marker)
        .bind(reference.image_id.get())
        .bind(&expected)
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("mark_wildlife_file_reference_unresolved", e))?;
        Ok(())
    }

    /// Return whether a wildlife-positive row still has an unresolved or
    /// non-canonical path. This short query is also used inside the final
    /// collection transaction to close the legacy alias race.
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

    /// Serialize the final eligibility check, shared-file check, unlink, and
    /// path clearing against classification writes.
    ///
    /// The `BEGIN IMMEDIATE` transaction remains open while `operation` runs.
    /// Classification completion also uses `BEGIN IMMEDIATE`, so a positive
    /// classification cannot commit between these checks and unlinking.  The
    /// Canonical paths are persisted for new downloads and legacy wildlife
    /// paths are reconciled before this transaction starts. The short
    /// unresolved-path guard below also closes the race where a legacy row
    /// receives a positive classification after reconciliation.
    pub async fn with_gc_candidate<F, Fut, T>(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        canonical_identity: Option<&Path>,
        operation: F,
    ) -> AppResult<Option<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let cutoff_str = super::format_timestamp(cutoff);
        let path_str = candidate.local_path.to_string_lossy().to_string();
        let identity_str = canonical_identity.map(|path| path.to_string_lossy().to_string());
        let mut tx = self
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_sqlx_error("with_gc_candidate", e))?;

        let eligible: i64 = sqlx::query_scalar(
            r#"SELECT EXISTS(
                 SELECT 1 FROM images
                  WHERE id = ?
                    AND local_path = ?
                    AND processing_status = 'done'
                    AND capture_start_at < ?
                    AND EXISTS (
                        SELECT 1 FROM classifications
                         WHERE classifications.image_id = images.id
                           AND classifications.contains_wildlife = 0
                    )
                    AND NOT EXISTS (
                        SELECT 1 FROM classifications
                         WHERE classifications.image_id = images.id
                           AND classifications.contains_wildlife = 1
                    )
             )"#,
        )
        .bind(candidate.image_id.get())
        .bind(&path_str)
        .bind(&cutoff_str)
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("with_gc_candidate_eligibility", e))?;
        if eligible == 0 {
            return Ok(None);
        }

        // Exact local_path matching is required even when the identity column
        // is NULL. This protects rows created before migration 0007.
        let shared_positive: i64 = sqlx::query_scalar(
            r#"SELECT EXISTS(
                 SELECT 1 FROM images AS positive_images
                  WHERE positive_images.local_path IS NOT NULL
                    AND (positive_images.local_path = ?
                         OR positive_images.local_file_identity = ?)
                    AND EXISTS (
                        SELECT 1 FROM classifications
                         WHERE classifications.image_id = positive_images.id
                           AND classifications.contains_wildlife = 1
                    )
             )"#,
        )
        .bind(&path_str)
        .bind(&identity_str)
        .fetch_one(tx.as_mut())
        .await
        .map_err(|e| map_sqlx_error("with_gc_candidate_shared_wildlife", e))?;
        if shared_positive != 0 {
            return Ok(None);
        }

        // A legacy wildlife row that was not reconciled before this
        // transaction is ambiguous. Do not perform filesystem I/O while the
        // writer reservation is held; simply defer this candidate to the next
        // pass after reconciliation.
        if Self::has_unreconciled_wildlife_file_reference(&mut tx)
            .await
            .map_err(|e| map_sqlx_error("with_gc_candidate_unreconciled_wildlife", e))?
        {
            return Ok(None);
        }

        let operation_result = operation().await?;
        let collected_at = super::format_timestamp(&Timestamp::new(Utc::now()));
        let update = sqlx::query(
            r#"UPDATE images
                  SET local_path = NULL,
                      local_file_identity = NULL,
                      updated_at = ?
                WHERE id = ? AND local_path = ?
                  AND processing_status = 'done'
                  AND capture_start_at < ?
                  AND EXISTS (
                      SELECT 1 FROM classifications
                       WHERE classifications.image_id = images.id
                         AND classifications.contains_wildlife = 0
                  )
                  AND NOT EXISTS (
                      SELECT 1 FROM classifications
                       WHERE classifications.image_id = images.id
                         AND classifications.contains_wildlife = 1
                  )"#,
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
        Ok(Some(operation_result))
    }

    /// Clear `local_path` for a garbage-collected image.
    ///
    /// A guarded UPDATE clears `local_path` and refreshes `updated_at` only
    /// when the row still matches the expected state:
    /// - The image ID matches
    /// - The local path still matches the candidate path (prevents clearing
    ///   a path that was reassigned by another process)
    /// - The image is still eligible (done status, no wildlife classification)
    ///
    /// Shared-positive path safety is enforced at the filesystem level by the
    /// garbage collector (which canonicalizes paths), so this guard does not
    /// check for shared wildlife paths.  That prevents alias-direction bugs
    /// where a string-based SQL predicate matches one direction but not the
    /// other.
    ///
    /// Returns `Ok(())` when the update succeeds or when the row has already
    /// been cleared (idempotent). Returns an error when the guard fails for
    /// any other reason (database consistency error).
    pub async fn mark_local_file_garbage_collected(
        &self,
        candidate: &GarbageCollectionCandidate,
        cutoff: &Timestamp,
        collected_at: &Timestamp,
    ) -> AppResult<()> {
        let cutoff_str = super::format_timestamp(cutoff);
        let collected_str = super::format_timestamp(collected_at);
        let path_str = candidate.local_path.to_string_lossy().to_string();

        let result = sqlx::query(
            r#"UPDATE images
                   SET local_path = NULL,
                       local_file_identity = NULL,
                       updated_at = ?
               WHERE id = ?
                 AND local_path = ?
                 AND processing_status = 'done'
                 AND capture_start_at < ?
                 AND EXISTS (
                     SELECT 1 FROM classifications
                     WHERE classifications.image_id = images.id
                       AND classifications.contains_wildlife = 0
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM classifications
                     WHERE classifications.image_id = images.id
                       AND classifications.contains_wildlife = 1
                 )"#,
        )
        .bind(&collected_str)
        .bind(candidate.image_id.get())
        .bind(&path_str)
        .bind(&cutoff_str)
        .execute(&self.0)
        .await
        .map_err(|e| map_sqlx_error("mark_local_file_garbage_collected", e))?;

        if result.rows_affected() == 0 {
            // Check if the row was already cleared (idempotent case).
            let already_cleared: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM images WHERE id = ? AND local_path IS NULL",
            )
            .bind(candidate.image_id.get())
            .fetch_one(&self.0)
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
}

/// Raw row for GarbageCollectionCandidate deserialization.
#[derive(sqlx::FromRow)]
struct GarbageCollectionRow {
    id: i64,
    local_path: String,
}
