//! Library surface for `tapesctl`, kept separate from `main.rs` so the command
//! dispatch is unit-testable without spawning the binary.

pub mod cli;
pub mod error;
pub mod start;

use cli::{Cli, Command, PluginCommand, StartArgs};
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
        Some(Command::Sync) => Err(Error::NotImplemented {
            what: "tapesctl sync",
        }),
        Some(Command::Plugin(cmd)) => plugin(cmd),
    }
}

/// Launch a harness under a just-in-time capture proxy.
///
/// Spawns the harness with launch env from `tapes_harnesses::launch`, stands up
/// the dumb byte-forwarding proxy, stamps the `tapes_harnesses::envelope` onto
/// every turn, and POSTs `TurnPayload`s to the tapes ingest server. See
/// [`start`] for the lifetime and the division of knowledge.
async fn start(args: StartArgs) -> Result<()> {
    start::run(args).await
}

/// Manage harness capture plugins (backed by `tapes-harness` plugin assets).
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_subcommand_is_ok() {
        let cli = Cli {
            verbose: 0,
            command: None,
        };
        assert!(run(cli).await.is_ok());
    }

    #[tokio::test]
    async fn sync_is_not_implemented_yet() {
        let cli = Cli {
            verbose: 0,
            command: Some(Command::Sync),
        };
        assert!(matches!(run(cli).await, Err(Error::NotImplemented { .. })));
    }
}
