//! Vision classifier client (OpenAI-compatible chat-completions API).
//!
//! Implements Phase 9: a safe classifier client that submits JPEG images,
//! supports configured authentication, extracts supported response formats,
//! strictly validates wildlife classification JSON, preserves normalized and
//! raw responses, and reports retryable versus permanent failures.

use std::fmt;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::configuration::{ClassifierConfig, ClassifierEndpointConfig, Secret};
use crate::error::{AppError, AppResult, ErrorCategory};
use crate::http::{HttpClientConfig, SharedHttpClient};

// ── Public types ───────────────────────────────────────────────────────────

/// A single species or type prediction with its confidence score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeciesPrediction {
    /// Species or animal type name.
    pub name: String,
    /// Confidence between 0.0 and 1.0.
    pub confidence: f64,
}

/// Validated wildlife classification result returned by the classifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WildlifeClassification {
    /// Whether an animal is present in the image.
    pub contains_animal: bool,
    /// Whether the animal appears to be wildlife (not domestic).
    pub contains_wildlife: bool,
    /// Whether the image is interesting enough to review.
    pub is_interesting: bool,
    /// Predicted species or broad animal types with confidence.
    pub species: Vec<SpeciesPrediction>,
    /// Overall confidence score (0.0 to 1.0).
    pub overall_confidence: f64,
    /// Concise scene description.
    pub summary: String,
    /// Uncertainty or visibility issues noted by the model.
    pub uncertainties: Vec<String>,
}

/// Complete classifier output carrying typed result, normalized JSON, and
/// the full raw server response for persistence.
pub struct ClassifierOutput {
    /// Strictly validated typed classification.
    pub classification: WildlifeClassification,
    /// Compact canonical JSON preserving additional model fields.
    pub classification_json: String,
    /// The complete raw server response as returned.
    pub raw_response: String,
}

/// Tells the scanner whether a classifier failure may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDisposition {
    /// The failure is transient and may be retried.
    Retryable,
    /// The failure is permanent and should not be retried.
    Permanent,
}

/// A payload-safe classifier error carrying category, retry disposition, and
/// optional raw response without exposing secrets, Base64 data, or model text.
pub struct ClassifierError {
    /// The underlying categorized error.
    error: AppError,
    /// Whether the failure may be retried.
    disposition: RetryDisposition,
    /// Raw response from the server, retained for diagnostics.
    raw_response: Option<String>,
    /// Provider-supplied delay from an HTTP 429 `Retry-After` header.
    retry_after: Option<Duration>,
}

// ── ClassifierClient ───────────────────────────────────────────────────────

/// A prepared (but not yet sent) classification HTTP request.
///
/// Created by `build_classification_request` so that callers can renew
/// their processing lease between preparation and submission.
pub struct PreparedClassificationRequest {
    /// The built HTTP request ready to be sent.
    request: reqwest::RequestBuilder,
}

/// Configured OpenAI-compatible vision classifier client.
#[derive(Debug)]
pub struct ClassifierClient {
    /// Shared HTTP client with configured timeouts.
    http: SharedHttpClient,
    /// Assembled endpoint URL (base_url + endpoint).
    endpoint_url: Url,
    /// Model name to use.
    model: String,
    /// Optional API key for Bearer authentication.
    api_key: Option<Secret>,
    /// Basic auth username (empty string means no Basic auth).
    username: Option<String>,
    /// Basic auth password (redacted in Debug/Display).
    password: Option<Secret>,
    /// Temperature for generation.
    temperature: f32,
    /// Maximum tokens to generate.
    max_tokens: u32,
    /// Version identifier for the prompts.
    prompt_version: String,
}

/// Read a `reqwest::Response` body incrementally, rejecting once
/// `MAX_RESPONSE_BYTES + 1` bytes are observed.
///
/// Uses the chunk-based streaming API so that even a fast oversized
/// response cannot consume arbitrary memory.  `Content-Length` is
/// checked first as an early shortcut, but the chunk-level guard is
/// the authoritative enforcement.
async fn read_response_body_bounded(
    response: &mut reqwest::Response,
    status: u16,
) -> Result<Bytes, ClassifierError> {
    let mut total = 0u64;
    let limit = ClassifierClient::MAX_RESPONSE_BYTES;

    // Use a Vec<u8> to collect chunks, then convert to Bytes.
    // This avoids allocating a separate Bytes for each chunk.
    let mut buf = Vec::new();

    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                total = match total.checked_add(chunk.len() as u64) {
                    Some(n) => n,
                    None => {
                        // Overflow — body is definitely too large.
                        let app_err = AppError::new(
                            ErrorCategory::ClassifierTransport,
                            "get_raw_response",
                            "classifier response body exceeds maximum allowed size",
                        );
                        return Err(ClassifierError {
                            error: app_err,
                            disposition: oversized_response_disposition(status),
                            raw_response: None,
                            retry_after: None,
                        });
                    }
                };
                if total > limit {
                    let app_err = AppError::new(
                        classify_status_category(status),
                        "get_raw_response",
                        "classifier response body exceeds maximum allowed size",
                    )
                    .with_http_status(status);
                    return Err(ClassifierError {
                        error: app_err,
                        disposition: oversized_response_disposition(status),
                        raw_response: None,
                        retry_after: None,
                    });
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_e) => {
                let app_err = AppError::new(
                    ErrorCategory::ClassifierTransport,
                    "get_raw_response",
                    "failed to read classifier response body chunk",
                );
                return Err(ClassifierError {
                    error: app_err,
                    disposition: RetryDisposition::Retryable,
                    raw_response: None,
                    retry_after: None,
                });
            }
        }
    }

    // Convert the collected Vec to Bytes.
    Ok(Bytes::from(buf))
}

impl ClassifierClient {
    /// Build a classifier client from configuration.
    ///
    /// Requires at least one endpoint, re-validates its base URL (rejecting
    /// credentials, unsupported schemes, and missing hosts),
    /// assembles the endpoint URL without dropping base-path segments, and
    /// constructs the HTTP client with the configured request timeout.
    pub fn from_config(config: &ClassifierConfig) -> AppResult<Self> {
        let endpoint = config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.enabled)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Configuration,
                    "classifier_from_config",
                    "at least one classifier endpoint must be configured and enabled",
                )
            })?;
        Self::from_endpoint_config(endpoint)
    }

    /// Build a client for one configured endpoint in a classifier pool.
    pub fn from_endpoint_config(config: &ClassifierEndpointConfig) -> AppResult<Self> {
        // Re-validate the base URL to reject credentials, unsupported schemes,
        // and missing hosts. The config-level validation checks these too, but
        // this protects callers that construct endpoint configs directly.
        validate_classifier_base_url(&config.base_url)?;

        let endpoint_url = build_classifier_url(&config.base_url, &config.endpoint)?;
        let http = SharedHttpClient::build(HttpClientConfig::from_seconds(
            config.request_timeout_seconds,
            config.request_timeout_seconds,
            false,
        ))?;

        Ok(Self {
            http,
            endpoint_url,
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            username: (!config.username.is_empty()).then(|| config.username.clone()),
            password: config.password.clone(),
            temperature: config.generation.temperature,
            max_tokens: config.generation.max_tokens,
            prompt_version: config.prompt_version.clone(),
        })
    }

    /// Encode a JPEG image into a prepared HTTP request body.
    ///
    /// This is the synchronous preparation phase that converts the JPEG
    /// into a Base64 data URL and builds the chat-completions request.
    /// It does NOT send the HTTP request.
    ///
    /// Callers should renew their processing lease after calling this
    /// method and before calling `submit_classification` so that the
    /// HTTP request is bounded by the lease duration.
    pub fn build_classification_request(
        &self,
        jpeg: &[u8],
    ) -> Result<PreparedClassificationRequest, ClassifierError> {
        // Encode JPEG as Base64 data URL.
        let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let image_url = format!("data:image/jpeg;base64,{encoded}");

        // Build the request body.
        let body_bytes = build_request_body(
            &image_url,
            &self.model,
            self.temperature,
            self.max_tokens,
            &self.prompt_version,
        )
        .map_err(|e| ClassifierError {
            error: e,
            disposition: RetryDisposition::Permanent,
            raw_response: None,
            retry_after: None,
        })?;

        // Build the HTTP request (but do not send it).
        let request = build_request(
            &self.http,
            &self.endpoint_url,
            &body_bytes,
            self.api_key.as_ref(),
            self.username.as_deref(),
            self.password.as_ref(),
        )
        .map_err(|e| ClassifierError {
            error: e,
            disposition: RetryDisposition::Permanent,
            raw_response: None,
            retry_after: None,
        })?;

        Ok(PreparedClassificationRequest { request })
    }

    /// Maximum classifier response body size (10 MB).
    ///
    /// Applied incrementally during body transfer so that a fast oversized
    /// response cannot consume arbitrary memory, for both successful and
    /// non-success statuses.
    const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

    /// Send a prepared classification request and return the raw HTTP response
    /// bytes.
    ///
    /// This method handles the HTTP transport phase only — it sends the
    /// request, reads the response body incrementally, and returns the raw
    /// bytes.  It does NOT perform UTF-8 conversion, JSON parsing, extraction,
    /// or validation.
    ///
    /// **Incremental body reading:** The response body is read in chunks and
    /// rejected as soon as `MAX_RESPONSE_BYTES + 1` bytes are observed, for
    /// both successful and non-success statuses.  `Content-Length` is used as
    /// an early check when available, but the incremental guard is the
    /// authoritative enforcement.
    ///
    /// Callers should renew their processing lease immediately after calling
    /// this method (before any CPU-heavy response processing) so that the
    /// lease does not expire during classification parsing.
    ///
    /// Returns `ClassifierError` for transport failures, oversized bodies,
    /// invalid UTF-8, non-success HTTP status, and body transfer failures.
    pub async fn get_raw_response(
        &self,
        prepared: PreparedClassificationRequest,
    ) -> Result<Bytes, ClassifierError> {
        let operation = "get_raw_response";

        // Send the request.
        let mut response = match prepared.request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                let app_err = map_classifier_transport_error(operation, e);
                return Err(ClassifierError {
                    error: app_err,
                    disposition: RetryDisposition::Retryable,
                    raw_response: None,
                    retry_after: None,
                });
            }
        };

        let status = response.status().as_u16();
        // Capture this before consuming the body. The scanner uses it to
        // cool down the entire provider/endpoint, not merely this image.
        let retry_after = (status == 429)
            .then(|| {
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(Duration::from_secs)
            })
            .flatten();

        // Early Content-Length check — if the header is present and already
        // exceeds the limit, reject immediately without reading the body.
        // Preserve the HTTP status disposition for oversized non-success
        // responses: authentication/authorization and other permanent HTTP
        // failures must not consume retry attempts.  Successful responses
        // remain retryable classifier-response failures.
        if let Some(content_length) = response.content_length()
            && content_length > Self::MAX_RESPONSE_BYTES
        {
            let category = classify_status_category(status);
            let app_err = AppError::new(
                category,
                operation,
                "classifier response Content-Length exceeds maximum allowed size",
            )
            .with_http_status(status);
            // Do not call `bytes()` here: Content-Length is untrusted and
            // the body may be arbitrarily large. Dropping the response is
            // preferable to buffering it after the early rejection.
            return Err(ClassifierError {
                error: app_err,
                disposition: oversized_response_disposition(status),
                raw_response: None,
                retry_after,
            });
        }

        // Read the response body incrementally, enforcing the size limit at
        // the byte-stream level.  This prevents a fast oversized response
        // from consuming arbitrary memory, for both 2xx and non-2xx statuses.
        let raw_bytes = match read_response_body_bounded(&mut response, status).await {
            Ok(bytes) => bytes,
            Err(classifier_err) => return Err(classifier_err),
        };

        // Non-success HTTP status — convert body to String in a blocking
        // thread to prevent a large error body from blocking the async
        // runtime.  The body size is already bounded by the incremental
        // reader, so we only need to clamp the conversion for safety.
        if !(200..300).contains(&status) {
            let status_for_fn = status;
            let body_for_fn = raw_bytes;
            let raw_response = tokio::task::spawn_blocking(move || {
                let len = body_for_fn.len().min(Self::MAX_RESPONSE_BYTES as usize);
                match String::from_utf8(body_for_fn[..len].to_vec()) {
                    Ok(text) => text,
                    Err(_e) => "<non-UTF-8 response body>".to_string(),
                }
            })
            .await
            .unwrap_or_else(|_| "<conversion panicked>".to_string());

            let category = classify_status_category(status_for_fn);
            let disposition = classify_status_disposition(status_for_fn);
            let app_err = AppError::new(
                category,
                operation,
                "classifier returned non-success status",
            )
            .with_http_status(status_for_fn);
            return Err(ClassifierError {
                error: app_err,
                disposition,
                raw_response: Some(raw_response),
                retry_after,
            });
        }

        // Successful status — return Bytes directly without cloning or
        // UTF-8 conversion so the async worker is not blocked on CPU work.
        Ok(raw_bytes)
    }

    /// Parse raw classifier response bytes into a validated `ClassifierOutput`.
    ///
    /// This method performs UTF-8 conversion, JSON parsing, classification
    /// extraction, and validation.  It is CPU-heavy and should be called
    /// AFTER the processing lease has been renewed.
    ///
    /// `raw_bytes` must be the raw HTTP response bytes returned by
    /// `get_raw_response`.
    pub fn parse_response(&self, raw_bytes: Bytes) -> Result<ClassifierOutput, ClassifierError> {
        let operation = "parse_response";

        // UTF-8 conversion — done here (in spawn_blocking) so the async
        // worker is not blocked on CPU work.
        let raw_response = match String::from_utf8(raw_bytes.to_vec()) {
            Ok(text) => text,
            Err(_e) => {
                let app_err = AppError::new(
                    ErrorCategory::ClassifierResponse,
                    operation,
                    "classifier response body is not valid UTF-8",
                );
                return Err(ClassifierError {
                    error: app_err,
                    disposition: RetryDisposition::Retryable,
                    raw_response: None,
                    retry_after: None,
                });
            }
        };

        // Parse JSON.
        let root = match serde_json::from_str::<Value>(&raw_response) {
            Ok(v) => v,
            Err(_) => {
                let app_err = AppError::new(
                    ErrorCategory::ClassifierResponse,
                    operation,
                    "classifier response is not valid JSON",
                );
                return Err(ClassifierError {
                    error: app_err,
                    disposition: RetryDisposition::Retryable,
                    raw_response: Some(raw_response),
                    retry_after: None,
                });
            }
        };

        // Extract classification.
        let classification_value = match extract_classification_value(root) {
            Ok(v) => v,
            Err(e) => {
                return Err(ClassifierError {
                    error: e,
                    disposition: RetryDisposition::Retryable,
                    raw_response: Some(raw_response),
                    retry_after: None,
                });
            }
        };

        // Validate classification.
        let (classification, classification_json) =
            match validate_classification(classification_value) {
                Ok(pair) => pair,
                Err(e) => {
                    return Err(ClassifierError {
                        error: e,
                        disposition: RetryDisposition::Retryable,
                        raw_response: Some(raw_response),
                        retry_after: None,
                    });
                }
            };

        Ok(ClassifierOutput {
            classification,
            classification_json,
            raw_response,
        })
    }

    /// Send a prepared classification request and return the validated result.
    ///
    /// This is the async HTTP submission phase.  Callers must have renewed
    /// their processing lease immediately before calling this method so that
    /// the request is bounded by the lease duration.
    ///
    /// Internally uses `get_raw_response` + `parse_response` to allow
    /// callers to renew the lease between HTTP response and CPU-heavy parsing.
    ///
    /// `get_raw_response` returns `Bytes` directly for successful statuses
    /// without cloning or UTF-8 conversion, so the async worker is not
    /// blocked on CPU work during response-body handling.
    pub async fn submit_classification(
        &self,
        prepared: PreparedClassificationRequest,
    ) -> Result<ClassifierOutput, ClassifierError> {
        let raw_bytes = self.get_raw_response(prepared).await?;
        self.parse_response(raw_bytes)
    }

    /// Submit a JPEG image for wildlife classification.
    ///
    /// Convenience method that combines `build_classification_request` and
    /// `submit_classification` into a single call.  Callers that need to
    /// renew their processing lease between preparation and submission
    /// should use the split methods directly.
    pub async fn classify_jpeg(&self, jpeg: &[u8]) -> Result<ClassifierOutput, ClassifierError> {
        let prepared = self.build_classification_request(jpeg)?;
        self.submit_classification(prepared).await
    }

    /// Return the configured maximum completion tokens for quota reservation.
    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// Return the assembled endpoint URL for testing.
    pub fn endpoint_url(&self) -> &Url {
        &self.endpoint_url
    }

    /// Model recorded with classifications sent through this client.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Prompt version recorded with classifications sent through this client.
    pub fn prompt_version(&self) -> &str {
        &self.prompt_version
    }
}

// Manual Debug for ClassifierOutput — omits classification, classification_json,
// and raw_response to keep the output payload-safe.
// Only safe metadata (counts and lengths) is exposed.
impl fmt::Debug for ClassifierOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassifierOutput")
            .field("species_count", &self.classification.species.len())
            .field(
                "uncertainties_count",
                &self.classification.uncertainties.len(),
            )
            .field("classification_json_len", &self.classification_json.len())
            .field("raw_response_len", &self.raw_response.len())
            .finish_non_exhaustive()
    }
}

// ── Endpoint URL assembly ──────────────────────────────────────────────────

/// Assemble the classifier endpoint URL by appending the endpoint path to the
/// base URL path without dropping base-path segments (e.g. /v1).
///
/// `Url::join` treats an endpoint starting with `/` as an absolute path that
/// replaces the base path, so we must manually concatenate paths.
fn build_classifier_url(base_url: &Url, endpoint: &str) -> AppResult<Url> {
    // Reject endpoints with query strings or fragments — they could alter
    // the target URL in unexpected ways.
    if endpoint.contains('?') || endpoint.contains('#') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must not contain query or fragment components",
        ));
    }

    // Reject endpoints with credentials.
    if endpoint.contains('@') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must not contain embedded credentials",
        ));
    }

    // Reject scheme-relative references (//host/path).
    if endpoint.starts_with("//") {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must be an absolute path, not a scheme-relative reference",
        ));
    }

    // Reject absolute URLs.
    if endpoint.contains("://") {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must be a relative path, not a full URL",
        ));
    }

    // Reject backslashes.
    if endpoint.contains('\\') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must not contain backslashes",
        ));
    }

    // Reject dot segments in the endpoint (absolute path segments).
    // These could be used to traverse above the configured base path.
    // We check both literal and percent-encoded forms.
    if has_path_traversal(endpoint) {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must not contain dot segments that could traverse above the base path",
        ));
    }

    let mut url = base_url.clone();

    // Strip trailing slash from base URL path and clone it so we can
    // mutate `url` afterwards without borrow conflicts.
    let base_path_str = url
        .path()
        .strip_suffix('/')
        .unwrap_or(url.path())
        .to_string();

    // Ensure endpoint starts with / for proper path joining.
    let ep = if endpoint.starts_with('/') {
        endpoint
    } else {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must start with '/'",
        ));
    };

    // Concatenate base path + endpoint.
    let full_path = format!("{base_path_str}{ep}");
    url.set_path(&full_path);

    // Clear any query/fragment from the base that might have been present.
    url.set_query(None);
    url.set_fragment(None);

    // Final safety check: ensure the assembled URL still has a path that
    // starts with the base path (i.e. no dot-segment traversal succeeded).
    let assembled_path = url.path();
    if !assembled_path.starts_with(&base_path_str) {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "build_classifier_url",
            "classifier.endpoint must not contain dot segments that could traverse above the base path",
        ));
    }

    Ok(url)
}

/// Check whether a path segment contains `.` or `..` components,
/// including percent-encoded variants like `%2e` and `%2E`.
///
/// This is segment-aware: it splits by `/`, URL-decodes each segment,
/// and checks whether any decoded segment is `.` or `..`.
fn has_path_traversal(segment: &str) -> bool {
    for part in segment.split('/') {
        // Check literal dot segments.
        if part == "." || part == ".." {
            return true;
        }
        // URL-decode and check for dot segments.
        if url_decoded_has_dot_segment(part) {
            return true;
        }
    }
    false
}

/// URL-decode a single path segment and check whether the decoded result
/// is a dot segment (`.` or `..`).
fn url_decoded_has_dot_segment(segment: &str) -> bool {
    let decoded = url_decode(segment);
    decoded == "." || decoded == ".."
}

/// Minimal URL percent-decoder for path segments.
///
/// Decodes `%XX` sequences (case-insensitive hex) into their byte
/// representation. Non-percent-encoded bytes pass through unchanged.
fn url_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = hex_pair(bytes[i + 1], bytes[i + 2])
        {
            result.push(byte as char);
            i += 3;
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Decode two hex ASCII bytes into a single byte, or `None` if invalid.
fn hex_pair(high: u8, low: u8) -> Option<u8> {
    let h = hex_digit(high)?;
    let l = hex_digit(low)?;
    Some((h << 4) | l)
}

/// Decode a single hex ASCII digit into its 4-bit value.
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Validate the base URL for the classifier.
///
/// Rejects URLs with embedded credentials, unsupported schemes, or missing hosts.
/// This is a second layer of validation that runs in from_config to ensure
/// the base URL is safe even if the config was constructed programmatically.
fn validate_classifier_base_url(url: &Url) -> AppResult<()> {
    // Reject URLs with embedded credentials.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_classifier_base_url",
            "classifier.base_url must not contain embedded credentials (username/password)",
        ));
    }

    // Reject unsupported schemes.
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_classifier_base_url",
            format!("classifier.base_url must use the http or https scheme, got {scheme}"),
        ));
    }

    // Reject missing hosts.
    if url.host().is_none() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_classifier_base_url",
            "classifier.base_url must have a valid host",
        ));
    }

    Ok(())
}

/// Map a reqwest transport error to a ClassifierTransport AppError.
///
/// This ensures that all transport-level failures (timeout, DNS, connection,
/// body transfer) are consistently categorized as ClassifierTransport rather
/// than leaking Timeout, Network, or other categories from the shared HTTP layer.
fn map_classifier_transport_error(operation: &'static str, _error: reqwest::Error) -> AppError {
    // Always categorize transport failures as ClassifierTransport.
    // The message is fixed and payload-safe — never includes URLs or raw error text.
    AppError::new(
        ErrorCategory::ClassifierTransport,
        operation,
        "classifier request transport failed",
    )
}

// ── Request construction ───────────────────────────────────────────────────

/// Versioned system prompt instructing the model to classify wildlife.
fn system_prompt(version: &str) -> String {
    format!(
        "\
You are a wildlife image classifier. Analyze the provided image and return \
a structured JSON classification. Your task is to determine whether the image \
contains an animal, whether it is wildlife (not domestic), and whether it is \
interesting enough for review.

Rules:
- Animals include mammals, birds, reptiles, amphibians, fish, and insects.
- Wildlife excludes domesticated animals (cats, dogs, farm livestock, pets).
- Domestic animals may be identified but should normally have \
contains_wildlife = false unless the prompt defines otherwise.
- Interesting images include rare species, unusual behavior, multiple animals, \
close encounters, or scenic wildlife moments.
- Humans, vehicles, vegetation movement, shadows, rain, insects near the lens, \
and camera artifacts should NOT be classified as wildlife.
- Be cautious with species identification — use broad labels when uncertain.
- Report uncertainties about visibility, image quality, or identification confidence.
- Confidence values must be between 0.0 and 1.0.

Prompt version: {version}",
    )
}

/// Versioned user prompt for the classification task.
fn user_prompt(version: &str) -> String {
    format!(
        "Classify this image for wildlife detection.\n\n\
        Prompt version: {version}\n\n\
        Return a JSON object with the following fields:\n\
        - contains_animal: boolean\n\
        - contains_wildlife: boolean\n\
        - is_interesting: boolean\n\
        - species: array of {{ name: string, confidence: number }}\n\
        - overall_confidence: number between 0.0 and 1.0\n\
        - summary: brief description of the scene\n\
        - uncertainties: array of uncertainty strings",
    )
}

// ── OpenAI message content types ───────────────────────────────────────────

/// A single content part inside a user message array.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserMessagePart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlPart },
}

#[derive(Serialize)]
struct ImageUrlPart {
    url: String,
}

/// Message content that can be a plain string (system) or an array of parts (user).
#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    /// Plain text content (used for system messages).
    Text(String),
    /// Array of content parts (used for user messages with images).
    Parts(UserMessageParts),
}

/// Newtype wrapper that serialises as a bare JSON array.
#[derive(Serialize)]
struct UserMessageParts(Vec<UserMessagePart>);

/// Internal serializable request model for the OpenAI chat-completions API.
#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
    max_tokens: u32,
    #[serde(rename = "response_format")]
    response_format: ResponseFormat,
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    content: MessageContent,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
    #[serde(rename = "json_schema")]
    json_schema: JsonSchema,
}

#[derive(Serialize)]
struct JsonSchema {
    name: &'static str,
    #[serde(rename = "strict")]
    strict: bool,
    schema: Value,
}

/// Build the serializable request body and return it as JSON bytes.
fn build_request_body(
    image_url: &str,
    model: &str,
    temperature: f32,
    max_tokens: u32,
    prompt_version: &str,
) -> AppResult<Vec<u8>> {
    let system_text = system_prompt(prompt_version);
    let user_text = user_prompt(prompt_version);

    let system_msg = Message {
        role: "system",
        content: MessageContent::Text(system_text),
    };

    let user_parts = UserMessageParts(vec![
        UserMessagePart::Text {
            text: user_text.clone(),
        },
        UserMessagePart::ImageUrl {
            image_url: ImageUrlPart {
                url: image_url.to_string(),
            },
        },
    ]);

    let user_msg = Message {
        role: "user",
        content: MessageContent::Parts(user_parts),
    };

    let schema = build_strict_json_schema();

    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![system_msg, user_msg],
        temperature,
        max_tokens,
        response_format: ResponseFormat {
            format_type: "json_schema",
            json_schema: JsonSchema {
                name: "wildlife_classification",
                strict: true,
                schema,
            },
        },
    };

    serde_json::to_vec(&request).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Internal,
            "build_request_body",
            format!("failed to serialize classifier request: {e}"),
            e,
        )
    })
}

/// Build a JSON schema compliant with OpenAI strict structured-output mode.
///
/// Requirements for strict mode:
/// - `additionalProperties: false` on all object schemas.
/// - `required` on each object schema listing all properties.
/// - No `required` on array schemas; it belongs on the item object.
/// - Numeric fields use `minimum` and `maximum` constraints.
fn build_strict_json_schema() -> Value {
    // Species item schema (used inside the species array).
    let species_item_schema = Value::Object(serde_json::Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        (
            "properties".to_string(),
            Value::Object(serde_json::Map::from_iter([
                (
                    "name".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("string".to_string())),
                        (
                            "description".to_string(),
                            Value::String("Species or animal type name".to_string()),
                        ),
                    ])),
                ),
                (
                    "confidence".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("number".to_string())),
                        (
                            "description".to_string(),
                            Value::String("Confidence between 0.0 and 1.0".to_string()),
                        ),
                        (
                            "minimum".to_string(),
                            Value::Number(serde_json::Number::from(0)),
                        ),
                        (
                            "maximum".to_string(),
                            Value::Number(serde_json::Number::from(1)),
                        ),
                    ])),
                ),
            ])),
        ),
        (
            "required".to_string(),
            Value::Array(vec![
                Value::String("name".to_string()),
                Value::String("confidence".to_string()),
            ]),
        ),
        ("additionalProperties".to_string(), Value::Bool(false)),
    ]));

    Value::Object(serde_json::Map::from_iter([
        ("type".to_string(), Value::String("object".to_string())),
        (
            "properties".to_string(),
            Value::Object(serde_json::Map::from_iter([
                (
                    "contains_animal".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("boolean".to_string())),
                        (
                            "description".to_string(),
                            Value::String("Whether an animal is present".to_string()),
                        ),
                    ])),
                ),
                (
                    "contains_wildlife".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("boolean".to_string())),
                        (
                            "description".to_string(),
                            Value::String(
                                "Whether the animal is wildlife (not domestic)".to_string(),
                            ),
                        ),
                    ])),
                ),
                (
                    "is_interesting".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("boolean".to_string())),
                        (
                            "description".to_string(),
                            Value::String(
                                "Whether the image is interesting enough to review".to_string(),
                            ),
                        ),
                    ])),
                ),
                (
                    "species".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("array".to_string())),
                        ("items".to_string(), species_item_schema),
                    ])),
                ),
                (
                    "overall_confidence".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("number".to_string())),
                        (
                            "description".to_string(),
                            Value::String("Overall confidence between 0.0 and 1.0".to_string()),
                        ),
                        (
                            "minimum".to_string(),
                            Value::Number(serde_json::Number::from(0)),
                        ),
                        (
                            "maximum".to_string(),
                            Value::Number(serde_json::Number::from(1)),
                        ),
                    ])),
                ),
                (
                    "summary".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("string".to_string())),
                        (
                            "description".to_string(),
                            Value::String("Concise scene description".to_string()),
                        ),
                    ])),
                ),
                (
                    "uncertainties".to_string(),
                    Value::Object(serde_json::Map::from_iter([
                        ("type".to_string(), Value::String("array".to_string())),
                        (
                            "items".to_string(),
                            Value::Object(serde_json::Map::from_iter([(
                                "type".to_string(),
                                Value::String("string".to_string()),
                            )])),
                        ),
                    ])),
                ),
            ])),
        ),
        (
            "required".to_string(),
            Value::Array(vec![
                Value::String("contains_animal".to_string()),
                Value::String("contains_wildlife".to_string()),
                Value::String("is_interesting".to_string()),
                Value::String("species".to_string()),
                Value::String("overall_confidence".to_string()),
                Value::String("summary".to_string()),
                Value::String("uncertainties".to_string()),
            ]),
        ),
        ("additionalProperties".to_string(), Value::Bool(false)),
    ]))
}

/// Build an HTTP request with authentication headers applied.
fn build_request(
    http: &SharedHttpClient,
    endpoint_url: &Url,
    body_bytes: &[u8],
    api_key: Option<&Secret>,
    username: Option<&str>,
    password: Option<&Secret>,
) -> AppResult<reqwest::RequestBuilder> {
    // Build a HeaderMap so we can include multiple Authorization values
    // when both Bearer and Basic auth are configured.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Content-Type",
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    // Apply Bearer API-key authentication if configured.
    if let Some(api_key) = api_key {
        let value = format!("Bearer {}", api_key.expose());
        headers.insert(
            "Authorization",
            reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
                AppError::new(
                    ErrorCategory::Internal,
                    "build_request",
                    "invalid Bearer authorization header",
                )
            })?,
        );
    }

    // Apply Basic authentication if configured.
    // When both Bearer and Basic are configured, we append the Basic
    // header so both Authorization values are present.
    if let (Some(user), Some(pass)) = (username, password) {
        let basic_value = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{}", pass.expose()))
        );
        headers.append(
            "Authorization",
            reqwest::header::HeaderValue::from_str(&basic_value).map_err(|_| {
                AppError::new(
                    ErrorCategory::Internal,
                    "build_request",
                    "invalid Basic authorization header",
                )
            })?,
        );
    }

    Ok(http
        .inner()
        .post(endpoint_url.to_string())
        .headers(headers)
        .body(body_bytes.to_vec()))
}

// ── Shared candidate decoder ───────────────────────────────────────────────

/// Decode a candidate classification value from a structured-output location
/// (output_parsed or message.parsed).
///
/// Accepts both JSON objects and JSON strings:
/// - Objects are validated directly.
/// - Strings are trimmed, optionally unwrapped from a Markdown fence, parsed
///   as JSON, then validated.
fn decode_structured_candidate(value: &Value) -> Result<Value, AppError> {
    // Direct object — validate in place.
    if let Some(obj) = value.as_object() {
        return Ok(Value::Object(obj.clone()));
    }

    // String candidate — trim, unwrap fences, parse, validate.
    if let Some(s) = value.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(AppError::new(
                ErrorCategory::ClassifierResponse,
                "decode_structured_candidate",
                "structured output candidate is empty",
            ));
        }
        let unwrapped = unwrap_markdown_json_fence(trimmed)?;
        let parsed = serde_json::from_str::<Value>(unwrapped).map_err(|e| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "decode_structured_candidate",
                format!("failed to parse structured output candidate JSON: {e}"),
            )
        })?;
        return Ok(parsed);
    }

    Err(AppError::new(
        ErrorCategory::ClassifierResponse,
        "decode_structured_candidate",
        "structured output candidate must be an object or string",
    ))
}

// ── Response extraction ────────────────────────────────────────────────────

/// Extract the candidate classification JSON value from the response in a
/// deterministic order:
/// 1. Direct `classification` object.
/// 2. Top-level `output_parsed` (OpenAI structured output).
/// 3. `choices[0].message.parsed` (structured output).
/// 4. `choices[0].message.content` (assistant message content).
fn extract_classification_value(root: Value) -> Result<Value, AppError> {
    // 1. Direct classification object — check root itself first.
    if let Some(obj) = root.as_object()
        && is_classification_object(obj)
    {
        return Ok(root.clone());
    }

    // 2. Top-level output_parsed (OpenAI structured output).
    if let Some(parsed) = root.get("output_parsed")
        && let Ok(decoded) = decode_structured_candidate(parsed)
        && let Some(obj) = decoded.as_object()
        && is_classification_object(obj)
    {
        return Ok(Value::Object(obj.clone()));
    }

    // 3. choices[0].message.parsed (structured output).
    if let Some(choices) = root.get("choices").and_then(|v| v.as_array())
        && let Some(first) = choices.first()
        && let Some(message) = first.get("message")
        && let Some(parsed) = message.get("parsed")
        && let Ok(decoded) = decode_structured_candidate(parsed)
        && let Some(obj) = decoded.as_object()
        && is_classification_object(obj)
    {
        return Ok(Value::Object(obj.clone()));
    }

    // 4. choices[0].message.content.
    if let Some(choices) = root.get("choices").and_then(|v| v.as_array())
        && let Some(first) = choices.first()
        && let Some(message) = first.get("message")
        && let Some(content) = message.get("content")
    {
        if let Some(text) = content.as_str() {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(AppError::new(
                    ErrorCategory::ClassifierResponse,
                    "extract_classification",
                    "assistant message content is empty",
                ));
            }
            let unwrapped = unwrap_markdown_json_fence(trimmed)?;
            return serde_json::from_str(unwrapped).map_err(|e| {
                AppError::new(
                    ErrorCategory::ClassifierResponse,
                    "extract_classification",
                    format!("failed to parse classification JSON: {e}"),
                )
            });
        }
        // content could be an array of content parts (image + text).
        if let Some(parts) = content.as_array() {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let unwrapped = unwrap_markdown_json_fence(trimmed)?;
                    return serde_json::from_str(unwrapped).map_err(|e| {
                        AppError::new(
                            ErrorCategory::ClassifierResponse,
                            "extract_classification",
                            format!("failed to parse classification JSON: {e}"),
                        )
                    });
                }
            }
        }
    }

    Err(AppError::new(
        ErrorCategory::ClassifierResponse,
        "extract_classification",
        "unsupported response envelope: could not locate classification data",
    ))
}

/// Check whether a JSON object has the minimum structure of a classification
/// response (at least one required field present).
fn is_classification_object(obj: &serde_json::Map<String, Value>) -> bool {
    obj.contains_key("contains_animal")
        || obj.contains_key("contains_wildlife")
        || obj.contains_key("is_interesting")
        || obj.contains_key("species")
        || obj.contains_key("overall_confidence")
        || obj.contains_key("summary")
        || obj.contains_key("uncertainties")
}

/// Remove a single well-formed whole-content Markdown fence.
///
/// Only removes fences where the entire content is wrapped — does not mine
/// JSON from arbitrary prose.
fn unwrap_markdown_json_fence(content: &str) -> AppResult<&str> {
    let trimmed = content.trim();

    // Match ```json ... ``` or ``` ... ```
    if let Some(after_open) = trimmed.strip_prefix("```json") {
        let after_open = after_open.trim_start();
        if let Some(inner) = after_open.strip_suffix("```") {
            let inner = inner.trim();
            if inner.is_empty() {
                return Err(AppError::new(
                    ErrorCategory::ClassifierResponse,
                    "unwrap_markdown_json_fence",
                    "markdown fence contains empty JSON",
                ));
            }
            return Ok(inner);
        }
    }

    if let Some(after_open) = trimmed.strip_prefix("```") {
        let after_open = after_open.trim_start();
        if let Some(inner) = after_open.strip_suffix("```") {
            let inner = inner.trim();
            if inner.is_empty() {
                return Err(AppError::new(
                    ErrorCategory::ClassifierResponse,
                    "unwrap_markdown_json_fence",
                    "markdown fence contains empty JSON",
                ));
            }
            return Ok(inner);
        }
    }

    // No fence found — return the content as-is for JSON parsing.
    Ok(trimmed)
}

// ── Classification validation ──────────────────────────────────────────────

/// Validate the classification JSON and return the typed result plus
/// normalized compact JSON string.
fn validate_classification(value: Value) -> Result<(WildlifeClassification, String), AppError> {
    // Parse as a generic object first to check required fields.
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            return Err(AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "classification must be a JSON object",
            ));
        }
    };

    // Validate contains_animal.
    let contains_animal = obj
        .get("contains_animal")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: contains_animal",
            )
        })?
        .as_bool()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "contains_animal must be a boolean",
            )
        })?;

    // Validate contains_wildlife.
    let contains_wildlife = obj
        .get("contains_wildlife")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: contains_wildlife",
            )
        })?
        .as_bool()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "contains_wildlife must be a boolean",
            )
        })?;

    // Validate is_interesting.
    let is_interesting = obj
        .get("is_interesting")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: is_interesting",
            )
        })?
        .as_bool()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "is_interesting must be a boolean",
            )
        })?;

    // Validate species array.
    let species = obj
        .get("species")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: species",
            )
        })?
        .as_array()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "species must be an array",
            )
        })?
        .iter()
        .map(validate_species_item)
        .collect::<Result<Vec<SpeciesPrediction>, AppError>>()?;

    // Validate overall_confidence.
    let overall_confidence = obj
        .get("overall_confidence")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: overall_confidence",
            )
        })?
        .as_f64()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "overall_confidence must be a number",
            )
        })?;

    #[allow(clippy::manual_range_contains)]
    if !overall_confidence.is_finite() || overall_confidence < 0.0 || overall_confidence > 1.0 {
        return Err(AppError::new(
            ErrorCategory::ClassifierResponse,
            "validate_classification",
            "overall_confidence must be a finite number between 0.0 and 1.0",
        ));
    }

    // Validate summary.
    let summary = obj
        .get("summary")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: summary",
            )
        })?
        .as_str()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "summary must be a string",
            )
        })?
        .to_string();

    // Validate uncertainties.
    let uncertainties = obj
        .get("uncertainties")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "missing required field: uncertainties",
            )
        })?
        .as_array()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_classification",
                "uncertainties must be an array",
            )
        })?
        .iter()
        .map(|u| {
            u.as_str().map(|s| s.to_string()).ok_or_else(|| {
                AppError::new(
                    ErrorCategory::ClassifierResponse,
                    "validate_classification",
                    "uncertainties must contain strings",
                )
            })
        })
        .collect::<Result<Vec<String>, AppError>>()?;

    let classification = WildlifeClassification {
        contains_animal,
        contains_wildlife,
        is_interesting,
        species,
        overall_confidence,
        summary,
        uncertainties,
    };

    // Serialize the original value compactly to preserve additional fields.
    let classification_json = serde_json::to_string(&value).map_err(|e| {
        AppError::with_source(
            ErrorCategory::Internal,
            "validate_classification",
            format!("failed to serialize classification: {e}"),
            e,
        )
    })?;

    Ok((classification, classification_json))
}

/// Validate a single species item in the species array.
fn validate_species_item(item: &Value) -> Result<SpeciesPrediction, AppError> {
    let obj = match item.as_object() {
        Some(o) => o,
        None => {
            return Err(AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_species_item",
                "species item must be a JSON object",
            ));
        }
    };

    let name = obj
        .get("name")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_species_item",
                "species item missing required field: name",
            )
        })?
        .as_str()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_species_item",
                "species name must be a string",
            )
        })?
        .to_string();

    let confidence = obj
        .get("confidence")
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_species_item",
                "species item missing required field: confidence",
            )
        })?
        .as_f64()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::ClassifierResponse,
                "validate_species_item",
                "species confidence must be a number",
            )
        })?;

    #[allow(clippy::manual_range_contains)]
    if !confidence.is_finite() || confidence < 0.0 || confidence > 1.0 {
        return Err(AppError::new(
            ErrorCategory::ClassifierResponse,
            "validate_species_item",
            "species confidence must be a finite number between 0.0 and 1.0",
        ));
    }

    Ok(SpeciesPrediction { name, confidence })
}

// ── HTTP status categorization ─────────────────────────────────────────────

/// Categorize an HTTP status code into an error category.
fn classify_status_category(status: u16) -> ErrorCategory {
    match status {
        401 => ErrorCategory::Authentication,
        403 => ErrorCategory::Authorization,
        408 | 429 => ErrorCategory::ClassifierTransport,
        400..=499 => ErrorCategory::ClassifierTransport,
        500..=599 => ErrorCategory::ClassifierTransport,
        _ => ErrorCategory::ClassifierTransport,
    }
}

/// Determine whether an HTTP status code indicates a retryable failure.
fn classify_status_disposition(status: u16) -> RetryDisposition {
    match status {
        401 | 403 => RetryDisposition::Permanent,
        408 | 429 | 500..=599 => RetryDisposition::Retryable,
        300..=399 => RetryDisposition::Permanent,
        _ => RetryDisposition::Permanent,
    }
}

/// Oversized successful responses are retryable response failures, while an
/// oversized non-success response keeps the disposition of its HTTP status.
fn oversized_response_disposition(status: u16) -> RetryDisposition {
    if (200..300).contains(&status) {
        RetryDisposition::Retryable
    } else {
        classify_status_disposition(status)
    }
}

// ── ClassifierError Display/Debug ──────────────────────────────────────────

impl ClassifierError {
    /// Create a new ClassifierError.
    pub fn new(
        error: AppError,
        disposition: RetryDisposition,
        raw_response: Option<String>,
    ) -> Self {
        Self {
            error,
            disposition,
            raw_response,
            retry_after: None,
        }
    }

    /// Return the underlying error category.
    pub fn category(&self) -> ErrorCategory {
        self.error.category
    }

    /// Return the retry disposition.
    pub fn disposition(&self) -> RetryDisposition {
        self.disposition
    }

    /// Return true if the failure may be retried.
    pub fn is_retryable(&self) -> bool {
        self.disposition == RetryDisposition::Retryable
    }

    /// Return the HTTP status code if present.
    pub fn http_status(&self) -> Option<u16> {
        self.error.http_status()
    }

    /// Return the raw response if available.
    pub fn raw_response(&self) -> Option<&str> {
        self.raw_response.as_deref()
    }

    /// Return the provider-supplied 429 cooldown, if one was present.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Return the underlying AppError.
    pub fn into_parts(self) -> (AppError, RetryDisposition, Option<String>) {
        (self.error, self.disposition, self.raw_response)
    }
}

impl fmt::Debug for ClassifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassifierError")
            .field("category", &self.error.category)
            .field("operation", &self.error.operation)
            .field("disposition", &self.disposition)
            .field("http_status", &self.error.http_status)
            .field("retry_after", &self.retry_after)
            .field(
                "raw_response_len",
                &self.raw_response.as_ref().map(|r| r.len()),
            )
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ClassifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;
        if self.disposition == RetryDisposition::Retryable {
            write!(f, " (retryable)")?;
        } else {
            write!(f, " (permanent)")?;
        }
        Ok(())
    }
}

impl std::error::Error for ClassifierError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source.as_ref().map(|e| e.as_ref())
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Endpoint assembly tests ───────────────────────────────────────────

    #[test]
    fn endpoint_assembly_basic() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let url = build_classifier_url(&base, "/chat/completions").unwrap();
        assert_eq!(url.as_str(), "http://localhost:8081/v1/chat/completions");
    }

    #[test]
    fn endpoint_assembly_trailing_slash_normalized() {
        let base = Url::parse("http://localhost:8081/v1/").unwrap();
        let url = build_classifier_url(&base, "/chat/completions").unwrap();
        assert_eq!(url.as_str(), "http://localhost:8081/v1/chat/completions");
    }

    #[test]
    fn endpoint_assembly_no_base_path() {
        let base = Url::parse("http://localhost:8081").unwrap();
        let url = build_classifier_url(&base, "/chat/completions").unwrap();
        assert_eq!(url.as_str(), "http://localhost:8081/chat/completions");
    }

    #[test]
    fn endpoint_assembly_rejects_query() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/chat/completions?foo=bar");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_fragment() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/chat/completions#section");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_credentials() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/chat/@user:pass/completions");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_scheme_relative() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "//evil.example.com/path");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_absolute_url() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "http://other.com/path");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_backslash() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/chat\\completions");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_dot_segment() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/../admin");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_double_dot_segment() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/foo/../../admin");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_percent_encoded_dot() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/%2e%2e/admin");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_percent_encoded_double_dot() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/%2e%2e/%2e%2e/admin");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_percent_encoded_single_dot_lower() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/%2e/admin");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_percent_encoded_single_dot_upper() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/%2E/admin");
        assert!(result.is_err());
    }

    #[test]
    fn endpoint_assembly_rejects_percent_encoded_dot_mixed_case() {
        let base = Url::parse("http://localhost:8081/v1").unwrap();
        let result = build_classifier_url(&base, "/%2E%2e/admin");
        assert!(result.is_err());
    }

    #[test]
    fn build_classifier_url_rejects_credential_base_url() {
        let base = Url::parse("http://user:pass@localhost:8081/v1").unwrap();
        let result = ClassifierClient::from_config(&ClassifierConfig {
            endpoints: vec![ClassifierEndpointConfig {
                enabled: true,
                base_url: base,
                endpoint: "/chat/completions".to_string(),
                model: "test".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 30,
                prompt_version: "wildlife-v1".to_string(),
                generation: crate::configuration::ClassifierGenerationConfig {
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
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("credentials"));
    }

    #[test]
    fn build_classifier_url_rejects_unsupported_scheme() {
        let base = Url::parse("ftp://localhost:8081/v1").unwrap();
        let result = ClassifierClient::from_config(&ClassifierConfig {
            endpoints: vec![ClassifierEndpointConfig {
                enabled: true,
                base_url: base,
                endpoint: "/chat/completions".to_string(),
                model: "test".to_string(),
                api_key: None,
                username: String::new(),
                password: None,
                request_timeout_seconds: 30,
                prompt_version: "wildlife-v1".to_string(),
                generation: crate::configuration::ClassifierGenerationConfig {
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
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("scheme"));
    }

    // ── Request body tests ────────────────────────────────────────────────

    #[test]
    fn request_body_contains_model() {
        let body = build_request_body(
            "data:image/jpeg;base64,SENTINEL",
            "test-model",
            0.1,
            1000,
            "wildlife-v1",
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["model"], "test-model");
    }

    #[test]
    fn request_body_system_is_plain_text() {
        let body = build_request_body(
            "data:image/jpeg;base64,SENTINEL",
            "test-model",
            0.1,
            1000,
            "wildlife-v1",
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        // System message content must be a plain string.
        let sys_content = &parsed["messages"][0]["content"];
        assert!(
            sys_content.is_string(),
            "system content should be a string, got: {sys_content}"
        );
        assert!(sys_content.as_str().unwrap().contains("wildlife"));
        assert!(sys_content.as_str().unwrap().contains("wildlife-v1"));
    }

    #[test]
    fn request_body_user_content_is_array_of_parts() {
        let body = build_request_body(
            "data:image/jpeg;base64,SENTINEL",
            "test-model",
            0.1,
            1000,
            "wildlife-v1",
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        // User message content must be an array of parts.
        let user_content = &parsed["messages"][1]["content"];
        assert!(
            user_content.is_array(),
            "user content should be an array, got: {user_content}"
        );
        let parts = user_content.as_array().unwrap();
        assert_eq!(parts.len(), 2);
        // First part is text.
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"].as_str().unwrap().contains("wildlife-v1"));
        // Second part is image_url.
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,SENTINEL")
        );
    }

    #[test]
    fn request_body_contains_temperature_and_max_tokens() {
        let body = build_request_body(
            "data:image/jpeg;base64,SENTINEL",
            "test-model",
            0.5,
            500,
            "wildlife-v1",
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["temperature"], 0.5);
        assert_eq!(parsed["max_tokens"], 500);
    }

    #[test]
    fn request_body_contains_json_schema_response_format() {
        let body = build_request_body(
            "data:image/jpeg;base64,SENTINEL",
            "test-model",
            0.1,
            1000,
            "wildlife-v1",
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["response_format"]["type"], "json_schema");
        assert_eq!(
            parsed["response_format"]["json_schema"]["name"],
            "wildlife_classification"
        );
        assert_eq!(parsed["response_format"]["json_schema"]["strict"], true);
    }

    // ── JSON schema strict compliance tests ───────────────────────────────

    #[test]
    fn json_schema_has_additional_properties_false_on_root() {
        let schema = build_strict_json_schema();
        assert_eq!(
            schema["additionalProperties"],
            Value::Bool(false),
            "root schema must have additionalProperties: false"
        );
    }

    #[test]
    fn json_schema_has_additional_properties_false_on_object_properties() {
        let schema = build_strict_json_schema();
        let props = schema["properties"].as_object().unwrap();
        for (key, prop_schema) in props {
            // Only object-type properties need additionalProperties.
            if prop_schema["type"] == "object" {
                assert_eq!(
                    prop_schema["additionalProperties"],
                    Value::Bool(false),
                    "property '{key}' object schema must have additionalProperties: false"
                );
            }
        }
    }

    #[test]
    fn json_schema_no_required_on_species_array() {
        let schema = build_strict_json_schema();
        let species_schema = schema["properties"]["species"].as_object().unwrap();
        // The array schema itself must NOT have a "required" key.
        assert!(
            !species_schema.contains_key("required"),
            "species array schema must not have 'required'"
        );
        // The items schema (object) must have "required" with name and confidence.
        let items_schema = species_schema["items"].as_object().unwrap();
        let items_required: Vec<&str> = items_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(items_required.contains(&"name"));
        assert!(items_required.contains(&"confidence"));
    }

    #[test]
    fn json_schema_has_confidence_constraints() {
        let schema = build_strict_json_schema();
        // overall_confidence should have minimum and maximum.
        let oc = schema["properties"]["overall_confidence"]
            .as_object()
            .unwrap();
        assert_eq!(oc["minimum"], 0);
        assert_eq!(oc["maximum"], 1);
        // species item confidence should also have constraints.
        let sp_conf = schema["properties"]["species"]["items"]["properties"]["confidence"]
            .as_object()
            .unwrap();
        assert_eq!(sp_conf["minimum"], 0);
        assert_eq!(sp_conf["maximum"], 1);
    }

    // ── Markdown fence tests ──────────────────────────────────────────────

    #[test]
    fn unwrap_markdown_json_fence_json_tag() {
        let input = "```json\n{\"hello\": true}\n```";
        let result = unwrap_markdown_json_fence(input).unwrap();
        assert_eq!(result.trim(), "{\"hello\": true}");
    }

    #[test]
    fn unwrap_markdown_json_fence_generic_tag() {
        let input = "```\n{\"hello\": true}\n```";
        let result = unwrap_markdown_json_fence(input).unwrap();
        assert_eq!(result.trim(), "{\"hello\": true}");
    }

    #[test]
    fn unwrap_markdown_json_fence_no_fence_returns_content() {
        let input = "{\"hello\": true}";
        let result = unwrap_markdown_json_fence(input).unwrap();
        assert_eq!(result.trim(), "{\"hello\": true}");
    }

    #[test]
    fn unwrap_markdown_json_fence_empty_fence_fails() {
        let input = "```\n```";
        let result = unwrap_markdown_json_fence(input);
        assert!(result.is_err());
    }

    #[test]
    fn unwrap_markdown_json_fence_prose_rejected() {
        // Prose with a fence inside — only the fence is removed, prose remains.
        let input = "Here is the result:\n```json\n{\"hello\": true}\n```\nDone.";
        let result = unwrap_markdown_json_fence(input).unwrap();
        // The result still contains prose — JSON parsing will fail later.
        assert!(result.contains("Here is the result"));
    }

    // ── Response extraction tests ─────────────────────────────────────────

    #[test]
    fn extract_direct_classification_object() {
        let root = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"name": "squirrel", "confidence": 0.8}],
            "overall_confidence": 0.8,
            "summary": "A squirrel",
            "uncertainties": []
        });
        let result = extract_classification_value(root).unwrap();
        assert!(result.is_object());
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn extract_output_parsed_object() {
        let root = serde_json::json!({
            "output_parsed": {
                "contains_animal": true,
                "contains_wildlife": false,
                "is_interesting": false,
                "species": [],
                "overall_confidence": 0.5,
                "summary": "A cat",
                "uncertainties": []
            }
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_wildlife"], false);
    }

    #[test]
    fn extract_output_parsed_string() {
        let root = serde_json::json!({
            "output_parsed": "{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [], \"overall_confidence\": 0.5, \"summary\": \"A cat\", \"uncertainties\": []}"
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_wildlife"], false);
    }

    #[test]
    fn extract_output_parsed_string_with_markdown_fence() {
        let root = serde_json::json!({
            "output_parsed": "```json\n{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [], \"overall_confidence\": 0.5, \"summary\": \"A cat\", \"uncertainties\": []}\n```"
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_wildlife"], false);
    }

    #[test]
    fn extract_choices_message_content() {
        let root = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "{\"contains_animal\": true, \"contains_wildlife\": true, \"is_interesting\": true, \"species\": [], \"overall_confidence\": 0.9, \"summary\": \"A deer\", \"uncertainties\": []}"
                }
            }]
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn extract_choices_message_content_markdown_fence() {
        let root = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "```json\n{\"contains_animal\": true, \"contains_wildlife\": true, \"is_interesting\": true, \"species\": [], \"overall_confidence\": 0.9, \"summary\": \"A deer\", \"uncertainties\": []}\n```"
                }
            }]
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn extract_choices_message_parsed_object() {
        let root = serde_json::json!({
            "choices": [{
                "message": {
                    "parsed": {
                        "contains_animal": true,
                        "contains_wildlife": true,
                        "is_interesting": true,
                        "species": [],
                        "overall_confidence": 0.9,
                        "summary": "A deer",
                        "uncertainties": []
                    }
                }
            }]
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn extract_choices_message_parsed_string() {
        let root = serde_json::json!({
            "choices": [{
                "message": {
                    "parsed": "{\"contains_animal\": true, \"contains_wildlife\": true, \"is_interesting\": true, \"species\": [], \"overall_confidence\": 0.9, \"summary\": \"A deer\", \"uncertainties\": []}"
                }
            }]
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn extract_choices_message_parsed_string_with_fence() {
        let root = serde_json::json!({
            "choices": [{
                "message": {
                    "parsed": "```json\n{\"contains_animal\": true, \"contains_wildlife\": true, \"is_interesting\": true, \"species\": [], \"overall_confidence\": 0.9, \"summary\": \"A deer\", \"uncertainties\": []}\n```"
                }
            }]
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn extract_empty_content_fails() {
        let root = serde_json::json!({
            "choices": [{
                "message": {
                    "content": ""
                }
            }]
        });
        let result = extract_classification_value(root);
        assert!(result.is_err());
    }

    #[test]
    fn extract_missing_choices_fails() {
        let root = serde_json::json!({});
        let result = extract_classification_value(root);
        assert!(result.is_err());
    }

    #[test]
    fn extract_extra_fields_preserved() {
        let root = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.9,
            "summary": "A deer",
            "uncertainties": [],
            "extra_field": "extra_value"
        });
        let result = extract_classification_value(root).unwrap();
        assert_eq!(result["extra_field"], "extra_value");
    }

    // ── Validation tests ──────────────────────────────────────────────────

    #[test]
    fn validate_valid_classification() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"name": "squirrel", "confidence": 0.82}],
            "overall_confidence": 0.82,
            "summary": "A squirrel on a fence",
            "uncertainties": ["distance uncertain"]
        });
        let (classification, json) = validate_classification(value).unwrap();
        assert!(classification.contains_animal);
        assert!(classification.contains_wildlife);
        assert!(!classification.uncertainties.is_empty());
        assert!(!json.is_empty());
    }

    #[test]
    fn validate_missing_contains_animal_fails() {
        let value = serde_json::json!({
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("contains_animal"));
    }

    #[test]
    fn validate_wrong_type_contains_animal_fails() {
        let value = serde_json::json!({
            "contains_animal": "true",
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("boolean"));
    }

    #[test]
    fn validate_wrong_type_species_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": "not-an-array",
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("species"));
    }

    #[test]
    fn validate_invalid_species_item_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"confidence": 0.5}],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("name"));
    }

    #[test]
    fn validate_negative_confidence_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": -0.1,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("0.0"));
    }

    #[test]
    fn validate_confidence_above_one_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 1.1,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("1.0"));
    }

    #[test]
    fn validate_non_finite_confidence_fails() {
        // Use 2.0 which also exercises the range check.
        let mut obj = serde_json::Map::new();
        obj.insert("contains_animal".to_string(), serde_json::json!(true));
        obj.insert("contains_wildlife".to_string(), serde_json::json!(true));
        obj.insert("is_interesting".to_string(), serde_json::json!(true));
        obj.insert("species".to_string(), serde_json::json!([]));
        obj.insert("overall_confidence".to_string(), serde_json::json!(2.0));
        obj.insert("summary".to_string(), serde_json::json!("test"));
        obj.insert("uncertainties".to_string(), serde_json::json!([]));
        let value = serde_json::Value::Object(obj);
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("finite") || err.message.contains("0.0"),
            "expected confidence range error, got: {}",
            err.message
        );
    }

    #[test]
    fn validate_species_negative_confidence_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"name": "test", "confidence": -0.1}],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("confidence"));
    }

    #[test]
    fn validate_species_above_one_confidence_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"name": "test", "confidence": 1.5}],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("confidence"));
    }

    #[test]
    fn validate_missing_summary_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.5,
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("summary"));
    }

    #[test]
    fn validate_summary_wrong_type_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.5,
            "summary": 42,
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("summary"));
    }

    #[test]
    fn validate_uncertainties_wrong_type_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": [42]
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("uncertainties"));
    }

    #[test]
    fn validate_extra_fields_preserved_in_json() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": [],
            "model_notes": "some extra info"
        });
        let (_, json) = validate_classification(value).unwrap();
        assert!(json.contains("model_notes"));
        assert!(json.contains("some extra info"));
    }

    #[test]
    fn validate_non_object_fails() {
        let value = serde_json::json!("just a string");
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("object"));
    }

    #[test]
    fn validate_species_null_confidence_fails() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": true,
            "is_interesting": true,
            "species": [{"name": "test", "confidence": null}],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = validate_classification(value);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("confidence"));
    }

    // ── Redaction tests ───────────────────────────────────────────────────

    #[test]
    fn classifier_error_debug_no_secrets() {
        let _secret = Secret::new("SENTINEL-API-KEY".to_string());
        let app_err = AppError::new(
            ErrorCategory::ClassifierTransport,
            "classify_jpeg",
            "network error",
        );
        let err = ClassifierError {
            error: app_err,
            disposition: RetryDisposition::Retryable,
            raw_response: Some("raw body content SENTINEL-RAW".to_string()),
            retry_after: None,
        };

        let debug_output = format!("{err:?}");
        assert!(
            !debug_output.contains("SENTINEL-API-KEY"),
            "secret leaked in Debug: {debug_output}"
        );
        assert!(
            !debug_output.contains("SENTINEL-RAW"),
            "raw response leaked in Debug: {debug_output}"
        );
    }

    #[test]
    fn classifier_error_display_no_secrets() {
        let app_err = AppError::new(
            ErrorCategory::ClassifierTransport,
            "classify_jpeg",
            "network error",
        );
        let err = ClassifierError {
            error: app_err,
            disposition: RetryDisposition::Retryable,
            raw_response: Some("raw body content".to_string()),
            retry_after: None,
        };

        let display_output = format!("{err}");
        assert!(display_output.contains("retryable"));
        assert!(display_output.contains("network error"));
    }

    #[test]
    fn classifier_error_debug_no_base64() {
        let app_err = AppError::new(
            ErrorCategory::ClassifierTransport,
            "classify_jpeg",
            "network error",
        );
        let err = ClassifierError {
            error: app_err,
            disposition: RetryDisposition::Retryable,
            raw_response: None,
            retry_after: None,
        };

        let debug_output = format!("{err:?}");
        assert!(
            !debug_output.contains("SENTINEL"),
            "no sentinel should appear: {debug_output}"
        );
    }

    #[test]
    fn classifier_output_debug_no_raw_response() {
        let classification = WildlifeClassification {
            contains_animal: true,
            contains_wildlife: false,
            is_interesting: false,
            species: vec![SpeciesPrediction {
                name: "cat".to_string(),
                confidence: 0.9,
            }],
            overall_confidence: 0.9,
            summary: "A cat".to_string(),
            uncertainties: vec![],
        };
        let output = ClassifierOutput {
            classification,
            classification_json: r#"{"extra":"value"}"#.to_string(),
            raw_response: "raw body content SENTINEL-RAW-RESPONSE".to_string(),
        };
        let debug_output = format!("{output:?}");
        // Raw response must NOT appear.
        assert!(
            !debug_output.contains("SENTINEL-RAW-RESPONSE"),
            "raw response leaked in ClassifierOutput Debug: {debug_output}"
        );
        // classification_json content must NOT appear.
        assert!(
            !debug_output.contains("extra"),
            "classification_json leaked in ClassifierOutput Debug: {debug_output}"
        );
        // Should report lengths instead.
        assert!(debug_output.contains("classification_json_len"));
        assert!(debug_output.contains("raw_response_len"));
    }

    /// ClassifierOutput Debug must not expose model-generated content
    /// such as summary, species names, or uncertainties.
    #[test]
    fn classifier_output_debug_no_model_content() {
        let classification = WildlifeClassification {
            contains_animal: true,
            contains_wildlife: true,
            is_interesting: true,
            species: vec![SpeciesPrediction {
                name: "SENTINEL-SPECIES-LEAK".to_string(),
                confidence: 0.9,
            }],
            overall_confidence: 0.9,
            summary: "SENTINEL-SUMMARY-LEAK".to_string(),
            uncertainties: vec!["SENTINEL-UNCERTAINTY-LEAK".to_string()],
        };
        let output = ClassifierOutput {
            classification,
            classification_json: r#"{"extra":"value"}"#.to_string(),
            raw_response: "raw body content".to_string(),
        };
        let debug_output = format!("{output:?}");
        assert!(
            !debug_output.contains("SENTINEL-SPECIES-LEAK"),
            "species name leaked in ClassifierOutput Debug: {debug_output}"
        );
        assert!(
            !debug_output.contains("SENTINEL-SUMMARY-LEAK"),
            "summary leaked in ClassifierOutput Debug: {debug_output}"
        );
        assert!(
            !debug_output.contains("SENTINEL-UNCERTAINTY-LEAK"),
            "uncertainty leaked in ClassifierOutput Debug: {debug_output}"
        );
        // Should report counts instead.
        assert!(debug_output.contains("species_count"));
        assert!(debug_output.contains("uncertainties_count"));
    }

    // ── ClassifierClient construction tests ─────────────────────────────────

    #[test]
    fn from_config_without_endpoints_fails() {
        let config = ClassifierConfig {
            endpoints: Vec::new(),
            poll_interval_seconds: 10,
            retry_limit: 5,
            retry_initial_delay_seconds: 10,
            retry_max_delay_seconds: 300,
            processing_lease_seconds: 600,
        };
        let result = ClassifierClient::from_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("at least one classifier endpoint"));
    }

    #[test]
    fn build_classifier_url_preserves_base_path() {
        let base = Url::parse("http://example.com/v1").unwrap();
        let url = build_classifier_url(&base, "/chat/completions").unwrap();
        assert!(url.as_str().contains("/v1/chat/completions"));
    }

    #[test]
    fn build_classifier_url_rejects_query_params() {
        let base = Url::parse("http://example.com/v1").unwrap();
        let result = build_classifier_url(&base, "/chat/completions?foo=bar");
        assert!(result.is_err());
    }

    #[test]
    fn build_classifier_url_rejects_fragment() {
        let base = Url::parse("http://example.com/v1").unwrap();
        let result = build_classifier_url(&base, "/chat/completions#top");
        assert!(result.is_err());
    }

    #[test]
    fn build_classifier_url_rejects_host_change() {
        let base = Url::parse("http://example.com/v1").unwrap();
        let result = build_classifier_url(&base, "//evil.com/path");
        assert!(result.is_err());
    }

    #[test]
    fn build_classifier_url_rejects_credentials() {
        let base = Url::parse("http://example.com/v1").unwrap();
        let result = build_classifier_url(&base, "/chat/@user:pass/completions");
        assert!(result.is_err());
    }

    // ── Prompt version tests ──────────────────────────────────────────────

    #[test]
    fn system_prompt_includes_version() {
        let prompt = system_prompt("wildlife-v2");
        assert!(prompt.contains("wildlife-v2"));
    }

    #[test]
    fn user_prompt_includes_version() {
        let prompt = user_prompt("wildlife-v2");
        assert!(prompt.contains("wildlife-v2"));
    }

    // ── Status categorization tests ───────────────────────────────────────

    #[test]
    fn http_401_is_authentication() {
        assert_eq!(classify_status_category(401), ErrorCategory::Authentication);
    }

    #[test]
    fn http_403_is_authorization() {
        assert_eq!(classify_status_category(403), ErrorCategory::Authorization);
    }

    #[test]
    fn http_408_is_classifier_transport() {
        assert_eq!(
            classify_status_category(408),
            ErrorCategory::ClassifierTransport
        );
    }

    #[test]
    fn http_429_is_classifier_transport() {
        assert_eq!(
            classify_status_category(429),
            ErrorCategory::ClassifierTransport
        );
    }

    #[test]
    fn http_500_is_classifier_transport() {
        assert_eq!(
            classify_status_category(500),
            ErrorCategory::ClassifierTransport
        );
    }

    #[test]
    fn http_400_is_classifier_transport() {
        assert_eq!(
            classify_status_category(400),
            ErrorCategory::ClassifierTransport
        );
    }

    #[test]
    fn http_401_is_permanent() {
        assert_eq!(
            classify_status_disposition(401),
            RetryDisposition::Permanent
        );
    }

    #[test]
    fn http_403_is_permanent() {
        assert_eq!(
            classify_status_disposition(403),
            RetryDisposition::Permanent
        );
    }

    #[test]
    fn http_408_is_retryable() {
        assert_eq!(
            classify_status_disposition(408),
            RetryDisposition::Retryable
        );
    }

    #[test]
    fn http_429_is_retryable() {
        assert_eq!(
            classify_status_disposition(429),
            RetryDisposition::Retryable
        );
    }

    #[test]
    fn http_500_is_retryable() {
        assert_eq!(
            classify_status_disposition(500),
            RetryDisposition::Retryable
        );
    }

    #[test]
    fn http_400_is_permanent() {
        assert_eq!(
            classify_status_disposition(400),
            RetryDisposition::Permanent
        );
    }

    #[test]
    fn http_502_is_retryable() {
        assert_eq!(
            classify_status_disposition(502),
            RetryDisposition::Retryable
        );
    }

    // ── Shared decoder tests ──────────────────────────────────────────────

    #[test]
    fn decode_structured_candidate_object() {
        let value = serde_json::json!({
            "contains_animal": true,
            "contains_wildlife": false,
            "is_interesting": false,
            "species": [],
            "overall_confidence": 0.5,
            "summary": "test",
            "uncertainties": []
        });
        let result = decode_structured_candidate(&value).unwrap();
        assert!(result.is_object());
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn decode_structured_candidate_string() {
        let value = serde_json::json!(
            "{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [], \"overall_confidence\": 0.5, \"summary\": \"test\", \"uncertainties\": []}"
        );
        let result = decode_structured_candidate(&value).unwrap();
        assert!(result.is_object());
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn decode_structured_candidate_string_with_fence() {
        let value = serde_json::json!(
            "```json\n{\"contains_animal\": true, \"contains_wildlife\": false, \"is_interesting\": false, \"species\": [], \"overall_confidence\": 0.5, \"summary\": \"test\", \"uncertainties\": []}\n```"
        );
        let result = decode_structured_candidate(&value).unwrap();
        assert!(result.is_object());
        assert_eq!(result["contains_animal"], true);
    }

    #[test]
    fn decode_structured_candidate_empty_string_fails() {
        let value = serde_json::json!("");
        let result = decode_structured_candidate(&value);
        assert!(result.is_err());
    }

    #[test]
    fn decode_structured_candidate_number_fails() {
        let value = serde_json::json!(42);
        let result = decode_structured_candidate(&value);
        assert!(result.is_err());
    }
}
