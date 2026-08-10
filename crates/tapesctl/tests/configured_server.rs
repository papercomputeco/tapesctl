//! What a configured default server buys, end to end.
//!
//! The flag and the environment variable were always enough to *run* a command.
//! What they were not enough for is the surface a user sees before they run
//! one: cassette commands are discovered from a server, so with no server named
//! `tapesctl --help` lists none — and a help page that silently lists less than
//! the tool can do is worse than an error, because nothing says so.
//!
//! This exercises the whole path a real run takes: read the configuration, let
//! it stand in for the missing flag, discover from the server it names, and put
//! the result in the help. Nothing here is a live server; discovery runs against
//! a mock serving the shapes core actually serves.
//!
//! One long test on purpose, and one test in the binary: both the cache
//! location and the absence of `TAPES_URL` are process-global, so separate
//! `#[test]`s would race for them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use tapesctl::cassette::cache;
use tapesctl::cli::{Cli, Command, SessionsCommand};
use tapesctl::config::Config;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn discovery() -> serde_json::Value {
    json!({
        "contract_version": "v1",
        "cassettes": [{
            "name": "hello-world",
            "version": "0.0.1",
            "display_name": "Hello World",
            "description": "The smallest API that is still a tapes cassette.",
            "route_prefix": "/v1/cassettes/hello-world",
            "depends": {"core": "v1", "views": []},
            "tables": ["hello-world.hello"],
            "config": [],
            "openapi_path": "/v1/cassettes/hello-world/openapi.json",
            "openapi_status": "fresh",
            "manifest_digest": "sha256:manifest"
        }],
        "problems": []
    })
}

fn cassette_spec() -> serde_json::Value {
    json!({
        "openapi": "3.1.0",
        "info": {"title": "Hello World Cassette", "version": "0.0.1"},
        "paths": {
            "/v1/cassettes/hello-world/hello": {
                "get": {"operationId": "getHello", "summary": "Greet"}
            }
        }
    })
}

#[tokio::test]
async fn a_configured_server_is_enough_to_list_and_call_what_it_serves() {
    let cache_dir = tempfile::tempdir().unwrap();
    // SAFETY: this test binary touches the environment once, before anything
    // reads it, and is the only test in it. `TAPES_URL` is removed rather than
    // set because the whole question here is what happens with *no* flag and no
    // environment — a developer who happens to export it would otherwise be
    // testing their own server.
    unsafe {
        std::env::set_var(cache::CACHE_DIR_ENV, cache_dir.path());
        std::env::remove_var("TAPES_URL");
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/hello-world/openapi.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(cassette_spec()))
        .mount(&server)
        .await;

    // --- with nothing configured, help says so and lists nothing ----------
    let bare = ["tapesctl".to_owned()];
    let (unconfigured, surface) = tapesctl::build_parser(&bare, &Config::default()).await;
    assert!(surface.cassettes.is_empty());
    let help = unconfigured.clone().render_long_help().to_string();
    assert!(
        help.contains("No server is configured"),
        "the epilogue must explain the empty list: {help}",
    );
    assert!(
        help.contains("config set tapes-url"),
        "and it must teach the durable fix, not only the per-run ones: {help}",
    );
    assert!(!help.contains("hello-world"), "got: {help}");

    // --- configure one, and the same help lists its cassettes -------------
    let config = Config {
        tapes_url: Some(server.uri()),
    };
    let (configured, surface) = tapesctl::build_parser(&bare, &config).await;
    assert_eq!(
        surface.cassettes.len(),
        1,
        "the configured server should have been discovered from",
    );

    let help = configured.clone().render_long_help().to_string();
    assert!(
        help.contains("hello-world"),
        "the cassette must be listed: {help}",
    );
    assert!(
        help.contains("The smallest API that is still a tapes cassette."),
        "with the description discovery gave it: {help}",
    );
    assert!(
        !help.contains("No server is configured"),
        "a configured server is a server; the line is now a lie: {help}",
    );

    // --- and a command that names no server reaches the configured one ----
    let matches = configured
        .clone()
        .try_get_matches_from(["tapesctl", "sessions", "list"])
        .expect("the command should parse without a server flag");
    let cli = <Cli as clap::FromArgMatches>::from_arg_matches(&matches).unwrap();
    match cli.command {
        Command::Sessions(SessionsCommand::List(args)) => {
            assert_eq!(args.api.tapes_url.as_deref(), Some(server.uri().as_str()));
        }
        other => panic!("got: {other:?}"),
    }

    // --- including a generated cassette method, which dispatches through it
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/hello-world/hello"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"hello": "world"})))
        .mount(&server)
        .await;
    let matches = configured
        .try_get_matches_from(["tapesctl", "hello-world", "get-hello"])
        .expect("the generated command should parse without a server flag");
    let (name, cassette_matches) = matches.subcommand().unwrap();
    tapesctl::cassette::command::dispatch(&surface, name, cassette_matches)
        .await
        .expect("the configured server should be where the call goes");
}
