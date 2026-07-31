//! XDG path resolution and non-mutating filesystem validation.
//!
//! Keeps environment-dependent path computation and filesystem checks
//! separate from TOML deserialization models.

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult, ErrorCategory};

/// Resolved Fauna Scan XDG directories.
#[derive(Debug, Clone)]
pub struct XdgPaths {
    /// Resolved configuration directory (e.g. `$XDG_CONFIG_HOME/fauna-scan`).
    pub config_dir: PathBuf,
    /// Resolved state directory (e.g. `$XDG_STATE_HOME/fauna-scan`).
    pub state_dir: PathBuf,
}

impl XdgPaths {
    /// Resolve XDG directories using an injected environment lookup.
    ///
    /// Each XDG base is resolved independently: `HOME` is required only for
    /// a base whose corresponding XDG variable is unavailable. Empty or
    /// relative XDG values are treated as unusable per XDG base-directory
    /// rules and fall back to `HOME`.
    pub fn resolve<F>(get_env: F) -> AppResult<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let config_dir = resolve_xdg_config_dir(&get_env)?;
        let state_dir = resolve_xdg_state_dir(&get_env)?;

        Ok(Self {
            config_dir,
            state_dir,
        })
    }

    /// Return the default configuration file path.
    pub fn default_config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Return the default database file path.
    pub fn default_database_path(&self) -> PathBuf {
        self.state_dir.join("fauna-scan.sqlite3")
    }
}

/// Resolve the XDG config directory independently.
///
/// Uses `XDG_CONFIG_HOME` if set and valid (absolute, non-empty), otherwise
/// falls back to `$HOME/.config`. Returns an error only when neither source
/// is available.
fn resolve_xdg_config_dir<F>(get_env: &F) -> AppResult<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = get_env("XDG_CONFIG_HOME")
        && !dir.is_empty()
        && Path::new(&dir).is_absolute()
    {
        return Ok(PathBuf::from(dir).join("fauna-scan"));
    }
    // Fallback to HOME/.config
    let home = get_env("HOME").ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "resolve_xdg",
            "HOME environment variable is not set and XDG_CONFIG_HOME is not available",
        )
    })?;
    if home.is_empty() || !Path::new(&home).is_absolute() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "resolve_xdg",
            "HOME is empty or not an absolute path; cannot resolve XDG config directory",
        ));
    }
    Ok(PathBuf::from(&home).join(".config").join("fauna-scan"))
}

/// Resolve the XDG state directory independently.
///
/// Uses `XDG_STATE_HOME` if set and valid (absolute, non-empty), otherwise
/// falls back to `$HOME/.local/state`. Returns an error only when neither
/// source is available.
fn resolve_xdg_state_dir<F>(get_env: &F) -> AppResult<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = get_env("XDG_STATE_HOME")
        && !dir.is_empty()
        && Path::new(&dir).is_absolute()
    {
        return Ok(PathBuf::from(dir).join("fauna-scan"));
    }
    // Fallback to HOME/.local/state
    let home = get_env("HOME").ok_or_else(|| {
        AppError::new(
            ErrorCategory::Configuration,
            "resolve_xdg",
            "HOME environment variable is not set and XDG_STATE_HOME is not available",
        )
    })?;
    if home.is_empty() || !Path::new(&home).is_absolute() {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "resolve_xdg",
            "HOME is empty or not an absolute path; cannot resolve XDG state directory",
        ));
    }
    Ok(PathBuf::from(&home)
        .join(".local")
        .join("state")
        .join("fauna-scan"))
}

/// Resolve XDG paths using the real process environment.
pub fn resolve_real_xdg() -> AppResult<XdgPaths> {
    XdgPaths::resolve(|name| std::env::var(name).ok())
}

/// Resolve only the default config path using the real environment.
///
/// Does not require `XDG_STATE_HOME` or `HOME` if `XDG_CONFIG_HOME` is set.
pub fn resolve_real_xdg_config_path() -> AppResult<PathBuf> {
    resolve_xdg_config_dir(&|name| std::env::var(name).ok()).map(|dir| dir.join("config.toml"))
}

/// Resolve only the default database (state) path using the real environment.
///
/// Does not require `XDG_CONFIG_HOME` or `HOME` if `XDG_STATE_HOME` is set.
pub fn resolve_real_xdg_state_path() -> AppResult<PathBuf> {
    resolve_xdg_state_dir(&|name| std::env::var(name).ok())
        .map(|dir| dir.join("fauna-scan.sqlite3"))
}

/// Return the nearest existing writable ancestor of `path`, or `None` if
/// none exists.
///
/// Uses `symlink_metadata()` to detect symlinks. A broken symlink in the
/// ancestor chain is rejected (the function returns `None` at that point).
/// A valid symlink (whose target exists and is accessible) is accepted and
/// returned as the existing ancestor.
///
/// For relative paths whose ancestor chain reaches the empty path (""),
/// the empty path is treated as the current directory (".").
///
/// Returns `Some(path)` when the ancestor exists and is accessible.
/// The caller must still verify it is a writable directory.
fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors().skip(1) {
        // Empty relative path ("" from Path::parent() of a single-component
        // relative path) represents the current directory.
        let ancestor = if ancestor.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            ancestor.to_path_buf()
        };

        match ancestor.symlink_metadata() {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    // Symlink exists — check if target is accessible.
                    match ancestor.metadata() {
                        Ok(_) => {
                            // Valid symlink to an accessible target.
                            return Some(ancestor);
                        }
                        Err(_) => {
                            // Broken symlink — reject.
                            return None;
                        }
                    }
                }
                // Regular entry exists.
                return Some(ancestor);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // This component does not exist; continue to next ancestor.
                continue;
            }
            Err(_) => {
                // Other error (permission denied, etc.) — cannot verify this
                // ancestor; stop and return None so the caller can report it.
                return None;
            }
        }
    }
    None
}

// ── Effective access helpers (Linux) ───────────────────────────────────────

/// Check whether the effective user has write access to `path` using
/// `eaccess(2)` with `W_OK` via the safe `nix` crate wrapper.
///
/// Uses the effective UID/GID (not the real UID/GID) so that ownership,
/// ACLs, setuid/setgid, and root behavior are correctly determined.
///
/// Returns `Ok(())` when the path exists and is effectively writable.
/// Returns `Err` with the OS error when access is denied or the path
/// cannot be accessed.
fn effective_write_access(path: &Path) -> std::io::Result<()> {
    nix::unistd::eaccess(path, nix::unistd::AccessFlags::W_OK).map_err(std::io::Error::from)
}

/// Check whether the effective user has both write and execute/search access
/// to `path` using `eaccess(2)` with `W_OK | X_OK` via the safe `nix` crate
/// wrapper.
///
/// Directories require execute (search) permission in addition to write
/// permission to create or access entries within them.
///
/// Uses the effective UID/GID (not the real UID/GID) so that ownership,
/// ACLs, setuid/setgid, and root behavior are correctly determined.
///
/// Returns `Ok(())` when the path exists and is effectively writable and
/// searchable. Returns `Err` with the OS error when access is denied or the
/// path cannot be accessed.
fn effective_write_and_execute_access(path: &Path) -> std::io::Result<()> {
    nix::unistd::eaccess(
        path,
        nix::unistd::AccessFlags::W_OK | nix::unistd::AccessFlags::X_OK,
    )
    .map_err(std::io::Error::from)
}

/// Require an existing path to be a directory with effective write and search
/// access without modifying it.
pub(crate) fn require_effective_directory_access(path: &Path, label: &str) -> AppResult<()> {
    let meta = path.metadata().map_err(|e| {
        AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!(
                "cannot access metadata for {label} {}: {}",
                path.display(),
                safe_io_message(&e)
            ),
        )
    })?;

    if !meta.is_dir() {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!("{label} is not a directory: {}", path.display()),
        ));
    }

    // Use eaccess(2) for effective write AND execute/search check.
    // This respects ownership, ACLs, setuid/setgid, and root behavior.
    // Do NOT add a secondary Permissions::readonly() check: mode bits alone
    // do not establish effective writability and can override the
    // eaccess result under different ownership, ACLs, or root execution.
    if let Err(e) = effective_write_and_execute_access(path) {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!(
                "{label} is not writable or searchable: {} ({})",
                path.display(),
                safe_io_message(&e)
            ),
        ));
    }

    Ok(())
}

/// Validate that an existing database path is a regular writable file.
///
/// Rejects directories, special files, and non-writable files.
pub(crate) fn validate_existing_database_file(path: &Path) -> AppResult<()> {
    let meta = path.metadata().map_err(|e| {
        AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!(
                "cannot access metadata for database file {}: {}",
                path.display(),
                safe_io_message(&e)
            ),
        )
    })?;

    if meta.is_dir() {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!(
                "database_path exists but is a directory, not a regular file: {}",
                path.display()
            ),
        ));
    }

    if !meta.is_file() {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!("database_path is not a regular file: {}", path.display()),
        ));
    }

    // Check effective write access.
    if let Err(e) = effective_write_access(path) {
        return Err(AppError::new(
            ErrorCategory::Filesystem,
            "validate_paths",
            format!(
                "database file is not writable: {} ({})",
                path.display(),
                safe_io_message(&e)
            ),
        ));
    }

    Ok(())
}

/// Validate that filesystem paths are feasible without creating or modifying
/// any filesystem entries.
///
/// Uses `symlink_metadata()` to detect symlinks (including broken ones) at
/// every level: the target path itself, its immediate parent, and ancestor
/// directories. Broken symlinks are rejected rather than being silently
/// skipped.
///
/// Checks:
/// - If database_path exists, it must be a regular writable file.
/// - If database_path does not exist, its nearest existing parent must be a
///   writable directory.
/// - If output_directory exists, it must be a writable directory.
/// - If output_directory does not exist, its nearest existing parent must be
///   a writable directory.
pub fn validate_filesystem_paths(database_path: &Path, output_directory: &Path) -> AppResult<()> {
    // ── Database path ─────────────────────────────────────────────────────
    match database_path.symlink_metadata() {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                // Symlink exists — follow it and validate the resolved target.
                match database_path.metadata() {
                    Ok(_) => {
                        // Valid symlink — validate the resolved target is a
                        // regular writable file. metadata() follows the link.
                        validate_existing_database_file(database_path)?;
                    }
                    Err(e) => {
                        // Broken symlink — target does not exist or is not
                        // accessible.
                        return Err(AppError::new(
                            ErrorCategory::Filesystem,
                            "validate_paths",
                            format!(
                                "database_path is a broken symlink: {} ({})",
                                database_path.display(),
                                safe_io_message(&e)
                            ),
                        ));
                    }
                }
            } else {
                // Regular entry exists — validate it is a regular writable file.
                validate_existing_database_file(database_path)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // database_path does not exist — validate parent chain.
            let db_parent = database_path.parent().ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Configuration,
                    "validate_paths",
                    format!(
                        "database_path has no parent directory: {}",
                        database_path.display()
                    ),
                )
            })?;

            // Empty relative parent ("" from Path::parent() of a single-component
            // relative path) represents the current directory.
            let db_parent = if db_parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                db_parent.to_path_buf()
            };

            // Use symlink_metadata() on the immediate parent to detect
            // broken symlinks that metadata() would silently follow.
            match db_parent.symlink_metadata() {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        // Parent is a symlink — follow it and validate target.
                        match db_parent.metadata() {
                            Ok(_) => {
                                // Valid symlink — verify the resolved target
                                // is a writable directory.
                                require_effective_directory_access(&db_parent, "database parent")?;
                            }
                            Err(e) => {
                                return Err(AppError::new(
                                    ErrorCategory::Filesystem,
                                    "validate_paths",
                                    format!(
                                        "database parent is a broken symlink: {} ({})",
                                        db_parent.display(),
                                        safe_io_message(&e)
                                    ),
                                ));
                            }
                        }
                    } else {
                        // Parent exists as a real entry — verify writable directory.
                        require_effective_directory_access(&db_parent, "database parent")?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Parent doesn't exist — check nearest existing ancestor.
                    // nearest_existing_parent uses symlink_metadata() and
                    // rejects broken symlinks in the ancestor chain.
                    let ancestor = nearest_existing_parent(&db_parent).ok_or_else(|| {
                        AppError::new(
                            ErrorCategory::Filesystem,
                            "validate_paths",
                            format!(
                                "no existing parent directory for database path: {}",
                                db_parent.display()
                            ),
                        )
                    })?;
                    require_effective_directory_access(&ancestor, "database parent ancestor")?;
                }
                Err(e) => {
                    return Err(AppError::new(
                        ErrorCategory::Filesystem,
                        "validate_paths",
                        format!(
                            "cannot access database parent metadata {}: {}",
                            db_parent.display(),
                            safe_io_message(&e)
                        ),
                    ));
                }
            }
        }
        Err(e) => {
            // Other metadata error (permission denied, etc.)
            return Err(AppError::new(
                ErrorCategory::Filesystem,
                "validate_paths",
                format!(
                    "cannot access database_path {}: {}",
                    database_path.display(),
                    safe_io_message(&e)
                ),
            ));
        }
    }

    // ── Output directory ──────────────────────────────────────────────────
    match output_directory.symlink_metadata() {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                // Symlink exists — follow it and validate the resolved target.
                match output_directory.metadata() {
                    Ok(_) => {
                        // Valid symlink — verify the resolved target is a
                        // writable directory. metadata() follows the link.
                        require_effective_directory_access(output_directory, "output_directory")?;
                    }
                    Err(e) => {
                        // Broken symlink — target does not exist or is not
                        // accessible.
                        return Err(AppError::new(
                            ErrorCategory::Filesystem,
                            "validate_paths",
                            format!(
                                "output_directory is a broken symlink: {} ({})",
                                output_directory.display(),
                                safe_io_message(&e)
                            ),
                        ));
                    }
                }
            } else {
                // Regular entry exists — verify it is a writable directory.
                require_effective_directory_access(output_directory, "output_directory")?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // output_directory does not exist — validate parent chain.
            // nearest_existing_parent uses symlink_metadata() and rejects
            // broken symlinks in the ancestor chain.
            let ancestor = nearest_existing_parent(output_directory).ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Filesystem,
                    "validate_paths",
                    format!(
                        "no existing parent directory for output_directory: {}",
                        output_directory.display()
                    ),
                )
            })?;
            require_effective_directory_access(&ancestor, "output_directory parent")?;
        }
        Err(e) => {
            // Other metadata error (permission denied, etc.)
            return Err(AppError::new(
                ErrorCategory::Filesystem,
                "validate_paths",
                format!(
                    "cannot access output_directory {}: {}",
                    output_directory.display(),
                    safe_io_message(&e)
                ),
            ));
        }
    }

    Ok(())
}

/// Validate database and output paths based on the resolved database config.
///
/// For SQLite, validates the database file path using the existing checks.
/// For PostgreSQL, only validates the output directory (no filesystem checks
/// for the database connection URL).
pub fn validate_database_and_output_paths(
    database: &crate::configuration::DatabaseConfig,
    output_directory: &Path,
) -> AppResult<()> {
    match database {
        crate::configuration::DatabaseConfig::Sqlite { path, .. } => {
            validate_filesystem_paths(path, output_directory)
        }
        #[cfg(feature = "postgres")]
        crate::configuration::DatabaseConfig::Postgres { .. } => {
            // PostgreSQL does not need filesystem validation for the database.
            // Only validate the output directory.
            validate_output_directory(output_directory)
        }
    }
}

/// Validate only the output directory (for PostgreSQL or other backends
/// that do not require a local database file).
pub fn validate_output_directory(output_directory: &Path) -> AppResult<()> {
    match output_directory.symlink_metadata() {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                match output_directory.metadata() {
                    Ok(_) => {
                        require_effective_directory_access(output_directory, "output_directory")?
                    }
                    Err(e) => {
                        return Err(AppError::new(
                            ErrorCategory::Filesystem,
                            "validate_paths",
                            format!(
                                "output_directory is a broken symlink: {} ({})",
                                output_directory.display(),
                                safe_io_message(&e)
                            ),
                        ));
                    }
                }
            } else {
                require_effective_directory_access(output_directory, "output_directory")?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let ancestor = nearest_existing_parent(output_directory).ok_or_else(|| {
                AppError::new(
                    ErrorCategory::Filesystem,
                    "validate_paths",
                    format!(
                        "no existing parent directory for output_directory: {}",
                        output_directory.display()
                    ),
                )
            })?;
            require_effective_directory_access(&ancestor, "output_directory parent")?;
        }
        Err(e) => {
            return Err(AppError::new(
                ErrorCategory::Filesystem,
                "validate_paths",
                format!(
                    "cannot access output_directory {}: {}",
                    output_directory.display(),
                    safe_io_message(&e)
                ),
            ));
        }
    }
    Ok(())
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
    fn xdg_config_home_precedence() {
        let paths = XdgPaths::resolve(env_map(&[
            ("HOME", "/home/user"),
            ("XDG_CONFIG_HOME", "/custom/config"),
        ]))
        .unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/custom/config/fauna-scan"));
    }

    #[test]
    fn xdg_state_home_precedence() {
        let paths = XdgPaths::resolve(env_map(&[
            ("HOME", "/home/user"),
            ("XDG_STATE_HOME", "/custom/state"),
        ]))
        .unwrap();
        assert_eq!(paths.state_dir, PathBuf::from("/custom/state/fauna-scan"));
    }

    #[test]
    fn home_fallback_for_config() {
        let paths = XdgPaths::resolve(env_map(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/user/.config/fauna-scan")
        );
    }

    #[test]
    fn home_fallback_for_state() {
        let paths = XdgPaths::resolve(env_map(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/user/.local/state/fauna-scan")
        );
    }

    #[test]
    fn missing_home_without_xdg_fails() {
        let result = XdgPaths::resolve(env_map(&[]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(err.message.contains("HOME"));
    }

    #[test]
    fn xdg_config_home_without_home_succeeds() {
        // Only XDG_CONFIG_HOME is set, no HOME — config dir should resolve
        let paths = XdgPaths::resolve(env_map(&[("XDG_CONFIG_HOME", "/custom/config")]));
        // Should fail because state dir needs HOME and XDG_STATE_HOME is not set
        assert!(paths.is_err());
    }

    #[test]
    fn both_xdg_bases_without_home_succeeds() {
        // Both XDG_CONFIG_HOME and XDG_STATE_HOME are set; HOME is not needed.
        let paths = XdgPaths::resolve(env_map(&[
            ("XDG_CONFIG_HOME", "/custom/config"),
            ("XDG_STATE_HOME", "/custom/state"),
        ]))
        .unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/custom/config/fauna-scan"));
        assert_eq!(paths.state_dir, PathBuf::from("/custom/state/fauna-scan"));
    }

    #[test]
    fn empty_xdg_config_home_falls_back_to_home() {
        // Empty XDG_CONFIG_HOME is treated as unset per XDG rules.
        let paths =
            XdgPaths::resolve(env_map(&[("HOME", "/home/user"), ("XDG_CONFIG_HOME", "")])).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/user/.config/fauna-scan")
        );
    }

    #[test]
    fn relative_xdg_config_home_falls_back_to_home() {
        // Relative XDG_CONFIG_HOME is treated as unusable per XDG rules.
        let paths = XdgPaths::resolve(env_map(&[
            ("HOME", "/home/user"),
            ("XDG_CONFIG_HOME", "relative/path"),
        ]))
        .unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/user/.config/fauna-scan")
        );
    }

    #[test]
    fn empty_home_fails() {
        // Empty HOME should be rejected.
        let result = XdgPaths::resolve(env_map(&[("HOME", "")]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("HOME"));
        assert!(
            err.message.contains("empty") || err.message.contains("absolute"),
            "expected empty/absolute error, got: {}",
            err.message
        );
    }

    #[test]
    fn relative_home_fails() {
        // Relative HOME should be rejected.
        let result = XdgPaths::resolve(env_map(&[("HOME", "relative/home")]));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("HOME"));
    }

    #[test]
    fn default_config_path() {
        let paths = XdgPaths::resolve(env_map(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(
            paths.default_config_path(),
            PathBuf::from("/home/user/.config/fauna-scan/config.toml")
        );
    }

    #[test]
    fn default_database_path() {
        let paths = XdgPaths::resolve(env_map(&[("HOME", "/home/user")])).unwrap();
        assert_eq!(
            paths.default_database_path(),
            PathBuf::from("/home/user/.local/state/fauna-scan/fauna-scan.sqlite3")
        );
    }

    #[test]
    fn resolve_real_xdg_config_path_independent() {
        // This tests the standalone config-path resolver.
        // It should not fail just because HOME is set (it will find HOME).
        let result = resolve_real_xdg_config_path();
        assert!(result.is_ok());
    }

    #[test]
    fn validate_paths_existing_output_dir() {
        use std::sync::OnceLock;
        static TMP: OnceLock<tempfile::TempDir> = OnceLock::new();
        let dir = TMP.get_or_init(|| tempfile::tempdir().unwrap());
        let output = dir.path();
        let db = output.join("test.db");
        assert!(validate_filesystem_paths(&db, output).is_ok());
    }

    #[test]
    fn validate_paths_missing_output_with_usable_parent() {
        use std::sync::OnceLock;
        static TMP: OnceLock<tempfile::TempDir> = OnceLock::new();
        let dir = TMP.get_or_init(|| tempfile::tempdir().unwrap());
        let output = dir.path().join("new_output");
        let db = dir.path().join("test.db");
        assert!(validate_filesystem_paths(&db, &output).is_ok());
    }

    #[test]
    fn validate_paths_nonexistent_parent() {
        let db = Path::new("/nonexistent/parent/here/test.db");
        let output = Path::new("/nonexistent/parent/here/output");
        let result = validate_filesystem_paths(db, output);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(
            err.category,
            ErrorCategory::Filesystem | ErrorCategory::Configuration
        ));
    }

    #[test]
    fn validate_paths_readonly_db_parent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_parent = dir.path().join("db_parent");
        std::fs::create_dir(&db_parent).unwrap();

        // On Unix, set read-only permissions on the parent directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&db_parent).unwrap().permissions();
            perms.set_mode(0o555); // r-xr-xr-x (no write)
            std::fs::set_permissions(&db_parent, perms).unwrap();
        }

        let db = db_parent.join("test.db");
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        #[cfg(unix)]
        {
            let result = validate_filesystem_paths(&db, &output);
            assert!(
                result.is_err(),
                "read-only database parent should fail validation"
            );
            let err = result.unwrap_err();
            assert!(
                err.message.contains("not writable") || err.message.contains("read-only"),
                "expected writable/read-only error, got: {}",
                err.message
            );
        }

        #[cfg(not(unix))]
        {
            // On non-Unix, we can't easily test this; skip.
            let _ = db;
            let _ = output;
        }
    }

    #[test]
    fn validate_paths_non_directory_ancestor_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Create a regular file that will be an ancestor of the database path.
        let file_path = dir.path().join("not_a_dir");
        std::fs::write(&file_path, "data").unwrap();

        // Database path goes through this file: /tmp/.../not_a_dir/subdir/test.db
        // When the OS tries to stat not_a_dir/subdir, it returns ENOTDIR
        // because not_a_dir is a regular file, not a directory.
        let db = file_path.join("subdir").join("test.db");
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_err(),
            "non-directory ancestor should fail validation"
        );
        let err = result.unwrap_err();
        // The error comes from the OS (ENOTDIR) when trying to stat the parent
        // path, or from our own check. Either way, the message mentions the
        // non-directory problem.
        let msg_lower = err.message.to_lowercase();
        assert!(
            msg_lower.contains("not a directory") || msg_lower.contains("notadirectory"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[test]
    fn validate_paths_readonly_output_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("readonly_output");
        std::fs::create_dir(&output).unwrap();
        let db = dir.path().join("test.db");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&output).unwrap().permissions();
            perms.set_mode(0o555);
            std::fs::set_permissions(&output, perms).unwrap();

            let result = validate_filesystem_paths(&db, &output);
            assert!(
                result.is_err(),
                "read-only output directory should fail validation"
            );
            let err = result.unwrap_err();
            assert!(
                err.message.contains("not writable") || err.message.contains("read-only"),
                "expected writable/read-only error, got: {}",
                err.message
            );
        }

        #[cfg(not(unix))]
        {
            let _ = db;
        }
    }

    #[test]
    fn validate_existing_database_is_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fauna-scan.sqlite3");
        std::fs::create_dir(&db_path).unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let result = validate_filesystem_paths(&db_path, &output);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("directory"),
            "expected directory error, got: {}",
            err.message
        );
    }

    #[test]
    fn validate_existing_database_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fauna-scan.sqlite3");
        std::fs::write(&db_path, "").unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        assert!(validate_filesystem_paths(&db_path, &output).is_ok());
    }

    #[test]
    fn validate_existing_readonly_database_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fauna-scan.sqlite3");
        std::fs::write(&db_path, "").unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&db_path).unwrap().permissions();
            perms.set_mode(0o444); // read-only
            std::fs::set_permissions(&db_path, perms).unwrap();

            let result = validate_filesystem_paths(&db_path, &output);
            assert!(
                result.is_err(),
                "read-only database file should fail validation"
            );
            let err = result.unwrap_err();
            assert!(
                err.message.contains("not writable"),
                "expected not-writable error, got: {}",
                err.message
            );
        }

        #[cfg(not(unix))]
        {
            let _ = db_path;
            let _ = output;
        }
    }

    #[test]
    fn validate_non_directory_output_path_fails() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("output_file");
        std::fs::write(&output_path, "not a dir").unwrap();
        let db = dir.path().join("test.db");

        let result = validate_filesystem_paths(&db, &output_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("not a directory"),
            "expected not-a-directory error, got: {}",
            err.message
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_writable_but_not_executable_directory_fails() {
        // A directory with write permission but no execute/search permission
        // should fail validation because entries cannot be created or accessed.
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("no_exec_output");
        std::fs::create_dir(&output).unwrap();
        let db = dir.path().join("test.db");

        // Set mode to 0o200 (write-only, no read or execute)
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&output).unwrap().permissions();
        perms.set_mode(0o200);
        std::fs::set_permissions(&output, perms).unwrap();

        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_err(),
            "writable-but-not-executable directory should fail validation"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("not writable") || err.message.contains("not searchable"),
            "expected writable/searchable error, got: {}",
            err.message
        );
    }

    // ── Broken symlink tests ───────────────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn validate_broken_database_symlink_fails() {
        // A broken symlink at database_path should be rejected (not treated
        // as an absent creatable path).
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fauna-scan.sqlite3");
        // Create a symlink pointing to a nonexistent target
        symlink("/nonexistent-target-db-12345", &db_path).unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let result = validate_filesystem_paths(&db_path, &output);
        assert!(
            result.is_err(),
            "broken database symlink should fail validation"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("symlink"),
            "expected symlink error, got: {}",
            err.message
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_broken_output_symlink_fails() {
        // A broken symlink at output_directory should be rejected.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("output_link");
        // Create a symlink pointing to a nonexistent target
        symlink("/nonexistent-target-output-12345", &output_path).unwrap();
        let db = dir.path().join("test.db");

        let result = validate_filesystem_paths(&db, &output_path);
        assert!(
            result.is_err(),
            "broken output symlink should fail validation"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("symlink"),
            "expected symlink error, got: {}",
            err.message
        );
    }

    // ── Broken symlink ancestor tests ──────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn validate_broken_symlink_ancestor_of_database_parent_fails() {
        // When the database parent is a broken symlink, validation should
        // reject it (not silently skip to a higher writable ancestor).
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        // Create: dir/real_dir/broken_link/child/test.db
        let real_dir = dir.path().join("real_dir");
        std::fs::create_dir(&real_dir).unwrap();
        let broken_link = real_dir.join("broken_link");
        symlink("/nonexistent-broken-ancestor-target-67890", &broken_link).unwrap();
        let db = broken_link.join("child").join("test.db");
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_err(),
            "broken symlink ancestor of database parent should fail"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("symlink") || err.message.contains("broken"),
            "expected symlink/broken error, got: {}",
            err.message
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_broken_symlink_ancestor_of_output_fails() {
        // When an ancestor of output_directory is a broken symlink, validation
        // should reject it.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        // Create: dir/real_dir/broken_link/child/output
        let real_dir = dir.path().join("real_dir");
        std::fs::create_dir(&real_dir).unwrap();
        let broken_link = real_dir.join("broken_link");
        symlink(
            "/nonexistent-broken-output-ancestor-target-67890",
            &broken_link,
        )
        .unwrap();
        let output = broken_link.join("child").join("output");
        let db = dir.path().join("test.db");

        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_err(),
            "broken symlink ancestor of output should fail"
        );
        let err = result.unwrap_err();
        // The error may mention "no existing parent" because the broken
        // symlink causes nearest_existing_parent to return None.
        assert!(
            err.message.contains("no existing parent")
                || err.message.contains("symlink")
                || err.message.contains("broken"),
            "expected parent/symlink/broken error, got: {}",
            err.message
        );
    }

    // ── Valid symlink tests (reviewer feedback: follow valid symlinks) ─────

    #[test]
    #[cfg(unix)]
    fn validate_valid_database_file_symlink_succeeds() {
        // A symlink at database_path pointing to a valid writable regular file
        // should be accepted.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        // Create the real file
        let real_db = dir.path().join("real.db");
        std::fs::write(&real_db, "").unwrap();
        // Create a symlink to the real file
        let db_link = dir.path().join("fauna-scan.sqlite3");
        symlink(&real_db, &db_link).unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let result = validate_filesystem_paths(&db_link, &output);
        assert!(
            result.is_ok(),
            "valid database file symlink should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_valid_output_directory_symlink_succeeds() {
        // A symlink at output_directory pointing to a valid writable directory
        // should be accepted.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        // Create the real directory
        let real_output = dir.path().join("real_output");
        std::fs::create_dir(&real_output).unwrap();
        // Create a symlink to the real directory
        let output_link = dir.path().join("output_link");
        symlink(&real_output, &output_link).unwrap();
        let db = dir.path().join("test.db");

        let result = validate_filesystem_paths(&db, &output_link);
        assert!(
            result.is_ok(),
            "valid output directory symlink should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_valid_symlink_ancestor_of_output_succeeds() {
        // When an ancestor of output_directory is a valid symlink to a
        // writable directory, validation should succeed.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        // Create: dir/real_dir/ -> dir/link/ -> nonexistent/new_output
        let real_dir = dir.path().join("real_dir");
        std::fs::create_dir(&real_dir).unwrap();
        let link_dir = dir.path().join("link_dir");
        symlink(&real_dir, &link_dir).unwrap();
        // output is under the symlinked ancestor
        let output = link_dir.join("new_output");
        let db = dir.path().join("test.db");

        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_ok(),
            "valid symlink ancestor of output should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_valid_symlink_database_parent_succeeds() {
        // When the database parent is a valid symlink to a writable directory,
        // validation should succeed.
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        // Create: dir/real_parent/ -> dir/link_parent/
        let real_parent = dir.path().join("real_parent");
        std::fs::create_dir(&real_parent).unwrap();
        let link_parent = dir.path().join("link_parent");
        symlink(&real_parent, &link_parent).unwrap();
        let db = link_parent.join("test.db");
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_ok(),
            "valid symlink database parent should succeed: {:?}",
            result.err()
        );
    }

    // ── Relative path tests (reviewer feedback) ────────────────────────────

    /// Shared mutex to serialize tests that mutate the process-wide current
    /// directory, preventing races when tests run in parallel.
    static CURRENT_DIR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn validate_relative_single_component_db_succeeds() {
        // A single-component relative database path like "db.sqlite3"
        // should succeed when the current directory is writable.
        let _lock = CURRENT_DIR_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _guard = DirGuard(original_dir);

        // Create output dir in current dir
        std::fs::create_dir("output").unwrap();

        let db = Path::new("db.sqlite3");
        let output = Path::new("output");
        let result = validate_filesystem_paths(db, output);
        assert!(
            result.is_ok(),
            "relative single-component db path should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn validate_relative_single_component_output_succeeds() {
        // A single-component relative output path like "out"
        // should succeed when the current directory is writable.
        let _lock = CURRENT_DIR_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _guard = DirGuard(original_dir);

        // Create db file in the new current dir
        std::fs::write("test.db", "").unwrap();

        let db = Path::new("test.db");
        let output = Path::new("out");
        let result = validate_filesystem_paths(db, output);
        assert!(
            result.is_ok(),
            "relative single-component output path should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn validate_relative_nested_db_succeeds() {
        // A nested relative database path like "subdir/db.sqlite3"
        // where subdir exists should succeed.
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();

        let db = subdir.join("db.sqlite3");
        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_ok(),
            "relative nested db path should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn validate_relative_nested_output_succeeds() {
        // A nested relative output path like "subdir/out"
        // where subdir exists should succeed.
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let db = dir.path().join("test.db");
        std::fs::write(&db, "").unwrap();

        let output = subdir.join("out");
        let result = validate_filesystem_paths(&db, &output);
        assert!(
            result.is_ok(),
            "relative nested output path should succeed: {:?}",
            result.err()
        );
    }
}
