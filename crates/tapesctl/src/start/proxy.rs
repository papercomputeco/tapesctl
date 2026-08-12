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
//!   manifest. The request body is likewise forwarded exactly as it arrived;
//!   the decode in [`super::content_encoding`] runs on the capture copy only
//!   and cannot alter a byte of what goes upstream.
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
//! # Not every exchange is a turn
//!
//! Two gates run before any of that, in [`super::turn_policy`]: the request has
//! to be a turn-shaped request on a provider chat-completion endpoint, and the
//! upstream has to have completed the exchange. Both are decidable at
//! response-header time, so an exchange that fails either is forwarded without
//! ever being buffered or teed.
//!
//! These are shared capture policy rather than this client's preference — the
//! gateway route applies the same two rules, and both are specified as data in
//! a corpus both implementations run. A refusal is reported with its reason
//! (and, for an upstream failure, its status): the diagnostic value of a failed
//! call is in saying so, not in filing it as a conversation.
//!
//! # Refusing to forward is not a drop
//!
//! One case stops a request before it is sent at all: a harness whose extension
//! registers several providers labels each request with the provider it is for,
//! and a label this capture has no upstream for cannot be forwarded anywhere.
//! Sending it to the launch's upstream is the defect being fixed, not a
//! fallback — that upstream speaks another provider's API and answers with a
//! 404 the harness reports as a model failure.
//!
//! That refusal is reported like a drop and classified as neither: the gates
//! above decide whether an exchange that *happened* is a turn, and this one
//! decides whether the exchange happens. See
//! [`ROUTE_REFUSAL_UNROUTABLE_PROVIDER`] for why it does not borrow the shared
//! drop vocabulary.
//!
//! # Capture degrades; forwarding does not
//!
//! Whenever capture cannot be done correctly — an oversize request body, a
//! response past the raw cap, a request body this build cannot decode, a
//! request body that is not JSON — the turn is dropped from capture with a
//! warning and the proxy keeps forwarding. The harness must never fail because
//! telemetry could not be recorded.
//!
//! Those last two are separate warnings on purpose. Reporting an undecodable
//! encoding as "not JSON" describes the symptom of a body nobody tried to
//! decode, and a reader who believes it goes looking at the harness's payload
//! instead of at this proxy's decoder — which is how zstd request bodies stayed
//! silently uncaptured (PCC-1126).
//!
//! Bodiless requests are the exception, and they are logged at debug rather
//! than warn: a GET has nothing to capture by construction, so reporting it as
//! a degradation would bury the cases above in noise.

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
use tapes_capture::envelope;
use tapes_capture::peer_trust;
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexRequestIdentity, RequestFacts, attribute,
};
use tapes_harnesses::plugin::{GATEWAY_NONCE_HEADER, nonce_matches, split_provider_route};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tracing::{debug, warn};
use url::Url;

use super::ProviderRoutes;
use super::content_encoding::decode_content_encoding;
use super::ingest::{
    IngestClient, SessionEnvelope, TurnMeta, TurnPayload, encode_raw_response, status_class,
};
use super::peek::BoundedPeek;
use super::turn_policy;
use crate::error::{Result, error};
use crate::transcript::tailer::SessionTracker;

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
    ///
    /// The whole answer only when one upstream serves the capture. Under
    /// [`Self::provider_routes`] it is the fallback for a request that carried
    /// no label; a labelled one files under its own route's provider.
    pub provider: &'static str,
    /// The upstreams this proxy routes labelled requests to, when the launched
    /// extension was told to label them.
    ///
    /// `Some` for a harness whose extension registers several providers at
    /// once, and only when this launch left the upstream to default — the same
    /// condition that put the switch in the launched environment, so the proxy
    /// expects labels exactly when the extension was told to send them. `None`
    /// is the single-upstream capture, where [`Self::upstream`] is the whole
    /// answer.
    pub provider_routes: Option<Arc<ProviderRoutes>>,
    /// Name of this client's Codex attribution marker header.
    pub codex_marker_header: Arc<String>,
    /// True when the launched harness is Codex, which selects the Codex lane.
    pub codex_lane: bool,
    /// True when the launched harness stamps its own `X-Tapes-*` envelope, so
    /// the request's headers — not this process's peer-PID lookup — are the
    /// authority on which session a turn belongs to.
    ///
    /// Necessary but not sufficient: an envelope is believed only when the peer
    /// also proves to be that harness. See [`launched_pid`](Self::launched_pid).
    pub self_attributing: bool,
    /// PID of the launched harness, or [`super::NO_LAUNCHED_PID`] before it has
    /// been spawned.
    ///
    /// Shared rather than copied because the listener is open before the harness
    /// exists: the proxy is built first, the PID is filled in at spawn.
    pub launched_pid: Arc<std::sync::atomic::AtomicI32>,
    /// The per-launch capture secret a self-attributing harness must echo in
    /// [`GATEWAY_NONCE_HEADER`] before its envelope is believed.
    ///
    /// The peer-PID ancestry check cannot tell the launched harness apart from
    /// the harness's own subprocesses — a shell tool's child is a descendant
    /// too — so possession of this value is the second required proof. The
    /// value is compared, stripped from the outbound request, and never logged;
    /// it must not appear in captured or forwarded bytes anywhere.
    pub gateway_nonce: Arc<String>,
    /// Org id stamped on every captured turn.
    pub org_id: Arc<String>,
    /// Acting subject stamped on every captured turn.
    pub auth_subject: Arc<String>,
    /// Desktop sessions an authenticated lifecycle report introduced, when
    /// this proxy is capturing a harness attributed that way.
    ///
    /// `None` on every `start` proxy: a launched harness names its session
    /// through the pipeline or through its own envelope, and a registry here
    /// would be a third answer nothing populates. `Some` only under
    /// `tapesctl capture`, which is also the only command that serves the
    /// route that fills it.
    pub desktop_sessions: Option<Arc<crate::codex_app::lifecycle::DesktopSessions>>,
    /// Sessions whose transcripts this process is responsible for.
    ///
    /// An attributed request is the proof that a session's traffic flows
    /// through this proxy, which is exactly the transcript tailer's scope rule —
    /// so the registry is fed from here rather than from a separate discovery
    /// pass that could disagree with what was actually captured.
    pub transcript_tracker: SessionTracker,
    /// How much this launch captured, how much of it was attributed, and which
    /// session the exit summary may link to.
    ///
    /// Written by every capture task and read once after shutdown has drained
    /// them, which is what lets the exit summary distinguish "captured nothing"
    /// from "captured turns and could not say whose they were" — and keeps it
    /// from claiming either while a capture is still running.
    pub tally: Arc<crate::start::tally::CaptureTally>,
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
            report_forward_failure(&err);
            let mut response = Response::new(Body::from(format!("tapesctl proxy: {err}")));
            *response.status_mut() = refusal_status(&err);
            response
        }
    }
}

/// The label a routing refusal is greppable by.
///
/// Deliberately **not** a `drop_reason`, and the field name says so. A drop
/// reason answers "this exchange happened and was not captured"; the shared
/// corpus that specifies them defines every one of its reasons over an exchange
/// with a status, and [`turn_policy::capture_refusal`] takes one. There is no
/// status here and never was: nothing was sent. Reusing the drop vocabulary
/// would put a request that never reached a provider into the same counter as
/// turns a provider answered, and `unknown_provider` in particular would be a
/// lie with a wrong remedy attached — it means "no reducer claims this shape,
/// add one", where the actual fault is that this capture has no upstream for
/// the provider at all.
///
/// So it is its own vocabulary, one entry wide, sharing the *shape* of a drop
/// report — a stable machine-readable field, plus the identifier that names the
/// fault — without borrowing its meaning.
pub const ROUTE_REFUSAL_UNROUTABLE_PROVIDER: &str = "unroutable_provider";

/// Say why a forward did not happen.
///
/// Warned in both cases, and for the same reason [`report_refusal`] warns on an
/// upstream failure: a call the reader expected to see recorded did not happen.
/// A routing refusal is the stronger version of that — the exchange did not
/// merely go uncaptured, it did not occur — so it names the provider and stays
/// greppable rather than being folded into the generic line.
fn report_forward_failure(err: &crate::error::Error) {
    match err {
        crate::error::Error::UnroutableProvider { provider, known } => warn!(
            route_refusal = ROUTE_REFUSAL_UNROUTABLE_PROVIDER,
            provider = provider.as_str(),
            routable = known.as_str(),
            "no upstream routes this provider; refused rather than forwarded to the wrong host",
        ),
        _ => warn!(error = %err, "forwarding failed"),
    }
}

/// The status a failed forward answers with.
///
/// `502` for everything that went wrong *reaching* an upstream, which is what
/// that status means. A request labelled with a provider this capture has no
/// route for never reached one and never could: `421` says the request was
/// directed at a server that cannot produce a response for it, which is exactly
/// the case, and keeps it distinguishable in a harness's own logs from an
/// upstream that was tried and failed.
fn refusal_status(err: &crate::error::Error) -> StatusCode {
    match err {
        crate::error::Error::UnroutableProvider { .. } => StatusCode::MISDIRECTED_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
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

    // The nonce echo is likewise between the harness and this proxy alone, and
    // it is a *secret*: forwarding it upstream would hand every hop past this
    // process the value that authenticates the session's envelope. Stripped
    // unconditionally — before any capture or forwarding decision — so no code
    // path can leak it, not even on a lane that never validates it.
    out_headers.remove(GATEWAY_NONCE_HEADER);

    // What this Codex request says about its own identity — the root session
    // it belongs to, the sub-thread it is, that thread's immediate parent —
    // and the correlation id that joins the turn back to this decision.
    //
    // Withholding it does not merely stand the identity rungs down: a
    // sub-thread's turn is keyed on whichever rollout the ladder resolved
    // instead of on the ROOT the request names, so it files as a session of
    // its own and the spawning session loses its subagent subtree. The ladder
    // cannot recover from that later, because the child's rollout really is
    // the rollout that turn was written to — nothing downstream contradicts
    // it. Identity has to travel with the request or it is lost.
    //
    // Minted on the Codex lane only, and on exactly the condition the
    // pipeline uses to take that lane, so no request is parsed for headers
    // that will never be consulted.
    let codex_identity = (state.codex_lane || marker.is_some()).then(|| {
        CodexRequestIdentity::from_headers(&parts.headers)
            .with_correlation_id(uuid::Uuid::new_v4().to_string())
    });

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
            // The rollout the request itself names — the only evidence that
            // separates threads inside one Codex process. First present
            // header wins, in the crate's declared order. Superseded by the
            // parsed identity above, which reads the same two headers and
            // additionally withholds them from a request that contradicts
            // itself; kept so the lane still has rollout evidence on the
            // paths that pass no identity.
            codex_rollout_id: tapes_harnesses::attribution::codex::CODEX_ROLLOUT_ID_HEADERS
                .iter()
                .find_map(|name| parts.headers.get(*name).and_then(|v| v.to_str().ok()))
                .filter(|value| !value.trim().is_empty()),
            codex_identity: codex_identity.as_ref(),
            // Stood down: hook evidence presupposes a lifecycle-hook lane this
            // client does not have — `tapesctl start codex` launches the CLI
            // rather than configuring the desktop app. `None` is the
            // documented degradation; the hook rungs simply never fire.
            codex_hook_evidence: None,
        },
    )
    .await;

    // Register the session for transcript tailing before anything else can fail:
    // the wire turn may yet be skipped for being oversize or unparseable, but the
    // session's transcripts are still ours to deliver.
    if let Some(session) = attributed.claude_session() {
        state.transcript_tracker.observe(session);
    }

    // Whose account of the session wins. Three lanes can name one, and at
    // most one of them is live on any given proxy.
    //
    // For a redirected harness there is only the pipeline's. For a
    // self-attributing harness the request carries an envelope stamped inside
    // the harness, which is strictly better evidence: the peer-PID path cannot
    // see into an extension, so the pipeline's answer for those requests is
    // always `unknown`. For a harness attributed by lifecycle hooks there is
    // no launched process to anchor on at all, and the answer comes from the
    // registry an authenticated report filled.
    let desktop = desktop_envelope(&state, &parts.headers);
    let from_desktop = desktop.is_some();
    let envelope_attribution = desktop
        .or_else(|| trusted_inbound_envelope(&state, peer, &parts.headers))
        .or_else(|| attributed.envelope());
    let session = envelope_attribution
        .as_ref()
        .map(|a| SessionEnvelope::from_attribution(a, &state.org_id, &state.auth_subject));

    // Stamp the envelope outbound too. This proxy posts its own turns, so it
    // does not need the headers for itself — but when the upstream is itself a
    // capture-aware gateway, an unstamped request would be attributed
    // differently there than here.
    //
    // `stamp` rather than injecting the value above: it also honours the rule
    // that a harness which stamped its own complete envelope keeps it, which a
    // hand-rolled inject would silently overwrite. The desktop lane is the one
    // case `stamp` cannot express — the pipeline has no lane for a harness
    // this process did not launch, so it would write `unknown` over the
    // identity the turn is about to be filed under.
    match (from_desktop, envelope_attribution) {
        (true, Some(attribution)) => {
            envelope::inject_tapes_attribution(&mut out_headers, attribution)
                .context(error::EnvelopeSnafu)?;
        }
        _ => attributed
            .stamp(&mut out_headers)
            .context(error::EnvelopeSnafu)?,
    }

    let thread_id = envelope::thread_id(&parts.headers).map(str::to_owned);
    let route = resolve_route(&state, parts.uri.path())?;
    let url = build_upstream_url(&route.upstream, &route.path, parts.uri.query())?;
    debug!(method = %parts.method, url = %url, "forwarding");

    let method = parts.method.clone();
    // The path with this proxy's own routing label taken off, which for an
    // unlabelled request is the inbound path unchanged. The label is a private
    // arrangement between this proxy and the extension it launched; carrying it
    // into a turn's meta would put a route no provider serves into the record of
    // what the harness asked for.
    let path = route.path.clone();
    // Read off the URL the request is about to be sent to, so the eligibility
    // gate below and the bytes on the wire cannot disagree about which endpoint
    // this is. Taken before the URL is consumed by the send.
    //
    // Not the same as `path` above once a label is in play: that one has the
    // label removed, this one additionally carries whatever route prefix the
    // resolved upstream's base contributes.
    let provider_path = turn_policy::provider_path(&url).to_owned();
    let response = reqwest::Client::new()
        .request(method.clone(), url)
        .headers(out_headers)
        .body(reqwest::Body::wrap_stream(BodyDataStream::new(replay)))
        .send()
        .await
        .context(error::UpstreamSnafu)?;

    let status = response.status();
    let upstream_headers = response.headers().clone();

    // Is this exchange a turn at all? Both gates are decidable now, before a
    // byte of the response has been read, so an exchange that is not a turn is
    // never buffered and never teed — it is forwarded and nothing else.
    //
    // The path handed to the gate is the one the PROVIDER sees, taken back off
    // the URL this request was actually built from. See `turn_policy`: a
    // harness's own path can be a proper suffix of the provider's.
    if let Some(reason) =
        turn_policy::capture_refusal(method.as_str(), &provider_path, status.as_u16())
    {
        report_refusal(reason, &method, &path, status);
        return Ok(build_response(
            status,
            &upstream_headers,
            Body::from_stream(response.bytes_stream()),
        ));
    }

    // Everything the capture needs is known now, at response-header time. The
    // body has not been read yet — it is teed as it streams.
    let capture = TurnCapture {
        state: state.clone(),
        provider: route.provider,
        request_body: peeked.whole_body().cloned(),
        // The REQUEST's encoding, read from the inbound headers — not the
        // response's, which `TurnMeta::content_encoding` below carries. The two
        // are independent and routinely differ.
        request_encoding: header_string(&parts.headers, http::header::CONTENT_ENCODING),
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
    // Registered here, one statement before the spawn, because the capture is
    // owed to the tally from this moment: the response below returns to a
    // harness that may exit as soon as it has read it, and shutdown must not
    // find an idle tally while this turn is still on its way to ingest.
    let in_flight = state.tally.begin();
    tokio::spawn(async move {
        // Held for the whole task, and dropped by the runtime however it ends —
        // early return, error, or panic — so an abandoned capture releases the
        // drain instead of holding it to the deadline.
        let _in_flight = in_flight;
        capture.run(rx).await;
    });

    let stream = ResponseTee::new(response.bytes_stream(), tx);
    Ok(build_response(
        status,
        &upstream_headers,
        Body::from_stream(stream),
    ))
}

/// Say why an exchange was not captured.
///
/// A drop is never silent — the reason is the whole diagnostic value a
/// non-captured exchange still has, and an upstream failure that goes unsaid is
/// indistinguishable from a capture that quietly stopped working.
///
/// The severities follow this proxy's existing rule (see `tests/log_severity.rs`):
/// a warning means "a turn you expected to see was dropped". An upstream that
/// refused the call is exactly that, and it carries the status, which is the
/// first thing anyone reading the line needs. A request that was never a turn is
/// not — a harness makes several health probes and model listings per session,
/// by construction, and warning on each teaches the reader to skim past the one
/// severity that matters.
fn report_refusal(reason: &'static str, method: &http::Method, path: &str, status: StatusCode) {
    if reason == turn_policy::DROP_UPSTREAM_STATUS {
        warn!(
            drop_reason = reason,
            upstream_status = status.as_u16(),
            %method,
            path,
            "the upstream did not complete the exchange; forwarded but not captured",
        );
    } else {
        debug!(
            drop_reason = reason,
            %method,
            path,
            "not a turn request; forwarded but not captured",
        );
    }
}

/// The session a lifecycle-hook harness's request belongs to, if any.
///
/// `None` on every proxy that is not capturing such a harness, and `None` for
/// a request whose identities no authenticated report introduced — which is
/// the fail-closed rule for this lane stated in one place. There is no
/// fallback: a Codex desktop request naming a session nobody reported is filed
/// as `unknown`, never as the session that happens to be the only one known.
///
/// Note what is *not* consulted: the peer. A harness this process did not
/// launch has no PID to compare against, so the ancestry half of the
/// `start`-side trust pair simply does not exist here. Its job is done earlier
/// and elsewhere — by the secret that had to be presented before any of these
/// identities could enter the registry at all. See
/// [`crate::codex_app::lifecycle`] for what that does and does not buy.
fn desktop_envelope(state: &ProxyState, headers: &HeaderMap) -> Option<envelope::TapesAttribution> {
    let sessions = state.desktop_sessions.as_ref()?;
    let identities = crate::codex_app::lifecycle::request_identities(headers);
    let resolved = sessions.resolve(identities.iter().copied());
    if resolved.is_none() && !identities.is_empty() {
        debug!(
            "a request named a Codex session no lifecycle report introduced; \
             filing the turn as unattributed",
        );
    }
    Some(resolved?.envelope())
}

/// The inbound envelope, if this request is entitled to supply one.
///
/// Three conditions, in the order that costs least. The launched harness must
/// be one that attributes itself — otherwise no request on this proxy has any
/// business naming a session — the request must actually carry an envelope, and
/// the request must echo the per-launch nonce, all of which are header reads
/// and byte comparisons. Only then is the peer resolved to a process, which is
/// a scan of the kernel's socket table.
///
/// The nonce and the peer check are jointly the security boundary, and each
/// covers the other's blind spot. A loopback listener accepts connections from
/// every process on the machine, so without the peer check two header values
/// would be enough for any of them to have a turn persisted — and a session
/// link printed — under a session id it picked. But the peer check trusts the
/// launched harness's whole subtree, and the harness runs arbitrary
/// subprocesses on request: a command in a shell tool is a descendant with a
/// clean ancestry walk. The nonce is what it lacks — the secret this process
/// generated and handed only to the harness's own environment at spawn. An
/// envelope is believed only with both: the echoed secret and the ancestry.
///
/// A refusal is logged rather than passed over in silence — but never with the
/// nonce value, in either its expected or presented form. It is either an
/// attempt at exactly the above, or a misconfiguration now quietly filing pi
/// turns under `unknown` — and both are things whoever reads the log needs to
/// be able to see.
fn trusted_inbound_envelope(
    state: &ProxyState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<envelope::TapesAttribution> {
    if !state.self_attributing {
        return None;
    }
    let claimed = inbound_envelope(headers)?;
    let presented_nonce = headers
        .get(GATEWAY_NONCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !nonce_matches(&state.gateway_nonce, presented_nonce) {
        warn!(
            %peer,
            harness_id = %claimed.harness_id,
            "a request carrying a session envelope did not echo this launch's \
             nonce; filing the turn as unattributed",
        );
        return None;
    }
    let launched = match state
        .launched_pid
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        super::NO_LAUNCHED_PID => None,
        pid => Some(pid),
    };
    if peer_trust::peer_is_launched_harness(peer, launched) {
        return Some(claimed);
    }
    warn!(
        %peer,
        harness_id = %claimed.harness_id,
        launched_pid = ?launched,
        "a request carrying a session envelope did not come from the launched \
         harness; filing the turn as unattributed",
    );
    None
}

/// Read a session identity out of an envelope the harness stamped on itself.
///
/// The completeness rule — a harness id that is present and not the `unknown`
/// sentinel, plus a non-blank session id — is the crate's, restated because it
/// is private there. It has to be the same rule: the crate applies it when
/// deciding whether to *preserve* an inbound envelope on the outbound request,
/// and this decides whether to file the turn under one. Two different rules
/// would produce a request whose headers say `pi` and whose ingest row says
/// `unknown`.
///
/// Only the plain-text fields are read. `cwd`, session name, and metadata are
/// percent-encoded or base64url on the wire, and decoding them here would be a
/// second, drifting implementation of an encoder the crate owns — so a harness
/// that sends them wants a `TapesAttribution::from_headers` in the crate rather
/// than more parsing here. Nothing is lost today: pi's extension stamps exactly
/// the two headers this reads.
fn inbound_envelope(headers: &HeaderMap) -> Option<envelope::TapesAttribution> {
    let harness_id = envelope_field(headers, envelope::X_TAPES_HARNESS_ID)
        .filter(|id| id != envelope::HARNESS_ID_UNKNOWN)?;
    let session_id = envelope_field(headers, envelope::X_TAPES_HARNESS_SESSION_ID)?;
    Some(envelope::TapesAttribution {
        harness_id,
        session_id: Some(session_id),
        version: envelope_field(headers, envelope::X_TAPES_HARNESS_VERSION),
        cwd: None,
        name: None,
        parent_sid: envelope_field(headers, envelope::X_TAPES_PARENT_HARNESS_SESSION_ID),
        metadata: serde_json::Map::new(),
    })
}

/// One `X-Tapes-*` header, trimmed, absent when blank.
fn envelope_field(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
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

/// Where one request goes, and which provider its turn files under.
struct Route {
    /// The upstream to forward to.
    upstream: Url,
    /// The path to ask that upstream for, label stripped.
    path: String,
    /// The ingest `provider` for the captured turn.
    provider: &'static str,
}

/// Resolve the request path into an upstream, a path, and a provider.
///
/// Three cases, and the middle one is the whole point of the labelling:
///
/// * **Labelling is off.** Every request goes to the launch's upstream, which
///   is the only shape a single-provider capture has.
/// * **Labelled with a provider this capture routes.** The label is stripped
///   and the remainder goes to that provider's own upstream. This is what makes
///   a pi session on a provider other than the fronted schema work at all: the
///   registration named the provider, and nothing later in the request does.
/// * **Labelled with a provider this capture does not route.** Refused. The
///   default upstream speaks one provider's API and would answer a foreign
///   route with a 404 the harness reports as a model failure — and whose body
///   is then captured and rejected by ingest as a malformed turn. One refusal
///   naming the provider replaces both.
///
/// A request that carries no label while labelling is on takes the first case,
/// deliberately. It means the installed extension predates this launch's
/// binary, and the provider it wanted is simply not in the request — so there
/// is nothing to refuse *on*. Sending it to the launch's upstream is what would
/// have happened before any of this existed, which makes a stale extension no
/// worse than it was rather than a dead session.
fn resolve_route(state: &ProxyState, path: &str) -> Result<Route> {
    let unlabelled = |provider| {
        Ok(Route {
            upstream: state.upstream.clone(),
            path: path.to_owned(),
            provider,
        })
    };
    let Some(routes) = state.provider_routes.as_deref() else {
        return unlabelled(state.provider);
    };
    let Some((label, rest)) = split_provider_route(path) else {
        debug!(
            path,
            "request carries no provider label; forwarding to the launch upstream",
        );
        return unlabelled(state.provider);
    };
    let Some(route) = routes.resolve(label) else {
        return error::UnroutableProviderSnafu {
            provider: label.to_owned(),
            known: routes.known(),
        }
        .fail();
    };
    debug!(label, upstream = %route.upstream, "routing a labelled request");
    Ok(Route {
        upstream: route.upstream.clone(),
        path: rest.to_owned(),
        provider: route.provider,
    })
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
    /// Bytes cloned into the capture channel so far. The channel is
    /// unbounded, so the bound must live at the SENDER: without it a fast,
    /// large response queues every chunk clone ahead of the capture task's
    /// cap check, and the queue itself becomes the memory blowup.
    teed_bytes: usize,
    /// Set once the cap is crossed; one final over-cap chunk is sent (which
    /// deterministically flips the capture task's overflow handling) and
    /// nothing further is cloned. Forwarding is untouched.
    capture_capped: bool,
}

impl<S> ResponseTee<S> {
    fn new(stream: S, tx: UnboundedSender<TeeEvent>) -> Self {
        Self {
            inner: Box::pin(stream),
            tx: Some(tx),
            teed_bytes: 0,
            capture_capped: false,
        }
    }

    /// One non-blocking send, then straight back to the consumer. A failed
    /// send means the capture task is gone, which must not interrupt the
    /// stream the user is reading. Past the raw cap, nothing more is cloned —
    /// the last (over-cap) send is what tells the capture task to
    /// drop-and-mark, and the sender-side accounting is what bounds the
    /// unbounded channel's memory.
    fn tee_chunk(&mut self, chunk: &Bytes) {
        if self.capture_capped {
            return;
        }
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.send(TeeEvent::Chunk(chunk.clone()));
        }
        self.teed_bytes = self.teed_bytes.saturating_add(chunk.len());
        if self.teed_bytes > RAW_RESPONSE_CAP {
            self.capture_capped = true;
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
                self.tee_chunk(&chunk);
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
    /// The ingest `provider` this turn files under — the route's, which for a
    /// labelled request is the labelled provider's rather than the launch's.
    provider: &'static str,
    /// `None` when the request body exceeded the peek cap, which makes the turn
    /// undescribable and so uncapturable.
    request_body: Option<Bytes>,
    /// The request's `Content-Encoding`, if it declared one. Those bytes are
    /// forwarded untouched; this is what lets the capture copy be decoded.
    request_encoding: Option<String>,
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
        // An empty body on a method that never carries one is not a defect: it
        // is what every GET on this endpoint looks like, and a harness makes
        // several of those (model listing, auth probes) per session. Warning on
        // them trains the reader to ignore the one severity that means "a turn
        // you expected to see was dropped". But an empty body on a turn-shaped
        // method (POST/PUT/PATCH) IS that dropped turn, so it falls through and
        // keeps its warning below.
        //
        // Ahead of the decode, so a bodiless request whose headers still claim
        // an encoding is not reported as a decode failure: there are no bytes
        // for the claim to be about.
        if body.is_empty() && !matches!(self.meta.method.as_str(), "POST" | "PUT" | "PATCH") {
            debug!(
                request_id = %self.meta.request_id,
                method = %self.meta.method,
                "request had no body; nothing to capture",
            );
            return;
        }

        // Compressed request bodies are decoded before they are validated,
        // under the same rules the cloud capture route applies at the gateway.
        // Skipping this is how a `content-encoding: zstd` harness — pi's Codex
        // provider is one — forwarded perfectly while capturing nothing.
        let decoded = match decode_content_encoding(body, self.request_encoding.as_deref()) {
            Ok(decoded) => decoded,
            Err(err) => {
                warn!(
                    error = %err,
                    content_encoding = self.request_encoding.as_deref().unwrap_or_default(),
                    request_id = %self.meta.request_id,
                    "request body could not be decoded; turn forwarded but not captured",
                );
                return;
            }
        };
        if decoded.truncated {
            // Not a drop — the turn below is still posted — but the body it
            // carries is a prefix of what the harness sent, and a reader
            // comparing it against the response deserves to know that.
            debug!(
                request_id = %self.meta.request_id,
                content_encoding = self.request_encoding.as_deref().unwrap_or_default(),
                "request body was salvaged from a truncated stream",
            );
        }

        // `RawValue` requires valid JSON. Validating here is not parsing for
        // meaning — nothing reads a field — it is the minimum needed to embed
        // the bytes verbatim in a JSON document.
        let request =
            match RawValue::from_string(String::from_utf8_lossy(&decoded.bytes).into_owned()) {
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
            provider: self.provider,
            request: &request,
            response: (),
            raw_response: (!raw.is_empty()).then(|| encode_raw_response(&raw)),
            raw_response_encoding: self.meta.content_encoding.clone(),
            meta: self.meta.clone(),
            session: self.session.clone(),
        };

        // Whose turn this is: a harness id that is not the sentinel, plus a
        // session id to group under. Deciding it here rather than trusting
        // `session.is_some()` matters because the desktop lane always sends a
        // session block — including one that says `unknown` — so presence alone
        // would count an unattributed turn as attributed and re-hide the bug.
        let session_id = self.session.as_ref().and_then(|session| {
            (session.harness_id != envelope::HARNESS_ID_UNKNOWN)
                .then(|| session.harness_session_id.clone())
                .flatten()
        });

        match self.state.ingest.post_turn(&payload).await {
            Ok(()) => {
                // Recorded only on acceptance, and the session id travels with
                // it: the summary tells the caller what they can go and look
                // at, and a rejected turn is neither a capture to count nor a
                // session to link. Attribution resolved long before this point,
                // so nominating the link any earlier would name a session on
                // the strength of a turn that never landed.
                self.state.tally.record(session_id.as_deref());
                debug!(
                    request_id = %self.meta.request_id,
                    finalized_by = reason,
                    bytes = raw.len(),
                    attributed = session_id.is_some(),
                    "turn captured",
                );
            }
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

    // The queue is unbounded, so the byte bound must hold at the sender: a
    // response far past the raw cap must stop being cloned once one over-cap
    // chunk has been sent (that chunk is what flips the capture task into
    // drop-and-mark), while forwarding continues untouched.
    #[tokio::test]
    async fn tee_stops_cloning_past_the_raw_cap() {
        use futures_util::StreamExt;
        let big: &'static [u8] = Box::leak(vec![0u8; 4 * 1024 * 1024].into_boxed_slice());
        // 5 chunks of 4 MiB = 20 MiB forwarded; cap is 8 MiB.
        let (mut tee, mut rx) = tee_of(vec![big, big, big, big, big]);
        let mut forwarded = 0usize;
        while let Some(chunk) = tee.next().await {
            forwarded += chunk.unwrap().len();
        }
        drop(tee);
        let (teed, _finishes) = drain_events(&mut rx);
        assert_eq!(forwarded, 20 * 1024 * 1024, "forwarding must be untouched");
        assert!(
            teed.len() <= RAW_RESPONSE_CAP + 4 * 1024 * 1024,
            "the capture queue must be bounded by the cap plus one chunk, got {} bytes",
            teed.len(),
        );
        assert!(
            teed.len() > RAW_RESPONSE_CAP,
            "exactly one over-cap chunk must be sent so the capture task marks the overflow",
        );
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
