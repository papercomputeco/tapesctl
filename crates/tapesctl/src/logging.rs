//! Where one invocation's tracing goes.
//!
//! This mirrors paperd's `log_init`: a destination decided from the caller's
//! *role* rather than from a TTY probe, a filter-precedence resolver kept pure
//! so it can be tested without mutating process environment, and one `init` that
//! installs the global subscriber before anything can emit.
//!
//! # Why this file exists at all
//!
//! paperd never needed a file destination. It is a daemon: it writes to stdout
//! and its supervisor — launchd's `StandardOutPath`, or the systemd journal —
//! owns the redirection. `tapesctl start` has no supervisor and no separate
//! terminal. It hands the user's TTY to a harness TUI and then keeps running
//! beside it, so every `tracing` event it emits after launch is drawn *into*
//! that TUI: Claude artifacts on startup, codex gets WARN lines painted through
//! a half-rendered frame.
//!
//! Hence the rule this module exists to enforce: **while a harness holds the
//! terminal, nothing may reach stdout or stderr.** Diagnostics do not stop —
//! they move to a file, and the path is printed while printing is still safe.
//!
//! Note what is deliberately *not* here: a fallback to stderr when the log file
//! cannot be opened. A missing diagnostic channel costs a debugging session; a
//! corrupted TUI costs the one the user is in the middle of.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use time::OffsetDateTime;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, fmt};

/// The log file this process installed, once [`init`] has run.
///
/// A `OnceLock` rather than a value threaded through the call graph: the path is
/// decided in `main`, before any command is dispatched, and needed again deep
/// inside `start` when it prints its exit summary. Threading it would put a
/// logging parameter on every command's signature to serve one of them.
static ACTIVE_LOG_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Whether this invocation's tracing belongs in a file rather than on the
/// terminal.
///
/// Two conditions, both required. `hands_over_terminal` is the whole reason —
/// only a command that launches a TUI can have its output corrupted by a log
/// line. `verbose == 0` is the escape hatch: `-v` is how a user says they want
/// to watch the capture happen, and someone who asks for a live stream has
/// accepted what it does to the display.
#[must_use]
pub fn wants_log_file(hands_over_terminal: bool, verbose: u8) -> bool {
    hands_over_terminal && verbose == 0
}

/// Install the global tracing subscriber for this invocation.
///
/// Call once, before anything that might trace. Installing twice is a no-op:
/// `try_init` fails and the failure is ignored, which is what makes this safe to
/// call from tests that may already have a subscriber.
pub fn init(hands_over_terminal: bool, verbose: u8) {
    let filter = choose_filter(std::env::var("RUST_LOG").ok().as_deref(), verbose);
    let registry = tracing_subscriber::registry().with(filter);

    if !wants_log_file(hands_over_terminal, verbose) {
        let _ = ACTIVE_LOG_FILE.set(None);
        let _ = registry
            .with(fmt::layer().with_writer(io::stderr))
            .try_init();
        return;
    }

    match open_log_file() {
        Ok((path, file)) => {
            let _ = ACTIVE_LOG_FILE.set(Some(path));
            // ANSI off. Nothing renders this file, and colour escapes turn a
            // `grep` for a request id into a puzzle. paperd never had to decide
            // this: its file destination is JSON, which is emitted uncoloured
            // whatever the terminal looks like.
            let _ = registry
                .with(fmt::layer().with_ansi(false).with_writer(file))
                .try_init();
        }
        Err(err) => {
            let _ = ACTIVE_LOG_FILE.set(None);
            // Say so exactly once, here, while stdout is still ours — and then
            // discard events rather than diverting them to stderr. See the
            // module docs for why this is the safe direction.
            println!("tapesctl: diagnostics disabled — no log file ({err})");
            let _ = registry.with(fmt::layer().with_writer(io::sink)).try_init();
        }
    }
}

/// The log file this run is writing to, if it is writing to one.
///
/// `None` covers all three of: tracing went to stderr, the file could not be
/// opened, and [`init`] was never called.
#[must_use]
pub fn active_log_file() -> Option<&'static Path> {
    ACTIVE_LOG_FILE.get()?.as_deref()
}

/// Where capture logs live: `~/.tapes/logs`.
///
/// Beside `~/.tapes/skills`, which this client already owns, rather than a
/// second user-level home invented for one file.
#[must_use]
pub fn default_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".tapes").join("logs"))
}

/// Resolve the level filter for this run.
///
/// Precedence is paperd's: `RUST_LOG` first, then the verbosity flag, then the
/// default. Pure — the environment is a parameter, not a read — so the whole
/// chain is testable without racing other tests over a process-global.
fn choose_filter(env_rust_log: Option<&str>, verbose: u8) -> EnvFilter {
    // A set-but-empty `RUST_LOG` is treated as unset. `EnvFilter` parses `""`
    // into a filter that discards everything, so honouring it would produce an
    // empty log file indistinguishable from a session that had nothing to say —
    // a support trap paperd hit and moved off `try_from_default_env` to avoid.
    // `RUST_LOG=off` remains the way to ask for silence.
    if let Some(directive) = env_rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        match EnvFilter::try_new(directive) {
            Ok(filter) => return filter,
            // Not `warn!`: no subscriber is installed yet. This reaches stderr,
            // which is safe because `init` runs before any harness launches.
            Err(err) => {
                eprintln!("tapesctl: ignoring invalid RUST_LOG {directive:?} ({err})");
            }
        }
    }
    EnvFilter::new(default_directive(verbose))
}

/// The filter used when nothing in the environment names one.
fn default_directive(verbose: u8) -> &'static str {
    match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    }
}

/// Create `~/.tapes/logs` if needed and open this run's file.
fn open_log_file() -> io::Result<(PathBuf, File)> {
    let dir = default_log_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(log_file_name(OffsetDateTime::now_utc(), std::process::id()));

    let mut options = OpenOptions::new();
    // Append, never truncate: a name collision must not destroy the log of a
    // capture that is still running.
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // A capture log names sessions, request ids and the acting subject.
        // That is the user's business and nobody else's.
        options.mode(0o600);
    }

    let file = options.open(&path)?;
    Ok((path, file))
}

/// This run's log file name: sortable by time, unique per process.
///
/// The pid is not decoration. Two `tapesctl start` sessions launched in the
/// same second would otherwise interleave into one file, and untangling two
/// captures after the fact is exactly the debugging this file exists to avoid.
fn log_file_name(now: OffsetDateTime, pid: u32) -> String {
    format!(
        "start-{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{pid}.log",
        year = now.year(),
        month = u8::from(now.month()),
        day = now.day(),
        hour = now.hour(),
        minute = now.minute(),
        second = now.second(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn only_a_default_start_logs_to_a_file() {
        // The whole point: the command that hands over the terminal, and only
        // when the user has not asked to watch.
        assert!(wants_log_file(true, 0));
        assert!(!wants_log_file(false, 0));
    }

    #[test]
    fn verbose_opts_back_into_the_terminal() {
        // `-v` on `start` is a request to watch the capture live; honouring it
        // is the documented way back to streaming.
        assert!(!wants_log_file(true, 1));
        assert!(!wants_log_file(true, 2));
    }

    #[test]
    fn rust_log_wins_over_the_verbosity_flag() {
        assert_eq!(choose_filter(Some("warn"), 2).to_string(), "warn");
    }

    #[test]
    fn an_empty_rust_log_is_unset_rather_than_silence() {
        // The trap this avoids: `export RUST_LOG=` left in a shell profile
        // yielding an empty log file that looks like a capture with nothing to
        // report.
        assert_eq!(choose_filter(Some(""), 0).to_string(), "info");
        assert_eq!(choose_filter(Some("   "), 0).to_string(), "info");
    }

    #[test]
    fn an_unparseable_rust_log_falls_back_instead_of_aborting() {
        // A typo in an env var must not stop a capture from running.
        //
        // Note how hard it is to write an invalid directive: `EnvFilter` reads
        // a bare word as a target name, so `RUST_LOG=nonsense` is *valid* and
        // means "trace for the target `nonsense`". Only a broken level, like
        // the one below, actually fails to parse.
        assert_eq!(
            choose_filter(Some("tapesctl=louder"), 0).to_string(),
            "info"
        );
    }

    #[test]
    fn the_verbosity_flag_sets_the_default_when_rust_log_is_absent() {
        assert_eq!(choose_filter(None, 0).to_string(), "info");
        assert_eq!(choose_filter(None, 1).to_string(), "debug");
        assert_eq!(choose_filter(None, 2).to_string(), "trace");
    }

    #[test]
    fn log_file_names_sort_by_time_and_separate_concurrent_captures() {
        let now = OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap();
        let name = log_file_name(now, 4242);
        assert!(name.starts_with("start-"), "got: {name}");
        assert!(name.ends_with("-4242.log"), "got: {name}");

        // Same instant, different process: distinct files.
        assert_ne!(name, log_file_name(now, 4243));
    }

    #[test]
    fn the_log_dir_sits_beside_the_other_tapes_state() {
        let dir = default_log_dir().unwrap();
        assert!(dir.ends_with("logs"), "got: {}", dir.display());
        assert!(
            dir.parent().is_some_and(|p| p.ends_with(".tapes")),
            "got: {}",
            dir.display(),
        );
    }
}
