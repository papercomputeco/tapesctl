//! Library surface for `tapesctl`, kept separate from `main.rs` so the command
//! dispatch is unit-testable without spawning the binary.

pub mod api;
pub mod build_info;
pub mod capture;
pub mod cassette;
pub mod cli;
pub mod codex_app;
pub mod config;
pub mod error;
pub mod logging;
pub mod machine;
pub mod plugin;
pub mod ports;
pub mod start;
pub mod transcript;

use clap::{ArgMatches, CommandFactory, FromArgMatches};
use tapes_client::DirectHttp;
use url::Url;

use cassette::Surface;
use cli::{Cli, Command, PluginCommand, SkillCommand, StartArgs};
use config::Config;
pub use error::{Error, Result};

/// The tapesctl canary. The release smoke test asserts on this exact string,
/// so keep it stable.
///
/// It used to be the whole of what a bare `tapesctl` printed, which cost the
/// bare invocation its only chance to say what the tool does. `version` is
/// where it belongs instead: that command's entire job is printing this
/// binary's identity, it needs no server and no arguments, and it stays a
/// single stable line a smoke test can pin — which is what the canary was
/// being asked to be all along.
const CANARY: &str = "All in all, just another tape in the stereo";

/// This build's version, stamped at build time. See [`build_info`] for what
/// stamps it and why the manifest does not.
pub use build_info::version;

/// What `tapesctl version` prints: the build identity, then the canary.
///
/// The identity is the same block `--version` prints, verbatim, so the two ways
/// of asking cannot answer differently. The canary stays last: the release
/// smoke test reads it with `tail -n 1`.
#[must_use]
pub fn banner() -> String {
    format!("tapesctl {}\n{CANARY}", build_info::long_version())
}

/// What one command line resolved to.
///
/// The two arms exist because only one of them can be a typed value: the static
/// surface is a derived enum, while a cassette command is not known until a
/// server has been asked, so it stays as the matches plus the surface that
/// explains them.
#[derive(Debug)]
pub enum Invocation {
    /// A hand-written command.
    Static(Box<Cli>),
    /// A generated cassette command.
    Cassette {
        /// Verbosity from the global flag, which the derive would otherwise own.
        verbose: u8,
        /// The cassette noun.
        name: String,
        /// The cassette-level matches; its subcommand names the method.
        matches: Box<ArgMatches>,
        /// The surface the command was generated from.
        surface: Box<Surface>,
    },
}

impl Invocation {
    /// Verbosity, whichever arm this is.
    #[must_use]
    pub fn verbose(&self) -> u8 {
        match self {
            Self::Static(cli) => cli.verbose,
            Self::Cassette { verbose, .. } => *verbose,
        }
    }
}

/// Build the parser — cassette nouns included — and parse one command line.
///
/// Discovery happens before parsing because the generated commands have to be
/// in the parser for `tapesctl cassettes <name> <method>` to parse at all, and
/// for `tapesctl cassettes` to list them. And it is gated: only a command line
/// that can reach the generated surface — `cassettes …`, `help …`, or a bare /
/// flags-only invocation — runs it at all. Every other verb builds its parser
/// with zero discovery I/O; see [`build_parser`].
///
/// Exits the process on a parse error or on `--help`, which is what
/// `clap::Parser::parse` does and what the caller already expects.
pub async fn resolve<I, S>(argv: I, config: &Config) -> Invocation
where
    I: IntoIterator<Item = S>,
    S: Into<String> + Clone,
{
    let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
    let (command, surface) = build_parser(&argv, config).await;
    let matches = command.get_matches_from(&argv);

    // Two ways to reach a generated command: the `cassettes` noun — the
    // collision-proof spelling — and a cassette's own name at the top level,
    // mounted by discovery. Built-ins win a top-level name twice over: the
    // parser never mounts a generated command over one, and this dispatch
    // refuses to treat a static noun as generated even when a cassette
    // shares its name (`search` on a deployment serving the search cassette
    // is still this binary's own verb).
    let verbose = matches.get_count("verbose");
    let generated = match matches.subcommand() {
        Some((cassette::command::NOUN, noun_matches)) => noun_matches.subcommand(),
        Some((name, cassette_matches))
            if !cli::is_static_noun(name) && surface.cassette(name).is_some() =>
        {
            Some((name, cassette_matches))
        }
        _ => None,
    };
    if let Some((name, cassette_matches)) = generated {
        return Invocation::Cassette {
            verbose,
            name: name.to_owned(),
            matches: Box::new(cassette_matches.clone()),
            surface: Box::new(surface),
        };
    }

    match Cli::from_arg_matches(&matches) {
        Ok(cli) => Invocation::Static(Box::new(cli)),
        Err(error) => error.exit(),
    }
}

/// Discover this command line's cassettes and build the parser that knows them.
///
/// Split out of [`resolve`] so the help a real run prints can be *rendered* by
/// a test: `get_matches_from` answers `--help` by exiting the process, which
/// leaves nothing to assert on.
///
/// Discovery is gated on [`cli::gated`]: only `cassettes …`, `help …`, and a
/// bare / flags-only invocation can reach the generated surface, so only those
/// shapes pay for it. Everything else — `sessions list`, `start`, all of
/// them — gets a parser with the empty noun mounted and **zero** discovery
/// I/O: no cache read, no network. The empty noun still parses, so `tapesctl
/// cassettes` is never an unknown command; it is simply never reached from a
/// non-gated command line.
pub async fn build_parser(argv: &[String], config: &Config) -> (clap::Command, Surface) {
    if !cli::gated(argv) {
        let surface = Surface::default();
        // No epilogue text is ever rendered from here: clap only prints the
        // top-level help — the only place `after_help` shows — for the bare
        // and `help` shapes, and those are gated in.
        let command = parser(&surface, None, config.tapes_url.as_deref(), None);
        return (command, surface);
    }

    // Flag, then environment, then the configured default — the same three
    // sources, in the same order, that the parse below applies to every command
    // that needs a server. Discovery has to resolve them itself because it runs
    // before the parse that would otherwise do it.
    let server = cli::discovery_url(argv).or_else(|| config.tapes_url.clone());
    let (surface, provenance) = discover(server.as_deref()).await;
    (
        parser(
            &surface,
            server.as_deref(),
            config.tapes_url.as_deref(),
            provenance,
        ),
        surface,
    )
}

/// The parser for one run: the derived surface, the cassettes discovered for
/// it, and the configured server as the last-resort default.
fn parser(
    surface: &Surface,
    server: Option<&str>,
    configured: Option<&str>,
    provenance: Option<cassette::cache::Provenance>,
) -> clap::Command {
    // The epilogue is attached here rather than declared on `Cli`, because what
    // it has to say depends on the discovery that just ran: the derive can only
    // carry a constant, and a constant cannot tell a reader whether the missing
    // cassette commands are missing because no server was named or because the
    // named one served none — or listed from a cache because the server could
    // not answer just now.
    //
    // The discovered surface is mounted twice, deliberately: under the
    // `cassettes` noun (the collision-proof spelling both clients share) and —
    // via `augment` — as top-level commands, so `tapesctl search …` is the
    // cassette itself. `augment` skips any name a built-in already holds; a
    // server must not redefine what this binary's own command means.
    let command = tapes_client::cli::augment(
        cassette::command::mount(Cli::command(), surface),
        surface,
        cassette::command::with_tapes_url,
    )
    .after_help(cassette::command::epilogue(server, surface, provenance));

    // The configured server enters as clap's *default* for the global flag, so
    // the precedence is clap's own rather than a second implementation of it: a
    // default loses to an environment variable, which loses to an argument, and
    // clap propagates the winner into every subcommand that shares the id —
    // including the generated cassette methods. A default also does not count
    // as an argument the user supplied, so a bare `tapesctl` still answers with
    // help on a machine that has one configured.
    match configured {
        Some(configured) => command.mut_arg(cli::TAPES_URL_ARG, |arg| {
            arg.default_value(configured.to_owned())
        }),
        None => command,
    }
}

/// The cassette surface for the server this command line names, if any.
///
/// Never fails: with no server, an unparseable one, or an unreachable one, the
/// result is simply no cassette nouns. The hand-written surface has to keep
/// working on a machine that cannot reach any tapes server at all — which is
/// also why the caller keeps the server around to explain the empty result in
/// the help epilogue.
async fn discover(raw: Option<&str>) -> (Surface, Option<cassette::cache::Provenance>) {
    use cassette::cache::Provenance;

    let Some(raw) = raw else {
        return (Surface::default(), None);
    };
    let Ok(url) = Url::parse(raw) else {
        tracing::debug!(%raw, "not a URL, so no cassettes were discovered");
        return (Surface::default(), None);
    };
    let (surface, provenance) = cassette::cache::load_live(&DirectHttp::new(url)).await;
    // The warning is the dispatch-shape twin of the help epilogue's label: a
    // user running a generated command against a server that could not answer
    // is acting on cached knowledge, and should know it.
    match provenance {
        Provenance::Live => {}
        Provenance::TimedOut { .. } => {
            tracing::warn!(server = %raw, "cassette discovery timed out; cassette commands come from the local cache");
        }
        Provenance::FetchFailed { .. } => {
            tracing::warn!(server = %raw, "cassette discovery failed; cassette commands come from the local cache; re-run with -v for why");
        }
    }
    (surface, Some(provenance))
}

/// Dispatch one resolved invocation.
pub async fn dispatch(invocation: Invocation) -> Result<()> {
    match invocation {
        Invocation::Static(cli) => run(*cli).await,
        Invocation::Cassette {
            name,
            matches,
            surface,
            ..
        } => cassette::command::dispatch(&surface, &name, &matches).await,
    }
}

/// Dispatch a parsed CLI invocation.
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Version => {
            println!("{}", banner());
            Ok(())
        }
        Command::Start(args) => start(args).await,
        Command::Capture(args) => capture::run(args).await,
        Command::Sync(args) => transcript::sync::run(args).await,
        Command::Sessions(command) => api::sessions(command).await,
        Command::Traces(command) => api::traces(command).await,
        Command::Spans(command) => api::spans(command).await,
        Command::Search(args) => ports::search::run(args).await,
        Command::Export(args) => ports::export::run(args).await,
        Command::Seed(args) => ports::seed::run(args).await,
        Command::Skill(SkillCommand::Generate(args)) => ports::skill_generate::run(args).await,
        Command::Skill(SkillCommand::List(args)) => ports::skill_list::run(args),
        Command::Skill(SkillCommand::Sync(args)) => ports::skill::run(args),
        Command::Plugin(PluginCommand::Install(args)) => plugin::run(args),
        Command::Plugin(PluginCommand::Uninstall(args)) => plugin::uninstall(args),
        Command::Plugin(PluginCommand::Hook(args)) => codex_app::hook::run(&args).await,
        Command::Config(command) => config::run(&command),
    }
}

/// Launch a harness under a just-in-time capture proxy.
///
/// Spawns the harness with launch env from `tapes_harnesses::launch`, stands up
/// the dumb byte-forwarding proxy, stamps the `tapes_capture::envelope` onto
/// every turn, POSTs `TurnPayload`s to the tapes ingest server, and tails the
/// session's transcripts alongside it. See [`start`] for the lifetime and the
/// division of knowledge, and [`transcript::tailer`] for why the second lane is
/// not optional.
async fn start(args: StartArgs) -> Result<()> {
    start::run(args).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use clap::Parser;
    use cli::{ApiArgs, PluginInstallArgs, SeedArgs, SessionIdArgs, SessionsCommand};

    #[test]
    fn a_bare_invocation_is_answered_with_help_rather_than_a_silent_success() {
        // The canary used to be all a bare `tapesctl` said, so the one moment a
        // newcomer had the tool's attention was spent on a joke. clap owns the
        // answer now; this is what would notice if the required subcommand were
        // ever relaxed back to an optional one.
        let error =
            Cli::try_parse_from(["tapesctl"]).expect_err("a bare invocation should not parse");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
        );
        assert!(
            error.to_string().contains("Usage: tapesctl"),
            "the answer should be the usage: {error}"
        );
    }

    #[test]
    fn flags_without_a_subcommand_do_not_dispatch_either() {
        // `-v` is an argument, so it clears `arg_required_else_help`; the
        // required subcommand is what still stops the run.
        let error =
            Cli::try_parse_from(["tapesctl", "-v"]).expect_err("`-v` alone should not parse");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingSubcommand,
            "got: {error}"
        );
    }

    /// The configured server is not a fourth way of saying `--tapes-url`; it is
    /// the way that survives a new shell. This is the whole of what the config
    /// file buys, at the seam where it is applied.
    #[test]
    fn a_configured_server_reaches_a_command_that_names_none() {
        let matches = parser(&Surface::default(), None, Some("http://configured"), None)
            .try_get_matches_from(["tapesctl", "sessions", "list"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        match cli.command {
            Command::Sessions(SessionsCommand::List(args)) => {
                assert_eq!(args.api.tapes_url.as_deref(), Some("http://configured"));
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn an_argument_still_beats_the_configured_server() {
        // The precedence is clap's own — a default loses to an argument — which
        // is why it is expressed as a default rather than resolved by hand.
        for argv in [
            [
                "tapesctl",
                "--tapes-url",
                "http://typed",
                "sessions",
                "list",
            ],
            [
                "tapesctl",
                "sessions",
                "list",
                "--tapes-url",
                "http://typed",
            ],
        ] {
            let matches = parser(&Surface::default(), None, Some("http://configured"), None)
                .try_get_matches_from(argv)
                .unwrap();
            match Cli::from_arg_matches(&matches).unwrap().command {
                Command::Sessions(SessionsCommand::List(args)) => {
                    assert_eq!(args.api.tapes_url.as_deref(), Some("http://typed"));
                }
                other => panic!("got: {other:?}"),
            }
        }
    }

    /// The answer to a bare invocation has to survive a machine that has
    /// a server configured. A default value is not an argument the user
    /// supplied, and this is what would notice if that ever stopped being true.
    #[test]
    fn a_configured_server_does_not_cost_the_bare_invocation_its_help() {
        let error = parser(&Surface::default(), None, Some("http://configured"), None)
            .try_get_matches_from(["tapesctl"])
            .expect_err("a bare invocation should not parse");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "got: {error}",
        );
    }

    /// The canonical spelling is the only one: `cassettes hello-world
    /// get-hello` parses, and the retired top-level `hello-world get-hello`
    /// fails exactly like any other unknown command — even with the cassette
    /// on the discovered surface.
    #[test]
    fn both_spellings_reach_the_same_generated_command() {
        // `resolve` would reach the network for a URL it has no cache for,
        // so the parse is driven directly off a known surface instead.
        let surface = hello_surface();
        let matches = parser(&surface, Some("http://x"), None, None)
            .try_get_matches_from([
                "tapesctl",
                cassette::command::NOUN,
                "hello-world",
                "get-hello",
                "--tapes-url",
                "http://x",
            ])
            .unwrap();
        let (name, cassette_matches) = match matches.subcommand() {
            Some((cassette::command::NOUN, noun)) => noun.subcommand(),
            other => other,
        }
        .expect("the canonical spelling should reach the cassette");
        assert_eq!(name, "hello-world");
        assert!(cassette_matches.subcommand_name() == Some("get-hello"));

        // The cassette's own name is a top-level command too — the same
        // generated method, one level up. Discovery mounted it, so the
        // deployment's surface is the CLI's surface.
        let matches = parser(&surface, Some("http://x"), None, None)
            .try_get_matches_from([
                "tapesctl",
                "hello-world",
                "get-hello",
                "--tapes-url",
                "http://x",
            ])
            .expect("a cassette's name is a top-level command");
        let (name, cassette_matches) = matches
            .subcommand()
            .expect("the top-level spelling should reach the cassette");
        assert_eq!(name, "hello-world");
        assert!(cassette_matches.subcommand_name() == Some("get-hello"));
    }

    #[test]
    fn a_built_in_name_is_never_mounted_from_discovery() {
        // A cassette named after one of this binary's own verbs neither
        // shadows it nor errors: the built-in keeps the top level, and the
        // cassette stays reachable through the collision-proof spelling.
        let surface = Surface {
            cassettes: vec![cassette::spec::reduce(
                "sessions",
                None,
                &serde_json::json!({"paths": {"/v1/cassettes/sessions/hello": {
                    "get": {"operationId": "getHello"}
                }}}),
            )],
        };
        let matches = parser(&surface, Some("http://x"), None, None)
            .try_get_matches_from(["tapesctl", "sessions", "list"])
            .expect("the built-in must keep its name");
        assert!(Cli::from_arg_matches(&matches).is_ok());

        let matches = parser(&surface, Some("http://x"), None, None)
            .try_get_matches_from([
                "tapesctl",
                cassette::command::NOUN,
                "sessions",
                "get-hello",
                "--tapes-url",
                "http://x",
            ])
            .expect("the colliding cassette stays reachable under the noun");
        let (name, _) = match matches.subcommand() {
            Some((cassette::command::NOUN, noun)) => noun.subcommand(),
            other => other,
        }
        .unwrap();
        assert_eq!(name, "sessions");
    }

    fn hello_surface() -> Surface {
        Surface {
            cassettes: vec![cassette::spec::reduce(
                "hello-world",
                None,
                &serde_json::json!({"paths": {"/v1/cassettes/hello-world/hello": {
                    "get": {"operationId": "getHello"}
                }}}),
            )],
        }
    }

    /// Serializes tests that redirect [`cassette::cache::CACHE_DIR_ENV`]: the
    /// process environment is global state and cargo runs tests on threads, so
    /// two tests pointing the cache at two directories would race.
    static CACHE_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points the cassette cache at a directory for the guard's lifetime,
    /// restoring an unset variable on drop.
    struct CacheDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CacheDirGuard {
        fn set(dir: &std::path::Path) -> Self {
            let lock = CACHE_DIR_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: the lock serializes every mutation of this variable, and
            // only these tests read it.
            unsafe { std::env::set_var(cassette::cache::CACHE_DIR_ENV, dir) };
            Self { _lock: lock }
        }
    }

    impl Drop for CacheDirGuard {
        fn drop(&mut self) {
            // SAFETY: still under the lock held by `_lock`.
            unsafe { std::env::remove_var(cassette::cache::CACHE_DIR_ENV) };
        }
    }

    /// A mock deployment serving one cassette named `hello-world`.
    async fn serve_hello_world() -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "contract_version": "v1",
                "cassettes": [{
                    "name": "hello-world",
                    "route_prefix": "/v1/cassettes/hello-world",
                    "openapi_path": "/v1/cassettes/hello-world/openapi.json",
                    "openapi_status": "fresh"
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/hello-world/openapi.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "paths": {"/v1/cassettes/hello-world/hello": {
                    "get": {"operationId": "getHello"}
                }}
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_non_cassette_invocation_builds_its_parser_with_zero_discovery_io() {
        // The observables: a mock server standing in as the named deployment,
        // and an empty cache directory. The cache is cold, so if discovery ran
        // at all it would have to fetch — the cassette would land on the
        // surface, the server would see the requests, and the cache directory
        // would gain the entry it writes. All three must stay untouched.
        let cache_dir = tempfile::tempdir().unwrap();
        let _env = CacheDirGuard::set(cache_dir.path());
        let server = serve_hello_world().await;

        let configured = Config {
            tapes_url: Some(server.uri()),
        };
        let url_flag = format!("--tapes-url={}", server.uri());
        for shape in [
            vec!["tapesctl", "sessions", "list"],
            vec!["tapesctl", url_flag.as_str(), "sessions", "list"],
            vec!["tapesctl", "version"],
            vec!["tapesctl", "start", "claude", "--", "cassettes"],
        ] {
            let argv: Vec<String> = shape.iter().map(|s| (*s).to_owned()).collect();
            let (_, surface) = build_parser(&argv, &configured).await;
            assert!(surface.cassettes.is_empty(), "{shape:?} must not discover");
        }

        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a non-cassette invocation reached the server during parser build",
        );
        assert!(
            std::fs::read_dir(cache_dir.path())
                .unwrap()
                .next()
                .is_none(),
            "nor may it touch the cassette cache",
        );
    }

    #[tokio::test]
    async fn a_cassettes_invocation_still_discovers_and_parses() {
        let cache_dir = tempfile::tempdir().unwrap();
        let _env = CacheDirGuard::set(cache_dir.path());
        let server = serve_hello_world().await;

        let argv: Vec<String> = [
            "tapesctl",
            cassette::command::NOUN,
            "hello-world",
            "get-hello",
            "--tapes-url",
            &server.uri(),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let (command, surface) = build_parser(&argv, &Config::default()).await;

        assert!(
            surface.cassette("hello-world").is_some(),
            "a `cassettes` invocation is gated in and must discover",
        );
        assert!(
            !server.received_requests().await.unwrap().is_empty(),
            "the cold cache means discovery had to fetch",
        );

        let matches = command.try_get_matches_from(&argv).unwrap();
        let (name, cassette_matches) = matches
            .subcommand()
            .and_then(|(_, noun)| noun.subcommand())
            .expect("the generated command should parse");
        assert_eq!(name, "hello-world");
        assert_eq!(cassette_matches.subcommand_name(), Some("get-hello"));
    }

    #[test]
    fn the_generated_surface_and_the_global_flag_are_one_argument_not_two() {
        // Two ids sharing `--tapes-url` is a duplicate the moment the global
        // propagates into a generated method, and clap answers a duplicate by
        // panicking — a crash a user would trigger just by pointing tapesctl at
        // their own server.
        let surface = hello_surface();
        parser(&surface, Some("http://x"), Some("http://x"), None).debug_assert();

        // And the configured default reaches the generated method, which reads
        // the flag off its own matches — through the noun, which is one more
        // level for it to propagate down.
        let matches = parser(&surface, Some("http://x"), Some("http://configured"), None)
            .try_get_matches_from([
                "tapesctl",
                cassette::command::NOUN,
                "hello-world",
                "get-hello",
            ])
            .unwrap();
        let method = matches
            .subcommand()
            .and_then(|(_, noun)| noun.subcommand())
            .and_then(|(_, cassette)| cassette.subcommand())
            .map(|(_, method)| method)
            .unwrap();
        assert_eq!(
            method
                .get_one::<String>(cli::TAPES_URL_ARG)
                .map(String::as_str),
            Some("http://configured"),
        );
    }

    #[test]
    fn the_canary_survives_in_the_version_banner() {
        // It is the string the release smoke test pins, so its home moving must
        // not be its wording changing.
        let banner = banner();
        assert!(banner.contains(CANARY), "got: {banner}");
        assert!(banner.contains(version()), "got: {banner}");
    }

    #[tokio::test]
    async fn version_is_ok() {
        let cli = Cli {
            verbose: 0,
            tapes_url: None,
            command: Command::Version,
        };
        assert!(run(cli).await.is_ok());
    }

    #[tokio::test]
    async fn no_command_is_stubbed_out_any_more() {
        // `sync` and `plugin install` both used to answer `NotImplemented`
        // here. Both are implemented now, and this is what would notice if a
        // future refactor quietly stubbed one again.
        //
        // The harness is deliberately one that does not exist. Only the real
        // implementation answers an unknown name by listing the registry, so
        // `UnknownHarness` proves the arm reaches it — and it proves that
        // without dispatching an install, which would resolve the *runner's*
        // home, `$CODEX_HOME`, and `codex` on `PATH`. A dispatched install
        // once did exactly that, and the real `codex` it found rewrote the
        // developer's own `~/.codex/config.toml`; `Machine::resolve` now
        // refuses under `cfg(test)` so no test can repeat it. What an install
        // actually writes is asserted in `plugin::tests` against a temporary
        // home.
        let cli = Cli {
            verbose: 0,
            tapes_url: None,
            command: Command::Plugin(PluginCommand::Install(PluginInstallArgs {
                harness: "not-a-harness".to_owned(),
                dry_run: false,
                port: None,
                codex_auth: None,
            })),
        };
        let answered = run(cli).await;
        assert!(!matches!(answered, Err(Error::NotImplemented { .. })));
        assert!(
            matches!(answered, Err(Error::UnknownHarness { .. })),
            "got: {answered:?}"
        );
    }

    #[tokio::test]
    async fn a_read_command_without_a_server_fails_on_the_missing_url() {
        // Not on a connection attempt: with no URL there is nowhere to connect.
        let cli = Cli {
            verbose: 0,
            tapes_url: None,
            command: Command::Sessions(SessionsCommand::Get(SessionIdArgs {
                api: ApiArgs { tapes_url: None },
                id: "s-1".to_owned(),
            })),
        };
        assert!(matches!(run(cli).await, Err(Error::MissingTapesUrl)));
    }

    #[tokio::test]
    async fn seed_without_a_server_fails_on_the_missing_url() {
        let cli = Cli {
            verbose: 0,
            tapes_url: None,
            command: Command::Seed(SeedArgs {
                api: ApiArgs { tapes_url: None },
            }),
        };
        assert!(matches!(run(cli).await, Err(Error::MissingTapesUrl)));
    }
}
