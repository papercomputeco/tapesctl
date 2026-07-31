//! Command-line surface for `tapesctl`.

use std::path::PathBuf;

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
/// The `<resource> <method>` surface is hand-written here for the core data
/// model — sessions, traces, spans. The *generated* `<cassette> <method>`
/// surface from the RFC is a separate thing and still to come: it arrives with
/// `/v1/cassettes` discovery and OpenAPI client generation in Track 4, and will
/// cover resources this binary cannot know about at compile time.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch a harness under a just-in-time capture proxy.
    Start(StartArgs),

    /// Sweep completed harness transcripts into the tapes ingest server.
    ///
    /// The live tailer that runs during `start` is the primary path; this is the
    /// backstop for sessions no capture was running for (dedup makes re-push
    /// safe).
    Sync(SyncArgs),

    /// Read sessions.
    #[command(subcommand)]
    Sessions(SessionsCommand),

    /// Read traces.
    #[command(subcommand)]
    Traces(TracesCommand),

    /// Read spans.
    #[command(subcommand)]
    Spans(SpansCommand),

    /// Write a session's export bundle to a file or stdout.
    Export(ExportArgs),

    /// Populate a server with demo sessions.
    Seed(SeedArgs),

    /// Manage agent skills.
    #[command(subcommand)]
    Skill(SkillCommand),

    /// Install or manage harness capture plugins.
    #[command(subcommand)]
    Plugin(PluginCommand),

    /// Print version information.
    Version,
}

/// Where the tapes server is, shared by every command that talks to one.
#[derive(Debug, Clone, Args)]
pub struct ApiArgs {
    /// Base URL of the tapes server. Falls back to `TAPES_URL`.
    #[arg(long, env = "TAPES_URL")]
    pub tapes_url: Option<String>,
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

    /// Do not tail this session's transcripts.
    ///
    /// Transcripts are the only source of a session's causal skeleton, so a
    /// capture without them renders subagent work as flat text. This exists for
    /// the case where another capture client is already tailing the same tree.
    #[arg(long)]
    pub no_transcripts: bool,
}

/// Arguments for `tapesctl sync`.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Base URL of the tapes ingest server. Falls back to `TAPES_URL`.
    #[arg(long, env = "TAPES_URL")]
    pub tapes_url: Option<String>,

    /// Transcript tree to sweep. Defaults to `~/.claude/projects`.
    #[arg(long)]
    pub projects_root: Option<PathBuf>,

    /// Acting subject to stamp on uploaded transcripts. Defaults to
    /// `local:<username>`.
    #[arg(long, env = "TAPES_AUTH_SUBJECT")]
    pub auth_subject: Option<String>,

    /// Only sweep sessions touched within this many days. `0` sweeps
    /// everything. A cost bound, not a correctness one — the server dedups.
    #[arg(long = "since-days")]
    pub since_days: Option<u64>,
}

/// `tapesctl sessions` methods.
#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List sessions, newest first.
    List(SessionsListArgs),
    /// Fetch one session's metadata and rollup.
    Get(SessionIdArgs),
    /// Fetch a session's derived traces — what the console renders.
    Traces(SessionPayloadArgs),
    /// List the raw wire turns behind a session's derivation.
    RawTurns(SessionIdArgs),
}

/// Arguments for `tapesctl sessions list`.
#[derive(Debug, Args)]
pub struct SessionsListArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// Page size. The server defaults to 50 and clamps at 200.
    #[arg(long)]
    pub limit: Option<u64>,

    /// Page cursor from a previous response's `next_cursor`. Only valid with
    /// the same `--sort` and `--direction` it was minted under.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Sort column (e.g. `last_active`, `started_at`, `total_cost_usd`).
    #[arg(long)]
    pub sort: Option<String>,

    /// `asc` or `desc`.
    #[arg(long)]
    pub direction: Option<String>,

    /// Only sessions active at or after this RFC 3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,

    /// Only sessions active at or before this RFC 3339 timestamp.
    #[arg(long)]
    pub until: Option<String>,

    /// Only sessions stamped with this acting subject.
    #[arg(long)]
    pub auth_subject: Option<String>,
}

/// A session id and where to find its server.
#[derive(Debug, Args)]
pub struct SessionIdArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The session id.
    pub id: String,
}

/// A session id plus the payload grain to fetch it at.
#[derive(Debug, Args)]
pub struct SessionPayloadArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The session id.
    pub id: String,

    /// `full` (default) or `preview`, which truncates payload strings
    /// server-side.
    #[arg(long)]
    pub payload: Option<String>,
}

/// `tapesctl traces` methods.
#[derive(Debug, Subcommand)]
pub enum TracesCommand {
    /// List trace summaries for one session.
    List(TracesListArgs),
    /// Fetch one trace with its spans.
    Get(TracesGetArgs),
}

/// Arguments for `tapesctl traces list`.
#[derive(Debug, Args)]
pub struct TracesListArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The session whose traces to list.
    pub session_id: String,
}

/// Arguments for `tapesctl traces get`.
#[derive(Debug, Args)]
pub struct TracesGetArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The trace id.
    pub trace_id: String,

    /// `full` (default) or `preview`.
    #[arg(long)]
    pub payload: Option<String>,
}

/// `tapesctl spans` methods.
///
/// Spans have no collection route of their own — they exist only inside a trace
/// — so `list` takes a trace id and prints that trace's spans.
#[derive(Debug, Subcommand)]
pub enum SpansCommand {
    /// List the spans of one trace.
    List(SpansListArgs),
    /// Fetch one span.
    Get(SpansGetArgs),
}

/// Arguments for `tapesctl spans list`.
#[derive(Debug, Args)]
pub struct SpansListArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The trace whose spans to list.
    pub trace_id: String,

    /// `full` (default) or `preview`.
    #[arg(long)]
    pub payload: Option<String>,
}

/// Arguments for `tapesctl spans get`.
#[derive(Debug, Args)]
pub struct SpansGetArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The trace the span belongs to.
    pub trace_id: String,

    /// The span id.
    pub span_id: String,
}

/// Arguments for `tapesctl export`.
#[derive(Debug, Args)]
pub struct ExportArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// The session to export.
    pub session_id: String,

    /// Export grain: `spans` (default) or `traces`.
    #[arg(long)]
    pub detail: Option<String>,

    /// Write to this file instead of stdout.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}

/// Arguments for `tapesctl seed`.
#[derive(Debug, Args)]
pub struct SeedArgs {
    #[command(flatten)]
    pub api: ApiArgs,
}

/// `tapesctl skill` subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Copy an authored skill into an agent's skills directory.
    Sync(SkillSyncArgs),
}

/// Arguments for `tapesctl skill sync`.
#[derive(Debug, Args)]
pub struct SkillSyncArgs {
    /// The skill name, without the `.md` suffix.
    pub name: String,

    /// Write into a Claude skills directory rather than the agent-neutral one.
    #[arg(long)]
    pub claude: bool,

    /// Write into this project rather than the user's home.
    #[arg(long)]
    pub local: bool,

    /// Report the destination without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Where authored skills live. Defaults to `~/.tapes/skills`.
    #[arg(long)]
    pub source_dir: Option<PathBuf>,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_well_formed() {
        // clap panics at runtime on a malformed definition — a duplicated short
        // flag between a flattened struct and its parent, for instance — so this
        // is the assertion that the surface is constructible at all.
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn resource_and_method_parse_as_two_words() {
        let cli = parse(&["tapesctl", "sessions", "list", "--tapes-url", "http://x"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions(SessionsCommand::List(_))),
        ));
    }

    #[test]
    fn a_span_is_addressed_by_its_trace_and_its_own_id() {
        let cli = parse(&[
            "tapesctl",
            "spans",
            "get",
            "t-1",
            "s-1",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Some(Command::Spans(SpansCommand::Get(args))) => {
                assert_eq!(args.trace_id, "t-1");
                assert_eq!(args.span_id, "s-1");
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn harness_arguments_after_start_are_not_eaten_by_tapesctl() {
        // `--verbose` here belongs to the harness; treating it as tapesctl's
        // would change what the user's command does.
        let cli = parse(&[
            "tapesctl",
            "start",
            "claude",
            "--tapes-url",
            "http://x",
            "--",
            "--verbose",
            "-p",
            "hi",
        ]);
        match cli.command {
            Some(Command::Start(args)) => {
                assert_eq!(args.harness, "claude");
                assert_eq!(args.harness_args, vec!["--verbose", "-p", "hi"]);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn sync_defaults_to_the_bounded_window_and_the_home_tree() {
        let cli = parse(&["tapesctl", "sync", "--tapes-url", "http://x"]);
        match cli.command {
            Some(Command::Sync(args)) => {
                assert_eq!(args.since_days, None);
                assert_eq!(args.projects_root, None);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn export_takes_a_short_output_flag_like_the_command_it_ports() {
        let cli = parse(&[
            "tapesctl",
            "export",
            "s-1",
            "-o",
            "out.jsonl",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Some(Command::Export(args)) => {
                assert_eq!(args.output, Some(PathBuf::from("out.jsonl")));
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn skill_sync_needs_no_server() {
        // It is a local file copy; requiring --tapes-url would be a lie.
        let cli = parse(&["tapesctl", "skill", "sync", "review", "--claude", "--local"]);
        match cli.command {
            Some(Command::Skill(SkillCommand::Sync(args))) => {
                assert_eq!(args.name, "review");
                assert!(args.claude);
                assert!(args.local);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn a_missing_required_positional_is_rejected() {
        assert!(Cli::try_parse_from(["tapesctl", "sessions", "get"]).is_err());
        assert!(Cli::try_parse_from(["tapesctl", "spans", "get", "t-1"]).is_err());
    }
}
