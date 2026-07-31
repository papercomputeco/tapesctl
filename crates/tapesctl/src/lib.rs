//! Library surface for `tapesctl`, kept separate from `main.rs` so the command
//! dispatch is unit-testable without spawning the binary.

pub mod api;
pub mod cassette;
pub mod cli;
pub mod error;
pub mod logging;
pub mod ports;
pub mod start;
pub mod transcript;

use clap::{ArgMatches, CommandFactory, FromArgMatches};
use url::Url;

use api::client::ApiClient;
use cassette::Surface;
use cli::{Cli, Command, PluginCommand, SkillCommand, StartArgs};
pub use error::{Error, Result};

/// The tapesctl canary. Printed when the binary is invoked with no subcommand;
/// the release smoke test asserts on this exact string, so keep it stable.
const CANARY: &str = "All in all, just another tape in the stereo";

/// The build version, stamped by cargo.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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
/// Discovery happens before parsing because the generated nouns have to be in
/// the parser for `tapesctl <cassette> ...` to parse at all, and for
/// `tapesctl --help` to list them. It is cheap in the common case: the surface
/// comes from [`cassette::cache`] and only reaches the network when that has
/// gone stale.
///
/// Exits the process on a parse error or on `--help`, which is what
/// `clap::Parser::parse` does and what the caller already expects.
pub async fn resolve<I, S>(argv: I) -> Invocation
where
    I: IntoIterator<Item = S>,
    S: Into<String> + Clone,
{
    let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
    let surface = discover(&argv).await;

    let command = cassette::command::augment(Cli::command(), &surface);
    let matches = command.get_matches_from(&argv);

    // A noun on the surface is a generated command; anything else is one of the
    // derived ones. Built-ins win the name — `augment` never generates over one.
    if let Some((name, cassette_matches)) = matches.subcommand() {
        if surface.cassette(name).is_some() {
            return Invocation::Cassette {
                verbose: matches.get_count("verbose"),
                name: name.to_owned(),
                matches: Box::new(cassette_matches.clone()),
                surface: Box::new(surface),
            };
        }
    }

    match Cli::from_arg_matches(&matches) {
        Ok(cli) => Invocation::Static(Box::new(cli)),
        Err(error) => error.exit(),
    }
}

/// The cassette surface for whatever server this command line names.
///
/// Never fails: with no server, an unparseable one, or an unreachable one, the
/// result is simply no cassette nouns. The hand-written surface has to keep
/// working on a machine that cannot reach any tapes server at all.
async fn discover(argv: &[String]) -> Surface {
    let Some(raw) = cli::discovery_url(argv) else {
        return Surface::default();
    };
    let Ok(url) = Url::parse(&raw) else {
        tracing::debug!(%raw, "not a URL, so no cassettes were discovered");
        return Surface::default();
    };
    cassette::cache::load(&ApiClient::new(url)).await
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
        None => {
            println!("{CANARY}");
            Ok(())
        }
        Some(Command::Version) => {
            println!("tapesctl {}", version());
            Ok(())
        }
        Some(Command::Start(args)) => start(args).await,
        Some(Command::Sync(args)) => transcript::sync::run(args).await,
        Some(Command::Sessions(command)) => api::sessions(command).await,
        Some(Command::Traces(command)) => api::traces(command).await,
        Some(Command::Spans(command)) => api::spans(command).await,
        Some(Command::Export(args)) => ports::export::run(args).await,
        Some(Command::Seed(args)) => ports::seed::run(args).await,
        Some(Command::Skill(SkillCommand::Sync(args))) => ports::skill::run(args),
        Some(Command::Plugin(cmd)) => plugin(cmd),
    }
}

/// Launch a harness under a just-in-time capture proxy.
///
/// Spawns the harness with launch env from `tapes_harnesses::launch`, stands up
/// the dumb byte-forwarding proxy, stamps the `tapes_harnesses::envelope` onto
/// every turn, POSTs `TurnPayload`s to the tapes ingest server, and tails the
/// session's transcripts alongside it. See [`start`] for the lifetime and the
/// division of knowledge, and [`transcript::tailer`] for why the second lane is
/// not optional.
async fn start(args: StartArgs) -> Result<()> {
    start::run(args).await
}

/// Manage harness capture plugins (backed by `tapes-harnesses` plugin assets).
fn plugin(cmd: PluginCommand) -> Result<()> {
    match cmd {
        PluginCommand::Install { harness } => {
            tracing::info!(%harness, "tapesctl plugin install is not implemented yet");
            Err(Error::NotImplemented {
                what: "tapesctl plugin install",
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use cli::{ApiArgs, SeedArgs, SessionIdArgs, SessionsCommand};

    #[tokio::test]
    async fn no_subcommand_is_ok() {
        let cli = Cli {
            verbose: 0,
            command: None,
        };
        assert!(run(cli).await.is_ok());
    }

    #[tokio::test]
    async fn version_is_ok() {
        let cli = Cli {
            verbose: 0,
            command: Some(Command::Version),
        };
        assert!(run(cli).await.is_ok());
    }

    #[tokio::test]
    async fn plugin_install_is_still_the_only_unimplemented_command() {
        // `sync` used to live here too; it is implemented now, and this test is
        // what would notice if a future refactor quietly stubbed it again.
        let cli = Cli {
            verbose: 0,
            command: Some(Command::Plugin(PluginCommand::Install {
                harness: "claude".to_owned(),
            })),
        };
        assert!(matches!(run(cli).await, Err(Error::NotImplemented { .. })));
    }

    #[tokio::test]
    async fn a_read_command_without_a_server_fails_on_the_missing_url() {
        // Not on a connection attempt: with no URL there is nowhere to connect.
        let cli = Cli {
            verbose: 0,
            command: Some(Command::Sessions(SessionsCommand::Get(SessionIdArgs {
                api: ApiArgs { tapes_url: None },
                id: "s-1".to_owned(),
            }))),
        };
        assert!(matches!(run(cli).await, Err(Error::MissingTapesUrl)));
    }

    #[tokio::test]
    async fn seed_without_a_server_fails_on_the_missing_url() {
        let cli = Cli {
            verbose: 0,
            command: Some(Command::Seed(SeedArgs {
                api: ApiArgs { tapes_url: None },
            })),
        };
        assert!(matches!(run(cli).await, Err(Error::MissingTapesUrl)));
    }
}
