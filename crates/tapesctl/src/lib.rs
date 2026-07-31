//! Library surface for `tapesctl`, kept separate from `main.rs` so the command
//! dispatch is unit-testable without spawning the binary.

pub mod api;
pub mod cli;
pub mod error;
pub mod logging;
pub mod ports;
pub mod start;
pub mod transcript;

use cli::{Cli, Command, PluginCommand, SkillCommand, StartArgs};
pub use error::{Error, Result};

/// The tapesctl canary. Printed when the binary is invoked with no subcommand;
/// the release smoke test asserts on this exact string, so keep it stable.
const CANARY: &str = "All in all, just another tape in the stereo";

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
