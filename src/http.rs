//! Shared HTTP infrastructure for Fauna Scan.
//!
//! Provides a reusable, timeout-aware HTTP client builder, origin
//! comparison for credential-forwarding safety, URL redaction for
//! diagnostics, and safe mapping of reqwest errors into the
//! structured error taxonomy.
//!
//! Implemented in Phase 4.

use std::time::Duration;

use url::Url;

use crate::error::{AppError, AppResult, ErrorCategory};

/// A fixed marker string used in non-sensitive contexts where a
/// placeholder value is needed (e.g. cnonce generation seeding).
/// This is not a secret — it is a compile-time constant.
pub const REDACT_MARKER: &str = "fauna-scan-phase4-http";

// ── HttpClientConfig ──────────────────────────────────────────────────────

/// Configuration for building a shared, pooled HTTP client.
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    /// Maximum time allowed to establish a TCP/TLS connection.
    pub connect_timeout: Duration,
    /// Maximum time allowed for the entire request (headers + body).
    pub request_timeout: Duration,
    /// Whether to accept invalid TLS certificates.
    ///
    /// Defaults to `false`; invalid certificates will be rejected.
    pub allow_invalid_tls_certificates: bool,
}

impl HttpClientConfig {
    /// Create a config from seconds-based durations.
    pub fn from_seconds(
        connect_timeout_secs: u64,
        request_timeout_secs: u64,
        allow_invalid_tls: bool,
    ) -> Self {
        Self {
            connect_timeout: Duration::from_secs(connect_timeout_secs),
            request_timeout: Duration::from_secs(request_timeout_secs),
            allow_invalid_tls_certificates: allow_invalid_tls,
        }
    }
}

// ── SharedHttpClient ──────────────────────────────────────────────────────

/// A shared, pooled `reqwest::Client` with configured timeouts, TLS policy,
/// and redirects disabled.
#[derive(Debug)]
pub struct SharedHttpClient {
    client: reqwest::Client,
}

impl SharedHttpClient {
    /// Build a pooled HTTP client from configuration.
    ///
    /// Fails only when the configuration itself is invalid (e.g. zero timeout).
    pub fn build(config: HttpClientConfig) -> AppResult<Self> {
        if config.connect_timeout.is_zero() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "build_http_client",
                "connect_timeout must be greater than zero",
            ));
        }
        if config.request_timeout.is_zero() {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "build_http_client",
                "request_timeout must be greater than zero",
            ));
        }

        let builder = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(config.allow_invalid_tls_certificates)
            .use_rustls_tls();

        let client = builder.build().map_err(|e| {
            AppError::with_source(
                ErrorCategory::Internal,
                "build_http_client",
                format!("failed to build HTTP client: {e}"),
                e,
            )
        })?;

        Ok(Self { client })
    }

    /// Return a reference to the underlying `reqwest::Client`.
    pub fn inner(&self) -> &reqwest::Client {
        &self.client
    }
}

// ── Origin ────────────────────────────────────────────────────────────────

/// A normalized origin (scheme + host + effective port) used to verify
/// that credentials are only sent to the configured NVR.
#[derive(Debug, Clone)]
pub struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    /// Construct an origin from a URL.
    ///
    /// Returns an error if the URL uses an unsupported scheme, has no host,
    /// or contains embedded credentials (username or password).
    pub fn from_url(url: &Url) -> AppResult<Self> {
        // Reject URLs with embedded credentials — credentials in the URL
        // could be leaked to a different origin if the URL is later
        // resolved against a different target.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "origin_from_url",
                "URL must not contain embedded credentials (username/password)",
            ));
        }

        let scheme = url.scheme().to_string();
        if scheme != "http" && scheme != "https" {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "origin_from_url",
                format!("unsupported scheme \"{}\" for origin comparison", scheme),
            ));
        }

        let host = url
            .host_str()
            .ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Protocol,
                    "origin_from_url",
                    "URL has no host for origin comparison",
                )
            })?
            .to_lowercase();

        let effective_port = url.port().unwrap_or(match scheme.as_str() {
            "https" => 443,
            _ => 80,
        });

        Ok(Self {
            scheme,
            host,
            port: effective_port,
        })
    }

    /// Check whether a URL matches this origin.
    ///
    /// The URL must use the same scheme, have a host that matches
    /// (case-insensitive), and have the same effective port.
    /// Embedded credentials in the URL do not affect the match
    /// (they are rejected separately by the caller).
    pub fn matches(&self, url: &Url) -> bool {
        if url.scheme() != self.scheme {
            return false;
        }
        let url_host = match url.host_str() {
            Some(h) => h.to_lowercase(),
            None => return false,
        };
        if url_host != self.host {
            return false;
        }
        let url_port = url.port().unwrap_or_else(|| match url.scheme() {
            "https" => 443,
            _ => 80,
        });
        url_port == self.port
    }

    /// Return the scheme.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Return the host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return the effective port.
    pub fn port(&self) -> u16 {
        self.port
    }
}

// ── URL redaction ─────────────────────────────────────────────────────────

/// Produce a safe diagnostic URL with credentials and query parameters removed.
///
/// This retains the scheme, host, port, and path for diagnostic context
/// while stripping user info and all query values.
pub fn redact_url(url: &Url) -> String {
    let mut url = url.clone();
    // Remove user info (username/password).
    let _ = url.set_username("");
    let _ = url.set_password(None);
    // Remove all query parameters.
    url.set_query(None);
    url.to_string()
}

// ── reqwest error mapping ─────────────────────────────────────────────────

/// Map a reqwest error into a safe AppError without exposing raw URLs.
///
/// Timeout errors are categorized as `Timeout`. Redirect errors (which
/// occur when redirects are disabled) are categorized as `Protocol`.
/// Remaining transport errors (DNS, socket, TLS, body transfer) are
/// categorized as `Network`. The raw reqwest error message is never
/// used because it may contain a full URL with sensitive parameters.
pub fn map_reqwest_error(operation: &'static str, error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        return AppError::new(ErrorCategory::Timeout, operation, "request timed out");
    }

    // Redirect errors occur when redirects are disabled (Policy::none).
    // These are Protocol-level issues, not transport failures.
    if error.is_redirect() {
        return AppError::new(
            ErrorCategory::Protocol,
            operation,
            "redirect response received",
        );
    }

    // Classify the remaining transport errors.
    // Check connect before request since some connection errors
    // may also have is_request() return true.
    let category = if error.is_connect() || error.is_body() || error.is_decode() {
        ErrorCategory::Network
    } else if error.is_request() {
        // Request construction error — internal.
        ErrorCategory::Internal
    } else {
        // Fallback for unclassified transport errors.
        ErrorCategory::Network
    };

    AppError::new(category, operation, "network transport error")
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Origin tests ──────────────────────────────────────────────────────

    #[test]
    fn origin_normalizes_host_casing() {
        let url = Url::parse("http://PiGate:8080/path").unwrap();
        let origin = Origin::from_url(&url).unwrap();
        assert_eq!(origin.host(), "pigate");
    }

    #[test]
    fn origin_normalizes_default_http_port() {
        let url = Url::parse("http://example.com/path").unwrap();
        let origin = Origin::from_url(&url).unwrap();
        assert_eq!(origin.port(), 80);
    }

    #[test]
    fn origin_normalizes_default_https_port() {
        let url = Url::parse("https://example.com/path").unwrap();
        let origin = Origin::from_url(&url).unwrap();
        assert_eq!(origin.port(), 443);
    }

    #[test]
    fn origin_accepts_identical_origin() {
        let url = Url::parse("http://example.com:9090/path").unwrap();
        let origin = Origin::from_url(&url).unwrap();
        assert!(origin.matches(&url));
    }

    #[test]
    fn origin_rejects_different_scheme() {
        let url = Url::parse("https://example.com:8080/path").unwrap();
        let origin =
            Origin::from_url(&Url::parse("http://example.com:8080/path").unwrap()).unwrap();
        assert!(!origin.matches(&url));
    }

    #[test]
    fn origin_rejects_different_host() {
        let url = Url::parse("http://other.com:8080/path").unwrap();
        let origin =
            Origin::from_url(&Url::parse("http://example.com:8080/path").unwrap()).unwrap();
        assert!(!origin.matches(&url));
    }

    #[test]
    fn origin_rejects_different_port() {
        let url = Url::parse("http://example.com:9090/path").unwrap();
        let origin =
            Origin::from_url(&Url::parse("http://example.com:8080/path").unwrap()).unwrap();
        assert!(!origin.matches(&url));
    }

    #[test]
    fn origin_default_port_match() {
        let url = Url::parse("http://example.com/path").unwrap();
        let origin = Origin::from_url(&Url::parse("http://example.com:80/path").unwrap()).unwrap();
        assert!(origin.matches(&url));
    }

    #[test]
    fn origin_rejects_unsupported_scheme() {
        let url = Url::parse("ftp://example.com/path").unwrap();
        let result = Origin::from_url(&url);
        assert!(result.is_err());
    }

    #[test]
    fn origin_from_url_rejects_username_only() {
        let url = Url::parse("http://user@example.com:8080/path").unwrap();
        let result = Origin::from_url(&url);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
        assert!(err.message.contains("embedded credentials"));
    }

    #[test]
    fn origin_from_url_rejects_username_and_password() {
        let url = Url::parse("http://user:pass@example.com:8080/path").unwrap();
        let result = Origin::from_url(&url);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
        assert!(err.message.contains("embedded credentials"));
    }

    #[test]
    fn origin_matches_url_with_credentials() {
        // Origin comparison should match even when URL has credentials;
        // the caller rejects credential-bearing URLs separately.
        let url = Url::parse("http://user:pass@example.com:8080/path").unwrap();
        let origin =
            Origin::from_url(&Url::parse("http://example.com:8080/path").unwrap()).unwrap();
        assert!(origin.matches(&url));
    }

    // ── Redaction tests ───────────────────────────────────────────────────

    #[test]
    fn redact_url_removes_credentials() {
        let url = Url::parse("http://user:pass@example.com/path").unwrap();
        let redacted = redact_url(&url);
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("pass"));
        assert!(redacted.contains("example.com"));
    }

    #[test]
    fn redact_url_removes_query() {
        let url = Url::parse("http://example.com/path?key=secret&foo=bar").unwrap();
        let redacted = redact_url(&url);
        assert!(!redacted.contains("key"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("foo"));
        assert!(redacted.contains("/path"));
    }

    #[test]
    fn redact_url_preserves_path() {
        let url = Url::parse("http://example.com/a/b/c?x=1").unwrap();
        let redacted = redact_url(&url);
        assert!(redacted.contains("/a/b/c"));
    }

    #[test]
    fn redact_url_no_credentials_no_query() {
        let url = Url::parse("http://example.com/path").unwrap();
        let redacted = redact_url(&url);
        assert_eq!(redacted, "http://example.com/path");
    }

    // ── HttpClientConfig tests ────────────────────────────────────────────

    #[test]
    fn from_seconds_constructs_config() {
        let config = HttpClientConfig::from_seconds(10, 30, false);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert!(!config.allow_invalid_tls_certificates);
    }
}
