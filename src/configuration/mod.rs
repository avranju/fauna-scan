//! Typed TOML configuration loading, XDG path resolution, secret handling,
//! and validation.
//!
//! Implemented in Phase 2.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono::{FixedOffset, Timelike};
use serde::Deserialize;
use url::Url;

use crate::cli::LogLevel;
use crate::domain::Timestamp;
use crate::error::{AppError, AppResult, ErrorCategory};

pub use paths::{
    XdgPaths, resolve_real_xdg, resolve_real_xdg_config_path, resolve_real_xdg_state_path,
    validate_database_and_output_paths, validate_filesystem_paths,
};
pub use secret::{Secret, SecretSource};

mod paths;
mod secret;

// ── Database configuration ───────────────────────────────────────────────

/// Resolved database backend configuration.
#[derive(Debug, Clone)]
pub enum DatabaseConfig {
    /// SQLite backend with a local file path.
    Sqlite { path: PathBuf, max_connections: u32 },
    /// PostgreSQL backend with a connection URL.
    #[cfg(feature = "postgres")]
    Postgres { url: Secret, max_connections: u32 },
}

/// Raw database configuration fields from TOML.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawDatabaseConfig {
    pub backend: Option<String>,
    pub path: Option<String>,
    pub max_connections: Option<u32>,
    pub url: Option<String>,
    pub url_file: Option<PathBuf>,
    pub url_env: Option<String>,
}

impl RawDatabaseConfig {
    /// Resolve raw database config into a validated `DatabaseConfig`.
    pub fn resolve<F>(
        self,
        legacy_database_path: Option<&str>,
        get_env: &F,
        xdg_state: &Path,
    ) -> AppResult<DatabaseConfig>
    where
        F: Fn(&str) -> Option<String>,
    {
        let backend = self.backend.as_deref().unwrap_or("sqlite");
        let max_connections = self.max_connections.unwrap_or(8);
        if max_connections == 0 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "resolve_database_config",
                "database.max_connections must be greater than zero",
            ));
        }

        match backend {
            "sqlite" | "" => {
                // Resolve path: explicit > legacy > XDG default
                let path = match self.path {
                    Some(p) => {
                        if legacy_database_path.is_some() {
                            return Err(AppError::new(
                                ErrorCategory::Configuration,
                                "resolve_database_config",
                                "cannot specify both [database].path and general.database_path",
                            ));
                        }
                        PathBuf::from(p)
                    }
                    None => {
                        if let Some(legacy) = legacy_database_path {
                            PathBuf::from(legacy)
                        } else {
                            // resolve_real_xdg_state_path already includes the filename
                            xdg_state.to_path_buf()
                        }
                    }
                };
                Ok(DatabaseConfig::Sqlite {
                    path,
                    max_connections,
                })
            }
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => {
                // PostgreSQL requires exactly one URL source
                let url_literal = self.url;
                let url_file = self.url_file;
                let url_env = self.url_env;
                let sources = [url_literal.is_some(), url_file.is_some(), url_env.is_some()]
                    .iter()
                    .filter(|&&b| b)
                    .count();
                if sources == 0 {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "PostgreSQL requires exactly one of: url, url_file, or url_env",
                    ));
                }
                if sources > 1 {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "PostgreSQL may only specify one URL source (url, url_file, or url_env)",
                    ));
                }
                // Reject SQLite-specific fields for PostgreSQL
                if self.path.is_some() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "PostgreSQL must not use [database].path",
                    ));
                }
                if legacy_database_path.is_some() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "PostgreSQL must not use general.database_path",
                    ));
                }
                let url_str = match (url_literal, url_file, url_env) {
                    (Some(url), None, None) => url,
                    (None, Some(file), None) => std::fs::read_to_string(&file).map_err(|e| {
                        AppError::with_source(
                            ErrorCategory::Configuration,
                            "resolve_database_config",
                            format!("cannot read url_file {}: {e}", file.display()),
                            e,
                        )
                    })?,
                    (None, None, Some(env)) => get_env(&env).ok_or_else(|| {
                        AppError::new(
                            ErrorCategory::Configuration,
                            "resolve_database_config",
                            format!("url_env variable '{}' is not set", env),
                        )
                    })?,
                    _ => unreachable!(),
                };
                let url_str = url_str.trim().to_string();
                let parsed = url_str.parse::<Url>().map_err(|_| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "database.url is not a valid URL",
                    )
                })?;
                if parsed.scheme() != "postgres" && parsed.scheme() != "postgresql" {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "database.url scheme must be postgres or postgresql",
                    ));
                }
                if parsed.host().is_none() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "database.url must include a host",
                    ));
                }
                if parsed.path().is_empty() || parsed.path() == "/" {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_database_config",
                        "database.url must include a database name",
                    ));
                }
                let secret = Secret::new(url_str);
                Ok(DatabaseConfig::Postgres {
                    url: secret,
                    max_connections,
                })
            }
            _ => Err(AppError::new(
                ErrorCategory::Configuration,
                "resolve_database_config",
                format!(
                    "unknown database backend: {backend} (expected sqlite, postgres, or postgresql)"
                ),
            )),
        }
    }
}

// ── Resolved configuration (returned by Config::load) ──────────────────────

/// Fully resolved and validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// General application settings.
    pub general: GeneralConfig,
    /// Database backend configuration.
    pub database: DatabaseConfig,
    /// NVR connection and operational settings.
    pub nvr: NvrConfig,
    /// Classifier (vision LLM) settings.
    pub classifier: ClassifierConfig,
    /// Optional local API server settings.
    pub web: WebConfig,
    /// The configuration file path that was loaded.
    pub source_path: PathBuf,
}

/// Resolved API server settings.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Whether `run` should serve the API.
    pub enabled: bool,
    /// Address on which the Axum server listens.
    pub listen_address: SocketAddr,
    /// Seconds included before an image timestamp in recording searches.
    pub clip_pre_roll_seconds: u64,
    /// Seconds included after an image timestamp in recording searches.
    pub clip_post_roll_seconds: u64,
    /// Maximum total recording-search interval accepted from an API client.
    pub maximum_clip_duration_seconds: u64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_address: "127.0.0.1:8787".parse().expect("static socket address"),
            clip_pre_roll_seconds: 10,
            clip_post_roll_seconds: 20,
            maximum_clip_duration_seconds: 120,
        }
    }
}

/// Resolved general settings.
#[derive(Debug, Clone)]
pub struct GeneralConfig {
    /// Directory for downloaded images.
    pub output_directory: PathBuf,
    /// Application log level.
    pub log_level: LogLevel,
    /// Number of days to retain images classified as not containing wildlife.
    /// Images older than this duration (measured from capture time) and
    /// classified as no-wildlife will be garbage collected.
    /// Images with any wildlife-positive classification are never collected.
    pub non_wildlife_image_retention_days: u64,
}

/// Resolved NVR settings.
#[derive(Debug, Clone)]
pub struct NvrConfig {
    /// NVR scheme (http or https).
    pub scheme: String,
    /// NVR hostname or IP address.
    pub host: String,
    /// NVR port.
    pub port: u16,
    /// NVR username.
    pub username: String,
    /// NVR password (redacted in Debug/Display).
    pub password: Option<Secret>,
    /// Normalized UTC start timestamp.
    pub start_at: Timestamp,
    /// Request timeout in seconds.
    pub request_timeout_seconds: u64,
    /// Connect timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Allow invalid TLS certificates.
    pub allow_invalid_tls_certificates: bool,
    /// Search settings.
    pub search: NvrSearchConfig,
    /// Download settings.
    pub download: NvrDownloadConfig,
}

/// Resolved NVR search settings.
#[derive(Debug, Clone)]
pub struct NvrSearchConfig {
    /// Optional daily capture-time filter applied to NVR search results.
    ///
    /// A matching image is accepted only when its capture start time falls in
    /// this half-open interval in the configured fixed UTC offset.
    pub capture_time_window: Option<CaptureTimeWindow>,
    /// Search window size in minutes.
    pub window_minutes: u64,
    /// Maximum results per page.
    pub max_results: u64,
    /// Polling interval in seconds.
    pub poll_interval_seconds: u64,
    /// Overlap with previous window in seconds.
    pub poll_overlap_seconds: u64,
    /// Camera refresh interval in seconds.
    pub camera_refresh_interval_seconds: u64,
    /// Settlement delay in seconds.
    pub settlement_delay_seconds: u64,
}

/// A daily, half-open capture-time interval used to filter NVR search results.
///
/// The interval is evaluated using `utc_offset`, which should match the NVR's
/// configured clock. Equal start and end times are rejected because an
/// all-day window would be ambiguous; omit this setting to disable filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTimeWindow {
    start_seconds: u32,
    end_seconds: u32,
    utc_offset: FixedOffset,
}

impl CaptureTimeWindow {
    /// Return whether an image captured at `capture_start_at` belongs to this
    /// window. Start is inclusive and end is exclusive. A start later than
    /// end denotes an overnight window (for example, 19:00–07:00).
    pub fn contains(&self, capture_start_at: &Timestamp) -> bool {
        let local_time = capture_start_at
            .as_datetime()
            .with_timezone(&self.utc_offset)
            .time();
        let seconds = local_time.num_seconds_from_midnight();

        if self.start_seconds < self.end_seconds {
            self.start_seconds <= seconds && seconds < self.end_seconds
        } else {
            seconds >= self.start_seconds || seconds < self.end_seconds
        }
    }
}

/// Resolved NVR download settings.
#[derive(Debug, Clone)]
pub struct NvrDownloadConfig {
    /// Maximum retry attempts.
    pub retry_limit: u32,
    /// Initial retry delay in seconds.
    pub retry_initial_delay_seconds: u32,
    /// Maximum retry delay in seconds.
    pub retry_max_delay_seconds: u32,
    /// Maximum image size in bytes.
    pub maximum_image_size_bytes: u64,
    /// Verify JPEG signature on download.
    pub verify_jpeg: bool,
    /// Rebase playback URLs to the configured NVR origin.
    pub rebase_playback_urls: bool,
    /// Bounded download concurrency.
    pub concurrency: usize,
    /// Playback-host allowlist for rebasing-disabled mode.
    pub playback_host_allowlist: Vec<String>,
}

/// Resolved classifier settings.
#[derive(Debug, Clone)]
pub struct ClassifierConfig {
    /// Configured classifier endpoints. An empty list disables classification.
    pub endpoints: Vec<ClassifierEndpointConfig>,
    /// Poll interval in seconds.
    pub poll_interval_seconds: u64,
    /// Maximum retry attempts.
    pub retry_limit: u32,
    /// Initial retry delay in seconds.
    pub retry_initial_delay_seconds: u32,
    /// Maximum retry delay in seconds.
    pub retry_max_delay_seconds: u32,
    /// Processing lease duration in seconds.
    pub processing_lease_seconds: u64,
}

/// Resolved classifier generation settings.
#[derive(Debug, Clone)]
pub struct ClassifierGenerationConfig {
    /// Temperature for generation.
    pub temperature: f32,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
}

/// Provider quota policy for a classifier endpoint.
///
/// Token budgets are reserved before a request using `estimated_input_tokens_per_request`
/// plus the endpoint's `generation.max_tokens`; this is intentionally
/// conservative so the service cannot overshoot a provider quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierRateLimitConfig {
    /// Stable identifier for the provider/account/model quota shared by endpoints.
    pub quota_group: String,
    pub requests_per_minute: u64,
    pub requests_per_day: u64,
    pub tokens_per_minute: u64,
    pub tokens_per_day: u64,
    pub estimated_input_tokens_per_request: u64,
    pub max_images_per_request: u32,
}

/// A fully resolved classifier endpoint.
#[derive(Debug, Clone)]
pub struct ClassifierEndpointConfig {
    /// Whether this endpoint participates in classification request rotation.
    pub enabled: bool,
    /// Optional provider quota policy. Endpoints sharing a quota group share
    /// one budget even when their transport URLs differ.
    pub rate_limit: Option<ClassifierRateLimitConfig>,
    /// Base URL of the classifier API.
    pub base_url: Url,
    /// API endpoint path.
    pub endpoint: String,
    /// Model name.
    pub model: String,
    /// API key (redacted in Debug/Display).
    pub api_key: Option<Secret>,
    /// Basic auth username.
    pub username: String,
    /// Basic auth password (redacted in Debug/Display).
    pub password: Option<Secret>,
    /// Request timeout in seconds.
    pub request_timeout_seconds: u64,
    /// Prompt version identifier.
    pub prompt_version: String,
    /// Generation settings.
    pub generation: ClassifierGenerationConfig,
}

// ── Raw TOML input models ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    general: RawGeneralConfig,
    #[serde(default)]
    database: RawDatabaseConfig,
    #[serde(default)]
    nvr: RawNvrConfig,
    #[serde(default)]
    classifier: RawClassifierConfig,
    #[serde(default)]
    web: RawWebConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawWebConfig {
    enabled: Option<bool>,
    listen_address: Option<String>,
    clip_pre_roll_seconds: Option<u64>,
    clip_post_roll_seconds: Option<u64>,
    maximum_clip_duration_seconds: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawGeneralConfig {
    database_path: Option<String>,
    output_directory: Option<String>,
    log_level: Option<String>,
    non_wildlife_image_retention_days: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawNvrConfig {
    scheme: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
    password_file: Option<PathBuf>,
    password_env: Option<String>,
    start_at: Option<String>,
    request_timeout_seconds: Option<u64>,
    connect_timeout_seconds: Option<u64>,
    allow_invalid_tls_certificates: Option<bool>,
    search: Option<RawNvrSearchConfig>,
    download: Option<RawNvrDownloadConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawNvrSearchConfig {
    capture_time_window: Option<RawCaptureTimeWindow>,
    window_minutes: Option<u64>,
    max_results: Option<u64>,
    poll_interval_seconds: Option<u64>,
    poll_overlap_seconds: Option<u64>,
    camera_refresh_interval_seconds: Option<u64>,
    settlement_delay_seconds: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawCaptureTimeWindow {
    start_time: Option<String>,
    end_time: Option<String>,
    utc_offset: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawNvrDownloadConfig {
    retry_limit: Option<u32>,
    retry_initial_delay_seconds: Option<u32>,
    retry_max_delay_seconds: Option<u32>,
    maximum_image_size_bytes: Option<u64>,
    verify_jpeg: Option<bool>,
    rebase_playback_urls: Option<bool>,
    concurrency: Option<usize>,
    playback_host_allowlist: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawClassifierConfig {
    poll_interval_seconds: Option<u64>,
    retry_limit: Option<u32>,
    retry_initial_delay_seconds: Option<u32>,
    retry_max_delay_seconds: Option<u32>,
    processing_lease_seconds: Option<u64>,
    endpoints: Vec<RawClassifierEndpointConfig>,
}

/// Per-endpoint settings for `[[classifier.endpoints]]`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawClassifierEndpointConfig {
    enabled: Option<bool>,
    base_url: Option<String>,
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
    username: Option<String>,
    password: Option<String>,
    password_file: Option<PathBuf>,
    password_env: Option<String>,
    request_timeout_seconds: Option<u64>,
    prompt_version: Option<String>,
    generation: Option<RawClassifierGenerationConfig>,
    rate_limit: Option<RawClassifierRateLimitConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawClassifierRateLimitConfig {
    quota_group: Option<String>,
    requests_per_minute: Option<u64>,
    requests_per_day: Option<u64>,
    tokens_per_minute: Option<u64>,
    tokens_per_day: Option<u64>,
    estimated_input_tokens_per_request: Option<u64>,
    max_images_per_request: Option<u32>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawClassifierGenerationConfig {
    temperature: Option<f32>,
    max_tokens: Option<u32>,
}

// ── Config::load ───────────────────────────────────────────────────────────

impl Config {
    /// Load configuration from the given path, or the XDG default if `None`.
    ///
    /// Reads the TOML file, deserializes it, applies defaults, resolves
    /// secrets, validates all fields, and checks filesystem paths.
    pub fn load(path: Option<&Path>) -> AppResult<Self> {
        // Select config path — resolve XDG config path only when no explicit
        // path is provided.
        let config_path_buf: PathBuf = match path {
            Some(p) => p.to_path_buf(),
            None => resolve_real_xdg_config_path()?,
        };
        let config_path = config_path_buf.as_path();

        // Read and parse TOML
        let contents = std::fs::read_to_string(config_path).map_err(|e| {
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                format!(
                    "cannot read configuration file {}: {}",
                    config_path.display(),
                    safe_io_message(&e)
                ),
            )
        })?;

        let raw: RawConfig = toml::from_str(&contents).map_err(|e| {
            // Construct a sanitized TOML diagnostic that omits source-line
            // contents (no raw source snippets, which could contain secrets).
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                safe_toml_diagnostic(&contents, &e),
            )
        })?;

        // Resolve secrets using the real environment
        let get_env = |name: &str| std::env::var(name).ok();

        // ── Resolve database ───────────────────────────────────────────────
        // Determine backend and whether we need XDG state for the default path.
        // Only resolve XDG state when the backend is SQLite AND no explicit or
        // legacy database path is configured.
        let backend_hint = raw.database.backend.as_deref().unwrap_or("sqlite");
        let has_explicit_path = raw.database.path.is_some();
        let has_legacy_path = raw.general.database_path.is_some();
        let needs_xdg_state = (backend_hint == "sqlite" || backend_hint.is_empty())
            && !has_explicit_path
            && !has_legacy_path;
        let xdg_state = if needs_xdg_state {
            resolve_real_xdg_state_path()?
        } else {
            // For PostgreSQL or when an explicit/legacy path is set, XDG state
            // is not needed; provide a dummy path so resolve() can still run.
            PathBuf::from("/dev/null")
        };
        let database =
            raw.database
                .resolve(raw.general.database_path.as_deref(), &get_env, &xdg_state)?;

        let output_directory = raw.general.output_directory.as_ref().ok_or_else(|| {
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                "general.output_directory is required",
            )
        })?;
        let output_directory = PathBuf::from(output_directory);

        let log_level = parse_log_level(raw.general.log_level.as_deref())?;

        let non_wildlife_image_retention_days =
            raw.general.non_wildlife_image_retention_days.unwrap_or(4);

        let general = GeneralConfig {
            output_directory: output_directory.clone(),
            log_level,
            non_wildlife_image_retention_days,
        };

        // ── Resolve NVR ────────────────────────────────────────────────────
        // Default scheme to "http" when omitted.
        let nvr_scheme = raw.nvr.scheme.as_deref().unwrap_or("http");

        let nvr_host = raw.nvr.host.as_deref().ok_or_else(|| {
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                "nvr.host is required",
            )
        })?;

        let nvr_port = raw.nvr.port.ok_or_else(|| {
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                "nvr.port is required",
            )
        })?;

        let nvr_username = raw.nvr.username.clone().unwrap_or_default();

        // NVR password secret source — always validate source count
        let nvr_password_source = SecretSource {
            literal: raw.nvr.password.clone(),
            file: raw.nvr.password_file.clone(),
            environment: raw.nvr.password_env.clone(),
        };
        let nvr_password = nvr_password_source.resolve("nvr.password", get_env)?;

        let start_at = raw
            .nvr
            .start_at
            .as_deref()
            .ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Configuration,
                    "load_config",
                    "nvr.start_at is required",
                )
            })?
            .parse::<Timestamp>()
            .map_err(|_| {
                AppError::new(
                    ErrorCategory::Configuration,
                    "load_config",
                    "nvr.start_at is not a valid RFC 3339 timestamp",
                )
            })?;

        let request_timeout = raw.nvr.request_timeout_seconds.unwrap_or(30);
        let connect_timeout = raw.nvr.connect_timeout_seconds.unwrap_or(10);
        let allow_invalid_tls = raw.nvr.allow_invalid_tls_certificates.unwrap_or(false);

        // NVR search
        let search_raw = raw.nvr.search.unwrap_or_default();
        let capture_time_window = search_raw
            .capture_time_window
            .as_ref()
            .map(resolve_capture_time_window)
            .transpose()?;
        let search = NvrSearchConfig {
            capture_time_window,
            window_minutes: search_raw.window_minutes.unwrap_or(60),
            max_results: search_raw.max_results.unwrap_or(50),
            poll_interval_seconds: search_raw.poll_interval_seconds.unwrap_or(60),
            poll_overlap_seconds: search_raw.poll_overlap_seconds.unwrap_or(120),
            camera_refresh_interval_seconds: search_raw
                .camera_refresh_interval_seconds
                .unwrap_or(3600),
            settlement_delay_seconds: search_raw.settlement_delay_seconds.unwrap_or(10),
        };

        // NVR download
        let download_raw = raw.nvr.download.unwrap_or_default();
        let download = NvrDownloadConfig {
            retry_limit: download_raw.retry_limit.unwrap_or(10),
            retry_initial_delay_seconds: download_raw.retry_initial_delay_seconds.unwrap_or(5),
            retry_max_delay_seconds: download_raw.retry_max_delay_seconds.unwrap_or(300),
            maximum_image_size_bytes: download_raw.maximum_image_size_bytes.unwrap_or(25_000_000),
            verify_jpeg: download_raw.verify_jpeg.unwrap_or(true),
            rebase_playback_urls: download_raw.rebase_playback_urls.unwrap_or(true),
            concurrency: download_raw.concurrency.unwrap_or(2),
            playback_host_allowlist: resolve_playback_host_allowlist(
                &download_raw.playback_host_allowlist,
            )?,
        };

        let nvr = NvrConfig {
            scheme: nvr_scheme.to_string(),
            host: nvr_host.to_string(),
            port: nvr_port,
            username: nvr_username,
            password: nvr_password,
            start_at,
            request_timeout_seconds: request_timeout,
            connect_timeout_seconds: connect_timeout,
            allow_invalid_tls_certificates: allow_invalid_tls,
            search,
            download,
        };

        // ── Resolve classifier ─────────────────────────────────────────────
        let classifier = resolve_classifier(&raw.classifier, &get_env)?;

        let web = WebConfig {
            enabled: raw.web.enabled.unwrap_or(false),
            listen_address: raw
                .web
                .listen_address
                .as_deref()
                .unwrap_or("127.0.0.1:8787")
                .parse::<SocketAddr>()
                .map_err(|_| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "load_config",
                        "web.listen_address must be a valid IP socket address",
                    )
                })?,
            clip_pre_roll_seconds: raw.web.clip_pre_roll_seconds.unwrap_or(10),
            clip_post_roll_seconds: raw.web.clip_post_roll_seconds.unwrap_or(20),
            maximum_clip_duration_seconds: raw.web.maximum_clip_duration_seconds.unwrap_or(120),
        };

        let config = Config {
            general,
            database,
            nvr,
            classifier,
            web,
            source_path: config_path_buf.clone(),
        };

        // ── Validate ───────────────────────────────────────────────────────
        validate_config(&config)?;

        // ── Filesystem validation (non-mutating) ───────────────────────────
        validate_database_and_output_paths(&config.database, &config.general.output_directory)?;

        Ok(config)
    }
}

/// Resolve classifier pool settings and each independently configured endpoint.
fn resolve_classifier<F>(raw: &RawClassifierConfig, get_env: &F) -> AppResult<ClassifierConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let endpoints = raw
        .endpoints
        .iter()
        .enumerate()
        .map(|(index, endpoint)| resolve_classifier_endpoint(endpoint, get_env, index))
        .collect::<AppResult<Vec<_>>>()?;

    Ok(ClassifierConfig {
        endpoints,
        poll_interval_seconds: raw.poll_interval_seconds.unwrap_or(10),
        retry_limit: raw.retry_limit.unwrap_or(5),
        retry_initial_delay_seconds: raw.retry_initial_delay_seconds.unwrap_or(10),
        retry_max_delay_seconds: raw.retry_max_delay_seconds.unwrap_or(300),
        processing_lease_seconds: raw.processing_lease_seconds.unwrap_or(600),
    })
}

/// Resolve one classifier endpoint. Endpoint settings never inherit from
/// another endpoint, so credentials and model selection remain isolated.
fn resolve_classifier_endpoint<F>(
    raw: &RawClassifierEndpointConfig,
    get_env: &F,
    index: usize,
) -> AppResult<ClassifierEndpointConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let label = format!("classifier.endpoints[{index}]");
    let base_url = raw
        .base_url
        .as_deref()
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                format!("{label}.base_url is required"),
            )
        })?
        .parse::<Url>()
        .map_err(|_| {
            AppError::new(
                ErrorCategory::Configuration,
                "load_config",
                format!("{label}.base_url is not a valid URL"),
            )
        })?;

    if !base_url.username().is_empty() || base_url.password().is_some() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.base_url must not contain embedded credentials"),
        ));
    }

    let model = raw.model.clone().ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.model is required"),
        )
    })?;
    let api_key = SecretSource {
        literal: raw.api_key.clone(),
        file: raw.api_key_file.clone(),
        environment: raw.api_key_env.clone(),
    }
    .resolve(&format!("{label}.api_key"), get_env)?;
    let password = SecretSource {
        literal: raw.password.clone(),
        file: raw.password_file.clone(),
        environment: raw.password_env.clone(),
    }
    .resolve(&format!("{label}.password"), get_env)?;
    let generation = raw.generation.clone().unwrap_or_default();
    let rate_limit = raw
        .rate_limit
        .as_ref()
        .map(|limit| {
            let required = |value: Option<u64>, field: &str| {
                value.ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "load_config",
                        format!("{label}.rate_limit.{field} is required"),
                    )
                })
            };
            Ok(ClassifierRateLimitConfig {
                quota_group: limit.quota_group.clone().ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "load_config",
                        format!("{label}.rate_limit.quota_group is required"),
                    )
                })?,
                requests_per_minute: required(limit.requests_per_minute, "requests_per_minute")?,
                requests_per_day: required(limit.requests_per_day, "requests_per_day")?,
                tokens_per_minute: required(limit.tokens_per_minute, "tokens_per_minute")?,
                tokens_per_day: required(limit.tokens_per_day, "tokens_per_day")?,
                estimated_input_tokens_per_request: required(
                    limit.estimated_input_tokens_per_request,
                    "estimated_input_tokens_per_request",
                )?,
                max_images_per_request: limit.max_images_per_request.ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "load_config",
                        format!("{label}.rate_limit.max_images_per_request is required"),
                    )
                })?,
            })
        })
        .transpose()?;

    Ok(ClassifierEndpointConfig {
        enabled: raw.enabled.unwrap_or(true),
        rate_limit,
        base_url,
        endpoint: validate_classifier_endpoint(raw.endpoint.as_deref(), &label)?,
        model,
        api_key,
        username: raw.username.clone().unwrap_or_default(),
        password,
        request_timeout_seconds: raw.request_timeout_seconds.unwrap_or(120),
        prompt_version: raw
            .prompt_version
            .clone()
            .unwrap_or_else(|| "wildlife-v1".to_string()),
        generation: ClassifierGenerationConfig {
            temperature: generation.temperature.unwrap_or(0.1),
            max_tokens: generation.max_tokens.unwrap_or(1000),
        },
    })
}

/// Validate a classifier endpoint path.
///
/// The endpoint must be a relative path (starting with `/`), must not contain
/// a URL scheme (no `://`), and must not contain `@` which could indicate
/// embedded credentials. Returns the validated path or the default if `None`.
fn validate_classifier_endpoint(endpoint: Option<&str>, label: &str) -> AppResult<String> {
    let endpoint = endpoint.unwrap_or("/chat/completions");

    if endpoint.is_empty() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.endpoint must not be empty"),
        ));
    }

    // Reject values that look like full URLs (contain a scheme).
    // This check must come before the leading-slash check so that
    // absolute URLs like "http://host/path" produce a clear error.
    if endpoint.contains("://") {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.endpoint must be a relative path, not a full URL"),
        ));
    }

    // Endpoint should start with `/` to be a valid absolute path.
    if !endpoint.starts_with('/') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.endpoint must start with '/'"),
        ));
    }

    // Reject scheme-relative references (//host/path) which would replace
    // the configured base_url host when resolved as a URL.
    if endpoint.starts_with("//") {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.endpoint must be an absolute path, not a scheme-relative reference"),
        ));
    }

    // Reject backslashes. Some URL resolvers (including the url crate) treat
    // backslashes as forward slashes, so \/\/evil.invalid/path would be
    // resolved as //evil.invalid/path, replacing the configured classifier
    // host with evil.invalid.
    if endpoint.contains('\\') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.endpoint must not contain backslashes"),
        ));
    }

    // Reject values containing `@` which could indicate embedded credentials
    // (e.g. http://user:pass@host/path).
    if endpoint.contains('@') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("{label}.endpoint must not contain embedded credentials"),
        ));
    }

    Ok(endpoint.to_string())
}

// ── Semantic validation ────────────────────────────────────────────────────

/// Resolve a configured daily capture-time filter.
fn resolve_capture_time_window(raw: &RawCaptureTimeWindow) -> AppResult<CaptureTimeWindow> {
    let start_time = raw.start_time.as_deref().ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "nvr.search.capture_time_window.start_time is required",
        )
    })?;
    let end_time = raw.end_time.as_deref().ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "nvr.search.capture_time_window.end_time is required",
        )
    })?;
    let utc_offset = raw.utc_offset.as_deref().ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "nvr.search.capture_time_window.utc_offset is required",
        )
    })?;

    let start_seconds = parse_clock_time(start_time, "start_time")?;
    let end_seconds = parse_clock_time(end_time, "end_time")?;
    if start_seconds == end_seconds {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "nvr.search.capture_time_window.start_time and end_time must differ",
        ));
    }

    Ok(CaptureTimeWindow {
        start_seconds,
        end_seconds,
        utc_offset: parse_utc_offset(utc_offset)?,
    })
}

/// Parse a 24-hour `HH:MM` time without accepting ambiguous formats.
fn parse_clock_time(value: &str, field: &str) -> AppResult<u32> {
    let bytes = value.as_bytes();
    let valid_shape = bytes.len() == 5
        && bytes[2] == b':'
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4].is_ascii_digit();
    if !valid_shape {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("nvr.search.capture_time_window.{field} must use 24-hour HH:MM format"),
        ));
    }
    let hours = u32::from(bytes[0] - b'0') * 10 + u32::from(bytes[1] - b'0');
    let minutes = u32::from(bytes[3] - b'0') * 10 + u32::from(bytes[4] - b'0');
    if hours > 23 || minutes > 59 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            format!("nvr.search.capture_time_window.{field} must use 24-hour HH:MM format"),
        ));
    }
    Ok(hours * 3_600 + minutes * 60)
}

/// Parse an explicit fixed UTC offset such as `+05:30`, `-07:00`, or `Z`.
fn parse_utc_offset(value: &str) -> AppResult<FixedOffset> {
    if value == "Z" {
        return Ok(FixedOffset::east_opt(0).expect("zero is a valid UTC offset"));
    }

    let bytes = value.as_bytes();
    let valid_shape = bytes.len() == 6
        && matches!(bytes[0], b'+' | b'-')
        && bytes[3] == b':'
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[4].is_ascii_digit()
        && bytes[5].is_ascii_digit();
    if !valid_shape {
        return invalid_utc_offset();
    }
    let hours = i32::from(bytes[1] - b'0') * 10 + i32::from(bytes[2] - b'0');
    let minutes = i32::from(bytes[4] - b'0') * 10 + i32::from(bytes[5] - b'0');
    if hours > 23 || minutes > 59 {
        return invalid_utc_offset();
    }
    let seconds = hours * 3_600 + minutes * 60;
    let seconds = if bytes[0] == b'-' { -seconds } else { seconds };
    FixedOffset::east_opt(seconds).ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "nvr.search.capture_time_window.utc_offset must be Z or a UTC offset in ±HH:MM format",
        )
    })
}

fn invalid_utc_offset<T>() -> AppResult<T> {
    Err(AppError::new(
        ErrorCategory::Configuration,
        "load_config",
        "nvr.search.capture_time_window.utc_offset must be Z or a UTC offset in ±HH:MM format",
    ))
}

fn validate_config(config: &Config) -> AppResult<()> {
    if config.web.listen_address.port() == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "web.listen_address port must be greater than zero",
        ));
    }
    if config.web.maximum_clip_duration_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "web.maximum_clip_duration_seconds must be greater than zero",
        ));
    }
    if config.web.maximum_clip_duration_seconds > 86_400 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "web.maximum_clip_duration_seconds must not exceed 86400",
        ));
    }
    let default_clip_duration = config
        .web
        .clip_pre_roll_seconds
        .checked_add(config.web.clip_post_roll_seconds)
        .ok_or_else(|| {
            AppError::new(
                ErrorCategory::Configuration,
                "validate_config",
                "web clip duration overflows the supported range",
            )
        })?;
    if default_clip_duration == 0
        || default_clip_duration > config.web.maximum_clip_duration_seconds
    {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "web clip pre-roll plus post-roll must be positive and not exceed maximum_clip_duration_seconds",
        ));
    }

    // NVR scheme
    if config.nvr.scheme != "http" && config.nvr.scheme != "https" {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.scheme must be \"http\" or \"https\"",
        ));
    }

    // NVR host — trim-based emptiness check
    require_nonempty(&config.nvr.host, "nvr.host")?;

    // NVR username — trim-based emptiness check
    require_nonempty(&config.nvr.username, "nvr.username")?;

    // NVR port
    if config.nvr.port == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.port must be greater than zero",
        ));
    }

    // NVR password — a source must be configured
    if config.nvr.password.is_none() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.password is required; configure password, password_file, or password_env",
        ));
    }

    // NVR request timeout
    if config.nvr.request_timeout_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.request_timeout_seconds must be greater than zero",
        ));
    }

    // NVR connect timeout
    if config.nvr.connect_timeout_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.connect_timeout_seconds must be greater than zero",
        ));
    }

    // Search window
    if config.nvr.search.window_minutes == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.search.window_minutes must be greater than zero",
        ));
    }

    // Search max_results
    if config.nvr.search.max_results == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.search.max_results must be greater than zero",
        ));
    }

    // Search poll_interval
    if config.nvr.search.poll_interval_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.search.poll_interval_seconds must be greater than zero",
        ));
    }

    // Search camera_refresh_interval
    if config.nvr.search.camera_refresh_interval_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.search.camera_refresh_interval_seconds must be greater than zero",
        ));
    }

    // Download retry_limit
    if config.nvr.download.retry_limit == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.retry_limit must be greater than zero",
        ));
    }

    // Download retry_initial_delay
    if config.nvr.download.retry_initial_delay_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.retry_initial_delay_seconds must be greater than zero",
        ));
    }

    // Download retry_max_delay
    if config.nvr.download.retry_max_delay_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.retry_max_delay_seconds must be greater than zero",
        ));
    }

    // Download concurrency
    if config.nvr.download.concurrency == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.concurrency must be greater than zero",
        ));
    }

    // Download maximum_image_size
    if config.nvr.download.maximum_image_size_bytes == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.maximum_image_size_bytes must be greater than zero",
        ));
    }

    // Retry delay ordering
    if config.nvr.download.retry_initial_delay_seconds > config.nvr.download.retry_max_delay_seconds
    {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.retry_initial_delay_seconds must not exceed retry_max_delay_seconds",
        ));
    }

    // ── Classifier pool validation ───────────────────────────────────────
    if config.classifier.poll_interval_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "classifier.poll_interval_seconds must be greater than zero",
        ));
    }
    if config.classifier.retry_limit == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "classifier.retry_limit must be greater than zero",
        ));
    }
    if config.classifier.retry_initial_delay_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "classifier.retry_initial_delay_seconds must be greater than zero",
        ));
    }
    if config.classifier.retry_max_delay_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "classifier.retry_max_delay_seconds must be greater than zero",
        ));
    }
    if config.classifier.processing_lease_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "classifier.processing_lease_seconds must be greater than zero",
        ));
    }
    if config.classifier.retry_initial_delay_seconds > config.classifier.retry_max_delay_seconds {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "classifier.retry_initial_delay_seconds must not exceed retry_max_delay_seconds",
        ));
    }

    // Non-wildlife image retention — must be positive.
    if config.general.non_wildlife_image_retention_days == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "general.non_wildlife_image_retention_days must be greater than zero",
        ));
    }

    // Reject retention values so large that subtracting them from a DateTime<Utc>
    // could overflow Chrono's bounds.  Chrono's DateTime is bounded to ~1677 AD
    // to ~2262 AD; subtracting a very large duration from the lower bound would
    // underflow.  We conservatively cap retention at ~100 years.
    const MAX_RETENTION_DAYS: u64 = 36_525; // ~100 years
    if config.general.non_wildlife_image_retention_days > MAX_RETENTION_DAYS {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!(
                "general.non_wildlife_image_retention_days ({}) exceeds maximum supported duration \
                 ({MAX_RETENTION_DAYS} days, ~100 years)",
                config.general.non_wildlife_image_retention_days
            ),
        ));
    }

    // Reject leases so large that adding them to a DateTime<Utc> could
    // overflow Chrono's bounds.
    const MAX_LEASE_SECS: u64 = 3_155_760_000;
    if config.classifier.processing_lease_seconds > MAX_LEASE_SECS {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!(
                "classifier.processing_lease_seconds ({}) exceeds maximum supported duration \
                 ({MAX_LEASE_SECS} seconds, ~100 years)",
                config.classifier.processing_lease_seconds
            ),
        ));
    }

    let mut quota_groups = BTreeMap::<&str, &ClassifierRateLimitConfig>::new();
    for (index, endpoint) in config.classifier.endpoints.iter().enumerate() {
        let label = format!("classifier.endpoints[{index}]");
        validate_classifier_endpoint_config(endpoint, &label)?;
        if endpoint.enabled {
            if let Some(limit) = &endpoint.rate_limit
                && let Some(existing) = quota_groups.insert(&limit.quota_group, limit)
                && existing != limit
            {
                return Err(AppError::new(
                    ErrorCategory::Configuration,
                    "validate_config",
                    format!(
                        "{label}.rate_limit differs from another endpoint in quota_group {}",
                        limit.quota_group
                    ),
                ));
            }
            if config.classifier.processing_lease_seconds <= endpoint.request_timeout_seconds {
                return Err(AppError::new(
                    ErrorCategory::Configuration,
                    "validate_config",
                    format!(
                        "classifier.processing_lease_seconds ({}) must be greater than \
                         {label}.request_timeout_seconds ({}) to provide lease headroom",
                        config.classifier.processing_lease_seconds,
                        endpoint.request_timeout_seconds,
                    ),
                ));
            }
        }
    }

    Ok(())
}

/// Validate endpoint-specific transport and generation settings.
fn validate_classifier_endpoint_config(
    endpoint: &ClassifierEndpointConfig,
    label: &str,
) -> AppResult<()> {
    if endpoint.base_url.scheme() != "http" && endpoint.base_url.scheme() != "https"
        || endpoint.base_url.host().is_none()
    {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!("{label}.base_url must use the http or https scheme and have a host"),
        ));
    }
    let has_username = !endpoint.username.is_empty();
    if has_username != endpoint.password.is_some() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!("{label}.username and {label}.password must be configured together"),
        ));
    }
    require_nonempty(&endpoint.model, &format!("{label}.model"))?;
    require_nonempty(&endpoint.prompt_version, &format!("{label}.prompt_version"))?;
    if endpoint.request_timeout_seconds == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!("{label}.request_timeout_seconds must be greater than zero"),
        ));
    }
    if endpoint.generation.max_tokens == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!("{label}.generation.max_tokens must be greater than zero"),
        ));
    }
    if let Some(limit) = &endpoint.rate_limit {
        require_nonempty(
            &limit.quota_group,
            &format!("{label}.rate_limit.quota_group"),
        )?;
        if limit.requests_per_minute == 0
            || limit.requests_per_day == 0
            || limit.tokens_per_minute == 0
            || limit.tokens_per_day == 0
            || limit.max_images_per_request == 0
        {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "validate_config",
                format!("{label}.rate_limit values must be greater than zero"),
            ));
        }
        let reservation = limit
            .estimated_input_tokens_per_request
            .saturating_add(endpoint.generation.max_tokens as u64);
        if reservation > limit.tokens_per_minute || reservation > limit.tokens_per_day {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "validate_config",
                format!("{label}.rate_limit token budget cannot admit one request"),
            ));
        }
    }
    let temperature = endpoint.generation.temperature;
    if !temperature.is_finite() || temperature < 0.0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!("{label}.generation.temperature must be a finite non-negative value"),
        ));
    }
    Ok(())
}

/// Apply consistent trim-based required-string validation.
///
/// Returns an error if the value is empty after trimming whitespace.
/// Does not silently alter the stored value.
fn require_nonempty(value: &str, field: &str) -> AppResult<()> {
    if value.trim().is_empty() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
}

/// Build a safe TOML diagnostic that omits source-line contents.
///
/// TOML parser errors include the offending source line in their Display
/// output, which could contain literal secrets. This function extracts only
/// safe information (line/column from the span and a sanitized parser reason)
/// and constructs an actionable message.
///
/// `source` is the original TOML text used to calculate real one-based
/// line and column coordinates from the parser span. The source is never
/// included in the returned diagnostic.
fn safe_toml_diagnostic(source: &str, e: &toml::de::Error) -> String {
    // Calculate real line/column from the parser span using the source.
    let (line, col) = if let Some(span) = e.span() {
        toml_line_column(source, span.start)
    } else {
        (0, 0)
    };

    // Build the reason from e.message() (the parser's actionable reason),
    // not from the full Display output which includes location headers and
    // potentially sensitive source-line content.
    let reason = sanitize_toml_reason(e.message());

    if line != 0 && col != 0 {
        format!(
            "TOML parse error at line {}, column {}: {}",
            line, col, reason
        )
    } else {
        format!("TOML parse error: {}", reason)
    }
}

/// Whitelist of expected type names the toml/serde parser can emit.
///
/// These are schema-derived vocabulary tokens that are safe to include in
/// diagnostic messages. The list covers every type used by the raw TOML
/// models and common serde type names. Entries are sorted longest-first so
/// prefix matching selects the most specific type.
const EXPECTED_TYPE_WHITELIST: &[&str] = &[
    // Compound / structured types
    "map",
    "sequence",
    "table",
    "inline table",
    "boolean",
    // Numeric types
    "integer",
    "float",
    // String types
    "string",
    "str",
    // Concrete Rust types the toml crate emits
    "u16",
    "u32",
    "u64",
    "f32",
    "bool",
    "PathBuf",
    "String",
    "Option",
];

/// Convert a toml/serde error message into an actionable reason while
/// omitting all untrusted tokens.
///
/// This is a fail-closed diagnostic formatter: it classifies the parser
/// message and emits only trusted fixed vocabulary and whitelisted expected
/// types. It never copies arbitrary substrings from the untrusted parser
/// message that could contain attacker-controlled configuration values.
///
/// The toml crate does NOT escape embedded backticks in its error messages.
/// An unknown field name like `SENTINEL`KEY` would cause a delimiter-based
/// scanner to stop at the embedded backtick and leak `KEY` as plain text.
/// Classification-based construction avoids this entirely.
fn sanitize_toml_reason(message: &str) -> String {
    // ── unknown field ──────────────────────────────────────────────────
    // The unknown field name rendered by the parser is untrusted and must
    // never be echoed.  Do not attempt to extract expected-field names from
    // the parser message: a crafted field name containing ", expected one of
    // ` would cause delimiter-based scanners to misidentify the expected
    // section and include attacker-controlled tokens.
    if message.starts_with("unknown field") {
        return "unknown field".to_string();
    }

    // ── missing field ──────────────────────────────────────────────────
    // The field name comes from the serde struct definition (schema-derived)
    // and is safe to include.  We still validate it contains only
    // identifier-like characters as a defense-in-depth measure.
    if let Some(rest) = message.strip_prefix("missing field `") {
        if let Some(end) = rest.find('`') {
            let field_name = &rest[..end];
            if field_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return format!("missing field `{field_name}`");
            }
        }
        // If the field name contains unexpected characters, fall through to
        // the generic fallback rather than echoing it.
        return "missing field".to_string();
    }

    // ── duplicate key ──────────────────────────────────────────────────
    // The key name and table name rendered by the parser are untrusted.
    if message.starts_with("duplicate key") {
        return "duplicate key".to_string();
    }

    // ── expected <token> (syntax-level) ────────────────────────────────
    // toml crate syntax errors include "expected" with a fixed set of
    // tokens: newline, `#`, `.`, `]`, `}`, `=`, `"`, `'`, `,`.
    // These are all parser-internal vocabulary and safe to echo.
    //
    // Handle both comma-separated ("expected newline, `#`") and
    // newline-separated ("invalid table header\nexpected `.`, `]`") forms.
    //
    // We match against a whitelist of trusted expected-token fragments
    // rather than extracting arbitrary substrings from the message.
    const EXPECTED_TOKEN_WHITELIST: &[(&str, &str)] = &[
        ("expected newline", "expected newline"),
        ("expected `]`", "expected `]`"),
        ("expected `}`", "expected `}`"),
        ("expected `=`, `.`", "expected `=` or `.`"),
        ("expected `.`, `=`", "expected `.` or `=`"),
        ("expected `.`, `]`", "expected `.` or `]`"),
        ("expected `\"`, `'`", "expected a quoted string"),
        ("expected `,`", "expected `,`"),
        ("expected `,`, `]`", "expected `,` or `]`"),
    ];

    // Check comma-separated form first (e.g. "expected newline, `#`").
    if let Some(comma_expected) = message.find(", expected ") {
        let expected_tail = &message[comma_expected + ", expected ".len()..];
        // First check if this is a type-mismatch (serde-level) error.
        // The expected tail starts with a whitelisted Rust type name.
        if let Some(whitelisted) = match_whitelisted_type(expected_tail) {
            return format!("expected {whitelisted}");
        }
        // Then check syntax-level expected tokens.
        // The full message before the comma may contain the "expected" keyword.
        let before_comma = &message[..comma_expected];
        if let Some(rest) = before_comma.strip_prefix("expected ") {
            let expected_fragment = rest.trim_end();
            if let Some((_, safe)) = EXPECTED_TOKEN_WHITELIST.iter().find(|(frag, _)| {
                expected_fragment.starts_with(frag.trim_start_matches("expected "))
            }) {
                return safe.to_string();
            }
        }
        // Could not classify the expected tail — do not echo it.
        return "type mismatch".to_string();
    }

    // Check "expected X, found Y" where expected comes first.
    if let Some(comma_found) = message.find(", found ") {
        let expected_part = &message[..comma_found];
        if let Some(rest) = expected_part.strip_prefix("expected ") {
            // Check type whitelist first.
            if let Some(whitelisted) = match_whitelisted_type(rest.trim_end()) {
                return format!("expected {whitelisted}");
            }
            // Check syntax token whitelist.
            let trimmed = rest.trim_end();
            if let Some((_, safe)) = EXPECTED_TOKEN_WHITELIST
                .iter()
                .find(|(frag, _)| trimmed.starts_with(frag.trim_start_matches("expected ")))
            {
                return safe.to_string();
            }
            return "type mismatch".to_string();
        }
    }

    // Check newline-separated form (e.g. "invalid table header\nexpected `.`, `]`").
    for line in message.split('\n') {
        if let Some(rest) = line.strip_prefix("expected ") {
            let trimmed = rest.trim_end();
            if let Some((_, safe)) = EXPECTED_TOKEN_WHITELIST
                .iter()
                .find(|(frag, _)| trimmed.starts_with(frag.trim_start_matches("expected ")))
            {
                return safe.to_string();
            }
        }
    }

    // ── common syntax error prefixes ───────────────────────────────────
    // These are fixed parser vocabulary tokens; the message text after them
    // may contain untrusted values so we do not echo them.
    if message.starts_with("invalid basic string") {
        return "unterminated string".to_string();
    }
    if message.starts_with("invalid table header") {
        return "invalid table header".to_string();
    }
    if message.starts_with("invalid array") {
        return "invalid array syntax".to_string();
    }
    if message.starts_with("invalid inline table") {
        return "invalid inline table syntax".to_string();
    }
    if message.starts_with("invalid string") {
        return "invalid string".to_string();
    }

    // ── fallback — unclassified message, do not echo untrusted content ─
    "TOML syntax error".to_string()
}

/// Match the beginning of `tail` against the expected-type whitelist.
///
/// Returns the first whitelisted type that `tail` starts with, or `None`
/// if no type matches.  The whitelist is sorted longest-first so that
/// prefix matches select the most specific type (e.g. "inline table" before
/// "table").
fn match_whitelisted_type(tail: &str) -> Option<&'static str> {
    EXPECTED_TYPE_WHITELIST
        .iter()
        .find(|&&t| tail.starts_with(t))
        .copied()
}

/// Translate a byte offset into one-based line and character-column
/// coordinates.
///
/// Counts newlines in the source up to `byte_offset` to determine the
/// line number, and counts Unicode scalar values (characters) since the
/// last newline for the column. Does not return the source line — only
/// the position.
fn toml_line_column(source: &str, byte_offset: usize) -> (usize, usize) {
    let mut line: usize = 1;
    let mut char_col: usize = 0; // characters on current line before this byte

    for (i, byte) in source.bytes().enumerate() {
        if i >= byte_offset {
            break;
        }
        if byte == b'\n' {
            line += 1;
            char_col = 0;
        } else if (byte & 0b1100_0000) != 0b1000_0000 {
            // Not a UTF-8 continuation byte (10xxxxxx), so it starts a
            // new Unicode scalar value.
            char_col += 1;
        }
        // Continuation bytes are skipped; they belong to the preceding char.
    }

    // One-based column: position within the line after the byte at offset.
    (line, char_col + 1)
}

/// Strip potentially sensitive details from an io::Error message.
fn safe_io_message(e: &std::io::Error) -> String {
    e.to_string()
}

/// Resolve and validate the playback host allowlist.
///
/// Each entry must be a bare hostname or canonical IP address:
/// no scheme, port, path, query, fragment, credentials, backslashes,
/// percent encoding, or IPv6 brackets.
fn resolve_playback_host_allowlist(raw: &Option<Vec<String>>) -> AppResult<Vec<String>> {
    let entries = raw.as_ref().map(Vec::as_slice).unwrap_or(&[]);
    let mut allowed = Vec::with_capacity(entries.len());
    for entry in entries {
        let normalized = validate_playback_host_entry(entry)?;
        allowed.push(normalized);
    }
    allowed.sort();
    allowed.dedup();
    Ok(allowed)
}

/// Validate and normalize a single playback host allowlist entry.
///
/// Returns the lowercased normalized entry or an error for invalid input.
///
/// Strict IPv4 canonical-form validation: rejects non-canonical forms
/// (hex, octal, truncated dotted-decimal, leading-zero octets) that
/// the url crate would silently normalize, ensuring that the stored
/// allowlist entry matches the URL host representation.
fn validate_playback_host_entry(entry: &str) -> AppResult<String> {
    // Reject empty entries.
    if entry.is_empty() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not be empty",
        ));
    }

    // Reject any scheme prefix.
    if entry.contains("://") {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must be bare hostnames or IP addresses, not URLs",
        ));
    }

    // Reject embedded credentials.
    if entry.contains('@') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain embedded credentials",
        ));
    }

    // Reject backslashes.
    if entry.contains('\\') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain backslashes",
        ));
    }

    // Reject percent encoding.
    if entry.contains('%') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain percent-encoded characters",
        ));
    }

    // Reject path-like characters.
    if entry.contains('/') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain path separators",
        ));
    }

    // Reject query/fragment characters.
    if entry.contains('?') || entry.contains('#') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain query or fragment characters",
        ));
    }

    // Reject IPv6 brackets (the entry must be bare).
    if entry.starts_with('[') || entry.contains(']') {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain brackets",
        ));
    }

    // Validate using the url crate's Host::parse (same as NvrTransport).
    // Handle bare IPv6 literals by wrapping them in brackets before parsing.
    // First, check if the entry is a bare IPv6 address (contains colons but
    // is not a hostname with a port — we distinguish by trying to parse
    // the entry as an Ipv6Addr).
    let host_for_parse = if entry.contains(':') && !entry.starts_with('[') {
        // Try to parse as a bare IPv6 address first.
        if let Ok(_ipv6) = entry.parse::<std::net::Ipv6Addr>() {
            // Valid bare IPv6 — wrap in brackets for url::Host::parse.
            format!("[{entry}]")
        } else {
            // Not a valid IPv6 — check if it looks like hostname:port.
            // If the part after the last colon is all digits, it's likely
            // a port number (which is not allowed). If it's not all digits,
            // it could be a hostname with colons (like a domain with port),
            // but we reject that since ports are not allowed.
            if let Some(last_colon) = entry.rfind(':') {
                let after = &entry[last_colon + 1..];
                if after.chars().all(|c| c.is_ascii_digit()) && !after.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "validate_config",
                        "nvr.download.playback_host_allowlist entries must not contain port numbers",
                    ));
                }
            }
            // Not a valid IPv6 and not a hostname:port — reject.
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "validate_config",
                "nvr.download.playback_host_allowlist entries must not contain invalid characters or is not a valid hostname/IP",
            ));
        }
    } else {
        entry.to_string()
    };

    let parsed_host = url::Host::parse(&host_for_parse).map_err(|_| {
        AppError::new(
            ErrorCategory::Configuration,
            "validate_config",
            "nvr.download.playback_host_allowlist entries must not contain invalid characters or is not a valid hostname/IP",
        )
    })?;

    // ── Strict IPv4 canonical-form validation ──────────────────────────
    // url::Host::parse normalizes several legacy IPv4 forms (component-level
    // hex like "127.0.0x0.1", trailing-dot like "1.2.3.4.", shortened
    // dotted-decimal, etc.) into canonical dotted-decimal.  We must reject
    // those non-canonical originals by validating through a typed Ipv4Addr
    // and comparing the canonical string representation.
    if let url::Host::Ipv4(addr) = parsed_host {
        let canonical = addr.to_string();
        // Reject if the original string differs from the canonical
        // dotted-decimal representation.  This catches component-level
        // hexadecimal ("0x7f.0.0.1"), trailing-dot ("1.2.3.4."),
        // shortened dotted-decimal ("127.0.1"), integer forms, and
        // any other non-canonical spelling the url crate would normalize.
        if entry != canonical {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "validate_config",
                "nvr.download.playback_host_allowlist entries must use canonical dotted-decimal IPv4 notation",
            ));
        }
        // Return the canonical IPv4 representation.
        return Ok(canonical.to_lowercase());
    }

    // ── IPv6 canonical-form normalization ──────────────────────────────
    // url::Host::parse accepts bare IPv6 with brackets.  If the entry
    // was a bare IPv6 address, normalize it through the typed Ipv6Addr
    // so that non-canonical forms (e.g. "2001:0db8::1") become canonical
    // ("2001:db8::1") matching what url::Url::host_str returns.
    if entry.contains(':')
        && !entry.starts_with('[')
        && let Ok(ipv6) = entry.parse::<std::net::Ipv6Addr>()
    {
        return Ok(ipv6.to_string().to_lowercase());
    }

    // ── Domain name normalization ──────────────────────────────────────
    // url::Host::parse normalizes internationalized domain names and
    // other representations.  Return the parsed host's canonical string
    // representation so stored entries match what URL parsing produces.
    Ok(parsed_host.to_string().to_lowercase())
}

// ── Capture time window tests ─────────────────────────────────────────────

#[cfg(test)]
mod capture_time_window_tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn window() -> CaptureTimeWindow {
        resolve_capture_time_window(&RawCaptureTimeWindow {
            start_time: Some("19:00".to_string()),
            end_time: Some("07:00".to_string()),
            utc_offset: Some("+05:30".to_string()),
        })
        .unwrap()
    }

    fn captured_at(hour: u32, minute: u32) -> Timestamp {
        Timestamp::new(Utc.with_ymd_and_hms(2026, 7, 11, hour, minute, 0).unwrap())
    }

    #[test]
    fn overnight_window_uses_configured_nvr_clock_and_half_open_bounds() {
        let window = window();
        assert!(window.contains(&captured_at(13, 30)), "19:00 is included");
        assert!(window.contains(&captured_at(1, 29)), "06:59 is included");
        assert!(!window.contains(&captured_at(1, 30)), "07:00 is excluded");
        assert!(!window.contains(&captured_at(12, 0)), "17:30 is excluded");
    }

    #[test]
    fn capture_time_window_is_loaded_from_nvr_search_table() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"[general]
output_directory = "{output}"

[nvr]
host = "nvr"
port = 80
username = "user"
password = "password"
start_at = "2026-01-01T00:00:00Z"

[nvr.search.capture_time_window]
start_time = "19:00"
end_time = "07:00"
utc_offset = "Z"

[classifier]
"#,
                output = output.display()
            ),
        )
        .unwrap();

        let config = Config::load(Some(&config_path)).unwrap();
        let window = config.nvr.search.capture_time_window.unwrap();
        assert!(window.contains(&captured_at(19, 0)));
        assert!(!window.contains(&captured_at(7, 0)));
    }

    #[test]
    fn capture_time_window_requires_unambiguous_valid_values() {
        let invalid_time = resolve_capture_time_window(&RawCaptureTimeWindow {
            start_time: Some("7 PM".to_string()),
            end_time: Some("07:00".to_string()),
            utc_offset: Some("Z".to_string()),
        })
        .unwrap_err();
        assert!(invalid_time.message.contains("start_time"));

        let equal_bounds = resolve_capture_time_window(&RawCaptureTimeWindow {
            start_time: Some("19:00".to_string()),
            end_time: Some("19:00".to_string()),
            utc_offset: Some("Z".to_string()),
        })
        .unwrap_err();
        assert!(equal_bounds.message.contains("must differ"));

        let invalid_offset = resolve_capture_time_window(&RawCaptureTimeWindow {
            start_time: Some("19:00".to_string()),
            end_time: Some("07:00".to_string()),
            utc_offset: Some("UTC".to_string()),
        })
        .unwrap_err();
        assert!(invalid_offset.message.contains("utc_offset"));
    }
}

// ── Configuration tests for Phase 7 fields ───────────────────────────────

#[cfg(test)]
mod phase7_tests {
    use super::*;

    fn write_config(dir: &tempfile::TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[allow(dead_code)]
    fn minimal_with_download(extra: &str) -> &'static str {
        format!(
            r#"[general]
output_directory = "/tmp/fauna-output"

[nvr]
scheme = "http"
host = "nvr.example.invalid"
port = 8080
username = "admin"
password = "test-pass"
start_at = "2026-07-11T00:00:00Z"
{extra}

[classifier]
"#,
            extra = extra
        )
        .leak()
    }

    #[test]
    fn download_defaults_include_concurrency_and_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/fauna-output"

[nvr]
scheme = "http"
host = "nvr.example.invalid"
port = 8080
username = "admin"
password = "test-pass"
start_at = "2026-07-11T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.nvr.download.concurrency, 2);
        assert!(config.nvr.download.playback_host_allowlist.is_empty());
    }

    #[test]
    fn download_concurrency_explicit_value() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/fauna-output"

[nvr]
scheme = "http"
host = "nvr.example.invalid"
port = 8080
username = "admin"
password = "test-pass"
start_at = "2026-07-11T00:00:00Z"
download = { concurrency = 4, playback_host_allowlist = ["cdn.example.com"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.nvr.download.concurrency, 4);
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["cdn.example.com"],
        );
    }

    #[test]
    fn download_zero_concurrency_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/fauna-output"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { concurrency = 0 }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn allowlist_rejects_url_with_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["http://evil.com"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_rejects_entry_with_port() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["cdn.example.com:443"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_rejects_embedded_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["user@evil.com"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_accepts_valid_hostname() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["cdn.example.com"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["cdn.example.com"],
        );
    }

    #[test]
    fn allowlist_accepts_valid_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["10.0.0.50"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["10.0.0.50"],
        );
    }

    #[test]
    fn allowlist_normalized_to_lowercase() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["CDN.Example.COM"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["cdn.example.com"],
        );
    }

    #[test]
    fn allowlist_deduplicates_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["b.example.com", "a.example.com", "b.example.com"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["a.example.com", "b.example.com"]
        );
    }

    // ── IPv4 canonical-form validation tests ───────────────────────────

    #[test]
    fn allowlist_accepts_canonical_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["192.168.1.50", "127.0.0.1", "10.0.0.1"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["10.0.0.1", "127.0.0.1", "192.168.1.50"]
        );
    }

    #[test]
    fn allowlist_rejects_truncated_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["127.1"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_rejects_leading_zero_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["127.00.0.1"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_rejects_hex_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["0x7f000001"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_rejects_decimal_ipv4() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["2130706433"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        assert!(Config::load(Some(&path)).is_err());
    }

    #[test]
    fn allowlist_accepts_bare_ipv6() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["::1", "2001:db8::1"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        // IPv6 entries are lowercased and sorted lexicographically.
        // "2001:db8::1" sorts before "::1" because '2' < ':' in ASCII.
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["2001:db8::1", "::1"]
        );
    }

    #[test]
    fn allowlist_canonicalizes_non_canonical_ipv6() {
        // Non-canonical IPv6 like 2001:0db8::1 should be normalized
        // to 2001:db8::1 so it matches the canonical form returned
        // by url::Url::host_str.
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["2001:0db8::1"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        // The non-canonical form should be normalized to canonical.
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["2001:db8::1"]
        );
    }

    #[test]
    fn allowlist_canonicalizes_domain_names() {
        // Domain names should be normalized through url::Host::parse
        // and lowercased.
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"
download = { playback_host_allowlist = ["CDN.Example.COM"] }

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.nvr.download.playback_host_allowlist,
            vec!["cdn.example.com"]
        );
    }
}

/// Parse a log level string into a `LogLevel` value.
///
/// Returns an error if the value is not one of: error, warn, info, debug, trace.
fn parse_log_level(s: Option<&str>) -> AppResult<LogLevel> {
    let s = s.unwrap_or("info");
    match s.to_lowercase().as_str() {
        "error" => Ok(LogLevel::Error),
        "warn" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(AppError::new(
            ErrorCategory::Configuration,
            "load_config",
            "general.log_level must be one of error, warn, info, debug, or trace",
        )),
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn write_config(dir: &tempfile::TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn minimal_valid_toml() -> &'static str {
        "[general]\noutput_directory = \"/tmp/fauna-output\"\n\n[nvr]\nscheme = \"http\"\nhost = \"nvr.example.invalid\"\nport = 8080\nusername = \"admin\"\npassword = \"test-pass\"\nstart_at = \"2026-07-11T00:00:00Z\"\n\n[classifier]\n"
    }

    #[test]
    fn load_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, minimal_valid_toml());
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.nvr.scheme, "http");
        assert_eq!(config.nvr.host, "nvr.example.invalid");
        assert_eq!(config.nvr.port, 8080);
        assert!(config.classifier.endpoints.is_empty());
    }

    #[test]
    fn load_config_applies_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/fauna-output"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        // Defaults applied
        assert_eq!(config.nvr.request_timeout_seconds, 30);
        assert_eq!(config.nvr.connect_timeout_seconds, 10);
        assert_eq!(config.nvr.search.window_minutes, 60);
        assert_eq!(config.nvr.search.max_results, 50);
        assert_eq!(config.nvr.download.retry_limit, 10);
        assert!(config.nvr.download.verify_jpeg);
    }

    #[test]
    fn nvr_scheme_defaults_to_http() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.nvr.scheme, "http");
    }

    #[test]
    fn timestamp_offset_normalized_to_utc() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/fauna-output"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-07-11T05:30:00+05:30"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        // +05:30 offset should normalize to UTC 00:00:00
        assert_eq!(config.nvr.start_at.as_datetime().hour(), 0);
        assert_eq!(config.nvr.start_at.as_datetime().minute(), 0);
    }

    #[test]
    fn unknown_field_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"
unknown_field = true

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn invalid_scheme_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "ftp"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("scheme"));
    }

    #[test]
    fn empty_host_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = ""
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn zero_port_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 0
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn invalid_timestamp_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "not-a-timestamp"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn missing_output_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("output_directory"));
    }

    #[test]
    fn classifier_enabled_requires_url_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
model = "test"
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("base_url"));
    }

    #[test]
    fn empty_classifier_endpoint_list_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        // Should succeed because classification is optional without endpoints
        assert!(result.is_ok());
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-SECRET-12345"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains("SENTINEL-SECRET-12345"));
        assert!(debug_output.contains("[REDACTED]"));
    }

    #[test]
    fn retry_initial_must_not_exceed_max() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.download]
retry_initial_delay_seconds = 500
retry_max_delay_seconds = 100

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("retry_initial_delay_seconds"));
    }

    #[test]
    fn zero_poll_interval_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.search]
poll_interval_seconds = 0

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn zero_window_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.search]
window_minutes = 0

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn zero_max_results_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.search]
max_results = 0

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn zero_max_image_size_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.download]
maximum_image_size_bytes = 0

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn zero_retry_limit_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.download]
retry_limit = 0

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn classifier_enabled_with_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "vision-model"
api_key = "test-key"

[classifier.endpoints.generation]
temperature = 0.1
max_tokens = 1000
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.classifier.endpoints.is_empty());
        assert!(
            config.classifier.endpoints[0]
                .base_url
                .as_str()
                .starts_with("http://localhost:8081/v1")
        );
        assert_eq!(config.classifier.endpoints[0].model, "vision-model");
        assert_eq!(config.classifier.endpoints[0].generation.temperature, 0.1);
        assert_eq!(config.classifier.endpoints[0].generation.max_tokens, 1000);
    }

    #[test]
    fn full_example_config_loads() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
database_path = "{db}/fauna-scan.sqlite3"
output_directory = "{output}"
log_level = "info"

[nvr]
scheme = "http"
host = "nvr.example.invalid"
port = 8080
username = "admin"
password = "test-pass"
start_at = "2026-07-11T00:00:00+05:30"
request_timeout_seconds = 30
connect_timeout_seconds = 10
allow_invalid_tls_certificates = false

[nvr.search]
window_minutes = 60
max_results = 50
poll_interval_seconds = 60
poll_overlap_seconds = 120
camera_refresh_interval_seconds = 3600
settlement_delay_seconds = 10

[nvr.download]
retry_limit = 10
retry_initial_delay_seconds = 5
retry_max_delay_seconds = 300
maximum_image_size_bytes = 25000000
verify_jpeg = true
rebase_playback_urls = true

[classifier]
poll_interval_seconds = 10
retry_limit = 5
retry_initial_delay_seconds = 10
retry_max_delay_seconds = 300
processing_lease_seconds = 600

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "vision-model"
api_key = "test-key"
username = ""
request_timeout_seconds = 120
prompt_version = "wildlife-v1"

[classifier.endpoints.generation]
temperature = 0.1
max_tokens = 1000
"#,
            db = dir.path().display(),
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();

        assert_eq!(config.nvr.scheme, "http");
        assert_eq!(config.nvr.host, "nvr.example.invalid");
        assert_eq!(config.nvr.port, 8080);
        assert_eq!(config.nvr.request_timeout_seconds, 30);
        assert_eq!(config.nvr.connect_timeout_seconds, 10);
        assert!(!config.nvr.allow_invalid_tls_certificates);
        assert_eq!(config.nvr.search.window_minutes, 60);
        assert_eq!(config.nvr.search.poll_overlap_seconds, 120);
        assert_eq!(config.nvr.download.retry_limit, 10);
        assert_eq!(config.nvr.download.maximum_image_size_bytes, 25_000_000);
        assert!(!config.classifier.endpoints.is_empty());
        assert_eq!(config.classifier.endpoints[0].prompt_version, "wildlife-v1");

        // Timestamp normalized to UTC: 00:00:00+05:30 = 18:30:00 UTC (previous day)
        assert_eq!(config.nvr.start_at.as_datetime().hour(), 18);
        assert_eq!(config.nvr.start_at.as_datetime().minute(), 30);
    }

    #[test]
    fn classifier_enabled_secret_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "literal-key"
api_key_file = "/tmp/ignored"
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("multiple sources"));
        assert!(err.message.contains("classifier.endpoints[0].api_key"));
    }

    #[test]
    fn nvr_password_secret_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "literal"
password_file = "/tmp/ignored"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("multiple sources"));
        assert!(err.message.contains("nvr.password"));
    }

    #[test]
    fn classifier_enabled_zero_max_tokens_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "k"

[classifier.endpoints.generation]
max_tokens = 0
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn missing_config_file_fails() {
        let result = Config::load(Some(Path::new("/nonexistent/config.toml")));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("nonexistent"));
    }

    #[test]
    fn overlap_and_settlement_zero_permitted() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[nvr.search]
poll_overlap_seconds = 0
settlement_delay_seconds = 0

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.nvr.search.poll_overlap_seconds, 0);
        assert_eq!(config.nvr.search.settlement_delay_seconds, 0);
    }

    // ── New tests for reviewer feedback ────────────────────────────────────

    #[test]
    fn invalid_log_level_fails() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"
log_level = "verbose"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("log_level"));
        // Error must NOT echo the invalid value
        assert!(!err.message.contains("verbose"));
    }

    #[test]
    fn default_log_level_is_info() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.general.log_level, LogLevel::Info);
    }

    #[test]
    fn legacy_top_level_classifier_api_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
api_key = "legacy-key"
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unknown field"));
    }

    #[test]
    fn legacy_top_level_classifier_password_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
password = "legacy-password"
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unknown field"));
    }

    #[test]
    fn malformed_toml_error_does_not_leak_secrets() {
        // A malformed TOML line containing a sentinel secret should not
        // appear in the error message.
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"[general]
output_directory = "/tmp/x"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "SENTINEL-TOML-LEAK" trailing-junk
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#;
        let path = write_config(&dir, toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains("SENTINEL-TOML-LEAK"),
            "secret leaked in Display: {}",
            display
        );
        assert!(
            !debug.contains("SENTINEL-TOML-LEAK"),
            "secret leaked in Debug: {}",
            debug
        );
    }

    #[test]
    fn explicit_config_path_does_not_require_xdg_state() {
        // When an explicit config path is given and database_path is explicit,
        // no XDG state resolution should be needed.
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let db_path = dir.path().join("test.db");

        let toml = format!(
            r#"[general]
database_path = "{db}"
output_directory = "{output}"

[nvr]
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]
"#,
            db = db_path.display(),
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);

        // Config loads without needing XDG state path resolution
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.nvr.scheme, "http"); // default
        // Legacy general.database_path selects SQLite with the given path
        assert!(
            matches!(&config.database, crate::configuration::DatabaseConfig::Sqlite { path, .. } if path == &db_path),
            "expected SQLite config with path {}, got {:?}",
            db_path.display(),
            config.database
        );
    }

    #[test]
    fn safe_toml_diagnostic_no_source_content() {
        // Verify that safe_toml_diagnostic does not include source text.
        let bad_toml = "password = \"LEAK-ME-PLEASE\" broken";
        let result: Result<RawConfig, _> = toml::from_str(bad_toml);
        let err = result.unwrap_err();
        let diagnostic = safe_toml_diagnostic(bad_toml, &err);
        assert!(
            !diagnostic.contains("LEAK-ME-PLEASE"),
            "source content leaked in diagnostic: {}",
            diagnostic
        );
        // The diagnostic should contain safe info (line/column and message)
        assert!(
            diagnostic.contains("TOML parse error"),
            "diagnostic should mention TOML parse error: {}",
            diagnostic
        );
    }

    // ── sanitize_toml_reason: syntax error classifications ────────────────

    #[test]
    fn sanitize_trailing_junk_produces_expected_newline() {
        // Trailing junk after a value: "expected newline, `#`"
        let reason = sanitize_toml_reason("expected newline, `#`");
        assert!(
            reason.contains("expected newline"),
            "expected newline in reason, got: {}",
            reason
        );
    }

    #[test]
    fn sanitize_unterminated_string() {
        let reason = sanitize_toml_reason("invalid basic string");
        assert_eq!(reason, "unterminated string");
    }

    #[test]
    fn sanitize_invalid_table_header() {
        let reason = sanitize_toml_reason("invalid table header\nexpected `.`, `]`");
        assert!(
            reason.contains("invalid table header") || reason.contains("expected"),
            "expected actionable table header reason, got: {}",
            reason
        );
    }

    #[test]
    fn sanitize_duplicate_key() {
        let reason = sanitize_toml_reason("duplicate key `password` in table `nvr`");
        assert_eq!(reason, "duplicate key");
        // Must not echo the key name (which could be attacker-controlled)
        assert!(!reason.contains("password"));
    }

    #[test]
    fn sanitize_invalid_array() {
        let reason = sanitize_toml_reason("invalid array\nexpected `]`");
        assert!(
            reason.contains("invalid array") || reason.contains("expected"),
            "expected actionable array reason, got: {}",
            reason
        );
    }

    #[test]
    fn sanitize_invalid_inline_table() {
        let reason = sanitize_toml_reason("invalid inline table\nexpected `}`");
        assert!(
            reason.contains("invalid inline table") || reason.contains("expected"),
            "expected actionable inline table reason, got: {}",
            reason
        );
    }

    #[test]
    fn sanitize_invalid_string() {
        let reason = sanitize_toml_reason("invalid string\nexpected `\"`, `'`");
        assert!(
            reason.contains("invalid string") || reason.contains("quoted string"),
            "expected actionable string reason, got: {}",
            reason
        );
    }

    #[test]
    fn sanitize_unclassified_fallback() {
        // An unclassified message should produce a safe fallback without
        // echoing the untrusted content.
        let reason = sanitize_toml_reason("SENTINEL-UNCLASSIFIED-ERROR");
        assert!(
            !reason.contains("SENTINEL"),
            "unclassified sentinel leaked in reason: {}",
            reason
        );
        assert!(
            reason.contains("TOML syntax error"),
            "expected safe fallback: {}",
            reason
        );
    }

    // ── Reviewer feedback: classifier URL credentials ──────────────────────

    #[test]
    fn classifier_url_with_embedded_credentials_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://user:SENTINEL-URL-PASS@localhost:8081/v1"
model = "test"
api_key = "k"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("embedded credentials")
                || err.message.contains("must not contain"),
            "expected embedded-credentials error, got: {}",
            err.message
        );
        // The error must not contain the sentinel credential
        assert!(
            !err.message.contains("SENTINEL-URL-PASS"),
            "credential leaked in error message: {}",
            err.message
        );
    }

    #[test]
    fn classifier_url_with_username_only_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://user@localhost:8081/v1"
model = "test"
api_key = "k"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("embedded credentials")
                || err.message.contains("must not contain"),
            "expected embedded-credentials error, got: {}",
            err.message
        );
    }

    #[test]
    fn classifier_invalid_url_error_does_not_leak_raw_url() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "SENTINEL-BAD-URL-VALUE"
model = "test"
api_key = "k"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            !err.message.contains("SENTINEL-BAD-URL-VALUE"),
            "raw URL leaked in error message: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_debug_does_not_leak_url_credentials() {
        // When a credential-bearing URL is rejected, the error message must
        // not contain the credential. This is covered by the rejection test
        // above, but we also verify that a valid config's Debug output
        // never shows URL-embedded credentials (since they are rejected).
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "SENTINEL-API-KEY"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("SENTINEL-API-KEY"),
            "api key leaked in Debug: {}",
            debug_output
        );
    }

    // ── Reviewer feedback: classifier scheme and auth pairing ──────────────

    #[test]
    fn classifier_enabled_non_http_scheme_fails() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "ftp://localhost:8081/v1"
model = "test"
api_key = "k"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("http") && err.message.contains("https"),
            "expected scheme error mentioning http/https, got: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_username_without_password_fails() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
username = "cls-user"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("username") && err.message.contains("password"),
            "expected auth-pairing error, got: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_password_without_username_fails() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
password = "cls-pass"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("password") && err.message.contains("username"),
            "expected auth-pairing error, got: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_api_key_only_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "test-key"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.classifier.endpoints.is_empty());
    }

    #[test]
    fn classifier_enabled_basic_auth_only_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
username = "cls-user"
password = "cls-pass"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.classifier.endpoints.is_empty());
    }

    #[test]
    fn classifier_enabled_api_key_and_basic_auth_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
api_key = "test-key"
username = "cls-user"
password = "cls-pass"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.classifier.endpoints.is_empty());
    }

    #[test]
    fn classifier_enabled_no_auth_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.classifier.endpoints.is_empty());
    }

    #[test]
    fn classifier_enabled_https_scheme_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "https://localhost:8081/v1"
model = "test"
api_key = "k"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert!(!config.classifier.endpoints.is_empty());
        assert_eq!(config.classifier.endpoints[0].base_url.scheme(), "https");
    }

    #[test]
    fn classifier_enabled_endpoint_with_absolute_url_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "http://user:SENTINEL-EP-CRED@host/path"
model = "test"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("relative path") || err.message.contains("full URL"),
            "expected endpoint-as-URL error, got: {}",
            err.message
        );
        // Must not leak the sentinel credential
        assert!(
            !err.message.contains("SENTINEL-EP-CRED"),
            "credential leaked in error message: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_endpoint_with_at_sign_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/user:SENTINEL-AT-CRED@/path"
model = "test"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("embedded credentials") || err.message.contains("credentials"),
            "expected credentials error, got: {}",
            err.message
        );
        assert!(
            !err.message.contains("SENTINEL-AT-CRED"),
            "credential leaked in error message: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_endpoint_without_leading_slash_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "chat/completions"
model = "test"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let result = Config::load(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("start with"),
            "expected leading-slash error, got: {}",
            err.message
        );
    }

    #[test]
    fn classifier_enabled_valid_endpoint_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/v1/images/classify"
model = "test"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(
            config.classifier.endpoints[0].endpoint,
            "/v1/images/classify"
        );
    }

    #[test]
    fn classifier_enabled_default_endpoint_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
model = "test"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.classifier.endpoints[0].endpoint, "/chat/completions");
    }

    #[test]
    fn classifier_enabled_endpoint_debug_does_not_leak() {
        // A valid endpoint should appear in Debug output (it's a path, not
        // a secret), but the endpoint validation ensures no credential-bearing
        // values are accepted.
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let toml = format!(
            r#"[general]
output_directory = "{output}"

[nvr]
scheme = "http"
host = "p"
port = 80
username = "u"
password = "x"
start_at = "2026-01-01T00:00:00Z"

[classifier]

[[classifier.endpoints]]
base_url = "http://localhost:8081/v1"
endpoint = "/chat/completions"
model = "test"
api_key = "SENTINEL-API-KEY-EP"
"#,
            output = output_dir.display(),
        );
        let path = write_config(&dir, &toml);
        let config = Config::load(Some(&path)).unwrap();
        let debug_output = format!("{config:?}");
        // The endpoint path itself is safe to show
        assert!(debug_output.contains("/chat/completions"));
        // But the API key must be redacted
        assert!(
            !debug_output.contains("SENTINEL-API-KEY-EP"),
            "api key leaked in Debug: {debug_output}"
        );
    }
}
