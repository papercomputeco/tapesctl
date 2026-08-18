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
//! Every command prints the server's JSON, pretty-printed, and nothing else. See
//! [`client`] for why these particular responses are not decoded through the
//! shared models on the way through: in short, a model can only carry the
//! fields the build it shipped in knew about, and these commands exist to show
//! what the server said.
//!
//! # Requests
//!
//! The routes in the table above are not hand-built, and neither are the
//! parameters: each command fills in the vendored contract's own `*Params`
//! struct and resolves the operation by id, so a misspelled parameter is a
//! compile error rather than a request the server has to refuse.

pub mod client;
pub mod contract;

use serde_json::Value;
use snafu::{OptionExt, ResultExt};
use tapes_client::core::models::params::ContractParams;
use tapes_client::core::models::{
    PayloadDetail, SessionListParams, SessionTracesParams, TraceParams,
};
use url::Url;

use crate::cli::{ApiArgs, SessionsCommand, SpansCommand, TracesCommand};
use crate::error::{Result, error};
use client::{ApiClient, connect, narrow};
use contract::ops;

/// Resolve the API base URL from arguments and the environment.
pub fn resolve_client(args: &ApiArgs) -> Result<ApiClient> {
    let raw = args
        .tapes_url
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
            let mut values = SessionListParams {
                limit: args.limit.map(narrow),
                cursor: args.cursor,
                sort: args.sort,
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
            let value: Value = client.call(ops::LIST_SESSIONS, values).await?;
            print_json(&value)
        }
        SessionsCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let value: Value = client.call(ops::GET_SESSION, vec![("id", args.id)]).await?;
            print_json(&value)
        }
        SessionsCommand::Traces(args) => {
            let client = resolve_client(&args.api)?;
            let payload = payload_of(args.payload.as_deref())?;
            let mut values = SessionTracesParams { payload }.values();
            values.push(("id", args.id));
            let value: Value = client.call(ops::GET_SESSION_TRACES, values).await?;
            print_json(&value)
        }
        SessionsCommand::RawTurns(args) => {
            let client = resolve_client(&args.api)?;
            let value: Value = client
                .call(ops::LIST_RAW_TURNS, vec![("id", args.id)])
                .await?;
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
            print_json(&value)
        }
        TracesCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let payload = payload_of(args.payload.as_deref())?;
            let mut values = TraceParams { payload }.values();
            values.push(("trace_id", args.trace_id));
            let value: Value = client.call(ops::GET_TRACE, values).await?;
            print_json(&value)
        }
    }
}

/// Dispatch `tapesctl spans <method>`.
pub async fn spans(command: SpansCommand) -> Result<()> {
    match command {
        SpansCommand::List(args) => {
            let client = resolve_client(&args.api)?;
            let payload = payload_of(args.payload.as_deref())?;
            let mut values = TraceParams { payload }.values();
            values.push(("trace_id", args.trace_id));
            let trace: Value = client.call(ops::GET_TRACE, values).await?;
            // The trace document nests its spans; a missing key means the server
            // returned a trace with none, which prints as an empty array rather
            // than as an error.
            let spans = trace
                .get("spans")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            print_json(&spans)
        }
        SpansCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let value: Value = client
                .call(
                    ops::GET_SPAN,
                    vec![("trace_id", args.trace_id), ("span_id", args.span_id)],
                )
                .await?;
            print_json(&value)
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
        ApiArgs { tapes_url: url }
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
        }
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
        }))
        .await;

        assert!(result.is_ok(), "got: {result:?}");
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
        }))
        .await;

        assert!(result.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
