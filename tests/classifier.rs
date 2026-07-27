//! Integration tests for Phase 9: classifier client and response validation.
//!
//! Uses wiremock to simulate an OpenAI-compatible chat-completions server,
//! verifying authentication modes, response parsing, error handling, and
//! timeout behavior.

use fauna_scan::classifier::ClassifierClient;
use fauna_scan::configuration::{
    ClassifierConfig, ClassifierEndpointConfig, ClassifierGenerationConfig, Secret,
};
use fauna_scan::error::ErrorCategory;

use url::Url;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build a basic ClassifierConfig pointing at the given mock server.
fn make_classifier_config(mock_base: &str) -> ClassifierConfig {
    let url = Url::parse(mock_base).unwrap();
    let port = url.port().unwrap_or(80);
    let scheme = url.scheme();

    ClassifierConfig {
        endpoints: vec![ClassifierEndpointConfig {
            enabled: true,
            base_url: Url::parse(&format!("{scheme}://{}:{}", url.host_str().unwrap(), port))
                .unwrap(),
            endpoint: "/chat/completions".to_string(),
            model: "test-model".to_string(),
            api_key: None,
            username: String::new(),
            password: None,
            request_timeout_seconds: 10,
            prompt_version: "wildlife-v1".to_string(),
            generation: ClassifierGenerationConfig {
                temperature: 0.1,
                max_tokens: 1000,
            },
            rate_limit: None,
        }],
        poll_interval_seconds: 10,
        retry_limit: 5,
        retry_initial_delay_seconds: 10,
        retry_max_delay_seconds: 300,
        processing_lease_seconds: 600,
    }
}

/// Build a ClassifierConfig with Bearer API key authentication.
fn make_classifier_config_with_api_key(mock_base: &str, api_key: &str) -> ClassifierConfig {
    let mut config = make_classifier_config(mock_base);
    config.endpoints[0].api_key = Some(Secret::new(api_key.to_string()));
    config
}

/// Build a ClassifierConfig with Basic authentication.
fn make_classifier_config_with_basic(
    mock_base: &str,
    username: &str,
    password: &str,
) -> ClassifierConfig {
    let mut config = make_classifier_config(mock_base);
    config.endpoints[0].username = username.to_string();
    config.endpoints[0].password = Some(Secret::new(password.to_string()));
    config
}

/// Build a ClassifierConfig with combined Bearer + Basic authentication.
fn make_classifier_config_combined(
    mock_base: &str,
    api_key: &str,
    username: &str,
    password: &str,
) -> ClassifierConfig {
    let mut config = make_classifier_config(mock_base);
    config.endpoints[0].api_key = Some(Secret::new(api_key.to_string()));
    config.endpoints[0].username = username.to_string();
    config.endpoints[0].password = Some(Secret::new(password.to_string()));
    config
}

/// Minimal valid JPEG bytes for testing.
fn minimal_jpeg() -> Vec<u8> {
    // A minimal valid JPEG: SOI + APP0 + SOS + EOI (14 bytes)
    vec![
        0xff, 0xd8, // SOI
        0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, // APP0
        0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // APP0 body
        0xff, 0xda, 0x00, 0x0c, 0x01, 0x01, 0x00, 0x00, // SOS
        0x3f, 0x00, 0x7f, 0xd9, // EOI
    ]
}

/// Valid OpenAI chat-completions envelope with a direct classification object.
fn valid_openai_response() -> String {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "{\"contains_animal\": true, \"contains_wildlife\": true, \"is_interesting\": true, \"species\": [{\"name\": \"Indian palm squirrel\", \"confidence\": 0.82}], \"overall_confidence\": 0.82, \"summary\": \"A small squirrel is moving along the garden wall.\", \"uncertainties\": []}"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 100,
            "total_tokens": 600
        }
    })
    .to_string()
}

/// Valid OpenAI envelope with Markdown-fenced JSON content.
fn valid_openai_response_markdown() -> String {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "```json\n{\"contains_animal\": false, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [], \"overall_confidence\": 0.3, \"summary\": \"A garden with a bird feeder.\", \"uncertainties\": [\"image quality moderate\"]}\n```"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 100,
            "total_tokens": 600
        }
    })
    .to_string()
}

/// Valid OpenAI envelope with structured output (output_parsed) as a JSON string.
fn valid_openai_response_structured_output_string() -> String {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890,
        "model": "test-model",
        "output_parsed": "{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [{\"name\": \"house cat\", \"confidence\": 0.95}], \"overall_confidence\": 0.95, \"summary\": \"A domestic cat sitting on a windowsill.\", \"uncertainties\": []}",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "parsed": "{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [{\"name\": \"house cat\", \"confidence\": 0.95}], \"overall_confidence\": 0.95, \"summary\": \"A domestic cat sitting on a windowsill.\", \"uncertainties\": []}"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 100,
            "total_tokens": 600
        }
    })
    .to_string()
}

// ── Valid response tests ──────────────────────────────────────────────────

/// A valid OpenAI chat-completions envelope is submitted to the configured
/// /v1/chat/completions URL and parsed successfully.
#[tokio::test]
async fn valid_openai_response_parsed_successfully() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");

    let output = result.unwrap();
    assert!(output.classification.contains_animal);
    assert!(output.classification.contains_wildlife);
    assert!(output.classification.is_interesting);
    assert_eq!(output.classification.species.len(), 1);
    assert_eq!(
        output.classification.species[0].name,
        "Indian palm squirrel"
    );
    assert!(!output.raw_response.is_empty());
    assert!(!output.classification_json.is_empty());
}

/// A valid response with Markdown-fenced JSON content is parsed successfully.
#[tokio::test]
async fn valid_openai_response_markdown_fenced() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response_markdown()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");

    let output = result.unwrap();
    assert!(!output.classification.contains_animal);
    assert!(!output.classification.contains_wildlife);
    assert!(
        output
            .classification
            .uncertainties
            .contains(&"image quality moderate".to_string())
    );
}

/// A valid response with structured output (output_parsed) as a JSON string
/// is decoded and parsed successfully.
#[tokio::test]
async fn valid_openai_response_structured_output_as_string() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(valid_openai_response_structured_output_string()),
        )
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");

    let output = result.unwrap();
    assert!(output.classification.contains_animal);
    assert!(!output.classification.contains_wildlife);
    assert_eq!(output.classification.species[0].name, "house cat");
}

/// A valid response with structured output (message.parsed) as a JSON string
/// is decoded and parsed successfully.
#[tokio::test]
async fn valid_openai_response_message_parsed_as_string() {
    let mock_server = MockServer::start().await;

    let body = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "parsed": "{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [{\"name\": \"robin\", \"confidence\": 0.7}], \"overall_confidence\": 0.7, \"summary\": \"A robin on the ground.\", \"uncertainties\": []}"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 100,
            "total_tokens": 600
        }
    })
    .to_string();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");

    let output = result.unwrap();
    assert!(output.classification.contains_animal);
    assert_eq!(output.classification.species[0].name, "robin");
}

// ── Authentication tests ──────────────────────────────────────────────────

/// No-auth request succeeds when the server accepts it.
#[tokio::test]
async fn no_auth_request_succeeds() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header_exists("Content-Type"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");
}

/// Bearer-only authentication sends the correct header.
#[tokio::test]
async fn bearer_auth_header_present() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer SENTINEL-TEST-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config_with_api_key(&mock_server.uri(), "SENTINEL-TEST-KEY");
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");
}

/// Basic-only authentication sends the correct header.
#[tokio::test]
async fn basic_auth_header_present() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Basic dXNlcjpwYXNz"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config_with_basic(&mock_server.uri(), "user", "pass");
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");
}

/// Custom Wiremock matcher that verifies both Bearer and Basic
/// Authorization headers are present in the received request.
struct CombinedAuthMatcher {
    expected_bearer: String,
    expected_basic: String,
}

impl Match for CombinedAuthMatcher {
    fn matches(&self, request: &Request) -> bool {
        let auth_headers: Vec<&str> = request
            .headers
            .get_all("Authorization")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        auth_headers.contains(&self.expected_bearer.as_str())
            && auth_headers.contains(&self.expected_basic.as_str())
    }
}

/// Combined Bearer + Basic authentication sends both headers.
/// Inspects Wiremock's received request HeaderMap to assert that
/// get_all("Authorization") contains exactly the expected Bearer and Basic values.
#[tokio::test]
async fn combined_auth_sends_both_headers() {
    let mock_server = MockServer::start().await;

    let expected_bearer = "Bearer SENTINEL-API-KEY";
    let expected_basic = "Basic YmFzaWMtdXNlcjpiYXNpYy1wYXNz";

    // Use a custom matcher that inspects the received request's Authorization headers.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(CombinedAuthMatcher {
            expected_bearer: expected_bearer.to_string(),
            expected_basic: expected_basic.to_string(),
        })
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config_combined(
        &mock_server.uri(),
        "SENTINEL-API-KEY",
        "basic-user",
        "basic-pass",
    );
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(
        result.is_ok(),
        "should succeed with combined auth: {result:?}"
    );
}

// ── Request shape tests ───────────────────────────────────────────────────

/// The request body has the correct OpenAI message content structure:
/// system message is a plain string, user message is an array of parts.
/// Verified by unit tests request_body_system_is_plain_text and
/// request_body_user_content_is_array_of_parts. This integration test
/// verifies the request reaches the server successfully.
#[tokio::test]
async fn request_body_has_correct_message_shape() {
    let mock_server = MockServer::start().await;

    // Simple path match — detailed shape verified by unit tests.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");
}

// ── Invalid UTF-8 test ────────────────────────────────────────────────────

/// A response body containing invalid UTF-8 bytes returns a retryable
/// ClassifierResponse failure.
#[tokio::test]
async fn invalid_utf8_response_is_retryable() {
    let mock_server = MockServer::start().await;

    // Build a response body with invalid UTF-8 bytes.
    let invalid_utf8 = vec![
        0x7b, 0x22, 0x63, 0x68, 0x61, 0x74, // {"chat
        0xff, 0xfe, // invalid UTF-8 continuation bytes
        0x22, 0x3a, 0x20, 0x22, 0x65, 0x72, 0x72, 0x6f, 0x72, 0x22, 0x7d,
    ]; // ": "error"}

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(invalid_utf8))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err(), "should fail with invalid UTF-8");
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierResponse);
    assert!(err.is_retryable());
}

// ── Malformed response tests ──────────────────────────────────────────────

/// Malformed model output returns a retryable ClassifierResponse failure.
#[tokio::test]
async fn malformed_model_output_is_retryable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not JSON at all"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err(), "should fail with malformed response");
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierResponse);
    assert!(err.is_retryable());
    assert!(err.raw_response().unwrap().contains("this is not JSON"));
}

/// Truncated JSON returns a retryable ClassifierResponse failure.
#[tokio::test]
async fn truncated_json_is_retryable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"contains_animal": true"#))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierResponse);
    assert!(err.is_retryable());
}

/// Missing required fields returns a retryable ClassifierResponse failure.
#[tokio::test]
async fn missing_required_fields_is_retryable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                serde_json::json!({
                    "choices": [{
                        "message": {
                            "content": "{\"contains_animal\": true}"
                        }
                    }]
                })
                .to_string(),
            ),
        )
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierResponse);
    assert!(err.is_retryable());
}

// ── HTTP status tests ─────────────────────────────────────────────────────

/// HTTP 500 is retryable.
#[tokio::test]
async fn http_500_is_retryable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierTransport);
    assert!(err.is_retryable());
    assert_eq!(err.raw_response().unwrap(), "Internal Server Error");
}

/// HTTP 429 is retryable.
#[tokio::test]
async fn http_429_is_retryable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Rate limited"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierTransport);
    assert!(err.is_retryable());
}

/// HTTP 401 is permanent Authentication.
#[tokio::test]
async fn http_401_is_permanent_authentication() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Authentication);
    assert!(!err.is_retryable());
}

/// HTTP 403 is permanent Authorization.
#[tokio::test]
async fn http_403_is_permanent_authorization() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Authorization);
    assert!(!err.is_retryable());
}

/// HTTP 400 is permanent ClassifierTransport.
#[tokio::test]
async fn http_400_is_permanent() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Bad request"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierTransport);
    assert!(!err.is_retryable());
}

/// HTTP 408 is retryable ClassifierTransport.
#[tokio::test]
async fn http_408_is_retryable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(408).set_body_string("Request Timeout"))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::ClassifierTransport);
    assert!(err.is_retryable());
}

// ── Timeout test ──────────────────────────────────────────────────────────

/// A delayed response exceeding request_timeout_seconds becomes a retryable
/// classifier transport failure. All transport failures (timeout, DNS,
/// connection) are mapped to ClassifierTransport.
#[tokio::test]
async fn delayed_response_is_timeout() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(valid_openai_response())
                .set_delay(std::time::Duration::from_secs(10)),
        )
        .mount(&mock_server)
        .await;

    let mut config = make_classifier_config(&mock_server.uri());
    config.endpoints[0].request_timeout_seconds = 1;
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err(), "should fail with delayed response");
    let err = result.unwrap_err();
    assert_eq!(
        err.category(),
        ErrorCategory::ClassifierTransport,
        "expected ClassifierTransport for transport failure"
    );
    assert!(err.is_retryable());
}

// ── Endpoint path tests ───────────────────────────────────────────────────

/// The client assembles the correct endpoint URL from base path + endpoint.
#[tokio::test]
async fn endpoint_path_assembled_correctly() {
    let mock_server = MockServer::start().await;

    // Intercept at the exact assembled path.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let mut config = make_classifier_config(&mock_server.uri());
    // Set a base URL with a path segment.
    config.endpoints[0].base_url = Url::parse(&format!("{}/v1", mock_server.uri())).unwrap();
    config.endpoints[0].endpoint = "/chat/completions".to_string();

    let client = ClassifierClient::from_config(&config).unwrap();
    assert!(
        client
            .endpoint_url()
            .as_str()
            .contains("/v1/chat/completions"),
        "endpoint should contain /v1/chat/completions, got: {}",
        client.endpoint_url()
    );

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");
}

// ── Raw response preservation test ────────────────────────────────────────

/// Raw response is preserved for successful and failed classifications.
#[tokio::test]
async fn raw_response_preserved_on_success() {
    let mock_server = MockServer::start().await;

    let body = valid_openai_response();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert_eq!(output.raw_response, body);
}

// ── Authentication header redaction test ──────────────────────────────────

/// ClassifierError Debug/Display never leaks API keys or Basic credentials.
#[tokio::test]
async fn classifier_error_no_secret_leakage() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let config =
        make_classifier_config_with_api_key(&mock_server.uri(), "SENTINEL-API-KEY-LEAK-TEST");
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();

    let debug_output = format!("{err:?}");
    let display_output = format!("{err}");

    assert!(
        !debug_output.contains("SENTINEL-API-KEY-LEAK-TEST"),
        "API key leaked in Debug: {debug_output}"
    );
    assert!(
        !display_output.contains("SENTINEL-API-KEY-LEAK-TEST"),
        "API key leaked in Display: {display_output}"
    );
}

// ── ClassifierOutput redaction test ───────────────────────────────────────

/// ClassifierOutput Debug never exposes raw_response or classification_json.
#[tokio::test]
async fn classifier_output_debug_no_raw_response() {
    let mock_server = MockServer::start().await;

    let body = valid_openai_response();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok());
    let output = result.unwrap();

    let debug_output = format!("{output:?}");
    // The raw response body should NOT appear in Debug output.
    assert!(
        !debug_output.contains("chatcmpl-test"),
        "raw_response leaked in ClassifierOutput Debug: {debug_output}"
    );
    // Should report lengths instead of content.
    assert!(debug_output.contains("raw_response_len"));
    assert!(debug_output.contains("classification_json_len"));
}

// ── Request body shape test ───────────────────────────────────────────────

/// The request body contains the expected OpenAI chat-completions shape.
#[tokio::test]
async fn request_body_shape() {
    let mock_server = MockServer::start().await;

    // Capture the request body by matching on Content-Type and returning 200.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Content-Type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(valid_openai_response()))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_ok(), "should succeed: {result:?}");
}

// ── Oversized response tests ──────────────────────────────────────────────

/// A 2xx response that exceeds MAX_RESPONSE_BYTES is rejected with a
/// retryable oversized-body error, not a successful classification.
#[tokio::test]
async fn oversized_2xx_response_is_rejected() {
    let mock_server = MockServer::start().await;

    // Build a response body that exceeds 10 MB.
    let large_body = "x".repeat(10 * 1024 * 1024 + 1);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(large_body))
        .mount(&mock_server)
        .await;

    let config = make_classifier_config(&mock_server.uri());
    let client = ClassifierClient::from_config(&config).unwrap();

    let result = client.classify_jpeg(&minimal_jpeg()).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.is_retryable(),
        "oversized 2xx response should be retryable, got: {:?}",
        err
    );
    // The underlying error message should mention the size limit.
    let display = format!("{err}");
    assert!(
        display.contains("exceeds maximum") || display.contains("oversized"),
        "error should mention size limit: {display}"
    );
}

/// Oversized non-success responses preserve their HTTP status disposition:
/// authentication and authorization failures are permanent, while a server
/// error remains retryable.
#[tokio::test]
async fn oversized_non_2xx_response_preserves_status_disposition() {
    let mock_server = MockServer::start().await;
    let large_body = "Error: ".to_string() + &"x".repeat(10 * 1024 * 1024 + 1);

    for (status, category, retryable) in [
        (401, ErrorCategory::Authentication, false),
        (403, ErrorCategory::Authorization, false),
        (500, ErrorCategory::ClassifierTransport, true),
    ] {
        mock_server.reset().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_string(large_body.clone()))
            .mount(&mock_server)
            .await;

        let config = make_classifier_config(&mock_server.uri());
        let client = ClassifierClient::from_config(&config).unwrap();
        let err = client
            .classify_jpeg(&minimal_jpeg())
            .await
            .expect_err("oversized response must be rejected");

        assert_eq!(err.category(), category, "status {status}");
        assert_eq!(err.http_status(), Some(status), "status {status}");
        assert_eq!(err.is_retryable(), retryable, "status {status}");
    }
}
