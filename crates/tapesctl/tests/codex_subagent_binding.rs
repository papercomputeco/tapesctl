//! Where a Codex sub-thread's turn is filed by `tapesctl start codex`.
//!
//! The property: **one Codex workload is one captured session, with the
//! subagent's turns inside it.** A sub-thread is not a session of its own —
//! it shares its root's, exactly as a Claude subagent shares its root's
//! transcript file — so a spawned agent's turns must land on the row the
//! spawning turns landed on.
//!
//! What makes that hard is timing. Codex spawns a sub-thread and immediately
//! makes its first inference call, so the child's rollout file is routinely
//! not on disk (or not yet scanned) when its first turn arrives, and the only
//! live rollouts a watcher can see may belong to a *previous* run entirely.
//! Every lane that could answer "which rollout is this" is therefore either
//! empty or wrong at exactly the moment it is asked.
//!
//! The answer is that identity does not come from the rollout lanes at all
//! for a sub-thread: the request names its own root, and that is what the
//! envelope is keyed on. These tests hold the rollout lanes in each of the
//! three states the race produces and assert the same session id every time.
//!
//! Regression: a client that did not pass the request's identity to the
//! attribution pipeline had these turns keyed on whichever rollout the ladder
//! resolved — the child's own — which files as a second, unparented session
//! and silently costs the spawning session its whole subagent subtree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tapes_capture::envelope::{
    CODEX_PARENT_THREAD_ID_HEADER, CODEX_SESSION_ID_HEADER, CODEX_THREAD_ID_HEADER,
};
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::harness::RegistryUserAgents;
use tapesctl::start::ingest::IngestClient;
use tapesctl::start::proxy::{ProxyState, forward_handler};
use tapesctl::transcript::tailer::SessionTracker;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The route a Codex request takes through the proxy.
const RESPONSES_PATH: &str = "/v1/responses";

/// The provider id family `tapesctl start codex` stamps on its launches. The
/// per-launch suffix is what distinguishes two concurrent launches; the base
/// is what the filter matches.
const PROVIDER_BASE: &str = "tapesctl-openai";

/// This launch's provider id.
const PROVIDER: &str = "tapesctl-openai-11111111-1111-4111-8111-111111111111";

/// A *previous* launch's provider id. Still matches the filter — that is the
/// point — so its rollout is a live candidate long after its run ended.
const PREVIOUS_PROVIDER: &str = "tapesctl-openai-22222222-2222-4222-8222-222222222222";

const ROOT: &str = "019fd83f-b8ef-7b43-a572-561267b796b3";
const CHILD: &str = "019fd83f-c9de-7c60-86e7-88250ef5e37a";
const PREVIOUS_ROOT: &str = "019fd837-b56c-7731-a18e-442fa316fef1";

struct Harness {
    proxy: SocketAddr,
    ingest: MockServer,
    /// Held only to keep the server alive; see `codex_app_capture.rs` for why
    /// dropping it mid-test hands this proxy's upstream traffic to whichever
    /// concurrent test binds the freed port next.
    _upstream: MockServer,
}

/// Write a root rollout: a session Codex started for a user, with no lineage.
fn write_root_rollout(dir: &Path, id: &str, provider: &str) {
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    std::fs::write(
        dir.join(format!("rollout-{id}.jsonl")),
        format!(
            r#"{{"timestamp":"{now}","type":"session_meta","payload":{{"id":"{id}","timestamp":"{now}","cwd":"/tmp/work","originator":"codex_exec","cli_version":"0.146.0","source":"exec","thread_source":"user","model_provider":"{provider}"}}}}"#
        ),
    )
    .unwrap();
}

/// Write a sub-thread rollout as codex-cli 0.146.0 actually writes one.
///
/// Note the absence of `session_id`: the shipped CLI records the immediate
/// parent and declares itself a subagent, but names no root. A transcript
/// that cannot state its own root cannot be joined to a request by lineage,
/// which is precisely why identity has to come from the request instead.
fn write_child_rollout(dir: &Path, id: &str, parent: &str, provider: &str) {
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    std::fs::write(
        dir.join(format!("rollout-{id}.jsonl")),
        format!(
            r#"{{"timestamp":"{now}","type":"session_meta","payload":{{"id":"{id}","timestamp":"{now}","parent_thread_id":"{parent}","cwd":"/tmp/work","originator":"codex_exec","cli_version":"0.146.0","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}","depth":1}}}}}},"thread_source":"subagent","model_provider":"{provider}"}}}}"#
        ),
    )
    .unwrap();
}

/// A proxy configured the way `start codex` configures one: the open-rollout
/// lane on, no desktop registry, no self-attribution.
///
/// `rollouts` is the sessions directory the watcher scans; the caller has
/// already put into it whatever the race is meant to have left there.
async fn start_harness(rollouts: &Path) -> Harness {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"resp_1"}"#))
        .mount(&upstream)
        .await;

    let ingest = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ingest"))
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted"}"#))
        .mount(&ingest)
        .await;

    let claude_dir = tempfile::tempdir().unwrap();
    let attribution = AttributionState::new(
        spawn_watcher(claude_dir.path().to_path_buf()),
        spawn_codex_watcher(rollouts.to_path_buf()),
    );

    // Production timeouts with a short Codex budget. The ladder's bounded wait
    // is not what these tests are about, and a turn whose rollout will never
    // appear would otherwise hold each one for the full production budget.
    let mut config = AttributionConfig::new(
        CodexProviderFilter::new(PROVIDER_BASE),
        RegistryUserAgents::default(),
    );
    config.codex_timeout = Duration::from_millis(200);

    let state = ProxyState {
        // Counted but never read here; the exit summary is `start`'s.
        tally: Arc::new(tapesctl::start::tally::CaptureTally::new()),
        upstream: Url::parse(&upstream.uri()).unwrap(),
        ingest: IngestClient::new(&Url::parse(&ingest.uri()).unwrap()).unwrap(),
        attribution: Arc::new(attribution),
        attribution_config: Arc::new(config),
        provider: "openai",
        provider_routes: None,
        codex_marker_header: Arc::new("x-tapesctl-codex-attribution".to_owned()),
        codex_lane: true,
        self_attributing: false,
        launched_pid: Arc::new(std::sync::atomic::AtomicI32::new(0)),
        gateway_nonce: Arc::new(String::new()),
        org_id: Arc::new(String::new()),
        auth_subject: Arc::new("local:test".to_owned()),
        session_seen: Arc::new(tokio::sync::Mutex::new(None)),
        desktop_sessions: None,
        transcript_tracker: SessionTracker::new(),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .fallback(forward_handler)
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    std::mem::forget(claude_dir);

    Harness {
        proxy,
        ingest,
        _upstream: upstream,
    }
}

/// One inference call from the root thread.
async fn root_turn(proxy: SocketAddr) {
    send(
        proxy,
        &[
            (CODEX_SESSION_ID_HEADER, ROOT),
            (CODEX_THREAD_ID_HEADER, ROOT),
        ],
    )
    .await;
}

/// One inference call from a spawned sub-thread, carrying the three ids Codex
/// stamps on every child-shaped call: the root it belongs to, the thread it
/// is, and that thread's immediate parent.
async fn subagent_turn(proxy: SocketAddr) {
    send(
        proxy,
        &[
            (CODEX_SESSION_ID_HEADER, ROOT),
            (CODEX_THREAD_ID_HEADER, CHILD),
            (CODEX_PARENT_THREAD_ID_HEADER, ROOT),
        ],
    )
    .await;
}

async fn send(proxy: SocketAddr, headers: &[(&str, &str)]) {
    let mut request = reqwest::Client::new()
        .post(format!("http://{proxy}{RESPONSES_PATH}"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex"}"#);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap().bytes().await.unwrap();
}

/// Wait for `expected` turns to reach ingest and return their session blocks.
async fn session_blocks(ingest: &MockServer, expected: usize) -> Vec<serde_json::Value> {
    for _ in 0..200 {
        let requests = ingest.received_requests().await.unwrap_or_default();
        if requests.len() >= expected {
            return requests
                .iter()
                .map(|request| {
                    let turn: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    assert!(
                        !turn["session"].is_null(),
                        "a turn reached ingest with no session block at all: {turn}",
                    );
                    turn["session"].clone()
                })
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("fewer than the {expected} expected turns were posted to ingest");
}

/// Assert a turn was filed under the launched harness, not the sentinel.
///
/// The session-id assertions below are about *which* session a turn joined;
/// this is about whether it joined one at all. They are separable failures: a
/// turn can carry the right session id and still be filed under `unknown`, and
/// a suite that only checks the id would pass while every row lands
/// unattributed. Both halves of the envelope name the launched harness or the
/// capture is not doing its job.
fn assert_attributed_to_codex(block: &serde_json::Value) {
    assert_eq!(
        block["harness_id"], "codex",
        "a launched codex turn must be filed under codex, not the unknown \
         sentinel: {block}",
    );
    assert!(
        block["harness_session_id"].is_string(),
        "an attributed turn must name the session it belongs to: {block}",
    );
}

/// Every session id a set of turns was filed under, deduplicated.
fn distinct_session_ids(blocks: &[serde_json::Value]) -> Vec<String> {
    let mut ids: Vec<String> = blocks
        .iter()
        .map(|block| {
            block["harness_session_id"]
                .as_str()
                .unwrap_or_else(|| panic!("a turn was filed with no session id: {block}"))
                .to_owned()
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The whole property, in the shape the race actually produces: the root's
/// rollout is on disk, the child's is not yet, and both turns must file under
/// the root.
#[tokio::test]
async fn a_subagent_turn_joins_its_root_while_its_own_rollout_is_still_missing() {
    let rollouts = tempfile::tempdir().unwrap();
    write_root_rollout(rollouts.path(), ROOT, PROVIDER);
    let harness = start_harness(rollouts.path()).await;

    root_turn(harness.proxy).await;
    subagent_turn(harness.proxy).await;

    let blocks = session_blocks(&harness.ingest, 2).await;
    for block in &blocks {
        assert_attributed_to_codex(block);
    }
    assert_eq!(
        distinct_session_ids(&blocks),
        vec![ROOT.to_owned()],
        "one workload must be one session: {blocks:?}",
    );
}

/// The orphan, reproduced exactly. By the time the child's turn is attributed
/// its rollout HAS reached the watcher — so the ladder can resolve it, and
/// resolving it is the trap: that rollout genuinely is where the turn was
/// written, so keying on it looks right and files a second, unparented
/// session that no later pass can re-parent.
#[tokio::test]
async fn a_subagent_turn_does_not_split_off_onto_its_own_rollout() {
    let rollouts = tempfile::tempdir().unwrap();
    write_root_rollout(rollouts.path(), ROOT, PROVIDER);
    write_child_rollout(rollouts.path(), CHILD, ROOT, PROVIDER);
    let harness = start_harness(rollouts.path()).await;

    subagent_turn(harness.proxy).await;

    let blocks = session_blocks(&harness.ingest, 1).await;
    assert_attributed_to_codex(&blocks[0]);
    assert_eq!(
        blocks[0]["harness_session_id"], ROOT,
        "the sub-thread's turn was keyed on its own rollout: {blocks:?}",
    );
    // A sub-thread shares its root's session rather than descending from it.
    // A parent id here would placeholder-insert a session keyed by a THREAD
    // id — a second row again, by another route.
    assert!(
        blocks[0]["parent_harness_session_id"].is_null(),
        "a sub-thread's turn must not claim a parent session: {blocks:?}",
    );
}

/// The coldest case, and the one the live capture log named: no rollout of
/// this run has been scanned yet, and the only live candidate belongs to a
/// PREVIOUS run whose provider id still matches the filter. Every rollout lane
/// is either empty or wrong, and the turn still files under its root, because
/// the request said so.
#[tokio::test]
async fn a_subagent_turn_binds_when_the_only_live_rollout_is_a_previous_runs() {
    let rollouts = tempfile::tempdir().unwrap();
    write_root_rollout(rollouts.path(), PREVIOUS_ROOT, PREVIOUS_PROVIDER);
    let harness = start_harness(rollouts.path()).await;

    subagent_turn(harness.proxy).await;

    let blocks = session_blocks(&harness.ingest, 1).await;
    assert_attributed_to_codex(&blocks[0]);
    assert_eq!(
        blocks[0]["harness_session_id"], ROOT,
        "a cold watcher must not cost a sub-thread its identity: {blocks:?}",
    );
}

/// The refusal itself is not softened by any of the above. A ROOT turn names
/// a rollout that is not among the live candidates, so nothing may be guessed
/// for it — the previous run's session is not an acceptable substitute.
///
/// What it does carry is the correlation id, which is what makes the refusal
/// recoverable rather than terminal: it is the key a later repair pass joins
/// this stored turn back to.
#[tokio::test]
async fn a_root_turn_still_refuses_a_rollout_that_is_not_among_the_candidates() {
    let rollouts = tempfile::tempdir().unwrap();
    write_root_rollout(rollouts.path(), PREVIOUS_ROOT, PREVIOUS_PROVIDER);
    let harness = start_harness(rollouts.path()).await;

    root_turn(harness.proxy).await;

    let blocks = session_blocks(&harness.ingest, 1).await;
    assert!(
        blocks[0]["harness_session_id"].is_null(),
        "the previous run's session was guessed for this turn: {blocks:?}",
    );
    assert!(
        blocks[0]["harness_metadata"]["paperProxyRequestId"].is_string(),
        "an unbound turn carries no correlation id, so it can never be repaired: {blocks:?}",
    );
}
