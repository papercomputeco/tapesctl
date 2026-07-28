//! Discover fork-parent lineage from the Claude transcript files.
//!
//! `~/.claude/sessions/<pid>.json` does NOT carry a `parentSessionId`
//! field. The fork-parent lineage is only recoverable by reading the
//! *transcript* at `~/.claude/projects/<cwd-encoded>/<sid>.jsonl`: the
//! first user message in a forked session carries a `parentUuid` that
//! resolves to a message `uuid` inside the parent session's transcript.
//!
//! The cwd-to-directory-name encoding is "replace `/` with `-`"; for
//! example, cwd `/Users/matt/git/paper-forest/groves/sessions` becomes
//! `-Users-matt-git-paper-forest-groves-sessions`.
//!
//! The scan is bounded to ~25 ms wall-clock. On timeout, missing
//! transcript, or no `parentUuid`, we return `None` and the request
//! handler proceeds without attaching `X-Tapes-Parent-Harness-Session-Id`.
//! That header is enrichment, not a routing input, so dropping it is
//! acceptable degradation.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

/// Wall-clock budget for fork-parent discovery. paperd MUST bound the
/// scan: if it takes more than ~25 ms, log a warning and proceed
/// *without* attaching the parent header.
///
/// 25 ms is empirically below the visual-flinch threshold on the warm
/// cache miss path. Increasing the budget would let one slow
/// transcript scan visibly delay an interactive turn; decreasing it
/// would make legitimate (deeply nested project dirs, cold pagecache)
/// scans degrade to no-parent unnecessarily.
const FORK_PARENT_BUDGET: Duration = Duration::from_millis(25);

/// Concurrency cap on in-flight `discover_parent` blocking scans.
/// Bounded so that a burst of cold cache requests against a slow
/// filesystem (NFS, network mount) cannot saturate the tokio blocking
/// pool — which would stall *every* other proxy operation that uses
/// `spawn_blocking`. The default tokio blocking pool is 512 threads;
/// 64 leaves ample headroom for any other blocking work the proxy
/// performs while still allowing reasonable parallelism for callers
/// arriving simultaneously.
const FORK_PARENT_CONCURRENCY: usize = 64;

fn discovery_semaphore() -> &'static Arc<Semaphore> {
    static SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| Arc::new(Semaphore::new(FORK_PARENT_CONCURRENCY)))
}

/// Bytes of the child transcript paperd reads to find the first user
/// message. The first user message lands within the first record or
/// two of every `.jsonl` we've inspected; 2 KiB covers that head with
/// margin while keeping the read bounded.
const HEAD_BYTES: usize = 2 * 1024;

/// Discover the fork-parent `harness_session_id` for a session given
/// the harness's working directory and session id.
///
/// 1. Read the first ~2 KiB of `~/.claude/projects/<cwd-encoded>/<sid>.jsonl`.
/// 2. Extract the first user message's `parentUuid`.
/// 3. Scan sibling `.jsonl` files for a record whose `uuid` matches.
/// 4. Return the owning session id (the filename sans `.jsonl`).
///
/// Returns `None` on:
/// * `parentUuid` is null or missing → not a forked session.
/// * Transcript missing or unreadable → the parent's machine, sandbox,
///   or trimmed transcript.
/// * Scan exceeded the 25 ms budget → degrade gracefully; the next
///   request for the same sid will hit the cache (caller memoizes).
///
/// Async because the scan does sync filesystem IO and we wrap it in
/// `tokio::task::spawn_blocking` so the runtime can preempt other
/// requests while the disk seeks. Bounded two ways:
/// * outer `tokio::time::timeout` returns control to the request handler
///   at the budget regardless of what the blocking task is doing;
/// * inner wall-clock deadline (`Deadline`) plumbed into every step of
///   the blocking scan so a stalled filesystem syscall (NFS, slow
///   disk) doesn't hold a blocking-pool thread past the budget.
///
/// A `Semaphore` caps total in-flight scans so a burst of cold cache
/// requests cannot saturate the blocking pool and starve other
/// `spawn_blocking` users in paperd.
pub async fn discover_parent(cwd: &str, sid: &str) -> Option<String> {
    let Some(projects_dir) = projects_dir_for(cwd) else {
        debug!("fork-parent: no home dir; skipping");
        return None;
    };
    let sid_owned = sid.to_owned();
    let sem = discovery_semaphore().clone();
    // acquire_owned() so the permit can travel into spawn_blocking,
    // releasing exactly when the blocking task finishes (or is
    // dropped). If the semaphore is closed for some reason, treat it
    // as overload and skip — no-parent fallback is always acceptable.
    let Ok(permit) = sem.acquire_owned().await else {
        debug!("fork-parent: discovery semaphore closed; skipping");
        return None;
    };
    let deadline = Deadline::starting_now(FORK_PARENT_BUDGET);
    let scan_deadline = deadline.clone();
    let scan = async move {
        let join = tokio::task::spawn_blocking(move || {
            let result =
                discover_parent_blocking_with_deadline(&projects_dir, &sid_owned, &scan_deadline);
            // Hold the permit until the blocking task is done so the
            // semaphore reflects true outstanding work (not just queued
            // futures awaiting the runtime).
            drop(permit);
            result
        });
        join.await.ok().flatten()
    };
    match tokio::time::timeout(FORK_PARENT_BUDGET, scan).await {
        Ok(parent) => parent,
        Err(_) => {
            warn!(
                budget_ms = FORK_PARENT_BUDGET.as_millis() as u64,
                "fork-parent: scan exceeded budget; degrading to no parent",
            );
            None
        }
    }
}

/// Synchronous core of [`discover_parent`]. Split out so the test
/// suite can exercise it without a tokio runtime — the timeout +
/// blocking-wrap live in the async wrapper.
///
/// Tests call this entry point; it uses an effectively-unbounded
/// deadline so the test surface stays unchanged. Production callers
/// go through `discover_parent_blocking_with_deadline`.
#[cfg(test)]
pub(crate) fn discover_parent_blocking(projects_dir: &Path, sid: &str) -> Option<String> {
    discover_parent_blocking_with_deadline(projects_dir, sid, &Deadline::infinite())
}

/// Production-path entry: same logic as [`discover_parent_blocking`]
/// but threads a wall-clock `Deadline` through every disk read so a
/// stalled syscall bails out of the blocking thread instead of
/// holding it past the outer `tokio::time::timeout`.
pub(crate) fn discover_parent_blocking_with_deadline(
    projects_dir: &Path,
    sid: &str,
    deadline: &Deadline,
) -> Option<String> {
    if deadline.exceeded() {
        return None;
    }
    let transcript = projects_dir.join(format!("{sid}.jsonl"));
    let head = read_head(&transcript, HEAD_BYTES)?;
    if deadline.exceeded() {
        return None;
    }
    let parent_uuid = extract_first_user_parent_uuid(&head)?;
    find_owning_sid_with_deadline(projects_dir, sid, &parent_uuid, deadline)
}

/// Wall-clock budget tracker. Held by the blocking scan so it can
/// short-circuit between filesystem syscalls — `tokio::time::timeout`
/// only stops *awaiting*, not the underlying blocking thread.
#[derive(Clone)]
pub(crate) struct Deadline {
    start: Instant,
    budget: Duration,
}

impl Deadline {
    fn starting_now(budget: Duration) -> Self {
        Self {
            start: Instant::now(),
            budget,
        }
    }

    /// Effectively-unbounded deadline for the test entry point.
    #[cfg(test)]
    fn infinite() -> Self {
        Self {
            start: Instant::now(),
            budget: Duration::from_secs(u64::MAX / 2),
        }
    }

    fn exceeded(&self) -> bool {
        self.start.elapsed() >= self.budget
    }
}

/// Compute the Claude projects subdirectory for a given cwd. The
/// harness encodes the cwd by replacing `/` with `-` (e.g. cwd
/// `/Users/matt` → directory `-Users-matt`). Returns the absolute
/// path under `~/.claude/projects/`.
fn projects_dir_for(cwd: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let encoded = encode_cwd(cwd);
    Some(home.join(".claude").join("projects").join(encoded))
}

/// `/foo/bar` → `-foo-bar`. The harness uses this as the project's
/// directory name.
///
/// Public because locating a session's transcript is harness knowledge a
/// capture client needs outside fork-parent discovery — the transcript lane
/// resolves `~/.claude/projects/<encode_cwd(cwd)>/` to find the files it
/// uploads.
///
/// This is the *producer* spelling and is deliberately left as-is: the two
/// envelope parsers disagree about how `cwd` is encoded, and reconciling them
/// is a contract decision tracked separately — not something to settle by
/// quietly changing this function.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Read up to `cap` bytes from the head of `path`. Used to bound the
/// IO cost of the first-user-message extraction — the first user
/// message lands within the first one or two records, well under 2 KiB
/// in all real samples we've inspected.
fn read_head(path: &Path, cap: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; cap];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Newline-delimited JSON: parse each line, find the first `type:user`
/// record, return its `parentUuid` if non-null.
pub(crate) fn extract_first_user_parent_uuid(bytes: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        type_: Option<String>,
        #[serde(rename = "parentUuid")]
        parent_uuid: Option<String>,
    }
    // Lines may be truncated at the head's tail; ignore parse errors
    // and keep looking. A first-user record near the start of the
    // file will be intact under any plausible budget.
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(trimmed) else {
            continue;
        };
        if rec.type_.as_deref() == Some("user") {
            return rec.parent_uuid.filter(|s| !s.is_empty());
        }
    }
    None
}

/// Scan sibling `.jsonl` files under `projects_dir` for a record whose
/// `uuid` matches `parent_uuid`. Returns the owning session's id (the
/// filename sans `.jsonl`). The transcript that triggered the scan
/// (`exclude_sid`'s `.jsonl`) is skipped — a session never forks
/// itself.
///
/// The match is a substring search on the file bytes (looking for
/// `"uuid":"<parent_uuid>"`); this is the cheapest way to handle the
/// usual case (parent record is deep inside a multi-MB transcript)
/// without parsing every record. The substring is uniquely identifying
/// — `uuid` values are random UUIDs — so a false positive is
/// astronomically unlikely.
#[cfg(test)]
pub(crate) fn find_owning_sid(
    projects_dir: &Path,
    exclude_sid: &str,
    parent_uuid: &str,
) -> Option<String> {
    find_owning_sid_with_deadline(
        projects_dir,
        exclude_sid,
        parent_uuid,
        &Deadline::infinite(),
    )
}

/// Production-path scan that respects a wall-clock deadline between
/// every file. The async wrapper's `tokio::time::timeout` returns
/// control to the caller at the budget, but the blocking thread keeps
/// running until it next checks the clock — without this hop the
/// thread can park on a slow open()/read() syscall well past 25 ms.
pub(crate) fn find_owning_sid_with_deadline(
    projects_dir: &Path,
    exclude_sid: &str,
    parent_uuid: &str,
    deadline: &Deadline,
) -> Option<String> {
    let needle = format!(r#""uuid":"{parent_uuid}""#);
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        // Fail-fast: a pathological directory of thousands of large
        // transcripts returns quickly instead of using the full
        // budget on a doomed scan. Checked once per file so we don't
        // overshoot between the outer await and the blocking thread.
        if deadline.exceeded() {
            return None;
        }
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if stem == exclude_sid {
            continue;
        }
        // `read` slurps the whole file; for typical transcripts (< a
        // few MB each) this beats line-by-line reading because the
        // substring scan is contiguous. If transcripts grow to tens
        // of MB the trade-off flips; revisit then.
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if find_subslice(&bytes, needle.as_bytes()) {
            return Some(stem.to_owned());
        }
    }
    None
}

/// Cheap substring search on raw bytes. `bytes.windows(needle.len()).any(...)`
/// is fine for the small needles (~50 bytes) and per-transcript byte
/// counts (< a few MB) we deal with; a Boyer-Moore would not move the
/// needle.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_replaces_slashes_with_dashes() {
        // paperd targets the directory Claude already uses: replace
        // `/` with `-`. Pinned byte-for-byte against a known cwd so a
        // future encoding change is an obvious diff.
        assert_eq!(
            encode_cwd("/Users/matt/git/paper-forest/groves/sessions"),
            "-Users-matt-git-paper-forest-groves-sessions",
        );
        assert_eq!(encode_cwd("/"), "-");
        assert_eq!(encode_cwd("/Users/matt"), "-Users-matt");
    }

    #[test]
    fn extract_parent_uuid_finds_first_user_record() {
        // First record is the session header (`type: "summary"` or
        // similar); the first `type: "user"` record is the one that
        // carries the parentUuid. Earlier user records would never
        // exist in a forked transcript, but a non-user record might
        // appear ahead of it.
        let head = br#"{"type":"summary","sessionId":"abc"}
{"type":"user","parentUuid":"PARENT-UUID-HERE","uuid":"child-1"}
{"type":"assistant","uuid":"child-2"}
"#;
        let p = extract_first_user_parent_uuid(head);
        assert_eq!(p.as_deref(), Some("PARENT-UUID-HERE"));
    }

    #[test]
    fn extract_parent_uuid_returns_none_when_null() {
        let head = br#"{"type":"user","parentUuid":null,"uuid":"root"}"#;
        assert!(extract_first_user_parent_uuid(head).is_none());
    }

    #[test]
    fn extract_parent_uuid_skips_non_user_records() {
        let head = br#"{"type":"summary","parentUuid":"WRONG"}
{"type":"user","parentUuid":"RIGHT","uuid":"x"}
"#;
        assert_eq!(
            extract_first_user_parent_uuid(head).as_deref(),
            Some("RIGHT"),
        );
    }

    #[test]
    fn extract_parent_uuid_returns_none_on_no_user_record() {
        let head = br#"{"type":"summary"}
{"type":"assistant","uuid":"a"}
"#;
        assert!(extract_first_user_parent_uuid(head).is_none());
    }

    #[test]
    fn extract_parent_uuid_skips_truncated_tail_line() {
        // A 2 KiB head may slice mid-record; the partial line must
        // not abort the scan — we want the first complete `type:user`
        // record. Here the tail line is intentionally broken JSON.
        let head = br#"{"type":"user","parentUuid":"GOOD","uuid":"x"}
{"type":"user","parentUuid":"BAD","uuid":"y","tru"#;
        assert_eq!(
            extract_first_user_parent_uuid(head).as_deref(),
            Some("GOOD"),
        );
    }

    #[test]
    fn find_owning_sid_returns_match() {
        let dir = tempfile::tempdir().unwrap();
        // Sibling transcript that contains the parent uuid.
        std::fs::write(
            dir.path().join("parent-sid.jsonl"),
            r#"{"type":"user","uuid":"PARENT-UUID","parentUuid":null}
{"type":"assistant","uuid":"other"}"#,
        )
        .unwrap();
        // The child transcript: must be skipped even though it
        // *mentions* the parent uuid as its parentUuid.
        std::fs::write(
            dir.path().join("child-sid.jsonl"),
            r#"{"type":"user","uuid":"child-1","parentUuid":"PARENT-UUID"}"#,
        )
        .unwrap();
        let got = find_owning_sid(dir.path(), "child-sid", "PARENT-UUID");
        assert_eq!(got.as_deref(), Some("parent-sid"));
    }

    #[test]
    fn find_owning_sid_returns_none_when_no_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("foo.jsonl"),
            r#"{"type":"user","uuid":"different"}"#,
        )
        .unwrap();
        assert!(find_owning_sid(dir.path(), "child", "PARENT-UUID").is_none());
    }

    #[test]
    fn find_owning_sid_excludes_the_child_transcript() {
        let dir = tempfile::tempdir().unwrap();
        // Only file in the dir is the child itself — mentioning its
        // own parentUuid. Must NOT match (a session never forks
        // itself).
        std::fs::write(
            dir.path().join("child.jsonl"),
            r#"{"type":"user","uuid":"child-1","parentUuid":"PARENT-UUID"}"#,
        )
        .unwrap();
        assert!(find_owning_sid(dir.path(), "child", "PARENT-UUID").is_none());
    }

    #[test]
    fn discover_parent_blocking_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path();
        // Parent's transcript: contains the uuid the child will name.
        std::fs::write(
            projects.join("parent-sid.jsonl"),
            r#"{"type":"summary"}
{"type":"user","uuid":"PARENT-UUID","parentUuid":null}
"#,
        )
        .unwrap();
        // Child's transcript: first user message names PARENT-UUID
        // as its parentUuid.
        std::fs::write(
            projects.join("child-sid.jsonl"),
            r#"{"type":"summary"}
{"type":"user","uuid":"child-1","parentUuid":"PARENT-UUID"}
"#,
        )
        .unwrap();
        let got = discover_parent_blocking(projects, "child-sid");
        assert_eq!(got.as_deref(), Some("parent-sid"));
    }

    #[test]
    fn discover_parent_blocking_returns_none_for_root_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("root-sid.jsonl"),
            r#"{"type":"user","uuid":"root-1","parentUuid":null}"#,
        )
        .unwrap();
        assert!(discover_parent_blocking(dir.path(), "root-sid").is_none());
    }

    #[test]
    fn discover_parent_blocking_missing_transcript_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_parent_blocking(dir.path(), "ghost").is_none());
    }

    #[test]
    fn already_exceeded_deadline_short_circuits_scan() {
        // Even a transcript that *would* match must return None when
        // the deadline is already exceeded at entry. Guards the
        // wall-clock fail-fast plumbed through every blocking step.
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path();
        std::fs::write(
            projects.join("parent.jsonl"),
            r#"{"type":"user","uuid":"P","parentUuid":null}"#,
        )
        .unwrap();
        std::fs::write(
            projects.join("child.jsonl"),
            r#"{"type":"user","uuid":"c","parentUuid":"P"}"#,
        )
        .unwrap();
        // Zero-budget deadline — `exceeded()` becomes true after the
        // first `Instant::now()` tick, which is well before the first
        // file open in a release build.
        let expired = Deadline {
            start: Instant::now() - Duration::from_secs(1),
            budget: Duration::from_millis(0),
        };
        let got = discover_parent_blocking_with_deadline(projects, "child", &expired);
        assert!(got.is_none());
    }

    #[test]
    fn find_owning_sid_with_deadline_bails_when_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("parent-sid.jsonl"),
            r#"{"type":"user","uuid":"PARENT-UUID","parentUuid":null}"#,
        )
        .unwrap();
        let expired = Deadline {
            start: Instant::now() - Duration::from_secs(1),
            budget: Duration::from_millis(0),
        };
        let got = find_owning_sid_with_deadline(dir.path(), "child-sid", "PARENT-UUID", &expired);
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn discover_parent_concurrency_cap_is_initialised() {
        // Smoke test: the semaphore must exist and have the expected
        // permit count so a regression that drops the cap (e.g. by
        // removing the OnceLock) is caught early.
        let sem = discovery_semaphore().clone();
        assert_eq!(sem.available_permits(), FORK_PARENT_CONCURRENCY);
    }
}
