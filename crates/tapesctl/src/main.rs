//! `tapesctl` — the Tapes client CLI.
//!
//! See the "Tapes and Cassettes" RFC for the intended surface. Every
//! hand-written command dispatches to a real implementation; what is still to
//! come is the *generated* `<cassette> <method>` surface, which arrives with
//! `/v1/cassettes` discovery in Track 4 and covers resources this binary cannot
//! know about at compile time.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    // Destination before discovery: a command that hands the terminal to a
    // harness must never trace onto it (see `tapesctl::logging`), and
    // discovery's own tracing is the only account of why an expected cassette
    // did not appear — so both the destination and the level are read off raw
    // argv, ahead of the parse that has not happened yet.
    let hands_over_terminal = argv
        .iter()
        .skip(1)
        .find(|argument| !argument.starts_with('-'))
        .is_some_and(|argument| argument == "start");
    tapesctl::logging::init(hands_over_terminal, tapesctl::cli::verbosity(&argv));

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
