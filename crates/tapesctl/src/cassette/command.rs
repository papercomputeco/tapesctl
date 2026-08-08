//! Synthesizing clap commands from a cassette surface, and dispatching them.
//!
//! The synthesis itself lives in [`tapes_cassette_client::command`] since the
//! PCC-1104 split. What stays here is what makes a generated command a
//! *tapesctl* command: the `--tapes-url` flag (with its `TAPES_URL` fallback)
//! decorated onto every generated method — mirroring [`crate::cli::ApiArgs`]
//! — and the dispatch that builds this CLI's client from it, executes the
//! resolved call, and prints the server's JSON verbatim, so a cassette
//! command is not visibly a second-class citizen next to `sessions list`.

use clap::{Arg, ArgAction, ArgMatches, Command};
use snafu::{OptionExt, ResultExt};
use url::Url;

use crate::api::client::ApiClient;
use crate::api::print_json;
use crate::cassette::spec::Surface;
use crate::error::{Result, error};
use tapes_cassette_client::command::resolve_invocation;

/// The flag every generated method carries, mirroring [`crate::cli::ApiArgs`].
const TAPES_URL: &str = "tapes-url";

/// The decorator applied to every generated method command: tapesctl's server
/// flag, exactly as the pre-extraction synthesis hard-coded it.
fn with_tapes_url(command: Command) -> Command {
    command.arg(
        Arg::new(TAPES_URL)
            .long(TAPES_URL)
            .env("TAPES_URL")
            .action(ArgAction::Set)
            .value_name("URL")
            .help("Base URL of the tapes server"),
    )
}

/// Add a subcommand for every cassette on the surface.
///
/// Cassette nouns are appended to the static ones rather than replacing them,
/// and a cassette whose name collides with a built-in command is skipped: a
/// server must not be able to redefine what `tapesctl sessions` means on
/// someone's machine.
#[must_use]
pub fn augment(base: Command, surface: &Surface) -> Command {
    tapes_cassette_client::command::augment(base, surface, with_tapes_url)
}

/// The sentence the top-level help always carries about cassette commands.
///
/// Without it the dynamic surface is invisible when it is empty, and an empty
/// list of cassette commands looks exactly like a tool that does not have any —
/// so `tapesctl --help` on a machine with no server configured reads as
/// "cassettes unsupported" rather than "nothing to discover from yet".
const CASSETTES_ARE_DISCOVERED: &str = "Cassette commands are served by your tapes deployment, not built into this \
     binary: they are\ndiscovered from the server and listed above alongside the built-in commands.";

/// Build the top-level help epilogue for one run's discovery result.
///
/// `server` is the URL discovery was pointed at, if any, and `surface` is what
/// came back. The base sentence is unconditional; the second one exists because
/// "no cassette commands" has two very different causes and the caller's next
/// move differs — configure a server, versus look at why the configured one
/// served nothing.
#[must_use]
pub fn epilogue(server: Option<&str>, surface: &Surface) -> String {
    match server {
        None => format!(
            "{CASSETTES_ARE_DISCOVERED}\nNo server is configured, so none are listed; \
             set --tapes-url or TAPES_URL to see them."
        ),
        Some(server) if surface.cassettes.is_empty() => format!(
            "{CASSETTES_ARE_DISCOVERED}\nNo cassettes were discovered from {server}, \
             so none are listed; re-run with -v for why."
        ),
        Some(_) => CASSETTES_ARE_DISCOVERED.to_owned(),
    }
}

/// Run a matched cassette invocation.
///
/// `matches` is the cassette-level match; its own subcommand names the method.
/// The crate resolves the invocation back to a call; executing it against the
/// server `--tapes-url` names, and printing the response, happen here.
pub async fn dispatch(surface: &Surface, name: &str, matches: &ArgMatches) -> Result<()> {
    let (_method, call) = resolve_invocation(surface, name, matches)?;

    // `resolve_invocation` only succeeds when the method subcommand parsed,
    // so the matches carry it; `--tapes-url` is this module's own flag,
    // added through the decorator, and is read back off the method's matches.
    let raw = matches
        .subcommand()
        .and_then(|(_, method_matches)| method_matches.get_one::<String>(TAPES_URL))
        .context(error::MissingTapesUrlSnafu)?;
    let client = ApiClient::new(Url::parse(raw).context(error::TapesUrlSnafu)?);

    let value = client.call(&call).await?;
    print_json(&value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cassette::spec;
    use serde_json::json;
    use tapes_cassette_client::command::{call_for, read_body};

    fn surface_from(name: &str, document: &serde_json::Value) -> Surface {
        Surface {
            cassettes: vec![spec::reduce(name, None, document)],
        }
    }

    fn hello_surface() -> Surface {
        surface_from(
            "hello-world",
            &json!({"paths": {"/v1/cassettes/hello-world/hello": {
                "get": {"operationId": "getHello", "summary": "Greet"},
                "post": {"operationId": "createHello", "requestBody": {"required": true}}
            }}}),
        )
    }

    fn root() -> Command {
        Command::new("tapesctl").subcommand(Command::new("sessions"))
    }

    #[test]
    fn the_epilogue_says_cassettes_exist_even_with_nothing_to_list() {
        // The whole point of the line: an empty cassette list must not be
        // readable as "this build has no cassette support".
        for text in [
            epilogue(None, &Surface::default()),
            epilogue(Some("http://tapes.example"), &Surface::default()),
            epilogue(Some("http://tapes.example"), &hello_surface()),
        ] {
            assert!(text.contains("Cassette commands"), "got: {text}");
        }
    }

    #[test]
    fn the_epilogue_separates_no_server_from_a_server_that_served_none() {
        // Two different next moves — configure a server, versus find out what
        // the configured one did — so the help must not blur them together.
        let unconfigured = epilogue(None, &Surface::default());
        assert!(unconfigured.contains("TAPES_URL"), "got: {unconfigured}");

        let empty = epilogue(Some("http://tapes.example"), &Surface::default());
        assert!(
            empty.contains("http://tapes.example"),
            "the server that served nothing should be named: {empty}"
        );
        assert!(!empty.contains("No server is configured"), "got: {empty}");
    }

    #[test]
    fn the_epilogue_stops_explaining_once_there_is_something_to_list() {
        let listed = epilogue(Some("http://tapes.example"), &hello_surface());
        assert!(!listed.contains("none are listed"), "got: {listed}");
    }

    #[test]
    fn the_epilogue_reaches_the_rendered_help() {
        // `after_help` is applied by the caller in `crate::resolve`; this is the
        // check that the text clap renders is the text this module produced.
        let surface = Surface::default();
        let text = epilogue(None, &surface);
        let rendered = augment(root(), &surface)
            .after_help(text.clone())
            .render_help()
            .to_string();
        assert!(rendered.contains(&text), "got: {rendered}");
    }

    #[test]
    fn a_generated_surface_is_a_well_formed_clap_definition() {
        // clap panics at runtime on a malformed definition, and the workspace
        // denies panics — so a spec that produced one would be a crash the user
        // triggers just by pointing tapesctl at their own server.
        augment(root(), &hello_surface()).debug_assert();
    }

    #[test]
    fn a_cassette_becomes_a_noun_and_its_operations_become_methods() {
        let command = augment(root(), &hello_surface());
        let cassette = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "hello-world")
            .expect("the cassette noun should be generated");
        let methods: Vec<&str> = cassette
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert!(methods.contains(&"get-hello"), "got: {methods:?}");
        assert!(methods.contains(&"create-hello"), "got: {methods:?}");
    }

    #[test]
    fn a_cassette_cannot_redefine_a_built_in_command() {
        // A server that shipped a cassette named `sessions` would otherwise
        // change what an existing command does on the user's machine.
        let surface = surface_from(
            "sessions",
            &json!({"paths": {"/v1/cassettes/sessions/x": {"get": {"operationId": "getX"}}}}),
        );
        let command = augment(root(), &surface);
        let sessions: Vec<&clap::Command> = command
            .get_subcommands()
            .filter(|sub| sub.get_name() == "sessions")
            .collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].get_subcommands().count(), 0);
    }

    #[test]
    fn the_generated_help_names_the_route_it_calls() {
        // The one thing a user cannot infer from the command name.
        let mut command = augment(root(), &hello_surface());
        let help = command
            .find_subcommand_mut("hello-world")
            .and_then(|c| c.find_subcommand_mut("get-hello"))
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(
            help.contains("GET /v1/cassettes/hello-world/hello"),
            "got: {help}"
        );
    }

    #[test]
    fn a_path_parameter_parses_as_a_positional_and_a_query_parameter_as_a_flag() {
        let surface = surface_from(
            "summary",
            &json!({"paths": {"/v1/cassettes/summary/reports/{id}": {
                "get": {"operationId": "getReport", "parameters": [
                    {"name": "id", "in": "path", "required": true},
                    {"name": "since", "in": "query"}
                ]}
            }}}),
        );
        let matches = augment(root(), &surface)
            .try_get_matches_from([
                "tapesctl",
                "summary",
                "get-report",
                "r-1",
                "--since",
                "yesterday",
                "--tapes-url",
                "http://x",
            ])
            .unwrap();

        let (name, cassette) = matches.subcommand().unwrap();
        assert_eq!(name, "summary");
        let (_, method) = cassette.subcommand().unwrap();
        assert_eq!(method.get_one::<String>("id").unwrap(), "r-1");
        assert_eq!(method.get_one::<String>("since").unwrap(), "yesterday");
    }

    #[test]
    fn a_missing_required_path_parameter_is_rejected_before_any_request() {
        let surface = surface_from(
            "summary",
            &json!({"paths": {"/v1/cassettes/summary/reports/{id}": {
                "get": {"operationId": "getReport"}
            }}}),
        );
        assert!(
            augment(root(), &surface)
                .try_get_matches_from([
                    "tapesctl",
                    "summary",
                    "get-report",
                    "--tapes-url",
                    "http://x"
                ])
                .is_err(),
        );
    }

    #[test]
    fn a_required_body_is_required_and_an_absent_one_is_not_offered() {
        let command = augment(root(), &hello_surface());
        assert!(
            command
                .clone()
                .try_get_matches_from([
                    "tapesctl",
                    "hello-world",
                    "create-hello",
                    "--tapes-url",
                    "http://x"
                ])
                .is_err(),
            "a required body must be demanded up front",
        );
        // `get-hello` declares no request body, so `--body` is not a flag it has.
        assert!(
            command
                .try_get_matches_from([
                    "tapesctl",
                    "hello-world",
                    "get-hello",
                    "--body",
                    "{}",
                    "--tapes-url",
                    "http://x",
                ])
                .is_err(),
        );
    }

    #[test]
    fn a_method_that_takes_no_body_still_builds_a_call() {
        // clap panics on a lookup of an argument id the command does not
        // define, so reading `--body` unconditionally crashed every method that
        // declares none — which is most of them.
        let surface = surface_from(
            "summary",
            &json!({"paths": {"/v1/cassettes/summary/reports": {
                "get": {"operationId": "listReports"}
            }}}),
        );
        let matches = augment(root(), &surface)
            .try_get_matches_from([
                "tapesctl",
                "summary",
                "list-reports",
                "--tapes-url",
                "http://x",
            ])
            .unwrap();
        let (_, cassette) = matches.subcommand().unwrap();
        let (_, method_matches) = cassette.subcommand().unwrap();

        let cassette_spec = surface.cassette("summary").unwrap();
        let call = call_for(&cassette_spec.methods[0], method_matches).unwrap();
        assert!(call.body.is_none());
    }

    #[test]
    fn a_body_is_validated_as_json_before_it_is_sent() {
        // The cassette's 400 would be about its schema, not about the quoting.
        assert!(read_body("{\"a\":1}").is_ok());
        assert!(read_body("not json").is_err());
    }

    #[test]
    fn a_body_can_be_read_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.json");
        std::fs::write(&path, "{\"hello\": \"world\"}").unwrap();

        let body = read_body(&format!("@{}", path.display())).unwrap();
        assert_eq!(body, r#"{"hello":"world"}"#);
        assert!(read_body("@/nonexistent/body.json").is_err());
    }

    #[test]
    fn parameters_are_sent_under_their_wire_names_not_their_flag_names() {
        // `--auth-subject` on the command line, `auth_subject` on the wire —
        // the same split the hand-written surface makes.
        let surface = surface_from(
            "summary",
            &json!({"paths": {"/v1/cassettes/summary/reports": {
                "get": {"operationId": "listReports", "parameters": [
                    {"name": "auth_subject", "in": "query"},
                    {"name": "X-Report-Kind", "in": "header"}
                ]}
            }}}),
        );
        let matches = augment(root(), &surface)
            .try_get_matches_from([
                "tapesctl",
                "summary",
                "list-reports",
                "--auth-subject",
                "local:me",
                "--x-report-kind",
                "daily",
                "--tapes-url",
                "http://x",
            ])
            .unwrap();
        let (_, cassette) = matches.subcommand().unwrap();
        let (_, method_matches) = cassette.subcommand().unwrap();

        let cassette_spec = surface.cassette("summary").unwrap();
        let call = call_for(&cassette_spec.methods[0], method_matches).unwrap();

        assert_eq!(
            call.query,
            vec![("auth_subject".to_owned(), "local:me".to_owned())]
        );
        assert_eq!(
            call.headers,
            vec![("X-Report-Kind".to_owned(), "daily".to_owned())]
        );
    }

    #[tokio::test]
    async fn dispatch_calls_the_route_the_spec_named() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/summary/reports/r-1"))
            .and(query_param("since", "yesterday"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"report":"r-1"}"#))
            .mount(&server)
            .await;

        let surface = surface_from(
            "summary",
            &json!({"paths": {"/v1/cassettes/summary/reports/{id}": {
                "get": {"operationId": "getReport", "parameters": [
                    {"name": "id", "in": "path", "required": true},
                    {"name": "since", "in": "query"}
                ]}
            }}}),
        );
        let matches = augment(root(), &surface)
            .try_get_matches_from([
                "tapesctl",
                "summary",
                "get-report",
                "r-1",
                "--since",
                "yesterday",
                "--tapes-url",
                &server.uri(),
            ])
            .unwrap();
        let (name, cassette_matches) = matches.subcommand().unwrap();

        let result = dispatch(&surface, name, cassette_matches).await;
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn a_cassette_error_body_is_surfaced_rather_than_the_bare_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/summary/reports"))
            .respond_with(ResponseTemplate::new(502).set_body_string(
                r#"{"error":"cassette_unavailable","message":"summary is not answering"}"#,
            ))
            .mount(&server)
            .await;

        let surface = surface_from(
            "summary",
            &json!({"paths": {"/v1/cassettes/summary/reports": {
                "get": {"operationId": "listReports"}
            }}}),
        );
        let matches = augment(root(), &surface)
            .try_get_matches_from([
                "tapesctl",
                "summary",
                "list-reports",
                "--tapes-url",
                &server.uri(),
            ])
            .unwrap();
        let (name, cassette_matches) = matches.subcommand().unwrap();

        let err = dispatch(&surface, name, cassette_matches)
            .await
            .unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("502"), "got: {rendered}");
        assert!(rendered.contains("cassette_unavailable"), "got: {rendered}");
    }
}
