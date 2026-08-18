//! `tapesctl start` must leave the terminal alone.
//!
//! The bug these tests exist for is invisible to every other kind of test: the
//! proxy runs in the foreground of the terminal it hands to a harness TUI, so a
//! single `tracing` line emitted after launch is drawn into someone's
//! half-rendered frame. Unit tests cannot see it, and agent-driven smokes cannot
//! either — agents do not look at TUIs.
//!
//! So these run the real binary against a real (trivial) harness and assert on
//! the two streams a TUI shares: nothing may reach stderr, and stdout may carry
//! only the lines printed on either side of the harness's lifetime.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A `claude` on PATH that exits immediately.
///
/// The harness's behaviour is irrelevant here — what matters is that `start`
/// takes its full launch-and-exit path around a real child process, which is
/// where the leaking log lines are emitted.
fn fake_harness_dir(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("claude");
    fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Run the real binary with a private HOME and a private PATH.
///
/// Every `TAPES_*` variable is cleared: these read from the environment by
/// design, and a developer with one exported would otherwise change what the
/// test runs.
fn run_tapesctl(home: &Path, args: &[&str]) -> Output {
    let bin = fake_harness_dir(home);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default(),
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_tapesctl"));
    command
        .env("HOME", home)
        .env("PATH", path)
        .env_remove("RUST_LOG")
        .env_remove("TAPES_INGEST_URL")
        .env_remove("TAPES_UPSTREAM")
        .env_remove("TAPES_WEB_URL")
        .env_remove("TAPES_ORG_ID")
        .env_remove("TAPES_AUTH_SUBJECT")
        .args(args);
    command.output().unwrap()
}

/// `start`, with nothing listening on the ingest URL — no turns are captured,
/// which is fine: the log lines under test are emitted before any turn is.
fn start_args() -> Vec<&'static str> {
    vec![
        "start",
        "claude",
        "--tapes-url",
        "http://127.0.0.1:1",
        "--no-transcripts",
    ]
}

fn log_dir(home: &Path) -> PathBuf {
    home.join(".tapes").join("logs")
}

/// The single log file this run produced.
fn only_log(home: &Path) -> String {
    let dir = log_dir(home);
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("no log dir at {}: {err}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        paths.len(),
        1,
        "expected exactly one log file, got {paths:?}"
    );
    fs::read_to_string(paths.pop().unwrap()).unwrap()
}

#[test]
fn a_default_start_writes_nothing_to_stderr() {
    // The headline assertion. `start` emits an INFO the moment the proxy binds
    // ("capture proxy listening") and warns from several places after that; on
    // a default run every one of them must land in a file instead.
    let home = tempfile::tempdir().unwrap();
    let out = run_tapesctl(home.path(), &start_args());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.is_empty(),
        "stderr belongs to the harness TUI, but got: {stderr}",
    );
}

#[test]
fn the_diagnostics_a_default_start_suppresses_are_in_the_log_file() {
    // Suppressing the leak by dropping the diagnostics would pass the test
    // above and defeat the point. They have to still exist, just elsewhere.
    let home = tempfile::tempdir().unwrap();
    let out = run_tapesctl(home.path(), &start_args());
    assert!(out.status.success(), "start failed: {out:?}");

    let log = only_log(home.path());
    assert!(
        log.contains("capture proxy listening"),
        "the proxy's own startup line is missing from the log: {log}",
    );
}

#[test]
fn stdout_carries_the_log_path_and_no_tracing() {
    // stdout is allowed exactly two moments: before the harness is spawned and
    // after it exits. What it must never carry is tracing output.
    let home = tempfile::tempdir().unwrap();
    let out = run_tapesctl(home.path(), &start_args());

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("logs at"),
        "the user is never told where the diagnostics went: {stdout}",
    );
    assert!(
        stdout.contains(&log_dir(home.path()).display().to_string()),
        "the printed path is not the log dir: {stdout}",
    );
    assert!(
        !stdout.contains("capture proxy listening"),
        "tracing reached stdout: {stdout}",
    );
}

#[test]
fn verbose_opts_back_into_streaming_and_writes_no_file() {
    // `-v` is the documented way to watch a capture live. It has to actually
    // put the events back on the terminal, or the flag is a lie.
    //
    // Before `start`, not after: `harness_args` is a trailing var-arg, so a
    // trailing `-v` would be handed to the harness instead.
    let home = tempfile::tempdir().unwrap();
    let mut args = vec!["-v"];
    args.extend(start_args());
    let out = run_tapesctl(home.path(), &args);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("capture proxy listening"),
        "-v did not stream to stderr: {stderr}",
    );
    assert!(
        !log_dir(home.path()).exists(),
        "-v should not also open a log file",
    );
}

#[test]
fn a_command_that_keeps_the_terminal_still_logs_to_it() {
    // Only `start` changes destination. Redirecting every command would move
    // diagnostics away from the terminal for people who are reading them there.
    let home = tempfile::tempdir().unwrap();
    let out = run_tapesctl(home.path(), &["version"]);

    assert!(out.status.success(), "version failed: {out:?}");
    assert!(
        !log_dir(home.path()).exists(),
        "a non-start command should not open a log file",
    );
}
