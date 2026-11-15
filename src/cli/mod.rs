//! Command-line interface (`anchor <command>`).
//!
//! Placeholder: the real CLI is assembled once the subsystems exist.

use std::process::ExitCode;

/// Entry point of the `anchor` binary.
pub fn run() -> ExitCode {
    eprintln!("anchor: the command-line interface is not available in this build");
    ExitCode::FAILURE
}
