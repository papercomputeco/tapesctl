//! What the capture proxy considers worth warning about.
//!
//! A warning means "a turn you expected to see was dropped". Requests with no
//! body — the model listings and auth probes a harness makes several of per
//! session — are not that: there is nothing to capture and never could be.
//! Warning on them trains the reader to skim past the severity that matters,
//! which is how the genuinely dropped turn goes unnoticed.
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
    let claude_dir = tempfile::tempdir().unwrap();
    let codex_dir = tempfile::tempdir().unwrap();
    let state = ProxyState {
        upstream: Url::parse(&upstream.uri()).unwrap(),
        ingest: IngestClient::new(&Url::parse(&ingest.uri()).unwrap()).unwrap(),
        attribution: Arc::new(AttributionState::new(
            spawn_watcher(claude_dir.path().to_path_buf()),
            spawn_codex_watcher(codex_dir.path().to_path_buf()),
        )),
        attribution_config: Arc::new(AttributionConfig::new(CodexProviderFilter::new(
            "tapesctl-openai",
        ))),
        provider: "anthropic",
        codex_marker_header: Arc::new("x-tapesctl-codex-attribution".to_owned()),
        codex_lane: false,
        self_attributing: false,
        org_id: Arc::new(String::new()),
        auth_subject: Arc::new("local:test".to_owned()),
        session_seen: Arc::new(tokio::sync::Mutex::new(None)),
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
        after_get.contains("request had no body"),
        "the diagnostic must survive the demotion, at debug: {after_get}",
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
}

/// Give the detached capture task time to finalize.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}
