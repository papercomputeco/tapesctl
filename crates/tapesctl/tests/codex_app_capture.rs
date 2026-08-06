//! End-to-end desktop capture: the lifecycle route and the wire lane on one
//! proxy, exactly as `tapesctl capture codex-app` serves them.
//!
//! The claim under test is the one that makes capturing an app nobody launched
//! safe: **a turn is filed under a Codex session only when an authenticated
//! lifecycle report introduced that session, and under `unknown` otherwise.**
//! Unit tests show the receiver refuses a bad secret; only running both lanes
//! against one proxy shows what that refusal does to the turn — which is the
//! part a regression would actually cost.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, spawn_codex_watcher, spawn_watcher,
};
use tapesctl::codex_app::lifecycle::DesktopSessions;
use tapesctl::codex_app::{LIFECYCLE_PATH, LIFECYCLE_SECRET_HEADER, LifecycleReport};
use tapesctl::start::ingest::IngestClient;
use tapesctl::start::proxy::{ProxyState, forward_handler};
use tapesctl::transcript::tailer::SessionTracker;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The handoff secret this capture was started with.
const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The route a Codex request takes through the proxy.
const RESPONSES_PATH: &str = "/responses";

struct Harness {
    proxy: SocketAddr,
    ingest: MockServer,
    /// Held only to keep the server alive. Dropping it frees the port the
    /// proxy is still pointed at, and these tests run concurrently in one
    /// process — so another test's `MockServer` can bind that port and start
    /// receiving this proxy's upstream traffic. The symptom is a mock seeing
    /// a request meant for someone else, which reads as a capture defect.
    _upstream: MockServer,
}

/// A proxy configured the way `capture codex-app` configures one: no launched
/// PID, no self-attribution, no Codex open-rollout lane, and a desktop
/// registry behind an authenticated route.
async fn start_harness() -> Harness {
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
    let codex_dir = tempfile::tempdir().unwrap();
    let attribution = AttributionState::new(
        spawn_watcher(claude_dir.path().to_path_buf()),
        spawn_codex_watcher(codex_dir.path().to_path_buf()),
    );

    let sessions = Arc::new(DesktopSessions::new(SECRET));
    let state = ProxyState {
        upstream: Url::parse(&upstream.uri()).unwrap(),
        ingest: IngestClient::new(&Url::parse(&ingest.uri()).unwrap()).unwrap(),
        attribution: Arc::new(attribution),
        attribution_config: Arc::new(AttributionConfig::new(CodexProviderFilter::new(
            "tapesctl-codex-app",
        ))),
        provider: "openai",
        codex_marker_header: Arc::new("x-tapesctl-codex-attribution".to_owned()),
        // The open-rollout lane is deliberately off. It would resolve some of
        // these requests to `harness_id: codex` — the CLI, not the app — and a
        // desktop session must not be able to land under two harnesses
        // depending on which lane answered first.
        codex_lane: false,
        self_attributing: false,
        launched_pid: Arc::new(std::sync::atomic::AtomicI32::new(0)),
        gateway_nonce: Arc::new(String::new()),
        org_id: Arc::new(String::new()),
        auth_subject: Arc::new("local:test".to_owned()),
        session_seen: Arc::new(tokio::sync::Mutex::new(None)),
        desktop_sessions: Some(Arc::clone(&sessions)),
        transcript_tracker: SessionTracker::new(),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            LIFECYCLE_PATH,
            axum::routing::post(tapesctl::codex_app::lifecycle::receive),
        )
        .with_state(sessions)
        .merge(
            axum::Router::new()
                .fallback(forward_handler)
                .with_state(state),
        );
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    std::mem::forget(claude_dir);
    std::mem::forget(codex_dir);

    Harness {
        proxy,
        ingest,
        _upstream: upstream,
    }
}

/// Post one lifecycle report, presenting `secret` if given.
async fn report(
    proxy: SocketAddr,
    secret: Option<&str>,
    session_id: &str,
    agent_id: Option<&str>,
) -> reqwest::StatusCode {
    let mut request = reqwest::Client::new()
        .post(format!("http://{proxy}{LIFECYCLE_PATH}"))
        .json(&LifecycleReport {
            session_id: session_id.to_owned(),
            cwd: "/tmp/desktop".to_owned(),
            agent_id: agent_id.map(str::to_owned),
        });
    if let Some(secret) = secret {
        request = request.header(LIFECYCLE_SECRET_HEADER, secret);
    }
    request.send().await.unwrap().status()
}

/// One inference request, carrying the identity headers Codex stamps.
async fn turn(proxy: SocketAddr, thread_id: &str, session_id: &str) {
    reqwest::Client::new()
        .post(format!("http://{proxy}{RESPONSES_PATH}"))
        .header("content-type", "application/json")
        .header("thread-id", thread_id)
        .header("session-id", session_id)
        .body(r#"{"model":"gpt-5-codex"}"#)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
}

async fn await_session_block(ingest: &MockServer) -> serde_json::Value {
    for _ in 0..100 {
        let requests = ingest.received_requests().await.unwrap_or_default();
        if let Some(request) = requests.first() {
            let turn: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            // `session` is skipped when serialising a turn that carries no
            // envelope, so a missing block reads as `Null` at every call site
            // and compares unequal to whatever was expected. Saying so here
            // keeps that failure from looking like a wrong harness id.
            assert!(
                !turn["session"].is_null(),
                "the turn carries no session block at all: {turn}",
            );
            return turn["session"].clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no turn was posted to ingest");
}

/// The happy path, and the one that has to match paper: a desktop session
/// captured here lands under `codex-app` with the app's own session id, so a
/// session captured through either client is one session under one harness.
#[tokio::test]
async fn an_introduced_session_files_its_turns_under_codex_app() {
    let harness = start_harness().await;
    assert_eq!(
        report(harness.proxy, Some(SECRET), "root-session", None).await,
        reqwest::StatusCode::NO_CONTENT,
    );

    turn(harness.proxy, "root-session", "root-session").await;

    let session = await_session_block(&harness.ingest).await;
    assert_eq!(session["harness_id"], "codex-app");
    assert_eq!(session["harness_session_id"], "root-session");
    assert_eq!(session["cwd"], "/tmp/desktop");
}

/// The trust boundary, proved end to end. The report is refused, so the
/// session is never introduced, so the turn that names it is unattributable —
/// and files as `unknown` rather than under the id it claimed.
#[tokio::test]
async fn a_turn_whose_report_was_refused_files_as_unknown() {
    let harness = start_harness().await;
    assert_eq!(
        report(harness.proxy, Some("wrong-secret"), "root-session", None).await,
        reqwest::StatusCode::UNAUTHORIZED,
    );

    turn(harness.proxy, "root-session", "root-session").await;

    let session = await_session_block(&harness.ingest).await;
    assert_eq!(session["harness_id"], "unknown");
    assert!(session["harness_session_id"].is_null(), "got: {session}");
}

/// Same for a report that presented no secret at all — the shape a local
/// process with no access to the handoff file can produce.
#[tokio::test]
async fn a_turn_whose_report_carried_no_secret_files_as_unknown() {
    let harness = start_harness().await;
    assert_eq!(
        report(harness.proxy, None, "root-session", None).await,
        reqwest::StatusCode::UNAUTHORIZED,
    );

    turn(harness.proxy, "root-session", "root-session").await;

    assert_eq!(
        await_session_block(&harness.ingest).await["harness_id"],
        "unknown",
    );
}

/// Nothing introduced this session, so nothing files under it. This is the
/// closed-set property: the proxy never falls back to the only session it
/// happens to know.
#[tokio::test]
async fn a_session_no_report_named_files_as_unknown_even_beside_a_known_one() {
    let harness = start_harness().await;
    report(harness.proxy, Some(SECRET), "root-session", None).await;

    turn(harness.proxy, "some-other-session", "some-other-session").await;

    assert_eq!(
        await_session_block(&harness.ingest).await["harness_id"],
        "unknown",
    );
}

/// The property the three `unknown` cases above each show once: on a desktop
/// capture every turn carries a session block. A turn posted without one is
/// not an unattributed turn — it is an invisible one, indistinguishable
/// downstream from traffic nobody was asked to attribute, so a whole broken
/// install could report nothing rather than a stream of `unknown`.
#[tokio::test]
async fn every_desktop_turn_carries_a_session_block_even_when_unattributed() {
    let harness = start_harness().await;

    turn(harness.proxy, "never-reported", "never-reported").await;

    let session = await_session_block(&harness.ingest).await;
    assert_eq!(session["harness_id"], "unknown");
    assert!(session["harness_session_id"].is_null(), "got: {session}");
}

/// A subagent's request names its own thread; the report that announced the
/// child is what joins it back to the session it ran under, and the sub-thread
/// id still reaches ingest so the derived spans keep the lineage.
#[tokio::test]
async fn a_subagent_turn_files_under_the_root_and_keeps_its_thread_id() {
    let harness = start_harness().await;
    report(
        harness.proxy,
        Some(SECRET),
        "root-session",
        Some("child-thread"),
    )
    .await;

    turn(harness.proxy, "child-thread", "root-session").await;

    for _ in 0..100 {
        let requests = harness.ingest.received_requests().await.unwrap_or_default();
        if let Some(request) = requests.first() {
            let posted: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(posted["session"]["harness_id"], "codex-app");
            assert_eq!(posted["session"]["harness_session_id"], "root-session");
            assert_eq!(posted["meta"]["thread_id"], "child-thread");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no turn was posted to ingest");
}
