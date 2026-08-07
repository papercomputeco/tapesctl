//! What the capture proxy considers worth warning about.
//!
//! A warning means "a turn you expected to see was dropped". The model listings,
//! health probes and auth probes a harness makes several of per session are not
//! that: they were never turns, so there is nothing to capture and never could
//! be. Warning on them trains the reader to skim past the severity that matters,
//! which is how the genuinely dropped turn goes unnoticed.
//!
//! An upstream that refused the call is on the other side of that line. The
//! exchange is not captured — a turn is a completed exchange, and the store
//! refuses a reduction with no assistant message anyway — but it is a call the
//! reader expected to see recorded, so it warns and it names the status. That
//! is the whole diagnostic value a failed call still has, and discarding it
//! silently would be the one genuinely bad outcome.
//!
//! The turns that ARE dropped must also say which of the reasons applies. A
//! body in an encoding this build cannot decode and a body that is genuinely
//! malformed are two different defects with two different fixes, and reporting
//! the first as the second is what let every zstd request go uncaptured while
//! the log calmly described the payload as invalid JSON.
//!
//! One test, deliberately: this binary installs a process-global subscriber and
//! reads what it collected, so two tests running in parallel would interleave
//! into the same buffer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::harness::RegistryUserAgents;
use tapesctl::start::ProviderRoutes;
use tapesctl::start::ingest::IngestClient;
use tapesctl::start::proxy::{ProxyState, forward_handler};
use tapesctl::transcript::tailer::SessionTracker;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A writer the test can read back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn contents(&self) -> String {
        self.0
            .lock()
            .map(|buf| String::from_utf8_lossy(&buf).into_owned())
            .unwrap_or_default()
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut inner) = self.0.lock() {
            inner.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn proxy_against(upstream: &MockServer, ingest: &MockServer) -> SocketAddr {
    proxy_with(upstream, ingest, None).await
}

async fn proxy_with(
    upstream: &MockServer,
    ingest: &MockServer,
    provider_routes: Option<Arc<ProviderRoutes>>,
) -> SocketAddr {
    let claude_dir = tempfile::tempdir().unwrap();
    let codex_dir = tempfile::tempdir().unwrap();
    let state = ProxyState {
        upstream: Url::parse(&upstream.uri()).unwrap(),
        ingest: IngestClient::new(&Url::parse(&ingest.uri()).unwrap()).unwrap(),
        attribution: Arc::new(AttributionState::new(
            spawn_watcher(claude_dir.path().to_path_buf()),
            spawn_codex_watcher(codex_dir.path().to_path_buf()),
        )),
        attribution_config: Arc::new(AttributionConfig::new(
            CodexProviderFilter::new("tapesctl-openai"),
            RegistryUserAgents::default(),
        )),
        provider: "anthropic",
        provider_routes,
        codex_marker_header: Arc::new("x-tapesctl-codex-attribution".to_owned()),
        codex_lane: false,
        self_attributing: false,
        launched_pid: Arc::new(std::sync::atomic::AtomicI32::new(0)),
        gateway_nonce: Arc::new("test-launch-nonce".to_owned()),
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

    // The watchers hold paths into these; the OS reclaims them at exit.
    std::mem::forget(claude_dir);
    std::mem::forget(codex_dir);
    proxy
}

#[tokio::test]
async fn a_bodiless_request_is_not_a_warning_but_a_malformed_one_still_is() {
    let logs = Captured::default();
    // `trace`, so the demoted line is observable — the test has to show the
    // diagnostic still exists, not merely that the warning is gone. Scoped to
    // this crate, because a bare `trace` buries the assertion message under
    // hyper's connection-pool chatter when it fails.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("tapesctl=trace"))
        .with_writer(logs.clone())
        .with_ansi(false)
        .try_init();

    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&upstream)
        .await;
    // A second turn path, so the refused-exchange shape below can be exercised
    // without a second answer for the one above.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"bad_request"}"#))
        .mount(&upstream)
        .await;

    let ingest = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ingest"))
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted"}"#))
        .mount(&ingest)
        .await;

    let proxy = proxy_against(&upstream, &ingest).await;
    let client = reqwest::Client::new();

    // A GET, exactly as a harness lists models: no body at all.
    let response = client
        .get(format!("http://{proxy}/v1/models"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let _ = response.bytes().await.unwrap();

    // Capture is finalized after the response stream ends, so the log line
    // lands a moment after the client has been served.
    settle().await;

    let after_get = logs.contents();
    assert!(
        !after_get.contains("request body is not JSON"),
        "a bodiless GET must not warn — this is the line that corrupted TUIs: {after_get}",
    );
    assert!(
        !after_get.contains("WARN"),
        "a model listing was never a turn; nothing about it is worth a warning: {after_get}",
    );
    assert!(
        after_get.contains("non_turn_request"),
        "the diagnostic must survive the demotion, at debug, and must name the \
         reason the shared corpus specifies: {after_get}",
    );

    // The other half: a body that is present and genuinely unparseable is a
    // dropped turn, and must keep its warning.
    let response = client
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body("this is not json")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let _ = response.bytes().await.unwrap();

    settle().await;

    let after_post = logs.contents();
    assert!(
        after_post.contains("request body is not JSON"),
        "a malformed body is a dropped turn and must still warn: {after_post}",
    );

    // And the third shape: an EMPTY body on a turn-shaped method. Unlike a
    // GET, a POST with nothing in it is a turn that will never be captured —
    // the demotion must not swallow it.
    let response = client
        .post(format!("http://{proxy}/v1/messages"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let _ = response.bytes().await.unwrap();

    settle().await;

    let after_empty_post = logs.contents();
    let warns_before = after_post.matches("request body is not JSON").count();
    let warns_after = after_empty_post.matches("request body is not JSON").count();
    assert!(
        warns_after > warns_before,
        "an empty POST is a dropped turn and must add its own warn \
         (before: {warns_before}, after: {warns_after}): {after_empty_post}",
    );

    // And the fourth: a body in an encoding this build cannot decode. It is a
    // dropped turn like the others, but it is a dropped turn for a different
    // reason, and saying "not JSON" would send whoever reads the log to look at
    // the harness's payload instead of at this proxy's decoder. `br` stands in
    // for any such encoding; zstd used to be one of them, which is how a whole
    // class of sessions captured nothing at all.
    let response = client
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("content-encoding", "br")
        .body("\u{1b}brotli-ish bytes")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let _ = response.bytes().await.unwrap();

    settle().await;

    let after_undecodable = logs.contents();
    assert!(
        after_undecodable.contains("request body could not be decoded"),
        "an undecodable encoding must warn in its own words: {after_undecodable}",
    );
    assert!(
        after_undecodable.contains("br"),
        "the warning must name the encoding that could not be read: {after_undecodable}",
    );
    assert_eq!(
        after_undecodable
            .matches("request body is not JSON")
            .count(),
        warns_after,
        "an undecodable body must NOT be reported as malformed JSON — that \
         conflation is what kept this invisible: {after_undecodable}",
    );

    // And the fifth: a turn-shaped request the upstream refused. Not captured —
    // a turn is a completed exchange — but the operator asked for it and it did
    // not happen, so it warns, and the warning carries the status. A drop that
    // said nothing would be indistinguishable from capture quietly breaking.
    let response = client
        .post(format!("http://{proxy}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400, "the status reaches the harness");
    let _ = response.bytes().await.unwrap();

    settle().await;

    let after_failed = logs.contents();
    assert!(
        after_failed.contains("upstream_status"),
        "a refused exchange must name the reason the shared corpus specifies: {after_failed}",
    );
    assert!(
        after_failed.contains("upstream_status=400"),
        "and must carry the status, which is the first thing a reader needs: {after_failed}",
    );
    assert!(
        after_failed
            .lines()
            .any(|line| line.contains("WARN") && line.contains("upstream_status")),
        "at a severity an operator sees, not buried at debug: {after_failed}",
    );

    // And the sixth, which is not a drop at all: a request labelled with a
    // provider this capture has no upstream for. The exchange does not happen,
    // so it has no status and no drop reason — but it is the strongest form of
    // "a call you expected was not made", and it has to be as visible as the
    // drops above and greppable on its own field.
    let routed = routing_proxy_against(&upstream, &ingest).await;
    let response = client
        .post(format!(
            "http://{routed}/_tapes/provider/gemini/v1/messages"
        ))
        .header("content-type", "application/json")
        .body(r#"{"model":"gemini"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 421);
    let _ = response.bytes().await.unwrap();

    settle().await;

    let after_unroutable = logs.contents();
    assert!(
        after_unroutable
            .lines()
            .any(|line| line.contains("WARN") && line.contains("route_refusal")),
        "a refusal to forward must warn, and carry its own field: {after_unroutable}",
    );
    assert!(
        after_unroutable.contains("unroutable_provider") && after_unroutable.contains("gemini"),
        "the line must name the fault and the provider that caused it: {after_unroutable}",
    );
    // Deliberately NOT in the drop vocabulary: nothing was forwarded, so there
    // is no exchange for a drop reason to be about. Folding it into
    // `drop_reason` would count a request that never reached a provider
    // alongside turns a provider answered.
    assert!(
        !after_unroutable
            .lines()
            .any(|line| line.contains("route_refusal") && line.contains("drop_reason")),
        "a routing refusal must not borrow the drop-reason field: {after_unroutable}",
    );
}

/// A proxy that routes per provider, so a label it cannot resolve is refused
/// rather than forwarded. The route table is deliberately narrow: what is being
/// exercised is the refusal, and a table naming every real provider would need
/// a server per entry to say nothing more.
async fn routing_proxy_against(upstream: &MockServer, ingest: &MockServer) -> SocketAddr {
    let routes =
        ProviderRoutes::from_pairs([("anthropic", upstream.uri().as_str(), "anthropic")]).unwrap();
    proxy_with(upstream, ingest, Some(Arc::new(routes))).await
}

/// Give the detached capture task time to finalize.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}
