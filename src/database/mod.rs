//! Backend-neutral database abstraction for Fauna Scan.
//!
//! Exposes `Database::connect` which selects a concrete backend
//! (SQLite or PostgreSQL) based on configuration, applies that backend's
//! embedded migrations, and returns a `Database` handle that yields
//! `DatabaseOps` — the cloneable façade all application code depends on.
//!
//! Concrete implementations live in [`sqlite`] and [`postgres`].

pub mod models;
pub mod repository;
pub mod sqlite;
pub mod web_models;

#[cfg(feature = "postgres")]
pub mod postgres;

use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use self::repository::DatabaseOps;

/// Connected database handle that exposes only the backend-neutral
/// [`DatabaseOps`] façade.
#[derive(Clone)]
pub struct Database {
    ops: DatabaseOps,
}

impl Database {
    /// Open the configured backend, apply migrations, and return a handle.
    pub async fn connect(
        config: &crate::configuration::DatabaseConfig,
    ) -> crate::error::AppResult<Self> {
        match config {
            crate::configuration::DatabaseConfig::Sqlite {
                path,
                max_connections,
            } => {
                let store = sqlite::SqliteDataStore::connect(path, *max_connections).await?;
                Ok(Self { ops: store.ops() })
            }
            #[cfg(feature = "postgres")]
            crate::configuration::DatabaseConfig::Postgres {
                url,
                max_connections,
            } => {
                let store = postgres::PostgresDataStore::connect(url, *max_connections).await?;
                Ok(Self { ops: store.ops() })
            }
            #[cfg(not(feature = "postgres"))]
            _ => {
                use crate::error::{AppError, ErrorCategory};
                Err(AppError::new(
                    ErrorCategory::Configuration,
                    "database_connect",
                    "PostgreSQL support not compiled in; rebuild with the `postgres` feature",
                ))
            }
        }
    }

    /// Return the backend-neutral operations façade.
    pub fn ops(&self) -> DatabaseOps {
        self.ops.clone()
    }
}

/// Serialize a `Timestamp` to a fixed-width RFC 3339 UTC string.
///
/// Uses nanosecond precision so that lexicographic comparison in both
/// SQLite and PostgreSQL is correct even when fractional seconds are present.
pub fn format_timestamp(ts: &crate::domain::Timestamp) -> String {
    ts.as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Helper to map sqlx::Error into AppError with a given operation name
/// and backend label.
pub(crate) fn map_sqlx_error(operation: &'static str, e: sqlx::Error) -> crate::error::AppError {
    use crate::error::{AppError, ErrorCategory};
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
                "Database operation failed"
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
            tracing::error!(database_operation = operation, database_error_message = %msg, "Database operation failed");
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
pub(crate) fn parse_timestamp_col(
    row: &SqliteRow,
    idx: usize,
) -> crate::error::AppResult<crate::domain::Timestamp> {
    use crate::error::{AppError, ErrorCategory};
    let s: String = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_timestamp",
            format!("failed to parse timestamp column: {e}"),
            e,
        )
    })?;
    s.parse::<crate::domain::Timestamp>().map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_timestamp",
            format!("invalid timestamp value '{s}': {e}"),
            e,
        )
    })
}

/// Helper to parse a TEXT column as an optional Timestamp.
pub(crate) fn parse_timestamp_col_opt(
    row: &SqliteRow,
    idx: usize,
) -> crate::error::AppResult<Option<crate::domain::Timestamp>> {
    use crate::error::{AppError, ErrorCategory};
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
            let ts = s.parse::<crate::domain::Timestamp>().map_err(|e| {
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
pub(crate) fn parse_download_status(
    row: &SqliteRow,
    idx: usize,
) -> crate::error::AppResult<crate::domain::DownloadStatus> {
    use crate::error::{AppError, ErrorCategory};
    let s: String = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_download_status",
            format!("failed to parse download_status: {e}"),
            e,
        )
    })?;
    s.parse::<crate::domain::DownloadStatus>().map_err(|e_msg| {
        AppError::new(
            ErrorCategory::Database,
            "parse_download_status",
            format!("unknown download_status value '{s}': {e_msg}"),
        )
    })
}

/// Helper to parse a TEXT column as a ProcessingStatus, returning an AppError
/// for unknown values.
pub(crate) fn parse_processing_status(
    row: &SqliteRow,
    idx: usize,
) -> crate::error::AppResult<crate::domain::ProcessingStatus> {
    use crate::error::{AppError, ErrorCategory};
    let s: String = row.try_get(idx).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Database,
            "parse_processing_status",
            format!("failed to parse processing_status: {e}"),
            e,
        )
    })?;
    s.parse::<crate::domain::ProcessingStatus>()
        .map_err(|e_msg| {
            AppError::new(
                ErrorCategory::Database,
                "parse_processing_status",
                format!("unknown processing_status value '{s}': {e_msg}"),
            )
        })
}
