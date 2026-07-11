//! Fauna Scan — executable entry point.
//!
//! Parses CLI arguments, initializes structured logging, dispatches the
//! selected command, and exits with an appropriate status code.

use std::process::ExitCode;

use clap::Parser;
use fauna_scan::cli::Cli;
use fauna_scan::{app, logging};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialize logging; if this fails, report and exit without panicking.
    if let Err(e) = logging::init(cli.log_level) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // Dispatch the selected command.
    match app::execute(cli.command).await {
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
