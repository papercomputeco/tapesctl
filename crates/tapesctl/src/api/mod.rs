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
//! [`client`] for why the responses are not re-modelled on the way through: in
//! short, a partial hand-written model silently eats fields the server grows.
//!
//! # Requests
//!
//! The routes in the table above are not hand-built: each command resolves its
//! operation in the vendored core contract and lets [`contract`] assemble the
//! request from it. The table names the routes because they are what a reader
//! greps for, and the contract tests keep the two in agreement.

pub mod client;
pub mod contract;

use snafu::{OptionExt, ResultExt};
use url::Url;

use crate::cli::{ApiArgs, SessionsCommand, SpansCommand, TracesCommand};
use crate::error::{Result, error};
use client::{ApiClient, PayloadDetail, SessionListParams};

/// Resolve the API base URL from arguments and the environment.
pub fn resolve_client(args: &ApiArgs) -> Result<ApiClient> {
    let raw = args
        .tapes_url
        .as_deref()
        .context(error::MissingTapesUrlSnafu)?;
    Ok(ApiClient::new(
        Url::parse(raw).context(error::TapesUrlSnafu)?,
    ))
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
            let value = client
                .list_sessions(&SessionListParams {
                    limit: args.limit,
                    cursor: args.cursor.as_deref(),
                    sort: args.sort.as_deref(),
                    direction: args.direction.as_deref(),
                    since: args.since.as_deref(),
                    until: args.until.as_deref(),
                    auth_subject: args.auth_subject.as_deref(),
                })
                .await?;
            print_json(&value)
        }
        SessionsCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let value = client.get_session(&args.id).await?;
            print_json(&value)
        }
        SessionsCommand::Traces(args) => {
            let client = resolve_client(&args.api)?;
            let payload = args
                .payload
                .as_deref()
                .map(PayloadDetail::parse)
                .transpose()?;
            let value = client.get_session_traces(&args.id, payload).await?;
            print_json(&value)
        }
        SessionsCommand::RawTurns(args) => {
            let client = resolve_client(&args.api)?;
            let value = client.list_session_raw_turns(&args.id).await?;
            print_json(&value)
        }
    }
}

/// Dispatch `tapesctl traces <method>`.
pub async fn traces(command: TracesCommand) -> Result<()> {
    match command {
        TracesCommand::List(args) => {
            let client = resolve_client(&args.api)?;
            let value = client.list_traces(&args.session_id).await?;
            print_json(&value)
        }
        TracesCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let payload = args
                .payload
                .as_deref()
                .map(PayloadDetail::parse)
                .transpose()?;
            let value = client.get_trace(&args.trace_id, payload).await?;
            print_json(&value)
        }
    }
}

/// Dispatch `tapesctl spans <method>`.
pub async fn spans(command: SpansCommand) -> Result<()> {
    match command {
        SpansCommand::List(args) => {
            let client = resolve_client(&args.api)?;
            let payload = args
                .payload
                .as_deref()
                .map(PayloadDetail::parse)
                .transpose()?;
            let trace = client.get_trace(&args.trace_id, payload).await?;
            // The trace document nests its spans; a missing key means the server
            // returned a trace with none, which prints as an empty array rather
            // than as an error.
            let spans = trace
                .get("spans")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
            print_json(&spans)
        }
        SpansCommand::Get(args) => {
            let client = resolve_client(&args.api)?;
            let value = client.get_span(&args.trace_id, &args.span_id).await?;
            print_json(&value)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::SpansListArgs;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_args(url: Option<String>) -> ApiArgs {
        ApiArgs { tapes_url: url }
    }

    #[test]
    fn a_missing_tapes_url_is_an_error_rather_than_a_guessed_host() {
        assert!(resolve_client(&api_args(None)).is_err());
    }

    #[test]
    fn a_malformed_tapes_url_is_rejected() {
        assert!(resolve_client(&api_args(Some("not a url".to_owned()))).is_err());
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
