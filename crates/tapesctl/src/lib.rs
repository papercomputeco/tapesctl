//! Library surface for `tapesctl`, kept separate from `main.rs` so the command
//! dispatch is unit-testable without spawning the binary.

pub mod api;
pub mod cli;
pub mod error;
pub mod plugin;
pub mod ports;
pub mod start;
pub mod transcript;

use cli::{Cli, Command, PluginCommand, SkillCommand, StartArgs};
pub use error::{Error, Result};

/// The tapesctl canary. Printed when the binary is invoked with no subcommand;
/// the release smoke test asserts on this exact string, so keep it stable.
const CANARY: &str = "All in all, just another tape in the stereo";

/// Initialize `tracing` output on stderr. `verbose` bumps the default filter
/// (`RUST_LOG` still wins when set): `-v` → debug, `-vv` → trace.
pub fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt};

    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    // `try_init` is fine to ignore: a failure just means a subscriber is
    // already installed (e.g. in tests), which is not fatal here.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// The build version, stamped by cargo.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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
        Some(Command::Plugin(PluginCommand::Install(args))) => plugin::run(args),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use cli::{ApiArgs, PluginInstallArgs, SeedArgs, SessionIdArgs, SessionsCommand};

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
    async fn no_command_is_stubbed_out_any_more() {
        // `sync` and `plugin install` both used to answer `NotImplemented`
        // here. Both are implemented now, and this is what would notice if a
        // future refactor quietly stubbed one again.
        let cli = Cli {
            verbose: 0,
            command: Some(Command::Plugin(PluginCommand::Install(PluginInstallArgs {
                // A harness needing no plugin: reaches the implementation and
                // reports, without writing to the runner's home.
                harness: "claude".to_owned(),
                dry_run: false,
            }))),
        };
        assert!(!matches!(run(cli).await, Err(Error::NotImplemented { .. })));
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
