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

/// The flag, and the environment variable behind it, that name a server.
const TAPES_URL_FLAG: &str = "--tapes-url";

/// The environment variable `--tapes-url` falls back to.
pub const TAPES_URL_ENV: &str = "TAPES_URL";

/// Find the server to discover cassettes from, before anything is parsed.
///
/// This is a scan rather than a parse because of an ordering problem the derive
/// cannot solve: the cassette nouns have to exist in the parser *before* argv is
/// parsed, but the flag naming the server they come from is itself in argv. So
/// the flag is read off the raw arguments first, under both spellings clap
/// accepts, and the environment is the fallback exactly as it is for the
/// hand-written commands.
///
/// Reading a value that later fails to parse costs nothing: a bad URL yields no
/// cassettes here, and the real parse still reports it.
#[must_use]
pub fn discovery_url<I, S>(argv: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let joined = format!("{TAPES_URL_FLAG}=");
    let mut arguments = argv.into_iter();

    while let Some(argument) = arguments.next() {
        let argument = argument.as_ref();
        // Everything after a bare `--` belongs to the launched harness, not
        // to tapesctl — a harness flag that happens to be spelled
        // `--tapes-url` must not steer discovery.
        if argument == "--" {
            break;
        }
        let value = if let Some(value) = argument.strip_prefix(&joined) {
            Some(value.to_owned())
        } else if argument == TAPES_URL_FLAG {
            // A trailing `--tapes-url` with nothing after it is a mistake clap
            // will report; there is simply no value to discover from.
            arguments.next().map(|value| value.as_ref().to_owned())
        } else {
            None
        };

        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            return Some(value);
        }
    }

    std::env::var(TAPES_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Count `-v`/`--verbose` before anything is parsed.
///
/// Discovery runs ahead of the parse, and its tracing is the only account of why
/// a cassette did not appear — so the subscriber has to be installed before it,
/// which means the verbosity has to be read the same way the server URL is.
/// clap still owns the real flag; this only decides how loud the run before it
/// is.
#[must_use]
pub fn verbosity<I, S>(argv: I) -> u8
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut count: u8 = 0;
    for argument in argv {
        let argument = argument.as_ref();
        // Everything after a bare `--` is the harness's, exactly as in
        // `discovery_url`: a harness's own -v must not raise tapesctl's.
        if argument == "--" {
            break;
        }
        if argument == "--verbose" {
            count = count.saturating_add(1);
        } else if argument.len() > 1
            && argument.starts_with('-')
            && !argument.starts_with("--")
            && argument.chars().skip(1).all(|c| c == 'v')
        {
            // `-vv` is two, the same as clap's counting action reads it.
            count = count
                .saturating_add(u8::try_from(argument.len().saturating_sub(1)).unwrap_or(u8::MAX));
        }
    }
    count
}

/// Top-level subcommands.
///
/// The `<resource> <method>` surface is hand-written here for the core data
/// model — sessions, traces, spans. It sits alongside the *generated*
/// `<cassette> <method>` surface, which is discovered from `/v1/cassettes` at
/// runtime and covers resources this binary cannot know about at compile time;
/// see [`crate::cassette`].
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

impl Command {
    /// Whether this command gives the terminal to a child process.
    ///
    /// The one thing the logging setup needs to know before dispatch: a command
    /// that launches a TUI cannot also write to the terminal, so its diagnostics
    /// have to go somewhere else. See [`crate::logging`].
    #[must_use]
    pub fn hands_over_terminal(&self) -> bool {
        matches!(self, Self::Start(_))
    }
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
    fn the_discovery_server_is_found_under_both_spellings_clap_accepts() {
        // The cassette nouns must exist before argv is parsed, so this scan is
        // what stands in for the parse that has not happened yet.
        assert_eq!(
            discovery_url(["tapesctl", "sessions", "list", "--tapes-url", "http://x"]),
            Some("http://x".to_owned()),
        );
        assert_eq!(
            discovery_url(["tapesctl", "summary", "reports", "--tapes-url=http://y"]),
            Some("http://y".to_owned()),
        );
    }

    #[test]
    fn verbosity_after_the_separator_belongs_to_the_harness() {
        assert_eq!(
            verbosity(["tapesctl", "start", "claude", "--", "-vv", "--verbose"]),
            0
        );
        assert_eq!(
            verbosity(["tapesctl", "-v", "start", "claude", "--", "-vv"]),
            1
        );
    }

    #[test]
    fn flags_after_the_separator_belong_to_the_harness_not_discovery() {
        assert_eq!(
            discovery_url([
                "tapesctl",
                "start",
                "claude",
                "--",
                "--tapes-url",
                "http://evil"
            ]),
            std::env::var(TAPES_URL_ENV).ok().filter(|v| !v.is_empty()),
        );
    }

    #[test]
    fn a_dangling_server_flag_is_not_read_as_a_value() {
        assert_eq!(
            discovery_url(["tapesctl", "sessions", "list", "--tapes-url"]),
            std::env::var(TAPES_URL_ENV).ok().filter(|v| !v.is_empty()),
        );
    }

    #[test]
    fn a_missing_required_positional_is_rejected() {
        assert!(Cli::try_parse_from(["tapesctl", "sessions", "get"]).is_err());
        assert!(Cli::try_parse_from(["tapesctl", "spans", "get", "t-1"]).is_err());
    }
}
