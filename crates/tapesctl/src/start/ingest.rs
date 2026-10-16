//! The ingest client: one captured turn in, one `POST /v1/ingest` out.
//!
//! # Raw-only, structurally
//!
//! tapesctl is born raw-only: it ships the verbatim upstream response bytes and
//! lets the server reduce them. That is not a policy comment — [`TurnPayload`]
//! has **no field a reduction could go in**. Its `response` is `()`, which
//! serialises as `null`, and ingest treats an absent reduction plus a non-empty
//! `raw_response` as "reduce this yourself".
//!
//! The discipline matters because a client-side reducer is how the two capture
//! paths drift: the moment tapesctl reduces locally, its rows and the daemon's rows
//! are produced by different code, and the parity corpus starts policing a
//! difference that should never have been expressible. Reduction lives in
//! exactly one place — ingest — where a re-derive can revisit it. Shipping raw
//! also means a turn whose reduction fails is recoverable rather than lost.
//!
//! # Wire-shape gotchas
//!
//! Four details of the Go contract are easy to get wrong and produce either a
//! silent misfile or a 400:
//!
//! * `parent_harness_session_id` must be **omitted**, never sent as `""` — the
//!   server rejects the empty string rather than treating it as absent.
//! * `parent_harness_id` follows the same rule, and is sent only for a parent
//!   in a *different* harness. The server resolves the parent session under
//!   that harness, and under the child's own when the field is absent, so a
//!   same-harness parent omits it and a cross-harness parent without it would
//!   be looked up — and placeholder-inserted — in the wrong namespace.
//! * `org_id` and `auth_subject` carry no `omitempty` on the Go side, so they
//!   are always emitted, empty string included.
//! * `raw_response` is a Go `[]byte`, which is standard **padded** base64 in
//!   JSON — not base64url, not an array of integers.
//!
//! # Asking before sending a cross-harness parent
//!
//! Ingest ignores an envelope field it does not know, so `parent_harness_id`
//! sent to a tapes that predates it is not an error: that server resolves the
//! parent in the child's harness, misses, and files the child under a
//! placeholder parent there. The write succeeds and the lineage is wrong.
//!
//! So the field is sent only to a server that says it understands it. The
//! first turn that would carry a cross-harness parent asks `GET /ping` for the
//! server's capabilities, once per client; a server that advertises
//! [`CAPABILITY_PARENT_HARNESS_ID`] gets the parent and its harness, and any
//! other answer — an older server's bare `{"status":"ok"}`, an error, a
//! timeout — gets the behaviour that predates the field: the cross-harness
//! parent is left off the row, which costs the link and mints nothing. A
//! same-harness parent never needs the field and is unaffected. The fallback is
//! logged once, and reported at exit.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use snafu::ResultExt;
use tapes_capture::envelope::TapesAttribution;
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};
use url::Url;

use crate::error::{Error, Result, error};

/// Path of the turn-ingest endpoint, relative to the tapes ingest base URL.
pub const INGEST_PATH: &str = "/v1/ingest";

/// Path of the ingest liveness probe, which also lists what the server
/// understands. See the module docs.
pub const PING_PATH: &str = "/ping";

/// The capability a tapes ingest advertises when it resolves a fork parent
/// under the envelope's `parent_harness_id`.
pub const CAPABILITY_PARENT_HARNESS_ID: &str = "parent_harness_id";

/// How long the capability probe may take. Past it the server is treated as
/// having no capabilities: the probe runs in a capture task, after the harness
/// already has its response, so waiting longer costs nothing but the turn's
/// own delay in landing — and an ingest that slow is about to fail the post too.
const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// One captured turn, in the shape `POST /v1/ingest` accepts.
#[derive(Debug, Serialize)]
pub struct TurnPayload<'a> {
    /// Provider family whose wire format `request` and `raw_response` are in.
    /// Ingest rejects anything outside its known set with a 422.
    pub provider: &'a str,

    /// The request body, embedded verbatim. A `RawValue` so the bytes reach
    /// the server exactly as the harness wrote them — round-tripping through
    /// `serde_json::Value` would reorder map keys.
    pub request: &'a RawValue,

    /// Always `null`. This is the raw-only contract in the type system: there
    /// is no reduced response to send, because reducing is the server's job.
    /// `()` serialises as `null`, which ingest reads as "no reduction supplied".
    pub response: (),

    /// The verbatim upstream response bytes, standard-padded base64. Absent
    /// only when the capture had to be abandoned, in which case the turn is
    /// not posted at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_response: Option<String>,

    /// The upstream `Content-Encoding` those bytes are still in — the proxy
    /// never decompresses. Ingest decodes it before reducing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_response_encoding: Option<String>,

    /// Transport-level facts about the turn. Every field is derived from
    /// headers, timing, or byte counts; none requires parsing a body.
    pub meta: TurnMeta,

    /// Who this turn belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionEnvelope>,
}

/// Transport metadata for one turn.
#[derive(Debug, Default, Clone, Serialize)]
pub struct TurnMeta {
    /// Idempotency key. `(org_id, request_id)` is ingest's dedup key, so a
    /// retried POST of the same turn is a no-op insert rather than a duplicate
    /// row — which is why this is never left empty.
    pub request_id: String,

    /// The harness-native sub-thread id, when the harness stamped one. Present
    /// on subagent traffic and absent on the main thread, which is what makes
    /// thread attribution deterministic at capture time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,

    /// Request method as forwarded.
    pub method: String,

    /// Request path as forwarded.
    pub path: String,

    /// Response `Content-Type`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,

    /// Response `Content-Encoding`, mirroring `raw_response_encoding`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,

    /// `"true"` when the response was an event stream. Inferred from the
    /// response content type, not from the request body — this client does not
    /// parse bodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,

    /// Upstream HTTP status.
    pub upstream_status: u16,

    /// `2xx`-style status class.
    pub upstream_status_class: String,

    /// Bytes forwarded upstream.
    pub request_bytes: usize,

    /// Bytes streamed back.
    pub response_bytes: usize,

    /// Wall-clock duration of the turn.
    pub elapsed_seconds: f64,
}

/// The session a turn belongs to, as ingest's envelope block.
#[derive(Debug, Clone, Serialize)]
pub struct SessionEnvelope {
    /// Owning org. Must parse as a UUID, or be empty for the local sentinel.
    /// Emitted unconditionally: the Go field carries no `omitempty`.
    pub org_id: String,

    /// The acting subject. Also emitted unconditionally.
    pub auth_subject: String,

    /// Harness identifier, or `unknown` when attribution failed.
    pub harness_id: String,

    /// The harness's own session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,

    /// Harness version string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,

    /// Working directory the harness was launched in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// User-given session name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Fork parent. **Omitted** when unknown — the server rejects `""` here
    /// rather than reading it as absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_harness_session_id: Option<String>,

    /// The fork parent's harness, when it is not this envelope's. **Omitted**
    /// for a same-harness parent and whenever there is no parent — the server
    /// rejects `""`, and absence already means "the child's harness".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_harness_id: Option<String>,

    /// Free-form harness facts, stored as `sessions.harness_metadata`. Must be
    /// a JSON object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

impl SessionEnvelope {
    /// Build the envelope block from a pipeline attribution plus the identity
    /// the client supplies.
    ///
    /// The attribution half comes from `tapes-harnesses`, so the session facts
    /// a tapesctl turn carries are produced by the same code the daemon runs. The
    /// identity half — org and subject — is the client's: standalone tapesctl
    /// has no gateway to stamp validated claims, so it names the local user.
    #[must_use]
    pub fn from_attribution(
        attribution: &TapesAttribution,
        org_id: &str,
        auth_subject: &str,
    ) -> Self {
        Self {
            org_id: org_id.to_owned(),
            auth_subject: auth_subject.to_owned(),
            harness_id: attribution.harness_id.clone(),
            harness_session_id: attribution.session_id.clone(),
            harness_version: attribution.version.clone(),
            cwd: attribution.cwd.clone(),
            name: attribution.name.clone(),
            // Filtered rather than cloned: an empty parent id is a 400, and
            // the only safe representation of "no parent" is omission.
            parent_harness_session_id: parent_session_id_of(attribution),
            parent_harness_id: parent_harness_id_of(attribution),
            harness_metadata: (!attribution.metadata.is_empty())
                .then(|| attribution.metadata.clone()),
        }
    }

    /// Whether this envelope names a parent in another harness — the one
    /// shape an ingest without [`CAPABILITY_PARENT_HARNESS_ID`] would misfile.
    #[must_use]
    pub fn has_cross_harness_parent(&self) -> bool {
        self.parent_harness_id.is_some()
    }

    /// This envelope as a server that predates `parent_harness_id` must
    /// receive it: a parent in another harness is left off entirely, since
    /// that server would look it up in this harness and file a placeholder
    /// there. A same-harness parent is kept — it never carried the field.
    #[must_use]
    pub fn without_cross_harness_parent(mut self) -> Self {
        if self.parent_harness_id.take().is_some() {
            self.parent_harness_session_id = None;
        }
        self
    }
}

/// The parent's harness, only beside a parent session and only when it is
/// not the child's own. See the module docs.
fn parent_harness_id_of(attribution: &TapesAttribution) -> Option<String> {
    parent_session_id_of(attribution)?;
    attribution
        .parent_harness_id
        .as_deref()
        .map(str::trim)
        .filter(|parent| !parent.is_empty() && *parent != attribution.harness_id)
        .map(str::to_owned)
}

fn parent_session_id_of(attribution: &TapesAttribution) -> Option<String> {
    attribution
        .parent_sid
        .as_deref()
        .filter(|sid| !sid.trim().is_empty())
        .map(str::to_owned)
}

/// Encode verbatim response bytes for the `raw_response` field.
#[must_use]
pub fn encode_raw_response(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// Classify a status code the way ingest's `upstream_status_class` expects.
#[must_use]
pub fn status_class(status: u16) -> String {
    format!("{}xx", status / 100)
}

/// What a tapes ingest server says it understands, from `GET /ping`.
///
/// The default is "nothing": every capability is opt-in by the server, so a
/// server that predates the list, or a probe that failed, reads as none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestCapabilities {
    parent_harness_id: bool,
}

impl IngestCapabilities {
    /// Read the capabilities out of a `/ping` body.
    ///
    /// Anything other than an object whose `capabilities` is an array of
    /// strings — an older server's `{"status":"ok"}`, a `null`, a gateway's
    /// HTML error page — is no capabilities. Unknown entries are ignored, so a
    /// newer server's list does not confuse this client.
    #[must_use]
    pub fn from_ping_body(body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Ping {
            #[serde(default)]
            capabilities: Vec<String>,
        }
        let Ok(ping) = serde_json::from_slice::<Ping>(body) else {
            return Self::default();
        };
        Self {
            parent_harness_id: ping
                .capabilities
                .iter()
                .any(|capability| capability == CAPABILITY_PARENT_HARNESS_ID),
        }
    }

    /// The server resolves a fork parent under `parent_harness_id`.
    #[must_use]
    pub fn parent_harness_id(&self) -> bool {
        self.parent_harness_id
    }
}

/// The stderr note for a launch whose cross-harness parent this server could
/// not be given. Printed at exit, the one time the terminal is ours.
#[must_use]
pub fn withheld_parent_note(withheld: usize) -> String {
    format!(
        "tapesctl: note: this tapes server predates cross-harness parents (it does not \
         advertise `parent_harness_id`), so {withheld} {} filed without the parent \
         session from another harness; upgrade tapes to record that link",
        if withheld == 1 {
            "turn was"
        } else {
            "turns were"
        },
    )
}

/// A client for one tapes ingest server.
///
/// Clones share the capability probe and its outcome, so one launch asks the
/// server at most once however many capture tasks it runs.
#[derive(Debug, Clone)]
pub struct IngestClient {
    http: reqwest::Client,
    endpoint: Url,
    ping: Url,
    capabilities: Arc<OnceCell<IngestCapabilities>>,
    withheld_parents: Arc<AtomicUsize>,
}

impl IngestClient {
    /// Build a client posting to `base` + [`INGEST_PATH`].
    pub fn new(base: &Url) -> Result<Self> {
        let endpoint = base.join(INGEST_PATH).context(error::IngestUrlSnafu)?;
        let ping = base.join(PING_PATH).context(error::IngestUrlSnafu)?;
        Ok(Self {
            http: reqwest::Client::new(),
            endpoint,
            ping,
            capabilities: Arc::new(OnceCell::new()),
            withheld_parents: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// What this server understands, probed on first use and cached for the
    /// life of the client. Never fails: an unanswerable probe is "nothing".
    pub async fn capabilities(&self) -> IngestCapabilities {
        *self.capabilities.get_or_init(|| self.probe()).await
    }

    async fn probe(&self) -> IngestCapabilities {
        let response = self
            .http
            .get(self.ping.clone())
            .timeout(CAPABILITY_PROBE_TIMEOUT)
            .send()
            .await;
        let capabilities = match response {
            Ok(response) if response.status().is_success() => match response.bytes().await {
                Ok(body) => IngestCapabilities::from_ping_body(&body),
                Err(err) => {
                    debug!(error = %err, "could not read the ingest capability probe");
                    IngestCapabilities::default()
                }
            },
            Ok(response) => {
                debug!(status = %response.status(), "the ingest capability probe was refused");
                IngestCapabilities::default()
            }
            Err(err) => {
                debug!(error = %err, "the ingest capability probe failed");
                IngestCapabilities::default()
            }
        };
        info!(
            ping = %self.ping,
            parent_harness_id = capabilities.parent_harness_id(),
            "tapes ingest capabilities",
        );
        capabilities
    }

    /// `session` as this server can be trusted to file it.
    ///
    /// A cross-harness parent goes only to a server that advertises
    /// [`CAPABILITY_PARENT_HARNESS_ID`]; to any other it is withheld — the
    /// behaviour that predates the field — and the first withholding is logged.
    /// Every other envelope passes through without a probe.
    pub async fn fit_session(&self, session: Option<SessionEnvelope>) -> Option<SessionEnvelope> {
        let session = session?;
        if !session.has_cross_harness_parent() || self.capabilities().await.parent_harness_id() {
            return Some(session);
        }
        if self.withheld_parents.fetch_add(1, Ordering::Relaxed) == 0 {
            warn!(
                ping = %self.ping,
                parent_harness = session.parent_harness_id.as_deref().unwrap_or_default(),
                "this tapes ingest does not advertise `parent_harness_id`, so a parent session \
                 in another harness is left off the ingest row rather than misfiled; upgrade \
                 tapes to link it",
            );
        }
        Some(session.without_cross_harness_parent())
    }

    /// How many turns had a cross-harness parent withheld by [`Self::fit_session`].
    #[must_use]
    pub fn withheld_parents(&self) -> usize {
        self.withheld_parents.load(Ordering::Relaxed)
    }

    /// The resolved ingest endpoint, for logging.
    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Post one turn.
    ///
    /// Ingest carries no auth of its own — it is not internet-facing and trusts
    /// the identity in the envelope — so there is no credential to attach here.
    pub async fn post_turn(&self, payload: &TurnPayload<'_>) -> Result<()> {
        let response = self
            .http
            .post(self.endpoint.clone())
            .json(payload)
            .send()
            .await
            .context(error::IngestSendSnafu)?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // Surface the server's own explanation: a 400 names the invalid
        // envelope field and a 422 names the unprocessable turn, and both are
        // far more useful than the bare status.
        let body = response.text().await.unwrap_or_default();
        Err(Error::IngestRejected {
            status: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn attribution_with_parent(parent: Option<&str>) -> TapesAttribution {
        let mut attribution = TapesAttribution::unknown();
        attribution.harness_id = "claude".to_owned();
        attribution.session_id = Some("sid-1".to_owned());
        attribution.parent_sid = parent.map(str::to_owned);
        attribution
    }

    fn json_of(envelope: &SessionEnvelope) -> serde_json::Value {
        serde_json::to_value(envelope).unwrap()
    }

    #[test]
    fn an_absent_parent_is_omitted_entirely() {
        // Sending `""` here is a 400 from the server, not a tolerated "no
        // parent" — omission is the only safe encoding.
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution_with_parent(None),
            "",
            "local:test",
        ));
        assert!(got.get("parent_harness_session_id").is_none(), "got: {got}",);
    }

    #[test]
    fn a_blank_parent_is_treated_as_absent() {
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution_with_parent(Some("   ")),
            "",
            "local:test",
        ));
        assert!(got.get("parent_harness_session_id").is_none(), "got: {got}",);
    }

    #[test]
    fn a_real_parent_is_carried() {
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution_with_parent(Some("sid-parent")),
            "",
            "local:test",
        ));
        assert_eq!(got["parent_harness_session_id"], "sid-parent");
    }

    #[test]
    fn a_parent_in_the_same_harness_is_carried() {
        let attribution =
            attribution_with_parent(Some("sid-parent")).with_parent_harness_id("claude");
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution,
            "",
            "local:test",
        ));
        assert_eq!(got["parent_harness_session_id"], "sid-parent");
        // Absent already means "the child's harness"; naming it is redundant
        // and keeps the row's shape identical to a pre-lineage one.
        assert!(got.get("parent_harness_id").is_none(), "got: {got}");
    }

    #[test]
    fn a_parent_in_another_harness_names_its_harness_on_the_row() {
        // Without the harness the server would look `sid-parent` up as a
        // *claude* session and placeholder-insert one when it found none.
        let attribution = attribution_with_parent(Some("sid-parent")).with_parent_harness_id("pi");
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution,
            "",
            "local:test",
        ));
        assert_eq!(got["parent_harness_session_id"], "sid-parent");
        assert_eq!(got["parent_harness_id"], "pi");
    }

    #[test]
    fn a_parent_harness_without_a_parent_session_is_omitted() {
        // A blank parent harness is a 400; one beside no parent names nothing.
        for parent in [None, Some("  ")] {
            let mut attribution = attribution_with_parent(parent);
            attribution.parent_harness_id = Some("pi".to_owned());
            let got = json_of(&SessionEnvelope::from_attribution(
                &attribution,
                "",
                "local:test",
            ));
            assert!(got.get("parent_harness_id").is_none(), "got: {got}");
        }
        let mut blank = attribution_with_parent(Some("sid-parent"));
        blank.parent_harness_id = Some(" ".to_owned());
        let got = json_of(&SessionEnvelope::from_attribution(&blank, "", "local:test"));
        assert!(got.get("parent_harness_id").is_none(), "got: {got}");
    }

    #[test]
    fn org_and_subject_are_emitted_even_when_empty() {
        // The Go fields have no `omitempty`; omitting them here would diverge
        // from every other producer's wire shape.
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution_with_parent(None),
            "",
            "",
        ));
        assert_eq!(got["org_id"], "");
        assert_eq!(got["auth_subject"], "");
    }

    #[test]
    fn empty_metadata_is_omitted_rather_than_sent_as_an_empty_object() {
        let got = json_of(&SessionEnvelope::from_attribution(
            &attribution_with_parent(None),
            "",
            "local:test",
        ));
        assert!(got.get("harness_metadata").is_none(), "got: {got}");
    }

    #[test]
    fn a_turn_always_sends_a_null_reduction() {
        // The raw-only contract: `response` is `()` and there is no field a
        // client-side reduction could occupy.
        let request = RawValue::from_string(r#"{"model":"x"}"#.to_owned()).unwrap();
        let payload = TurnPayload {
            provider: "anthropic",
            request: &request,
            response: (),
            raw_response: Some(encode_raw_response(b"event: ping\n\n")),
            raw_response_encoding: None,
            meta: TurnMeta::default(),
            session: None,
        };
        let got = serde_json::to_value(&payload).unwrap();
        assert!(got["response"].is_null());
    }

    #[test]
    fn the_request_body_is_embedded_verbatim() {
        // Key order is preserved because the body rides as a RawValue; a
        // round-trip through Value would reorder it and change the bytes the
        // server sees.
        let raw = r#"{"zeta":1,"alpha":2}"#;
        let request = RawValue::from_string(raw.to_owned()).unwrap();
        let payload = TurnPayload {
            provider: "anthropic",
            request: &request,
            response: (),
            raw_response: None,
            raw_response_encoding: None,
            meta: TurnMeta::default(),
            session: None,
        };
        let got = serde_json::to_string(&payload).unwrap();
        assert!(
            got.contains(r#""request":{"zeta":1,"alpha":2}"#),
            "got: {got}"
        );
    }

    #[test]
    fn raw_response_is_standard_padded_base64() {
        // Go's encoding/json renders []byte as standard padded base64; a
        // base64url or unpadded encoding decodes to different bytes server-side.
        assert_eq!(encode_raw_response(b"ab"), "YWI=");
        assert_eq!(encode_raw_response(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn status_class_matches_the_servers_spelling() {
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(429), "4xx");
        assert_eq!(status_class(503), "5xx");
    }

    #[test]
    fn the_ingest_path_is_joined_onto_the_base_url() {
        let client = IngestClient::new(&Url::parse("http://127.0.0.1:8090").unwrap()).unwrap();
        assert_eq!(
            client.endpoint().as_str(),
            "http://127.0.0.1:8090/v1/ingest"
        );
    }

    #[test]
    fn a_server_that_lists_the_capability_resolves_cross_harness_parents() {
        let got = IngestCapabilities::from_ping_body(
            br#"{"status":"ok","capabilities":["parent_harness_id"]}"#,
        );
        assert!(got.parent_harness_id());
        // A newer server's other entries are not this client's business.
        let got = IngestCapabilities::from_ping_body(
            br#"{"status":"ok","capabilities":["something_later","parent_harness_id"]}"#,
        );
        assert!(got.parent_harness_id());
    }

    #[test]
    fn anything_but_a_listed_capability_reads_as_unsupported() {
        for body in [
            // A tapes from before the list.
            &br#"{"status":"ok"}"#[..],
            br#"{"status":"ok","capabilities":[]}"#,
            br#"{"status":"ok","capabilities":null}"#,
            br#"{"status":"ok","capabilities":"parent_harness_id"}"#,
            br#"{"status":"ok","capabilities":[1]}"#,
            br#""pong""#,
            b"<html>bad gateway</html>",
            b"",
        ] {
            assert!(
                !IngestCapabilities::from_ping_body(body).parent_harness_id(),
                "{}",
                String::from_utf8_lossy(body),
            );
        }
    }

    fn cross_harness_envelope() -> SessionEnvelope {
        SessionEnvelope::from_attribution(
            &attribution_with_parent(Some("sid-parent")).with_parent_harness_id("pi"),
            "",
            "local:test",
        )
    }

    #[test]
    fn withholding_a_cross_harness_parent_removes_the_parent_entirely() {
        let got = json_of(&cross_harness_envelope().without_cross_harness_parent());
        // Not just the harness: a bare parent id would be resolved in the
        // child's harness, which is the misfile this exists to avoid.
        assert!(got.get("parent_harness_session_id").is_none(), "got: {got}");
        assert!(got.get("parent_harness_id").is_none(), "got: {got}");
        assert_eq!(got["harness_session_id"], "sid-1");
    }

    #[test]
    fn withholding_keeps_a_same_harness_parent() {
        let same = SessionEnvelope::from_attribution(
            &attribution_with_parent(Some("sid-parent")).with_parent_harness_id("claude"),
            "",
            "local:test",
        );
        assert!(!same.has_cross_harness_parent());
        let got = json_of(&same.without_cross_harness_parent());
        assert_eq!(got["parent_harness_session_id"], "sid-parent");
    }

    async fn ingest_answering_ping(body: Option<&str>) -> (wiremock::MockServer, IngestClient) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let response = match body {
            Some(body) => ResponseTemplate::new(200).set_body_string(body.to_owned()),
            None => ResponseTemplate::new(404),
        };
        Mock::given(method("GET"))
            .and(path(PING_PATH))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = IngestClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        (server, client)
    }

    async fn ping_count(server: &wiremock::MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    #[tokio::test]
    async fn a_capable_server_receives_the_cross_harness_parent() {
        let (server, client) = ingest_answering_ping(Some(
            r#"{"status":"ok","capabilities":["parent_harness_id"]}"#,
        ))
        .await;
        let got = client
            .fit_session(Some(cross_harness_envelope()))
            .await
            .unwrap();
        assert_eq!(got.parent_harness_id.as_deref(), Some("pi"));
        assert_eq!(got.parent_harness_session_id.as_deref(), Some("sid-parent"));
        assert_eq!(client.withheld_parents(), 0);
        assert_eq!(ping_count(&server).await, 1);
    }

    #[tokio::test]
    async fn an_older_server_gets_the_envelope_from_before_the_field() {
        let (server, client) = ingest_answering_ping(Some(r#"{"status":"ok"}"#)).await;
        for _ in 0..3 {
            let got = client
                .fit_session(Some(cross_harness_envelope()))
                .await
                .unwrap();
            assert!(got.parent_harness_id.is_none());
            assert!(got.parent_harness_session_id.is_none());
        }
        assert_eq!(client.withheld_parents(), 3);
        // Probed once, and the answer shared with every clone.
        let clone = client.clone();
        let _ = clone.fit_session(Some(cross_harness_envelope())).await;
        assert_eq!(client.withheld_parents(), 4);
        assert_eq!(ping_count(&server).await, 1);
    }

    #[tokio::test]
    async fn a_probe_that_fails_reads_as_unsupported() {
        let (_server, client) = ingest_answering_ping(None).await;
        let got = client
            .fit_session(Some(cross_harness_envelope()))
            .await
            .unwrap();
        assert!(got.parent_harness_id.is_none());

        // Nothing listening at all.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = listener.local_addr().unwrap();
        drop(listener);
        let client = IngestClient::new(&Url::parse(&format!("http://{closed}")).unwrap()).unwrap();
        assert!(!client.capabilities().await.parent_harness_id());
    }

    #[tokio::test]
    async fn an_envelope_without_a_cross_harness_parent_is_never_probed_for() {
        let (server, client) = ingest_answering_ping(Some(r#"{"status":"ok"}"#)).await;
        assert!(client.fit_session(None).await.is_none());
        let plain = SessionEnvelope::from_attribution(
            &attribution_with_parent(Some("sid-parent")),
            "",
            "local:test",
        );
        let got = client.fit_session(Some(plain)).await.unwrap();
        assert_eq!(got.parent_harness_session_id.as_deref(), Some("sid-parent"));
        assert_eq!(ping_count(&server).await, 0);
    }

    #[test]
    fn the_exit_note_counts_what_was_withheld() {
        assert!(withheld_parent_note(1).contains("1 turn was filed"));
        assert!(withheld_parent_note(2).contains("2 turns were filed"));
    }

    #[test]
    fn a_base_url_with_a_trailing_path_still_resolves_to_the_ingest_route() {
        let client =
            IngestClient::new(&Url::parse("http://127.0.0.1:8090/base/").unwrap()).unwrap();
        assert_eq!(
            client.endpoint().as_str(),
            "http://127.0.0.1:8090/v1/ingest"
        );
    }
}
