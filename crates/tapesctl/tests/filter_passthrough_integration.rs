//! The generic claimed-param passthrough, and the discovery-only cassette path.
//!
//! Both tests run the **actual binary** against a mock tapes server: argv in,
//! stdout/stderr/exit code out, never the crate's internals. What they pin is
//! one boundary from two sides:
//!
//! - `--filter key=value` maps to `?key=value` with the param name treated as
//!   **data** — a key some deployment's cassette claims at runtime, never a
//!   name compiled into this binary — and the response is rendered untouched.
//! - The runtime-discovered `cassettes <name> <method>` surface is this
//!   binary's complete access path to any cassette's API. The mock cassette
//!   below is *named* `labels` precisely because this repository compiles in
//!   no such noun: every occurrence of that word in this file is fixture data
//!   or a value typed at the command line.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::process::Output;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Run the real binary against `api_url`, with a private cassette cache and
/// the ambient `TAPES_*` overrides a developer might have exported cleared —
/// these read from the environment by design, and an exported value would
/// change what the test runs.
async fn run_tapesctl(cache_dir: &Path, api_url: &str, args: &[&str]) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tapesctl"));
    command
        .env("TAPESCTL_CACHE_DIR", cache_dir)
        .env("TAPES_API_URL", api_url)
        .env_remove("RUST_LOG")
        .env_remove("TAPES_INGEST_URL")
        .args(args);
    command.output().await.unwrap()
}

#[tokio::test]
async fn filter_flags_pass_claimed_params_through_generically() {
    let cache_dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // The response carries a field this build has never heard of, so the
    // untouched-rendering assertion below cannot pass through a typed model.
    Mock::given(method("GET"))
        .and(path("/v1/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{"id": "s-1", "a_field_from_the_future": 7}],
            "next_cursor": ""
        })))
        .mount(&server)
        .await;

    // `label` below is a value typed at the command line — the name of a
    // param the mock deployment's cassette would claim — not a flag or noun
    // this binary defines.
    let uri = server.uri();
    let out = run_tapesctl(
        cache_dir.path(),
        &uri,
        &[
            "sessions",
            "list",
            "--filter",
            "label=a",
            "--filter",
            "label=b",
            "--json",
            "--api-url",
            &uri,
        ],
    )
    .await;
    assert!(out.status.success(), "tapesctl failed: {out:?}");

    // The mock records exactly what crossed the wire: both pairs, repeated
    // under one key, in the order they were typed, and nothing else added.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "got: {requests:?}");
    assert_eq!(
        requests[0].url.query(),
        Some("label=a&label=b"),
        "the flags must map to repeated query params, verbatim and in order",
    );

    // And the response comes back untouched: no client-side filtering, no
    // model in the way, fields from the future included.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("a_field_from_the_future"),
        "the server's document must be rendered untouched: {stdout}",
    );
    assert!(stdout.contains("s-1"), "got: {stdout}");
}

#[tokio::test]
async fn tapesctl_drives_labels_via_discovery_only() {
    // --- nothing is compiled in: help knows no such noun -------------------
    // Against a deployment serving no cassettes, any occurrence of the word
    // in help output could only have been compiled into the binary.
    let empty_cache = tempfile::tempdir().unwrap();
    let empty = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "contract_version": "v1",
            "cassettes": []
        })))
        .mount(&empty)
        .await;

    let help = run_tapesctl(empty_cache.path(), &empty.uri(), &["--help"]).await;
    assert!(help.status.success(), "got: {help:?}");
    let help_text = String::from_utf8_lossy(&help.stdout).to_lowercase();
    assert!(
        !help_text.contains("label"),
        "no such noun may be compiled into the top-level surface: {help_text}",
    );

    let list_help = run_tapesctl(
        empty_cache.path(),
        &empty.uri(),
        &["sessions", "list", "--help"],
    )
    .await;
    assert!(list_help.status.success(), "got: {list_help:?}");
    let list_help_text = String::from_utf8_lossy(&list_help.stdout);
    assert!(
        list_help_text.contains("--filter"),
        "the generic passthrough is the only filter flag: {list_help_text}",
    );
    assert!(
        !list_help_text.to_lowercase().contains("label"),
        "no claimed-param name may become flag sugar: {list_help_text}",
    );

    // --- and discovery alone serves the whole surface ----------------------
    // A deployment serving a cassette named `labels` (fixture data): the
    // binary learns it at runtime and drives it end to end with zero
    // compiled-in knowledge.
    let cache_dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "contract_version": "v1",
            "cassettes": [{
                "name": "labels",
                "route_prefix": "/v1/cassettes/labels",
                "openapi_path": "/v1/cassettes/labels/openapi.json",
                "openapi_status": "fresh"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/labels/openapi.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "paths": {"/v1/cassettes/labels/labels": {
                "get": {"operationId": "listLabels"}
            }}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/labels/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "labels": [{"name": "urgent", "color": "#ff0000"}]
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let listing = run_tapesctl(
        cache_dir.path(),
        &uri,
        &["cassettes", "--help", "--api-url", &uri],
    )
    .await;
    assert!(listing.status.success(), "got: {listing:?}");
    let listing_text = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing_text.contains("labels"),
        "the discovered cassette must be listed under the noun: {listing_text}",
    );

    let call = run_tapesctl(
        cache_dir.path(),
        &uri,
        &["cassettes", "labels", "list-labels", "--api-url", &uri],
    )
    .await;
    assert!(
        call.status.success(),
        "the generated command failed: {call:?}"
    );
    let call_text = String::from_utf8_lossy(&call.stdout);
    assert!(
        call_text.contains("urgent"),
        "the round trip must print the cassette's own document: {call_text}",
    );

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.url.path() == "/v1/cassettes/labels/labels"),
        "the generated command must call the route the spec named: {requests:?}",
    );
}
