//! `tapesctl sync` — sweep completed transcripts into the ingest server.
//!
//! # What this is for, and what it is not
//!
//! The live tailer in [`super::tailer`] is the primary path: it runs alongside
//! the capture proxy and pushes a session's transcripts as they settle. This
//! command is the backstop for the cases the tailer structurally cannot cover —
//! a session that began and ended while no capture was running, a `tapesctl
//! start` that was killed before its exit push, a transcript tree carried over
//! from a machine that never ran tapesctl at all.
//!
//! # Blind by design
//!
//! It keeps no record of what a previous run sent. It sweeps the tree, offers
//! every transcript it finds, and lets the server decide what is new — which is
//! safe because the ingest endpoint keys rows on a content hash: unchanged
//! content answers `deduped: true` and a grown transcript appends a new version.
//!
//! That trade is deliberate. A client-side "already sent" ledger would be a
//! second source of truth about what the server holds, and every way it can go
//! stale — a restored backup, a wiped server, a half-written state file — loses
//! data silently. The transcript files on disk are the spool; a redundant push
//! costs one `deduped` response.
//!
//! `--since` bounds the sweep for cost, not correctness: a long-lived transcript
//! tree is a lot of pointless dedups at every run. Widening it is always safe.

use std::path::PathBuf;
use std::time::Duration;

use snafu::{OptionExt, ResultExt};
use tapes_harnesses::envelope::HARNESS_ID_CLAUDE;
use tapes_harnesses::transcript::{SweepOptions, TranscriptSession, sweep};
use tracing::{info, warn};
use url::Url;

use super::client::TranscriptClient;
use super::tailer::default_projects_root;
use crate::cli::SyncArgs;
use crate::error::{Error, Result, error};

/// Default sweep window. Matches paperd's startup sweep: far enough back to
/// catch anything a reasonable outage lost, short enough that a years-old tree
/// does not turn every run into a dedup storm.
pub const DEFAULT_SINCE_DAYS: u64 = 7;

/// Tallies for one sweep, reported to the user at the end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncSummary {
    /// Sessions found in the window.
    pub sessions: usize,
    /// Transcript files offered.
    pub files: usize,
    /// Files the server stored as a new version.
    pub stored: usize,
    /// Files the server already had, byte for byte.
    pub deduped: usize,
    /// Files that could not be delivered.
    pub failed: usize,
}

impl SyncSummary {
    /// A one-line human summary.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "tapesctl: swept {} session(s), {} file(s): {} stored, {} deduped, {} failed",
            self.sessions, self.files, self.stored, self.deduped, self.failed,
        )
    }
}

/// Resolved configuration for one `tapesctl sync`.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Base URL of the tapes ingest server.
    pub tapes_url: Url,
    /// Root of the transcript tree to sweep.
    pub projects_root: PathBuf,
    /// Acting subject stamped on uploaded transcripts.
    pub auth_subject: String,
    /// How far back to sweep. `None` sweeps the whole tree.
    pub since: Option<Duration>,
}

impl SyncConfig {
    /// Resolve CLI arguments and the environment into a config.
    pub fn resolve(args: SyncArgs) -> Result<Self> {
        let tapes_url = args
            .tapes_url
            .as_deref()
            .context(error::MissingTapesUrlSnafu)?;
        let projects_root = match args.projects_root {
            Some(root) => root,
            None => default_projects_root().context(error::NoHomeDirSnafu)?,
        };
        Ok(Self {
            tapes_url: Url::parse(tapes_url).context(error::TapesUrlSnafu)?,
            projects_root,
            auth_subject: args
                .auth_subject
                .unwrap_or_else(|| format!("local:{}", crate::start::local_username())),
            // `--since 0` is the explicit "sweep everything" spelling; a window
            // of zero would otherwise mean "sweep nothing", which no one wants.
            since: match args.since_days {
                Some(0) => None,
                Some(days) => Some(Duration::from_secs(days * 24 * 60 * 60)),
                None => Some(Duration::from_secs(DEFAULT_SINCE_DAYS * 24 * 60 * 60)),
            },
        })
    }

    /// The sweep bounds this config implies.
    #[must_use]
    pub fn sweep_options(&self) -> SweepOptions {
        match self.since {
            Some(window) => SweepOptions::modified_within(window),
            None => SweepOptions::default(),
        }
    }
}

/// Run one sweep.
pub async fn run(args: SyncArgs) -> Result<()> {
    let config = SyncConfig::resolve(args)?;
    let client = TranscriptClient::new(&config.tapes_url)?;
    info!(
        projects_root = %config.projects_root.display(),
        ingest = %client.endpoint(),
        "sweeping transcripts",
    );

    let summary = sweep_into(&client, &config).await;
    println!("{}", summary.render());

    // A partial failure is still a failure for an explicitly invoked command:
    // unlike background capture — which must never take the harness down — the
    // user ran this to move data and deserves a non-zero exit if it did not
    // all move. Everything that *did* land is already durable.
    if summary.failed > 0 {
        return Err(Error::SyncIncomplete {
            failed: summary.failed,
            files: summary.files,
        });
    }
    Ok(())
}

/// Sweep and push, collecting tallies. Split from [`run`] so tests can drive it
/// without going through argument resolution or stdout.
pub async fn sweep_into(client: &TranscriptClient, config: &SyncConfig) -> SyncSummary {
    let mut summary = SyncSummary::default();
    let swept = sweep(&config.projects_root, &config.sweep_options());
    summary.sessions = swept.len();

    for session in swept {
        // The envelope is rebuilt from the transcript's own records — a swept
        // session has no live harness to ask, and the directory name is a lossy
        // encoding of the cwd that cannot be decoded back.
        let envelope = TranscriptSession::new(HARNESS_ID_CLAUDE, session.session_id.clone())
            .with_harness_version(session.harness_version.clone())
            .with_cwd(session.cwd.clone())
            .with_auth_subject(config.auth_subject.clone());

        for file in &session.files {
            summary.files += 1;
            match client.upload_file(&envelope, file).await {
                Ok(outcome) if outcome.deduped => summary.deduped += 1,
                Ok(_) => summary.stored += 1,
                Err(err) => {
                    warn!(
                        error = %err,
                        file = %file.label(&session.session_id),
                        "transcript push failed",
                    );
                    summary.failed += 1;
                }
            }
        }
    }
    summary
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::attribution::fork_parent::encode_cwd;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args() -> SyncArgs {
        SyncArgs {
            tapes_url: Some("http://127.0.0.1:8090".to_owned()),
            projects_root: Some(PathBuf::from("/tmp/nope")),
            auth_subject: None,
            since_days: None,
        }
    }

    /// Write a session transcript into a sweepable tree.
    fn write_session(root: &std::path::Path, cwd: &str, sid: &str, subagents: &[&str]) {
        let dir = root.join(encode_cwd(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{sid}.jsonl")),
            format!("{{\"cwd\":\"{cwd}\",\"version\":\"2.1.161\"}}\n"),
        )
        .unwrap();
        if !subagents.is_empty() {
            let sub_dir = dir.join(sid).join("subagents");
            std::fs::create_dir_all(&sub_dir).unwrap();
            for agent in subagents {
                std::fs::write(
                    sub_dir.join(format!("agent-{agent}.jsonl")),
                    "{\"type\":\"assistant\"}\n",
                )
                .unwrap();
            }
        }
    }

    async fn server_replying(template: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    }

    fn config_for(server: &MockServer, root: PathBuf) -> SyncConfig {
        SyncConfig {
            tapes_url: Url::parse(&server.uri()).unwrap(),
            projects_root: root,
            auth_subject: "local:test".to_owned(),
            since: None,
        }
    }

    #[test]
    fn a_missing_tapes_url_is_an_error_rather_than_a_silent_no_op() {
        let mut args = args();
        args.tapes_url = None;
        assert!(SyncConfig::resolve(args).is_err());
    }

    #[test]
    fn the_default_window_is_bounded_but_zero_means_everything() {
        assert_eq!(
            SyncConfig::resolve(args()).unwrap().since,
            Some(Duration::from_secs(DEFAULT_SINCE_DAYS * 24 * 60 * 60)),
        );

        let mut args = args();
        args.since_days = Some(0);
        let config = SyncConfig::resolve(args).unwrap();
        assert_eq!(config.since, None);
        assert_eq!(config.sweep_options(), SweepOptions::default());
    }

    #[test]
    fn the_default_subject_names_the_local_user() {
        let subject = SyncConfig::resolve(args()).unwrap().auth_subject;
        assert!(subject.starts_with("local:"), "got: {subject}");
    }

    #[tokio::test]
    async fn every_transcript_in_the_tree_is_offered_including_subagents() {
        let server =
            server_replying(ResponseTemplate::new(202).set_body_string(r#"{"records":1}"#)).await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &["a1"]);
        write_session(tree.path(), "/tmp/two", "sid-2", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.tapes_url).unwrap();
        let summary = sweep_into(&client, &config).await;

        assert_eq!(summary.sessions, 2);
        assert_eq!(summary.files, 3, "two mains plus one subagent");
        assert_eq!(summary.stored, 3);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn a_dedup_is_counted_as_success_not_failure() {
        // Re-running sync over an already-synced tree is the expected steady
        // state, and it must exit zero.
        let server = server_replying(
            ResponseTemplate::new(202).set_body_string(r#"{"deduped":true,"records":1}"#),
        )
        .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.tapes_url).unwrap();
        let summary = sweep_into(&client, &config).await;

        assert_eq!(summary.deduped, 1);
        assert_eq!(summary.stored, 0);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn the_envelope_is_rebuilt_from_the_transcripts_own_records() {
        // The directory name is a lossy encoding of the cwd, so the real value
        // has to come out of the transcript itself.
        let server =
            server_replying(ResponseTemplate::new(202).set_body_string(r#"{"records":1}"#)).await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.tapes_url).unwrap();
        sweep_into(&client, &config).await;

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains(r#""cwd":"/tmp/one""#), "got: {body}");
        assert!(
            body.contains(r#""harness_version":"2.1.161""#),
            "got: {body}"
        );
        assert!(
            body.contains(r#""auth_subject":"local:test""#),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn a_rejected_file_is_counted_as_failed() {
        let server =
            server_replying(ResponseTemplate::new(400).set_body_string("bad envelope")).await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.tapes_url).unwrap();
        let summary = sweep_into(&client, &config).await;

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.stored, 0);
    }

    #[tokio::test]
    async fn an_empty_tree_sweeps_cleanly() {
        let server =
            server_replying(ResponseTemplate::new(202).set_body_string(r#"{"records":1}"#)).await;
        let tree = tempfile::tempdir().unwrap();

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.tapes_url).unwrap();
        let summary = sweep_into(&client, &config).await;

        assert_eq!(summary, SyncSummary::default());
    }
}
