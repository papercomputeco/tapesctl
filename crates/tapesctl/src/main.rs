//! `tapesctl` — the Tapes client CLI.
//!
//! See the "Tapes and Cassettes" RFC for the intended surface. This is the
//! Rust bootstrap: the CLI parses and dispatches, `version` and the canary
//! work today, and the capture/sync/plugin commands are wired but return
//! `NotImplemented` until their implementations land (Track 1 — the JIT proxy
//! and the `tapes-harness` extraction from paperd).

use std::process::ExitCode;

use clap::Parser;
use tapesctl::cli::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Destination before dispatch, because a command that hands the terminal to
    // a harness must never trace onto it. See `tapesctl::logging`.
    tapesctl::logging::init(
        cli.command
            .as_ref()
            .is_some_and(Command::hands_over_terminal),
        cli.verbose,
    );

    match tapesctl::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The daemon/proxy work will grow structured error reporting; for
            // now a single line to stderr is enough and keeps `main` panic-free.
            eprintln!("tapesctl: {err}");
            ExitCode::FAILURE
        }
    }
}
