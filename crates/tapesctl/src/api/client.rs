//! The core read client: `<resource> <method>` against the tapes API.
//!
//! # Why the responses are not re-modelled here
//!
//! Every read returns a [`serde_json::Value`] and every command prints it. That
//! is a deliberate choice, and the same one [`crate::start::ingest`] makes in
//! the other direction: tapesctl ships raw and lets the server own the shape.
//!
//! Hand-writing Rust mirrors of `SessionItem`, `TraceDetail`, `SpanItem` and
//! their nested usage/verdict/link types would be a second, drifting copy of a
//! contract that already has one home. Worse, a typed client silently *drops*
//! fields it does not know: a server that grows a column would return it, and a
//! partial model would eat it before the user ever saw it. Passing the document
//! through means `tapesctl sessions get` shows exactly what the API said, today
//! and after the next server release.
//!
//! # The request side comes from the vendored contract
//!
//! The request side is where a client can be wrong in a way the server cannot
//! correct — and it is no longer hand-written either. Every core method here
//! resolves its operation in [`crate::api::contract`] (the vendored
//! `contracts/tapes-api.yaml`, reduced by the same reducer the cassette
//! surface uses) and assembles a [`Call`] from the contract's verb, path
//! template, and declared parameters. What remains modelled locally is which
//! parameters each command *sends* and their client-side defaults; the routes
//! themselves are the contract's.
//!
//! # No auth
//!
//! The tapes read API carries no authentication of its own. Tenancy is settled
//! by the deployment before a request reaches the process, and the header that
//! once let a caller name its own tenant was removed precisely because nothing
//! verified it. A standalone client sends no credential; a Paper deployment's
//! gateway adds its own on the way through.

use serde_json::Value;
use tapes_cassette_client::DirectHttp;
use url::Url;

use crate::api::contract::{self, ops};
use crate::error::{Result, error};
// Re-exported from the shared cassette crate since the PCC-1104 split, so
// the call sites across ports/ and start/ read exactly as they did when the
// types were defined here.
pub use tapes_cassette_client::{Call, SpecFetch};

/// Server-side ceiling on `limit`. A larger request is silently clamped rather
/// than rejected, so a caller asking for more must still follow `next_cursor`.
pub const MAX_LIMIT: u64 = 200;

/// Default number of span search hits, matching both the server's default and
/// the `tapes search` flag this port reproduces.
///
/// There is no server-side ceiling on `top_k` — the handler passes it straight
/// through — so this is a default, not a clamp.
pub const DEFAULT_SEARCH_TOP_K: u64 = 5;

/// Which grain a session export is written at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportDetail {
    /// One line per trace, with its spans. The default.
    Spans,
    /// One line per trace, without spans or links.
    Traces,
}

impl ExportDetail {
    /// The query value the server expects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spans => "spans",
            Self::Traces => "traces",
        }
    }

    /// Resolve a user-typed value.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "spans" => Ok(Self::Spans),
            "traces" => Ok(Self::Traces),
            other => error::InvalidExportDetailSnafu {
                detail: other.to_owned(),
            }
            .fail(),
        }
    }
}

/// How much of each span payload to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadDetail {
    /// Whole payloads.
    Full,
    /// Payload strings truncated server-side, for a cheap overview.
    Preview,
}

impl PayloadDetail {
    /// The query value the server expects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Preview => "preview",
        }
    }

    /// Resolve a user-typed value.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" => Ok(Self::Full),
            "preview" => Ok(Self::Preview),
            other => error::InvalidPayloadDetailSnafu {
                payload: other.to_owned(),
            }
            .fail(),
        }
    }
}

/// Filters and paging for `GET /v1/sessions`.
///
/// Every field is optional and an unset one is *omitted from the query* rather
/// than sent as a default — so the server's own defaults apply, and this client
/// never has to be updated when one of them changes.
#[derive(Debug, Default, Clone)]
pub struct SessionListParams<'a> {
    /// Page size. Server default 50, ceiling [`MAX_LIMIT`].
    pub limit: Option<u64>,
    /// Opaque page cursor from a previous response's `next_cursor`.
    ///
    /// A cursor is only valid under the sort and direction it was minted with;
    /// changing either invalidates it, which the server answers with a 400.
    pub cursor: Option<&'a str>,
    /// Sort column.
    pub sort: Option<&'a str>,
    /// `asc` or `desc`.
    pub direction: Option<&'a str>,
    /// Lower bound, RFC 3339.
    pub since: Option<&'a str>,
    /// Upper bound, RFC 3339.
    pub until: Option<&'a str>,
    /// Exact-match filter on the acting subject.
    pub auth_subject: Option<&'a str>,
}

impl SessionListParams<'_> {
    /// The wire pairs to send: set parameters only, under the contract's
    /// names. An unset parameter is omitted so the server's defaults apply.
    fn values(&self) -> Vec<(&'static str, String)> {
        let mut values = Vec::new();
        if let Some(limit) = self.limit {
            values.push(("limit", limit.to_string()));
        }
        if let Some(cursor) = self.cursor {
            values.push(("cursor", cursor.to_owned()));
        }
        if let Some(sort) = self.sort {
            values.push(("sort", sort.to_owned()));
        }
        if let Some(direction) = self.direction {
            values.push(("direction", direction.to_owned()));
        }
        if let Some(since) = self.since {
            values.push(("since", since.to_owned()));
        }
        if let Some(until) = self.until {
            values.push(("until", until.to_owned()));
        }
        if let Some(subject) = self.auth_subject {
            values.push(("auth_subject", subject.to_owned()));
        }
        values
    }
}

/// A client for one tapes API server.
///
/// The transport half — the no-redirect HTTP client, the spec-path guards,
/// URL building, and JSON decoding — lives in
/// [`tapes_cassette_client::DirectHttp`] since the PCC-1104 split; this type
/// wraps it and keeps the contract-driven core methods and the CLI's error
/// type. Behaviour is the extraction's, which is to say the pre-extraction
/// behaviour, verbatim.
#[derive(Debug, Clone)]
pub struct ApiClient {
    direct: DirectHttp,
}

impl ApiClient {
    /// Build a client against `base`.
    ///
    /// Redirects are refused, not followed: this client speaks to exactly the
    /// server the user configured. See [`DirectHttp::new`] for the policy and
    /// its rationale.
    #[must_use]
    pub fn new(base: Url) -> Self {
        Self {
            direct: DirectHttp::new(base),
        }
    }

    /// The base URL, for logging.
    #[must_use]
    pub fn base(&self) -> &Url {
        self.direct.base()
    }

    /// Join an absolute API path onto the base.
    ///
    /// Only the tests exercise this directly any more — request URLs are
    /// built inside the shared transport — but it pins the join behaviour a
    /// prefix-carrying base URL relies on.
    #[cfg(test)]
    fn url(&self, path: &str) -> Result<Url> {
        use snafu::ResultExt;
        self.base()
            .join(path)
            .context(crate::error::error::ApiUrlSnafu)
    }

    /// Resolve one core operation in the vendored contract and call it.
    ///
    /// Every hand-written URL builder this client used to carry is this line
    /// now: the verb, the path template, and the parameter routing all come
    /// from `contracts/tapes-api.yaml`.
    async fn call_operation(
        &self,
        operation_id: &str,
        values: Vec<(&str, String)>,
    ) -> Result<Value> {
        let method = contract::core()?.method(operation_id)?;
        self.call(&contract::call_for(method, values)?).await
    }

    /// `listSessions` — `GET /v1/sessions`.
    pub async fn list_sessions(&self, params: &SessionListParams<'_>) -> Result<Value> {
        self.call_operation(ops::LIST_SESSIONS, params.values())
            .await
    }

    /// `getSession` — `GET /v1/sessions/{id}`.
    pub async fn get_session(&self, id: &str) -> Result<Value> {
        self.call_operation(ops::GET_SESSION, vec![("id", id.to_owned())])
            .await
    }

    /// `getSessionTraces` — the derived span read model, which is exactly what
    /// the console renders.
    pub async fn get_session_traces(
        &self,
        id: &str,
        payload: Option<PayloadDetail>,
    ) -> Result<Value> {
        let mut values = vec![("id", id.to_owned())];
        if let Some(payload) = payload {
            values.push(("payload", payload.as_str().to_owned()));
        }
        self.call_operation(ops::GET_SESSION_TRACES, values).await
    }

    /// `listRawTurns` — the wire-turn metadata behind a derivation.
    pub async fn list_session_raw_turns(&self, id: &str) -> Result<Value> {
        self.call_operation(ops::LIST_RAW_TURNS, vec![("id", id.to_owned())])
            .await
    }

    /// `listTraces` — trace summaries for one session.
    pub async fn list_traces(&self, session_id: &str) -> Result<Value> {
        self.call_operation(
            ops::LIST_TRACES,
            vec![("session_id", session_id.to_owned())],
        )
        .await
    }

    /// `getTrace` — one trace with its spans.
    pub async fn get_trace(&self, trace_id: &str, payload: Option<PayloadDetail>) -> Result<Value> {
        let mut values = vec![("trace_id", trace_id.to_owned())];
        if let Some(payload) = payload {
            values.push(("payload", payload.as_str().to_owned()));
        }
        self.call_operation(ops::GET_TRACE, values).await
    }

    /// `getSpan` — one span with full payloads.
    pub async fn get_span(&self, trace_id: &str, span_id: &str) -> Result<Value> {
        self.call_operation(
            ops::GET_SPAN,
            vec![
                ("trace_id", trace_id.to_owned()),
                ("span_id", span_id.to_owned()),
            ],
        )
        .await
    }

    /// `searchSpans` — semantic search over span embeddings.
    ///
    /// Both parameters are always sent, unlike the list's omit-when-unset
    /// rule: the command that calls this always has a `top_k` (its flag
    /// carries the default), and the Go command it ports sets both
    /// unconditionally. Sending them keeps one request spelling rather than
    /// two.
    ///
    /// Answers 503 when the deployment has no embedder or no span embedding
    /// store, and 503 again when the store exists but no embed pass has run —
    /// both surface as [`Error::ApiStatus`] carrying the server's message,
    /// which names which of the two it is.
    pub async fn search_spans(&self, query: &str, top_k: u64) -> Result<Value> {
        self.call_operation(
            ops::SEARCH_SPANS,
            vec![("query", query.to_owned()), ("top_k", top_k.to_string())],
        )
        .await
    }

    /// `exportSession` — the streaming export bundle.
    ///
    /// Returns the live response rather than a buffered body: an export can be
    /// far larger than a session's working set, and there is no reason to hold
    /// it in memory on the way to a file.
    pub async fn export_session(
        &self,
        id: &str,
        detail: Option<ExportDetail>,
    ) -> Result<reqwest::Response> {
        let mut values = vec![("id", id.to_owned())];
        if let Some(detail) = detail {
            values.push(("detail", detail.as_str().to_owned()));
        }
        let method = contract::core()?.method(ops::EXPORT_SESSION)?;
        self.call_stream(&contract::call_for(method, values)?).await
    }

    /// `listCassettes` — the cassette discovery document.
    ///
    /// Returned raw for the same reason every other read is: discovery grows
    /// fields (`problems` and `contract_version` are both younger than the
    /// route) and a partial model would eat them.
    pub async fn list_cassettes(&self) -> Result<Value> {
        self.call_operation(ops::LIST_CASSETTES, Vec::new()).await
    }

    /// `GET /v1/cassettes/{name}/openapi.json` — one cassette's own document.
    ///
    /// `path` is the `openapi_path` discovery published rather than a path this
    /// client builds, so core stays free to move the route. It is required to be
    /// server-relative: `Url::join` treats an absolute URL as a replacement, so
    /// a discovery document naming `http://elsewhere/openapi.json` would
    /// otherwise redirect this fetch off the server the user asked for.
    ///
    /// `etag` is the validator from a previous fetch. The route answers a
    /// matching `If-None-Match` with 304 and an empty body, which is what makes
    /// revalidating a cached surface cheap.
    pub async fn fetch_cassette_spec(&self, path: &str, etag: Option<&str>) -> Result<SpecFetch> {
        Ok(self.direct.fetch_spec(path, etag).await?)
    }

    /// Make one call described by an OpenAPI document — a cassette's, or the
    /// vendored core contract.
    ///
    /// The verb, path and parameter names all come from the document rather
    /// than from anything hand-written here, which is the whole point of both
    /// generated surfaces — see [`crate::cassette`] and
    /// [`crate::api::contract`].
    pub async fn call(&self, call: &Call<'_>) -> Result<Value> {
        Ok(self.direct.execute(call).await?)
    }

    /// Make one described call and hand back the live response for streaming.
    ///
    /// A non-success status is read and surfaced as
    /// [`crate::error::Error::ApiStatus`], so a caller streaming to a file can
    /// never write an error page into it.
    pub async fn call_stream(&self, call: &Call<'_>) -> Result<reqwest::Response> {
        Ok(self.direct.execute_stream(call).await?)
    }

    /// Build the URL for a cassette call.
    ///
    /// Delegates to the shared builder, which substitutes path parameters
    /// into their segment and pushes the segment through `path_segments_mut`
    /// so it is percent-encoded whole. Only the tests exercise this directly
    /// any more — the shared transport builds its own request URLs — but it
    /// pins the encoding behaviour in this crate's own suite.
    #[cfg(test)]
    fn call_url(&self, call: &Call<'_>) -> Result<Url> {
        Ok(tapes_cassette_client::invoke::call_url(self.base(), call)?)
    }

    /// `seedDemo` — populate a server with demo sessions.
    pub async fn seed_demo(&self) -> Result<Value> {
        let method = contract::core()?.method(ops::SEED_DEMO)?;
        let mut call = contract::call_for(method, Vec::new())?;
        // The server's request schema has one optional field, and the only
        // value it ever accepted for it is now rejected. An empty object is
        // the whole request.
        call.body = Some("{}".to_owned());
        self.call(&call).await
    }
}

/// The shared surface cache fetches through this seam. Discovery still goes
/// through the vendored contract's `listCassettes` operation — the same
/// request the pre-extraction cache made — and every failure maps onto the
/// CLI's own error type for display.
impl tapes_cassette_client::SpecTransport for ApiClient {
    type Error = crate::error::Error;

    async fn fetch_discovery(&self) -> Result<Value> {
        self.list_cassettes().await
    }

    async fn fetch_spec(&self, path: &str, etag: Option<&str>) -> Result<SpecFetch> {
        self.fetch_cassette_spec(path, etag).await
    }

    async fn execute(&self, call: &Call<'_>) -> Result<Value> {
        self.call(call).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> ApiClient {
        ApiClient::new(Url::parse(&server.uri()).unwrap())
    }

    fn base(raw: &str) -> ApiClient {
        ApiClient::new(Url::parse(raw).unwrap())
    }

    /// A [`Call`] assembled the way the client methods assemble theirs: from
    /// the vendored contract's reduction of the named operation.
    fn core_call(operation: &str, values: Vec<(&str, String)>) -> Call<'static> {
        let method = contract::core().unwrap().method(operation).unwrap();
        contract::call_for(method, values).unwrap()
    }

    #[tokio::test]
    async fn a_spec_path_may_not_change_the_request_authority() {
        // `//host/path` is protocol-relative: it survives a naive
        // leading-slash check while Url::join moves the request onto a
        // different host. Both the prefix guard and the origin backstop must
        // refuse before anything is sent.
        let client = base("http://tapes.local:8081");
        for path in ["//evil.example/spec.json", "relative/spec.json", ""] {
            let err = client.fetch_cassette_spec(path, None).await.unwrap_err();
            assert!(
                err.to_string().contains("non-relative OpenAPI path"),
                "{path:?} produced the wrong error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_redirected_spec_fetch_may_not_leave_the_configured_origin() {
        // The URL guards validate what this client builds; a 30x can still
        // walk the request onto another host, and reqwest follows it. The
        // answering origin is checked after the fact, so the foreign
        // document is refused unread.
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/spec.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "openapi": "3.1.0"
            })))
            .mount(&elsewhere)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/x/openapi.json"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                format!("{}/spec.json", elsewhere.uri()).as_str(),
            ))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .fetch_cassette_spec("/v1/cassettes/x/openapi.json", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("redirect"), "wrong error: {err}");
        // Nothing left the configured origin: the redirect was refused, not
        // followed and then rejected.
        assert!(
            elsewhere
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the foreign host must never see a request"
        );
    }

    #[test]
    fn unset_parameters_are_omitted_so_the_server_applies_its_own_defaults() {
        // Sending `limit=50` because that happens to be today's default would
        // pin this client to a value the server is free to change. The bare `?`
        // goes too: `query_pairs_mut` leaves one behind, and it would give the
        // same request two spellings.
        let call = core_call(ops::LIST_SESSIONS, SessionListParams::default().values());
        let url = base("http://127.0.0.1:8081").call_url(&call).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:8081/v1/sessions");
    }

    #[test]
    fn set_parameters_are_appended_under_their_documented_names() {
        let params = SessionListParams {
            limit: Some(25),
            since: Some("2026-07-01T00:00:00Z"),
            ..Default::default()
        };
        let call = core_call(ops::LIST_SESSIONS, params.values());
        let url = base("http://127.0.0.1:8081").call_url(&call).unwrap();
        let query = url.query().unwrap();
        assert!(query.contains("limit=25"), "got: {query}");
        assert!(
            query.contains("since=2026-07-01T00%3A00%3A00Z"),
            "the timestamp must be percent-encoded: {query}",
        );
        assert!(!query.contains("cursor"), "got: {query}");
    }

    #[test]
    fn a_session_id_is_encoded_as_one_path_segment() {
        // A raw join would let `../` in an id address a different route.
        let call = core_call(
            ops::GET_SESSION_TRACES,
            vec![("id", "../admin/seed/demo".to_owned())],
        );
        let url = base("http://127.0.0.1:8081").call_url(&call).unwrap();
        assert!(
            url.as_str()
                .starts_with("http://127.0.0.1:8081/v1/sessions/"),
            "got: {url}",
        );
        assert!(!url.path().contains("/admin/"), "got: {url}");
    }

    #[test]
    fn a_base_url_with_a_path_prefix_still_resolves_to_the_api_route() {
        let client = base("http://127.0.0.1:8081/base/");
        assert_eq!(
            client.url("/v1/sessions").unwrap().as_str(),
            "http://127.0.0.1:8081/v1/sessions",
        );
        // The described-call path drops the prefix the same way.
        let call = core_call(ops::LIST_SESSIONS, Vec::new());
        assert_eq!(
            client.call_url(&call).unwrap().as_str(),
            "http://127.0.0.1:8081/v1/sessions",
        );
    }

    #[test]
    fn the_span_route_is_nested_under_its_trace() {
        let call = core_call(
            ops::GET_SPAN,
            vec![
                ("trace_id", "t-1".to_owned()),
                ("span_id", "s-1".to_owned()),
            ],
        );
        assert_eq!(
            base("http://127.0.0.1:8081")
                .call_url(&call)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:8081/v1/traces/t-1/spans/s-1",
        );
    }

    #[test]
    fn detail_and_payload_values_are_the_servers_spelling() {
        assert_eq!(ExportDetail::parse("SPANS").unwrap(), ExportDetail::Spans);
        assert_eq!(ExportDetail::Traces.as_str(), "traces");
        assert_eq!(
            PayloadDetail::parse(" preview ").unwrap(),
            PayloadDetail::Preview,
        );
        assert_eq!(PayloadDetail::Full.as_str(), "full");
    }

    #[test]
    fn an_unknown_detail_is_rejected_before_a_request_is_made() {
        // The server would 400; naming the valid values locally is a better
        // error and costs no round trip.
        let err = ExportDetail::parse("everything").unwrap_err();
        assert!(format!("{err}").contains("everything"), "got: {err}");
    }

    #[tokio::test]
    async fn a_list_response_is_passed_through_verbatim() {
        // Fields this client has never heard of must survive to the user.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"items":[{"id":"s1","a_field_from_the_future":7}],"next_cursor":"abc"}"#,
            ))
            .mount(&server)
            .await;

        let got = client_for(&server)
            .list_sessions(&SessionListParams::default())
            .await
            .unwrap();

        assert_eq!(got["next_cursor"], "abc");
        assert_eq!(got["items"][0]["a_field_from_the_future"], 7);
    }

    #[tokio::test]
    async fn list_parameters_reach_the_server_under_their_documented_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions"))
            .and(query_param("limit", "10"))
            .and(query_param("cursor", "cur"))
            .and(query_param("sort", "started_at"))
            .and(query_param("direction", "asc"))
            .and(query_param("auth_subject", "local:me"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"items":[]}"#))
            .mount(&server)
            .await;

        let got = client_for(&server)
            .list_sessions(&SessionListParams {
                limit: Some(10),
                cursor: Some("cur"),
                sort: Some("started_at"),
                direction: Some("asc"),
                auth_subject: Some("local:me"),
                ..Default::default()
            })
            .await;

        assert!(got.is_ok(), "got: {got:?}");
    }

    #[tokio::test]
    async fn an_error_body_is_surfaced_with_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid cursor"}"#),
            )
            .mount(&server)
            .await;

        let err = client_for(&server)
            .list_sessions(&SessionListParams::default())
            .await
            .unwrap_err();

        let rendered = format!("{err}");
        assert!(rendered.contains("400"), "got: {rendered}");
        assert!(rendered.contains("invalid cursor"), "got: {rendered}");
    }

    #[tokio::test]
    async fn the_session_traces_route_carries_the_payload_grain() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions/s1/traces"))
            .and(query_param("payload", "preview"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"traces":[]}"#))
            .mount(&server)
            .await;

        assert!(
            client_for(&server)
                .get_session_traces("s1", Some(PayloadDetail::Preview))
                .await
                .is_ok(),
        );
    }

    #[tokio::test]
    async fn listing_traces_requires_the_session_id_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces"))
            .and(query_param("session_id", "s1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"items":[]}"#))
            .mount(&server)
            .await;

        assert!(client_for(&server).list_traces("s1").await.is_ok());
    }

    #[test]
    fn a_search_sends_both_parameters_under_the_servers_names() {
        let call = core_call(
            ops::SEARCH_SPANS,
            vec![
                ("query", "gum glow charm".to_owned()),
                ("top_k", "10".to_owned()),
            ],
        );
        let url = base("http://127.0.0.1:8081").call_url(&call).unwrap();
        assert_eq!(url.path(), "/v1/search/spans");
        let query = url.query().unwrap();
        assert!(
            query.contains("query=gum+glow+charm"),
            "the query must be form-encoded: {query}",
        );
        assert!(query.contains("top_k=10"), "got: {query}");
    }

    #[tokio::test]
    async fn a_search_response_is_passed_through_verbatim() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search/spans"))
            .and(query_param("query", "hooks"))
            .and(query_param("top_k", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"query":"hooks","count":1,"results":[{"trace_id":"t-1","a_new_field":1}]}"#,
            ))
            .mount(&server)
            .await;

        let got = client_for(&server).search_spans("hooks", 5).await.unwrap();

        assert_eq!(got["count"], 1);
        assert_eq!(got["results"][0]["a_new_field"], 1);
    }

    #[tokio::test]
    async fn an_unconfigured_span_search_surfaces_the_servers_explanation() {
        // 503 is the "no embedder" / "no embed pass has run" answer, and its
        // body is the only thing that says which — losing it would leave the
        // user with a bare status and no next step.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search/spans"))
            .respond_with(ResponseTemplate::new(503).set_body_string(
                r#"{"error":"span search is not configured: embedder and span embedding store are required"}"#,
            ))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .search_spans("hooks", 5)
            .await
            .unwrap_err();

        let rendered = format!("{err}");
        assert!(rendered.contains("503"), "got: {rendered}");
        assert!(
            rendered.contains("span search is not configured"),
            "got: {rendered}",
        );
    }

    #[tokio::test]
    async fn seeding_posts_an_empty_object() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/admin/seed/demo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"sessions":3,"raw_turns":9,"raw_turns_inserted":9}"#),
            )
            .mount(&server)
            .await;

        let got = client_for(&server).seed_demo().await.unwrap();
        assert_eq!(got["sessions"], 3);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(String::from_utf8(requests[0].body.clone()).unwrap(), "{}");
    }

    #[tokio::test]
    async fn an_export_is_returned_as_a_stream_not_a_buffered_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions/s1/export"))
            .and(query_param("detail", "traces"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"trace\":1}\n"))
            .mount(&server)
            .await;

        let response = client_for(&server)
            .export_session("s1", Some(ExportDetail::Traces))
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(response.text().await.unwrap(), "{\"trace\":1}\n");
    }

    #[tokio::test]
    async fn a_failed_export_reports_the_status_rather_than_writing_an_error_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions/nope/export"))
            .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"not found"}"#))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .export_session("nope", None)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("404"), "got: {err}");
    }
}
