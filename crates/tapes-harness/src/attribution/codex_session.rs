//! Verbatim session metadata from Codex JSONL transcripts.
//!
//! Codex writes transcript files under
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<session-id>.jsonl`
//! (or `$CODEX_HOME/sessions/...`). The first JSONL row is
//! `type=session_meta` and carries the stable Codex session id plus
//! launch metadata. Unlike Claude's `~/.claude/sessions/<pid>.json`,
//! this file is not PID-indexed, so callers must use a conservative
//! disambiguation policy before attaching it to traffic.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use time::OffsetDateTime;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionFile {
    pub session_id: String,
    pub timestamp: OffsetDateTime,
    pub modified_at: Option<OffsetDateTime>,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    pub source: Option<String>,
    pub thread_source: Option<String>,
    pub model_provider: Option<String>,
    pub path: PathBuf,
}

impl CodexSessionFile {
    #[must_use]
    pub fn is_paper_provider(&self) -> bool {
        self.model_provider.as_deref().is_some_and(|provider| {
            provider == "paper-openai" || provider.starts_with("paper-openai-")
        })
    }

    #[must_use]
    pub fn has_model_provider(&self, provider: &str) -> bool {
        self.model_provider.as_deref() == Some(provider)
    }
}

#[derive(Deserialize)]
struct JsonlRow {
    #[serde(rename = "type")]
    row_type: String,
    payload: Option<SessionMetaPayload>,
}

#[derive(Deserialize)]
struct SessionMetaPayload {
    id: String,
    timestamp: String,
    cwd: Option<String>,
    originator: Option<String>,
    cli_version: Option<String>,
    source: Option<serde_json::Value>,
    thread_source: Option<String>,
    model_provider: Option<String>,
}

/// Read the first `session_meta` row from a Codex JSONL transcript.
pub fn read(path: &Path) -> Option<CodexSessionFile> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let row = match serde_json::from_str::<JsonlRow>(&line) {
            Ok(row) => row,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "codex-session: could not parse jsonl row",
                );
                continue;
            }
        };
        if row.row_type != "session_meta" {
            continue;
        }
        let payload = row.payload?;
        let timestamp = match OffsetDateTime::parse(
            &payload.timestamp,
            &time::format_description::well_known::Rfc3339,
        ) {
            Ok(ts) => ts,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "codex-session: could not parse session timestamp",
                );
                return None;
            }
        };
        return Some(CodexSessionFile {
            session_id: payload.id,
            timestamp,
            modified_at: modified_at(path),
            cwd: payload.cwd,
            originator: payload.originator,
            cli_version: payload.cli_version,
            source: payload.source.and_then(metadata_value_to_string),
            thread_source: payload.thread_source,
            model_provider: payload.model_provider,
            path: path.to_path_buf(),
        });
    }
    None
}

fn modified_at(path: &Path) -> Option<OffsetDateTime> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(system_time_to_offset(modified))
}

fn system_time_to_offset(t: SystemTime) -> OffsetDateTime {
    t.into()
}

/// Default Codex session directory. `$CODEX_HOME` wins when set, matching
/// Codex's own home-directory override; otherwise use `~/.codex/sessions`.
#[must_use]
pub fn default_sessions_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home).join("sessions"));
    }
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

fn metadata_value_to_string(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(value) => Some(value),
        value => serde_json::to_string(&value).ok(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn read_parses_session_meta_first_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-test.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-06-15T23:11:58.261Z","type":"session_meta","payload":{"id":"019ecd8e-4281-7353-8a00-09df678443b1","timestamp":"2026-06-15T23:11:52.984Z","cwd":"/tmp/work","originator":"codex-tui","cli_version":"0.139.0","source":"cli","thread_source":"user","model_provider":"paper-openai"}}"#,
        )
        .unwrap();

        let got = read(&path).unwrap();
        assert_eq!(got.session_id, "019ecd8e-4281-7353-8a00-09df678443b1");
        assert_eq!(got.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(got.cli_version.as_deref(), Some("0.139.0"));
        assert!(got.is_paper_provider());
    }

    #[test]
    fn read_accepts_structured_source_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-test.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-06-15T23:11:58.261Z","type":"session_meta","payload":{"id":"019ecd8e-4281-7353-8a00-09df678443b1","timestamp":"2026-06-15T23:11:52.984Z","cwd":"/tmp/work","source":{"subagent":{"agent_nickname":"Kant"}},"thread_source":"subagent","model_provider":"paper-openai"}}"#,
        )
        .unwrap();

        let got = read(&path).unwrap();
        assert_eq!(
            got.source.as_deref(),
            Some(r#"{"subagent":{"agent_nickname":"Kant"}}"#)
        );
    }

    #[test]
    fn read_skips_malformed_rows_before_session_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-test.jsonl");
        std::fs::write(
            &path,
            r#"not json
{"timestamp":"2026-06-15T23:11:58.261Z","type":"session_meta","payload":{"id":"019ecd8e-4281-7353-8a00-09df678443b1","timestamp":"2026-06-15T23:11:52.984Z","cwd":"/tmp/work","model_provider":"paper-openai"}}"#,
        )
        .unwrap();

        let got = read(&path).unwrap();
        assert_eq!(got.session_id, "019ecd8e-4281-7353-8a00-09df678443b1");
    }
}
