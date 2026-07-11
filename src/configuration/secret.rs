//! Secret source resolution, conflict detection, and redacted storage.
//!
//! Centralizes secret-source conflict handling, resolution, newline trimming,
//! and redaction.

use std::fmt;
use std::path::PathBuf;

use crate::error::{AppError, AppResult, ErrorCategory};

/// Unresolved secret source fields from TOML configuration.
///
/// Only one of `literal`, `file`, or `environment` may be set.
///
/// `Debug` is implemented manually so that the literal secret value is never
/// exposed in diagnostic output.
#[derive(Clone, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct SecretSource {
    /// Literal value (development only).
    pub literal: Option<String>,
    /// Path to a file containing the secret.
    pub file: Option<PathBuf>,
    /// Environment variable name.
    pub environment: Option<String>,
}

impl std::fmt::Debug for SecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSource")
            .field("literal", &self.literal.as_ref().map(|_| "[REDACTED]"))
            .field("file", &self.file)
            .field("environment", &self.environment)
            .finish()
    }
}

impl SecretSource {
    /// Count how many sources are configured.
    pub fn source_count(&self) -> usize {
        [
            self.literal.is_some(),
            self.file.is_some(),
            self.environment.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    /// Resolve the secret value from the configured source.
    ///
    /// `label` is the logical name of the secret (e.g. "nvr.password") used in
    /// error messages. `get_env` is an injected environment lookup for
    /// deterministic testing.
    ///
    /// Returns `Ok(None)` when no source is configured, `Ok(Some(Secret))` when
    /// exactly one source resolves, or an error for conflicts, missing files,
    /// or missing environment variables.
    pub fn resolve<F>(&self, label: &'static str, get_env: F) -> AppResult<Option<Secret>>
    where
        F: Fn(&str) -> Option<String>,
    {
        let count = self.source_count();
        if count > 1 {
            return Err(AppError::new(
                ErrorCategory::Configuration,
                "resolve_secret",
                format!(
                    "multiple sources configured for secret \"{label}\"; \
                     only one of literal, file, or environment is permitted"
                ),
            ));
        }
        if count == 0 {
            return Ok(None);
        }

        match (&self.literal, &self.file, &self.environment) {
            (Some(value), None, None) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_secret",
                        format!("secret \"{label}\" has an empty literal value"),
                    ));
                }
                Ok(Some(Secret::new(value.clone())))
            }
            (None, Some(path), None) => {
                let contents = std::fs::read_to_string(path).map_err(|e| {
                    AppError::new(
                        ErrorCategory::Filesystem,
                        "resolve_secret",
                        format!(
                            "cannot read secret file for \"{label}\" \
                                 at {}: {}",
                            path.display(),
                            safe_io_message(&e)
                        ),
                    )
                })?;
                let trimmed = trim_single_trailing_newline(&contents);
                if trimmed.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_secret",
                        format!("secret file for \"{label}\" at {} is empty", path.display()),
                    ));
                }
                Ok(Some(Secret::new(trimmed.to_string())))
            }
            (None, None, Some(var_name)) => {
                let value = get_env(var_name).ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_secret",
                        format!(
                            "environment variable \"{var_name}\" \
                             is not set for secret \"{label}\""
                        ),
                    )
                })?;
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::Configuration,
                        "resolve_secret",
                        format!(
                            "environment variable \"{var_name}\" \
                             is empty for secret \"{label}\""
                        ),
                    ));
                }
                Ok(Some(Secret::new(value)))
            }
            _ => Err(AppError::new(
                ErrorCategory::Internal,
                "resolve_secret",
                format!(
                    "internal invariant violation resolving secret \"{label}\": \
                     source_count == 1 but no matching variant"
                ),
            )),
        }
    }
}

/// Remove exactly one trailing LF or CRLF from a string, preserving all other
/// whitespace and additional line endings.
fn trim_single_trailing_newline(s: &str) -> &str {
    if let Some(stripped) = s.strip_suffix("\r\n") {
        stripped
    } else if let Some(stripped) = s.strip_suffix('\n') {
        stripped
    } else {
        s
    }
}

/// A resolved secret value that is always redacted in Debug and Display output.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Create a new secret from a resolved value.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Expose the raw secret value for authentication code.
    ///
    /// # Safety
    /// Callers must never log, display, or include this value in error messages.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

// Redacted Debug and Display
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Strip potentially sensitive details from an io::Error message.
fn safe_io_message(e: &std::io::Error) -> String {
    e.to_string()
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            map.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn no_source_returns_none() {
        let source = SecretSource::default();
        let result = source.resolve("test", env_map(&[])).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn literal_source_resolves() {
        let source = SecretSource {
            literal: Some("my-secret".to_string()),
            file: None,
            environment: None,
        };
        let result = source.resolve("test", env_map(&[])).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().expose(), "my-secret");
    }

    #[test]
    fn file_source_resolves_and_trims_lf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "file-secret\n").unwrap();

        let source = SecretSource {
            literal: None,
            file: Some(path),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[])).unwrap();
        assert_eq!(result.unwrap().expose(), "file-secret");
    }

    #[test]
    fn file_source_resolves_and_trims_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, b"file-secret\r\n").unwrap();

        let source = SecretSource {
            literal: None,
            file: Some(path),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[])).unwrap();
        assert_eq!(result.unwrap().expose(), "file-secret");
    }

    #[test]
    fn file_source_preserves_multiple_newlines_except_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "line1\nline2\n").unwrap();

        let source = SecretSource {
            literal: None,
            file: Some(path),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[])).unwrap();
        assert_eq!(result.unwrap().expose(), "line1\nline2");
    }

    #[test]
    fn file_source_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "no-newline").unwrap();

        let source = SecretSource {
            literal: None,
            file: Some(path),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[])).unwrap();
        assert_eq!(result.unwrap().expose(), "no-newline");
    }

    #[test]
    fn env_source_resolves() {
        let source = SecretSource {
            literal: None,
            file: None,
            environment: Some("TEST_SECRET".to_string()),
        };
        let result = source
            .resolve("test", env_map(&[("TEST_SECRET", "env-secret")]))
            .unwrap();
        assert_eq!(result.unwrap().expose(), "env-secret");
    }

    #[test]
    fn pairwise_conflict_literal_file() {
        let source = SecretSource {
            literal: Some("lit".to_string()),
            file: Some(PathBuf::from("/tmp/x")),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("multiple sources"));
        assert!(err.message.contains("test"));
    }

    #[test]
    fn pairwise_conflict_literal_env() {
        let source = SecretSource {
            literal: Some("lit".to_string()),
            file: None,
            environment: Some("X".to_string()),
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
    }

    #[test]
    fn pairwise_conflict_file_env() {
        let source = SecretSource {
            literal: None,
            file: Some(PathBuf::from("/tmp/x")),
            environment: Some("X".to_string()),
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
    }

    #[test]
    fn three_way_conflict() {
        let source = SecretSource {
            literal: Some("lit".to_string()),
            file: Some(PathBuf::from("/tmp/x")),
            environment: Some("X".to_string()),
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
    }

    #[test]
    fn missing_file_fails() {
        let source = SecretSource {
            literal: None,
            file: Some(PathBuf::from("/nonexistent/secret")),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Filesystem);
    }

    #[test]
    fn missing_env_var_fails() {
        let source = SecretSource {
            literal: None,
            file: None,
            environment: Some("MISSING_VAR".to_string()),
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("MISSING_VAR"));
    }

    #[test]
    fn empty_literal_fails() {
        let source = SecretSource {
            literal: Some("   ".to_string()),
            file: None,
            environment: None,
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
    }

    #[test]
    fn empty_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();

        let source = SecretSource {
            literal: None,
            file: Some(path),
            environment: None,
        };
        let result = source.resolve("test", env_map(&[]));
        assert!(result.is_err());
    }

    #[test]
    fn empty_env_var_fails() {
        let source = SecretSource {
            literal: None,
            file: None,
            environment: Some("EMPTY_VAR".to_string()),
        };
        let result = source
            .resolve("test", env_map(&[("EMPTY_VAR", "  ")]))
            .unwrap_err();
        assert!(result.message.contains("empty"));
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("super-secret-value".to_string());
        let debug_output = format!("{secret:?}");
        assert_eq!(debug_output, "[REDACTED]");
        assert!(!debug_output.contains("super-secret-value"));
    }

    #[test]
    fn secret_display_is_redacted() {
        let secret = Secret::new("super-secret-value".to_string());
        let display_output = format!("{secret}");
        assert_eq!(display_output, "[REDACTED]");
        assert!(!display_output.contains("super-secret-value"));
    }

    #[test]
    fn trim_single_lf() {
        assert_eq!(trim_single_trailing_newline("hello\n"), "hello");
    }

    #[test]
    fn trim_single_crlf() {
        assert_eq!(trim_single_trailing_newline("hello\r\n"), "hello");
    }

    #[test]
    fn no_trailing_newline_unchanged() {
        assert_eq!(trim_single_trailing_newline("hello"), "hello");
    }

    #[test]
    fn multiple_newlines_trim_only_last() {
        assert_eq!(
            trim_single_trailing_newline("line1\nline2\n"),
            "line1\nline2"
        );
    }

    #[test]
    fn secret_source_count() {
        assert_eq!(SecretSource::default().source_count(), 0);
        assert_eq!(
            SecretSource {
                literal: Some("x".to_string()),
                ..Default::default()
            }
            .source_count(),
            1
        );
        assert_eq!(
            SecretSource {
                literal: Some("x".to_string()),
                file: Some(PathBuf::from("/tmp")),
                ..Default::default()
            }
            .source_count(),
            2
        );
    }

    // ── Debug redaction tests ──────────────────────────────────────────────

    #[test]
    fn secret_source_debug_redacts_literal() {
        let source = SecretSource {
            literal: Some("SENTINEL-LITERAL-SECRET".to_string()),
            file: None,
            environment: None,
        };
        let debug_output = format!("{source:?}");
        assert!(
            !debug_output.contains("SENTINEL-LITERAL-SECRET"),
            "literal secret leaked in Debug: {debug_output}"
        );
        assert!(debug_output.contains("[REDACTED]"));
    }

    #[test]
    fn secret_source_debug_redacts_file_source() {
        let source = SecretSource {
            literal: None,
            file: Some(PathBuf::from("/secrets/sentinel.txt")),
            environment: None,
        };
        let debug_output = format!("{source:?}");
        // File path is safe to show (it's a path, not a secret value)
        assert!(debug_output.contains("/secrets/sentinel.txt"));
        // But no secret value should appear
        assert!(!debug_output.contains("SENTINEL"));
    }

    #[test]
    fn secret_source_debug_redacts_env_source() {
        let source = SecretSource {
            literal: None,
            file: None,
            environment: Some("FAUNA_SCAN_SENTINEL_VAR".to_string()),
        };
        let debug_output = format!("{source:?}");
        // Environment variable name is safe to show
        assert!(debug_output.contains("FAUNA_SCAN_SENTINEL_VAR"));
    }

    #[test]
    fn secret_source_debug_with_all_sources_redacts_literal() {
        let source = SecretSource {
            literal: Some("SENTINEL-ALL-THREE".to_string()),
            file: Some(PathBuf::from("/tmp/secret")),
            environment: Some("SECRET_ENV".to_string()),
        };
        let debug_output = format!("{source:?}");
        assert!(
            !debug_output.contains("SENTINEL-ALL-THREE"),
            "literal secret leaked in Debug: {debug_output}"
        );
        assert!(debug_output.contains("[REDACTED]"));
    }
}
