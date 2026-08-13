//! Library surface for `tapesctl`, kept separate from `main.rs` so the command
//! dispatch is unit-testable without spawning the binary.

pub mod api;
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

/// The build version, stamped by cargo.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// What `tapesctl version` prints: the build identity, then the canary.
#[must_use]
pub fn banner() -> String {
    format!("tapesctl {}\n{CANARY}", version())
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
/// for `tapesctl cassettes` to list them. It is cheap in the common case: the surface
/// comes from [`cassette::cache`] and only reaches the network when that has
/// gone stale.
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

    // Two ways to reach a generated command, and they resolve to the same thing:
    // the canonical `tapesctl cassettes <name> <method>`, and the hidden
    // top-level `tapesctl <name> <method>` the surface used to be spelled as.
    // Built-ins win the name in both — neither `mount` nor `augment` generates
    // over one.
    let verbose = matches.get_count("verbose");
    let generated = match matches.subcommand() {
        Some((cassette::command::NOUN, noun_matches)) => noun_matches.subcommand(),
        Some((name, cassette_matches)) if surface.cassette(name).is_some() => {
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
pub async fn build_parser(argv: &[String], config: &Config) -> (clap::Command, Surface) {
    // Flag, then environment, then the configured default — the same three
    // sources, in the same order, that the parse below applies to every command
    // that needs a server. Discovery has to resolve them itself because it runs
    // before the parse that would otherwise do it.
    let server = cli::discovery_url(argv).or_else(|| config.tapes_url.clone());
    let surface = discover(server.as_deref()).await;
    (
        parser(&surface, server.as_deref(), config.tapes_url.as_deref()),
        surface,
    )
}

/// The parser for one run: the derived surface, the cassettes discovered for
/// it, and the configured server as the last-resort default.
fn parser(surface: &Surface, server: Option<&str>, configured: Option<&str>) -> clap::Command {
    // The epilogue is attached here rather than declared on `Cli`, because what
    // it has to say depends on the discovery that just ran: the derive can only
    // carry a constant, and a constant cannot tell a reader whether the missing
    // cassette commands are missing because no server was named or because the
    // named one served none.
    //
    // The noun is mounted before the hidden top-level aliases are added, so a
    // deployment that serves a cassette actually named `cassettes` finds the
    // name taken and lands under the noun like every other — rather than
    // replacing the noun and taking its siblings with it.
    let command =
        cassette::command::augment(cassette::command::mount(Cli::command(), surface), surface)
            .after_help(cassette::command::epilogue(server, surface));

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
async fn discover(raw: Option<&str>) -> Surface {
    let Some(raw) = raw else {
        return Surface::default();
    };
    let Ok(url) = Url::parse(raw) else {
        tracing::debug!(%raw, "not a URL, so no cassettes were discovered");
        return Surface::default();
    };
    cassette::cache::load(&DirectHttp::new(url)).await
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
        let matches = parser(&Surface::default(), None, Some("http://configured"))
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
            let matches = parser(&Surface::default(), None, Some("http://configured"))
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
        let error = parser(&Surface::default(), None, Some("http://configured"))
            .try_get_matches_from(["tapesctl"])
            .expect_err("a bare invocation should not parse");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "got: {error}",
        );
    }

    /// Both spellings have to arrive at the same place, because for one release
    /// they are the same command: the canonical `cassettes hello-world
    /// get-hello` and the hidden `hello-world get-hello` it replaced.
    #[test]
    fn both_spellings_resolve_to_the_same_generated_invocation() {
        for argv in [
            vec![
                "tapesctl".to_owned(),
                cassette::command::NOUN.to_owned(),
                "hello-world".to_owned(),
                "get-hello".to_owned(),
                "--tapes-url".to_owned(),
                "http://x".to_owned(),
            ],
            vec![
                "tapesctl".to_owned(),
                "hello-world".to_owned(),
                "get-hello".to_owned(),
                "--tapes-url".to_owned(),
                "http://x".to_owned(),
            ],
        ] {
            // `resolve` would reach the network for a URL it has no cache for,
            // so the parse is driven directly off a known surface instead.
            let surface = hello_surface();
            let matches = parser(&surface, Some("http://x"), None)
                .try_get_matches_from(&argv)
                .unwrap();
            let (name, cassette_matches) = match matches.subcommand() {
                Some((cassette::command::NOUN, noun)) => noun.subcommand(),
                other => other,
            }
            .expect("both spellings should reach a cassette");
            assert_eq!(name, "hello-world");
            assert!(cassette_matches.subcommand_name() == Some("get-hello"));
        }
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

    #[test]
    fn the_generated_surface_and_the_global_flag_are_one_argument_not_two() {
        // Two ids sharing `--tapes-url` is a duplicate the moment the global
        // propagates into a generated method, and clap answers a duplicate by
        // panicking — a crash a user would trigger just by pointing tapesctl at
        // their own server.
        let surface = hello_surface();
        parser(&surface, Some("http://x"), Some("http://x")).debug_assert();

        // And the configured default reaches the generated method, which reads
        // the flag off its own matches — through the noun, which is one more
        // level for it to propagate down.
        let matches = parser(&surface, Some("http://x"), Some("http://configured"))
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
