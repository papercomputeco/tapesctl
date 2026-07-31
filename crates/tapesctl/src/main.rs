//! `tapesctl` — the Tapes client CLI.
//!
//! See the "Tapes and Cassettes" RFC for the intended surface. This is the
//! Rust bootstrap: the CLI parses and dispatches, `version` and the canary
//! work today, and the capture/sync/plugin commands are wired but return
//! `NotImplemented` until their implementations land (Track 1 — the JIT proxy
//! and the `tapes-harness` extraction from paperd).

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    // Tracing is installed before discovery runs, because discovery's own
    // tracing is the only account of why an expected cassette did not appear.
    tapesctl::init_tracing(tapesctl::cli::verbosity(&argv));

    // `resolve` rather than `Cli::parse`: the cassette nouns are discovered from
    // the server before the command line is parsed, so that
    // `tapesctl <cassette> <method>` parses and `--help` lists them.
    let invocation = tapesctl::resolve(argv).await;

    match tapesctl::dispatch(invocation).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The daemon/proxy work will grow structured error reporting; for
            // now a single line to stderr is enough and keeps `main` panic-free.
            eprintln!("tapesctl: {err}");
            ExitCode::FAILURE
        }
    }
}
