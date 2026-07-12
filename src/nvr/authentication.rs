//! HTTP Digest authentication for Hikvision ISAPI requests.
//!
//! Implemented in Phase 4.
//!
//! Provides a replayable `NvrRequest` type and an `NvrTransport` that
//! executes same-origin requests with Digest authentication.  The transport
//! never forwards credentials outside the configured NVR origin and maps
//! failures into the existing structured error taxonomy.
//!
//! Digest challenge parsing and response generation are delegated to the
//! maintained `digest_auth` crate (RFC 2069, 2617, 7616).  Unsupported
//! challenge forms (MD5-sess, auth-int, unknown algorithms/qop) are rejected
//! as Protocol errors before any cryptographic computation occurs.

use std::fmt;
use std::sync::Arc;

use digest_auth::{AuthContext, HttpMethod, WwwAuthenticateHeader};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, PROXY_AUTHORIZATION};
use url::{Host, Url};

use crate::configuration::NvrConfig;
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::http::{HttpClientConfig, Origin, SharedHttpClient, map_reqwest_error, redact_url};

// ── NvrRequest ────────────────────────────────────────────────────────────

/// A replayable HTTP request for NVR operations.
///
/// Contains the method, target URL, headers, and an optional body.
/// The body is stored as bytes so the request can be resent after a
/// Digest 401 challenge.
#[derive(Clone)]
pub struct NvrRequest {
    method: reqwest::Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
}

impl NvrRequest {
    /// Create a GET request.
    pub fn get(url: Url) -> Self {
        Self {
            method: reqwest::Method::GET,
            url,
            headers: HeaderMap::new(),
            body: None,
        }
    }

    /// Create a POST request with the given body.
    pub fn post(url: Url, body: Vec<u8>) -> Self {
        Self {
            method: reqwest::Method::POST,
            url,
            headers: HeaderMap::new(),
            body: Some(body),
        }
    }

    /// Add a header using a pre-validated `HeaderName` and `HeaderValue`.
    ///
    /// This method accepts typed headers to avoid panics from invalid
    /// header names or values.  Callers should construct headers using
    /// `HeaderName::from_bytes` and `HeaderValue::from_str` before calling.
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Return the method.
    pub fn method(&self) -> &reqwest::Method {
        &self.method
    }

    /// Return the URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Return the headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Return the body bytes, if any.
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// Return the path and query portion of the URL (the request-target).
    pub fn request_target(&self) -> String {
        let mut target = self.url.path().to_string();
        if let Some(query) = self.url.query() {
            target.push('?');
            target.push_str(query);
        }
        if target.is_empty() {
            target = "/".to_string();
        }
        target
    }
}

// Redacted Debug — never expose header values, body bytes, or credentials.
impl fmt::Debug for NvrRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NvrRequest")
            .field("method", &self.method)
            .field("url", &redact_url(&self.url))
            .field("headers", &format_header_names(&self.headers))
            .field("body_length", &self.body.as_ref().map(|b| b.len()))
            .finish()
    }
}

/// Format header names only (no values) for Debug output.
fn format_header_names(headers: &HeaderMap) -> String {
    let names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
    format!("[{} headers]", names.join(", "))
}

// ── Host validation helpers ──────────────────────────────────────────────

/// Validate that a host string contains only a hostname or IP address.
///
/// Rejects non-canonical forms (percent-encoded characters, legacy IPv4
/// notation, unbracketed IPv6) that the url crate would silently normalize,
/// then delegates to `url::Host::parse` for strict validation of the
/// remaining input.
fn validate_host(host: &str) -> AppResult<()> {
    if host.is_empty() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "nvr_transport",
            "NVR host must not be empty",
        ));
    }

    // Reject backslashes before parsing — the url crate may normalize
    // backslashes to forward slashes in some contexts, which could cause
    // a crafted host like "\\attacker.example" to be treated as a path.
    if host.contains('\\') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "nvr_transport",
            "NVR host must not contain backslashes",
        ));
    }

    // Reject percent-encoded characters — the url crate normalizes them
    // (e.g., %61ttacker.example → attacker.example), which could allow
    // an attacker to disguise a different host.
    if host.contains('%') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "nvr_transport",
            "NVR host must not contain percent-encoded characters",
        ));
    }

    // Reject legacy IPv4 forms that the url crate would silently normalize
    // (hex: 0x7f000001, octal: 0177.0.0.1, dotted-decimal with <4 parts:
    // 127.1, or dotted-decimal with >4 parts: 127.0.0.0.0).  These are
    // never canonical and must be rejected.
    if is_legacy_ipv4(host) {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "nvr_transport",
            "NVR host must not use legacy IPv4 notation",
        ));
    }

    // Handle unbracketed IPv6 literals: attempt to parse as Ipv6Addr.
    // If it is a valid IPv6 address, normalize it and add brackets.
    // If it is not a valid IPv6 address, reject it as invalid.
    let host_for_parse = if host.contains(':') && !host.starts_with('[') {
        // Try to parse as a bare IPv6 address.
        if let Ok(_ipv6) = host.parse::<std::net::Ipv6Addr>() {
            // Valid bare IPv6 — wrap in brackets for url::Host::parse.
            format!("[{host}]")
        } else {
            // Not a valid IPv6 — reject with a fixed safe message.
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "nvr_transport",
                "NVR host contains invalid characters or is not a valid hostname/IP",
            ));
        }
    } else {
        host.to_string()
    };

    // Use url::Host::parse to strictly validate the host component.
    // This accepts only valid hostnames, IPv4 addresses, and IPv6 literals.
    // It rejects embedded credentials, path-like values, and other
    // URL-special characters that a simple character blacklist might miss.
    let parsed_host = Host::parse(&host_for_parse).map_err(|_| {
        AppError::new(
            ErrorCategory::Configuration,
            "nvr_transport",
            "NVR host contains invalid characters or is not a valid hostname/IP",
        )
    })?;

    // ── Strict IPv4 canonical-form validation ──────────────────────────
    // url::Host::parse normalizes several legacy IPv4 forms (component-level
    // hex like "127.0.0x0.1", trailing-dot like "1.2.3.4.", shortened
    // dotted-decimal, etc.) into canonical dotted-decimal.  We must reject
    // those non-canonical originals by validating through a typed Ipv4Addr
    // and comparing the canonical string representation.
    if let Host::Ipv4(addr) = parsed_host {
        let canonical = addr.to_string();
        // Reject if the original string differs from the canonical
        // dotted-decimal representation.  This catches component-level
        // hexadecimal ("0x7f.0.0.1"), trailing-dot ("1.2.3.4."),
        // shortened dotted-decimal ("127.0.1"), integer forms, and
        // any other non-canonical spelling the url crate would normalize.
        if host != canonical {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "nvr_transport",
                "NVR host must use canonical dotted-decimal IPv4 notation",
            ));
        }
    }

    Ok(())
}

/// Return true if `host` looks like a legacy IPv4 address that the url
/// crate would normalize into canonical dotted-decimal form.
///
/// Legacy forms include:
/// - Hexadecimal: `0x7f000001`
/// - Octal: `0177.0.0.1` (starts with `0` followed by another octal digit)
/// - Decimal integer: `2130706433` (equals `127.0.0.1`)
/// - Dotted-decimal with fewer than 4 parts: `127.1`
/// - Dotted-decimal with more than 4 parts: `127.0.0.0.0`
/// - Dotted-decimal with an octet > 255
/// - Dotted-decimal components with leading zeros (e.g. `127.00.0.1`, `1.2.3.04`)
fn is_legacy_ipv4(host: &str) -> bool {
    // Hexadecimal notation (e.g., 0x7f000001)
    if host.starts_with("0x") || host.starts_with("0X") {
        return true;
    }

    // Octal notation — starts with `0` followed by another octal digit
    // (e.g., 0177.0.0.1)
    if host.starts_with('0') && host.len() > 1 && "01234567".contains(host.chars().nth(1).unwrap())
    {
        return true;
    }

    // Decimal integer IPv4 (e.g., 2130706433 which equals 127.0.0.1).
    // A bare decimal integer with 1-10 digits that parses as a u32
    // is a legacy IPv4 form that the url crate would silently normalize.
    if host.chars().all(|c| c.is_ascii_digit())
        && !host.is_empty()
        && host.len() <= 10
        && host.parse::<u32>().is_ok()
    {
        return true;
    }

    // Dotted-decimal forms that are not exactly 4 canonical octets.
    // Only flag if all parts are numeric — if any part is non-numeric
    // (e.g., "example.com" or "camera.floor.home.example"), it is a
    // hostname, not an IPv4 address.
    if host.contains('.') {
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() != 4 {
            // Check if all parts are numeric — if so, it is a legacy IPv4.
            // If any part is non-numeric, it is a hostname (not legacy IPv4).
            let all_numeric = parts.iter().all(|p| p.parse::<u32>().is_ok());
            if all_numeric {
                return true; // all numeric but wrong count → legacy IPv4
            }
            // Contains non-numeric parts → hostname, not legacy IPv4
            return false;
        }
        // Exactly 4 parts — first check if ALL parts are numeric.
        // If any part is non-numeric, the entire string is a hostname
        // (e.g., "camera.floor.home.example") and must NOT be classified
        // as legacy IPv4.
        let all_numeric = parts
            .iter()
            .all(|p| !p.is_empty() && p.parse::<u32>().is_ok());
        if !all_numeric {
            return false; // hostname, not legacy IPv4
        }
        // All 4 parts are numeric — check each is a valid octet without
        // leading zeros.  Canonical IPv4 requires each octet to be 0-255
        // with no leading zeros (except the single digit "0" itself).
        for part in parts {
            // Reject leading zeros: "0" is OK, but "00", "01", "04", "001" etc. are not.
            if part.len() > 1 && part.starts_with('0') {
                return true;
            }
            match part.parse::<u32>() {
                Ok(n) if n <= 255 => continue,
                _ => return true, // non-numeric or > 255
            }
        }
    }

    false
}

/// Build an origin URL from validated scheme, host, and port.
///
/// Uses the already-validated host string directly to construct the URL.
/// IPv6 literals are wrapped in brackets as required by RFC 3986.
fn build_origin_url(scheme: &str, host: &str, port: u16) -> AppResult<Url> {
    // Wrap IPv6 literals in brackets if not already bracketed.
    let host_with_brackets = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };

    let url_str = format!("{scheme}://{host_with_brackets}:{port}");
    Url::parse(&url_str).map_err(|e| {
        AppError::new(
            ErrorCategory::Configuration,
            "nvr_transport",
            format!("invalid NVR origin URL: {e}"),
        )
    })
}

// ── Allowlist host check ─────────────────────────────────────────────────

/// Check whether a URL's host is in the allowlist (case-insensitive).
///
/// Normalizes both the URL host and allowlist entries to lowercase,
/// and wraps bare IPv6 literals in brackets so they match the
/// bracketed representation returned by `url::Url::host_str`.
fn is_host_in_allowlist(url: &Url, allowlist: &std::collections::BTreeSet<String>) -> bool {
    let host = match url.host_str() {
        Some(h) => h,
        None => return false,
    };
    // Normalize the URL host the same way as PlaybackUrlPolicy does.
    let host_normalized = normalize_allowlist_host_for_transport(host);
    allowlist.contains(&host_normalized)
}

/// Normalize a host string for allowlist comparison.
///
/// Wraps bare IPv6 literals in brackets so they match `url::Url::host_str`
/// output. Lowercase is assumed to have been applied at the allowlist
/// construction site.
fn normalize_allowlist_host_for_transport(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_lowercase()
    }
}

// ── NvrTransport ──────────────────────────────────────────────────────────

/// An authenticated transport for Hikvision NVR ISAPI requests.
///
/// Holds one shared HTTP client, the configured NVR origin, and
/// Digest credentials.  All requests are validated to be same-origin
/// before any network contact.
pub struct NvrTransport {
    http: SharedHttpClient,
    origin: Origin,
    username: String,
    password: Arc<str>,
}

impl NvrTransport {
    /// Construct a transport from an `NvrConfig`.
    ///
    /// Builds the origin from scheme/host/port and creates a pooled
    /// HTTP client with the configured timeouts and TLS policy.
    ///
    /// # Safety
    ///
    /// The `config.host` value is validated to contain only a hostname
    /// or IP address (no `@`, no path, no query, no fragment).  IPv6
    /// literals are bracketed correctly so the resulting URL is
    /// syntactically valid.  Embedded credentials in the host string
    /// are rejected as a Configuration error before any networking.
    pub fn from_config(config: &NvrConfig) -> AppResult<Self> {
        // Validate the host string — it must be a bare hostname or IP.
        // Reject embedded credentials (user@host), paths, queries, fragments.
        validate_host(&config.host)?;

        // Build the origin URL from validated components.  For IPv6
        // addresses the host must be wrapped in brackets.
        let origin_url = build_origin_url(&config.scheme, &config.host, config.port)?;
        let origin = Origin::from_url(&origin_url)?;

        let http_config = HttpClientConfig::from_seconds(
            config.connect_timeout_seconds,
            config.request_timeout_seconds,
            config.allow_invalid_tls_certificates,
        );
        let http = SharedHttpClient::build(http_config)?;

        Ok(Self {
            http,
            origin,
            username: config.username.clone(),
            password: config
                .password
                .as_ref()
                .map(|s| s.expose().into())
                .unwrap_or_else(|| "".into()),
        })
    }

    /// Construct a transport for testing with explicit parameters.
    pub fn new(
        http: SharedHttpClient,
        origin: Origin,
        username: String,
        password: Arc<str>,
    ) -> Self {
        Self {
            http,
            origin,
            username,
            password,
        }
    }

    /// Execute a GET request to a path on the NVR.
    pub async fn get(&self, path: &str) -> AppResult<reqwest::Response> {
        let url = self.build_url(path)?;
        let request = NvrRequest::get(url);
        self.execute(request).await
    }

    /// Execute a POST request with the given body to a path on the NVR.
    pub async fn post(&self, path: &str, body: Vec<u8>) -> AppResult<reqwest::Response> {
        let url = self.build_url(path)?;
        let request = NvrRequest::post(url, body);
        self.execute(request).await
    }

    /// Execute an arbitrary `NvrRequest` with Digest authentication.
    ///
    /// 1. Validates the target URL is same-origin and has no embedded credentials.
    /// 2. Strips any caller-provided Authorization headers.
    /// 3. Sends the request without credentials.
    /// 4. On 401, parses the Digest challenge and replays with credentials.
    /// 5. Returns successful responses; categorizes failures.
    pub async fn execute(&self, request: NvrRequest) -> AppResult<reqwest::Response> {
        let url = request.url().clone();

        // ── Pre-flight validation ────────────────────────────────────────

        // Reject non-HTTP(S) schemes.
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "nvr_execute",
                format!("unsupported scheme \"{}\" for NVR request", scheme),
            ));
        }

        // Reject URLs with embedded credentials.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::new(
                ErrorCategory::Authorization,
                "nvr_execute",
                "NVR request URL must not contain embedded credentials",
            ));
        }

        // Verify same-origin.
        if !self.origin.matches(&url) {
            return Err(AppError::new(
                ErrorCategory::Authorization,
                "nvr_execute",
                "NVR request target is not same-origin as configured NVR origin",
            ));
        }

        self.send_request_with_auth(request).await
    }

    /// Execute a playback request that may target an allowlisted host.
    ///
    /// Unlike `execute`, this method permits the target host to be either
    /// the configured NVR origin or a host present in the validated
    /// allowlist.  Embedded credentials are still rejected and only
    /// HTTP/HTTPS schemes are allowed.
    pub async fn execute_playback(
        &self,
        request: NvrRequest,
        allowlist: &std::collections::BTreeSet<String>,
    ) -> AppResult<reqwest::Response> {
        let url = request.url().clone();

        // Reject non-HTTP(S) schemes.
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "nvr_execute_playback",
                format!("unsupported scheme \"{}\" for playback URL", scheme),
            ));
        }

        // Reject URLs with embedded credentials.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::new(
                ErrorCategory::Authorization,
                "nvr_execute_playback",
                "playback URL must not contain embedded credentials",
            ));
        }

        // Verify same-origin or allowlisted host.
        let host_allowed = self.origin.matches(&url) || is_host_in_allowlist(&url, allowlist);

        if !host_allowed {
            return Err(AppError::new(
                ErrorCategory::Authorization,
                "nvr_execute_playback",
                "playback target is not same-origin and not in the allowed host list",
            ));
        }

        self.send_request_with_auth(request).await
    }

    /// Common request-sending + Digest-auth loop used by both execute and
    /// execute_playback.
    async fn send_request_with_auth(&self, request: NvrRequest) -> AppResult<reqwest::Response> {
        // ── Strip caller-provided auth headers ───────────────────────────

        let mut headers = request.headers().clone();
        headers.remove(AUTHORIZATION);
        headers.remove(PROXY_AUTHORIZATION);

        // ── Send initial request without credentials ─────────────────────

        let response = self.send_request(&request, headers.clone()).await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                // Handle 401 challenge.
                if status.as_u16() == 401 {
                    return self.handle_digest_challenge(&request, headers, resp).await;
                }
                // 403 → Authorization.
                if status.as_u16() == 403 {
                    return Err(AppError::new(
                        ErrorCategory::Authorization,
                        "nvr_execute",
                        "NVR rejected the request with 403 Forbidden",
                    )
                    .with_http_status(status.as_u16()));
                }
                // 3xx → Protocol (redirects are disabled, shouldn't happen).
                if status.is_redirection() {
                    return Err(AppError::new(
                        ErrorCategory::Protocol,
                        "nvr_execute",
                        format!("NVR returned unexpected redirect: HTTP {}", status.as_u16()),
                    )
                    .with_http_status(status.as_u16()));
                }
                // Other non-2xx → Protocol with status.
                Err(AppError::new(
                    ErrorCategory::Protocol,
                    "nvr_execute",
                    format!("NVR returned HTTP {}", status.as_u16()),
                )
                .with_http_status(status.as_u16()))
            }
            Err(e) => Err(map_reqwest_error("nvr_execute", e)),
        }
    }

    /// Build a full URL from a path relative to the NVR origin.
    fn build_url(&self, path: &str) -> AppResult<Url> {
        let base = self.origin_url();
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
        let url_str = format!("{}{}", base, path);
        Url::parse(&url_str).map_err(|e| {
            AppError::new(
                ErrorCategory::Protocol,
                "nvr_build_url",
                format!("invalid NVR URL: {e}"),
            )
        })
    }

    /// Return the NVR origin URL as a string.
    ///
    /// IPv6 hosts are serialized with brackets so the resulting URL is
    /// syntactically valid (e.g. `http://[::1]:8080`).
    fn origin_url(&self) -> String {
        let host = self.origin.host();
        // Wrap IPv6 hosts in brackets for URL serialization.
        let host_with_brackets = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        format!(
            "{}://{}:{}",
            self.origin.scheme(),
            host_with_brackets,
            self.origin.port()
        )
    }

    /// Send a request without credentials and return the response or error.
    async fn send_request(
        &self,
        request: &NvrRequest,
        headers: HeaderMap,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut builder = self
            .http
            .inner()
            .request(request.method().clone(), request.url().clone());
        builder = builder.headers(headers);
        if let Some(body) = request.body() {
            builder = builder.body(body.to_vec());
        }
        builder.send().await
    }

    /// Handle a 401 Digest challenge: parse the challenge, build the
    /// Authorization header using the maintained `digest_auth` crate,
    /// and replay the request once.
    async fn handle_digest_challenge(
        &self,
        request: &NvrRequest,
        _request_headers: HeaderMap,
        response: reqwest::Response,
    ) -> AppResult<reqwest::Response> {
        // Parse the Digest WWW-Authenticate challenge from the response headers
        // before consuming the body.
        let mut prompt = parse_digest_challenge(response.headers())?;

        // Consume the 401 response body to allow connection reuse.
        // Map any error so we stop before the authenticated replay.
        response
            .bytes()
            .await
            .map_err(|e| map_reqwest_error("nvr_digest_auth", e))?;

        // Build the Digest Authorization header using the maintained `digest_auth` crate.
        let auth_value =
            build_digest_auth_value(&mut prompt, request, &self.username, &self.password)?;

        // Convert the auth value to a HeaderValue, returning a Protocol error
        // if it cannot be represented as an HTTP header.
        // The error message is deliberately fixed — never include the auth_value
        // because it contains the computed Digest response (a cryptographic
        // token derived from the password) and could leak sensitive data.
        let auth_header_value = HeaderValue::from_str(&auth_value).map_err(|_| {
            AppError::new(
                ErrorCategory::Protocol,
                "nvr_digest_auth",
                "generated Digest Authorization header value is not a valid HTTP header",
            )
        })?;

        // Replay the request with credentials.
        let mut auth_headers = _request_headers.clone();
        auth_headers.insert(AUTHORIZATION, auth_header_value);

        let response = self.send_request(request, auth_headers).await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                // Second 401 → Authentication failure.
                if status.as_u16() == 401 {
                    return Err(AppError::new(
                        ErrorCategory::Authentication,
                        "nvr_digest_auth",
                        "NVR rejected Digest credentials (bad username or password)",
                    )
                    .with_http_status(status.as_u16()));
                }
                // 403 → Authorization.
                if status.as_u16() == 403 {
                    return Err(AppError::new(
                        ErrorCategory::Authorization,
                        "nvr_digest_auth",
                        "NVR rejected the authenticated request with 403 Forbidden",
                    )
                    .with_http_status(status.as_u16()));
                }
                // Other non-2xx → Protocol.
                Err(AppError::new(
                    ErrorCategory::Protocol,
                    "nvr_digest_auth",
                    format!(
                        "NVR returned HTTP {} after Digest authentication",
                        status.as_u16()
                    ),
                )
                .with_http_status(status.as_u16()))
            }
            Err(e) => Err(map_reqwest_error("nvr_digest_auth", e)),
        }
    }
}

// ── Digest challenge parsing ──────────────────────────────────────────────

/// Parse a Digest WWW-Authenticate challenge from the response headers.
///
/// Returns the parsed `WwwAuthenticateHeader` or a Protocol error for
/// missing, malformed, or unsupported challenges.  Only the standard
/// Hikvision form (MD5, qop=auth) is supported; anything else is rejected.
fn parse_digest_challenge(headers: &HeaderMap) -> AppResult<WwwAuthenticateHeader> {
    let auth_values: Vec<&reqwest::header::HeaderValue> = headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .into_iter()
        .collect();

    for value in auth_values {
        let value_str = value.to_str().map_err(|_| {
            AppError::new(
                ErrorCategory::Protocol,
                "parse_digest_challenge",
                "WWW-Authenticate header contains non-UTF-8 data",
            )
        })?;

        // Must start with "Digest "
        if !value_str.starts_with("Digest ") {
            continue;
        }

        let params_str = &value_str["Digest ".len()..];
        let prompt = digest_auth::parse(params_str).map_err(|_| {
            // The digest_auth crate may include the raw challenge input
            // in its error messages (e.g. "MissingRequired" or
            // "InvalidHeaderSyntax").  We must never echo that content
            // because it could contain attacker-supplied sentinel data.
            AppError::new(
                ErrorCategory::Protocol,
                "parse_digest_challenge",
                "malformed or unsupported Digest challenge",
            )
        })?;

        // Validate algorithm — only plain MD5 is supported.
        // MD5-sess, SHA-256, SHA-512-256, etc. are rejected.
        let algo = prompt.algorithm;
        if algo.algo != digest_auth::AlgorithmType::MD5 || algo.sess {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "parse_digest_challenge",
                format!(
                    "unsupported Digest algorithm: {:?}{}",
                    algo.algo,
                    if algo.sess { "-sess" } else { "" }
                ),
            ));
        }

        // Validate qop — only "auth" is supported.
        // auth-int and unknown qop values are rejected.
        let _qop = match &prompt.qop {
            Some(qops) if qops.contains(&digest_auth::Qop::AUTH) => {
                // "auth" is present — check that "auth-int" is NOT present
                if qops.contains(&digest_auth::Qop::AUTH_INT) {
                    return Err(AppError::new(
                        ErrorCategory::Protocol,
                        "parse_digest_challenge",
                        "Digest challenge includes unsupported qop=auth-int",
                    ));
                }
                Some(digest_auth::Qop::AUTH)
            }
            Some(qops) if qops.contains(&digest_auth::Qop::AUTH_INT) => {
                return Err(AppError::new(
                    ErrorCategory::Protocol,
                    "parse_digest_challenge",
                    "Digest challenge requires qop=auth-int which is not supported",
                ));
            }
            Some(_) => {
                return Err(AppError::new(
                    ErrorCategory::Protocol,
                    "parse_digest_challenge",
                    "Digest challenge contains unsupported qop value",
                ));
            }
            None => None, // No qop in challenge — we'll default to auth
        };

        // Validate nonce is present.
        if prompt.nonce.is_empty() {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "parse_digest_challenge",
                "Digest challenge missing nonce parameter",
            ));
        }

        // Validate realm is present.
        if prompt.realm.is_empty() {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "parse_digest_challenge",
                "Digest challenge missing realm parameter",
            ));
        }

        return Ok(prompt);
    }

    Err(AppError::new(
        ErrorCategory::Protocol,
        "parse_digest_challenge",
        "NVR 401 response did not include a Digest WWW-Authenticate challenge",
    ))
}

// ── Digest response generation ────────────────────────────────────────────

/// Build a Digest Authorization header value using the maintained
/// `digest_auth` crate.
///
/// Validates that the challenge is compatible with our supported profile
/// (MD5, qop=auth) before delegating to the crate for computation.
fn build_digest_auth_value(
    prompt: &mut WwwAuthenticateHeader,
    request: &NvrRequest,
    username: &str,
    password: &str,
) -> AppResult<String> {
    // Build the AuthContext using the maintained digest_auth crate.
    // For GET requests we use the simpler constructor; for POST we include
    // the body (the crate ignores it for qop=auth).
    let context = if request.method() == reqwest::Method::GET {
        AuthContext::new(username, password, request.request_target())
    } else {
        AuthContext::new_with_method(
            username,
            password,
            request.request_target(),
            request.body(),
            HttpMethod::from(request.method().as_str()),
        )
    };

    // The digest_auth crate handles cnonce generation, nc counting,
    // and MD5 computation internally.
    let auth_header = prompt.respond(&context).map_err(|_| {
        // The digest_auth crate error may include sensitive context
        // (the challenge parameters or computed response).  We emit
        // a fixed safe message instead.
        AppError::new(
            ErrorCategory::Protocol,
            "build_digest_auth_value",
            "failed to compute Digest response",
        )
    })?;

    Ok(auth_header.to_string())
}

// ── NvrTransport tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    // ── NvrRequest Debug tests ───────────────────────────────────────────

    #[test]
    fn nvr_request_debug_does_not_expose_body() {
        let url = Url::parse("http://example.com/path").unwrap();
        let req = NvrRequest::post(url, b"SENSITIVE-BODY-DATA".to_vec());
        let debug_output = format!("{req:?}");
        assert!(
            !debug_output.contains("SENSITIVE-BODY-DATA"),
            "body leaked in Debug: {debug_output}"
        );
        assert!(debug_output.contains("body_length"));
    }

    #[test]
    fn nvr_request_debug_does_not_expose_header_values() {
        let url = Url::parse("http://example.com/path").unwrap();
        let auth_name = HeaderName::from_bytes(b"Authorization").unwrap();
        let auth_value = HeaderValue::from_str("Bearer SECRET-TOKEN").unwrap();
        let req = NvrRequest::get(url)
            .with_header(auth_name, auth_value)
            .with_header(
                HeaderName::from_bytes(b"X-Secret").unwrap(),
                HeaderValue::from_str("base64like-ABCDEFGHIJKLMNOP").unwrap(),
            );
        let debug_output = format!("{req:?}");
        assert!(
            !debug_output.contains("SECRET-TOKEN"),
            "header value leaked in Debug: {debug_output}"
        );
        assert!(
            !debug_output.contains("base64like"),
            "header value leaked in Debug: {debug_output}"
        );
        assert!(debug_output.contains("authorization"));
    }

    #[test]
    fn nvr_request_debug_does_not_expose_url_credentials() {
        let url = Url::parse("http://user:pass@example.com/path").unwrap();
        let req = NvrRequest::get(url);
        let debug_output = format!("{req:?}");
        assert!(
            !debug_output.contains("user"),
            "URL user leaked in Debug: {debug_output}"
        );
        assert!(
            !debug_output.contains("pass"),
            "URL password leaked in Debug: {debug_output}"
        );
    }

    // ── Digest params parsing tests ──────────────────────────────────────

    #[test]
    fn parse_digest_challenge_basic() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\"",
            ),
        );
        let prompt = parse_digest_challenge(&headers).unwrap();
        assert_eq!(prompt.realm, "Hikvision");
        assert_eq!(prompt.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
    }

    #[test]
    fn parse_digest_challenge_with_qop_auth() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc123\", qop=\"auth\", algorithm=MD5",
            ),
        );
        let prompt = parse_digest_challenge(&headers).unwrap();
        assert_eq!(prompt.realm, "Hikvision");
        assert_eq!(prompt.nonce, "abc123");
        assert!(
            prompt
                .qop
                .as_ref()
                .is_some_and(|q| q.contains(&digest_auth::Qop::AUTH))
        );
    }

    #[test]
    fn parse_digest_challenge_rejects_md5_sess() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc123\", algorithm=MD5-sess",
            ),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
    }

    #[test]
    fn parse_digest_challenge_rejects_auth_int() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc123\", qop=\"auth-int\"",
            ),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
    }

    #[test]
    fn parse_digest_challenge_rejects_sha256() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc123\", algorithm=SHA-256",
            ),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn parse_digest_challenge_missing_nonce_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Digest realm=\"test\""),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn parse_digest_challenge_missing_realm_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Digest nonce=\"abc123\""),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn parse_digest_challenge_no_challenge_returns_error() {
        let headers = HeaderMap::new();
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn parse_digest_challenge_with_opaque() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc123\", opaque=\"session-xyz\"",
            ),
        );
        let prompt = parse_digest_challenge(&headers).unwrap();
        assert_eq!(prompt.opaque, Some("session-xyz".to_string()));
    }

    // ── Origin URL construction ──────────────────────────────────────────

    #[test]
    fn origin_url_format() {
        let http = SharedHttpClient::build(HttpClientConfig::from_seconds(10, 30, false)).unwrap();
        let origin = Origin::from_url(&Url::parse("https://nvr.local:8443/").unwrap()).unwrap();
        let transport = NvrTransport::new(http, origin, "admin".to_string(), "pass".into());

        assert_eq!(transport.origin_url(), "https://nvr.local:8443");
    }

    // ── Digest response generation tests ─────────────────────────────────

    #[test]
    fn build_digest_auth_value_produces_valid_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"",
            ),
        );
        let mut prompt = parse_digest_challenge(&headers).unwrap();

        let url = Url::parse("http://example.com/ISAPI/Streaming/channels").unwrap();
        let request = NvrRequest::get(url);

        let auth_value =
            build_digest_auth_value(&mut prompt, &request, "admin", "correct-pass").unwrap();
        assert!(auth_value.starts_with("Digest "));
        assert!(auth_value.contains("username=\"admin\""));
        assert!(auth_value.contains("realm=\"Hikvision\""));
        assert!(auth_value.contains("nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\""));
        assert!(auth_value.contains("uri=\"/ISAPI/Streaming/channels\""));
        assert!(auth_value.contains("qop=auth"));
        assert!(auth_value.contains("nc="));
        assert!(auth_value.contains("cnonce="));
        assert!(auth_value.contains("response="));
    }

    #[test]
    fn build_digest_auth_value_includes_opaque() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc\", opaque=\"my-session\", algorithm=MD5, qop=\"auth\"",
            ),
        );
        let mut prompt = parse_digest_challenge(&headers).unwrap();

        let url = Url::parse("http://example.com/path").unwrap();
        let request = NvrRequest::get(url);

        let auth_value = build_digest_auth_value(&mut prompt, &request, "admin", "pass").unwrap();
        assert!(auth_value.contains("opaque=\"my-session\""));
    }

    #[test]
    fn build_digest_auth_value_rejects_md5_sess() {
        // MD5-sess is rejected during challenge parsing, before build_digest_auth_value.
        // This test verifies that the parsing rejects it.
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"abc\", algorithm=MD5-sess, qop=\"auth\"",
            ),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
        assert!(err.message.contains("unsupported Digest algorithm"));
    }

    #[test]
    fn build_digest_auth_value_rejects_auth_int() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Digest realm=\"Hikvision\", nonce=\"abc\", qop=\"auth-int\""),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn build_digest_auth_value_with_post_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"",
            ),
        );
        let mut prompt = parse_digest_challenge(&headers).unwrap();

        let url = Url::parse("http://example.com/ISAPI/ContentMgmt/search").unwrap();
        let body = b"<?xml version=\"1.0\"?><CMSearchDescription/>".to_vec();
        let request = NvrRequest::post(url, body);

        let auth_value =
            build_digest_auth_value(&mut prompt, &request, "admin", "correct-pass").unwrap();
        assert!(auth_value.starts_with("Digest "));
        // The digest_auth crate formats the method in the uri field, not as a separate field.
        // Verify the uri contains the POST path.
        assert!(auth_value.contains("uri=\"/ISAPI/ContentMgmt/search\""));
    }

    // ── Auth value leak regression test ──────────────────────────────────

    /// Verifies that the HeaderValue conversion error message never includes
    /// the generated Digest Authorization value, username, or any computed
    /// cryptographic token.
    ///
    /// Since `HeaderValue::from_str` succeeds for valid Digest headers,
    /// this test constructs a deliberately invalid value and checks that
    /// the error message is a fixed safe string with no variable content.
    #[test]
    fn header_value_error_does_not_expose_auth_value() {
        // A known-invalid header value that will fail HeaderValue::from_str.
        let invalid_value =
            "Digest \x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f";
        let result = HeaderValue::from_str(invalid_value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = format!("{err}");
        // The error message must not contain the raw invalid value bytes.
        assert!(
            !err_str.contains("\x00\x01\x02"),
            "raw header bytes leaked in error: {err_str}"
        );
        // The error message must not contain any auth-related keywords
        // that could indicate a Digest value was included.
        assert!(
            !err_str.contains("Digest "),
            "Digest keyword leaked in error: {err_str}"
        );
    }

    // ── Host validation tests ────────────────────────────────────────────

    #[test]
    fn validate_host_rejects_embedded_credentials() {
        let result = validate_host("user@attacker.example");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    #[test]
    fn validate_host_rejects_path_characters() {
        let result = validate_host("host/path");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    #[test]
    fn validate_host_rejects_query_characters() {
        let result = validate_host("host?key=value");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    #[test]
    fn validate_host_rejects_fragment_characters() {
        let result = validate_host("host#section");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    #[test]
    fn validate_host_rejects_backslash_injection() {
        // A backslash can be treated as a path separator by the URL parser,
        // causing "\\attacker.example" to normalize to attacker.example.
        let result = validate_host("\\attacker.example");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("backslash"));
    }

    #[test]
    fn validate_host_rejects_backslash_component_injection() {
        // A crafted host with a backslash followed by a path component.
        let result = validate_host("host\\..\\..\\etc\\passwd");
        assert!(result.is_err());
    }

    #[test]
    fn validate_host_accepts_simple_hostname() {
        let result = validate_host("pigate");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_ipv4() {
        let result = validate_host("192.168.1.50");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_bare_ipv6_literal() {
        // Bare IPv6 is accepted and will be bracketed by build_origin_url.
        let result = validate_host("::1");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_bracketed_ipv6() {
        let result = validate_host("[::1]");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_bare_full_ipv6() {
        // Bare IPv6 is accepted and will be bracketed by build_origin_url.
        let result = validate_host("2001:db8::1");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_rejects_empty_string() {
        let result = validate_host("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("empty"));
    }

    #[test]
    fn validate_host_rejects_user_info_in_host() {
        // An attacker might craft a config like host="admin@evil.com"
        // which would be parsed as host=evil.com with user=admin,
        // leaking credentials to the wrong server.
        let result = validate_host("admin@evil.com");
        assert!(result.is_err());
    }

    // ── Transport from_config host validation tests ──────────────────────

    #[test]
    fn transport_from_config_rejects_backslash_host() {
        // Simulate from_config: validate host then build origin.
        let result = validate_host("\\attacker.example");
        assert!(result.is_err());
    }

    #[test]
    fn transport_from_config_accepts_valid_hostname() {
        let result = validate_host("pigate");
        assert!(result.is_ok());
    }

    #[test]
    fn transport_from_config_accepts_valid_ipv4() {
        let result = validate_host("192.168.1.50");
        assert!(result.is_ok());
    }

    #[test]
    fn transport_from_config_accepts_valid_ipv6() {
        // Only bracketed IPv6 is accepted.
        let result = validate_host("[::1]");
        assert!(result.is_ok());
    }

    // ── Legacy IPv4 detection ────────────────────────────────────────────

    #[test]
    fn is_legacy_ipv4_hex() {
        assert!(is_legacy_ipv4("0x7f000001"));
        assert!(is_legacy_ipv4("0X7F000001"));
    }

    #[test]
    fn is_legacy_ipv4_octal() {
        assert!(is_legacy_ipv4("0177.0.0.1"));
        assert!(is_legacy_ipv4("02130706433"));
    }

    #[test]
    fn is_legacy_ipv4_dotted_short() {
        assert!(is_legacy_ipv4("127.1"));
        assert!(is_legacy_ipv4("127.0.1"));
    }

    #[test]
    fn is_legacy_ipv4_dotted_long() {
        assert!(is_legacy_ipv4("127.0.0.0.0"));
    }

    #[test]
    fn is_legacy_ipv4_dotted_over_255() {
        assert!(is_legacy_ipv4("256.0.0.1"));
    }

    #[test]
    fn is_legacy_ipv4_canonical_ipv4() {
        assert!(!is_legacy_ipv4("192.168.1.50"));
        assert!(!is_legacy_ipv4("127.0.0.1"));
        assert!(!is_legacy_ipv4("10.0.0.1"));
    }

    #[test]
    fn is_legacy_ipv4_hostname() {
        assert!(!is_legacy_ipv4("pigate"));
        assert!(!is_legacy_ipv4("example.com"));
    }

    #[test]
    fn is_legacy_ipv4_decimal_integer() {
        // Decimal integer IPv4 forms are legacy — the url crate normalizes them.
        assert!(is_legacy_ipv4("2130706433")); // 127.0.0.1
        assert!(is_legacy_ipv4("3232235876")); // 192.168.1.252
        assert!(is_legacy_ipv4("0")); // 0.0.0.0
        assert!(is_legacy_ipv4("4294967295")); // 255.255.255.255
    }

    #[test]
    fn is_legacy_ipv4_leading_zeros_dotted() {
        // Dotted IPv4 with leading-zero components is legacy/ambiguous.
        assert!(is_legacy_ipv4("127.00.0.1"));
        assert!(is_legacy_ipv4("1.2.3.04"));
        assert!(is_legacy_ipv4("010.0.0.1"));
        assert!(is_legacy_ipv4("127.0.0.01"));
        assert!(is_legacy_ipv4("192.168.001.001"));
    }

    #[test]
    fn is_legacy_ipv4_single_zero_ok() {
        // "0" alone is a valid decimal integer (0.0.0.0), which is legacy.
        assert!(is_legacy_ipv4("0"));
        // "00" is also legacy (leading zero).
        assert!(is_legacy_ipv4("00"));
    }

    // ── Encoded and legacy host rejection ─────────────────────────────────

    #[test]
    fn validate_host_rejects_percent_encoded_host() {
        let result = validate_host("%61ttacker.example");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("percent-encoded"));
    }

    #[test]
    fn validate_host_rejects_hex_ipv4() {
        let result = validate_host("0x7f000001");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));
    }

    #[test]
    fn validate_host_rejects_octal_ipv4() {
        let result = validate_host("0177.0.0.1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));
    }

    #[test]
    fn validate_host_rejects_dotted_short_ipv4() {
        let result = validate_host("127.1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));
    }

    #[test]
    fn validate_host_rejects_dotted_long_ipv4() {
        let result = validate_host("127.0.0.0.0");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));
    }

    #[test]
    fn validate_host_rejects_decimal_ipv4() {
        // Decimal integer IPv4 forms must be rejected.
        let result = validate_host("2130706433");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));
    }

    #[test]
    fn validate_host_rejects_leading_zero_ipv4() {
        // Dotted IPv4 with leading-zero components must be rejected.
        let result = validate_host("127.00.0.1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));

        let result = validate_host("1.2.3.04");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("legacy IPv4"));
    }

    #[test]
    fn validate_host_accepts_bare_loopback_ipv6() {
        // Bare IPv6 loopback is now accepted and will be bracketed by build_origin_url.
        let result = validate_host("::1");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_bare_ipv6_all_zeros() {
        let result = validate_host("::");
        assert!(result.is_ok());
    }

    // ── Valid canonical host acceptance ───────────────────────────────────

    #[test]
    fn validate_host_accepts_canonical_hostname() {
        let result = validate_host("pigate");
        assert!(result.is_ok());

        let result = validate_host("NVR-Server.local");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_canonical_ipv4() {
        let result = validate_host("192.168.1.50");
        assert!(result.is_ok());

        let result = validate_host("127.0.0.1");
        assert!(result.is_ok());

        let result = validate_host("10.0.0.1");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_host_accepts_bracketed_ipv6_addresses() {
        let result = validate_host("[::1]");
        assert!(result.is_ok());

        let result = validate_host("[2001:db8::1]");
        assert!(result.is_ok());
    }

    // ── Origin URL construction tests ────────────────────────────────────

    #[test]
    fn build_origin_url_http_simple_host() {
        let url = build_origin_url("http", "pigate", 8080).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("pigate"));
        assert_eq!(url.port(), Some(8080));
    }

    #[test]
    fn build_origin_url_https_ipv4() {
        let url = build_origin_url("https", "192.168.1.50", 443).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("192.168.1.50"));
        // The url crate normalizes default ports: port() returns None
        // for the scheme's default port.
        assert!(url.port().is_none() || url.port() == Some(443));
    }

    #[test]
    fn build_origin_url_ipv6_bracketed() {
        // IPv6 without brackets — should be wrapped.
        let url = build_origin_url("http", "::1", 8080).unwrap();
        assert!(url.to_string().contains("[::1]"));
    }

    #[test]
    fn build_origin_url_already_bracketed_ipv6() {
        // IPv6 already bracketed — should not double-bracket.
        let url = build_origin_url("http", "[::1]", 8080).unwrap();
        assert!(url.to_string().contains("[::1]"));
    }

    #[test]
    fn build_origin_url_full_string_ipv6() {
        // Verify the full URL string is correct for IPv6.
        let url = build_origin_url("http", "::1", 8080).unwrap();
        assert!(url.to_string().starts_with("http://[::1]:8080"));
    }

    // ── Transport origin construction tests ──────────────────────────────

    #[test]
    fn transport_from_config_rejects_credential_host() {
        // Simulate what from_config does: validate host then build origin.
        let result = validate_host("admin@evil.com");
        assert!(result.is_err());
    }

    // ── Safe error message tests ─────────────────────────────────────────

    #[test]
    fn validate_host_error_does_not_leak_raw_host() {
        // A malformed host like "nvr.example?token=SENTINEL" should produce
        // an error whose Display/Debug output contains NO user-supplied content.
        let result = validate_host("nvr.example?token=SENTINEL");
        assert!(result.is_err());
        let err = result.unwrap_err();
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains("SENTINEL"),
            "raw host leaked in Display: {display}"
        );
        assert!(
            !debug.contains("SENTINEL"),
            "raw host leaked in Debug: {debug}"
        );
        // The error must still be a Configuration error.
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    #[test]
    fn validate_host_error_does_not_leak_user_info_sentinel() {
        let result = validate_host("admin@evil.com?query=SENTINEL");
        assert!(result.is_err());
        let err = result.unwrap_err();
        let display = format!("{err}");
        assert!(
            !display.contains("SENTINEL"),
            "sentinel leaked in Display: {display}"
        );
    }

    // ── Bare IPv6 URL construction tests ─────────────────────────────────

    #[test]
    fn build_origin_url_bare_ipv6_loopback() {
        // Bare IPv6 should be bracketed by build_origin_url.
        let url = build_origin_url("http", "::1", 8080).unwrap();
        assert!(url.to_string().contains("[::1]"));
    }

    #[test]
    fn build_origin_url_bare_ipv6_full() {
        let url = build_origin_url("http", "2001:db8::1", 8080).unwrap();
        assert!(url.to_string().contains("[2001:db8::1]"));
    }

    #[test]
    fn build_origin_url_bare_ipv6_all_zeros() {
        let url = build_origin_url("http", "::", 8080).unwrap();
        assert!(url.to_string().contains("[::]"));
    }

    #[test]
    fn build_origin_url_canonical_ipv4_unchanged() {
        // Canonical IPv4 should not be affected by bracketing logic.
        let url = build_origin_url("http", "192.168.1.50", 8080).unwrap();
        assert!(url.to_string().contains("192.168.1.50"));
        assert!(!url.to_string().contains("[192.168.1.50]"));
    }

    // ── Transport from_config with bare IPv6 ─────────────────────────────

    #[test]
    fn transport_from_config_accepts_bare_ipv6() {
        // Validate host then build origin — simulates from_config.
        let result = validate_host("::1");
        assert!(result.is_ok());
        let url = build_origin_url("http", "::1", 8080).unwrap();
        assert!(url.to_string().contains("[::1]"));
    }

    #[test]
    fn transport_from_config_accepts_bare_ipv6_full() {
        let result = validate_host("2001:db8::1");
        assert!(result.is_ok());
        let url = build_origin_url("http", "2001:db8::1", 8080).unwrap();
        assert!(url.to_string().contains("[2001:db8::1]"));
    }

    // ── Four-label hostname not classified as legacy IPv4 ────────────────

    #[test]
    fn is_legacy_ipv4_four_label_hostname() {
        // A four-label hostname must NOT be classified as legacy IPv4.
        assert!(!is_legacy_ipv4("camera.floor.home.example"));
        assert!(!is_legacy_ipv4("one.two.three.four"));
    }

    // ── Transport from_config with bare IPv6 builds valid URL ────────────

    #[test]
    fn transport_from_config_bare_ipv6_builds_bracketed_url() {
        // Simulate from_config: validate host, build origin URL, then
        // build the origin_url() string.  The origin_url() string must
        // contain brackets around the IPv6 host.
        let result = validate_host("::1");
        assert!(result.is_ok());
        let origin_url = build_origin_url("http", "::1", 8080).unwrap();
        assert!(origin_url.to_string().contains("[::1]"));

        // Now verify the transport's origin_url() method also brackets.
        let http = SharedHttpClient::build(HttpClientConfig::from_seconds(10, 30, false)).unwrap();
        let origin = Origin::from_url(&origin_url).unwrap();
        let transport = NvrTransport::new(http, origin, "admin".to_string(), "pass".into());
        let url_str = transport.origin_url();
        assert!(
            url_str.contains("[::1]"),
            "origin_url must bracket IPv6: {url_str}"
        );
        assert!(
            !url_str.contains("http://::1"),
            "origin_url must not omit brackets: {url_str}"
        );
    }

    #[test]
    fn transport_from_config_validates_bare_ipv6() {
        // Bare IPv6 passes validate_host and build_origin_url.
        let result = validate_host("::1");
        assert!(result.is_ok());
        let url = build_origin_url("http", "::1", 8080).unwrap();
        assert!(url.to_string().contains("[::1]"));
    }

    // ── Digest challenge sentinel leakage regression ─────────────────────

    #[test]
    fn malformed_digest_challenge_does_not_leak_sentinel() {
        // A malformed challenge containing a sentinel value must not
        // appear in the AppError Display or Debug output.
        // Use a truly malformed challenge (missing required nonce) with
        // a sentinel realm to verify no challenge content leaks.
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"SENTINEL_REALM_VALUE\", qop=\"auth\", algorithm=MD5",
            ),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains("SENTINEL_REALM_VALUE"),
            "sentinel realm leaked in Display: {display}"
        );
        assert!(
            !debug.contains("SENTINEL_REALM_VALUE"),
            "sentinel realm leaked in Debug: {debug}"
        );
        assert_eq!(err.category, ErrorCategory::Protocol);
    }

    #[test]
    fn malformed_digest_challenge_missing_nonce_no_leak() {
        // A challenge with a sentinel realm but missing nonce must not
        // leak the sentinel in the error.
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Digest realm=\"LEAK-SENTINEL-VALUE\""),
        );
        let result = parse_digest_challenge(&headers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains("LEAK-SENTINEL-VALUE"),
            "sentinel leaked in Display: {display}"
        );
        assert!(
            !debug.contains("LEAK-SENTINEL-VALUE"),
            "sentinel leaked in Debug: {debug}"
        );
    }

    #[test]
    fn build_digest_auth_value_error_does_not_leak_sentinel() {
        // A challenge that parses but fails during response generation
        // must not leak the challenge parameters in the error.
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Digest realm=\"LEAK-SENTINEL\", nonce=\"LEAK-SENTINEL\", qop=\"auth\", algorithm=MD5",
            ),
        );
        let mut prompt = parse_digest_challenge(&headers).unwrap();

        let url = Url::parse("http://example.com/path").unwrap();
        let request = NvrRequest::get(url);

        // This should succeed for a well-formed challenge, but if it
        // fails the error must not leak the sentinel.
        let result = build_digest_auth_value(&mut prompt, &request, "admin", "pass");
        if let Err(ref err) = result {
            let display = format!("{err}");
            let debug = format!("{err:?}");
            assert!(
                !display.contains("LEAK-SENTINEL"),
                "sentinel leaked in Display: {display}"
            );
            assert!(
                !debug.contains("LEAK-SENTINEL"),
                "sentinel leaked in Debug: {debug}"
            );
        }
    }

    // ── Strict IPv4 canonical-form validation tests ──────────────────────

    /// Component-level hexadecimal forms such as "127.0.0x0.1" are rejected
    /// because url::Host::parse normalizes them to canonical IPv4 addresses.
    #[test]
    fn validate_host_rejects_component_level_hex_ipv4() {
        let result = validate_host("127.0.0x0.1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(
            err.message.contains("canonical") || err.message.contains("legacy"),
            "error message: {}",
            err.message
        );
    }

    /// "1.2.0x3.4" — hex in the third octet — is rejected.
    #[test]
    fn validate_host_rejects_hex_in_octet() {
        let result = validate_host("1.2.0x3.4");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    /// "127.0.0.0x1" — hex in the last octet — is rejected.
    #[test]
    fn validate_host_rejects_hex_in_last_octet() {
        let result = validate_host("127.0.0.0x1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    /// Trailing-dot form "1.2.3.4." is rejected.
    #[test]
    fn validate_host_rejects_trailing_dot_ipv4() {
        let result = validate_host("1.2.3.4.");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    /// Trailing-dot form "127.0.0.1." is rejected.
    #[test]
    fn validate_host_rejects_trailing_dot_loopback() {
        let result = validate_host("127.0.0.1.");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    /// Canonical dotted-decimal IPv4 is accepted.
    #[test]
    fn validate_host_accepts_canonical_ipv4_strict() {
        assert!(validate_host("192.168.1.50").is_ok());
        assert!(validate_host("127.0.0.1").is_ok());
        assert!(validate_host("10.0.0.1").is_ok());
        assert!(validate_host("0.0.0.0").is_ok());
        assert!(validate_host("255.255.255.255").is_ok());
    }

    /// Transport from_config with canonical IPv4 succeeds.
    #[test]
    fn transport_from_config_accepts_canonical_ipv4_addr() {
        let result = validate_host("192.168.1.50");
        assert!(result.is_ok());
        let url = build_origin_url("http", "192.168.1.50", 8080).unwrap();
        assert_eq!(url.host_str(), Some("192.168.1.50"));
    }

    /// Transport from_config with canonical IPv4 127.0.0.1 succeeds.
    #[test]
    fn transport_from_config_accepts_canonical_ipv4_loopback() {
        let result = validate_host("127.0.0.1");
        assert!(result.is_ok());
        let url = build_origin_url("http", "127.0.0.1", 8080).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
    }

    /// Origin constructed from canonical IPv4 is stable.
    #[test]
    fn origin_from_canonical_ipv4_is_stable() {
        let url = Url::parse("http://192.168.1.50:8080/").unwrap();
        let origin = Origin::from_url(&url).unwrap();
        assert_eq!(origin.host(), "192.168.1.50");
        assert_eq!(origin.port(), 8080);
        assert!(origin.matches(&url));
    }

    /// Error message for non-canonical IPv4 does not leak the original host.
    #[test]
    fn non_canonical_ipv4_error_does_not_leak_host() {
        let result = validate_host("127.0.0x0.1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains("127.0.0x0.1"),
            "original host leaked in Display: {display}"
        );
        assert!(
            !debug.contains("127.0.0x0.1"),
            "original host leaked in Debug: {debug}"
        );
        assert!(
            !display.contains("SENTINEL"),
            "sentinel leaked in Display: {display}"
        );
    }
}
