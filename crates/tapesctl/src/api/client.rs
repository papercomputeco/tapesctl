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
//! contract that already has one home — and it would be thrown away when the
//! generated client lands in Track 4. Worse, a typed client silently *drops*
//! fields it does not know: a server that grows a column would return it, and a
//! partial model would eat it before the user ever saw it. Passing the document
//! through means `tapesctl sessions get` shows exactly what the API said, today
//! and after the next server release.
//!
//! What *is* modelled here is the request side — the parameters, their names,
//! and their defaults — because that is where a client can be wrong in a way the
//! server cannot correct.
//!
//! # No auth
//!
//! The tapes read API carries no authentication of its own. Tenancy is settled
//! by the deployment before a request reaches the process, and the header that
//! once let a caller name its own tenant was removed precisely because nothing
//! verified it. A standalone client sends no credential; a Paper deployment's
//! gateway adds its own on the way through.

use serde_json::Value;
use snafu::{OptionExt, ResultExt};
use url::Url;

use crate::error::{Error, Result, error};

/// Server-side ceiling on `limit`. A larger request is silently clamped rather
/// than rejected, so a caller asking for more must still follow `next_cursor`.
pub const MAX_LIMIT: u64 = 200;

/// Default number of span search hits, matching both the server's default and
/// the `tapes search` flag this port reproduces.
///
/// There is no server-side ceiling on `top_k` — the handler passes it straight
/// through — so this is a default, not a clamp.
pub const DEFAULT_SEARCH_TOP_K: u64 = 5;
/// The cassette discovery route.
pub const CASSETTES_PATH: &str = "/v1/cassettes";

/// The outcome of a conditional fetch of a cassette's OpenAPI document.
#[derive(Debug, Clone)]
pub enum SpecFetch {
    /// The server matched our `If-None-Match` and sent no body; the cached copy
    /// is still current.
    Unchanged,
    /// A document, and the validator to revalidate it with next time.
    Fetched {
        /// The OpenAPI document, verbatim.
        document: Value,
        /// The response `ETag`, when the server sent one.
        etag: Option<String>,
    },
}

/// Remove the empty query `query_pairs_mut` leaves behind when no pair was
/// appended.
///
/// `url.query_pairs_mut()` sets the query to `Some("")` the moment it is called,
/// so a request with every parameter unset would go out as `/v1/sessions?`.
/// Servers ignore it, but it means the same request has two spellings — which
/// shows up in logs, in cached URLs, and in any test that compares them.
/// Replace the `{name}` placeholders in one path segment.
///
/// The result is pushed through `path_segments_mut`, which percent-encodes the
/// whole segment — so a value containing a slash stays one segment rather than
/// addressing a different route.
fn substitute(segment: &str, path_params: &[(String, String)]) -> String {
    let mut rendered = segment.to_owned();
    for (name, value) in path_params {
        rendered = rendered.replace(&format!("{{{name}}}"), value);
    }
    rendered
}

fn drop_empty_query(url: &mut Url) {
    if url.query() == Some("") {
        url.set_query(None);
    }
}

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

/// One call against a cassette route, assembled from that cassette's spec.
#[derive(Debug, Default, Clone)]
pub struct Call<'a> {
    /// The HTTP verb, uppercased.
    pub method: &'a str,
    /// The public path template, `{name}` placeholders included.
    pub path: &'a str,
    /// Values for those placeholders, by placeholder name.
    pub path_params: Vec<(String, String)>,
    /// Query parameters, under their wire names.
    pub query: Vec<(String, String)>,
    /// Header parameters, under their wire names.
    pub headers: Vec<(String, String)>,
    /// A JSON request body, when the operation takes one.
    pub body: Option<String>,
}

/// A client for one tapes API server.
///
/// `http` is `None` only if the no-redirect client could not be built at all —
/// in which case every request errors, rather than any fallback silently
/// following redirects.
#[derive(Debug, Clone)]
pub struct ApiClient {
    http: Option<reqwest::Client>,
    base: Url,
}

impl ApiClient {
    /// Build a client against `base`.
    #[must_use]
    pub fn new(base: Url) -> Self {
        // Redirects are refused, not followed: this client speaks to exactly
        // the server the user configured, and both the discovery document and
        // a cassette's own spec are data that must not be able to steer a
        // request — least of all one carrying a user-provided body — onto
        // another host. The tapes API never redirects, so a 3xx here is
        // always either a misconfiguration or an attempt to move the client.
        // There is deliberately NO fallback client: if this build fails
        // (which a redirect policy alone cannot cause in practice), every
        // request errors instead of any default client quietly following
        // redirects.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok();
        Self { http, base }
    }

    /// The one HTTP client, or a hard error — never a redirect-following one.
    fn http(&self) -> Result<&reqwest::Client> {
        self.http.as_ref().context(error::ClientInitSnafu)
    }

    /// Refuse a response that is a redirect or that a redirect produced.
    ///
    /// The primary defence is the client's `Policy::none`; this backstop makes
    /// the property visible per-response: any 3xx errors out, and the origin
    /// that answered must be the origin the user configured.
    fn refuse_moved(&self, response: &reqwest::Response) -> Result<()> {
        snafu::ensure!(
            !response.status().is_redirection(),
            error::ApiContractSnafu {
                detail: "the server answered with a redirect; this client does not follow them",
            }
        );
        snafu::ensure!(
            response.url().origin() == self.base.origin(),
            error::ApiContractSnafu {
                detail: "the response came from a different origin than the configured server",
            }
        );
        Ok(())
    }

    /// The base URL, for logging.
    #[must_use]
    pub fn base(&self) -> &Url {
        &self.base
    }

    /// Join an absolute API path onto the base.
    fn url(&self, path: &str) -> Result<Url> {
        self.base.join(path).context(error::ApiUrlSnafu)
    }

    /// Build `<base>/v1/sessions/<id>[/<tail>]`.
    ///
    /// The id goes through `path_segments_mut`, which percent-encodes it as a
    /// single segment — a user-supplied id can never break out of the path and
    /// address a different route.
    fn session_url(&self, id: &str, tail: Option<&str>) -> Result<Url> {
        let mut url = self.url("/v1/sessions")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| error::NotABaseSnafu.build())?;
            segments.push(id);
            if let Some(tail) = tail {
                segments.push(tail);
            }
        }
        Ok(url)
    }

    /// Build `<base>/v1/traces/<trace_id>[/spans/<span_id>]`.
    fn trace_url(&self, trace_id: &str, span_id: Option<&str>) -> Result<Url> {
        let mut url = self.url("/v1/traces")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| error::NotABaseSnafu.build())?;
            segments.push(trace_id);
            if let Some(span_id) = span_id {
                segments.push("spans");
                segments.push(span_id);
            }
        }
        Ok(url)
    }

    /// The URL `list_sessions` will call. Split out so the parameter names and
    /// the omit-when-unset rule can be asserted without a server.
    fn sessions_list_url(&self, params: &SessionListParams<'_>) -> Result<Url> {
        let mut url = self.url("/v1/sessions")?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(limit) = params.limit {
                query.append_pair("limit", &limit.to_string());
            }
            if let Some(cursor) = params.cursor {
                query.append_pair("cursor", cursor);
            }
            if let Some(sort) = params.sort {
                query.append_pair("sort", sort);
            }
            if let Some(direction) = params.direction {
                query.append_pair("direction", direction);
            }
            if let Some(since) = params.since {
                query.append_pair("since", since);
            }
            if let Some(until) = params.until {
                query.append_pair("until", until);
            }
            if let Some(subject) = params.auth_subject {
                query.append_pair("auth_subject", subject);
            }
        }
        drop_empty_query(&mut url);
        Ok(url)
    }

    /// `GET /v1/sessions`
    pub async fn list_sessions(&self, params: &SessionListParams<'_>) -> Result<Value> {
        let url = self.sessions_list_url(params)?;
        self.get_json(url).await
    }

    /// `GET /v1/sessions/{id}`
    pub async fn get_session(&self, id: &str) -> Result<Value> {
        let url = self.session_url(id, None)?;
        self.get_json(url).await
    }

    /// `GET /v1/sessions/{id}/traces` — the derived span read model, which is
    /// exactly what the console renders.
    pub async fn get_session_traces(
        &self,
        id: &str,
        payload: Option<PayloadDetail>,
    ) -> Result<Value> {
        let mut url = self.session_url(id, Some("traces"))?;
        if let Some(payload) = payload {
            url.query_pairs_mut()
                .append_pair("payload", payload.as_str());
        }
        self.get_json(url).await
    }

    /// `GET /v1/sessions/{id}/raw_turns` — the wire-turn metadata behind a
    /// derivation.
    pub async fn list_session_raw_turns(&self, id: &str) -> Result<Value> {
        let url = self.session_url(id, Some("raw_turns"))?;
        self.get_json(url).await
    }

    /// `GET /v1/traces?session_id=...` — trace summaries for one session.
    pub async fn list_traces(&self, session_id: &str) -> Result<Value> {
        let mut url = self.url("/v1/traces")?;
        url.query_pairs_mut().append_pair("session_id", session_id);
        self.get_json(url).await
    }

    /// `GET /v1/traces/{trace_id}`
    pub async fn get_trace(&self, trace_id: &str, payload: Option<PayloadDetail>) -> Result<Value> {
        let mut url = self.trace_url(trace_id, None)?;
        if let Some(payload) = payload {
            url.query_pairs_mut()
                .append_pair("payload", payload.as_str());
        }
        self.get_json(url).await
    }

    /// `GET /v1/traces/{trace_id}/spans/{span_id}`
    pub async fn get_span(&self, trace_id: &str, span_id: &str) -> Result<Value> {
        let url = self.trace_url(trace_id, Some(span_id))?;
        self.get_json(url).await
    }

    /// The URL `search_spans` will call.
    ///
    /// Both parameters are always sent, unlike [`Self::sessions_list_url`]'s
    /// omit-when-unset rule: the command that calls this always has a `top_k`
    /// (its flag carries the default), and the Go command it ports sets both
    /// unconditionally. Sending them keeps one request spelling rather than two.
    fn search_spans_url(&self, query: &str, top_k: u64) -> Result<Url> {
        let mut url = self.url("/v1/search/spans")?;
        url.query_pairs_mut()
            .append_pair("query", query)
            .append_pair("top_k", &top_k.to_string());
        Ok(url)
    }

    /// `GET /v1/search/spans` — semantic search over span embeddings.
    ///
    /// Answers 503 when the deployment has no embedder or no span embedding
    /// store, and 503 again when the store exists but no embed pass has run —
    /// both surface as [`Error::ApiStatus`] carrying the server's message,
    /// which names which of the two it is.
    pub async fn search_spans(&self, query: &str, top_k: u64) -> Result<Value> {
        let url = self.search_spans_url(query, top_k)?;
        self.get_json(url).await
    }

    /// `GET /v1/sessions/{id}/export` — the streaming export bundle.
    ///
    /// Returns the live response rather than a buffered body: an export can be
    /// far larger than a session's working set, and there is no reason to hold
    /// it in memory on the way to a file.
    pub async fn export_session(
        &self,
        id: &str,
        detail: Option<ExportDetail>,
    ) -> Result<reqwest::Response> {
        let mut url = self.session_url(id, Some("export"))?;
        if let Some(detail) = detail {
            url.query_pairs_mut().append_pair("detail", detail.as_str());
        }
        self.get_stream(url).await
    }

    /// `GET /v1/cassettes` — the cassette discovery document.
    ///
    /// Returned raw for the same reason every other read is: discovery grows
    /// fields (`problems` and `contract_version` are both younger than the
    /// route) and a partial model would eat them.
    pub async fn list_cassettes(&self) -> Result<Value> {
        let url = self.url(CASSETTES_PATH)?;
        self.get_json(url).await
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
        // Discovery is data, not authority. A single leading slash keeps the
        // request on the server that served discovery; `//host/path` is a
        // protocol-RELATIVE reference that Url::join resolves onto a
        // different host entirely, so it is rejected up front — and the
        // built URL's origin is checked against the base as the backstop for
        // any other authority-changing join.
        if !path.starts_with('/') || path.starts_with("//") {
            return error::CassetteSpecPathSnafu {
                path: path.to_owned(),
            }
            .fail();
        }
        let url = self.url(path)?;
        if url.origin() != self.base.origin() {
            return error::CassetteSpecPathSnafu {
                path: path.to_owned(),
            }
            .fail();
        }

        let mut request = self.http()?.get(url.clone());
        if let Some(etag) = etag {
            request = request.header(http::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.context(error::ApiSendSnafu)?;
        self.refuse_moved(&response)?;

        // The pre-flight guards validate the URL this client BUILT; a 30x
        // from the server can still walk the request elsewhere, and reqwest
        // follows redirects by default. The origin that ultimately answered
        // must be the origin the user configured — a spec served from
        // anywhere else is refused unread. (Nothing sensitive left with the
        // redirected request: this fetch carries no credentials.)
        if response.url().origin() != self.base.origin() {
            return error::CassetteSpecPathSnafu {
                path: path.to_owned(),
            }
            .fail();
        }

        if response.status() == http::StatusCode::NOT_MODIFIED {
            return Ok(SpecFetch::Unchanged);
        }
        let etag = response
            .headers()
            .get(http::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let document = Self::decode_json(response, &url).await?;

        Ok(SpecFetch::Fetched { document, etag })
    }

    /// Make one call described by a cassette's OpenAPI document.
    ///
    /// The verb, path and parameter names all come from the server's own spec
    /// rather than from anything compiled in here, which is the whole point of
    /// the generated surface — see [`crate::cassette`].
    pub async fn call(&self, call: &Call<'_>) -> Result<Value> {
        let url = self.call_url(call)?;
        let method = reqwest::Method::from_bytes(call.method.as_bytes()).map_err(|_| {
            error::CassetteMethodSnafu {
                method: call.method,
            }
            .build()
        })?;

        let mut request = self.http()?.request(method, url.clone());
        for (name, value) in &call.headers {
            request = request.header(name, value);
        }
        if let Some(body) = &call.body {
            request = request
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(body.clone());
        }

        let response = request.send().await.context(error::ApiSendSnafu)?;
        // Cassette calls carry user-provided bodies and headers, so of all
        // requests this client makes these are the ones a redirect must never
        // be able to move.
        self.refuse_moved(&response)?;
        Self::decode_json(response, &url).await
    }

    /// Build the URL for a cassette call.
    ///
    /// Path parameters are substituted into their segment and the segment is
    /// then pushed through `path_segments_mut`, which percent-encodes it whole.
    /// A value containing a slash therefore stays one segment instead of
    /// addressing a different route.
    fn call_url(&self, call: &Call<'_>) -> Result<Url> {
        let mut url = self.url("/")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| error::NotABaseSnafu.build())?;
            segments.clear();
            for segment in call.path.split('/').filter(|s| !s.is_empty()) {
                segments.push(&substitute(segment, &call.path_params));
            }
        }
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in &call.query {
                query.append_pair(name, value);
            }
        }
        drop_empty_query(&mut url);
        Ok(url)
    }

    /// `POST /v1/admin/seed/demo` — populate a server with demo sessions.
    pub async fn seed_demo(&self) -> Result<Value> {
        let url = self.url("/v1/admin/seed/demo")?;
        let response = self
            .http()?
            .post(url.clone())
            .header(http::header::CONTENT_TYPE, "application/json")
            // The server's request schema has one optional field, and the only
            // value it ever accepted for it is now rejected. An empty object is
            // the whole request.
            .body("{}")
            .send()
            .await
            .context(error::ApiSendSnafu)?;
        Self::decode_json(response, &url).await
    }

    async fn get_json(&self, url: Url) -> Result<Value> {
        let response = self
            .http()?
            .get(url.clone())
            .send()
            .await
            .context(error::ApiSendSnafu)?;
        self.refuse_moved(&response)?;
        Self::decode_json(response, &url).await
    }

    async fn get_stream(&self, url: Url) -> Result<reqwest::Response> {
        let response = self
            .http()?
            .get(url.clone())
            .send()
            .await
            .context(error::ApiSendSnafu)?;
        self.refuse_moved(&response)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiStatus {
                status: status.as_u16(),
                endpoint: url.to_string(),
                body,
            });
        }
        Ok(response)
    }

    async fn decode_json(response: reqwest::Response, url: &Url) -> Result<Value> {
        let status = response.status();
        let body = response.bytes().await.context(error::ApiSendSnafu)?;
        if !status.is_success() {
            // Every tapes error body is `{"error": "..."}`; surfacing it beats
            // the bare status, which never names the offending parameter.
            return Err(Error::ApiStatus {
                status: status.as_u16(),
                endpoint: url.to_string(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        // A successful response with no body is a real answer, not a decode
        // failure: cassette routes are free to return 204, and the core seed
        // route is simply the only one today that never does.
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).context(error::ApiDecodeSnafu)
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
        let url = base("http://127.0.0.1:8081")
            .sessions_list_url(&SessionListParams::default())
            .unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:8081/v1/sessions");
    }

    #[test]
    fn set_parameters_are_appended_under_their_documented_names() {
        let url = base("http://127.0.0.1:8081")
            .sessions_list_url(&SessionListParams {
                limit: Some(25),
                since: Some("2026-07-01T00:00:00Z"),
                ..Default::default()
            })
            .unwrap();
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
        let client = base("http://127.0.0.1:8081");
        let url = client
            .session_url("../admin/seed/demo", Some("traces"))
            .unwrap();
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
    }

    #[test]
    fn the_span_route_is_nested_under_its_trace() {
        let client = base("http://127.0.0.1:8081");
        assert_eq!(
            client.trace_url("t-1", Some("s-1")).unwrap().as_str(),
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
        let url = base("http://127.0.0.1:8081")
            .search_spans_url("gum glow charm", 10)
            .unwrap();
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
