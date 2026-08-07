//! Routing a labelled request to the provider's own upstream.
//!
//! A capture extension may register more providers than one upstream can
//! serve. pi's registers three, and before labelling existed all three pointed
//! at whichever upstream the launch had pinned — so a session on either of the
//! other two was forwarded to a host with no such route. The harness reported a
//! failure that looked like the model's, and the 404 body was captured and
//! rejected by ingest as a malformed turn.
//!
//! These tests use two upstreams so the claim being made is the one that
//! matters: not "the prefix is stripped" but "these bytes arrived at *that*
//! host and not the other one". A single-upstream test cannot tell the fix from
//! the bug.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::harness::RegistryUserAgents;
use tapes_harnesses::plugin::provider_route;
use tapesctl::start::ProviderRoutes;
use tapesctl::start::ingest::IngestClient;
use tapesctl::start::proxy::{ProxyState, forward_handler};
use tapesctl::transcript::tailer::SessionTracker;
use url::Url;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The path pi's Anthropic provider asks for. Used as a stand-in for "a path
/// the *other* upstream would not recognise", which is the real-world shape of
/// the bug.
const ANTHROPIC_PATH: &str = "/v1/messages";

/// The path pi's OpenAI provider asks for.
const OPENAI_PATH: &str = "/v1/responses";

struct Harness {
    proxy: SocketAddr,
    ingest: MockServer,
    /// The launch-pinned upstream — where an unlabelled request goes.
    anthropic: MockServer,
    /// The upstream only a label can reach.
    openai: MockServer,
}

/// A proxy fronting two upstreams, launched as a pi capture would be.
async fn start_harness() -> Harness {
    let anthropic = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"upstream":"anthropic"}"#))
        .mount(&anthropic)
        .await;

    let openai = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"upstream":"openai"}"#))
        .mount(&openai)
        .await;

    let ingest = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted"}"#))
        .mount(&ingest)
        .await;

    let claude_dir = tempfile::tempdir().unwrap();
    let codex_dir = tempfile::tempdir().unwrap();
    let attribution = AttributionState::new(
        spawn_watcher(claude_dir.path().to_path_buf()),
        spawn_codex_watcher(codex_dir.path().to_path_buf()),
    );

    // The same three labels a real launch routes, against loopback upstreams.
    // `openai-codex` shares the OpenAI *schema* with `openai` while riding a
    // different host, which is exactly why the table carries an upstream and an
    // ingest provider separately — pointing it at a third server here would
    // check the plumbing without checking that distinction.
    let routes = ProviderRoutes::from_pairs([
        ("anthropic", anthropic.uri().as_str(), "anthropic"),
        ("openai", openai.uri().as_str(), "openai"),
        ("openai-codex", openai.uri().as_str(), "openai"),
    ])
    .unwrap();

    let state = ProxyState {
        // The launch-pinned upstream: what every request went to before a
        // label could say otherwise.
        upstream: Url::parse(&anthropic.uri()).unwrap(),
        ingest: IngestClient::new(&Url::parse(&ingest.uri()).unwrap()).unwrap(),
        attribution: Arc::new(attribution),
        attribution_config: Arc::new(AttributionConfig::new(
            CodexProviderFilter::new("tapesctl-openai"),
            RegistryUserAgents::default(),
        )),
        provider: "anthropic",
        provider_routes: Some(Arc::new(routes)),
        codex_marker_header: Arc::new("x-tapesctl-codex-attribution".to_owned()),
        codex_lane: false,
        self_attributing: true,
        launched_pid: Arc::new(std::sync::atomic::AtomicI32::new(
            i32::try_from(std::process::id()).unwrap(),
        )),
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

    std::mem::forget(claude_dir);
    std::mem::forget(codex_dir);

    Harness {
        proxy,
        ingest,
        anthropic,
        openai,
    }
}

async fn post(proxy: SocketAddr, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy}{path}"))
        .header("content-type", "application/json")
        .body(r#"{"model":"test"}"#)
        .send()
        .await
        .unwrap()
}

/// The paths one upstream was actually asked for.
async fn paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect()
}

/// The posted turn, once the capture task has caught up with the response.
async fn await_captured_turn(ingest: &MockServer) -> serde_json::Value {
    for _ in 0..100 {
        let requests = ingest.received_requests().await.unwrap_or_default();
        if let Some(request) = requests.first() {
            return serde_json::from_slice(&request.body).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no turn was posted to ingest");
}

/// **The defect, as a test.** A request labelled with a provider other than the
/// launch-pinned one reaches that provider's own upstream — and reaches it at
/// the path the harness asked for, with the label gone. Before labelling this
/// went to the Anthropic upstream, which answered `/v1/responses` with a 404.
#[tokio::test]
async fn a_labelled_request_reaches_the_labelled_providers_upstream() {
    let harness = start_harness().await;

    let response = post(
        harness.proxy,
        &format!("{}{OPENAI_PATH}", provider_route("openai")),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"upstream":"openai"}"#,
        "the client was served by the wrong upstream",
    );

    assert_eq!(
        paths(&harness.openai).await,
        vec![OPENAI_PATH.to_owned()],
        "the label must be stripped before the request goes upstream",
    );
    assert!(
        paths(&harness.anthropic).await.is_empty(),
        "the launch-pinned upstream saw a request that was labelled for another",
    );
}

/// The launch-pinned provider is not a special case: it is routed by its label
/// like any other, so the two do not diverge as the table grows.
#[tokio::test]
async fn the_pinned_provider_is_routed_by_its_label_like_the_rest() {
    let harness = start_harness().await;

    let response = post(
        harness.proxy,
        &format!("{}{ANTHROPIC_PATH}", provider_route("anthropic")),
    )
    .await;
    assert_eq!(response.status(), 200);

    assert_eq!(
        paths(&harness.anthropic).await,
        vec![ANTHROPIC_PATH.to_owned()]
    );
    assert!(paths(&harness.openai).await.is_empty());
}

/// A captured turn files under the *labelled* provider, not the launch's.
///
/// Routing the bytes correctly while filing them under the pinned provider
/// would hand ingest an OpenAI payload labelled `anthropic`, which its reducer
/// cannot read — a turn captured and then thrown away, which is the failure
/// this whole path exists to avoid.
#[tokio::test]
async fn a_labelled_turn_files_under_the_labelled_provider() {
    let harness = start_harness().await;

    post(
        harness.proxy,
        &format!("{}{OPENAI_PATH}", provider_route("openai")),
    )
    .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["provider"], "openai");
    // And the turn's meta describes the request that was actually made, so a
    // reader comparing it against the upstream's own logs finds the same route.
    assert_eq!(turn["meta"]["path"], OPENAI_PATH);
}

/// pi's ChatGPT-plan provider is its own upstream but the OpenAI schema, so it
/// must route by label and still file as `openai`. Collapsing the two columns
/// would break one or the other.
#[tokio::test]
async fn the_plan_provider_routes_to_its_own_host_and_files_as_openai() {
    let harness = start_harness().await;

    post(
        harness.proxy,
        &format!("{}{OPENAI_PATH}", provider_route("openai-codex")),
    )
    .await;

    assert_eq!(paths(&harness.openai).await, vec![OPENAI_PATH.to_owned()]);
    assert_eq!(
        await_captured_turn(&harness.ingest).await["provider"],
        "openai"
    );
}

/// **The refusal.** A label this capture has no route for is answered directly,
/// and no upstream is contacted at all — forwarding it to the pinned upstream
/// is precisely the bug. The status is distinguishable from an upstream that
/// was tried and failed, and the message names the provider so the diagnosis is
/// in the response the harness surfaces.
#[tokio::test]
async fn an_unroutable_label_is_refused_rather_than_sent_to_the_wrong_host() {
    let harness = start_harness().await;

    let response = post(
        harness.proxy,
        &format!("{}{ANTHROPIC_PATH}", provider_route("gemini")),
    )
    .await;

    assert_eq!(
        response.status(),
        421,
        "an unroutable label must not be a 502"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("gemini"),
        "the refusal does not name the provider: {body}"
    );
    assert!(
        body.contains("anthropic") && body.contains("openai-codex"),
        "the refusal does not say what this capture can route: {body}",
    );

    assert!(
        paths(&harness.anthropic).await.is_empty() && paths(&harness.openai).await.is_empty(),
        "a request that could not be routed was forwarded anyway",
    );
}

/// An unlabelled request still goes to the launch-pinned upstream.
///
/// This is the installed extension being older than the binary that launched
/// it: the provider it wanted is not in the request, so there is nothing to
/// refuse *on*. Sending it where it would have gone before any of this existed
/// leaves a stale extension no worse than it was, rather than turning an
/// upgrade into a dead session.
#[tokio::test]
async fn an_unlabelled_request_still_reaches_the_launch_upstream() {
    let harness = start_harness().await;

    let response = post(harness.proxy, ANTHROPIC_PATH).await;
    assert_eq!(response.status(), 200);

    assert_eq!(
        paths(&harness.anthropic).await,
        vec![ANTHROPIC_PATH.to_owned()]
    );
    assert!(paths(&harness.openai).await.is_empty());
    assert_eq!(
        await_captured_turn(&harness.ingest).await["provider"],
        "anthropic"
    );
}
