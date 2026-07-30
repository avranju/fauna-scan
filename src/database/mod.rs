//! SQLite database schema, migrations, and durable state repositories.
//!
//! Implemented in Phase 3.

pub mod models;
pub mod repository;

use std::path::Path;

use sqlx::Row;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
};

use crate::domain::{DownloadStatus, ProcessingStatus, Timestamp};
use crate::error::{AppError, AppResult, ErrorCategory};

use self::repository::DatabaseOps;

/// Cloneable wrapper around an async SQLite pool with migrations applied.
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Open (or create) the SQLite database at `path`, apply migrations,
    /// and configure connection pragmas.
    pub async fn open(path: &Path) -> AppResult<Self> {
        let db_path = path.to_str().ok_or_else(|| {
            AppError::new(
                ErrorCategory::Database,
                "open",
                "database path contains invalid UTF-8",
            )
        })?;

        let mut connect_options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(10));

        // WAL mode where supported (in-memory databases don't support it).
        connect_options = match path.to_str() {
            Some(p) if !p.starts_with("file::memory:") => {
                connect_options.journal_mode(SqliteJournalMode::Wal)
            }
            _ => connect_options,
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(connect_options)
            .await
            .map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "open",
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
                    "migrate",
                    format!("migration failed for database at {}: {e}", path.display()),
                    e,
                )
            })?;

        Ok(Self { pool })
    }

    /// Return a reference to the underlying pool for integration tests.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Delegate all repository operations.
    pub fn ops(&self) -> DatabaseOps {
        DatabaseOps(self.pool.clone())
    }
}

/// Serialize a `Timestamp` to a fixed-width RFC 3339 UTC string.
///
/// Uses nanosecond precision so that lexicographic comparison in SQLite
/// is correct even when fractional seconds are present.
pub fn format_timestamp(ts: &Timestamp) -> String {
    ts.as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Helper to map sqlx::Error into AppError with a given operation name.
pub(crate) fn map_sqlx_error(operation: &'static str, e: sqlx::Error) -> AppError {
    match e {
        sqlx::Error::Database(ref db_err) => {
            let msg = db_err.message().to_string();
            let code = db_err
                .code()
                .map(|value| value.into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            tracing::error!(
                database_operation = operation,
                database_error_code = %code,
                database_error_message = %msg,
                "SQLite operation failed"
            );
            AppError::with_source(
                ErrorCategory::Database,
                operation,
                format!("database error (code={code}): {msg}"),
                e,
            )
        }
        sqlx::Error::RowNotFound => {
            tracing::error!(
                database_operation = operation,
                database_error_kind = "row_not_found",
                "Database operation failed"
            );
            AppError::new(ErrorCategory::Database, operation, "row not found")
        }
        _ => {
            let msg = e.to_string();
            tracing::error!(
                database_operation = operation,
                database_error_message = %msg,
                "Database operation failed"
            );
            AppError::with_source(
                ErrorCategory::Database,
                operation,
                format!("database operation failed: {msg}"),
                e,
            )
        }
    }
}

/// Helper to parse a TEXT column as a Timestamp, returning an AppError.
pub(crate) fn parse_timestamp_col(row: &SqliteRow, idx: usize) -> AppResult<Timestamp> {
    let s: String = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_timestamp",
            format!("failed to parse timestamp column: {e}"),
            e,
        )
    })?;
    s.parse::<Timestamp>().map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_timestamp",
            format!("invalid timestamp value '{s}': {e}"),
            e,
        )
    })
}

/// Helper to parse a TEXT column as an optional Timestamp.
pub(crate) fn parse_timestamp_col_opt(row: &SqliteRow, idx: usize) -> AppResult<Option<Timestamp>> {
    let opt: Option<String> = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_timestamp_opt",
            format!("failed to parse optional timestamp: {e}"),
            e,
        )
    })?;
    match opt {
        Some(s) => {
            let ts = s.parse::<Timestamp>().map_err(|e| {
                AppError::with_source(
                    ErrorCategory::Database,
                    "parse_timestamp_opt",
                    format!("invalid timestamp value '{s}': {e}"),
                    e,
                )
            })?;
            Ok(Some(ts))
        }
        None => Ok(None),
    }
}

/// Helper to parse a TEXT column as a DownloadStatus, returning an AppError
/// for unknown values.
pub(crate) fn parse_download_status(row: &SqliteRow, idx: usize) -> AppResult<DownloadStatus> {
    let s: String = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_download_status",
            format!("failed to parse download_status: {e}"),
            anyhow::Error::from(e),
        )
    })?;
    s.parse::<DownloadStatus>().map_err(|e_msg| {
        AppError::new(
            ErrorCategory::Database,
            "parse_download_status",
            format!("unknown download_status value '{s}': {e_msg}"),
        )
    })
}

/// Helper to parse a TEXT column as a ProcessingStatus, returning an AppError
/// for unknown values.
pub(crate) fn parse_processing_status(row: &SqliteRow, idx: usize) -> AppResult<ProcessingStatus> {
    let s: String = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_processing_status",
            format!("failed to parse processing_status: {e}"),
            anyhow::Error::from(e),
        )
    })?;
    s.parse::<ProcessingStatus>().map_err(|e_msg| {
        AppError::new(
            ErrorCategory::Database,
            "parse_processing_status",
            format!("unknown processing_status value '{s}': {e_msg}"),
        )
    })
}
