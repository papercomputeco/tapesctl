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
            // The whole chain, not just the outermost message. Every error in
            // this crate is a typed wrapper around the one beneath it, and the
            // outermost is deliberately the least specific — "upgrade failed"
            // is a category, while the cause a user acts on ("sha256 mismatch",
            // "install directory is not writable; re-run the installer") lives
            // one or two links down. Printing only the top discards exactly the
            // half that says what to do about it.
            //
            // A cause that only repeats the line above it is skipped: some
            // wrappers render their source inline because that is the whole
            // diagnosis, and printing it twice reads as a stutter.
            //
            // reqwest's own wrappers ("error sending request for url",
            // "client error (Connect)", "tcp connect error") say nothing the
            // OS line under them does not, so they are skipped too.
            eprintln!("tapesctl: {err}");
            let refused = print_causes(&err);
            if refused {
                eprintln!(
                    "  hint: nothing is listening there; start one with `tapes serve`, or point --api-url or TAPES_API_URL at a running server"
                );
            }
            ExitCode::FAILURE
        }
    }
}

/// Print the cause chain under an error, one `caused by:` line each, and say
/// whether a connection was refused somewhere down it.
fn print_causes(err: &dyn std::error::Error) -> bool {
    let mut previous = err.to_string();
    let mut source = err.source();
    let mut refused = false;
    while let Some(cause) = source {
        let line = cause.to_string();
        refused |= line.contains("Connection refused");
        let boilerplate = line.starts_with("error sending request")
            || line.starts_with("client error (")
            || line == "tcp connect error";
        if !previous.contains(&line) && !boilerplate {
            eprintln!("  caused by: {line}");
        }
        previous = line;
        source = cause.source();
    }
    refused
}
