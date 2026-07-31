//! The just-in-time capture proxy.
//!
//! A dumb byte forwarder that happens to keep a copy. It does not parse
//! request or response bodies, does not hash them, and does not reduce them —
//! it moves bytes upstream unchanged, streams the reply back unchanged, and
//! hands a verbatim copy of the response to the ingest client. Everything that
//! *interprets* those bytes lives server-side.
//!
//! # The rules that keep a stream a stream
//!
//! Streaming is where a capture proxy quietly breaks a harness, and each of
//! these was verified against paperd's forwarding path:
//!
//! * **No transparent decompression.** `reqwest` is built with no compression
//!   feature, so a `Content-Encoding: gzip` response arrives — and is
//!   forwarded — still encoded. See the dependency comment in the workspace
//!   manifest.
//! * **Framing headers are stripped on re-stream.** `Content-Length` and
//!   `Transfer-Encoding` describe the *upstream's* framing; the body is being
//!   re-framed by this server, so both are dropped and hyper recomputes them.
//!   Copying them through produces a response whose declared length disagrees
//!   with its body, which stalls a client mid-stream.
//! * **The tee never blocks the hot path.** [`ResponseTee::poll_next`] does one
//!   non-blocking send into an unbounded channel and returns the chunk
//!   immediately. Doing capture work inline — hashing, buffering with
//!   backpressure, posting — would add its latency to every SSE token the user
//!   is watching arrive.
//! * **Finalize happens exactly once.** A turn ends by clean EOF, by upstream
//!   error, or by the client hanging up, and all three must produce one capture.
//!   The sender is an `Option` that finalizing takes, so the `Drop` path is a
//!   no-op after `poll_next` already finished.
//!
//! # Capture degrades; forwarding does not
//!
//! Whenever capture cannot be done correctly — an oversize request body, a
//! response past the raw cap, a request body that is not JSON — the turn is
//! dropped from capture with a warning and the proxy keeps forwarding. The
//! harness must never fail because telemetry could not be recorded.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    response::Response,
};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::BodyDataStream;
use serde_json::value::RawValue;
use snafu::ResultExt;
use tapes_harnesses::attribution::{AttributionConfig, AttributionState, RequestFacts, attribute};
use tapes_harnesses::envelope;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tracing::{debug, warn};
use url::Url;

use super::ingest::{
    IngestClient, SessionEnvelope, TurnMeta, TurnPayload, encode_raw_response, status_class,
};
use super::peek::BoundedPeek;
use crate::error::{Result, error};

/// How much of a request body is buffered so the turn can be described to
/// ingest. Beyond this the turn is forwarded but not captured.
pub const REQUEST_PEEK_BYTES: usize = 4 * 1024 * 1024;

/// How many response bytes are retained for `raw_response`. Matches ingest's
/// own per-turn raw cap: retaining more only to have the server drop it wastes
/// memory on the hot path.
pub const RAW_RESPONSE_CAP: usize = 8 * 1024 * 1024;

/// Everything a request handler needs, shared across connections.
#[derive(Clone)]
pub struct ProxyState {
    /// Where forwarded traffic goes.
    pub upstream: Url,
    /// Where captured turns go.
    pub ingest: IngestClient,
    /// Watcher snapshots and the fork-parent cache.
    pub attribution: Arc<AttributionState>,
    /// Timeouts and this client's Codex provider id.
    pub attribution_config: Arc<AttributionConfig>,
    /// Provider family for the harness being captured (`anthropic`/`openai`).
    pub provider: &'static str,
    /// Name of this client's Codex attribution marker header.
    pub codex_marker_header: Arc<String>,
    /// True when the launched harness is Codex, which selects the Codex lane.
    pub codex_lane: bool,
    /// Org id stamped on every captured turn.
    pub org_id: Arc<String>,
    /// Acting subject stamped on every captured turn.
    pub auth_subject: Arc<String>,
    /// Notified with the harness session id the first time one resolves, so the
    /// caller can print a session URL.
    pub session_seen: Arc<tokio::sync::Mutex<Option<UnboundedSender<String>>>>,
}

/// Axum fallback handler — every method and path forwards through here.
pub async fn forward_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ProxyState>,
    req: Request,
) -> Response {
    match try_forward(state, peer, req).await {
        Ok(response) => response,
        Err(err) => {
            warn!(error = %err, "forwarding failed");
            let mut response = Response::new(Body::from(format!("tapesctl proxy: {err}")));
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            response
        }
    }
}

async fn try_forward(state: ProxyState, peer: SocketAddr, req: Request) -> Result<Response> {
    let started = Instant::now();
    let (parts, body) = req.into_parts();

    // Peek consumes the body and returns a Replay that re-emits the prefix in
    // front of the rest. The two halves are not separable, so forwarding is
    // byte-for-byte regardless of what capture decides to do.
    let (peeked, replay) = BoundedPeek::new(body, REQUEST_PEEK_BYTES).peek().await?;

    let mut out_headers = parts.headers.clone();
    strip_hop_by_hop(&mut out_headers);

    // The marker header is this client's private channel to its own proxy. Read
    // it, then remove it — upstream has no business seeing it.
    let marker = parts
        .headers
        .get(state.codex_marker_header.as_str())
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    out_headers.remove(state.codex_marker_header.as_str());

    // Attribution comes from the shared crate, so a tapesctl turn and a paperd
    // turn resolve identity through the same code.
    let attributed = attribute(
        &state.attribution,
        &state.attribution_config,
        RequestFacts {
            peer: Some(peer),
            user_agent: parts
                .headers
                .get(http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            codex_marker: marker.as_deref(),
            codex_route: state.codex_lane,
        },
    )
    .await;

    let envelope_attribution = attributed.envelope();
    let session = envelope_attribution
        .as_ref()
        .map(|a| SessionEnvelope::from_attribution(a, &state.org_id, &state.auth_subject));
    let session_id = envelope_attribution
        .as_ref()
        .and_then(|a| a.session_id.clone());

    // Stamp the envelope outbound too. This proxy posts its own turns, so it
    // does not need the headers for itself — but when the upstream is itself a
    // capture-aware gateway, an unstamped request would be attributed
    // differently there than here.
    //
    // `stamp` rather than injecting the value above: it also honours the rule
    // that a harness which stamped its own complete envelope keeps it, which a
    // hand-rolled inject would silently overwrite.
    attributed
        .stamp(&mut out_headers)
        .context(error::EnvelopeSnafu)?;
    if let Some(session_id) = session_id.as_deref() {
        announce_session(&state, session_id).await;
    }

    let thread_id = envelope::thread_id(&parts.headers).map(str::to_owned);
    let url = build_upstream_url(&state.upstream, parts.uri.path(), parts.uri.query())?;
    debug!(method = %parts.method, url = %url, "forwarding");

    let method = parts.method.clone();
    let path = parts.uri.path().to_owned();
    let response = reqwest::Client::new()
        .request(method.clone(), url)
        .headers(out_headers)
        .body(reqwest::Body::wrap_stream(BodyDataStream::new(replay)))
        .send()
        .await
        .context(error::UpstreamSnafu)?;

    let status = response.status();
    let upstream_headers = response.headers().clone();

    // Everything the capture needs is known now, at response-header time. The
    // body has not been read yet — it is teed as it streams.
    let capture = TurnCapture {
        state: state.clone(),
        request_body: peeked.whole_body().cloned(),
        session,
        meta: TurnMeta {
            request_id: uuid::Uuid::new_v4().to_string(),
            thread_id,
            method: method.to_string(),
            path,
            content_type: header_string(&upstream_headers, http::header::CONTENT_TYPE),
            content_encoding: header_string(&upstream_headers, http::header::CONTENT_ENCODING),
            stream: is_event_stream(&upstream_headers).then(|| "true".to_owned()),
            upstream_status: status.as_u16(),
            upstream_status_class: status_class(status.as_u16()),
            request_bytes: peeked.prefix.len(),
            response_bytes: 0,
            elapsed_seconds: 0.0,
        },
        started,
    };

    let (tx, rx) = unbounded_channel();
    tokio::spawn(capture.run(rx));

    let stream = ResponseTee::new(response.bytes_stream(), tx);
    Ok(build_response(
        status,
        &upstream_headers,
        Body::from_stream(stream),
    ))
}

async fn announce_session(state: &ProxyState, session_id: &str) {
    let mut slot = state.session_seen.lock().await;
    // Take the sender so only the first resolved session is announced; later
    // turns in the same session must not reprint the URL.
    if let Some(tx) = slot.take() {
        let _ = tx.send(session_id.to_owned());
    }
}

fn header_string(headers: &HeaderMap, name: http::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

/// Remove headers scoped to a single connection before re-sending upstream.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let doomed: Vec<_> = headers
        .keys()
        .filter(|name| envelope::is_hop_by_hop(name.as_str()))
        .cloned()
        .collect();
    for name in doomed {
        headers.remove(name);
    }
    // Host names the proxy's own listener; leaving it would send the upstream a
    // Host it does not serve.
    headers.remove(http::header::HOST);
}

/// Join a forwarded path onto the upstream base, preserving any base path.
///
/// The base may itself carry a route prefix (a Paper-managed gateway route, for
/// instance), so this concatenates rather than replaces — `Url::join` would
/// discard the base path for any absolute request path.
pub fn build_upstream_url(base: &Url, path: &str, query: Option<&str>) -> Result<Url> {
    let base_path = base.path().trim_end_matches('/');
    let mut url = base.clone();
    url.set_path(&format!("{base_path}{path}"));
    url.set_query(query);
    Ok(url)
}

/// Copy the upstream response headers onto our own, minus what must not travel.
fn build_response(status: StatusCode, upstream: &HeaderMap, body: Body) -> Response {
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.iter() {
        let key = name.as_str();
        if envelope::is_hop_by_hop(key) {
            continue;
        }
        // Both describe the upstream's framing of a body this server is
        // re-framing. Keeping them produces a reply whose declared length
        // disagrees with what is actually sent.
        if key.eq_ignore_ascii_case("content-length") {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        // Only reachable via a malformed status, which cannot occur here; an
        // empty body beats panicking inside the request path.
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// What the tee sends to the capture task.
#[derive(Debug)]
enum TeeEvent {
    Chunk(Bytes),
    /// The turn ended; the payload names which path ended it, for logs.
    Finish(&'static str),
}

/// Wraps the upstream byte stream, copying each chunk to the capture task.
struct ResponseTee<S> {
    inner: std::pin::Pin<Box<S>>,
    /// `None` once finalized. Taking it is what makes finalize-once true.
    tx: Option<UnboundedSender<TeeEvent>>,
}

impl<S> ResponseTee<S> {
    fn new(stream: S, tx: UnboundedSender<TeeEvent>) -> Self {
        Self {
            inner: Box::pin(stream),
            tx: Some(tx),
        }
    }

    fn finalize(&mut self, reason: &'static str) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(TeeEvent::Finish(reason));
        }
    }
}

impl<S> futures_util::Stream for ResponseTee<S>
where
    S: futures_util::Stream<Item = std::result::Result<Bytes, reqwest::Error>>,
{
    type Item = std::result::Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                // One non-blocking send, then straight back to the consumer.
                // A failed send means the capture task is gone, which must not
                // interrupt the stream the user is reading.
                if let Some(tx) = self.tx.as_ref() {
                    let _ = tx.send(TeeEvent::Chunk(chunk.clone()));
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.finalize("stream_error");
                Poll::Ready(Some(Err(std::io::Error::other(err.to_string()))))
            }
            Poll::Ready(None) => {
                self.finalize("stream_complete");
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for ResponseTee<S> {
    fn drop(&mut self) {
        // Reached when the consumer drops the body before exhausting it —
        // usually the harness disconnecting mid-stream. A no-op if `poll_next`
        // already finalized, which is the point of taking the sender.
        self.finalize("client_disconnect");
    }
}

/// Accumulates teed response bytes under a hard cap.
///
/// On overflow the buffer is released rather than kept: the turn cannot be
/// captured anyway, and holding megabytes that will never be posted is exactly
/// the memory pin the cap exists to prevent.
#[derive(Default)]
struct RawBuffer {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl RawBuffer {
    fn push(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.bytes.len() + chunk.len() > RAW_RESPONSE_CAP {
            self.overflowed = true;
            self.bytes = Vec::new();
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }
}

/// Assembles and posts one turn, off the forwarding path.
struct TurnCapture {
    state: ProxyState,
    /// `None` when the request body exceeded the peek cap, which makes the turn
    /// undescribable and so uncapturable.
    request_body: Option<Bytes>,
    session: Option<SessionEnvelope>,
    meta: TurnMeta,
    started: Instant,
}

impl TurnCapture {
    async fn run(mut self, mut rx: tokio::sync::mpsc::UnboundedReceiver<TeeEvent>) {
        let mut buffer = RawBuffer::default();
        let mut reason = "channel_closed";

        while let Some(event) = rx.recv().await {
            match event {
                TeeEvent::Chunk(chunk) => buffer.push(&chunk),
                TeeEvent::Finish(why) => {
                    reason = why;
                    break;
                }
            }
        }

        let raw = buffer.bytes;
        self.meta.response_bytes = raw.len();
        self.meta.elapsed_seconds = self.started.elapsed().as_secs_f64();

        if buffer.overflowed {
            warn!(
                cap_bytes = RAW_RESPONSE_CAP,
                request_id = %self.meta.request_id,
                "response exceeded the raw cap; turn forwarded but not captured",
            );
            return;
        }
        let Some(body) = self.request_body.as_ref() else {
            warn!(
                peek_bytes = REQUEST_PEEK_BYTES,
                request_id = %self.meta.request_id,
                "request body exceeded the peek cap; turn forwarded but not captured",
            );
            return;
        };
        // `RawValue` requires valid JSON. Validating here is not parsing for
        // meaning — nothing reads a field — it is the minimum needed to embed
        // the bytes verbatim in a JSON document.
        let request = match RawValue::from_string(String::from_utf8_lossy(body).into_owned()) {
            Ok(request) => request,
            Err(err) => {
                warn!(
                    error = %err,
                    request_id = %self.meta.request_id,
                    "request body is not JSON; turn forwarded but not captured",
                );
                return;
            }
        };

        let payload = TurnPayload {
            provider: self.state.provider,
            request: &request,
            response: (),
            raw_response: (!raw.is_empty()).then(|| encode_raw_response(&raw)),
            raw_response_encoding: self.meta.content_encoding.clone(),
            meta: self.meta.clone(),
            session: self.session.clone(),
        };

        match self.state.ingest.post_turn(&payload).await {
            Ok(()) => debug!(
                request_id = %self.meta.request_id,
                finalized_by = reason,
                bytes = raw.len(),
                "turn captured",
            ),
            Err(err) => warn!(
                error = %err,
                request_id = %self.meta.request_id,
                "ingest rejected the turn",
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn a_bare_upstream_takes_the_request_path_verbatim() {
        let got =
            build_upstream_url(&url("https://api.anthropic.com"), "/v1/messages", None).unwrap();
        assert_eq!(got.as_str(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn an_upstream_route_prefix_is_preserved_not_replaced() {
        // `Url::join` would discard `/local-gw/anthropic` for an absolute
        // request path, silently sending traffic to the wrong route.
        let got = build_upstream_url(
            &url("http://127.0.0.1:18832/local-gw/anthropic"),
            "/v1/messages",
            None,
        )
        .unwrap();
        assert_eq!(
            got.as_str(),
            "http://127.0.0.1:18832/local-gw/anthropic/v1/messages",
        );
    }

    #[test]
    fn a_trailing_slash_on_the_upstream_does_not_double_up() {
        let got =
            build_upstream_url(&url("http://127.0.0.1:1/base/"), "/v1/messages", None).unwrap();
        assert_eq!(got.as_str(), "http://127.0.0.1:1/base/v1/messages");
    }

    #[test]
    fn the_query_string_rides_along() {
        let got = build_upstream_url(
            &url("http://127.0.0.1:1"),
            "/v1/messages",
            Some("beta=true"),
        )
        .unwrap();
        assert_eq!(got.as_str(), "http://127.0.0.1:1/v1/messages?beta=true");
    }

    #[test]
    fn framing_headers_do_not_survive_the_re_stream() {
        // The body is re-framed by this server; forwarding the upstream's
        // Content-Length declares a length that will not match what is sent.
        let mut upstream = HeaderMap::new();
        upstream.insert(http::header::CONTENT_LENGTH, "1234".parse().unwrap());
        upstream.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        upstream.insert(
            http::header::CONTENT_TYPE,
            "text/event-stream".parse().unwrap(),
        );

        let response = build_response(StatusCode::OK, &upstream, Body::empty());

        assert!(
            !response
                .headers()
                .contains_key(http::header::CONTENT_LENGTH)
        );
        assert!(
            !response
                .headers()
                .contains_key(http::header::TRANSFER_ENCODING),
            "Transfer-Encoding is hop-by-hop and must not be copied",
        );
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "text/event-stream",
            "content headers the client needs are preserved",
        );
    }

    #[test]
    fn hop_by_hop_request_headers_are_stripped_but_content_headers_stay() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONNECTION, "keep-alive".parse().unwrap());
        headers.insert(http::header::HOST, "127.0.0.1:9999".parse().unwrap());
        headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        headers.insert(http::header::ACCEPT_ENCODING, "gzip".parse().unwrap());

        strip_hop_by_hop(&mut headers);

        assert!(!headers.contains_key(http::header::CONNECTION));
        assert!(
            !headers.contains_key(http::header::HOST),
            "Host names our listener, not the upstream",
        );
        assert!(headers.contains_key(http::header::CONTENT_TYPE));
        assert!(
            headers.contains_key(http::header::ACCEPT_ENCODING),
            "the harness's encoding preference is forwarded untouched",
        );
    }

    #[test]
    fn an_event_stream_content_type_marks_the_turn_as_streaming() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        assert!(is_event_stream(&headers));

        headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        assert!(!is_event_stream(&headers));
    }

    // --- the tee ---------------------------------------------------------

    fn tee_of(
        chunks: Vec<&'static [u8]>,
    ) -> (
        ResponseTee<impl futures_util::Stream<Item = std::result::Result<Bytes, reqwest::Error>>>,
        tokio::sync::mpsc::UnboundedReceiver<TeeEvent>,
    ) {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, reqwest::Error>(Bytes::from_static(c))),
        );
        let (tx, rx) = unbounded_channel();
        (ResponseTee::new(stream, tx), rx)
    }

    fn drain_events(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<TeeEvent>,
    ) -> (Vec<u8>, Vec<String>) {
        let mut bytes = Vec::new();
        let mut finishes = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                TeeEvent::Chunk(c) => bytes.extend_from_slice(&c),
                TeeEvent::Finish(why) => finishes.push(why.to_owned()),
            }
        }
        (bytes, finishes)
    }

    #[tokio::test]
    async fn every_forwarded_byte_is_teed_and_the_stream_is_unchanged() {
        use futures_util::StreamExt;

        let (tee, mut rx) = tee_of(vec![b"event: a\n", b"data: 1\n\n"]);
        let forwarded: Vec<u8> = tee
            .map(|chunk| chunk.unwrap())
            .fold(Vec::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(&chunk);
                acc
            })
            .await;

        let (teed, finishes) = drain_events(&mut rx);
        assert_eq!(forwarded, b"event: a\ndata: 1\n\n");
        assert_eq!(
            teed, forwarded,
            "the tee must see exactly what the client saw"
        );
        assert_eq!(finishes, vec!["stream_complete".to_owned()]);
    }

    #[tokio::test]
    async fn a_completed_stream_finalizes_once_even_after_being_dropped() {
        use futures_util::StreamExt;

        let (mut tee, mut rx) = tee_of(vec![b"x"]);
        while tee.next().await.is_some() {}
        drop(tee);

        let (_, finishes) = drain_events(&mut rx);
        // Drop also calls finalize; taking the sender is what keeps it to one.
        assert_eq!(finishes, vec!["stream_complete".to_owned()]);
    }

    #[tokio::test]
    async fn an_abandoned_stream_still_finalizes_so_the_turn_is_captured() {
        use futures_util::StreamExt;

        let (mut tee, mut rx) = tee_of(vec![b"a", b"b", b"c"]);
        // Read one chunk, then hang up — the harness disconnecting mid-stream.
        let _ = tee.next().await;
        drop(tee);

        let (teed, finishes) = drain_events(&mut rx);
        assert_eq!(teed, b"a");
        assert_eq!(finishes, vec!["client_disconnect".to_owned()]);
    }
}
