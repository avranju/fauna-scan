//! Command dispatch for Fauna Scan.
//!
//! Keeps the process entry point thin and centralizes command routing.

use crate::cli::Command;
use crate::error::{AppError, AppResult};

/// Execute the selected command.
///
/// In Phase 1 every operational command returns a contextual
/// "not yet implemented" error. Later phases replace each branch.
pub async fn execute(command: Command) -> AppResult<()> {
    match command {
        Command::Run => Err(AppError::not_implemented("run")),
        Command::CheckConfig => Err(AppError::not_implemented("check-config")),
        Command::Discover => Err(AppError::not_implemented("discover")),
        Command::Download(_) => Err(AppError::not_implemented("download")),
        Command::Scan(_) => Err(AppError::not_implemented("scan")),
        Command::Status => Err(AppError::not_implemented("status")),
    }
}
