//! Structured logging initialization for Fauna Scan.
//!
//! Configures a tracing-subscriber with formatting, timestamps, and
//! the filter level selected via the CLI `--log-level` option.

use crate::cli::LogLevel;
use crate::error::{AppError, AppResult};

/// Initialize the global tracing subscriber at the selected log level.
///
/// This function is safe to call only once per process. If a subscriber
/// is already set, it returns an error rather than panicking.
pub fn init(level: LogLevel) -> AppResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(format!("fauna_scan={}", level.as_str()))
        .with_target(true)
        .with_file(false)
        .with_line_number(false)
        .try_init()
        .map_err(|e| {
            AppError::with_source(
                crate::error::ErrorCategory::Internal,
                "initialize_logging",
                "failed to initialize logging subscriber",
                anyhow::anyhow!("{e}"),
            )
        })?;
    Ok(())
}
