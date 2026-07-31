//! End-to-end capture tests: a real proxy, a real upstream, a real ingest.
//!
//! These are the tests that hold the PR's central claim honest. Unit tests can
//! show that each piece behaves; only running bytes through the whole path
//! shows that what ingest receives is what the harness saw — and that the
//! harness saw exactly what the upstream sent.
//!
//! Everything is loopback: a `wiremock` upstream, a `wiremock` ingest, and the
//! proxy under test bound to an ephemeral port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, spawn_codex_watcher, spawn_watcher,
};
use tapesctl::start::ingest::IngestClient;
use tapesctl::start::proxy::{ProxyState, forward_handler};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The exact bytes a streaming Anthropic turn puts on the wire, framing and
/// all. The capture must reproduce these verbatim.
const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

struct Harness {
    proxy: SocketAddr,
    ingest: MockServer,
    upstream: MockServer,
}

async fn start_harness(response: ResponseTemplate) -> Harness {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(response)
        .mount(&upstream)
        .await;

    let ingest = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ingest"))
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted"}"#))
        .mount(&ingest)
        .await;

    // Watchers over empty temp dirs: no harness is actually running, so every
    // request lands on the unattributed path. That is deliberate here — these
    // tests are about byte fidelity, and attribution has its own tests in the
    // shared crate.
    let claude_dir = tempfile::tempdir().unwrap();
    let codex_dir = tempfile::tempdir().unwrap();
    let attribution = AttributionState::new(
        spawn_watcher(claude_dir.path().to_path_buf()),
        spawn_codex_watcher(codex_dir.path().to_path_buf()),
    );

    let state = ProxyState {
        upstream: Url::parse(&upstream.uri()).unwrap(),
        ingest: IngestClient::new(&Url::parse(&ingest.uri()).unwrap()).unwrap(),
        attribution: Arc::new(attribution),
        attribution_config: Arc::new(AttributionConfig::new(CodexProviderFilter::new(
            "tapesctl-openai",
        ))),
        provider: "anthropic",
        codex_marker_header: Arc::new("x-tapesctl-codex-attribution".to_owned()),
        codex_lane: false,
        org_id: Arc::new(String::new()),
        auth_subject: Arc::new("local:test".to_owned()),
        session_seen: Arc::new(tokio::sync::Mutex::new(None)),
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

    // Keep the temp dirs alive for the duration of the test by leaking them;
    // the watchers hold paths into them and the OS reclaims on exit.
    std::mem::forget(claude_dir);
    std::mem::forget(codex_dir);

    Harness {
        proxy,
        ingest,
        upstream,
    }
}

/// Wait for the capture task to post, which happens after the response stream
/// ends and therefore after the client has already been served.
async fn await_captured_turn_bytes(ingest: &MockServer) -> Vec<u8> {
    for _ in 0..100 {
        let requests = ingest.received_requests().await.unwrap_or_default();
        if let Some(request) = requests.first() {
            return request.body.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no turn was posted to ingest");
}

/// The posted turn, parsed. Convenient — but note that parsing reorders object
/// keys, so any assertion about byte order must use
/// [`await_captured_turn_bytes`] instead.
async fn await_captured_turn(ingest: &MockServer) -> serde_json::Value {
    serde_json::from_slice(&await_captured_turn_bytes(ingest).await).unwrap()
}

async fn post_through_proxy(proxy: SocketAddr, body: &'static str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("user-agent", "claude-cli/2.1.145")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_captured_response_is_byte_identical_to_what_the_client_received() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string(SSE_BODY)
            .insert_header("content-type", "text/event-stream"),
    )
    .await;

    let response = post_through_proxy(harness.proxy, r#"{"model":"claude","stream":true}"#).await;
    assert_eq!(response.status(), 200);
    let client_saw = response.text().await.unwrap();
    assert_eq!(client_saw, SSE_BODY, "the proxy must not alter the stream");

    let turn = await_captured_turn(&harness.ingest).await;
    let captured = BASE64
        .decode(turn["raw_response"].as_str().unwrap())
        .unwrap();

    // The whole point of the raw lane: SSE framing, blank lines, and all.
    assert_eq!(
        String::from_utf8(captured).unwrap(),
        SSE_BODY,
        "raw_response must be the verbatim upstream bytes",
    );
}

#[tokio::test]
async fn a_captured_turn_carries_no_client_side_reduction() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string(SSE_BODY)
            .insert_header("content-type", "text/event-stream"),
    )
    .await;

    let _ = post_through_proxy(harness.proxy, r#"{"model":"claude"}"#)
        .await
        .text()
        .await;

    let turn = await_captured_turn(&harness.ingest).await;
    // Raw-only from birth. A non-null `response` here would mean tapesctl had
    // grown a reducer, which is precisely the drift this client must not have.
    assert!(
        turn["response"].is_null(),
        "tapesctl must never send a reduction: {turn}",
    );
    assert_eq!(turn["provider"], "anthropic");
}

#[tokio::test]
async fn the_request_body_reaches_ingest_verbatim() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
    )
    .await;

    // Deliberately un-alphabetical: a round-trip through a JSON value would
    // reorder these and change the bytes the server hashes.
    let body = r#"{"zeta":1,"alpha":{"nested":[1,2,3]}}"#;
    let _ = post_through_proxy(harness.proxy, body).await.text().await;

    // Asserted against the raw posted bytes on purpose: parsing into a
    // `serde_json::Value` sorts object keys, which would make this test pass
    // even if the client had reordered the body.
    let posted = String::from_utf8(await_captured_turn_bytes(&harness.ingest).await).unwrap();
    assert!(
        posted.contains(&format!(r#""request":{body}"#)),
        "the request body must be embedded verbatim, key order included: {posted}",
    );
}

#[tokio::test]
async fn a_turn_is_captured_even_when_the_client_never_reads_the_body() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string(SSE_BODY)
            .insert_header("content-type", "text/event-stream"),
    )
    .await;

    // Drop the response without reading it. Whichever finalize path fires —
    // for a short body the stream usually completes first — a turn must still
    // land. The disconnect path specifically is isolated by the unit test
    // `an_abandoned_stream_still_finalizes_so_the_turn_is_captured`, which
    // drops the tee with chunks outstanding; an end-to-end test cannot pin
    // down which path wins the race.
    let response = post_through_proxy(harness.proxy, r#"{"model":"claude"}"#).await;
    drop(response);

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["meta"]["upstream_status"], 200);
}

#[tokio::test]
async fn an_upstream_error_is_still_captured_and_still_forwarded() {
    let harness = start_harness(
        ResponseTemplate::new(429)
            .set_body_string(r#"{"error":"rate_limited"}"#)
            .insert_header("content-type", "application/json"),
    )
    .await;

    let response = post_through_proxy(harness.proxy, r#"{"model":"claude"}"#).await;
    assert_eq!(response.status(), 429, "the status reaches the harness");
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":"rate_limited"}"#
    );

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["meta"]["upstream_status"], 429);
    assert_eq!(turn["meta"]["upstream_status_class"], "4xx");
    // A failed turn is still a turn — dropping it would hide exactly the
    // sessions an operator most wants to look at.
    assert_eq!(
        String::from_utf8(
            BASE64
                .decode(turn["raw_response"].as_str().unwrap())
                .unwrap()
        )
        .unwrap(),
        r#"{"error":"rate_limited"}"#,
    );
}

#[tokio::test]
async fn the_subagent_thread_header_becomes_the_turns_thread_id() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
    )
    .await;

    reqwest::Client::new()
        .post(format!("http://{}/v1/messages", harness.proxy))
        .header("content-type", "application/json")
        .header("user-agent", "claude-cli/2.1.145")
        .header("x-claude-code-agent-id", "agent-42")
        .body(r#"{"model":"claude"}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let turn = await_captured_turn(&harness.ingest).await;
    // Thread attribution is deterministic at capture time precisely because
    // this header is carried rather than recovered downstream.
    assert_eq!(turn["meta"]["thread_id"], "agent-42");
}

#[tokio::test]
async fn a_main_thread_turn_has_no_thread_id_at_all() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
    )
    .await;

    let _ = post_through_proxy(harness.proxy, r#"{"model":"claude"}"#)
        .await
        .text()
        .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert!(
        turn["meta"].get("thread_id").is_none(),
        "an absent thread id must be omitted, not empty: {turn}",
    );
}

#[tokio::test]
async fn the_envelope_is_stamped_on_the_outbound_request() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
    )
    .await;

    let _ = post_through_proxy(harness.proxy, r#"{"model":"claude"}"#)
        .await
        .text()
        .await;

    let seen: Request = harness
        .upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    // No harness is running, so this is the Claude lane's miss — which must
    // still produce a well-formed envelope rather than no envelope at all.
    assert_eq!(
        seen.headers.get("x-tapes-harness-id").unwrap(),
        "unknown",
        "every forwarded request on the claude lane carries the envelope",
    );
}

#[tokio::test]
async fn the_private_marker_header_never_travels_upstream() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
    )
    .await;

    reqwest::Client::new()
        .post(format!("http://{}/v1/messages", harness.proxy))
        .header("content-type", "application/json")
        .header("user-agent", "claude-cli/2.1.145")
        .header("x-tapesctl-codex-attribution", "tapesctl-openai-abc")
        .body(r#"{"model":"claude"}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let seen: Request = harness
        .upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(
        seen.headers.get("x-tapesctl-codex-attribution").is_none(),
        "the marker is this client's private channel to its own proxy",
    );
    // Carrying a marker puts the request on the Codex lane, and an
    // unresolvable Codex session emits no envelope rather than asserting an
    // identity the pipeline declined to assert.
    assert!(
        seen.headers.get("x-tapes-harness-id").is_none(),
        "an undecided codex turn must not be stamped with a harness id",
    );
}

#[tokio::test]
async fn a_non_json_request_body_is_forwarded_but_not_captured() {
    let harness = start_harness(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
    )
    .await;

    // Capture degrades; forwarding does not.
    let response = post_through_proxy(harness.proxy, "not json at all").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let posted = harness.ingest.received_requests().await.unwrap_or_default();
    assert!(
        posted.is_empty(),
        "a turn that cannot be described must be skipped, not sent malformed",
    );
}
