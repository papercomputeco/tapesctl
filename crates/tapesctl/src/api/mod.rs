//! `tapesctl <resource> <method>` — read access to the core tapes data model.
//!
//! Three resources, mapped onto the routes that actually exist:
//!
//! | command | route |
//! |---|---|
//! | `sessions list` | `GET /v1/sessions` |
//! | `sessions get <id>` | `GET /v1/sessions/{id}` |
//! | `sessions traces <id>` | `GET /v1/sessions/{id}/traces` |
//! | `sessions raw-turns <id>` | `GET /v1/sessions/{id}/raw_turns` |
//! | `traces list <session-id>` | `GET /v1/traces?session_id=` |
//! | `traces get <trace-id>` | `GET /v1/traces/{trace_id}` |
//! | `spans list <trace-id>` | `GET /v1/traces/{trace_id}`, projected to `spans` |
//! | `spans get <trace-id> <span-id>` | `GET /v1/traces/{trace_id}/spans/{span_id}` |
//!
//! `spans list` is the one entry that is not a route of its own: the API has no
//! standalone span collection — spans exist only inside a trace — so the command
//! fetches the trace and prints its `spans` array. Naming that projection here
//! is better than pretending the resource is flat, and better than omitting the
//! method and leaving `spans` with a single verb.
//!
//! # Output
//!
//! Every command has a human view (see [`view`]) and `--json` for the raw
//! document. `sessions traces` and `sessions raw-turns` stay JSON: they are the
//! console's own documents. See [`client`] for why responses are not decoded
//! through the shared models: a model only carries the fields its build knew.
//!
//! # Requests
//!
//! The routes in the table above are not hand-built, and neither are the
//! parameters: each command fills in the vendored contract's own `*Params`
//! struct and resolves the operation by id, so a misspelled parameter is a
//! compile error rather than a request the server has to refuse.

pub mod client;
pub mod contract;
pub mod view;

use serde_json::Value;
use snafu::{OptionExt, ResultExt};
use tapes_client::core::models::params::ContractParams;
use tapes_client::core::models::{
    PayloadDetail, RawTurnListParams, SessionListParams, SessionTracesParams, TraceParams,
};
use url::Url;

use crate::cli::{ApiArgs, SessionsCommand, SpansCommand, TracesCommand};
use crate::error::{Result, error};
use crate::render::Theme;
use client::{ApiClient, connect, narrow};
use contract::ops;

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

/// Resolve the API base URL from arguments and the environment.
pub fn resolve_client(args: &ApiArgs) -> Result<ApiClient> {
    let raw = args
        .api_url
        .as_deref()
        .context(error::MissingTapesUrlSnafu)?;
    Ok(connect(Url::parse(raw).context(error::TapesUrlSnafu)?))
}

/// Resolve `--payload` into the grain the contract declares.
///
/// Refused here rather than at the server, which is what makes an unknown
/// grain cost no round trip — see [`client::parse_grain`].
fn payload_of(raw: Option<&str>) -> Result<Option<PayloadDetail>> {
    match raw {
        Some(raw) => client::parse_grain(raw)
            .map(Some)
            .context(error::InvalidPayloadDetailSnafu {
                payload: raw.to_owned(),
            }),
        None => Ok(None),
    }
}

/// Split repeatable `--filter key=value` flags into wire pairs.
///
/// Only the flag's own grammar is checked — there must be a `=`, with a
/// non-empty key before it. The key is data: cassettes claim filter params on
/// the sessions listing at runtime, so which names mean anything is decided
/// by the deployment at request time, and validating names here would only
/// make this binary disagree with the server it talks to. Refusing the
/// malformed spelling here rather than sending it costs no round trip and
/// names the expected shape.
fn parse_filters(flags: &[String]) -> Result<Vec<(String, String)>> {
    flags
        .iter()
        .map(|flag| {
            flag.split_once('=')
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .context(error::InvalidFilterFlagSnafu { flag: flag.clone() })
        })
        .collect()
}

/// Print a JSON document the way every read command does.
pub fn print_json(value: &serde_json::Value) -> Result<()> {
    let rendered = serde_json::to_string_pretty(value).context(error::RenderJsonSnafu)?;
    println!("{rendered}");
    Ok(())
}

/// Dispatch `tapesctl sessions <method>`.
pub async fn sessions(command: SessionsCommand) -> Result<()> {
    match command {
        SessionsCommand::List(args) => {
            let client = resolve_client(&args.api)?;
            let claimed = parse_filters(&args.filter)?;
            let mut values = SessionListParams {
                limit: args.limit.map(narrow),
                cursor: args.cursor,
                sort: args.sort.clone(),
                since: args.since,
                until: args.until,
                harness_id: args.harness_id,
                harness_session_id: args.harness_session_id,
                auth_subject: args.auth_subject,
                ..Default::default()
            }
            .values();
            // `--direction` stays a free-text flag rather than becoming the
            // contract's closed enum, so an unrecognized value is still the
            // server's 400 in the server's words. Validating it here would be
            // a better message for a different command than the one that
            // shipped.
            if let Some(direction) = args.direction {
                values.push(("direction", direction));
            }
            // Claimed pairs ride the sealed method's own channel: appended to
            // the query after the declared parameters, verbatim and in order,
            // with the response passed through untouched. No client-side
            // filtering — server-side fail-open governs what a key means.
            let value: Value = client
                .call_with_claimed(ops::LIST_SESSIONS, values, &claimed)
                .await?;
            if args.json {
                print_json(&value)
            } else {
                print!(
                    "{}",
                    view::sessions(&value, args.sort.as_deref(), &Theme::detect(), now())
                );
                Ok(())
            }
        }
        SessionsCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let value: Value = client.call(ops::GET_SESSION, vec![("id", args.id)]).await?;
            if args.json {
                print_json(&value)
            } else {
                print!("{}", view::session(&value, &Theme::detect(), now()));
                Ok(())
            }
        }
        SessionsCommand::Traces(args) => {
            let client = resolve_client(&args.api)?;
            let payload = payload_of(args.payload.as_deref())?;
            let mut values = SessionTracesParams {
                payload,
                limit: args.limit,
                cursor: args.cursor,
            }
            .values();
            values.push(("id", args.id));
            let value: Value = client.call(ops::GET_SESSION_TRACES, values).await?;
            print_json(&value)
        }
        SessionsCommand::RawTurns(args) => {
            let client = resolve_client(&args.api)?;
            let mut values = RawTurnListParams {
                limit: args.limit,
                cursor: args.cursor,
            }
            .values();
            values.push(("id", args.id));
            let value: Value = client.call(ops::LIST_RAW_TURNS, values).await?;
            print_json(&value)
        }
    }
}

/// Dispatch `tapesctl traces <method>`.
pub async fn traces(command: TracesCommand) -> Result<()> {
    match command {
        TracesCommand::List(args) => {
            let client = resolve_client(&args.api)?;
            let value: Value = client
                .call(ops::LIST_TRACES, vec![("session_id", args.session_id)])
                .await?;
            if args.json {
                print_json(&value)
            } else {
                print!("{}", view::traces(&value, &Theme::detect(), now()));
                Ok(())
            }
        }
        TracesCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let payload = payload_of(args.payload.as_deref())?;
            let mut values = TraceParams {
                payload,
                limit: args.limit,
                cursor: args.cursor,
            }
            .values();
            values.push(("trace_id", args.trace_id.clone()));
            let value: Value = client.call(ops::GET_TRACE, values).await?;
            if args.json {
                print_json(&value)
            } else {
                print!("{}", view::trace(&value, &Theme::detect(), now()));
                // The server pages a trace's spans; a page that is not the
                // last says so, the same way a record view names the command
                // to run next.
                if let Some(cursor) = next_cursor(&value) {
                    println!(
                        "more  {}",
                        next_page_command(
                            &args.api,
                            &args.trace_id,
                            args.payload.as_deref(),
                            args.limit,
                            cursor
                        )
                    );
                }
                Ok(())
            }
        }
    }
}

/// The cursor a paged document ends with, when it is not the last page.
/// Absent, `null` and `""` are the three spellings of "no more pages".
fn next_cursor(value: &Value) -> Option<&str> {
    value
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
}

/// The `traces get` invocation that fetches the page after `cursor`: the same
/// server, payload grain and page size as the call that printed it, so the
/// next page continues the same request rather than a different one.
fn next_page_command(
    api: &ApiArgs,
    trace_id: &str,
    payload: Option<&str>,
    limit: Option<u32>,
    cursor: &str,
) -> String {
    let mut command = String::from("tapesctl traces get");
    if let Some(url) = api.api_url.as_deref() {
        command.push_str(&format!(" --api-url {url}"));
    }
    command.push(' ');
    command.push_str(trace_id);
    if let Some(payload) = payload {
        command.push_str(&format!(" --payload {payload}"));
    }
    if let Some(limit) = limit {
        command.push_str(&format!(" --limit {limit}"));
    }
    command.push_str(&format!(" --cursor {cursor}"));
    command
}

/// `GET /v1/traces/{trace_id}` walked to its last page: the first page's
/// document with every later page's `spans` appended and the cursor removed.
/// The server pages a trace's spans, so a projection of "the trace's spans"
/// must not stop at the first page. A cursor the server serves twice would
/// loop the walk and, if merged, print the same page twice as though the
/// walk had completed, so it is refused before its page is taken.
async fn whole_trace(
    client: &ApiClient,
    trace_id: String,
    payload: Option<PayloadDetail>,
) -> Result<Value> {
    let mut whole: Option<Value> = None;
    let mut cursor: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    loop {
        let mut values = TraceParams {
            payload,
            limit: None,
            cursor: cursor.clone(),
        }
        .values();
        values.push(("trace_id", trace_id.clone()));
        let mut page: Value = client.call(ops::GET_TRACE, values).await?;
        let next = next_cursor(&page).map(str::to_owned);
        if let Some(next) = next.as_deref()
            && seen.iter().any(|prior| prior == next)
        {
            return error::ApiPageRepeatedSnafu {
                endpoint: format!("/v1/traces/{trace_id}"),
                cursor: next.to_owned(),
            }
            .fail();
        }
        if let Some(doc) = whole.as_mut() {
            let more = page
                .get_mut("spans")
                .and_then(Value::as_array_mut)
                .map(std::mem::take)
                .unwrap_or_default();
            if let Some(spans) = doc.get_mut("spans").and_then(Value::as_array_mut) {
                spans.extend(more);
            }
        } else {
            whole = Some(page);
        }
        match next {
            Some(next) => {
                seen.push(next.clone());
                cursor = Some(next);
            }
            None => break,
        }
    }
    let mut doc = whole.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if let Some(object) = doc.as_object_mut() {
        object.remove("next_cursor");
    }
    Ok(doc)
}

/// Dispatch `tapesctl spans <method>`.
pub async fn spans(command: SpansCommand) -> Result<()> {
    match command {
        SpansCommand::List(args) => {
            let client = resolve_client(&args.api)?;
            let payload = payload_of(args.payload.as_deref())?;
            let trace = whole_trace(&client, args.trace_id, payload).await?;
            // The trace document nests its spans; a missing key means the server
            // returned a trace with none, which prints as an empty array rather
            // than as an error.
            let spans = trace
                .get("spans")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            if args.json {
                print_json(&spans)
            } else {
                print!("{}", view::spans(&spans, &Theme::detect()));
                Ok(())
            }
        }
        SpansCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let value: Value = client
                .call(
                    ops::GET_SPAN,
                    vec![("trace_id", args.trace_id), ("span_id", args.span_id)],
                )
                .await?;
            if args.json {
                print_json(&value)
            } else {
                print!("{}", view::span(&value, &Theme::detect(), now()));
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::{SessionsListArgs, SpansListArgs};
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_args(url: Option<String>) -> ApiArgs {
        ApiArgs { api_url: url }
    }

    fn sessions_list_args(url: String) -> SessionsListArgs {
        SessionsListArgs {
            api: api_args(Some(url)),
            limit: None,
            cursor: None,
            sort: None,
            direction: None,
            since: None,
            until: None,
            harness_session_id: None,
            harness_id: None,
            auth_subject: None,
            filter: Vec::new(),
            json: false,
        }
    }

    #[tokio::test]
    async fn a_malformed_filter_flag_fails_before_any_request() {
        // No `=` means no pair to send; the refusal happens here, with the
        // expected shape named, rather than as a server round trip.
        let server = MockServer::start().await;
        let mut args = sessions_list_args(server.uri());
        args.filter = vec!["no-equals".to_owned()];
        let result = sessions(SessionsCommand::List(args)).await;

        assert!(result.is_err(), "got: {result:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn a_manually_constructed_missing_url_is_an_error() {
        // Normal CLI parsing supplies the localhost default. This covers the
        // direct library call, which intentionally still refuses an omission.
        assert!(resolve_client(&api_args(None)).is_err());
    }

    #[test]
    fn a_malformed_tapes_url_is_rejected() {
        assert!(resolve_client(&api_args(Some("not a url".to_owned()))).is_err());
    }

    #[tokio::test]
    async fn the_harness_filter_pair_lands_on_the_wire() {
        // The mock only answers when both halves of the pair reach the
        // wire: the server 400s a lone harness param, so a flag that
        // stopped shipping its partner would fail here rather than
        // silently listing everything.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions"))
            .and(query_param(
                "harness_session_id",
                "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            ))
            .and(query_param("harness_id", "claude"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"items":[{"id":"s-1"}],"next_cursor":""}"#),
            )
            .mount(&server)
            .await;

        let mut args = sessions_list_args(server.uri());
        args.harness_session_id = Some("f47ac10b-58cc-4372-a567-0e02b2c3d479".to_owned());
        args.harness_id = Some("claude".to_owned());
        let result = sessions(SessionsCommand::List(args)).await;

        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn an_unset_harness_filter_stays_out_of_the_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions"))
            .and(query_param_is_missing("harness_session_id"))
            .and(query_param_is_missing("harness_id"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"items":[],"next_cursor":""}"#),
            )
            .mount(&server)
            .await;

        let result = sessions(SessionsCommand::List(sessions_list_args(server.uri()))).await;

        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn spans_list_projects_the_traces_span_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"trace":{"trace_id":"t-1"},"spans":[{"span_id":"s-1"},{"span_id":"s-2"}]}"#,
            ))
            .mount(&server)
            .await;

        let result = spans(SpansCommand::List(SpansListArgs {
            api: api_args(Some(server.uri())),
            trace_id: "t-1".to_owned(),
            payload: None,
            json: false,
        }))
        .await;

        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn the_span_projection_walks_every_page_of_the_trace() {
        // Page one ends with a cursor; page two is fetched with exactly that
        // cursor and ends without one. The projection is both pages' spans,
        // in order, with the cursor gone from the document.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .and(query_param_is_missing("cursor"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"trace":{"trace_id":"t-1","span_count":3},"spans":[{"span_id":"s-1"},{"span_id":"s-2"}],"next_cursor":"c-2"}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .and(query_param("cursor", "c-2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"trace":{"trace_id":"t-1","span_count":3},"spans":[{"span_id":"s-3"}]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = resolve_client(&api_args(Some(server.uri()))).unwrap();
        let whole = whole_trace(&client, "t-1".to_owned(), None).await.unwrap();
        let ids: Vec<&str> = whole["spans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["span_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["s-1", "s-2", "s-3"]);
        assert!(
            whole.get("next_cursor").is_none(),
            "the cursor must not survive the walk"
        );
        assert_eq!(whole["trace"]["span_count"], 3);
    }

    #[tokio::test]
    async fn a_repeated_cursor_fails_the_walk_instead_of_duplicating_a_page() {
        // Every page, with or without a cursor, answers with the same cursor:
        // a server that loops. The second request must not be merged and the
        // walk must say it is incomplete rather than print twice the spans.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"trace":{"trace_id":"t-1"},"spans":[{"span_id":"s-1"}],"next_cursor":"again"}"#,
            ))
            .expect(2)
            .mount(&server)
            .await;

        let client = resolve_client(&api_args(Some(server.uri()))).unwrap();
        let err = whole_trace(&client, "t-1".to_owned(), None)
            .await
            .unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("again") && shown.contains("incomplete"),
            "got: {shown}"
        );
    }

    #[test]
    fn the_next_page_command_continues_the_same_request() {
        let api = api_args(Some("http://tapes.example:8081".to_owned()));
        assert_eq!(
            next_page_command(&api, "t-1", Some("preview"), Some(50), "c-2"),
            "tapesctl traces get --api-url http://tapes.example:8081 t-1 --payload preview --limit 50 --cursor c-2"
        );
        assert_eq!(
            next_page_command(&api_args(None), "t-1", None, None, "c-2"),
            "tapesctl traces get t-1 --cursor c-2"
        );
    }

    #[tokio::test]
    async fn a_trace_without_spans_prints_an_empty_array_rather_than_failing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"trace":{"trace_id":"t-1"}}"#),
            )
            .mount(&server)
            .await;

        let result = spans(SpansCommand::List(SpansListArgs {
            api: api_args(Some(server.uri())),
            trace_id: "t-1".to_owned(),
            payload: None,
            json: false,
        }))
        .await;

        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn an_unknown_payload_grain_fails_before_any_request() {
        // No mock is mounted: reaching the server would be the bug.
        let server = MockServer::start().await;
        let result = spans(SpansCommand::List(SpansListArgs {
            api: api_args(Some(server.uri())),
            trace_id: "t-1".to_owned(),
            payload: Some("hologram".to_owned()),
            json: false,
        }))
        .await;

        assert!(result.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
