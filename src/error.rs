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
    /// Optional numeric HTTP status for transport-level categorization.
    ///
    /// Only the numeric status is stored — never a response body or raw URL.
    pub http_status: Option<u16>,
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
            http_status: None,
            source: None,
        }
    }

    /// Create a source-free categorized error with an operation and safe message.
    ///
    /// Use this for configuration and validation errors where attaching a
    /// raw source error could leak secret values or parser diagnostics.
    pub fn new(
        category: ErrorCategory,
        operation: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category,
            operation,
            message: message.into(),
            http_status: None,
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
            http_status: None,
            source: Some(source.into()),
        }
    }

    /// Attach a numeric HTTP status to an existing error without changing
    /// the category, operation, or safe message.
    ///
    /// This is used to carry status context (e.g. 403, 404) through the
    /// error chain while keeping the display output safe.
    pub fn with_http_status(mut self, status: u16) -> Self {
        self.http_status = Some(status);
        self
    }

    /// Return true if this error carries an HTTP status code.
    pub fn has_http_status(&self) -> bool {
        self.http_status.is_some()
    }

    /// Return the HTTP status code if present.
    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Format only the application-owned portions of this error's source chain.
    ///
    /// Raw third-party sources are deliberately omitted because transport and
    /// parser errors may contain request URLs, response fragments, or other
    /// sensitive external data. Every included `AppError::message` is covered
    /// by the type's existing safe-message contract.
    pub fn safe_diagnostic_chain(&self) -> String {
        const MAX_DEPTH: usize = 32;

        let mut entries = Vec::new();
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(self);
        let mut depth = 0;

        while let Some(error) = current {
            if depth >= MAX_DEPTH {
                entries.push("[diagnostic chain truncated]".to_string());
                break;
            }
            if let Some(app_error) = error.downcast_ref::<AppError>() {
                entries.push(app_error.to_string());
            }
            current = error.source();
            depth += 1;
        }

        entries.join(" <- ")
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(status) = self.http_status {
            write!(
                f,
                "[{}] {}: {} (HTTP {})",
                self.category, self.operation, self.message, status
            )
        } else {
            write!(
                f,
                "[{}] {}: {}",
                self.category, self.operation, self.message
            )
        }
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

    #[test]
    fn safe_diagnostic_chain_includes_nested_app_errors() {
        let inner = AppError::new(
            ErrorCategory::Database,
            "record_cursor_error",
            "database error (code=5): database is locked",
        );
        let outer = AppError::with_source(
            ErrorCategory::Database,
            "run",
            "camera search task failed: camera_id=1",
            inner,
        );

        let diagnostic = outer.safe_diagnostic_chain();
        assert!(diagnostic.contains("camera search task failed: camera_id=1"));
        assert!(diagnostic.contains("record_cursor_error"));
        assert!(diagnostic.contains("code=5"));
    }

    #[test]
    fn safe_diagnostic_chain_omits_raw_third_party_sources() {
        let err = AppError::with_source(
            ErrorCategory::Network,
            "fetch",
            "request failed",
            anyhow::anyhow!("https://user:secret@example.invalid/private"),
        );

        let diagnostic = err.safe_diagnostic_chain();
        assert_eq!(diagnostic, "[Network] fetch: request failed");
        assert!(!diagnostic.contains("secret"));
    }

    #[test]
    fn new_constructor_is_categorized_and_contextual() {
        let err = AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "missing required field",
        );
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert_eq!(err.operation, "load_config");
        assert!(err.source.is_none());
        let msg = format!("{err}");
        assert!(msg.contains("Configuration"));
        assert!(msg.contains("load_config"));
        assert!(msg.contains("missing required field"));
    }

    // ── HTTP status context tests ──────────────────────────────────────────

    #[test]
    fn http_status_is_none_by_default() {
        let err = AppError::new(ErrorCategory::Network, "fetch", "connection failed");
        assert!(!err.has_http_status());
        assert_eq!(err.http_status(), None);
    }

    #[test]
    fn with_http_status_attaches_code() {
        let err = AppError::new(ErrorCategory::Protocol, "request", "unexpected response")
            .with_http_status(403);
        assert!(err.has_http_status());
        assert_eq!(err.http_status(), Some(403));
    }

    #[test]
    fn display_includes_http_status() {
        let err =
            AppError::new(ErrorCategory::Protocol, "request", "forbidden").with_http_status(403);
        let msg = format!("{err}");
        assert!(msg.contains("403"));
        assert!(msg.contains("Protocol"));
        assert!(msg.contains("request"));
        assert!(msg.contains("forbidden"));
    }

    #[test]
    fn display_without_http_status_omits_code() {
        let err = AppError::new(ErrorCategory::Network, "fetch", "timeout");
        let msg = format!("{err}");
        assert!(!msg.contains("HTTP"));
        assert!(msg.contains("Network"));
    }

    #[test]
    fn with_http_status_preserves_category_and_operation() {
        let err =
            AppError::new(ErrorCategory::Authentication, "auth", "bad creds").with_http_status(401);
        assert_eq!(err.category, ErrorCategory::Authentication);
        assert_eq!(err.operation, "auth");
    }
}
