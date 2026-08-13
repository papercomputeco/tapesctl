//! End-to-end cover for the generated cassette surface.
//!
//! Everything here runs against a mock `/v1/cassettes`, never a live server. The
//! documents are the shapes core actually serves: discovery referencing each
//! spec rather than inlining it, and a per-cassette document whose paths core has
//! already republished onto the public surface.
//!
//! This is one long test on purpose. The cache location is process-global (an
//! environment variable), so splitting the phases into separate `#[test]`s would
//! have them race for it and fail for reasons that have nothing to do with the
//! code under test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::CommandFactory;
use serde_json::json;
use tapes_client::DirectHttp;
use tapesctl::cassette::{cache, command};
use tapesctl::cli::Cli;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The discovery document, with the spec referenced and a digest published —
/// the shape that exists so a client can decide whether a fetch is worth making.
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
            "config": [{"key": "greeting", "type": "string", "required": false, "secret": false}],
            "openapi_path": "/v1/cassettes/hello-world/openapi.json",
            "openapi_status": "fresh",
            "manifest_digest": "sha256:manifest"
        }],
        "problems": [{"subject": "http://sidecar.invalid/openapi", "reason": "kind is required"}]
    })
}

/// The cassette's own document, as core republishes it: public paths, bare
/// operation ids.
fn cassette_spec() -> serde_json::Value {
    json!({
        "openapi": "3.1.0",
        "info": {"title": "Hello World Cassette", "version": "0.0.1"},
        "paths": {
            "/v1/cassettes/hello-world/hello": {
                "get": {
                    "operationId": "getHello",
                    "summary": "Greet, and read back every stored row"
                },
                "post": {
                    "operationId": "createHello",
                    "summary": "Write one row to the hello table",
                    "requestBody": {"required": true}
                }
            },
            "/v1/cassettes/hello-world/hello/{id}": {
                "get": {
                    "operationId": "getHelloRow",
                    "parameters": [
                        {"name": "id", "in": "path", "required": true},
                        {"name": "verbose_rows", "in": "query", "required": false}
                    ]
                }
            }
        }
    })
}

async fn serve_discovery(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/cassettes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/hello-world/openapi.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(cassette_spec())
                .insert_header("ETag", "\"sha256:document\""),
        )
        .mount(server)
        .await;
}

/// The transport the cassette surface is fetched over: the same no-redirect
/// engine the sealed reads use, minus the operation table they need and this
/// surface discovers.
fn client(server: &MockServer) -> DirectHttp {
    DirectHttp::new(Url::parse(&server.uri()).unwrap())
}

/// The cache is keyed by the *parsed* base URL, which is how `http://host:1` and
/// `http://host:1/` end up sharing one entry instead of two.
fn base_of(server: &MockServer) -> String {
    client(server).base().to_string()
}

#[tokio::test]
async fn a_server_s_cassettes_become_commands_and_survive_it_going_away() {
    let cache_dir = tempfile::tempdir().unwrap();
    // SAFETY: this test binary sets the variable once, before anything reads it,
    // and is the only test in it.
    unsafe {
        std::env::set_var(cache::CACHE_DIR_ENV, cache_dir.path());
    }

    let server = MockServer::start().await;
    serve_discovery(&server).await;

    // --- discovery generates the surface ---------------------------------
    let surface = cache::load(&client(&server)).await;
    assert_eq!(
        surface.cassettes.len(),
        1,
        "the cassette should be discovered"
    );
    let cassette = surface.cassette("hello-world").unwrap();
    let methods: Vec<&str> = cassette.methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        methods,
        vec!["create-hello", "get-hello", "get-hello-row"],
        "operation ids become kebab-case methods, in a stable order",
    );

    // --- the `cassettes` noun and its help are the cassette listing -------
    let mut command = command::augment(command::mount(Cli::command(), &surface), &surface);
    let help = command.render_long_help().to_string();
    assert!(
        help.contains(command::NOUN),
        "the top-level help must name the noun the surface hangs off: {help}",
    );
    assert!(
        !help.contains("hello-world"),
        "and must not list the cassettes themselves — that is the noun's job: {help}",
    );

    let listing = command
        .find_subcommand_mut(command::NOUN)
        .expect("the noun is always mounted")
        .render_long_help()
        .to_string();
    assert!(
        listing.contains("hello-world"),
        "the cassette must be listed under the noun: {listing}",
    );
    assert!(
        listing.contains("The smallest API that is still a tapes cassette."),
        "its description comes from discovery: {listing}",
    );

    // --- a generated command parses like a hand-written one ---------------
    let matches = command
        .clone()
        .try_get_matches_from([
            "tapesctl",
            command::NOUN,
            "hello-world",
            "get-hello-row",
            "row-7",
            "--verbose-rows",
            "true",
            "--tapes-url",
            &server.uri(),
        ])
        .expect("the generated command should parse");
    let (_, noun_matches) = matches.subcommand().unwrap();
    let (name, cassette_matches) = noun_matches.subcommand().unwrap();
    assert_eq!(name, "hello-world");

    // --- and the spelling it replaced still works, unlisted ---------------
    let legacy = command
        .clone()
        .try_get_matches_from([
            "tapesctl",
            "hello-world",
            "get-hello-row",
            "row-7",
            "--tapes-url",
            &server.uri(),
        ])
        .expect("the top-level spelling must keep parsing for one release");
    assert_eq!(legacy.subcommand().unwrap().0, "hello-world");

    // --- and it calls the route the spec named ----------------------------
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/hello-world/hello/row-7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "row-7"})))
        .mount(&server)
        .await;
    command::dispatch(&surface, name, cassette_matches)
        .await
        .expect("the generated command should reach its route");

    // --- a required body is demanded before any request -------------------
    assert!(
        command
            .clone()
            .try_get_matches_from([
                "tapesctl",
                command::NOUN,
                "hello-world",
                "create-hello",
                "--tapes-url",
                &server.uri(),
            ])
            .is_err(),
        "createHello declares a required request body",
    );

    // --- a second load inside the window does not touch the network -------
    let before = server.received_requests().await.unwrap().len();
    let again = cache::load(&client(&server)).await;
    assert_eq!(again.cassettes.len(), 1);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        before,
        "a fresh cache must not make a request; --help would pay for it on every run",
    );

    // --- an expired cache revalidates with the ETag it was given ----------
    let mut stale = cache::read(&base_of(&server)).unwrap();
    stale.revalidated_at = 0;
    cache::write(&stale);

    let conditional = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/cassettes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery()))
        .mount(&conditional)
        .await;
    // Only a request carrying the stored validator is answered, so a load that
    // failed to send one would find no mock and fall back instead.
    Mock::given(method("GET"))
        .and(path("/v1/cassettes/hello-world/openapi.json"))
        .and(header("if-none-match", "\"sha256:document\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&conditional)
        .await;

    let mut moved = stale.clone();
    moved.base = base_of(&conditional);
    moved.revalidated_at = 0;
    cache::write(&moved);

    let revalidated = cache::load(&client(&conditional)).await;
    assert_eq!(
        revalidated.cassette("hello-world").unwrap().methods.len(),
        3,
        "a 304 must keep the cached document rather than drop the cassette",
    );

    // --- and an unreachable server falls back to the cache ----------------
    let mut expired = cache::read(&base_of(&server)).unwrap();
    expired.revalidated_at = 0;
    cache::write(&expired);
    drop(server);

    let offline_client = DirectHttp::new(Url::parse(&expired.base).unwrap());
    let offline = cache::load(&offline_client).await;
    assert_eq!(
        offline.cassettes.len(),
        1,
        "a stale surface beats none when the network is the thing that is broken",
    );
}
