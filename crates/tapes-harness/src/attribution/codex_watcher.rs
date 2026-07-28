//! Polls Codex JSONL transcripts and exposes recent `session_meta` rows.
//!
//! This intentionally does not try to map processes to sessions. Codex
//! transcript files are not PID-indexed, so the request path only uses a
//! session from this watcher when the candidate set is unambiguous.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use time::OffsetDateTime;

use super::codex_session::{CodexSessionFile, read};

const RETENTION: time::Duration = time::Duration::hours(24);

#[derive(Debug, Default)]
pub struct CodexWatcherSnapshot {
    pub sessions: Vec<CodexSessionFile>,
}

pub type Snapshot = Arc<ArcSwap<CodexWatcherSnapshot>>;

#[must_use]
pub fn spawn(sessions_dir: PathBuf) -> Snapshot {
    let initial = scan(&sessions_dir);
    let snapshot: Snapshot = Arc::new(ArcSwap::from_pointee(initial));

    let weak = Arc::downgrade(&snapshot);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let Some(slot) = weak.upgrade() else {
                break;
            };
            let dir = sessions_dir.clone();
            let next = match tokio::task::spawn_blocking(move || scan(&dir)).await {
                Ok(next) => next,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "codex-session-watcher: scan task failed",
                    );
                    continue;
                }
            };
            slot.store(Arc::new(next));
        }
    });

    snapshot
}

fn scan(dir: &Path) -> CodexWatcherSnapshot {
    let cutoff = OffsetDateTime::now_utc() - RETENTION;
    let mut out = CodexWatcherSnapshot::default();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(next_dir) = stack.pop() {
        let entries = match std::fs::read_dir(&next_dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::debug!(
                    dir = %next_dir.display(),
                    error = %err,
                    "codex-session-watcher: could not read sessions dir",
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_none_or(|ext| ext != "jsonl")
            {
                continue;
            }
            let Some(session) = read(&path) else {
                continue;
            };
            if session.timestamp >= cutoff || session.modified_at.is_some_and(|ts| ts >= cutoff) {
                out.sessions.push(session);
            }
        }
    }

    out.sessions.sort_by_key(|session| session.timestamp);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_nested_recent_jsonl_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("06").join("15");
        std::fs::create_dir_all(&day).unwrap();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        std::fs::write(
            day.join("rollout-test.jsonl"),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"sid-1","timestamp":"{now}","cwd":"/tmp","model_provider":"paper-openai"}}}}"#
            ),
        )
        .unwrap();

        let got = scan(dir.path());
        assert_eq!(got.sessions.len(), 1);
        assert_eq!(got.sessions[0].session_id, "sid-1");
    }
}
