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

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use snafu::{OptionExt, ResultExt};
use tapes_capture::envelope::HARNESS_ID_CLAUDE;
use tapes_harnesses::transcript::{SweepOptions, TranscriptSession, sweep};
use tracing::{info, warn};
use url::Url;

use super::client::{DetailedUploadOutcome, TranscriptClient};
use super::tailer::default_projects_root;
use crate::cli::SyncArgs;
use crate::error::{Error, Result, error};

/// Default sweep window. Matches the daemon's startup sweep: far enough back to
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
    /// Successful files whose acknowledgement did not identify their outcome.
    /// Derived from the compatibility fields so aliases can never drift.
    fn unavailable(&self) -> usize {
        self.files.saturating_sub(
            self.stored
                .saturating_add(self.deduped)
                .saturating_add(self.failed),
        )
    }

    /// The human summary of upload outcomes.
    #[must_use]
    pub fn render(&self) -> String {
        let version_label = if self.stored == 1 {
            "new version"
        } else {
            "new versions"
        };
        let unavailable = self.unavailable();
        let unavailable_suffix = if unavailable == 0 {
            String::new()
        } else {
            format!(", {unavailable} outcome(s) unavailable")
        };
        format!(
            "tapesctl: swept {} session(s), {} file(s): {} {}, {} already present, {} failed{}",
            self.sessions,
            self.files,
            self.stored,
            version_label,
            self.deduped,
            self.failed,
            unavailable_suffix,
        )
    }
}

/// Server outcome for one offered transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOutcome {
    /// The server accepted a new content version.
    New,
    /// The exact content version was already present.
    AlreadyPresent,
    /// The server succeeded but did not report whether the content was new.
    Unavailable,
    /// No successful server response was received.
    Failed,
}

impl FileOutcome {
    fn from_acknowledgement(
        summary: &mut SyncSummary,
        details: DetailedUploadOutcome,
    ) -> (Option<usize>, Self) {
        let outcome = match details.deduped() {
            Some(true) => {
                summary.deduped += 1;
                Self::AlreadyPresent
            }
            Some(false) => {
                summary.stored += 1;
                Self::New
            }
            None => Self::Unavailable,
        };
        (details.records(), outcome)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::AlreadyPresent => "already present",
            Self::Unavailable => "unavailable (dedup status unavailable)",
            Self::Failed => "failed",
        }
    }
}

/// Testable account of one offered file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileReport {
    /// Harness session id carried in the upload envelope.
    pub harness_session_id: String,
    /// Transcript path offered to the server.
    pub path: PathBuf,
    /// Record count from the server, independent of whether it reported dedup status.
    pub records: Option<usize>,
    /// Whether this version was new, already present, or failed.
    pub outcome: FileOutcome,
}

impl FileReport {
    /// The detail line printed by `sync -v`.
    #[must_use]
    pub fn render(&self) -> String {
        let records = self
            .records
            .map_or_else(|| "unavailable".to_owned(), |records| records.to_string());
        format!(
            "tapesctl: sync file: session {}, path {}, server records {}, outcome {}",
            self.harness_session_id,
            self.path.display(),
            records,
            self.outcome.as_str(),
        )
    }
}

/// Complete, renderable report for one sync sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SyncReport {
    /// Aggregate outcomes.
    summary: SyncSummary,
    /// Unique harness sessions with at least one successful response.
    queued_sessions: usize,
    /// One outcome per offered file, in sweep order.
    files: Vec<FileReport>,
}

impl SyncReport {
    /// Render user output. Per-file detail is a deliberate `-v` behavior, not
    /// an incidental consequence of whichever tracing filter is installed.
    #[must_use]
    pub fn render(&self, verbosity: u8) -> Vec<String> {
        let mut lines = if verbosity > 0 {
            self.files.iter().map(FileReport::render).collect()
        } else {
            Vec::new()
        };
        lines.push(self.summary.render());
        lines.push(format!(
            "tapesctl: projection queued asynchronously for {} unique session(s)",
            self.queued_sessions,
        ));
        lines
    }

    /// Preserve sync's nonzero exit behavior after all report lines print.
    pub fn ensure_complete(&self) -> Result<()> {
        if self.summary.failed > 0 {
            return Err(Error::SyncIncomplete {
                failed: self.summary.failed,
                files: self.summary.files,
            });
        }
        Ok(())
    }
}

/// Resolved configuration for one `tapesctl sync`.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Base URL of the tapes ingest server.
    pub ingest_url: Url,
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
        let ingest_url = args
            .ingest_url
            .as_deref()
            .context(error::MissingTapesUrlSnafu)?;
        let projects_root = match args.projects_root {
            Some(root) => root,
            None => default_projects_root().context(error::NoHomeDirSnafu)?,
        };
        Ok(Self {
            ingest_url: Url::parse(ingest_url).context(error::TapesUrlSnafu)?,
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

/// Run one sweep through the compatibility API.
pub async fn run(args: SyncArgs) -> Result<()> {
    run_with_verbosity(args, 0).await
}

/// Run one CLI sweep with the global verbosity resolved by dispatch.
pub(crate) async fn run_with_verbosity(args: SyncArgs, verbosity: u8) -> Result<()> {
    let config = SyncConfig::resolve(args)?;
    let client = TranscriptClient::new(&config.ingest_url)?;
    info!(
        projects_root = %config.projects_root.display(),
        ingest = %client.endpoint(),
        "sweeping transcripts",
    );

    let report = sweep_report(&client, &config).await;
    for line in report.render(verbosity) {
        println!("{line}");
    }

    // A partial failure is still a failure for an explicitly invoked command:
    // unlike background capture — which must never take the harness down — the
    // user ran this to move data and deserves a non-zero exit if it did not
    // all move. Everything that *did* land is already durable.
    report.ensure_complete()
}

/// Sweep and push, collecting compatibility tallies.
pub async fn sweep_into(client: &TranscriptClient, config: &SyncConfig) -> SyncSummary {
    sweep_report(client, config).await.summary
}

/// Sweep and push while retaining acknowledgement detail for CLI rendering.
async fn sweep_report(client: &TranscriptClient, config: &SyncConfig) -> SyncReport {
    let mut report = SyncReport::default();
    let mut queued_sessions = HashSet::new();
    let swept = sweep(&config.projects_root, &config.sweep_options());
    report.summary.sessions = swept.len();

    for session in swept {
        // The envelope is rebuilt from the transcript's own records — a swept
        // session has no live harness to ask, and the directory name is a lossy
        // encoding of the cwd that cannot be decoded back.
        let envelope = TranscriptSession::new(HARNESS_ID_CLAUDE, session.session_id.clone())
            .with_harness_version(session.harness_version.clone())
            .with_cwd(session.cwd.clone())
            .with_auth_subject(config.auth_subject.clone());

        for file in &session.files {
            report.summary.files += 1;
            let (records, outcome) = match client.upload_file_detailed(&envelope, file).await {
                Ok(details) => {
                    queued_sessions.insert(session.session_id.clone());
                    FileOutcome::from_acknowledgement(&mut report.summary, details)
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        file = %file.label(&session.session_id),
                        "transcript push failed",
                    );
                    report.summary.failed += 1;
                    (None, FileOutcome::Failed)
                }
            };
            report.files.push(FileReport {
                harness_session_id: session.session_id.clone(),
                path: file.path.clone(),
                records,
                outcome,
            });
        }
    }
    report.queued_sessions = queued_sessions.len();
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::attribution::claude::fork_parent::encode_cwd;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args() -> SyncArgs {
        SyncArgs {
            ingest_url: Some("http://127.0.0.1:8090".to_owned()),
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
            ingest_url: Url::parse(&server.uri()).unwrap(),
            projects_root: root,
            auth_subject: "local:test".to_owned(),
            since: None,
        }
    }

    #[test]
    fn a_missing_tapes_url_is_an_error_rather_than_a_silent_no_op() {
        let mut args = args();
        args.ingest_url = None;
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

    #[test]
    fn summary_distinguishes_new_versions_from_already_present_files() {
        // This literal is also a source-compatibility regression check for the
        // original public fields.
        let summary = SyncSummary {
            sessions: 2,
            files: 3,
            stored: 2,
            deduped: 1,
            failed: 0,
        };

        let rendered = summary.render();
        assert_eq!(
            rendered,
            "tapesctl: swept 2 session(s), 3 file(s): 2 new versions, 1 already present, 0 failed",
        );
        assert!(!rendered.contains("stored"), "got: {rendered}");
        assert!(!rendered.contains("deduped"), "got: {rendered}");
    }

    #[tokio::test]
    async fn queued_projection_counts_unique_successful_sessions_not_files() {
        let server = server_replying(
            ResponseTemplate::new(202).set_body_string(r#"{"deduped":false,"records":1}"#),
        )
        .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &["a1"]);
        write_session(tree.path(), "/tmp/two", "sid-2", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.sessions, 2);
        assert_eq!(report.summary.files, 3, "two mains plus one subagent");
        assert_eq!(report.summary.stored, 3);
        assert_eq!(report.summary.failed, 0);
        assert_eq!(report.queued_sessions, 2);
        assert_eq!(
            report.render(0)[1],
            "tapesctl: projection queued asynchronously for 2 unique session(s)",
        );
    }

    #[test]
    fn verbose_output_reports_every_file_session_path_records_and_outcome() {
        let report = SyncReport {
            summary: SyncSummary::default(),
            queued_sessions: 2,
            files: vec![
                FileReport {
                    harness_session_id: "sid-new".to_owned(),
                    path: PathBuf::from("/tmp/new.jsonl"),
                    records: Some(3),
                    outcome: FileOutcome::New,
                },
                FileReport {
                    harness_session_id: "sid-present".to_owned(),
                    path: PathBuf::from("/tmp/present.jsonl"),
                    records: Some(5),
                    outcome: FileOutcome::AlreadyPresent,
                },
                FileReport {
                    harness_session_id: "sid-failed".to_owned(),
                    path: PathBuf::from("/tmp/failed.jsonl"),
                    records: None,
                    outcome: FileOutcome::Failed,
                },
            ],
        };

        let lines = report.render(1);
        assert_eq!(lines.len(), 5, "three files plus two summary lines");
        assert!(lines[0].contains("session sid-new"), "got: {}", lines[0]);
        assert!(
            lines[0].contains("path /tmp/new.jsonl"),
            "got: {}",
            lines[0]
        );
        assert!(lines[0].contains("server records 3"), "got: {}", lines[0]);
        assert!(lines[0].ends_with("outcome new"), "got: {}", lines[0]);
        assert!(
            lines[1].ends_with("outcome already present"),
            "got: {}",
            lines[1],
        );
        assert!(
            lines[2].contains("server records unavailable") && lines[2].ends_with("outcome failed"),
            "got: {}",
            lines[2],
        );
    }

    #[test]
    fn normal_output_omits_per_file_success_detail() {
        let report = SyncReport {
            summary: SyncSummary {
                sessions: 1,
                files: 1,
                stored: 1,
                deduped: 0,
                failed: 0,
            },
            queued_sessions: 1,
            files: vec![FileReport {
                harness_session_id: "sid-1".to_owned(),
                path: PathBuf::from("/tmp/sid-1.jsonl"),
                records: Some(4),
                outcome: FileOutcome::New,
            }],
        };

        let lines = report.render(0);
        assert_eq!(lines.len(), 2, "only summary and projection status");
        assert!(lines.iter().all(|line| !line.contains("/tmp/sid-1.jsonl")));
    }

    #[tokio::test]
    async fn a_dedup_is_success_and_requeues_projection() {
        // Re-running sync over an already-synced tree is the expected steady
        // state, and it must exit zero. Ingest also requeues projection.
        let server = server_replying(
            ResponseTemplate::new(202).set_body_string(r#"{"deduped":true,"records":1}"#),
        )
        .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.deduped, 1);
        assert_eq!(report.summary.stored, 0);
        assert_eq!(report.summary.failed, 0);
        assert_eq!(report.queued_sessions, 1);
        assert!(report.ensure_complete().is_ok());
    }

    #[tokio::test]
    async fn verbose_output_keeps_a_known_dedup_outcome_when_records_are_missing() {
        let server =
            server_replying(ResponseTemplate::new(202).set_body_string(r#"{"deduped":true}"#))
                .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.deduped, 1);
        assert_eq!(report.summary.stored, 0);
        let detail = report.files[0].render();
        assert!(
            detail.contains("server records unavailable"),
            "got: {detail}"
        );
        assert!(detail.ends_with("outcome already present"), "got: {detail}");
    }

    #[tokio::test]
    async fn verbose_output_keeps_a_known_record_count_when_dedup_status_is_missing() {
        let server =
            server_replying(ResponseTemplate::new(202).set_body_string(r#"{"records":3}"#)).await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.deduped, 0);
        assert_eq!(report.summary.stored, 0);
        assert_eq!(report.summary.unavailable(), 1);
        let detail = report.files[0].render();
        assert!(detail.contains("server records 3"), "got: {detail}");
        assert!(
            detail.ends_with("outcome unavailable (dedup status unavailable)"),
            "got: {detail}"
        );
    }

    #[tokio::test]
    async fn verbose_output_keeps_dedup_when_the_records_type_is_malformed() {
        let server = server_replying(
            ResponseTemplate::new(202).set_body_string(r#"{"deduped":true,"records":"bad"}"#),
        )
        .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.deduped, 1);
        assert_eq!(report.summary.stored, 0);
        let detail = report.files[0].render();
        assert!(
            detail.contains("server records unavailable"),
            "got: {detail}"
        );
        assert!(detail.ends_with("outcome already present"), "got: {detail}");
    }

    #[tokio::test]
    async fn verbose_output_keeps_records_when_the_dedup_type_is_malformed() {
        let server = server_replying(
            ResponseTemplate::new(202).set_body_string(r#"{"deduped":"bad","records":3}"#),
        )
        .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.deduped, 0);
        assert_eq!(report.summary.stored, 0);
        assert_eq!(report.summary.unavailable(), 1);
        let detail = report.files[0].render();
        assert!(detail.contains("server records 3"), "got: {detail}");
        assert!(
            detail.ends_with("outcome unavailable (dedup status unavailable)"),
            "got: {detail}"
        );
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
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
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
    async fn partial_failure_queues_only_successful_sessions_and_still_exits_nonzero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .and(body_string_contains("sid-ok"))
            .respond_with(
                ResponseTemplate::new(202).set_body_string(r#"{"deduped":false,"records":2}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .and(body_string_contains("sid-failed"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad envelope"))
            .mount(&server)
            .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/ok", "sid-ok", &[]);
        write_session(tree.path(), "/tmp/failed", "sid-failed", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.files, 2);
        assert_eq!(report.summary.stored, 1);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.queued_sessions, 1);
        assert!(matches!(
            report.ensure_complete(),
            Err(Error::SyncIncomplete {
                failed: 1,
                files: 2,
            }),
        ));
    }

    async fn assert_acknowledgement_unavailable(template: ResponseTemplate) {
        let server = server_replying(template).await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let report = sweep_report(&client, &config).await;

        assert_eq!(report.summary.files, 1);
        assert_eq!(report.summary.stored, 0, "unknown is not new");
        assert_eq!(report.summary.deduped, 0, "unknown is not already present");
        assert_eq!(report.summary.failed, 0, "the 2xx remains successful");
        assert_eq!(report.queued_sessions, 1);
        assert!(report.summary.render().contains("1 outcome(s) unavailable"));
        let detail = report.files[0].render();
        assert!(
            detail.contains("server records unavailable"),
            "got: {detail}"
        );
        assert!(
            detail.contains("outcome unavailable (dedup status unavailable)"),
            "got: {detail}",
        );
        assert!(report.ensure_complete().is_ok());
    }

    #[tokio::test]
    async fn an_unparseable_acknowledgement_is_successful_but_not_reported_as_new() {
        assert_acknowledgement_unavailable(ResponseTemplate::new(202).set_body_string("not json"))
            .await;
    }

    #[tokio::test]
    async fn a_missing_acknowledgement_is_successful_but_not_reported_as_new() {
        assert_acknowledgement_unavailable(ResponseTemplate::new(204)).await;
    }

    #[tokio::test]
    async fn public_sweep_into_still_returns_the_compatibility_summary() {
        let server = server_replying(
            ResponseTemplate::new(202).set_body_string(r#"{"deduped":false,"records":1}"#),
        )
        .await;
        let tree = tempfile::tempdir().unwrap();
        write_session(tree.path(), "/tmp/one", "sid-1", &[]);

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let summary: SyncSummary = sweep_into(&client, &config).await;

        assert_eq!(summary.stored, 1);
        assert_eq!(summary.deduped, 0);
    }

    #[test]
    fn public_run_still_accepts_only_sync_args() {
        std::mem::drop(run(args()));
    }

    #[tokio::test]
    async fn an_empty_tree_sweeps_cleanly() {
        let server =
            server_replying(ResponseTemplate::new(202).set_body_string(r#"{"records":1}"#)).await;
        let tree = tempfile::tempdir().unwrap();

        let config = config_for(&server, tree.path().to_path_buf());
        let client = TranscriptClient::new(&config.ingest_url).unwrap();
        let summary = sweep_into(&client, &config).await;

        assert_eq!(summary, SyncSummary::default());
    }
}
