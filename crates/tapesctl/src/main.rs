//! `tapesctl` — the Tapes client CLI.
//!
//! See the "Tapes and Cassettes" RFC for the intended surface. Every
//! hand-written command dispatches to a real implementation; what is still to
//! come is the *generated* `<cassette> <method>` surface, which arrives with
//! `/v1/cassettes` discovery in Track 4 and covers resources this binary cannot
//! know about at compile time.

use std::process::ExitCode;

use clap::Parser;
use tapesctl::cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tapesctl::init_tracing(cli.verbose);

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
