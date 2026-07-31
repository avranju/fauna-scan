//! Authenticated image download from NVR playback URIs.
//!
//! Implemented in Phase 7.
//!
//! Provides `PlaybackUrlPolicy` for resolving and authorizing returned
//! playback URIs according to rebasing and allowlist configuration, and
//! `ImageDownloadClient` for fetching authorized playback responses
//! through the Digest-authenticated NVR transport.

use std::collections::BTreeSet;
use std::sync::Arc;

use url::Url;

use crate::configuration::NvrConfig;
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::nvr::authentication::{NvrRequest, NvrTransport};

// ── PlaybackUrlPolicy ─────────────────────────────────────────────────────

/// Policy for resolving and authorizing playback URIs.
///
/// When rebasing is enabled, the policy replaces scheme, host, and port
/// with the configured NVR origin while preserving path and query.
/// When rebasing is disabled, the policy only allows same-origin hosts
/// or hosts explicitly present in the allowlist.
#[derive(Debug, Clone)]
pub struct PlaybackUrlPolicy {
    /// The configured NVR origin URL.
    configured_origin: Url,
    /// Whether to rebase playback URLs.
    rebase: bool,
    /// Normalized (lowercased) allowlist of permitted playback hosts.
    allowed_hosts: BTreeSet<String>,
}

impl PlaybackUrlPolicy {
    /// Construct a playback URL policy from NVR configuration.
    pub fn from_config(config: &NvrConfig) -> Self {
        let origin = build_origin_url(&config.scheme, &config.host, config.port);
        // Normalize all allowlist entries: lowercase + bracket IPv6 for
        // canonical comparison with url::Url::host_str output.
        let allowed_hosts: BTreeSet<String> = config
            .download
            .playback_host_allowlist
            .iter()
            .map(|h| normalize_allowlist_host(h))
            .collect();
        Self {
            configured_origin: origin,
            rebase: config.download.rebase_playback_urls,
            allowed_hosts,
        }
    }

    /// Resolve a playback URI string into an authorized URL.
    ///
    /// When rebasing is enabled, the returned URL uses the configured NVR
    /// origin with the original path and query preserved.
    ///
    /// When rebasing is disabled, the original URL is returned only if its
    /// host matches the configured origin or is in the allowlist.
    ///
    /// Returns an error for malformed URLs, embedded credentials,
    /// unsupported schemes, or unauthorized hosts.
    pub fn resolve(&self, playback_uri: &str) -> AppResult<Url> {
        let url = parse_playback_uri(playback_uri)?;

        if self.rebase {
            self.rebase_url(&url)
        } else {
            self.authorize_url(&url)
        }
    }

    /// Rebase a URL to the configured NVR origin.
    fn rebase_url(&self, url: &Url) -> AppResult<Url> {
        let mut rebase = self.configured_origin.clone();

        // Preserve the path.
        rebase.set_path(url.path());

        // Preserve the query.
        rebase.set_query(url.query());

        // Preserve the fragment (though it should be stripped by the parser).
        if let Some(fragment) = url.fragment() {
            rebase.set_fragment(Some(fragment));
        }

        Ok(rebase)
    }

    /// Authorize a URL when rebasing is disabled.
    fn authorize_url(&self, url: &Url) -> AppResult<Url> {
        // Reject non-HTTP(S) schemes.
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(AppError::new(
                ErrorCategory::Protocol,
                "playback_url_authorize",
                format!("unsupported scheme \"{}\" for playback URL", scheme),
            ));
        }

        // Reject embedded credentials.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::new(
                ErrorCategory::Authorization,
                "playback_url_authorize",
                "playback URL must not contain embedded credentials",
            ));
        }

        // Get the host for comparison.
        let host = url.host_str().ok_or_else(|| {
            AppError::new(
                ErrorCategory::Protocol,
                "playback_url_authorize",
                "playback URL has no host",
            )
        })?;

        // Normalize the URL host for comparison: lowercased, with IPv6
        // brackets preserved (url::Url::host_str already brackets IPv6).
        let host_normalized = normalize_allowlist_host(host);

        // Check same-origin.
        let origin_host = normalize_allowlist_host(self.configured_origin.host_str().unwrap_or(""));
        let origin_port = self.configured_origin.port().unwrap_or_else(|| {
            if self.configured_origin.scheme() == "https" {
                443
            } else {
                80
            }
        });
        let url_port = url
            .port()
            .unwrap_or_else(|| if url.scheme() == "https" { 443 } else { 80 });

        // For default ports, the URL may not have an explicit port, so
        // normalize to the effective port for comparison.
        let url_port = if url_port == 0 { origin_port } else { url_port };

        let same_origin = host_normalized == origin_host
            && url_port == origin_port
            && url.scheme() == self.configured_origin.scheme();

        if same_origin {
            return Ok(url.clone());
        }

        // Check allowlist.
        if self.allowed_hosts.contains(&host_normalized) {
            return Ok(url.clone());
        }

        Err(AppError::new(
            ErrorCategory::Authorization,
            "playback_url_authorize",
            format!(
                "playback host \"{}\" is not same-origin and not in the allowlist",
                safe_host_display(host)
            ),
        ))
    }
}

/// Parse a playback URI string into a Url.
///
/// Rejects embedded credentials, unsupported schemes, missing hosts, and
/// fragments (fragments are never sent to the server and must be rejected
/// to avoid ambiguity).
fn parse_playback_uri(uri: &str) -> AppResult<Url> {
    let url = Url::parse(uri).map_err(|_| {
        AppError::new(
            ErrorCategory::Protocol,
            "parse_playback_uri",
            "playback URI is not a valid URL",
        )
    })?;

    // Reject fragments — they are never transmitted to the server and
    // their presence usually indicates a malformed or tampered URI.
    if url.fragment().is_some() {
        return Err(AppError::new(
            ErrorCategory::Protocol,
            "parse_playback_uri",
            "playback URI must not contain a fragment",
        ));
    }

    // Reject non-HTTP(S) schemes.
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(AppError::new(
            ErrorCategory::Protocol,
            "parse_playback_uri",
            format!("unsupported scheme \"{}\" for playback URI", scheme),
        ));
    }

    // Reject embedded credentials.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::new(
            ErrorCategory::Authorization,
            "parse_playback_uri",
            "playback URI must not contain embedded credentials",
        ));
    }

    // Reject missing host.
    if url.host_str().is_none() {
        return Err(AppError::new(
            ErrorCategory::Protocol,
            "parse_playback_uri",
            "playback URI has no host",
        ));
    }

    Ok(url)
}

/// Build the configured NVR origin URL.
fn build_origin_url(scheme: &str, host: &str, port: u16) -> Url {
    // Wrap IPv6 literals in brackets.
    let host_with_brackets = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };

    let url_str = format!("{scheme}://{host_with_brackets}:{port}");
    // This should never fail because the host was validated earlier.
    Url::parse(&url_str).unwrap_or_else(|_| {
        // Fallback to a safe placeholder.
        Url::parse("http://localhost:80").unwrap()
    })
}

/// Normalize an allowlist host entry for comparison with `url::Url::host_str`.
///
/// - Lowercases the entry.
/// - Wraps bare IPv6 literals in brackets so they match the bracketed
///   representation returned by `url::Url::host_str`.
fn normalize_allowlist_host(entry: &str) -> String {
    let lower = entry.to_lowercase();
    // Bare IPv6: contains colons but is not already bracketed.
    if lower.contains(':') && !lower.starts_with('[') {
        format!("[{lower}]")
    } else {
        lower
    }
}

/// Display a host safely for diagnostics, never including credentials.
fn safe_host_display(host: &str) -> String {
    host.to_string()
}

// ── ImageDownloadClient ───────────────────────────────────────────────────

/// Client for fetching authenticated playback responses.
///
/// Holds a shared NVR transport and a playback URL policy.
pub struct ImageDownloadClient {
    /// The authenticated NVR transport.
    pub transport: Arc<NvrTransport>,
    /// The playback URL resolution policy.
    pub policy: PlaybackUrlPolicy,
}

impl ImageDownloadClient {
    /// Construct an image download client from NVR configuration.
    pub fn from_config(transport: Arc<NvrTransport>, config: &NvrConfig) -> Self {
        Self {
            transport,
            policy: PlaybackUrlPolicy::from_config(config),
        }
    }

    /// Fetch a playback response for the given URI.
    ///
    /// Resolves the URI through the playback policy, constructs an
    /// authenticated NvrRequest, and sends it through the transport
    /// using the playback-only method that permits allowlisted hosts.
    pub async fn fetch(&self, playback_uri: &str) -> AppResult<reqwest::Response> {
        let resolved_url = self.policy.resolve(playback_uri)?;
        let request = NvrRequest::get(resolved_url);
        self.transport
            .execute_playback(request, &self.policy.allowed_hosts)
            .await
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{NvrConfig, NvrDownloadConfig, NvrSearchConfig};
    use crate::domain::Timestamp;
    use crate::error::ErrorCategory;

    fn make_config(host: &str, port: u16, rebase: bool, allowlist: Vec<&str>) -> NvrConfig {
        NvrConfig {
            scheme: "http".to_string(),
            host: host.to_string(),
            port,
            username: "admin".to_string(),
            password: Some(crate::configuration::Secret::new("pass".to_string())),
            start_at: Timestamp::new(chrono::Utc::now()),
            request_timeout_seconds: 5,
            connect_timeout_seconds: 2,
            allow_invalid_tls_certificates: false,
            search: NvrSearchConfig {
                window_minutes: 60,
                max_results: 50,
                poll_interval_seconds: 60,
                poll_overlap_seconds: 120,
                camera_refresh_interval_seconds: 3600,
                settlement_delay_seconds: 10,
                capture_time_window: None,
            },
            download: NvrDownloadConfig {
                retry_limit: 10,
                retry_initial_delay_seconds: 5,
                retry_max_delay_seconds: 300,
                maximum_image_size_bytes: 25_000_000,
                verify_jpeg: true,
                rebase_playback_urls: rebase,
                concurrency: 2,
                playback_host_allowlist: allowlist.into_iter().map(String::from).collect(),
            },
        }
    }

    // ── PlaybackUrlPolicy tests ─────────────────────────────────────────

    #[test]
    fn rebase_replaces_origin_preserves_path_query() {
        let config = make_config("nvr.local", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy
            .resolve("http://different-host:9090/picture/1?starttime=abc")
            .unwrap();

        assert_eq!(result.scheme(), "http");
        assert_eq!(result.host_str(), Some("nvr.local"));
        assert_eq!(result.port(), Some(8080));
        assert_eq!(result.path(), "/picture/1");
        assert_eq!(result.query(), Some("starttime=abc"));
    }

    #[test]
    fn rebase_handles_ipv6_origin() {
        let config = make_config("::1", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://other/pic.jpg").unwrap();
        assert_eq!(result.host_str(), Some("[::1]"));
        assert_eq!(result.port(), Some(8080));
    }

    #[test]
    fn rebase_with_default_port() {
        let config = make_config("nvr.local", 80, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://other:443/path").unwrap();
        assert_eq!(result.host_str(), Some("nvr.local"));
        // URL port() returns None for default ports.
        assert_eq!(result.port(), None);
        assert_eq!(result.path(), "/path");
    }

    #[test]
    fn rebase_preserves_empty_path() {
        let config = make_config("nvr.local", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://other").unwrap();
        // URL crate normalizes empty paths to "/".
        assert_eq!(result.path(), "/");
    }

    #[test]
    fn rebase_rejects_fragment() {
        let config = make_config("nvr.local", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        // Fragments are rejected at parse time since they are never
        // transmitted to the server and indicate a malformed URI.
        let result = policy.resolve("http://other/path#section");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Protocol);
    }

    #[test]
    fn authorize_same_origin_allowed() {
        let config = make_config("nvr.local", 8080, false, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://nvr.local:8080/picture/1").unwrap();
        assert_eq!(result.host_str(), Some("nvr.local"));
    }

    #[test]
    fn authorize_different_host_rejected() {
        let config = make_config("nvr.local", 8080, false, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://other.local:8080/picture/1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Authorization);
    }

    #[test]
    fn authorize_allowlisted_host_allowed() {
        let config = make_config("nvr.local", 8080, false, vec!["cdn.example.com"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://cdn.example.com/picture/1").unwrap();
        assert_eq!(result.host_str(), Some("cdn.example.com"));
    }

    #[test]
    fn authorize_allowlist_case_insensitive() {
        let config = make_config("nvr.local", 8080, false, vec!["CDN.EXAMPLE.COM"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://cdn.example.com/picture/1").unwrap();
        assert_eq!(result.host_str(), Some("cdn.example.com"));
    }

    #[test]
    fn authorize_non_allowlisted_host_rejected() {
        let config = make_config("nvr.local", 8080, false, vec!["cdn.example.com"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://other.example.com/picture/1");
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_embedded_credentials() {
        let config = make_config("nvr.local", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://user:pass@nvr.local/picture/1");
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_unsupported_scheme() {
        let config = make_config("nvr.local", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("ftp://nvr.local/picture/1");
        assert!(result.is_err());
    }

    #[test]
    fn parse_rejects_malformed_uri() {
        let config = make_config("nvr.local", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("not-a-url");
        assert!(result.is_err());
    }

    #[test]
    fn rebase_path_query_preservation() {
        let config = make_config("192.168.1.50", 8080, true, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy
            .resolve("http://other-host:9999/picture/Streaming/tracks/103/?starttime=2026-07-11T02%3A00%3A00Z")
            .unwrap();

        assert_eq!(result.host_str(), Some("192.168.1.50"));
        assert_eq!(result.port(), Some(8080));
        assert_eq!(result.path(), "/picture/Streaming/tracks/103/");
        assert_eq!(result.query(), Some("starttime=2026-07-11T02%3A00%3A00Z"));
    }

    #[test]
    fn authorize_default_port_match() {
        let config = make_config("nvr.local", 80, false, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        // Same host, default port — URL port() returns None for default ports.
        let result = policy.resolve("http://nvr.local/picture/1").unwrap();
        assert_eq!(result.host_str(), Some("nvr.local"));
        assert_eq!(result.port(), None);
    }

    #[test]
    fn authorize_different_port_rejected() {
        let config = make_config("nvr.local", 8080, false, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://nvr.local:9090/picture/1");
        assert!(result.is_err());
    }

    // ── IPv6 allowlist normalization tests ──────────────────────────────

    #[test]
    fn ipv6_allowlist_entry_matches_bracketed_url_host() {
        // Bare IPv6 in allowlist should match bracketed IPv6 from URL.
        let config = make_config("192.168.1.50", 8080, false, vec!["::1"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        // The URL host_str returns bracketed IPv6.
        let result = policy.resolve("http://[::1]:8080/picture/1");
        assert!(
            result.is_ok(),
            "bare IPv6 allowlist should match bracketed URL host"
        );
    }

    #[test]
    fn ipv6_allowlist_case_insensitive() {
        let config = make_config("192.168.1.50", 8080, false, vec!["::1"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://[::1]:8080/picture/1");
        assert!(result.is_ok());
    }

    #[test]
    fn ipv6_same_origin_with_bracketed_url() {
        // When the NVR origin is an IPv6 address, same-origin should work.
        let config = make_config("::1", 8080, false, vec![]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://[::1]:8080/picture/1");
        assert!(result.is_ok(), "same-origin IPv6 should be allowed");
    }

    #[test]
    fn non_allowlisted_ipv6_rejected() {
        let config = make_config("192.168.1.50", 8080, false, vec!["::1"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        let result = policy.resolve("http://[2001:db8::1]:8080/picture/1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Authorization);
    }

    #[test]
    fn normalize_allowlist_host_wraps_bare_ipv6() {
        use super::normalize_allowlist_host;

        assert_eq!(normalize_allowlist_host("::1"), "[::1]");
        assert_eq!(normalize_allowlist_host("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(normalize_allowlist_host("example.com"), "example.com");
        assert_eq!(normalize_allowlist_host("192.168.1.1"), "192.168.1.1");
        assert_eq!(normalize_allowlist_host("[::1]"), "[::1]");
    }

    #[test]
    fn ipv6_allowlist_canonical_form_matches_url_host() {
        // When the config module canonicalizes IPv6 (e.g., 2001:0db8::1 →
        // 2001:db8::1), the stored allowlist entry should match the
        // bracketed canonical form returned by url::Url::host_str.
        let config = make_config("192.168.1.50", 8080, false, vec!["2001:db8::1"]);
        let policy = PlaybackUrlPolicy::from_config(&config);

        // The URL host_str for [2001:db8::1] matches the canonical allowlist.
        let result = policy.resolve("http://[2001:db8::1]:8080/picture/1");
        assert!(
            result.is_ok(),
            "canonical IPv6 allowlist should match canonical URL host"
        );
    }
}
