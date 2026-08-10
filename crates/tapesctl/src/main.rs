//! `tapesctl` — the Tapes client CLI.
//!
//! See the "Tapes and Cassettes" RFC for the intended surface. Alongside the
//! hand-written commands sits the *generated* `cassettes <name> <method>`
//! surface, discovered from `/v1/cassettes` at runtime, which covers resources
//! this binary cannot know about at compile time.

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

    // The CLI boundary, and the only place the configured defaults are read
    // from the machine: everything below takes the loaded value. A machine with
    // no home directory has no configuration file either, which is the same
    // state as an empty one — the flag and the environment still work.
    let config = tapesctl::machine::Machine::resolve()
        .map(|machine| tapesctl::config::load_or_default(machine.tapes_config_path()))
        .unwrap_or_default();

    // `resolve` rather than `Cli::parse`: the cassette commands are discovered
    // from the server before the command line is parsed, so that
    // `tapesctl cassettes <name> <method>` parses and the noun lists them.
    let invocation = tapesctl::resolve(argv, &config).await;

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
