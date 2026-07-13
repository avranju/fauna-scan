//! Fauna Scan — executable entry point.
//!
//! Parses CLI arguments, initializes structured logging, dispatches the
//! selected command, and exits with an appropriate status code.

use std::process::ExitCode;

use clap::Parser;
use fauna_scan::cli::Cli;
use fauna_scan::service_lifecycle::install_sanitized_panic_hook;
use fauna_scan::{app, logging};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialize logging; if this fails, report and exit without panicking.
    if let Err(e) = logging::init(cli.log_level) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // Install this before any primary pipeline can be spawned. Panic payloads
    // are not safe to print because they may contain external response data.
    install_sanitized_panic_hook();

    // Dispatch the selected command, passing the optional global config path.
    let config_path = cli.config.as_deref();
    match app::execute(cli.command, config_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(
                category = %e.category,
                operation = e.operation,
                "{}",
                e.message,
            );
            eprintln!("error: {} — {}", e.operation, e.message);
            ExitCode::FAILURE
        }
    }
}
