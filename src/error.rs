//! Structured error taxonomy for Fauna Scan.
//!
//! Every application failure is categorized so that retry, reporting,
//! and lifecycle decisions are explicit.

use std::fmt;

/// Top-level error categories matching the specification taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCategory {
    /// Configuration loading, parsing, or validation failure.
    Configuration,
    /// Authentication challenge or credential rejection.
    Authentication,
    /// Insufficient permissions for a requested operation.
    Authorization,
    /// General network transport failure.
    Network,
    /// Request or connection timeout.
    Timeout,
    /// Unexpected protocol-level response.
    Protocol,
    /// XML parse failure from the NVR.
    XmlParsing,
    /// NVR returned data that does not match expected semantics.
    InvalidNvrResponse,
    /// Playback URI is unreachable or the content is missing.
    PlaybackUnavailable,
    /// Local filesystem operation failure.
    Filesystem,
    /// SQLite or other database error.
    Database,
    /// Classifier HTTP transport failure.
    ClassifierTransport,
    /// Classifier returned an invalid or unexpected response body.
    ClassifierResponse,
    /// Graceful shutdown signal received.
    Shutdown,
    /// Internal invariant violation or unclassified error.
    Internal,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A contextual application error carrying category, operation, safe message,
/// and an optional source chain.
#[derive(Debug)]
pub struct AppError {
    /// The error category for retry and lifecycle decisions.
    pub category: ErrorCategory,
    /// The operation or command that was in progress.
    pub operation: &'static str,
    /// A safe, user-facing description of the failure.
    pub message: String,
    /// Optional underlying cause chain.
    pub source: Option<anyhow::Error>,
}

impl AppError {
    /// Create a "not yet implemented" error for a given operation.
    pub fn not_implemented(operation: &'static str) -> Self {
        Self {
            category: ErrorCategory::Internal,
            operation,
            message: format!("{operation} is not yet implemented in this release"),
            source: None,
        }
    }

    /// Wrap an existing error with operation context and a safe message.
    pub fn with_source(
        category: ErrorCategory,
        operation: &'static str,
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self {
            category,
            operation,
            message: message.into(),
            source: Some(source.into()),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {}: {}",
            self.category, self.operation, self.message
        )
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e.as_ref())
    }
}

/// Standardized fallible application result.
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_category_operation_and_message() {
        let err = AppError::not_implemented("run");
        let msg = format!("{err}");
        assert!(msg.contains("Internal"));
        assert!(msg.contains("run"));
        assert!(msg.contains("not yet implemented"));
    }

    #[test]
    fn not_implemented_identifies_command() {
        let err = AppError::not_implemented("download");
        assert_eq!(err.category, ErrorCategory::Internal);
        assert_eq!(err.operation, "download");
    }

    #[test]
    fn with_source_carries_underlying_error() {
        let inner = anyhow::anyhow!("io error");
        let err = AppError::with_source(
            ErrorCategory::Filesystem,
            "write_file",
            "failed to write",
            inner,
        );
        assert!(err.source.is_some());
    }
}
