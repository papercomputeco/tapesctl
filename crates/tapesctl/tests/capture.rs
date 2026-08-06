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
use tapesctl::transcript::tailer::SessionTracker;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The exact bytes a streaming Anthropic turn puts on the wire, framing and
/// all. The capture must reproduce these verbatim.
const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// The per-launch capture secret these proxies are configured with. A real
/// `start` generates a fresh UUID per launch; the fixed value here is what the
/// tests below present (or deliberately withhold) as the echo.
const TEST_NONCE: &str = "test-launch-nonce-1234";

struct Harness {
    proxy: SocketAddr,
    ingest: MockServer,
    upstream: MockServer,
}

async fn start_harness(response: ResponseTemplate) -> Harness {
    start_harness_as(response, false, own_pid()).await
}

/// This process's PID, as the launched harness's.
///
/// The requests in these tests are issued by the test process itself, so naming
/// it as the launched harness is what makes the peer check pass — the same
/// relationship a real `start pi` has with the process it spawned.
fn own_pid() -> i32 {
    i32::try_from(std::process::id()).unwrap()
}

/// As [`start_harness`], but for a harness that stamps its own envelope — the
/// shape `tapesctl start pi` runs in — and with an explicit launched PID, which
/// is what decides whether an inbound envelope is believed.
async fn start_harness_as(
    response: ResponseTemplate,
    self_attributing: bool,
    launched_pid: i32,
) -> Harness {
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
        self_attributing,
        launched_pid: Arc::new(std::sync::atomic::AtomicI32::new(launched_pid)),
        gateway_nonce: Arc::new(TEST_NONCE.to_owned()),
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
    // Carrying a marker puts the request on the Codex lane. An unresolvable
    // session no longer suppresses the envelope: a Codex request is
    // self-describing, so the turn is stamped with what IS known — that it is
    // codex — and left without a session id, which is what lets a later
    // repair pass find it. Asserting a session id absent is the half that
    // matters: a miss must never invent one.
    assert_eq!(
        seen.headers.get("x-tapes-harness-id").unwrap(),
        "codex",
        "a codex turn is known to be codex even when its session is not",
    );
    assert!(
        seen.headers.get("x-tapes-harness-session-id").is_none(),
        "an unresolved codex turn must not assert a session id",
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

/// A request shaped like one pi's gateway extension makes: its own envelope,
/// stamped from inside the harness, the echoed launch nonce, and a User-Agent
/// no lane here claims. `nonce` is a parameter because withholding or
/// falsifying the echo is exactly what several tests below do.
async fn post_as_self_attributing_harness_with_nonce(
    proxy: SocketAddr,
    session_id: Option<&str>,
    nonce: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .header("user-agent", "pi/0.1")
        .header("x-tapes-harness-id", "pi");
    if let Some(session_id) = session_id {
        request = request.header("x-tapes-harness-session-id", session_id);
    }
    if let Some(nonce) = nonce {
        request = request.header("x-tapes-gateway-nonce", nonce);
    }
    request
        .body(r#"{"model":"claude-sonnet-4"}"#)
        .send()
        .await
        .unwrap()
}

/// The well-behaved shape: envelope plus the correct nonce echo.
async fn post_as_self_attributing_harness(
    proxy: SocketAddr,
    session_id: Option<&str>,
) -> reqwest::Response {
    post_as_self_attributing_harness_with_nonce(proxy, session_id, Some(TEST_NONCE)).await
}

#[tokio::test]
async fn a_self_attributing_harness_is_filed_under_the_session_it_named() {
    // The claim `tapesctl start pi` rests on. Nothing outside pi can discover
    // this session — there is no PID-indexed session file to read — so if the
    // proxy did not carry the inbound envelope into the payload, every pi turn
    // would land under `unknown` and no session would ever appear.
    //
    // This is also the acceptance half of the nonce contract: the request
    // carries the correct echo and comes from the launched harness's subtree,
    // and only that combination is believed.
    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        true,
        own_pid(),
    )
    .await;

    let _ = post_as_self_attributing_harness(harness.proxy, Some("pi-session-7"))
        .await
        .text()
        .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["session"]["harness_id"], "pi");
    assert_eq!(turn["session"]["harness_session_id"], "pi-session-7");
}

#[tokio::test]
async fn a_self_attributing_harness_with_a_partial_envelope_falls_back_to_unknown() {
    // A harness id with no session id is not something to group turns under.
    // The turn is still captured — a turn filed under `unknown` is recoverable,
    // a dropped one is not.
    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        true,
        own_pid(),
    )
    .await;

    let _ = post_as_self_attributing_harness(harness.proxy, None)
        .await
        .text()
        .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["session"]["harness_id"], "unknown");
    assert!(turn["session"]["harness_session_id"].is_null());
}

#[tokio::test]
async fn a_redirected_capture_does_not_take_session_identity_from_the_request() {
    // The lane is chosen by what was launched, not by what a request claims.
    // A capture of a redirected harness attributes from the outside, and an
    // inbound envelope on that lane is someone else's traffic — trusting it
    // would let any process on the loopback file turns under any session it
    // liked.
    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        false,
        own_pid(),
    )
    .await;

    let _ = post_as_self_attributing_harness(harness.proxy, Some("pi-session-7"))
        .await
        .text()
        .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["session"]["harness_id"], "unknown");
}

#[tokio::test]
async fn a_process_outside_the_launched_harness_cannot_claim_a_session() {
    // The forgery this proxy has to refuse. A loopback listener is reachable by
    // every process on the machine, so "the launched harness attributes itself"
    // cannot be the whole test — otherwise two headers from any local process
    // would persist a turn, and print a session link, under a session it chose.
    //
    // The launched harness here is a child process, which makes this test
    // process its *parent* and therefore not part of its subtree — exactly the
    // relationship an unrelated local process has with a real pi.
    let mut impostor_target = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("sleep is available");
    let launched = i32::try_from(impostor_target.id()).unwrap();

    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        true,
        launched,
    )
    .await;

    let _ = post_as_self_attributing_harness(harness.proxy, Some("victim-session"))
        .await
        .text()
        .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(
        turn["session"]["harness_id"], "unknown",
        "an envelope from outside the launched harness must not be believed",
    );
    assert_ne!(turn["session"]["harness_session_id"], "victim-session");

    let _ = impostor_target.kill();
    let _ = impostor_target.wait();
}

#[tokio::test]
async fn a_forged_envelope_with_correct_ancestry_but_no_nonce_is_refused() {
    // The forgery the ancestry check alone cannot refuse. A command the
    // harness runs in a shell tool is a descendant of the launched PID, so its
    // socket walks up to the harness exactly like the extension's does — here
    // the test process itself plays that descendant, with a peer check that
    // genuinely passes. What it does not have is the echo of the launch nonce,
    // and without it the envelope must not be believed.
    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        true,
        own_pid(),
    )
    .await;

    let _ =
        post_as_self_attributing_harness_with_nonce(harness.proxy, Some("stolen-session"), None)
            .await
            .text()
            .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(
        turn["session"]["harness_id"], "unknown",
        "ancestry alone must not be enough to have an envelope believed",
    );
    assert_ne!(turn["session"]["harness_session_id"], "stolen-session");
}

#[tokio::test]
async fn a_forged_envelope_with_correct_ancestry_but_a_wrong_nonce_is_refused() {
    // As above, but guessing rather than omitting: a wrong value must fail the
    // same way a missing one does.
    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        true,
        own_pid(),
    )
    .await;

    let _ = post_as_self_attributing_harness_with_nonce(
        harness.proxy,
        Some("stolen-session"),
        Some("not-the-launch-nonce"),
    )
    .await
    .text()
    .await;

    let turn = await_captured_turn(&harness.ingest).await;
    assert_eq!(turn["session"]["harness_id"], "unknown");
    assert_ne!(turn["session"]["harness_session_id"], "stolen-session");
}

#[tokio::test]
async fn the_nonce_echo_never_travels_upstream() {
    // The nonce is the secret that authenticates a session's envelope, and the
    // upstream is outside the trust boundary — a forwarded echo would hand the
    // value to every hop past this proxy and could land in recorded traffic.
    let harness = start_harness_as(
        ResponseTemplate::new(200)
            .set_body_string("ok")
            .insert_header("content-type", "application/json"),
        true,
        own_pid(),
    )
    .await;

    let _ = post_as_self_attributing_harness(harness.proxy, Some("pi-session-7"))
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
    assert!(
        seen.headers.get("x-tapes-gateway-nonce").is_none(),
        "the nonce echo is a secret between the harness and its own proxy",
    );
    // And the strip must not have cost the envelope: this was a believed
    // request, so its identity still travels.
    assert_eq!(seen.headers.get("x-tapes-harness-id").unwrap(), "pi");
}
