//! Command-line interface definitions for Fauna Scan.
//!
//! Uses Clap derive macros to define the complete CLI contract including
//! global options, subcommands, and command-specific arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Fauna Scan — retrieve NVR images and classify wildlife footage.
#[derive(Parser, Debug)]
#[command(name = "fauna-scan", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Path to the configuration file.
    #[arg(short = 'c', long = "config", global = true)]
    pub config: Option<PathBuf>,

    /// Logging level: error, warn, info, debug, trace.
    #[arg(long = "log-level", global = true, default_value_t = LogLevel::Info)]
    pub log_level: LogLevel,

    #[command(subcommand)]
    pub command: Command,
}

/// Operational subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start both downloader and scanner pipelines (continuous mode).
    Run,

    /// Serve the API without starting downloader or scanner workers.
    Web,

    /// Load and validate configuration without contacting external services.
    CheckConfig,

    /// Discover cameras connected to the NVR and print the results.
    Discover,

    /// Perform downloader work (discovery, search, download).
    Download(DownloadArgs),

    /// Process eligible images through the classifier.
    Scan(ScanArgs),

    /// Print database status counts grouped by download and processing state.
    Status,

    /// Manage web login credentials in the configured database.
    Users {
        #[command(subcommand)]
        command: UsersCommand,
    },
}

#[derive(Subcommand)]
pub enum UsersCommand {
    /// List user names (never passwords or hashes).
    List,
    /// Add a user with a salted password hash. Existing users are not replaced.
    Add {
        username: String,
        #[arg(allow_hyphen_values = true)]
        password: String,
    },
    /// Remove a user and revoke all their sessions.
    Remove { username: String },
}

impl std::fmt::Debug for UsersCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List => f.write_str("List"),
            Self::Add { username, .. } => f
                .debug_struct("Add")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .finish(),
            Self::Remove { username } => f
                .debug_struct("Remove")
                .field("username", username)
                .finish(),
        }
    }
}

/// Arguments for the `download` subcommand.
#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// Perform a single pass and exit.
    #[arg(long = "once")]
    pub once: bool,
}

/// Arguments for the `scan` subcommand.
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Process all eligible images once and exit.
    #[arg(long = "once")]
    pub once: bool,
}

/// Supported logging verbosity levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Return the tracing `EnvFilter` directive string for this level.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
