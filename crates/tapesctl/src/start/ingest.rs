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
//! paths drift: the moment tapesctl reduces locally, its rows and paperd's rows
//! are produced by different code, and the parity corpus starts policing a
//! difference that should never have been expressible. Reduction lives in
//! exactly one place — ingest — where a re-derive can revisit it. Shipping raw
//! also means a turn whose reduction fails is recoverable rather than lost.
//!
//! # Wire-shape gotchas
//!
//! Three details of the Go contract are easy to get wrong and produce either a
//! silent misfile or a 400:
//!
//! * `parent_harness_session_id` must be **omitted**, never sent as `""` — the
//!   server rejects the empty string rather than treating it as absent.
//! * `org_id` and `auth_subject` carry no `omitempty` on the Go side, so they
//!   are always emitted, empty string included.
//! * `raw_response` is a Go `[]byte`, which is standard **padded** base64 in
//!   JSON — not base64url, not an array of integers.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;
use serde_json::value::RawValue;
use snafu::ResultExt;
use tapes_harnesses::envelope::TapesAttribution;
use url::Url;

use crate::error::{Error, Result, error};

/// Path of the turn-ingest endpoint, relative to the tapes ingest base URL.
pub const INGEST_PATH: &str = "/v1/ingest";

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
    /// a tapesctl turn carries are produced by the same code paperd runs. The
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
            harness_metadata: (!attribution.metadata.is_empty())
                .then(|| attribution.metadata.clone()),
        }
    }
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

/// A client for one tapes ingest server.
#[derive(Debug, Clone)]
pub struct IngestClient {
    http: reqwest::Client,
    endpoint: Url,
}

impl IngestClient {
    /// Build a client posting to `base` + [`INGEST_PATH`].
    pub fn new(base: &Url) -> Result<Self> {
        let endpoint = base.join(INGEST_PATH).context(error::IngestUrlSnafu)?;
        Ok(Self {
            http: reqwest::Client::new(),
            endpoint,
        })
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
    fn a_base_url_with_a_trailing_path_still_resolves_to_the_ingest_route() {
        let client =
            IngestClient::new(&Url::parse("http://127.0.0.1:8090/base/").unwrap()).unwrap();
        assert_eq!(
            client.endpoint().as_str(),
            "http://127.0.0.1:8090/v1/ingest"
        );
    }
}
