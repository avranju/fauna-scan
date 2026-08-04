//! Integration tests for Phase 4: shared HTTP infrastructure and
//! Hikvision Digest-authenticated NVR transport.
//!
//! Uses wiremock for a reliable local mock server to simulate Digest
//! challenges, timeouts, HTTP status codes, and redirects.
//!
//! Some tests (TLS, connection reuse) use a custom tokio HTTP server
//! that can track TCP connections and serve self-signed TLS.

use fauna_scan::configuration::{NvrConfig, NvrDownloadConfig, NvrSearchConfig};
use fauna_scan::domain::Timestamp;
use fauna_scan::error::{AppError, AppResult, ErrorCategory};
use fauna_scan::http::{HttpClientConfig, Origin, SharedHttpClient};
use fauna_scan::nvr::{NvrRequest, NvrTransport};

use md5::{Digest, Md5};
use reqwest::header::{HeaderName, HeaderValue};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DIGEST_CHALLENGE: &str = "Digest realm=\"Hikvision\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", algorithm=MD5, qop=\"auth\"";

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build an `NvrConfig` pointing at the given mock server URL.
fn make_nvr_config(mock_base: &str) -> NvrConfig {
    let url = Url::parse(mock_base).unwrap();
    let port = url.port().unwrap_or(80);
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("127.0.0.1").to_string();

    NvrConfig {
        scheme: scheme.to_string(),
        host,
        port,
        username: "admin".to_string(),
        password: Some(fauna_scan::configuration::Secret::new(
            "correct-pass".to_string(),
        )),
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
            rebase_playback_urls: true,
            concurrency: 2,
            playback_host_allowlist: vec![],
        },
    }
}

/// Build an `NvrTransport` from an `NvrConfig`.
async fn build_transport(config: &NvrConfig) -> AppResult<NvrTransport> {
    NvrTransport::from_config(config)
}

// ── TLS provider setup ────────────────────────────────────────────────────

/// Ensure the aws-lc-rs CryptoProvider is installed for rustls.
///
/// This is required because rustls 0.23 does not auto-install a provider
/// and tests that construct TLS acceptors must have one available.
fn crypto_provider_install() {
    use rustls::crypto::CryptoProvider;
    use std::sync::Once;

    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // aws-lc-rs is enabled via Cargo.toml features.
        CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
            .expect("failed to install aws-lc-rs CryptoProvider");
    });
}

// ── Wiremock compatibility tests ──────────────────────────────────────────

/// Verify wiremock sends WWW-Authenticate header.
#[tokio::test]
async fn wiremock_sends_auth_header() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/test"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .mount(&mock_server)
        .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/test", mock_server.uri()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    let auth = resp.headers().get("WWW-Authenticate");
    assert!(
        auth.is_some(),
        "wiremock should send WWW-Authenticate header, got: {:?}",
        resp.headers()
    );
}

/// Verify that a reqwest client with redirects disabled still gets headers.
#[tokio::test]
async fn reqwest_with_no_redirects_gets_headers() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/test"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .mount(&mock_server)
        .await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls()
        .build()
        .unwrap();

    let resp = client
        .get(format!("{}/test", mock_server.uri()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    let auth = resp.headers().get("WWW-Authenticate");
    assert!(
        auth.is_some(),
        "reqwest with no redirects should get WWW-Authenticate, got: {:?}",
        resp.headers()
    );
}

/// Digest challenge is answered correctly and authenticated request succeeds.
#[tokio::test]
async fn digest_challenge_succeeds() {
    let mock_server = MockServer::start().await;

    // First request: return 401 with Digest challenge.
    // Second request: return 200 OK.
    let first_mock = Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        );
    first_mock.up_to_n_times(1).mount(&mock_server).await;

    let second_mock = Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK"));
    second_mock.up_to_n_times(1).mount(&mock_server).await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_ok(), "Digest auth should succeed: {result:?}");
}

/// Bad credentials result in Authentication error.
#[tokio::test]
async fn bad_digest_credentials_fail() {
    let mock_server = MockServer::start().await;

    // Always return 401 — the replayed request will also get 401
    let mock = Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        );
    mock.up_to_n_times(2).mount(&mock_server).await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        password: Some(fauna_scan::configuration::Secret::new(
            "wrong-pass".to_string(),
        )),
        ..config
    };

    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_err(), "should fail with wrong password");
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authentication);
    assert!(
        !err.message.contains("wrong-pass"),
        "password leaked in error"
    );
}

/// Timeout errors are categorized as Timeout.
#[tokio::test]
async fn timeout_is_categorized() {
    // Use a non-routable address to force a timeout.
    let config = make_nvr_config("http://127.0.0.1:1");
    let config = NvrConfig {
        request_timeout_seconds: 1,
        connect_timeout_seconds: 1,
        ..config
    };

    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_err(), "should fail with unreachable server");
    let err = result.unwrap_err();
    assert!(
        matches!(
            err.category,
            ErrorCategory::Timeout | ErrorCategory::Network
        ),
        "expected Timeout or Network, got {:?}",
        err.category
    );
}

/// HTTP 403 is mapped to Authorization.
#[tokio::test]
async fn http_403_is_authorization() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authorization);
    assert_eq!(err.http_status(), Some(403));
}

/// Redirects are not followed — returns Protocol error.
#[tokio::test]
async fn redirect_is_refused() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("Location", "http://127.0.0.1:9999/"),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Protocol);
}

/// Cross-origin target is refused before any request is sent.
#[tokio::test]
async fn cross_origin_is_refused() {
    let config = make_nvr_config("http://127.0.0.1:18080");
    let transport = build_transport(&config).await.unwrap();

    // Build a request targeting a different origin.
    let url = Url::parse("http://10.0.0.1:8080/ISAPI/Streaming/channels").unwrap();
    let request = NvrRequest::get(url);
    let result = transport.execute(request).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authorization);
}

/// Embedded credentials in target URL are rejected.
#[tokio::test]
async fn embedded_credentials_in_url_rejected() {
    let config = make_nvr_config("http://127.0.0.1:18080");
    let transport = build_transport(&config).await.unwrap();

    let url = Url::parse("http://user:SENTINEL@127.0.0.1:18080/ISAPI/Streaming/channels").unwrap();
    let request = NvrRequest::get(url);
    let result = transport.execute(request).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authorization);
    assert!(
        !err.message.contains("SENTINEL"),
        "credential leaked in error: {}",
        err.message
    );
}

/// Origin comparison normalizes host casing and default ports.
#[tokio::test]
async fn origin_normalization() {
    let _http = SharedHttpClient::build(HttpClientConfig::from_seconds(5, 10, false)).unwrap();
    let origin = Origin::from_url(&Url::parse("http://NVR.EXAMPLE.INVALID:80/").unwrap()).unwrap();

    // Same host with different casing — should match
    let url1 = Url::parse("http://nvr.example.invalid/ISAPI/Streaming/channels").unwrap();
    assert!(origin.matches(&url1));

    // Default port 80 — should match explicit 80
    let url2 = Url::parse("http://NVR.EXAMPLE.INVALID:80/ISAPI/Streaming/channels").unwrap();
    assert!(origin.matches(&url2));

    // Different port — should not match
    let url3 = Url::parse("http://nvr.example.invalid:8080/ISAPI/Streaming/channels").unwrap();
    assert!(!origin.matches(&url3));

    // HTTPS — should not match
    let url4 = Url::parse("https://nvr.example.invalid/ISAPI/Streaming/channels").unwrap();
    assert!(!origin.matches(&url4));
}

/// URL redaction removes credentials and query values.
#[tokio::test]
async fn url_redaction() {
    use fauna_scan::http::redact_url;

    let url = Url::parse("http://user:pass@example.com:8080/path?key=secret&foo=bar").unwrap();
    let redacted = redact_url(&url);
    assert!(!redacted.contains("user"));
    assert!(!redacted.contains("pass"));
    assert!(!redacted.contains("key"));
    assert!(!redacted.contains("secret"));
    assert!(!redacted.contains("foo"));
    assert!(redacted.contains("example.com"));
    assert!(redacted.contains("/path"));
}

/// AppError HTTP status context is retained and displayed.
#[tokio::test]
async fn http_status_context() {
    let err = AppError::new(ErrorCategory::Protocol, "test", "bad response").with_http_status(404);
    assert!(err.has_http_status());
    assert_eq!(err.http_status(), Some(404));

    let msg = format!("{err}");
    assert!(msg.contains("404"));
    assert!(msg.contains("Protocol"));
    assert!(msg.contains("test"));
    assert!(msg.contains("bad response"));

    // Verify category and operation are preserved
    assert_eq!(err.category, ErrorCategory::Protocol);
    assert_eq!(err.operation, "test");
}

/// NvrRequest Debug output is redacted.
#[tokio::test]
async fn nvr_request_debug_redacted() {
    let url = Url::parse("http://127.0.0.1:18080/ISAPI/Streaming/channels").unwrap();
    let request = NvrRequest::get(url)
        .with_header(
            HeaderName::from_bytes(b"Authorization").unwrap(),
            HeaderValue::from_str("Bearer SENTINEL-TOKEN").unwrap(),
        )
        .with_header(
            HeaderName::from_bytes(b"X-Secret").unwrap(),
            HeaderValue::from_str("base64like-ABCDEFGHIJKLMNOP").unwrap(),
        );

    let debug_output = format!("{request:?}");
    assert!(
        !debug_output.contains("SENTINEL-TOKEN"),
        "token leaked in Debug: {debug_output}"
    );
    assert!(
        !debug_output.contains("base64like"),
        "header value leaked in Debug: {debug_output}"
    );
}

/// POST request with body is replayable.
#[tokio::test]
async fn post_request_with_body() {
    let mock_server = MockServer::start().await;

    // First request: 401 challenge
    let m1 = Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        );
    m1.up_to_n_times(1).mount(&mock_server).await;

    // Second request (replay): 200 OK
    let m2 = Mock::given(method("POST"))
        .and(path("/ISAPI/ContentMgmt/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK"));
    m2.up_to_n_times(1).mount(&mock_server).await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let body = b"<?xml version=\"1.0\"?><test/>".to_vec();
    let result = transport.post("/ISAPI/ContentMgmt/search", body).await;
    assert!(result.is_ok(), "POST with body should succeed: {result:?}");
}

/// Sentinel values in errors and Debug output are never leaked.
#[tokio::test]
async fn no_secret_leakage_in_errors() {
    let config = make_nvr_config("http://127.0.0.1:1");
    let transport = build_transport(&config).await.unwrap();

    let result = transport.get("/ISAPI/Streaming/channels").await;
    let err = result.unwrap_err();
    let display = format!("{err}");
    let debug = format!("{err:?}");

    // Password should not appear in Display or Debug
    assert!(
        !display.contains("correct-pass"),
        "password leaked in Display: {display}"
    );
    assert!(
        !debug.contains("correct-pass"),
        "password leaked in Debug: {debug}"
    );
}

// ── Dynamic Digest verification tests ──────────────────────────────────────

/// Independent Digest verification: the server parses the replayed
/// Authorization header, independently computes the expected MD5 response,
/// and returns 200 only when the hash matches the configured password.
struct DigestAuthMatcher {
    method: reqwest::Method,
    path: String,
    expected_nonce: String,
    expected_realm: String,
    expected_username: String,
    expected_password: String,
}

impl DigestAuthMatcher {
    fn new(
        method: reqwest::Method,
        path: impl Into<String>,
        expected_nonce: &str,
        expected_realm: &str,
        expected_username: &str,
        expected_password: &str,
    ) -> Self {
        Self {
            method,
            path: path.into(),
            expected_nonce: expected_nonce.to_string(),
            expected_realm: expected_realm.to_string(),
            expected_username: expected_username.to_string(),
            expected_password: expected_password.to_string(),
        }
    }
}

impl wiremock::Match for DigestAuthMatcher {
    fn matches(&self, request: &wiremock::Request) -> bool {
        // Check method
        if request.method != self.method {
            return false;
        }

        // Check path
        if request.url.path() != self.path {
            return false;
        }

        // Check Authorization header exists and is Digest
        let auth_header = match request.headers.get("Authorization") {
            Some(v) => v.to_str().unwrap_or(""),
            None => return false,
        };
        if !auth_header.starts_with("Digest ") {
            return false;
        }

        // Parse Digest parameters
        let params = parse_digest_params(auth_header);
        let params = match params {
            Some(p) => p,
            None => return false,
        };

        // Verify required fields
        if params.username != self.expected_username {
            return false;
        }
        if params.realm != self.expected_realm {
            return false;
        }
        if params.nonce != self.expected_nonce {
            return false;
        }
        // URI must match request path (including query)
        let request_uri = format!(
            "{}{}",
            request.url.path(),
            request
                .url
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        );
        if params.uri != request_uri {
            return false;
        }
        if params.qop != Some("auth".to_string()) {
            return false;
        }
        if params.nonce_count.is_empty() {
            return false;
        }
        if params.client_nonce.is_empty() {
            return false;
        }
        if params.response.is_empty() {
            return false;
        }

        // Independently compute the expected MD5 response
        let expected_response = compute_expected_response(
            &self.expected_username,
            &self.expected_password,
            &self.expected_realm,
            &self.expected_nonce,
            &request_uri,
            &self.method,
            &params.qop,
            &params.nonce_count,
            &params.client_nonce,
        );

        // Constant-time comparison to prevent timing attacks
        let expected_bytes = hex_decode(&expected_response).unwrap_or_default();
        let actual_bytes = hex_decode(&params.response).unwrap_or_default();
        if expected_bytes.len() != actual_bytes.len() {
            return false;
        }
        let mut result: u8 = 0;
        for (x, y) in expected_bytes.iter().zip(actual_bytes.iter()) {
            result |= x ^ y;
        }
        result == 0
    }
}

/// Parse Digest parameters from an Authorization header value.
fn parse_digest_params(auth_header: &str) -> Option<DigestParams> {
    let params_str = auth_header.strip_prefix("Digest ")?;
    let mut params = DigestParams::default();

    for part in params_str.split(',') {
        let part = part.trim();
        if let Some(eq_pos) = part.find('=') {
            let key = part[..eq_pos].trim();
            let value = part[eq_pos + 1..].trim();
            // Strip surrounding quotes
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);

            match key {
                "username" => params.username = value.to_string(),
                "realm" => params.realm = value.to_string(),
                "nonce" => params.nonce = value.to_string(),
                "uri" => params.uri = value.to_string(),
                "response" => params.response = value.to_string(),
                "qop" => {
                    if value != "auth" {
                        return None;
                    }
                    params.qop = Some("auth".to_string());
                }
                "nc" => params.nonce_count = value.to_string(),
                "cnonce" => params.client_nonce = value.to_string(),
                "opaque" => params.opaque = Some(value.to_string()),
                _ => {}
            }
        }
    }

    Some(params)
}

#[derive(Default, Clone)]
struct DigestParams {
    username: String,
    realm: String,
    nonce: String,
    uri: String,
    response: String,
    qop: Option<String>,
    nonce_count: String,
    client_nonce: String,
    opaque: Option<String>,
}

/// Independently compute the expected MD5 Digest response.
#[allow(clippy::too_many_arguments)]
fn compute_expected_response(
    username: &str,
    password: &str,
    realm: &str,
    nonce: &str,
    uri: &str,
    method: &reqwest::Method,
    qop: &Option<String>,
    nc: &str,
    cnonce: &str,
) -> String {
    // HA1 = MD5(username:realm:password)
    let ha1 = compute_md5(&format!("{username}:{realm}:{password}"));
    let ha1_hex = format_bytes_to_hex(&ha1);

    // HA2 = MD5(method:uri)
    let ha2 = compute_md5(&format!("{}:{}", method.as_str(), uri));
    let ha2_hex = format_bytes_to_hex(&ha2);

    // response = MD5(HA1:nonce:nc:cnonce:qop:HA2)
    let qop_str = qop.as_deref().unwrap_or("auth");
    let response_input = format!("{ha1_hex}:{nonce}:{nc}:{cnonce}:{qop_str}:{ha2_hex}");
    let response = compute_md5(&response_input);
    format_bytes_to_hex(&response)
}

fn compute_md5(input: &str) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.into()
}

fn format_bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Re-export hex and subtle for the matcher.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// The Digest success server independently verifies the replayed Digest
/// header: method, exact URI including query, username, realm, nonce,
/// qop, nonce count, cnonce, opaque, and independently computes the
/// expected MD5 response. Returns 200 only for a valid response.
#[tokio::test]
async fn digest_challenge_validates_replay() {
    let mock_server = MockServer::start().await;

    // First request: return 401 with Digest challenge.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request: independently verify the Digest response.
    // The matcher computes the expected MD5 response and returns 200
    // only when the independently computed hash matches.
    Mock::given(DigestAuthMatcher::new(
        reqwest::Method::GET,
        "/ISAPI/Streaming/channels",
        "dcd98b7102dd2f0e8b11d0f600bfb0c093",
        "Hikvision",
        "admin",
        "correct-pass",
    ))
    .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
    .up_to_n_times(1)
    .mount(&mock_server)
    .await;

    let config = make_nvr_config(&mock_server.uri());
    let transport = build_transport(&config).await.unwrap();

    let result = transport.get("/ISAPI/Streaming/channels?trackID=103").await;
    assert!(
        result.is_ok(),
        "Digest auth with query-bearing target should succeed: {result:?}"
    );

    // Verify the second request had a valid Digest Authorization header.
    let requests = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 2, "expected 2 requests");

    // First request should NOT have an Authorization header.
    let first_auth = requests[0].headers.get("Authorization");
    assert!(
        first_auth.is_none(),
        "first request should not have Authorization header"
    );

    // Second request should have a valid Digest Authorization header.
    let second_auth = requests[1]
        .headers
        .get("Authorization")
        .map(|v| v.to_str().unwrap_or(""))
        .expect("Authorization header missing");
    assert!(
        second_auth.starts_with("Digest "),
        "second request Authorization should be Digest: {second_auth}"
    );
    assert!(
        second_auth.contains("username=\"admin\""),
        "missing username=admin"
    );
    assert!(
        second_auth.contains("realm=\"Hikvision\""),
        "missing realm=Hikvision"
    );
    assert!(
        second_auth.contains("nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\""),
        "missing nonce"
    );
    assert!(
        second_auth.contains("uri=\"/ISAPI/Streaming/channels"),
        "missing uri"
    );
    assert!(second_auth.contains("qop=auth"), "missing qop=auth");
    assert!(second_auth.contains("nc="), "missing nc");
    assert!(second_auth.contains("cnonce="), "missing cnonce");
    assert!(second_auth.contains("response="), "missing response");
}

/// The bad-password server independently verifies the replayed Digest
/// header by computing the expected MD5 response with the server's
/// actual password ("correct-pass").  Because the transport is
/// configured with a different password ("wrong-pass"), the
/// independently computed hash will NOT match and the matcher returns
/// false, causing wiremock to fall through to a complementary matcher
/// that returns 401 — which the transport maps to Authentication.
#[tokio::test]
async fn bad_digest_credentials_validated_independently() {
    let mock_server = MockServer::start().await;

    // First request: return 401 with Digest challenge.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second request: the matcher independently verifies the Digest
    // response using the SERVER'S actual password ("correct-pass").
    // When the transport sends a digest computed with "wrong-pass",
    // the independently computed hash will NOT match and the matcher
    // returns false, so wiremock skips this mock.
    Mock::given(DigestAuthMatcher::new(
        reqwest::Method::GET,
        "/ISAPI/Streaming/channels",
        "dcd98b7102dd2f0e8b11d0f600bfb0c093",
        "Hikvision",
        "admin",
        "correct-pass", // Server's actual password for independent verification
    ))
    .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
    .up_to_n_times(1)
    .mount(&mock_server)
    .await;

    // Fallback: when the independently computed hash does NOT match
    // (because the transport used "wrong-pass"), return 401 so the
    // transport maps it to Authentication.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("")
                .insert_header("WWW-Authenticate", DIGEST_CHALLENGE),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        password: Some(fauna_scan::configuration::Secret::new(
            "wrong-pass".to_string(),
        )),
        ..config
    };

    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_err(), "should fail with wrong password");
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Authentication);
    assert!(
        !err.message.contains("wrong-pass"),
        "password leaked in error: {}",
        err.message
    );

    // Verify the second request had a valid Digest Authorization header.
    let requests = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 2, "expected 2 requests");
    let second_auth = requests[1]
        .headers
        .get("Authorization")
        .map(|v| v.to_str().unwrap_or(""))
        .expect("Authorization header missing");
    assert!(
        second_auth.starts_with("Digest "),
        "second request Authorization should be Digest: {second_auth}"
    );
    assert!(
        second_auth.contains("username=\"admin\""),
        "missing username=admin"
    );
    assert!(second_auth.contains("response="), "missing response");
}

// ── Timeout test with delayed response ─────────────────────────────────────

/// A delayed local response that must produce ErrorCategory::Timeout.
#[tokio::test]
async fn delayed_response_is_timeout() {
    let mock_server = MockServer::start().await;

    // Delay the response beyond the configured request timeout.
    Mock::given(method("GET"))
        .and(path("/ISAPI/Streaming/channels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("OK")
                .set_delay(std::time::Duration::from_secs(10)),
        )
        .mount(&mock_server)
        .await;

    let config = make_nvr_config(&mock_server.uri());
    let config = NvrConfig {
        request_timeout_seconds: 1,
        ..config
    };

    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    assert!(result.is_err(), "should fail with delayed response");
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Timeout);
}

// ── TLS verification tests ────────────────────────────────────────────────

/// Connect to a local self-signed HTTPS fixture and verify default
/// certificate rejection, then verify success only when
/// allow_invalid_tls_certificates is explicitly true.
///
/// This test installs the aws-lc-rs CryptoProvider before running.
#[tokio::test]
async fn self_signed_tls_rejected_by_default() {
    crypto_provider_install();

    use rustls::pki_types::CertificateDer;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    // Generate self-signed certificate and key using rcgen.
    let server_key = rcgen::KeyPair::generate().unwrap();
    let mut server_params = rcgen::CertificateParams::default();
    server_params.subject_alt_names = vec![rcgen::SanType::IpAddress([127, 0, 0, 1].into())];
    let server_cert: rcgen::Certificate = server_params.self_signed(&server_key).unwrap();

    // Convert rcgen key and cert to rustls types.
    let cert_bytes = server_cert.der().as_ref().to_vec();
    let key_der = server_key.serialize_der();
    let private_key = rustls::pki_types::PrivateKeyDer::try_from(key_der).unwrap();
    let certs = vec![CertificateDer::from(cert_bytes)];

    // Create rustls server config.
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, private_key)
        .unwrap();

    // Create TLS acceptor.
    let acceptor = TlsAcceptor::from(std::sync::Arc::new(config));

    // Bind TCP listener.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Spawn TLS server that serves a simple HTTP response.
    tokio::spawn(async move {
        let acceptor = acceptor;
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut tls_stream = tls_stream;
            let mut buf = vec![0u8; 4096];
            match tls_stream.read(&mut buf).await {
                Ok(_) => {
                    let response =
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
                    let _ = tls_stream.write_all(response).await;
                }
                Err(_) => continue,
            }
        }
    });

    let tls_uri = format!("https://127.0.0.1:{port}");

    // Default client (reject invalid certs) should fail.
    let config = make_nvr_config(&tls_uri);
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/test").await;
    assert!(
        result.is_err(),
        "self-signed TLS should be rejected by default"
    );
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Network);

    // With explicit opt-out, the connection should succeed.
    let config = NvrConfig {
        allow_invalid_tls_certificates: true,
        ..config
    };
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/test").await;
    assert!(
        result.is_ok(),
        "TLS with opt-out should succeed: {result:?}"
    );
}

// ── Redirect credential non-forwarding test ───────────────────────────────

/// Return a redirect to another local origin and prove it is not followed
/// and no Authorization header reaches the redirect target.
#[tokio::test]
async fn redirect_does_not_forward_credentials() {
    let redirect_server = MockServer::start().await;
    let target_server = MockServer::start().await;

    // Redirect server returns 302 to the target server.
    Mock::given(method("GET"))
        .and(path("/redirect"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", target_server.uri() + "/target"),
        )
        .mount(&redirect_server)
        .await;

    // Target server: mount a simple mock (it should never be reached).
    Mock::given(method("GET"))
        .and(path("/target"))
        .respond_with(ResponseTemplate::new(200).set_body_string("target"))
        .mount(&target_server)
        .await;

    let config = make_nvr_config(&redirect_server.uri());
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/redirect").await;

    // The transport should refuse the redirect (Protocol error).
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category, ErrorCategory::Protocol);

    // Verify the target server received no requests.
    let requests = target_server.received_requests().await.unwrap_or_default();
    assert!(
        requests.is_empty(),
        "redirect target received {} request(s) — credentials may have been forwarded",
        requests.len()
    );
}

// ── Connection reuse test ─────────────────────────────────────────────────

/// A custom HTTP server that tracks TCP connection IDs and serves Digest
/// challenges. Used to verify that NvrTransport reuses a single pooled
/// connection across multiple requests.
struct TrackingDigestServer {
    listener: std::sync::Arc<tokio::net::TcpListener>,
    connection_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    request_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TrackingDigestServer {
    async fn bind(addr: &str) -> Self {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        Self {
            listener: std::sync::Arc::new(listener),
            connection_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            request_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn local_addr(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }

    fn connection_count(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.connection_count.clone()
    }

    fn request_count(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.request_count.clone()
    }

    async fn serve(self) {
        let connection_count = self.connection_count.clone();
        let request_count = self.request_count.clone();
        let listener = self.listener.clone();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                connection_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let stream = stream;
                let request_count = request_count.clone();
                tokio::spawn(async move {
                    handle_digest_stream(stream, request_count).await;
                });
            }
        });
    }
}

async fn handle_digest_stream(
    stream: tokio::net::TcpStream,
    request_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        // Read the request line (e.g. "GET /path HTTP/1.1")
        line.clear();
        let bytes = match reader.read_line(&mut line).await {
            Ok(0) => break, // Connection closed
            Ok(n) => n,
            Err(_) => break,
        };
        if bytes == 0 {
            break;
        }

        // Skip remaining headers until blank line
        loop {
            line.clear();
            let bytes = match reader.read_line(&mut line).await {
                Ok(0) => return,
                Ok(n) => n,
                Err(_) => return,
            };
            if bytes == 0 || line.trim().is_empty() {
                break;
            }
        }

        let req_count = request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // First request on each connection: return 401 Digest challenge
        // Subsequent requests (same connection, keep-alive): return 200 OK
        let response = if req_count == 0 {
            format!(
                "HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: {}\r\n\
                 Content-Length: 0\r\n\
                 Connection: keep-alive\r\n\
                 \r\n",
                DIGEST_CHALLENGE
            )
        } else {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nOK".to_string()
        };

        let _ = write_half.write_all(response.as_bytes()).await;
        let _ = write_half.flush().await;
    }
}

/// Consume two successful responses through one NvrTransport and use server
/// connection metadata to verify the retained reqwest client reuses an
/// HTTP connection where the server permits keep-alive.
#[tokio::test]
async fn connection_reuse_consumes_bodies() {
    let server = TrackingDigestServer::bind("127.0.0.1:0").await;
    let port = server.local_addr().port();
    let connection_count = server.connection_count();
    let request_count = server.request_count();

    server.serve().await;

    let config = make_nvr_config(&format!("http://127.0.0.1:{port}"));
    let transport = build_transport(&config).await.unwrap();

    // First request — Digest auth + consume body.
    let r1 = transport.get("/ISAPI/Streaming/channels").await.unwrap();
    let body1 = r1.text().await.unwrap();
    assert_eq!(body1, "OK");

    // Second request — Digest auth + consume body.
    let r2 = transport.get("/ISAPI/Streaming/channels").await.unwrap();
    let body2 = r2.text().await.unwrap();
    assert_eq!(body2, "OK");

    // Each transport call goes through: unauthenticated 401 + authenticated 200.
    // With connection reuse (keep-alive), the TCP connection should be reused
    // across the 401 → 200 transitions, proving the pooled reqwest client
    // reuses connections.
    let total_requests = request_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        total_requests >= 3,
        "expected at least 3 requests, got {total_requests}"
    );

    // With connection reuse, the server should have received fewer TCP connections
    // than total requests, proving the pooled reqwest client reuses connections.
    let total_connections = connection_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        total_connections < total_requests,
        "expected connection reuse: {total_connections} connections for {total_requests} requests"
    );
}

/// Sentinel leakage through failures: inject sentinel values and assert none
/// appears in AppError Display or Debug output.
#[tokio::test]
async fn no_sentinel_leakage_in_failure_paths() {
    // Test 1: embedded credentials in URL → Authorization error.
    let config = make_nvr_config("http://127.0.0.1:18080");
    let transport = build_transport(&config).await.unwrap();

    let url = Url::parse("http://user:SENTINEL-URL-PASS@127.0.0.1:18080/ISAPI/Streaming/channels")
        .unwrap();
    let request = NvrRequest::get(url);
    let result = transport.execute(request).await;
    let err = result.unwrap_err();
    let display = format!("{err}");
    let debug = format!("{err:?}");
    assert!(
        !display.contains("SENTINEL"),
        "sentinel leaked in Display: {display}"
    );
    assert!(
        !debug.contains("SENTINEL"),
        "sentinel leaked in Debug: {debug}"
    );

    // Test 2: cross-origin → Authorization error.
    let url = Url::parse("http://other-host:8080/path").unwrap();
    let request = NvrRequest::get(url);
    let result = transport.execute(request).await;
    let err = result.unwrap_err();
    let display = format!("{err}");
    assert!(
        !display.contains("other-host"),
        "cross-origin host leaked in Display: {display}"
    );

    // Test 3: timeout → no password in error.
    let config = make_nvr_config("http://127.0.0.1:1");
    let transport = build_transport(&config).await.unwrap();
    let result = transport.get("/ISAPI/Streaming/channels").await;
    let err = result.unwrap_err();
    let display = format!("{err}");
    let debug = format!("{err:?}");
    assert!(
        !display.contains("correct-pass"),
        "password leaked in Display: {display}"
    );
    assert!(
        !debug.contains("correct-pass"),
        "password leaked in Debug: {debug}"
    );
}
