//! The Codex anchor lane — Codex's answer to the transcript tailer.
//!
//! # Why Codex needs its own lane
//!
//! Claude writes a per-session transcript tree, so [`super::tailer`] can walk
//! it. Codex writes nothing of the sort: it keeps one append-only JSONL
//! *rollout* per thread, and the only causal fact in there that the wire lane
//! cannot see is the `sub_agent_activity` record — the exact
//! (spawn call_id ↔ child thread id) join. `spawn_agent`'s arguments are an
//! encrypted blob on the wire and its result names only the task, so without
//! that record a spawned agent's span has nothing to hang from and lands under
//! the trace root instead of under the call that created it.
//!
//! The gap this closes was one-sided rather than absent: the daemon client has shipped
//! these anchors for as long as it has captured Codex, while a `tapesctl start
//! codex` session shipped none. Identical sessions therefore reconstructed into
//! *different-shaped* trees depending on which client captured them, which is
//! the one asymmetry the shared-crate design exists to prevent.
//!
//! # The division with `tapes-harnesses`
//!
//! Everything about the rollout format is the crate's
//! ([`tapes_harnesses::transcript::codex_anchors`]): what an anchor is, how it
//! is parsed, what the ingest row looks like, which anchors a rollout still
//! owes, and when an append-only file is worth re-reading. This module owns
//! what the crate documents as each client's own — the scope rule (which
//! rollouts are *ours*), the delivery, the timer, and the failure backoff. It
//! is the same division [`super::tailer`] makes with the Claude lane, and it is
//! what makes this lane's rows byte-identical to the daemon's rather than merely
//! similar.
//!
//! # Two invariants worth stating
//!
//! * **The exit pass is awaited, not raced.** `tapesctl start` exits when the
//!   harness does, and the spawns near the end of a session are exactly the
//!   ones whose anchors have not been pushed yet. Aborting at shutdown would
//!   drop the newest part of the skeleton — the same reason the transcript
//!   tailer's final push is awaited.
//! * **The row's identity fields stay empty.** `org_id` and `auth_subject` are
//!   blank on every anchor row, here and in the daemon client, because the row's bytes are
//!   a shared contract and the ingest server's dedup key is a hash of them.
//!   Stamping this client's subject would fork the two clients' rows for no
//!   gain: the session those anchors attach to already carries the subject from
//!   the wire lane.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tapes_capture::envelope::HARNESS_ID_CODEX;
use tapes_harnesses::attribution::{
    CodexProviderFilter, CodexSessionFile, CodexWatcherSnapshotHandle,
};
use tapes_harnesses::transcript::codex_anchors::{
    CodexAnchorScanner, SubAgentAnchor, anchor_records, build_anchor_payload,
};
use tapes_harnesses::transcript::fingerprint;
use tracing::{debug, info, warn};

use super::client::TranscriptClient;

/// Cadence of the snapshot scan. Matches the daemon's: anchors are tiny and
/// time-sensitive (tapes re-derives on arrival), so they push on discovery with
/// no quiescence window — unlike a transcript, which is re-read whole.
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// First backoff after a failed push. Matches the daemon's schedule.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(30);

/// Ceiling on the backoff. A rollout is never lost by waiting — the file on
/// disk is the spool — so a long ceiling costs nothing but a late anchor.
pub const BACKOFF_CAP: Duration = Duration::from_secs(600);

/// Knobs for one anchor lane.
#[derive(Debug, Clone)]
pub struct AnchorLaneConfig {
    /// Prefix of the Codex provider id this client declares. Only rollouts
    /// naming it flow through this proxy, so only they belong in tapes — the
    /// same scope rule the attribution pipeline applies, and the reason this
    /// decision is not in the shared crate.
    pub provider_prefix: String,
    /// How often the watcher snapshot is scanned.
    pub tick: Duration,
    /// First backoff after a failure.
    pub backoff_initial: Duration,
    /// Ceiling on the backoff.
    pub backoff_cap: Duration,
    /// How long the shutdown pass may run before the terminal is handed back
    /// regardless. See [`FINAL_PASS_DEADLINE`].
    pub final_pass_deadline: Duration,
}

impl AnchorLaneConfig {
    /// Config with the daemon's shipped timings.
    #[must_use]
    pub fn new(provider_prefix: impl Into<String>) -> Self {
        Self {
            provider_prefix: provider_prefix.into(),
            tick: DEFAULT_TICK,
            backoff_initial: BACKOFF_INITIAL,
            backoff_cap: BACKOFF_CAP,
            final_pass_deadline: FINAL_PASS_DEADLINE,
        }
    }
}

/// Per-rollout retry state.
///
/// Separate from the shared [`CodexAnchorScanner`] on purpose: what a rollout
/// still owes is a fact both capture clients must agree on, while how long to
/// wait after a failed push is this client's policy.
#[derive(Debug, Default)]
struct DeliveryState {
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
}

/// Drives the anchor lane for the Codex rollouts one capture proxy observes.
pub struct CodexAnchorLane {
    client: TranscriptClient,
    snapshot: CodexWatcherSnapshotHandle,
    config: AnchorLaneConfig,
    scanner: CodexAnchorScanner,
    delivery: HashMap<PathBuf, DeliveryState>,
}

/// How long the shutdown pass may run before the terminal is handed back
/// regardless.
///
/// Sized against the work rather than the clock: a session's undelivered
/// anchors are few, and each push is separately bounded, so reaching this
/// means something is wrong rather than merely slow.
const FINAL_PASS_DEADLINE: Duration = Duration::from_secs(30);

impl CodexAnchorLane {
    /// Build a lane over the proxy's Codex watcher snapshot.
    ///
    /// Reusing the snapshot the attribution pipeline already publishes is
    /// deliberate: a second watcher would be a second answer to "which rollouts
    /// exist", and the two could disagree about a file that appeared mid-tick.
    #[must_use]
    pub fn new(
        client: TranscriptClient,
        snapshot: CodexWatcherSnapshotHandle,
        config: AnchorLaneConfig,
    ) -> Self {
        Self {
            client,
            snapshot,
            config,
            scanner: CodexAnchorScanner::new(),
            delivery: HashMap::new(),
        }
    }

    /// Tick until `shutdown` fires, then run one final pass.
    ///
    /// The final pass is what makes the lane complete: it ignores every open
    /// backoff window, because this is the last chance to deliver the anchors
    /// for spawns that happened seconds before the harness exited. The caller
    /// must **await** this future rather than aborting it.
    pub async fn run(mut self, shutdown: tokio::sync::oneshot::Receiver<()>) {
        let mut ticker = tokio::time::interval(self.config.tick);
        // `interval` fires immediately; at t=0 the watcher snapshot is still
        // empty and there is nothing to scan.
        ticker.tick().await;
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = ticker.tick() => self.tick(false).await,
            }
        }
        // The caller awaits this future to get its terminal back, so the last
        // pass answers to a clock as well as to the work. Each push is already
        // bounded by the client's request timeout; this bounds the pass, whose
        // length is otherwise the number of undelivered anchors times that.
        // Giving up here costs the anchors this pass had not reached yet —
        // strictly better than a session that has ended and a shell that has
        // not come back.
        let deadline = self.config.final_pass_deadline;
        if tokio::time::timeout(deadline, self.tick(true))
            .await
            .is_err()
        {
            warn!(
                deadline_secs = deadline.as_secs(),
                "codex anchor lane ran out of time on its final pass; \
                 some spawn anchors were not delivered",
            );
        }
    }

    /// One scan across the current watcher snapshot.
    ///
    /// `exiting` is the shutdown pass: the harness is gone, so no backoff
    /// window may defer a push past the end of the process.
    pub async fn tick(&mut self, exiting: bool) {
        let snapshot = self.snapshot.load_full();
        let provider = CodexProviderFilter::new(self.config.provider_prefix.clone());
        let rollouts: Vec<CodexSessionFile> = snapshot
            .sessions
            .iter()
            .filter(|session| provider.matches(session.model_provider.as_deref()))
            .cloned()
            .collect();

        // Drop state for rollouts that aged out of the snapshot so both maps
        // stay bounded over a long session.
        self.scanner
            .retain_live(rollouts.iter().map(|session| session.path.as_path()));
        let live: HashSet<&Path> = rollouts
            .iter()
            .map(|session| session.path.as_path())
            .collect();
        self.delivery
            .retain(|path, _| live.contains(path.as_path()));

        let now = Instant::now();
        for rollout in rollouts {
            self.evaluate_rollout(&rollout, now, exiting).await;
        }
    }

    async fn evaluate_rollout(&mut self, rollout: &CodexSessionFile, now: Instant, exiting: bool) {
        let state = self.delivery.entry(rollout.path.clone()).or_default();
        if !exiting && state.backoff_until.is_some_and(|until| now < until) {
            return;
        }
        let fp = fingerprint(&rollout.path);
        if !self.scanner.needs_read(&rollout.path, fp) {
            return;
        }

        let raw = match std::fs::read(&rollout.path) {
            Ok(raw) => raw,
            Err(err) => {
                debug!(
                    path = %rollout.path.display(),
                    error = %err,
                    "could not read codex rollout; skipping this tick",
                );
                return;
            }
        };
        let anchors = self.scanner.undelivered(&rollout.path, &raw);
        if anchors.is_empty() {
            // Nothing (new) to ship — remember the fingerprint so the file is
            // not re-read until it grows again.
            self.scanner.record_clean_scan(&rollout.path, fp);
            return;
        }

        let mut failed = 0usize;
        let mut delivered = 0usize;
        for anchor in &anchors {
            if self.push(rollout, anchor).await {
                self.scanner.record_delivered(&rollout.path, anchor);
                delivered += 1;
            } else {
                failed += 1;
            }
        }

        let state = self.delivery.entry(rollout.path.clone()).or_default();
        if failed == 0 {
            state.consecutive_failures = 0;
            state.backoff_until = None;
            self.scanner.record_clean_scan(&rollout.path, fp);
            info!(
                rollout = %rollout.path.display(),
                delivered,
                "codex spawn anchors pushed",
            );
            return;
        }
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let exponent = state.consecutive_failures.saturating_sub(1).min(16);
        let delay = self
            .config
            .backoff_initial
            .saturating_mul(1u32 << exponent)
            .min(self.config.backoff_cap);
        state.backoff_until = Some(now + delay);
        warn!(
            rollout = %rollout.path.display(),
            failed,
            delivered,
            consecutive_failures = state.consecutive_failures,
            retry_in_secs = delay.as_secs(),
            "codex spawn anchor push had failures; backing off",
        );
    }

    /// Push one anchor row. `true` when the server stored it — a dedup counts,
    /// since the bytes are already there.
    async fn push(&self, rollout: &CodexSessionFile, anchor: &SubAgentAnchor) -> bool {
        let Some(records) = anchor_records(anchor) else {
            // Unreachable in practice: the crate's converter emits a valid
            // array. Reported rather than silently dropped because it would
            // mean a rollout line the parser accepted and the encoder could
            // not.
            warn!(
                thread_id = %anchor.thread_id,
                "codex anchor records were not valid JSON; skipping",
            );
            return false;
        };
        let payload = build_anchor_payload(rollout, anchor, HARNESS_ID_CODEX, &records);
        match self.client.post_transcript(&payload).await {
            Ok(outcome) => {
                debug!(
                    thread_id = %anchor.thread_id,
                    call_id = %anchor.call_id,
                    deduped = outcome.deduped,
                    "codex spawn anchor pushed",
                );
                true
            }
            Err(err) => {
                warn!(
                    error = %err,
                    thread_id = %anchor.thread_id,
                    call_id = %anchor.call_id,
                    "codex spawn anchor push failed",
                );
                false
            }
        }
    }
}

/// Spawn an anchor lane, returning its shutdown trigger and join handle.
///
/// Split out so the caller's shutdown reads as one thing: fire the trigger,
/// then await the handle. Awaiting is mandatory — see [`CodexAnchorLane::run`].
pub fn spawn(
    client: TranscriptClient,
    snapshot: CodexWatcherSnapshotHandle,
    config: AnchorLaneConfig,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    info!(
        tick_secs = config.tick.as_secs(),
        "codex anchor lane started",
    );
    let handle = tokio::spawn(CodexAnchorLane::new(client, snapshot, config).run(rx));
    (tx, handle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use tapes_harnesses::attribution::CodexWatcherSnapshot;
    use tapes_harnesses::transcript::codex_anchors::fixtures;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// This client's Codex provider prefix, as `start` declares it.
    const PROVIDER: &str = crate::start::CODEX_PROVIDER_PREFIX;

    fn rollout_at(path: &Path) -> CodexSessionFile {
        let mut session = fixtures::session_file(path.to_path_buf());
        // A per-process suffix, the shape the provider filter matches.
        session.model_provider = Some(format!("{PROVIDER}-abc123"));
        session
    }

    fn snapshot_of(sessions: Vec<CodexSessionFile>) -> CodexWatcherSnapshotHandle {
        Arc::new(ArcSwap::from_pointee(CodexWatcherSnapshot { sessions }))
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

    fn lane_for(server: &MockServer, rollouts: Vec<CodexSessionFile>) -> CodexAnchorLane {
        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        CodexAnchorLane::new(
            client,
            snapshot_of(rollouts),
            AnchorLaneConfig::new(PROVIDER),
        )
    }

    fn write_rollout(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, body).unwrap();
        path
    }

    async fn bodies(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| String::from_utf8(request.body.clone()).unwrap())
            .collect()
    }

    /// The cross-client parity assertion, and the reason this module exists:
    /// the rows this lane POSTs are the exact bytes the shared crate's fixture
    /// declares, which is what the daemon's own lane asserts against. A Codex
    /// session captured here therefore carries the same causal skeleton it
    /// would have carried through the platform — not a similar one.
    #[tokio::test]
    async fn the_anchor_rows_are_the_shared_fixture_bytes() {
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), fixtures::ROLLOUT);

        lane_for(&server, vec![rollout_at(&rollout)])
            .tick(true)
            .await;

        assert_eq!(bodies(&server).await, fixtures::BODIES.to_vec());
    }

    #[tokio::test]
    async fn the_spawn_edge_reaches_ingest_with_its_call_id_and_child_thread() {
        // The claim in miniature: the row names the child thread and the
        // spawn_agent call that created it, keyed to the ROOT session — which
        // is what lets the deriver parent the child's span under that call.
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));

        lane_for(&server, vec![rollout_at(&rollout)])
            .tick(true)
            .await;

        let body = bodies(&server).await.join("\n");
        assert!(
            body.contains(r#""agent_id":"019f8d46-e663-74e1-940c-f82e34c07618""#),
            "got: {body}",
        );
        assert!(
            body.contains(r#""tool_use_id":"call_J7B6r7ZdtqkECtSJV8YDQaL7""#),
            "got: {body}",
        );
        assert!(
            body.contains(&format!(
                r#""harness_session_id":"{}""#,
                fixtures::ROOT_SESSION_ID
            )),
            "anchors key to the root session, not the spawning thread: {body}",
        );
        assert!(body.contains(r#""harness_id":"codex""#), "got: {body}");
    }

    #[tokio::test]
    async fn an_unchanged_rollout_is_not_pushed_twice() {
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), fixtures::ROLLOUT);
        let mut lane = lane_for(&server, vec![rollout_at(&rollout)]);

        lane.tick(true).await;
        lane.tick(true).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_spawn_appended_after_the_first_scan_is_pushed() {
        // The live case: a session spawns its second subagent minutes in, and
        // the rollout grows under a lane that already scanned it.
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));
        let mut lane = lane_for(&server, vec![rollout_at(&rollout)]);
        lane.tick(false).await;

        let second = fixtures::STARTED_LINE
            .replace("call_J7B6r7ZdtqkECtSJV8YDQaL7", "call_second")
            .replace(
                "019f8d46-e663-74e1-940c-f82e34c07618",
                "019f8d47-0473-7743-a1ed-9e4c0ae92ad8",
            );
        std::fs::write(&rollout, format!("{}\n{second}\n", fixtures::STARTED_LINE)).unwrap();
        lane.tick(false).await;

        let body = bodies(&server).await.join("\n");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert!(body.contains("call_second"), "got: {body}");
    }

    #[tokio::test]
    async fn a_rollout_from_another_provider_is_not_ours_to_ship() {
        // A codex the user ran directly, or one captured by a concurrent
        // daemon, writes rollouts into the same directory. Shipping those
        // would file another capture's skeleton under this proxy's ingest.
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), fixtures::ROLLOUT);
        let mut foreign = rollout_at(&rollout);
        foreign.model_provider = Some("openai".to_owned());

        lane_for(&server, vec![foreign]).tick(true).await;

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rollout_without_spawn_records_pushes_nothing() {
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(
            dir.path(),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n",
        );

        lane_for(&server, vec![rollout_at(&rollout)])
            .tick(true)
            .await;

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_push_opens_a_backoff_window_and_keeps_the_anchor_owed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream sad"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));
        let mut lane = lane_for(&server, vec![rollout_at(&rollout)]);

        lane.tick(false).await;

        let state = lane.delivery.get(&rollout).unwrap();
        assert_eq!(state.consecutive_failures, 1);
        assert!(state.backoff_until.is_some());
        assert_eq!(
            lane.scanner.delivered_count(&rollout),
            0,
            "a failed anchor must not be recorded as delivered",
        );
        assert!(
            lane.scanner.needs_read(&rollout, fingerprint(&rollout)),
            "a failed scan must re-run rather than be treated as clean",
        );
    }

    #[tokio::test]
    async fn the_exit_pass_overrides_an_open_backoff_window() {
        // The process is about to end. Deferring to a backoff window that
        // outlives it would drop the skeleton this lane exists to deliver.
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));
        let mut lane = lane_for(&server, vec![rollout_at(&rollout)]);
        lane.delivery.insert(
            rollout.clone(),
            DeliveryState {
                consecutive_failures: 1,
                backoff_until: Some(Instant::now() + Duration::from_secs(3600)),
            },
        );

        lane.tick(true).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_auth_header_rides_along() {
        // The daemon client's X-Paper-Auth is its channel to its hosted edge, not part of
        // the tapes contract — and the anchor row is the same row either way.
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));

        lane_for(&server, vec![rollout_at(&rollout)])
            .tick(true)
            .await;

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get("x-paper-auth").is_none());
        assert!(requests[0].headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn the_lane_stops_on_shutdown_after_a_final_pass() {
        let server = accepting_server().await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));
        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        let (shutdown, handle) = spawn(
            client,
            snapshot_of(vec![rollout_at(&rollout)]),
            AnchorLaneConfig::new(PROVIDER),
        );

        // Shut down before the first scheduled tick: the anchor must still
        // arrive, because the final pass runs after the trigger fires.
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the anchor lane must stop on shutdown")
            .expect("the anchor lane task must not panic");

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// An ingest that accepts the connection and then says nothing is the one
    /// failure a delivery lane cannot distinguish from slowness, so the final
    /// pass answers to a clock too. Without the bound this test hangs, which
    /// is exactly what the user would experience: the harness has exited and
    /// the shell has not come back.
    #[tokio::test]
    async fn a_silent_ingest_does_not_hold_the_terminal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            // Longer than any deadline this test allows, so the response never
            // arrives within the window under test.
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let rollout = write_rollout(dir.path(), &format!("{}\n", fixtures::STARTED_LINE));
        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        let (shutdown, handle) = spawn(
            client,
            snapshot_of(vec![rollout_at(&rollout)]),
            AnchorLaneConfig {
                final_pass_deadline: Duration::from_millis(200),
                ..AnchorLaneConfig::new(PROVIDER)
            },
        );

        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("a silent ingest must not keep the lane running")
            .expect("the anchor lane task must not panic");
    }
}
