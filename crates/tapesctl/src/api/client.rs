//! The core read client: `<resource> <method>` against the tapes API.
//!
//! # The client is the shared one
//!
//! There used to be an `ApiClient` of this crate's own that wrapped a transport
//! and grew one method per operation. Every one of those methods was the same three lines — resolve
//! the operation, route the values, decode — and the shared crate now ships
//! exactly that, typed, as [`tapes_client::CoreClient`]. So [`ApiClient`] is an
//! alias for it bound to this CLI's transport, and what is left in this module
//! is the part that is genuinely a *command line's*: the flag defaults, and the
//! translation of a user-typed grain into the contract's own enum.
//!
//! # Why the read commands still print `serde_json::Value`
//!
//! The named methods on [`tapes_client::CoreClient`] return the vendored
//! contract's models, and every command that *renders* a response — search,
//! seed, the skill transcript — uses them. The `<resource> <method>` commands
//! do not render: they print the server's document, and a document that had
//! been through a model would be missing whatever fields this build predates.
//! For those, [`tapes_client::CoreClient::call`] is the documented escape
//! hatch, decoding into [`serde_json::Value`] so `tapesctl sessions get` shows
//! exactly what the API said, today and after the next server release.
//!
//! # No auth
//!
//! The tapes read API carries no authentication of its own. Tenancy is settled
//! by the deployment before a request reaches the process, and the header that
//! once let a caller name its own tenant was removed precisely because nothing
//! verified it. A standalone client sends no credential; a Paper deployment's
//! gateway adds its own on the way through.

use serde::de::DeserializeOwned;
use serde_json::Value;
use tapes_client::core::models::params::ContractEnum;
use tapes_client::{CoreClient, DirectHttp};
use url::Url;

/// Default number of span search hits, matching both the server's default and
/// the `tapes search` flag this port reproduces.
///
/// There is no server-side ceiling on `top_k` — the handler passes it straight
/// through — so this is a default, not a clamp.
pub const DEFAULT_SEARCH_TOP_K: u64 = 5;

/// The sealed read surface, bound to one tapes server.
///
/// Redirects are refused, not followed: this client speaks to exactly the
/// server the user configured, and the check runs again on every response so
/// that a 30x cannot walk a request onto another host. See
/// [`tapes_client::DirectHttp`] for the policy and its rationale.
pub type ApiClient = CoreClient<DirectHttp>;

/// Bind the read surface to `base`.
#[must_use]
pub fn connect(base: Url) -> ApiClient {
    CoreClient::new(DirectHttp::new(base))
}

/// Resolve a user-typed value for a parameter whose accepted set the contract
/// closes with an `enum`.
///
/// The contract's own spellings are the only ones accepted, case-folded and
/// trimmed the way this CLI has always accepted them. Refusing here rather than
/// letting the server answer 400 costs no round trip and names the alternatives
/// — see [`crate::error::Error::InvalidPayloadDetail`] and its sibling, whose
/// wording a test holds to [`ContractEnum::VALUES`].
pub fn parse_grain<E: ContractEnum + DeserializeOwned>(raw: &str) -> Option<E> {
    serde_json::from_value(Value::String(raw.trim().to_ascii_lowercase())).ok()
}

/// Narrow a CLI count onto the contract's parameter width.
///
/// The flags are `u64` because that is what they have always parsed, and the
/// contract declares these as 32-bit. Saturating rather than refusing keeps an
/// absurd `--limit` doing what it does today: travelling to a server that
/// clamps it.
#[must_use]
pub fn narrow(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::api::contract::ops;
    use serde_json::json;
    use tapes_client::core::models::params::ContractParams;
    use tapes_client::core::models::{ExportDetail, PayloadDetail, SessionListParams};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> ApiClient {
        connect(Url::parse(&server.uri()).unwrap())
    }

    #[tokio::test]
    async fn a_spec_path_may_not_change_the_request_authority() {
        // `//host/path` is protocol-relative: it survives a naive
        // leading-slash check while Url::join moves the request onto a
        // different host. Both the prefix guard and the origin backstop must
        // refuse before anything is sent.
        let transport = DirectHttp::new(Url::parse("http://tapes.local:8081").unwrap());
        for path in ["//evil.example/spec.json", "relative/spec.json", ""] {
            let err: crate::error::Error =
                transport.fetch_spec(path, None).await.unwrap_err().into();
            assert!(
                err.to_string().contains("non-relative OpenAPI path"),
                "{path:?} produced the wrong error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_redirected_spec_fetch_may_not_leave_the_configured_origin() {
        // The URL guards validate what this client builds; a 30x can still
        // walk the request onto another host. The answering origin is checked
        // after the fact, so the foreign document is refused unread — and the
        // sentence the user sees still names the redirect, now sourced from
        // the shared taxonomy rather than from a transport of our own.
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/spec.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"openapi": "3.1.0"})))
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

        let transport = DirectHttp::new(Url::parse(&server.uri()).unwrap());
        let err: crate::error::Error = transport
            .fetch_spec("/v1/cassettes/x/openapi.json", None)
            .await
            .unwrap_err()
            .into();
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
    fn a_user_typed_grain_resolves_to_the_contracts_own_spelling() {
        assert_eq!(
            parse_grain::<ExportDetail>("SPANS"),
            Some(ExportDetail::Spans)
        );
        assert_eq!(
            parse_grain::<PayloadDetail>(" preview "),
            Some(PayloadDetail::Preview),
        );
        assert_eq!(parse_grain::<PayloadDetail>("hologram"), None);
    }

    #[test]
    fn the_refusal_messages_name_exactly_the_values_the_contract_declares() {
        // The two messages spell their alternatives inline, because a user
        // reading one wants the answer and not a cross-reference. This is what
        // keeps that spelling honest: a contract that grows a grain fails here
        // rather than teaching the user a stale set.
        let payload = crate::error::error::InvalidPayloadDetailSnafu { payload: "x" }
            .build()
            .to_string();
        for value in PayloadDetail::VALUES {
            assert!(payload.contains(value), "{value:?} missing from: {payload}");
        }
        let detail = crate::error::error::InvalidExportDetailSnafu { detail: "x" }
            .build()
            .to_string();
        for value in ExportDetail::VALUES {
            assert!(detail.contains(value), "{value:?} missing from: {detail}");
        }
    }

    #[test]
    fn an_absurd_count_saturates_rather_than_failing_a_command() {
        assert_eq!(narrow(25), 25);
        assert_eq!(narrow(u64::MAX), u32::MAX);
    }

    #[tokio::test]
    async fn a_list_response_is_passed_through_verbatim() {
        // Fields this client has never heard of must survive to the user; it
        // is the whole reason the read commands decode into a Value rather
        // than through a model.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"items":[{"id":"s1","a_field_from_the_future":7}],"next_cursor":"abc"}"#,
            ))
            .mount(&server)
            .await;

        let got: Value = client_for(&server)
            .call(ops::LIST_SESSIONS, SessionListParams::default().values())
            .await
            .unwrap();

        assert_eq!(got["next_cursor"], "abc");
        assert_eq!(got["items"][0]["a_field_from_the_future"], 7);
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

        let err: crate::error::Error = client_for(&server)
            .call::<Value>(ops::LIST_SESSIONS, Vec::new())
            .await
            .unwrap_err()
            .into();

        let rendered = format!("{err}");
        assert!(rendered.contains("400"), "got: {rendered}");
        assert!(rendered.contains("invalid cursor"), "got: {rendered}");
    }

    #[tokio::test]
    async fn a_search_response_is_decoded_through_the_shipped_model() {
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

        let got = client_for(&server)
            .search_spans(&tapes_client::core::models::SearchSpansParams {
                query: "hooks".to_owned(),
                top_k: Some(5),
            })
            .await
            .unwrap();

        // A field the model has never heard of is ignored rather than fatal:
        // an additive server change must not blank a page of results.
        assert_eq!(got.count, 1);
        assert_eq!(got.results[0].trace_id, "t-1");
    }
}
