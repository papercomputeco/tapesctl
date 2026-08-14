//! Command-line surface for `tapesctl`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The Tapes client CLI.
// Kept to one line on purpose: clap renders a struct's doc comment as the long
// `--help` about, so rationale written here would be printed at every user.
//
// The rationale, for a reader of the code: the subcommand is required rather
// than optional because a `tapesctl` with nothing to do should say what it
// *can* do. That is what makes clap answer a bare invocation — and a
// flags-only one like `tapesctl -v` — with help, instead of letting either
// reach dispatch with nothing to dispatch.
#[derive(Debug, Parser)]
#[command(
    name = "tapesctl",
    // Not clap's bare `version`, which would print the manifest's placeholder.
    // This is the stamped identity — the same block `tapesctl version` prints
    // above its canary, so the flag and the command agree by construction.
    version = crate::build_info::long_version(),
    about = "The Tapes client CLI",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Increase log verbosity (`-v` debug, `-vv` trace). `RUST_LOG` overrides.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Base URL of the tapes server, for every command that talks to one.
    ///
    /// Global so it can be given once, before the subcommand, and so it is
    /// documented in the top-level help rather than only in each leaf's.
    ///
    /// # Why no `env = "TAPES_URL"` here
    ///
    /// Deliberate, and the leaf declarations still carry it — this is not the
    /// flag losing its environment fallback. clap counts an environment-sourced
    /// value as an argument the user supplied, and `arg_required_else_help`
    /// only prints help when *no* argument was supplied. Binding `TAPES_URL` at
    /// the top level would therefore mean that anyone with the variable
    /// exported got `error: requires a subcommand` from a bare `tapesctl`
    /// instead of the help it now prints — a regression triggered by the
    /// environment, not by anything they typed. A default value carries no such
    /// weight (clap ranks it as non-explicit), which is why the configured
    /// server can be installed here as one; see [`crate::parser`].
    #[arg(
        long,
        global = true,
        value_name = "URL",
        help = "Base URL of the tapes server. Falls back to TAPES_URL, then to the configured default",
        long_help = "Base URL of the tapes server.\n\n\
                     Falls back to the TAPES_URL environment variable, and then to the default \
                     configured with `tapesctl config set tapes-url <url>`. With none of the three, \
                     commands that need a server refuse to run rather than guess a host."
    )]
    pub tapes_url: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// The argument id `--tapes-url` is known by, everywhere it is declared.
///
/// The derive takes it from the field name, so the hand-built declarations —
/// [`Cli::tapes_url`] here and the one decorated onto every generated cassette
/// method — have to spell the same id, not merely the same `--tapes-url`. Two
/// ids sharing one long name is a clap conflict the moment the global one
/// propagates into a command that declares the other, and the global one now
/// propagates everywhere.
pub const TAPES_URL_ARG: &str = "tapes_url";

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

/// Global flags that take a value in the space-separated spelling.
///
/// [`gated`]'s argv scan must not mistake a global flag's *value* for the
/// first subcommand: `tapesctl --tapes-url http://x cassettes …` names
/// `cassettes`, not `http://x`. Nothing has been parsed when the scan runs, so
/// it carries its own list of value-taking globals; a test pins the list to
/// the derived [`Cli`] so it cannot drift.
const VALUE_TAKING_GLOBALS: [&str; 1] = [TAPES_URL_FLAG];

/// The first token that can be a subcommand name, before any `--` cutoff.
fn first_noun(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1).map(String::as_str);
    while let Some(argument) = arguments.next() {
        // Everything after a bare `--` belongs to a launched harness, not to
        // tapesctl — a harness argument spelled `cassettes` must not run
        // discovery. The same cutoff `discovery_url` and `verbosity` apply.
        if argument == "--" {
            return None;
        }
        if VALUE_TAKING_GLOBALS.contains(&argument) {
            // Space-separated flag value; skip it. The `--flag=value`
            // spelling is one token and starts with `-`, so it needs no
            // special case.
            let _ = arguments.next();
            continue;
        }
        if !argument.starts_with('-') {
            return Some(argument);
        }
    }
    None
}

/// Whether this command line can possibly reach the generated cassette surface.
///
/// Only three shapes can: `tapesctl cassettes …` itself, `tapesctl help …`
/// (whose output may descend into the noun), and a bare / flags-only
/// invocation (whose help must list the noun and explain where its contents
/// come from). Every other verb builds its parser with **zero** discovery
/// I/O — no cache read, no network. The fixed [`crate::cassette::command::NOUN`]
/// literal is what makes this scan sufficient: since the retired top-level
/// aliases went, no first token other than these three can name a cassette.
#[must_use]
pub fn gated(argv: &[String]) -> bool {
    matches!(
        first_noun(argv),
        None | Some(crate::cassette::command::NOUN) | Some("help")
    )
}

/// Top-level subcommands.
///
/// The `<resource> <method>` surface is hand-written here for the core data
/// model — sessions, traces, spans. It sits alongside the *generated*
/// `cassettes <name> <method>` surface, which is discovered from `/v1/cassettes`
/// at runtime and covers resources this binary cannot know about at compile
/// time. That one is mounted onto this enum's command rather than declared in
/// it, because a variant has to exist at compile time and a cassette does not;
/// see [`crate::cassette`].
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch a harness under a just-in-time capture proxy.
    Start(StartArgs),

    /// Capture a harness that launches itself, such as the Codex desktop app.
    ///
    /// `start` owns the harness process; this does not. The proxy binds the
    /// address the harness was *installed* against and serves until
    /// interrupted, so an app the user starts from the dock is captured for
    /// whichever of its sessions run in that window.
    Capture(CaptureArgs),

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

    /// Semantic search over captured spans.
    Search(SearchArgs),

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

    /// Read and write the answers you should only have to give once.
    #[command(subcommand)]
    Config(ConfigCommand),

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
    /// The harness to launch (e.g. `claude`, `codex`, `pi`).
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

    /// Which upstream API schema the proxy fronts: `anthropic` (the default) or
    /// `openai`.
    ///
    /// Only meaningful for a harness that speaks several — `pi` redirects all
    /// of its providers to one endpoint, so this is what picks the
    /// upstream, the wire format ingest reduces, and the schema the extension
    /// reports. A
    /// harness that speaks exactly one schema takes it from the harness instead,
    /// and passing this there is an error rather than a silent no-op.
    #[arg(long)]
    pub schema: Option<String>,

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

/// Arguments for `tapesctl capture`.
///
/// Deliberately a subset of [`StartArgs`]. There is no `--schema` — a harness
/// nobody launches speaks whatever its own config says — and no
/// `--no-transcripts`, because the only harness on this surface writes Codex
/// rollouts, which the transcript lane does not walk.
#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// The harness to capture (today: `codex-app`).
    pub harness: String,

    /// Base URL of the tapes ingest server. Falls back to `TAPES_URL`.
    #[arg(long, env = "TAPES_URL")]
    pub tapes_url: Option<String>,

    /// Where to forward the harness's LLM traffic. Defaults to the backend
    /// that honours the credential the harness was configured with.
    #[arg(long, env = "TAPES_UPSTREAM")]
    pub upstream: Option<String>,

    /// Base URL of the web console, used to print links to captured sessions.
    #[arg(long, env = "TAPES_WEB_URL")]
    pub web_url: Option<String>,

    /// Org id to stamp on captured turns. Must be a UUID; empty (the default)
    /// selects the server's local sentinel org.
    #[arg(long, env = "TAPES_ORG_ID")]
    pub org_id: Option<String>,

    /// Acting subject to stamp on captured turns. Defaults to
    /// `local:<username>`.
    #[arg(long, env = "TAPES_AUTH_SUBJECT")]
    pub auth_subject: Option<String>,
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

    /// Only the session captured from this harness session id — the id
    /// `start` prints (distinct from the tapes session id read commands
    /// take). The server takes the harness filter only as a pair, so
    /// `--harness-id` must come with it.
    #[arg(long, requires = "harness_id")]
    pub harness_session_id: Option<String>,

    /// The harness the session ran under (e.g. `claude`), naming the
    /// other half of the harness filter pair.
    #[arg(long, requires = "harness_session_id")]
    pub harness_id: Option<String>,

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

/// Arguments for `tapesctl search`.
///
/// Hits are individual main-conversation LLM spans with their trace and turn
/// context — "find the turn where X happened". It needs a server whose span
/// embeddings have been written, which is why an unconfigured deployment
/// answers 503 rather than an empty result set.
#[derive(Debug, Args)]
pub struct SearchArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// What to search for.
    pub query: String,

    /// How many hits to return. The server has no ceiling on this.
    #[arg(long, short = 'k', default_value_t = crate::api::client::DEFAULT_SEARCH_TOP_K)]
    pub top: u64,

    /// Print only session ids, one per line, deduplicated in score order.
    ///
    /// The shape `skill generate` takes as positional arguments, so the two
    /// compose: `tapesctl skill generate $(tapesctl search "..." -q -k 1)`.
    #[arg(long, short = 'q')]
    pub quiet: bool,
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
///
/// `generate` carries far more flags than its siblings, so the variants differ
/// in size. Boxing it is the usual fix and is not available here — clap derives
/// a subcommand from a type implementing `Args`, which `Box<T>` does not — and
/// the enum is built exactly once, at parse time, so the difference costs
/// nothing worth contorting the surface for.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Extract a skill from one or more captured sessions.
    Generate(SkillGenerateArgs),

    /// List authored skills.
    List(SkillListArgs),

    /// Copy an authored skill into an agent's skills directory.
    Sync(SkillSyncArgs),
}

/// Arguments for `tapesctl skill generate`.
///
/// Two servers are involved and they are not the same one: the tapes API
/// supplies the session transcript, and an LLM provider does the extraction.
/// `--tapes-url` addresses the first; `--provider`/`--model`/`--api-key`
/// address the second.
#[derive(Debug, Args)]
pub struct SkillGenerateArgs {
    #[command(flatten)]
    pub api: ApiArgs,

    /// Sessions to extract from. Takes priority over `--search`.
    pub session_ids: Vec<String>,

    /// Skill name, kebab-case.
    #[arg(long)]
    pub name: String,

    /// `workflow` (default), `domain-knowledge`, or `prompt-template`.
    #[arg(long = "type", default_value = "workflow")]
    pub skill_type: String,

    /// Render the generated skill without writing it.
    #[arg(long)]
    pub preview: bool,

    /// LLM provider: `openai` (default), `anthropic`, or `ollama`.
    #[arg(long, default_value = "openai")]
    pub provider: String,

    /// Model for the extraction call. Each provider has its own default.
    #[arg(long)]
    pub model: Option<String>,

    /// API key for the LLM provider.
    ///
    /// Prefer the provider's environment variable — a key passed here is
    /// visible in the process list and in shell history to everything on the
    /// machine, for as long as the command runs.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Only include turns starting on or after this date (`YYYY-MM-DD` or
    /// RFC 3339).
    #[arg(long)]
    pub since: Option<String>,

    /// Only include turns starting on or before this date.
    #[arg(long)]
    pub until: Option<String>,

    /// Resolve sessions by span search instead of naming them.
    #[arg(long)]
    pub search: Option<String>,

    /// How many search hits to draw sessions from.
    #[arg(long = "search-top", default_value_t = 3)]
    pub search_top: u64,

    /// Where authored skills are written. Defaults to `~/.tapes/skills`.
    #[arg(long)]
    pub source_dir: Option<PathBuf>,
}

/// Arguments for `tapesctl skill list`.
#[derive(Debug, Args)]
pub struct SkillListArgs {
    /// Only show skills of this type.
    #[arg(long = "type")]
    pub skill_type: Option<String>,

    /// Where authored skills live. Defaults to `~/.tapes/skills`.
    #[arg(long)]
    pub source_dir: Option<PathBuf>,
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
    /// Install the capture plugin for a harness.
    Install(PluginInstallArgs),

    /// Remove a harness's capture plugin and any configuration it wrote.
    Uninstall(PluginUninstallArgs),

    /// Report one lifecycle event to a running capture proxy.
    ///
    /// Invoked by an installed hook plugin, never by a person: the harness
    /// writes the event payload to this process's stdin. Hidden because a user
    /// typing it has nothing to pipe in.
    #[command(hide = true)]
    Hook(PluginHookArgs),
}

/// Arguments for `tapesctl plugin install`.
#[derive(Debug, Args)]
pub struct PluginInstallArgs {
    /// The harness to install capture support for (e.g. `pi`, `opencode`,
    /// `codex-app`). Harnesses
    /// captured by redirection alone need no plugin and report so.
    pub harness: String,

    /// Report what would be installed, and where, without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Loopback port the capture proxy for this harness will bind.
    ///
    /// Only meaningful for a harness that is configured once and captured many
    /// times: its endpoint is written into a config file that outlives any one
    /// capture, so the port cannot be ephemeral the way `start`'s is. Omitted,
    /// a free port is chosen at install time and recorded. Re-run with an
    /// explicit port to move off one something else has since taken.
    #[arg(long)]
    pub port: Option<u16>,

    /// Which credential the harness will present upstream: `chatgpt` (the
    /// default, what the desktop app uses after a plan login) or `api-key`.
    #[arg(long)]
    pub codex_auth: Option<String>,
}

/// Arguments for `tapesctl plugin uninstall`.
#[derive(Debug, Args)]
pub struct PluginUninstallArgs {
    /// The harness to remove capture support for.
    pub harness: String,

    /// Report what would be removed, without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// `tapesctl config` subcommands.
///
/// Key/value rather than a flag per setting — `config set tapes-url <url>`,
/// not `config set --tapes-url <url>` — so the surface does not have to grow a
/// verb, a flag, and a printer for every future key. `git config` and `gh
/// config` are the same shape, which is most of why it is this one.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set one configuration key.
    Set(ConfigSetArgs),

    /// Print one configuration key, or every key that is set.
    Get(ConfigGetArgs),

    /// Print the path of the configuration file, whether or not it exists.
    ///
    /// The answer to "where would I edit this by hand", and the one thing that
    /// is still worth printing when the file will not parse.
    Path,
}

/// Arguments for `tapesctl config set`.
#[derive(Debug, Args)]
pub struct ConfigSetArgs {
    /// The key to set. Today: `tapes-url`.
    pub key: String,

    /// The value to store.
    pub value: String,
}

/// Arguments for `tapesctl config get`.
#[derive(Debug, Args)]
pub struct ConfigGetArgs {
    /// The key to print. Omitted, every key that is set is printed as
    /// `<key> = <value>`.
    pub key: Option<String>,
}

/// Arguments for `tapesctl plugin hook`.
#[derive(Debug, Args)]
pub struct PluginHookArgs {
    /// The harness whose hook surface this event came from.
    pub harness: String,

    /// The handoff file naming the capture proxy, and carrying the secret that
    /// authenticates a report to it.
    ///
    /// Passed explicitly rather than derived from the environment: this command
    /// line is written by the installer, which knows where it put the file,
    /// while a hook runs under whatever environment the harness happens to
    /// have.
    #[arg(long)]
    pub handoff: PathBuf,
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
    fn the_server_can_be_named_before_the_subcommand_and_still_reaches_it() {
        // The point of the global: `--tapes-url` given once, at the front,
        // where a shell alias or a wrapper script would put it.
        let cli = parse(&["tapesctl", "--tapes-url", "http://x", "sessions", "list"]);
        assert_eq!(cli.tapes_url.as_deref(), Some("http://x"));
        match cli.command {
            Command::Sessions(SessionsCommand::List(args)) => {
                assert_eq!(
                    args.api.tapes_url.as_deref(),
                    Some("http://x"),
                    "a global value must reach the leaf that consumes it",
                );
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn the_server_named_at_the_leaf_still_wins_and_still_works() {
        // The spelling every existing script uses. It has to keep working, and
        // it has to beat the global — the more specific position is the one the
        // user meant.
        let cli = parse(&[
            "tapesctl",
            "--tapes-url",
            "http://global",
            "sessions",
            "list",
            "--tapes-url",
            "http://leaf",
        ]);
        match cli.command {
            Command::Sessions(SessionsCommand::List(args)) => {
                assert_eq!(args.api.tapes_url.as_deref(), Some("http://leaf"));
            }
            other => panic!("got: {other:?}"),
        }
    }

    /// The global flag deliberately does not bind `TAPES_URL`, and this is the
    /// guard on that: clap treats an environment-sourced value as an argument
    /// the user supplied, so a top-level `env` binding would make a bare
    /// `tapesctl` answer "requires a subcommand" instead of help on every
    /// machine with the variable exported. The leaf declarations carry the
    /// binding, so the fallback itself is unaffected.
    #[test]
    fn the_global_server_flag_leaves_the_environment_to_the_leaves() {
        let command = Cli::command();
        let global = command
            .get_arguments()
            .find(|arg| arg.get_id() == TAPES_URL_ARG)
            .expect("the global flag should exist");
        assert!(global.is_global_set());
        assert!(
            global.get_env().is_none(),
            "binding TAPES_URL here would cost the bare invocation its help",
        );

        let leaf = command
            .find_subcommand("seed")
            .and_then(|sub| {
                sub.get_arguments()
                    .find(|arg| arg.get_id() == TAPES_URL_ARG)
                    .cloned()
            })
            .expect("a leaf should declare the flag itself");
        assert_eq!(leaf.get_env(), Some(std::ffi::OsStr::new(TAPES_URL_ENV)));
    }

    #[test]
    fn config_reads_and_writes_one_key_at_a_time() {
        let cli = parse(&["tapesctl", "config", "set", "tapes-url", "http://x"]);
        match cli.command {
            Command::Config(ConfigCommand::Set(args)) => {
                assert_eq!(args.key, "tapes-url");
                assert_eq!(args.value, "http://x");
            }
            other => panic!("got: {other:?}"),
        }

        // `get` with no key is the whole file, which is why the key is optional
        // here and required in `set`.
        match parse(&["tapesctl", "config", "get"]).command {
            Command::Config(ConfigCommand::Get(args)) => assert_eq!(args.key, None),
            other => panic!("got: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["tapesctl", "config", "set", "tapes-url"]).is_err(),
            "a set with no value would have nothing to store",
        );
    }

    #[test]
    fn config_needs_no_server() {
        // It writes a local file. Requiring --tapes-url to configure
        // --tapes-url would be a circle.
        assert!(Cli::try_parse_from(["tapesctl", "config", "path"]).is_ok());
    }

    #[test]
    fn resource_and_method_parse_as_two_words() {
        let cli = parse(&["tapesctl", "sessions", "list", "--tapes-url", "http://x"]);
        assert!(matches!(
            cli.command,
            Command::Sessions(SessionsCommand::List(_)),
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
            Command::Spans(SpansCommand::Get(args)) => {
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
            Command::Start(args) => {
                assert_eq!(args.harness, "claude");
                assert_eq!(args.harness_args, vec!["--verbose", "-p", "hi"]);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn the_schema_flag_is_tapesctls_and_the_harnesss_own_args_still_pass_through() {
        // `--schema` picks which upstream this capture fronts, so it must be
        // read here — while anything after `--` still belongs to pi.
        let cli = parse(&[
            "tapesctl",
            "start",
            "pi",
            "--tapes-url",
            "http://x",
            "--schema",
            "openai",
            "--",
            "--model",
            "gpt-5",
        ]);
        match cli.command {
            Command::Start(args) => {
                assert_eq!(args.harness, "pi");
                assert_eq!(args.schema.as_deref(), Some("openai"));
                assert_eq!(args.harness_args, vec!["--model", "gpt-5"]);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn sync_defaults_to_the_bounded_window_and_the_home_tree() {
        let cli = parse(&["tapesctl", "sync", "--tapes-url", "http://x"]);
        match cli.command {
            Command::Sync(args) => {
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
            Command::Export(args) => {
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
            Command::Skill(SkillCommand::Sync(args)) => {
                assert_eq!(args.name, "review");
                assert!(args.claude);
                assert!(args.local);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn plugin_install_names_a_harness_and_needs_no_server() {
        // Installing a plugin is a local file copy over crate-owned bytes;
        // nothing is fetched, so requiring --tapes-url would be a lie.
        let cli = parse(&["tapesctl", "plugin", "install", "pi"]);
        match cli.command {
            Command::Plugin(PluginCommand::Install(args)) => {
                assert_eq!(args.harness, "pi");
                assert!(!args.dry_run);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn search_defaults_match_the_command_it_ports() {
        let cli = parse(&[
            "tapesctl",
            "search",
            "gum glow charm",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Command::Search(args) => {
                assert_eq!(args.query, "gum glow charm");
                assert_eq!(args.top, 5, "the Go default is 5");
                assert!(!args.quiet);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn capture_names_a_harness_it_does_not_launch_and_takes_no_trailing_args() {
        // There is no child process, so there is nothing for `--` to pass
        // through to — unlike `start`, whose trailing args are the harness's.
        let cli = parse(&[
            "tapesctl",
            "capture",
            "codex-app",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Command::Capture(args) => {
                assert_eq!(args.harness, "codex-app");
                assert_eq!(args.tapes_url.as_deref(), Some("http://x"));
            }
            other => panic!("got: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["tapesctl", "capture", "codex-app", "--", "-p", "hi"]).is_err(),
        );
    }

    /// A capture keeps the terminal for its whole run — nothing is handed a
    /// TUI — so its diagnostics belong on the terminal rather than in a file.
    #[test]
    fn capture_does_not_hand_over_the_terminal() {
        let cli = parse(&[
            "tapesctl",
            "capture",
            "codex-app",
            "--tapes-url",
            "http://x",
        ]);
        assert!(!cli.command.hands_over_terminal());
    }

    #[test]
    fn a_hook_invocation_names_its_harness_and_its_handoff() {
        // The installer writes this command line; a person never types it.
        let cli = parse(&[
            "tapesctl",
            "plugin",
            "hook",
            "codex-app",
            "--handoff",
            "/home/someone/.tapes/codex-app/handoff.json",
        ]);
        match cli.command {
            Command::Plugin(PluginCommand::Hook(args)) => {
                assert_eq!(args.harness, "codex-app");
                assert_eq!(
                    args.handoff,
                    PathBuf::from("/home/someone/.tapes/codex-app/handoff.json"),
                );
            }
            other => panic!("got: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["tapesctl", "plugin", "hook", "codex-app"]).is_err(),
            "--handoff is required: without it a hook has nowhere to report",
        );
    }

    #[test]
    fn plugin_uninstall_mirrors_install() {
        let cli = parse(&["tapesctl", "plugin", "uninstall", "codex-app", "--dry-run"]);
        match cli.command {
            Command::Plugin(PluginCommand::Uninstall(args)) => {
                assert_eq!(args.harness, "codex-app");
                assert!(args.dry_run);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn plugin_install_takes_a_dry_run() {
        let cli = parse(&["tapesctl", "plugin", "install", "pi", "--dry-run"]);
        match cli.command {
            Command::Plugin(PluginCommand::Install(args)) => assert!(args.dry_run),
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn search_keeps_the_short_flags_muscle_memory_expects() {
        let cli = parse(&[
            "tapesctl",
            "search",
            "hooks",
            "-k",
            "10",
            "-q",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Command::Search(args) => {
                assert_eq!(args.top, 10);
                assert!(args.quiet);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn a_negative_result_count_is_refused_by_the_parser() {
        // The server would 400 on `top_k <= 0`; rejecting it here costs no
        // round trip and names the flag.
        assert!(Cli::try_parse_from(["tapesctl", "search", "x", "-k", "-1"]).is_err());
    }

    #[test]
    fn generate_requires_a_name_and_defaults_the_rest() {
        let cli = parse(&[
            "tapesctl",
            "skill",
            "generate",
            "s-1",
            "s-2",
            "--name",
            "debug-hooks",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Command::Skill(SkillCommand::Generate(args)) => {
                assert_eq!(args.session_ids, vec!["s-1", "s-2"]);
                assert_eq!(args.name, "debug-hooks");
                assert_eq!(args.skill_type, "workflow");
                assert_eq!(args.provider, "openai");
                assert_eq!(args.search_top, 3);
                assert!(!args.preview);
            }
            other => panic!("got: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["tapesctl", "skill", "generate", "s-1"]).is_err(),
            "--name is required",
        );
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
            discovery_url([
                "tapesctl",
                "cassettes",
                "summary",
                "reports",
                "--tapes-url=http://y"
            ]),
            Some("http://y".to_owned()),
        );
    }

    #[test]
    fn generate_can_take_its_sessions_from_a_search_instead() {
        let cli = parse(&[
            "tapesctl",
            "skill",
            "generate",
            "--search",
            "react hooks",
            "--search-top",
            "5",
            "--name",
            "react-debug",
            "--tapes-url",
            "http://x",
        ]);
        match cli.command {
            Command::Skill(SkillCommand::Generate(args)) => {
                assert!(args.session_ids.is_empty());
                assert_eq!(args.search.as_deref(), Some("react hooks"));
                assert_eq!(args.search_top, 5);
            }
            other => panic!("got: {other:?}"),
        }
    }

    #[test]
    fn skill_list_needs_no_server() {
        // It reads the authoring directory; requiring --tapes-url would be a lie.
        let cli = parse(&["tapesctl", "skill", "list", "--type", "workflow"]);
        match cli.command {
            Command::Skill(SkillCommand::List(args)) => {
                assert_eq!(args.skill_type.as_deref(), Some("workflow"));
            }
            other => panic!("got: {other:?}"),
        }
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

    #[test]
    fn a_lone_harness_filter_flag_is_rejected_at_parse() {
        // The server takes the harness filter only as a pair — a lone
        // param is a 400 — so the parser refuses the shapes the server
        // would refuse, with a message that names the missing half.
        assert!(
            Cli::try_parse_from([
                "tapesctl",
                "sessions",
                "list",
                "--harness-session-id",
                "sid"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["tapesctl", "sessions", "list", "--harness-id", "claude"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "tapesctl",
                "sessions",
                "list",
                "--harness-session-id",
                "sid",
                "--harness-id",
                "claude",
            ])
            .is_ok()
        );
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn value_taking_globals_track_the_real_cli() {
        // The gate's list must match the derived CLI exactly: a value-taking
        // global missing here makes the gate misread that flag's value as the
        // subcommand, and a listed flag that stopped taking a value makes the
        // gate swallow a real subcommand.
        let command = Cli::command();
        let mut expected: Vec<String> = command
            .get_arguments()
            .filter(|a| a.is_global_set() && a.get_action().takes_values())
            .map(|a| format!("--{}", a.get_long().expect("globals are long flags")))
            .collect();
        expected.sort();
        let mut actual: Vec<String> = VALUE_TAKING_GLOBALS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn only_cassettes_help_and_bare_invocations_are_gated_in() {
        assert!(gated(&argv(&["tapesctl"])));
        assert!(gated(&argv(&["tapesctl", "--help"])));
        assert!(gated(&argv(&["tapesctl", "-v"])));
        assert!(gated(&argv(&["tapesctl", "cassettes"])));
        assert!(gated(&argv(&[
            "tapesctl",
            "cassettes",
            "summary",
            "reports"
        ])));
        assert!(gated(&argv(&["tapesctl", "help"])));
        assert!(gated(&argv(&["tapesctl", "help", "cassettes"])));

        assert!(!gated(&argv(&["tapesctl", "sessions", "list"])));
        assert!(!gated(&argv(&["tapesctl", "start", "claude"])));
        assert!(!gated(&argv(&["tapesctl", "version"])));
        assert!(!gated(&argv(&["tapesctl", "config", "get", "tapes-url"])));
    }

    #[test]
    fn a_global_flag_value_is_not_mistaken_for_the_subcommand() {
        // `http://x` is --tapes-url's value, not the first noun.
        assert!(gated(&argv(&[
            "tapesctl",
            "--tapes-url",
            "http://x",
            "cassettes",
            "summary",
        ])));
        assert!(!gated(&argv(&[
            "tapesctl",
            "--tapes-url",
            "http://x",
            "sessions",
            "list",
        ])));
        // The `=` spelling is one token and needs no lookahead.
        assert!(!gated(&argv(&[
            "tapesctl",
            "--tapes-url=http://x",
            "sessions",
            "list",
        ])));
    }

    #[test]
    fn tokens_after_a_bare_dash_dash_never_gate_discovery_in() {
        // Everything after `--` belongs to the launched harness; a harness
        // argument spelled `cassettes` must not cost a discovery round trip.
        assert!(!gated(&argv(&[
            "tapesctl",
            "start",
            "claude",
            "--",
            "cassettes"
        ])));
        // A `--` with nothing before it means no subcommand of tapesctl's own
        // was named — and nothing after it is tapesctl's either.
        assert!(gated(&argv(&["tapesctl", "--", "sessions"])));
    }
}
