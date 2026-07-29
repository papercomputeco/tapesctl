//! Polls `~/.claude/sessions/` and maintains the candidate-PID set
//! plus its parsed metadata.
//!
//! A single snapshot is exposed via `ArcSwap` so the per-request hot
//! path reads it wait-free:
//!
//! * `candidate_pids` — the set of PIDs the peer-PID lookup
//!   ([`super::peer_pid::lookup`]) is restricted to. Restricting keeps
//!   the lookup bounded regardless of how busy the system is
//!   (measured macOS p99 78 µs over a 3-candidate set).
//! * `pid_metadata` — parsed `~/.claude/sessions/<pid>.json` for each
//!   candidate, so the request handler can attach `X-Tapes-*` headers
//!   without doing disk IO on the hot path.
//!
//! Bundling them in [`WatcherSnapshot`] behind one `ArcSwap` means the
//! handler can do a single `.load()` per attribution attempt, and a
//! watcher swap is atomic across both fields. A two-`ArcSwap` design
//! tore on the swap boundary: a request could see a PID in the new
//! candidate set but still hold the old metadata map (or vice versa),
//! and silently drop into the unknown-harness path despite the PID
//! being known.
//!
//! The watcher refreshes on at most a 1-second cadence — slower than
//! that and a freshly-started Claude session can land a request before
//! its `<pid>.json` is in the candidate set. The forwarding path waits
//! briefly for Claude-looking requests to cover that cold-race window.
//! A FSEvents/inotify implementation would close the window to
//! sub-millisecond; the poll plus bounded wait is the current approach.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;

use super::claude_session::{ClaudeSessionFile, read};

/// Combined wait-free snapshot of the candidate-PID set and the
/// parsed metadata for each candidate. Bundling them under a single
/// `ArcSwap` guarantees per-request consistency: the handler does
/// one `.load()` and either sees both fields at watcher tick N or
/// both at tick N+1 — never one from each side of a swap.
#[derive(Debug, Default)]
pub struct WatcherSnapshot {
    /// PIDs the peer-PID lookup is restricted to. Empty when
    /// `~/.claude/sessions/` is missing or empty, in which case every
    /// request lands on the unknown-harness path.
    pub candidate_pids: HashSet<i32>,
    /// Parsed `<pid>.json` metadata. A PID may be in `candidate_pids`
    /// without an entry here when its `<pid>.json` failed to parse —
    /// the handler treats that as the cold-race / unknown-harness
    /// fallback.
    pub pid_metadata: HashMap<i32, ClaudeSessionFile>,
}

/// Wait-free handle to the combined watcher snapshot. The forwarding
/// pipeline `.load()`s on every request; the watcher swaps in a fresh
/// `Arc<WatcherSnapshot>` once per poll.
pub type Snapshot = Arc<ArcSwap<WatcherSnapshot>>;

/// Spawn the candidate-set watcher on the current tokio runtime. The
/// returned [`Snapshot`] is cloned into `ProxyState`; the spawned
/// task holds a `Weak<...>` reference and exits cleanly when the last
/// snapshot owner drops.
///
/// MUST be called from within a tokio runtime — the watcher is a
/// `tokio::spawn`. The paperd shell sets up the runtime before
/// `ProxyServer::run` so the only caller (`ProxyServer::new`) is fine.
///
/// `sessions_dir` is typically `~/.claude/sessions/` per
/// [`super::claude_session::default_sessions_dir`]; tests pass a
/// tempdir.
///
/// The initial scan is deliberately inline: `spawn` is called during
/// proxy construction, before any request can observe the snapshot, so
/// paying for it on the caller's thread is what makes the very first
/// request see a populated candidate set. Every *periodic* scan is
/// offloaded with `spawn_blocking` — [`scan`] does directory iteration
/// plus a `read(2)` per session file, and running that inline on a
/// tokio worker stalls every future that worker owns for the duration
/// of the scan. The Codex watcher already offloads the same way; this
/// mirrors it.
#[must_use]
pub fn spawn(sessions_dir: PathBuf) -> Snapshot {
    let initial = scan(&sessions_dir);
    let snapshot: Snapshot = Arc::new(ArcSwap::from_pointee(initial));

    let weak = Arc::downgrade(&snapshot);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // skip the first immediate tick — we already loaded the
        // initial snapshot above.
        interval.tick().await;
        loop {
            interval.tick().await;
            // If the snapshot owner has dropped, the watcher has
            // nothing to update — exit cleanly.
            let Some(slot) = weak.upgrade() else {
                break;
            };
            let dir = sessions_dir.clone();
            // A failed join (blocking task panicked, or the runtime is
            // shutting down) leaves the previous snapshot in place
            // rather than clearing it — a scan we could not run is not
            // evidence that the sessions dir is empty.
            let next = match tokio::task::spawn_blocking(move || scan(&dir)).await {
                Ok(next) => next,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "session-watcher: scan task failed",
                    );
                    continue;
                }
            };
            slot.store(Arc::new(next));
        }
    });

    snapshot
}

/// Scan `~/.claude/sessions/` and return the candidate PIDs alongside
/// the parsed metadata for each. Non-numeric filenames and non-`.json`
/// entries are skipped. IO errors are logged at `debug` and an empty
/// pair is returned — the caller swaps it, which effectively clears
/// the snapshot until the next poll succeeds.
///
/// Parsing failures for an individual `<pid>.json` (warned by
/// [`super::claude_session::read`]) drop that PID from the metadata
/// map but keep it in the candidate set so peer-PID lookup can still
/// match the socket. The handler treats "PID in candidates, no
/// metadata" as the cold-race / unknown-harness fallback.
fn scan(dir: &Path) -> WatcherSnapshot {
    let mut snapshot = WatcherSnapshot::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(
                dir = %dir.display(),
                error = %e,
                "session-watcher: could not read sessions dir",
            );
            return snapshot;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name
            .to_string_lossy()
            .strip_suffix(".json")
            .map(str::to_owned)
        else {
            continue;
        };
        let Ok(pid) = stem.parse::<i32>() else {
            continue;
        };
        snapshot.candidate_pids.insert(pid);
        if let Some(file) = read(dir, pid) {
            snapshot.pid_metadata.insert(pid, file);
        }
    }
    snapshot
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn scan_parses_pid_filenames_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(
            p.join("123.json"),
            r#"{"pid":123,"sessionId":"abc","cwd":"/x"}"#,
        )
        .unwrap();
        std::fs::write(
            p.join("456.json"),
            r#"{"pid":456,"sessionId":"def","cwd":"/y"}"#,
        )
        .unwrap();
        std::fs::write(p.join("not-a-pid.json"), "{}").unwrap();
        std::fs::write(p.join("789.txt"), "{}").unwrap();
        let snap = scan(p);
        assert!(snap.candidate_pids.contains(&123));
        assert!(snap.candidate_pids.contains(&456));
        assert_eq!(snap.candidate_pids.len(), 2);
        assert_eq!(snap.pid_metadata.get(&123).unwrap().session_id, "abc");
        assert_eq!(snap.pid_metadata.get(&456).unwrap().session_id, "def");
    }

    #[test]
    fn scan_unparseable_metadata_drops_from_meta_keeps_in_pids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("777.json"), "not json").unwrap();
        let snap = scan(dir.path());
        // PID still in candidates so peer-PID lookup can match the
        // socket, but no metadata to attach — handler will fall back
        // to unknown-harness.
        assert!(snap.candidate_pids.contains(&777));
        assert!(!snap.pid_metadata.contains_key(&777));
    }

    #[test]
    fn scan_missing_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let snap = scan(&missing);
        assert!(snap.candidate_pids.is_empty());
        assert!(snap.pid_metadata.is_empty());
    }

    #[tokio::test]
    async fn spawn_starts_with_initial_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("42.json"),
            r#"{"pid":42,"sessionId":"hello"}"#,
        )
        .unwrap();
        let snap = spawn(dir.path().to_path_buf());
        let loaded = snap.load();
        assert!(loaded.candidate_pids.contains(&42));
        assert_eq!(loaded.pid_metadata.get(&42).unwrap().session_id, "hello");
    }

    /// Build a snapshot containing a single candidate PID and matching
    /// metadata. Used by the swap-atomicity test to push fresh
    /// snapshots without depending on the full `ClaudeSessionFile`
    /// schema (round-trips a minimal JSON instead).
    #[cfg(test)]
    fn make_snapshot(pid: i32, tick: i64) -> WatcherSnapshot {
        let mut next = WatcherSnapshot::default();
        next.candidate_pids.insert(pid);
        let raw = format!(r#"{{"pid":{pid},"sessionId":"sid-{tick}","cwd":"/"}}"#);
        let meta: ClaudeSessionFile = serde_json::from_str(&raw).unwrap();
        next.pid_metadata.insert(pid, meta);
        next
    }

    #[cfg(test)]
    async fn drive_writer(snap: Snapshot) {
        for tick in 0..200i64 {
            let pid = 1000 + (tick as i32 % 5);
            snap.store(Arc::new(make_snapshot(pid, tick)));
            tokio::task::yield_now().await;
        }
    }

    #[cfg(test)]
    async fn drive_reader(snap: Snapshot) {
        for _ in 0..2_000 {
            let loaded = snap.load();
            assert_snapshot_consistent(&loaded);
            tokio::task::yield_now().await;
        }
    }

    #[cfg(test)]
    fn assert_snapshot_consistent(loaded: &WatcherSnapshot) {
        for pid in &loaded.candidate_pids {
            // Contract: a PID observed in the candidate set in this
            // load must have metadata in this same load. The cold-race
            // path is the watcher intentionally publishing a candidate
            // without metadata; here we never produce such a snapshot,
            // so any miss would be a torn read.
            assert!(
                loaded.pid_metadata.contains_key(pid),
                "candidate {pid} present without metadata — torn snapshot read",
            );
        }
    }

    /// Block until `cond` holds, or `limit` elapses. Returns whether
    /// it held. Deliberately uses `std::thread::sleep`: the callers
    /// observe a runtime from the outside, and in the regression case
    /// the runtime's timer is exactly what stops advancing.
    #[cfg(unix)]
    fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if cond() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Create a FIFO at `path`. A `std::fs::read` of a FIFO parks in
    /// `open(2)` until a writer shows up, which is how the test below
    /// pins a scan in progress for as long as it needs to.
    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives
        // the call, and `mkfifo` only reads through the pointer.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            rc,
            0,
            "mkfifo({}) failed: {}",
            path.display(),
            std::io::Error::last_os_error(),
        );
    }

    /// Wait for a reader to park in `open(2)` on the FIFO at `path`,
    /// returning the write end. `O_WRONLY | O_NONBLOCK` on a FIFO fails
    /// with `ENXIO` while no reader is waiting and succeeds once one
    /// is, so this is an exact "the scan has started and is now
    /// blocked" signal rather than a sleep-and-hope.
    #[cfg(unix)]
    fn wait_for_fifo_reader(path: &Path) -> Option<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;

        let mut opened = None;
        wait_until(Duration::from_secs(15), || {
            opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .ok();
            opened.is_some()
        });
        opened
    }

    /// The periodic scan must not run on a tokio worker. [`scan`] does
    /// directory iteration plus a `read(2)` per session file; inline on
    /// a worker it stalls every other future that worker owns for the
    /// duration of the scan — on paperd that is the request-forwarding
    /// path the attribution exists to serve.
    ///
    /// The setup makes that observable without timing heuristics: one
    /// worker thread, and a FIFO named `<pid>.json` in the sessions dir
    /// so the scan parks indefinitely instead of finishing in
    /// microseconds. A heartbeat task then answers the question
    /// directly — inline, the sole worker is pinned and the heartbeat
    /// cannot advance; offloaded to `spawn_blocking`, it keeps counting
    /// while the scan is stuck.
    #[cfg(unix)]
    #[test]
    fn periodic_scan_does_not_block_the_runtime_worker() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("4242.json");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        // The initial scan is inline in `spawn` by design, so the FIFO
        // must not exist yet — otherwise this thread parks, not a
        // worker, and the test would be measuring the wrong scan.
        let snap = rt.block_on(async { spawn(dir.path().to_path_buf()) });
        mkfifo(&fifo);

        let beats = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&beats);
        rt.spawn(async move {
            loop {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        });

        let writer = wait_for_fifo_reader(&fifo)
            .expect("watcher never opened the FIFO — no periodic scan ran");

        let before = beats.load(Ordering::Relaxed);
        let progressed = wait_until(Duration::from_secs(5), || {
            beats.load(Ordering::Relaxed) > before
        });
        assert!(
            progressed,
            "the runtime's only worker made no progress while a sessions-dir \
             scan was in flight — the scan is running inline on the worker",
        );

        // Teardown, in order: drop the snapshot so the watcher loop
        // breaks at its next tick; unlink the FIFO so a re-scan cannot
        // park on it; then release the parked reader by closing the
        // write end. Without this the runtime's drop would wait forever
        // on a blocking task that can never finish.
        drop(snap);
        std::fs::remove_file(&fifo).unwrap();
        drop(writer);
    }

    /// Offloading the scan must not cost the watcher its actual job:
    /// a session file that appears after startup still has to reach the
    /// published snapshot.
    #[tokio::test]
    async fn periodic_scan_publishes_sessions_that_appear_after_startup() {
        let dir = tempfile::tempdir().unwrap();
        let snap = spawn(dir.path().to_path_buf());
        assert!(snap.load().candidate_pids.is_empty());

        std::fs::write(
            dir.path().join("4321.json"),
            r#"{"pid":4321,"sessionId":"appeared-later"}"#,
        )
        .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !snap.load().candidate_pids.contains(&4321) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "periodic scan never published the new session file",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            snap.load().pid_metadata.get(&4321).unwrap().session_id,
            "appeared-later",
        );
    }

    #[tokio::test]
    async fn snapshot_swap_is_atomic_across_candidate_pids_and_pid_metadata() {
        // A two-`ArcSwap` design (one for `candidate_pids`, one for
        // `pid_metadata`) would let a request see a PID present in the
        // new candidate set but missing from the old metadata map (or
        // vice versa). With the combined snapshot a single `.load()`
        // returns both fields from the same tick.
        let snap: Snapshot = Arc::new(ArcSwap::from_pointee(WatcherSnapshot::default()));
        let writer = tokio::spawn(drive_writer(snap.clone()));
        let reader = tokio::spawn(drive_reader(snap.clone()));
        writer.await.unwrap();
        reader.await.unwrap();
    }
}
