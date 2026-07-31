//! Command-line surface for `tapesctl`.

use clap::{Args, Parser, Subcommand};

/// The Tapes client CLI.
#[derive(Debug, Parser)]
#[command(name = "tapesctl", version, about = "The Tapes client CLI")]
pub struct Cli {
    /// Increase log verbosity (`-v` debug, `-vv` trace). `RUST_LOG` overrides.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands.
///
/// The `<resource> <method>` and generated `<cassette> <method>` surfaces from
/// the RFC are not modelled yet — they arrive once `/v1/cassettes` discovery
/// and the OpenAPI client generation land (Track 4).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch a harness under a just-in-time capture proxy.
    Start(StartArgs),

    /// Sweep completed harness transcripts into the tapes ingest server.
    ///
    /// Closes paperd's crash-window gap: sessions that exited while no capture
    /// was running are discovered and pushed on demand (dedup makes re-push
    /// safe).
    Sync,

    /// Install or manage harness capture plugins.
    #[command(subcommand)]
    Plugin(PluginCommand),

    /// Print version information.
    Version,
}

/// Arguments for `tapesctl start`.
#[derive(Debug, Args)]
pub struct StartArgs {
    /// The harness to launch (e.g. `claude`, `codex`).
    pub harness: String,

    /// Arguments passed through verbatim to the harness.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub harness_args: Vec<String>,

    /// Base URL of the tapes ingest server. Falls back to `TAPES_URL`.
    #[arg(long, env = "TAPES_URL")]
    pub tapes_url: Option<String>,

    /// Where to forward the harness's LLM traffic. Defaults to the harness's
    /// own provider API, so the harness behaves exactly as it would unproxied.
    #[arg(long, env = "TAPES_UPSTREAM")]
    pub upstream: Option<String>,

    /// Base URL of the web console, used to print a link to the captured
    /// session. Without it the session id is printed on its own.
    #[arg(long, env = "TAPES_WEB_URL")]
    pub web_url: Option<String>,

    /// Org id to stamp on captured turns. Must be a UUID; empty (the default)
    /// selects the server's local sentinel org.
    #[arg(long, env = "TAPES_ORG_ID")]
    pub org_id: Option<String>,

    /// Acting subject to stamp on captured turns. Defaults to
    /// `local:<username>`; agents and CI override it (e.g. `gardener-ci`).
    #[arg(long, env = "TAPES_AUTH_SUBJECT")]
    pub auth_subject: Option<String>,
}

/// `tapesctl plugin` subcommands.
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Install the capture plugin/hooks for a harness.
    Install {
        /// The harness to install capture support for (e.g. `claude`, `codex`).
        harness: String,
    },
}
