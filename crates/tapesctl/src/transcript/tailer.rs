//! The live transcript tailer.
//!
//! # Why this has to run during `start`, not only in `sync`
//!
//! Wire capture yields a complete call inventory but no causal skeleton. When a
//! harness forks a subagent, the only record of *which* `Task` tool_use spawned
//! *which* sub-thread lives in the harness's on-disk transcripts — so a session
//! captured on the wire alone renders as flat dispatch text where the same
//! session captured with transcripts renders nested subagent rows.
//!
//! That difference was measured, not assumed: two identical Claude subagent runs
//! produced equal wire lanes (38 vs 40 turns) and unequal transcript lanes — 8
//! transcript turns through the daemon client, 0 through a tapesctl that only swept at
//! `sync` time. The skeleton comes entirely from the lane this module drives.
//!
//! So the tailer runs *alongside* the capture proxy for the life of the session,
//! on the same trigger state machine the daemon client uses. [`super::sync`] is the
//! crash-window backstop, not the primary path.
//!
//! # The division with `tapes-harnesses`
//!
//! The crate owns discovery ([`session_files`]), the JSONL→records conversion,
//! and the *decision* ([`decide`]). This module owns what the crate documents as
//! each client's own: which sessions to track, how to detect that a harness
//! exited, the timer, and the failure backoff. Running the same `decide` against
//! the same inputs is what makes a tapesctl session's transcript lane identical
//! to the daemon's.
//!
//! # Ordering invariants
//!
//! Two are load-bearing and both are cheap to get wrong:
//!
//! * **Fingerprint before reading.** A transcript that grows while its own
//!   upload is in flight must stay dirty, or the grown tail is never pushed.
//!   Recording the pre-read fingerprint is what guarantees the next tick sees
//!   the difference.
//! * **The exit push is awaited, not raced.** `tapesctl start` exits when the
//!   harness does, and the final `PushReason::Exit` push is the one carrying the
//!   completed fork skeleton. Aborting the tailer at shutdown would drop exactly
//!   the data this module exists to deliver.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use tapes_capture::envelope::HARNESS_ID_CLAUDE;
use tapes_harnesses::attribution::claude::fork_parent::encode_cwd;
use tapes_harnesses::attribution::{ClaudeSessionFile, claude::session as claude_session};
use tapes_harnesses::transcript::{
    FileFingerprint, TranscriptSession, TriggerInput, TriggerPolicy, decide, fingerprint,
    session_files,
};
use tracing::{debug, info, warn};

use super::client::TranscriptClient;

/// First backoff after a failed push. Matches the daemon's schedule.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(30);

/// Ceiling on the backoff. A transcript is never lost by waiting — the files on
/// disk are the spool — so a long ceiling costs nothing but a late upload.
pub const BACKOFF_CAP: Duration = Duration::from_secs(600);

/// The default transcript tree for Claude.
#[must_use]
pub fn default_projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude").join("projects"))
}

/// One harness session this process is responsible for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedSession {
    /// OS pid of the harness process, used to detect its exit.
    pub pid: i64,
    /// The harness's own session id.
    pub session_id: String,
    /// Working directory, which is what locates the transcript tree. A session
    /// without one cannot be tailed.
    pub cwd: Option<String>,
    /// Harness version, carried into the envelope.
    pub harness_version: Option<String>,
}

impl TrackedSession {
    /// Build from the harness's own session file.
    #[must_use]
    pub fn from_session_file(file: &ClaudeSessionFile) -> Self {
        Self {
            pid: file.pid,
            session_id: file.session_id.clone(),
            cwd: file.cwd.clone(),
            harness_version: file.version.clone(),
        }
    }

    /// The ingest envelope for this session.
    ///
    /// `org_id` is left to the caller's identity: a standalone client has no
    /// gateway to stamp validated claims, and the server clears the field
    /// anyway on the gateway path.
    #[must_use]
    pub fn transcript_session(&self, auth_subject: &str) -> TranscriptSession {
        TranscriptSession::new(HARNESS_ID_CLAUDE, self.session_id.clone())
            .with_harness_version(self.harness_version.clone())
            .with_cwd(self.cwd.clone())
            .with_auth_subject(auth_subject)
    }
}

/// The set of sessions the capture proxy has seen traffic for.
///
/// This is tapesctl's answer to "which sessions to track", the question the
/// shared crate leaves open. An *attributed request* is the proof that a
/// session's traffic flows through this proxy, which is the tailer's scope rule
/// — a `claude` running direct against the provider is none of this process's
/// business.
///
/// Held in an [`ArcSwap`] rather than a mutex because the writer is the request
/// hot path: the common case (a session already recorded unchanged) does not
/// allocate and never blocks, and there is no lock to poison.
#[derive(Debug, Clone)]
pub struct SessionTracker {
    inner: Arc<ArcSwap<HashMap<String, TrackedSession>>>,
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Record (or refresh) a session seen on the wire.
    pub fn observe(&self, file: &ClaudeSessionFile) {
        let next = TrackedSession::from_session_file(file);
        // The overwhelmingly common call is a repeat of an unchanged session on
        // every request of a live capture; comparing first keeps that path free
        // of a map clone.
        if self.inner.load().get(&next.session_id) == Some(&next) {
            return;
        }
        self.inner.rcu(|current| {
            let mut updated = HashMap::clone(current);
            updated.insert(next.session_id.clone(), next.clone());
            updated
        });
    }

    /// Every tracked session, ordered by session id so a tick is reproducible.
    #[must_use]
    pub fn snapshot(&self) -> Vec<TrackedSession> {
        let mut sessions: Vec<TrackedSession> = self.inner.load().values().cloned().collect();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        sessions
    }

    /// Whether anything has been observed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.load().is_empty()
    }
}

/// Per-session bookkeeping. All of it is this client's own — the crate holds no
/// state between decisions.
#[derive(Debug)]
struct SessionState {
    /// Baseline for the periodic net before the first successful push.
    first_seen: Instant,
    last_push_at: Option<Instant>,
    /// Fingerprint of each file as of the last successful push of that file.
    /// Per-file so a partially failed batch does not lose the ground it gained.
    pushed: HashMap<PathBuf, FileFingerprint>,
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
}

impl SessionState {
    fn new(now: Instant) -> Self {
        Self {
            first_seen: now,
            last_push_at: None,
            pushed: HashMap::new(),
            consecutive_failures: 0,
            backoff_until: None,
        }
    }
}

/// Knobs for one tailer. Defaults mirror the daemon client so the two behave alike.
#[derive(Debug, Clone)]
pub struct TailerConfig {
    /// Root of the harness transcript tree (`~/.claude/projects`).
    pub projects_root: PathBuf,
    /// Directory of harness session files (`~/.claude/sessions`), for exit
    /// detection.
    pub sessions_dir: PathBuf,
    /// Acting subject stamped on uploaded transcripts.
    pub auth_subject: String,
    /// How often the trigger is evaluated.
    pub tick: Duration,
    /// Quiescence and periodic windows.
    pub policy: TriggerPolicy,
    /// First backoff after a failure.
    pub backoff_initial: Duration,
    /// Ceiling on the backoff.
    pub backoff_cap: Duration,
}

impl TailerConfig {
    /// Config with the daemon's shipped timings.
    #[must_use]
    pub fn new(projects_root: PathBuf, sessions_dir: PathBuf, auth_subject: String) -> Self {
        Self {
            projects_root,
            sessions_dir,
            auth_subject,
            tick: tapes_harnesses::transcript::DEFAULT_TICK,
            policy: TriggerPolicy::default(),
            backoff_initial: BACKOFF_INITIAL,
            backoff_cap: BACKOFF_CAP,
        }
    }
}

/// Drives the transcript lane for the sessions one capture proxy observes.
pub struct Tailer {
    client: TranscriptClient,
    tracker: SessionTracker,
    config: TailerConfig,
    states: HashMap<String, SessionState>,
}

impl Tailer {
    /// Build a tailer over `tracker`.
    #[must_use]
    pub fn new(client: TranscriptClient, tracker: SessionTracker, config: TailerConfig) -> Self {
        Self {
            client,
            tracker,
            config,
            states: HashMap::new(),
        }
    }

    /// Tick until `shutdown` fires, then run one final pass.
    ///
    /// The final pass is the point of the whole module: it treats every tracked
    /// session as exited, which is what makes [`decide`] return
    /// `PushReason::Exit` and push the completed transcript — including the
    /// subagent files that carry the fork skeleton. The caller must **await**
    /// this future rather than aborting it.
    pub async fn run(mut self, shutdown: tokio::sync::oneshot::Receiver<()>) {
        let mut ticker = tokio::time::interval(self.config.tick);
        // `interval` fires immediately; a tick at t=0 has nothing to say.
        ticker.tick().await;
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = ticker.tick() => self.tick(false).await,
            }
        }
        self.tick(true).await;
    }

    /// Evaluate every tracked session once.
    ///
    /// `exiting` is the shutdown pass: the harness is gone, so every session is
    /// exited regardless of what its session file still says.
    pub async fn tick(&mut self, exiting: bool) {
        for session in self.tracker.snapshot() {
            self.evaluate_session(&session, exiting).await;
        }
    }

    async fn evaluate_session(&mut self, session: &TrackedSession, exiting: bool) {
        let now = Instant::now();

        // No cwd means no transcript tree to look in. Nothing is recoverable
        // here — the harness never told us where it was running.
        let Some(cwd) = session.cwd.as_deref() else {
            return;
        };
        let projects_dir = self.config.projects_root.join(encode_cwd(cwd));
        let files = session_files(&projects_dir, &session.session_id);
        if files.is_empty() {
            if exiting {
                // At exit this is a loss, not a timing gap: the session was
                // attributed on the wire, the harness has stopped writing,
                // and there is nothing to deliver. Say so with the resolved
                // path — a wrong cwd encoding or a moved projects root looks
                // EXACTLY like this, and silence here once hid a real bug.
                warn!(
                    session_id = %session.session_id,
                    projects_dir = %projects_dir.display(),
                    "no transcript files found at exit; the session's causal skeleton was not delivered",
                );
            }
            // Before exit: attributed on the wire before the first flush.
            return;
        }

        // Fingerprint BEFORE reading, so a transcript that grows during its own
        // upload stays dirty and is pushed again next tick.
        let mut fingerprints: Vec<(PathBuf, FileFingerprint)> = Vec::with_capacity(files.len());
        let mut newest_mtime: Option<SystemTime> = None;
        for file in &files {
            if let Some(fp) = fingerprint(&file.path) {
                newest_mtime = Some(newest_mtime.map_or(fp.mtime, |seen| seen.max(fp.mtime)));
                fingerprints.push((file.path.clone(), fp));
            }
        }

        let (dirty, first_seen, last_push_at, backoff_until) = {
            let state = self
                .states
                .entry(session.session_id.clone())
                .or_insert_with(|| SessionState::new(now));
            let dirty = fingerprints
                .iter()
                .any(|(path, fp)| state.pushed.get(path) != Some(fp));
            (
                dirty,
                state.first_seen,
                state.last_push_at,
                state.backoff_until,
            )
        };

        let input = TriggerInput {
            dirty,
            exited: exiting || session_exited(&self.config.sessions_dir, session),
            idle_for: newest_mtime.and_then(|mtime| SystemTime::now().duration_since(mtime).ok()),
            since_last_push: now.duration_since(last_push_at.unwrap_or(first_seen)),
            // The shutdown pass ignores backoff on purpose: this is the last
            // chance to deliver the fork skeleton, and one more attempt against
            // a sick endpoint costs a single request at process exit.
            in_backoff: !exiting && backoff_until.is_some_and(|until| now < until),
        };

        let Some(reason) = decide(&self.config.policy, &input) else {
            return;
        };

        let envelope = session.transcript_session(&self.config.auth_subject);
        let mut delivered: Vec<(PathBuf, FileFingerprint)> = Vec::new();
        let mut all_ok = true;
        for file in &files {
            let pre_read = fingerprints
                .iter()
                .find(|(path, _)| path == &file.path)
                .map(|(path, fp)| (path.clone(), *fp));
            match self.client.upload_file(&envelope, file).await {
                Ok(outcome) => {
                    debug!(
                        session = %session.session_id,
                        file = %file.label(&session.session_id),
                        reason = reason.as_str(),
                        deduped = outcome.deduped,
                        records = outcome.records,
                        "transcript pushed",
                    );
                    // The fingerprint taken *before* the read is what gets
                    // recorded: a file that grew mid-upload must still look
                    // dirty next tick.
                    delivered.extend(pre_read);
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        session = %session.session_id,
                        file = %file.label(&session.session_id),
                        "transcript push failed",
                    );
                    all_ok = false;
                }
            }
        }

        let Some(state) = self.states.get_mut(&session.session_id) else {
            return;
        };
        // Record every file that landed, even in a failed batch: those bytes are
        // stored, and re-sending them would only earn a dedup.
        for (path, fp) in delivered {
            state.pushed.insert(path, fp);
        }
        if all_ok {
            state.consecutive_failures = 0;
            state.backoff_until = None;
            state.last_push_at = Some(now);
        } else {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            let exponent = state.consecutive_failures.saturating_sub(1).min(16);
            let delay = self
                .config
                .backoff_initial
                .saturating_mul(1u32 << exponent)
                .min(self.config.backoff_cap);
            state.backoff_until = Some(now + delay);
        }
    }
}

/// Spawn a tailer, returning its shutdown trigger and join handle.
///
/// Split out so the caller's shutdown reads as one thing: fire the trigger, then
/// await the handle. Awaiting is mandatory — see [`Tailer::run`].
pub fn spawn(
    client: TranscriptClient,
    tracker: SessionTracker,
    config: TailerConfig,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    info!(
        projects_root = %config.projects_root.display(),
        tick_secs = config.tick.as_secs(),
        "transcript tailer started",
    );
    let handle = tokio::spawn(Tailer::new(client, tracker, config).run(rx));
    (tx, handle)
}

/// `true` when the harness behind `session` is no longer running it: its
/// `~/.claude/sessions/<pid>.json` is gone, unparseable, or now names a
/// different session id (pid reuse).
fn session_exited(sessions_dir: &Path, session: &TrackedSession) -> bool {
    let Ok(pid) = i32::try_from(session.pid) else {
        return true;
    };
    match claude_session::read(sessions_dir, pid) {
        Some(file) => file.session_id != session.session_id,
        None => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::transcript::PushReason;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn session_file(pid: i64, sid: &str, cwd: &str) -> ClaudeSessionFile {
        ClaudeSessionFile {
            pid,
            session_id: sid.to_owned(),
            cwd: Some(cwd.to_owned()),
            version: Some("2.1.161".to_owned()),
            peer_protocol: None,
            kind: None,
            entrypoint: None,
            name: None,
            status: None,
            proc_start: None,
            started_at: None,
            updated_at: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Lay out a transcript tree the way the harness does, returning the
    /// projects root.
    fn transcript_tree(root: &Path, cwd: &str, sid: &str, subagents: &[&str]) -> PathBuf {
        let projects_dir = root.join(encode_cwd(cwd));
        std::fs::create_dir_all(&projects_dir).unwrap();
        std::fs::write(
            projects_dir.join(format!("{sid}.jsonl")),
            "{\"type\":\"user\"}\n",
        )
        .unwrap();
        if !subagents.is_empty() {
            let sub_dir = projects_dir.join(sid).join("subagents");
            std::fs::create_dir_all(&sub_dir).unwrap();
            for agent in subagents {
                std::fs::write(
                    sub_dir.join(format!("agent-{agent}.jsonl")),
                    "{\"type\":\"assistant\"}\n",
                )
                .unwrap();
                std::fs::write(
                    sub_dir.join(format!("agent-{agent}.meta.json")),
                    r#"{"toolUseId":"toolu_1","agentType":"explorer","description":"look"}"#,
                )
                .unwrap();
            }
        }
        root.to_path_buf()
    }

    async fn accepting_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .respond_with(
                ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted","records":1}"#),
            )
            .mount(&server)
            .await;
        server
    }

    fn tailer_for(server: &MockServer, projects_root: PathBuf, sessions_dir: PathBuf) -> Tailer {
        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        let tracker = SessionTracker::new();
        let config = TailerConfig::new(projects_root, sessions_dir, "local:test".to_owned());
        Tailer::new(client, tracker, config)
    }

    #[test]
    fn observing_the_same_session_twice_keeps_one_entry() {
        let tracker = SessionTracker::new();
        assert!(tracker.is_empty());
        tracker.observe(&session_file(1, "sid-1", "/tmp/a"));
        tracker.observe(&session_file(1, "sid-1", "/tmp/a"));
        assert_eq!(tracker.snapshot().len(), 1);
    }

    #[test]
    fn a_refreshed_session_replaces_its_earlier_facts() {
        // The session file is mutable across requests — a `/name` mid-session
        // changes it — so the newest observation must win.
        let tracker = SessionTracker::new();
        tracker.observe(&session_file(1, "sid-1", "/tmp/a"));
        let mut updated = session_file(1, "sid-1", "/tmp/a");
        updated.version = Some("9.9.9".to_owned());
        tracker.observe(&updated);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].harness_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn the_snapshot_is_ordered_so_a_tick_is_reproducible() {
        let tracker = SessionTracker::new();
        tracker.observe(&session_file(2, "sid-b", "/tmp/b"));
        tracker.observe(&session_file(1, "sid-a", "/tmp/a"));
        let ids: Vec<String> = tracker
            .snapshot()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, vec!["sid-a".to_owned(), "sid-b".to_owned()]);
    }

    #[test]
    fn a_missing_session_file_means_the_harness_exited() {
        let dir = tempfile::tempdir().unwrap();
        let session = TrackedSession::from_session_file(&session_file(4242, "sid-1", "/tmp/a"));
        assert!(session_exited(dir.path(), &session));
    }

    #[test]
    fn a_reused_pid_naming_another_session_counts_as_exited() {
        // pid reuse is the case a bare "does the pid exist" check gets wrong: the
        // process is alive, but it is not our session.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("4242.json"),
            r#"{"pid":4242,"sessionId":"someone-else","cwd":"/tmp/a"}"#,
        )
        .unwrap();
        let session = TrackedSession::from_session_file(&session_file(4242, "sid-1", "/tmp/a"));
        assert!(session_exited(dir.path(), &session));
    }

    #[test]
    fn a_live_session_file_naming_our_session_is_not_exited() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("4242.json"),
            r#"{"pid":4242,"sessionId":"sid-1","cwd":"/tmp/a"}"#,
        )
        .unwrap();
        let session = TrackedSession::from_session_file(&session_file(4242, "sid-1", "/tmp/a"));
        assert!(!session_exited(dir.path(), &session));
    }

    #[test]
    fn the_envelope_names_claude_and_carries_the_sessions_own_facts() {
        let session = TrackedSession::from_session_file(&session_file(1, "sid-1", "/tmp/proj"));
        let envelope = session.transcript_session("local:test");
        assert_eq!(envelope.harness_id, HARNESS_ID_CLAUDE);
        assert_eq!(envelope.harness_session_id, "sid-1");
        assert_eq!(envelope.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(envelope.harness_version.as_deref(), Some("2.1.161"));
        assert_eq!(envelope.auth_subject, "local:test");
    }

    // --- the lane that produces the fork skeleton -------------------------

    #[tokio::test]
    async fn the_exit_pass_pushes_the_main_transcript_and_every_subagent() {
        // This is the acceptance criterion in miniature: the subagent files are
        // the fork skeleton, and they must reach ingest before the process ends.
        let server = accepting_server().await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = transcript_tree(tree.path(), "/tmp/proj", "sid-1", &["a1", "a2"]);

        let mut tailer = tailer_for(&server, root, sessions.path().to_path_buf());
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.tick(true).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "main transcript plus both subagents must be pushed",
        );
        let bodies: Vec<String> = requests
            .iter()
            .map(|r| String::from_utf8(r.body.clone()).unwrap())
            .collect();
        let joined = bodies.join("\n");
        assert!(joined.contains(r#""agent_id":"a1""#), "got: {joined}");
        assert!(joined.contains(r#""agent_id":"a2""#), "got: {joined}");
        assert!(
            joined.contains(r#""tool_use_id":"toolu_1""#),
            "the fork edge the deriver attaches must ride along: {joined}",
        );
    }

    #[tokio::test]
    async fn a_clean_session_is_not_pushed_twice() {
        // Unchanged content would only earn a dedup, and a tick that re-pushes
        // every file forever is how a quiet session becomes a busy one.
        let server = accepting_server().await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = transcript_tree(tree.path(), "/tmp/proj", "sid-1", &[]);

        let mut tailer = tailer_for(&server, root, sessions.path().to_path_buf());
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.tick(true).await;
        tailer.tick(true).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_grown_transcript_is_pushed_again() {
        let server = accepting_server().await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = transcript_tree(tree.path(), "/tmp/proj", "sid-1", &[]);

        let mut tailer = tailer_for(&server, root.clone(), sessions.path().to_path_buf());
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.tick(true).await;

        let transcript = root.join(encode_cwd("/tmp/proj")).join("sid-1.jsonl");
        // A distinct length is what the coarse (size + mtime) fingerprint keys
        // on; an mtime-only change can fall inside filesystem timestamp
        // granularity.
        std::fs::write(
            &transcript,
            "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n",
        )
        .unwrap();
        tailer.tick(true).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_session_with_no_transcript_yet_is_skipped_without_error() {
        // Attribution can land on the wire before the harness's first flush.
        let server = accepting_server().await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();

        let mut tailer = tailer_for(
            &server,
            tree.path().to_path_buf(),
            sessions.path().to_path_buf(),
        );
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.tick(true).await;

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_live_unquiesced_session_is_not_pushed_mid_flight() {
        // A freshly written transcript is neither idle for 30 s nor 5 minutes
        // old, and its harness has not exited — nothing should fire yet.
        let server = accepting_server().await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = transcript_tree(tree.path(), "/tmp/proj", "sid-1", &[]);
        std::fs::write(
            sessions.path().join("1.json"),
            r#"{"pid":1,"sessionId":"sid-1","cwd":"/tmp/proj"}"#,
        )
        .unwrap();

        let mut tailer = tailer_for(&server, root, sessions.path().to_path_buf());
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.tick(false).await;

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_push_opens_a_backoff_window() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream sad"))
            .mount(&server)
            .await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = transcript_tree(tree.path(), "/tmp/proj", "sid-1", &[]);

        let mut tailer = tailer_for(&server, root, sessions.path().to_path_buf());
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.tick(false).await;

        let state = tailer.states.get("sid-1").unwrap();
        assert_eq!(state.consecutive_failures, 1);
        assert!(state.backoff_until.is_some());
        assert!(
            state.pushed.is_empty(),
            "a failed file must not be recorded as delivered",
        );
    }

    #[tokio::test]
    async fn the_exit_pass_overrides_an_open_backoff_window() {
        // The last push carries the fork skeleton; deferring it to a backoff
        // window that outlives the process would drop it entirely.
        let server = accepting_server().await;
        let tree = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let root = transcript_tree(tree.path(), "/tmp/proj", "sid-1", &[]);

        let mut tailer = tailer_for(&server, root, sessions.path().to_path_buf());
        tailer
            .tracker
            .observe(&session_file(1, "sid-1", "/tmp/proj"));
        tailer.states.insert("sid-1".to_owned(), {
            let mut state = SessionState::new(Instant::now());
            state.backoff_until = Some(Instant::now() + Duration::from_secs(3600));
            state
        });
        tailer.tick(true).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn the_shipped_policy_matches_the_crates_defaults() {
        // Behaving identically to the daemon client is the whole reason the decision lives
        // in the shared crate; a local re-tuning here would silently fork it.
        let config = TailerConfig::new(PathBuf::new(), PathBuf::new(), String::new());
        assert_eq!(config.policy, TriggerPolicy::default());
        assert_eq!(config.tick, tapes_harnesses::transcript::DEFAULT_TICK);
        assert_eq!(
            decide(
                &config.policy,
                &TriggerInput {
                    dirty: true,
                    exited: true,
                    idle_for: None,
                    since_last_push: Duration::ZERO,
                    in_backoff: false,
                },
            ),
            Some(PushReason::Exit),
        );
    }
}
