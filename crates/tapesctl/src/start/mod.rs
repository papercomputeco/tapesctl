//! `tapesctl start <harness>` — launch a harness under a just-in-time proxy.
//!
//! The proxy is *just in time* in the literal sense: it is bound when the
//! command starts, it exists only to serve one harness process, and it dies
//! with that process. There is no daemon, no port to reserve, and no state that
//! outlives the session — the listener takes an ephemeral port, the harness is
//! pointed at it by a launch recipe, and when the harness exits the server is
//! shut down and any config the recipe asked for is removed.
//!
//! # Where each piece of knowledge lives
//!
//! Almost nothing here is tapesctl's own. Directing a harness at a proxy comes
//! from `tapes_harnesses::launch`; deciding who sent a request comes from
//! `tapes_harnesses::attribution`; the header contract comes from
//! `tapes_harnesses::envelope`. What is genuinely local is deployment
//! knowledge: which upstream to forward to, which ingest server to post to,
//! what this client calls its own Codex provider, and what identity to stamp.
//! That split is deliberate — it is the same split paperd makes, which is what
//! keeps the two capture paths producing identical rows.
//!
//! # Two ways a harness reaches the proxy
//!
//! Claude and codex are *redirected*: they have a base-URL knob, a recipe sets
//! it, and nothing has to be installed. pi has no such knob, so it is captured
//! from the inside by an extension — `tapes-harnesses` owns the asset,
//! [`crate::plugin`] installs it, and this module's job is only to point an
//! already-installed extension at this proxy through the crate's environment
//! contract. That split is why `start pi` refuses to launch when the extension
//! is absent instead of running an uncaptured session.
//!
//! The consequence for attribution is the interesting one. A redirected harness
//! is identified from the outside, by peer PID; pi cannot be, so it stamps its
//! own `X-Tapes-*` envelope from within. For those harnesses the request's own
//! headers are the better evidence, and the proxy files the turn under them
//! rather than under this process's failure to recognise the peer.
//!
//! # The terminal is not ours
//!
//! Unlike paperd, this runs in the foreground of the terminal it hands to a
//! harness TUI. Between [`spawn_harness`] and the harness's exit, this process
//! must write nothing to stdout or stderr — a log line or a status print lands
//! inside someone's half-rendered frame. Diagnostics go to a file for the whole
//! of that window (see [`crate::logging`]), and the two things worth saying are
//! said on either side of it: the log path before the launch, the session link
//! after the exit.

pub mod ingest;
pub mod peek;
pub mod proxy;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use snafu::{OptionExt, ResultExt};
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, claude_session, codex_session,
    spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::envelope::{HARNESS_ID_CLAUDE, HARNESS_ID_CODEX, HARNESS_ID_PI};
use tapes_harnesses::harness as registry;
use tapes_harnesses::launch::{
    ClaudeRecipe, CodexAuth, CodexRecipe, LaunchPlan, LaunchRecipe, ProxyEndpoint,
    resolve_codex_auth,
};
use tapes_harnesses::plugin::{GATEWAY_SCHEMA_ENV, GATEWAY_URL_ENV};
use tokio::sync::mpsc::unbounded_channel;
use tracing::{info, warn};
use url::Url;

use crate::cli::StartArgs;
use crate::error::{Result, error};
use crate::logging;
use crate::transcript::client::TranscriptClient;
use crate::transcript::tailer::{self, SessionTracker};
use ingest::IngestClient;
use proxy::ProxyState;

/// Header this client asks Codex to stamp so concurrent Codex processes on one
/// loopback endpoint can be told apart.
///
/// The name is deliberately tapesctl's own rather than the crate's: it is a
/// private channel between this client and its own proxy, and a shared name
/// would collide with paperd's when both are capturing.
pub const CODEX_MARKER_HEADER: &str = "X-Tapesctl-Codex-Attribution";

/// Stable prefix of the Codex provider id this client declares. The launched
/// provider id is this plus a per-process suffix, which is exactly the
/// exact-or-suffixed shape [`CodexProviderFilter`] matches.
pub const CODEX_PROVIDER_PREFIX: &str = "tapesctl-openai";

/// Default upstream for Claude traffic when the caller names none.
pub const DEFAULT_ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// Default upstream for Codex traffic authenticated with an API key.
pub const DEFAULT_OPENAI_UPSTREAM: &str = "https://api.openai.com";

/// Default upstream for Codex traffic authenticated with a ChatGPT-plan login.
///
/// A separate host because OpenAI honours plan OAuth tokens only on the
/// ChatGPT backend, never on `api.openai.com` — the credential decides the
/// route, not a preference.
pub const DEFAULT_CHATGPT_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";

/// Which upstream API schema the proxy fronts.
///
/// One proxy forwards to one upstream, so a harness that speaks several schemas
/// has to be told which one this capture is for. The spellings are the crate's:
/// they are simultaneously the values the pi extension recognises (its
/// `SCHEMA_PROVIDERS`) and the `provider` names ingest keys its reducer on. That
/// those two sets coincide is a contract, not a coincidence — a schema whose
/// extension spelling differed from its ingest spelling would capture turns the
/// server could not reduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamSchema {
    /// The Anthropic Messages API.
    Anthropic,
    /// The OpenAI API.
    OpenAi,
}

impl UpstreamSchema {
    /// Resolve a user-typed schema name.
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAi),
            other => error::InvalidSchemaSnafu {
                schema: other.to_owned(),
            }
            .fail(),
        }
    }

    /// The wire name — both the ingest provider and the extension's schema hint.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    /// Default upstream for traffic in this schema.
    #[must_use]
    pub fn default_upstream(self) -> &'static str {
        match self {
            Self::Anthropic => DEFAULT_ANTHROPIC_UPSTREAM,
            Self::OpenAi => DEFAULT_OPENAI_UPSTREAM,
        }
    }
}

/// The schema a pi capture fronts when the user names none.
///
/// Anthropic because that is the provider pi ships selected, so the default
/// captures the default session.
pub const DEFAULT_PI_SCHEMA: UpstreamSchema = UpstreamSchema::Anthropic;

/// Which harness is being launched, and everything that differs between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code, over the Anthropic Messages API.
    Claude,
    /// Codex, over the OpenAI Responses API.
    Codex,
    /// pi, captured from inside by the crate's gateway extension.
    Pi,
}

/// Every harness `start` has an arm for, in the order they should be offered.
///
/// The registry is deliberately the wider set: it lists every harness the shared
/// crate knows, including ones no arm here launches yet (opencode). This is the
/// narrower claim — what this binary can actually do — and the error message for
/// an unsupported name is derived from it so the two cannot drift.
pub const SUPPORTED: &[Harness] = &[Harness::Claude, Harness::Codex, Harness::Pi];

impl Harness {
    /// Resolve a user-typed harness name.
    ///
    /// Resolution goes through the shared registry rather than a local match, so
    /// the names `start` accepts are the names every other command accepts —
    /// aliases and casing included. A harness the registry knows but this binary
    /// has no arm for fails here, with the same message as a name nobody knows:
    /// from the user's side both mean "not something `tapesctl start` launches".
    pub fn parse(name: &str) -> Result<Self> {
        let resolved = registry::find(name).and_then(|harness| match harness.id() {
            HARNESS_ID_CLAUDE => Some(Self::Claude),
            HARNESS_ID_CODEX => Some(Self::Codex),
            HARNESS_ID_PI => Some(Self::Pi),
            _ => None,
        });
        resolved.context(error::UnsupportedHarnessSnafu {
            harness: name.trim().to_owned(),
            supported: supported_names(),
        })
    }

    /// The canonical harness id — the registry's, and the envelope's.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => HARNESS_ID_CLAUDE,
            Self::Codex => HARNESS_ID_CODEX,
            Self::Pi => HARNESS_ID_PI,
        }
    }

    /// The binary to execute.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    /// The ingest `provider` family this harness's traffic is in. Ingest keys
    /// its server-side reducer on this, so it must name the wire format of the
    /// bytes actually captured — not the vendor of the harness.
    ///
    /// pi speaks whichever schema this capture fronts, so it is the one harness
    /// whose provider is not fixed by the harness.
    #[must_use]
    pub fn provider(self, schema: Option<UpstreamSchema>) -> &'static str {
        match self {
            Self::Claude => "anthropic",
            Self::Codex => "openai",
            Self::Pi => schema.unwrap_or(DEFAULT_PI_SCHEMA).as_str(),
        }
    }

    /// Default upstream when none is supplied.
    ///
    /// Codex's default depends on how it will authenticate, because the two
    /// credential kinds are accepted by different hosts; pi's depends on the
    /// schema being captured.
    #[must_use]
    pub fn default_upstream(
        self,
        auth: Option<CodexAuth>,
        schema: Option<UpstreamSchema>,
    ) -> &'static str {
        match (self, auth) {
            (Self::Claude, _) => DEFAULT_ANTHROPIC_UPSTREAM,
            (Self::Codex, Some(CodexAuth::ChatGpt)) => DEFAULT_CHATGPT_UPSTREAM,
            (Self::Codex, _) => DEFAULT_OPENAI_UPSTREAM,
            (Self::Pi, _) => schema.unwrap_or(DEFAULT_PI_SCHEMA).default_upstream(),
        }
    }

    /// Whether requests take the Codex attribution lane.
    #[must_use]
    pub fn is_codex(self) -> bool {
        matches!(self, Self::Codex)
    }

    /// Whether this harness stamps its own `X-Tapes-*` envelope from inside.
    ///
    /// Read from the registry rather than matched on here: which harnesses
    /// attribute themselves is harness knowledge, and the crate states it. A
    /// harness that gains an in-harness extension therefore takes this lane
    /// without this file changing.
    #[must_use]
    pub fn is_self_attributing(self) -> bool {
        registry::find(self.id()).is_some_and(|harness| {
            harness.attribution() == registry::AttributionStrategy::SelfAttributing
        })
    }

    /// The plugin artifacts that must already be installed for this harness's
    /// traffic to be captured at all. Empty for a harness captured by redirect.
    #[must_use]
    pub fn required_plugin_artifacts(self) -> &'static [tapes_harnesses::plugin::PluginArtifact] {
        registry::find(self.id()).map_or(&[], |harness| harness.plugin_artifacts())
    }

    /// Build the endpoint the harness should be pointed at.
    ///
    /// The path suffix is deployment knowledge, so it is decided here rather
    /// than in the recipe. Codex appends `/responses` to whatever it is given,
    /// and the two auth modes reach different paths: OpenAI's responses route
    /// is `/v1/responses`, so an API-key endpoint ends at a `/v1` segment,
    /// while the ChatGPT backend has no `/v1` component and its endpoint ends
    /// at the backend segment. Claude appends `/v1/messages` itself and needs
    /// no suffix at all, and pi's providers each append their own full path.
    #[must_use]
    pub fn endpoint_for(self, addr: SocketAddr, auth: Option<CodexAuth>) -> ProxyEndpoint {
        match (self, auth) {
            (Self::Claude | Self::Pi, _) | (Self::Codex, Some(CodexAuth::ChatGpt)) => {
                ProxyEndpoint::new(&format!("http://{addr}"))
            }
            (Self::Codex, _) => ProxyEndpoint::new(&format!("http://{addr}/v1")),
        }
    }

    /// Build the launch plan that points this harness at `endpoint`.
    pub fn plan(
        self,
        endpoint: ProxyEndpoint,
        provider_id: &str,
        auth: Option<CodexAuth>,
        schema: Option<UpstreamSchema>,
    ) -> Result<LaunchPlan> {
        match self {
            Self::Claude => ClaudeRecipe::new(endpoint).plan(),
            Self::Codex => {
                CodexRecipe::new(endpoint, auth.unwrap_or(CodexAuth::ChatGpt), provider_id)
                    .with_display_name("tapesctl capture")
                    .with_attribution_header(CODEX_MARKER_HEADER)
                    .plan()
            }
            // Not a crate recipe: pi is `LaunchSupport::ConsumerOwned`, so the
            // plan is built here. See [`pi_plan`].
            Self::Pi => return Ok(pi_plan(&endpoint, schema.unwrap_or(DEFAULT_PI_SCHEMA))),
        }
        .context(error::LaunchPlanSnafu)
    }

    /// How Codex will authenticate, resolved from the environment the harness
    /// will inherit. `None` for harnesses where the question does not arise.
    ///
    /// Delegated to the shared crate rather than re-tested here: "a blank
    /// `OPENAI_API_KEY` means absent, not empty" is exactly the kind of detail
    /// two clients would otherwise get subtly different.
    #[must_use]
    pub fn codex_auth(self) -> Option<CodexAuth> {
        if !self.is_codex() {
            return None;
        }
        let env: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        Some(resolve_codex_auth(&env))
    }
}

/// The harnesses `start` can launch, comma-separated, for error messages.
fn supported_names() -> String {
    SUPPORTED
        .iter()
        .map(|harness| harness.id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The launch plan for pi: no arguments, two environment variables.
///
/// This is what [`registry::LaunchSupport::ConsumerOwned`] means in practice.
/// The crate ships pi's extension but no recipe for launching it, because the
/// two consumers point pi at that extension differently — one materialises an
/// ephemeral copy and passes `--extension`, while tapesctl relies on the copy
/// `tapesctl plugin install pi` wrote into pi's global auto-discovery directory.
/// So there is no argv here at all: the extension is already loaded for every pi
/// session on this machine, and what a launch adds is only the environment that
/// wakes it up.
///
/// Both names come from [`tapes_harnesses::plugin`], which is also where the
/// asset reads them — the constant and the TypeScript literal are two spellings
/// of one contract, and the crate's tests pin them against each other. Spelling
/// either one here would be the drift the shared crate exists to prevent.
///
/// The schema variable is a hint, not a switch: the extension redirects
/// regardless, and uses this only to warn a user who picks a model the proxy is
/// not fronting. It is still always set, because a warning that never fires is
/// indistinguishable from one that cannot.
fn pi_plan(endpoint: &ProxyEndpoint, schema: UpstreamSchema) -> LaunchPlan {
    LaunchPlan {
        args: Vec::new(),
        env: vec![
            (GATEWAY_URL_ENV.to_owned(), endpoint.as_str().to_owned()),
            (GATEWAY_SCHEMA_ENV.to_owned(), schema.as_str().to_owned()),
        ],
        config_files: Vec::new(),
    }
}

/// Refuse to launch a harness whose capture plugin is not installed.
///
/// The check is against the artifacts the *registry* declares, not a path
/// spelled here, so a harness that gains an artifact is covered without this
/// function changing — and so this and `plugin install` can only ever disagree
/// if the crate contradicts itself.
///
/// `home` is a parameter for the same reason it is one in [`crate::plugin`]:
/// the behaviour worth testing is what happens for a home that does and does not
/// have the file, and neither is safe to assert against a developer's own.
fn ensure_plugin_installed(harness: Harness, home: &Path) -> Result<()> {
    for artifact in harness.required_plugin_artifacts() {
        let path = artifact.install_path(home);
        // `exists()` follows symlinks, which is the right question here: a
        // symlinked extension that resolves to a real file is one pi will load.
        // Whether writing *through* such a link is safe is the installer's
        // problem, and it refuses; this only asks whether pi has something to
        // load.
        snafu::ensure!(
            path.exists(),
            error::PluginNotInstalledSnafu {
                harness: harness.id(),
                path,
            }
        );
    }
    Ok(())
}

/// Resolved configuration for one `tapesctl start` invocation.
#[derive(Debug, Clone)]
pub struct StartConfig {
    /// Harness to launch.
    pub harness: Harness,
    /// How Codex will authenticate, when the harness is Codex.
    pub codex_auth: Option<CodexAuth>,
    /// Which upstream schema this capture fronts, for a harness that speaks
    /// more than one. `None` for harnesses whose schema follows from the
    /// harness itself.
    pub schema: Option<UpstreamSchema>,
    /// Arguments passed through to the harness verbatim.
    pub harness_args: Vec<String>,
    /// Where forwarded LLM traffic goes.
    pub upstream: Url,
    /// Base URL of the tapes ingest server.
    pub tapes_url: Url,
    /// Base URL of the web console, for the printed session link.
    pub web_url: Option<Url>,
    /// Org id stamped on captured turns.
    pub org_id: String,
    /// Acting subject stamped on captured turns.
    pub auth_subject: String,
    /// Whether to tail this session's transcripts alongside the wire lane.
    pub transcripts: bool,
}

impl StartConfig {
    /// Resolve CLI arguments and the environment into a config.
    pub fn resolve(args: StartArgs) -> Result<Self> {
        let harness = Harness::parse(&args.harness)?;
        let codex_auth = harness.codex_auth();
        let schema = resolve_schema(harness, args.schema.as_deref())?;
        let tapes_url = args
            .tapes_url
            .as_deref()
            .context(error::MissingTapesUrlSnafu)?;
        let tapes_url = Url::parse(tapes_url).context(error::TapesUrlSnafu)?;
        let upstream = match args.upstream.as_deref() {
            Some(upstream) => upstream,
            None => harness.default_upstream(codex_auth, schema),
        };
        let web_url = match args.web_url.as_deref() {
            Some(raw) => Some(Url::parse(raw).context(error::WebUrlSnafu)?),
            None => None,
        };

        Ok(Self {
            harness,
            codex_auth,
            schema,
            harness_args: args.harness_args,
            upstream: Url::parse(upstream).context(error::UpstreamUrlSnafu)?,
            tapes_url,
            web_url,
            org_id: args.org_id.unwrap_or_default(),
            // A standalone client has no gateway to stamp validated claims, so
            // it names the local user. Nothing parses the prefix — it is an
            // opaque attribution string in both worlds.
            auth_subject: args
                .auth_subject
                .unwrap_or_else(|| format!("local:{}", local_username())),
            // Transcripts are the only source of a session's fork skeleton, so
            // the lane is on unless the user explicitly says another client is
            // already tailing the same tree.
            transcripts: !args.no_transcripts,
        })
    }
}

/// Resolve `--schema` against the harness being launched.
///
/// A harness that speaks exactly one schema gets `None`, and naming one anyway
/// is refused: the flag would otherwise appear to route a capture it has no
/// power over.
fn resolve_schema(harness: Harness, schema: Option<&str>) -> Result<Option<UpstreamSchema>> {
    let Some(schema) = schema else {
        return Ok(matches!(harness, Harness::Pi).then_some(DEFAULT_PI_SCHEMA));
    };
    snafu::ensure!(
        matches!(harness, Harness::Pi),
        error::SchemaNotApplicableSnafu {
            harness: harness.id(),
            provider: harness.provider(None),
        }
    );
    Ok(Some(UpstreamSchema::parse(schema)?))
}

/// The local OS username, or `unknown`. Shared with `tapesctl sync`, which
/// stamps the same default subject.
#[must_use]
pub fn local_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Run one capture session: bind, launch, forward, and exit with the harness.
pub async fn run(args: StartArgs) -> Result<()> {
    let config = StartConfig::resolve(args)?;

    // Before anything is bound or spawned: a harness whose capture depends on an
    // installed extension cannot be captured without it, and the session would
    // otherwise run to completion and record nothing.
    if !config.harness.required_plugin_artifacts().is_empty() {
        let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
        ensure_plugin_installed(config.harness, &home)?;
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context(error::BindSnafu)?;
    let addr = listener.local_addr().context(error::BindSnafu)?;

    // A per-process provider id: it is echoed back in the marker header, which
    // is how the attribution pipeline tells two concurrent Codex processes
    // apart on one loopback endpoint.
    let provider_id = format!("{CODEX_PROVIDER_PREFIX}-{}", uuid::Uuid::new_v4());
    let endpoint = config.harness.endpoint_for(addr, config.codex_auth);
    let plan = config.harness.plan(
        endpoint.clone(),
        &provider_id,
        config.codex_auth,
        config.schema,
    )?;

    let attribution = AttributionState::new(
        spawn_watcher(claude_sessions_dir()?),
        spawn_codex_watcher(codex_sessions_dir()?),
    );
    let (session_tx, mut session_rx) = unbounded_channel::<String>();

    let tracker = SessionTracker::new();
    let state = ProxyState {
        upstream: config.upstream.clone(),
        ingest: IngestClient::new(&config.tapes_url)?,
        transcript_tracker: tracker.clone(),
        attribution: Arc::new(attribution),
        attribution_config: Arc::new(AttributionConfig::new(CodexProviderFilter::new(
            CODEX_PROVIDER_PREFIX,
        ))),
        provider: config.harness.provider(config.schema),
        codex_marker_header: Arc::new(CODEX_MARKER_HEADER.to_ascii_lowercase()),
        codex_lane: config.harness.is_codex(),
        self_attributing: config.harness.is_self_attributing(),
        org_id: Arc::new(config.org_id.clone()),
        auth_subject: Arc::new(config.auth_subject.clone()),
        session_seen: Arc::new(tokio::sync::Mutex::new(Some(session_tx))),
    };

    let app = axum::Router::new()
        .fallback(proxy::forward_handler)
        .with_state(state);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        if let Err(err) = served.await {
            warn!(error = %err, "proxy server stopped");
        }
    });

    info!(
        harness = config.harness.program(),
        proxy = %addr,
        upstream = %config.upstream,
        ingest = %config.tapes_url,
        "capture proxy listening",
    );

    let tailer = spawn_tailer(&config, tracker)?;

    let written = materialise_config_files(&plan)?;

    // The last thing this process writes to the terminal until the harness
    // gives it back.
    announce_capture();

    let status = spawn_harness(&config, &plan).await;

    // Dying with the session: stop accepting, then clean up whatever the recipe
    // asked to have on disk. Cleanup is the consumer's job precisely because
    // recipes are pure.
    let _ = shutdown_tx.send(());
    let _ = server.await;
    remove_config_files(&written);

    // The tailer is *awaited*, not aborted. Its shutdown pass is the
    // `PushReason::Exit` push that delivers the completed transcript set —
    // including the subagent files that carry the fork skeleton — and it can
    // only run after the harness has finished writing them. Aborting here would
    // drop exactly the data the transcript lane exists to capture.
    if let Some((shutdown, handle)) = tailer {
        let _ = shutdown.send(());
        if let Err(err) = handle.await {
            warn!(error = %err, "transcript tailer did not finish cleanly");
        }
    }

    // The terminal is the caller's again, so the session can finally be named.
    // The id only becomes known once a turn is attributed, which is mid-session
    // — the one moment printing is forbidden. The proxy sends exactly one id, so
    // a single non-blocking read after shutdown collects whatever there was.
    print_exit_summary(
        config.web_url.as_ref(),
        session_rx.try_recv().ok().as_deref(),
    );

    let status = status?;
    if !status.success() {
        warn!(code = ?status.code(), "harness exited with a non-zero status");
    }
    Ok(())
}

/// Start the transcript tailer for this session, when the harness has one.
///
/// `None` covers the two cases where the lane cannot or should not run: the user
/// opted out, or the harness is Codex — whose transcripts do not live in the
/// Claude project tree the shared crate's discovery walks. Returning `None`
/// rather than failing is deliberate: a Codex capture is still a good wire
/// capture, and refusing to start it over a lane that does not apply would be a
/// regression against PR 5.
fn spawn_tailer(
    config: &StartConfig,
    tracker: SessionTracker,
) -> Result<
    Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )>,
> {
    if !config.transcripts || config.harness != Harness::Claude {
        return Ok(None);
    }
    let projects_root = tailer::default_projects_root().context(error::NoHomeDirSnafu)?;
    let tailer_config = tailer::TailerConfig::new(
        projects_root,
        claude_sessions_dir()?,
        config.auth_subject.clone(),
    );
    let client = TranscriptClient::new(&config.tapes_url)?;
    Ok(Some(tailer::spawn(client, tracker, tailer_config)))
}

/// The last line printed before the harness takes the terminal.
///
/// The log path has to be handed over *before* that window opens, because from
/// the spawn until the harness exits this process may not write to the terminal
/// at all. Silent when tracing is streaming to stderr — then the user is already
/// watching it, and there is no file to point at.
fn announce_capture() {
    if let Some(path) = logging::active_log_file() {
        println!("tapesctl: capturing; logs at {}", path.display());
    }
}

/// What this session was, printed once the terminal belongs to the caller again.
fn print_exit_summary(web_url: Option<&Url>, session_id: Option<&str>) {
    match session_id {
        Some(id) => print_session_link(web_url, id),
        // Not an error: a harness can be launched and quit without ever calling
        // a model. Saying so beats printing nothing and leaving the user to
        // wonder whether capture was ever on.
        None => println!("tapesctl: no turns were captured"),
    }
    if let Some(path) = logging::active_log_file() {
        println!("tapesctl: logs at {}", path.display());
    }
}

fn print_session_link(web_url: Option<&Url>, session_id: &str) {
    match web_url.and_then(|base| base.join(&format!("/sessions/{session_id}")).ok()) {
        Some(url) => println!("tapesctl: captured session {session_id} — {url}"),
        // Without a console base URL there is no link to print; naming the flag
        // beats printing a guessed host that 404s.
        None => {
            println!("tapesctl: captured session {session_id} (pass --web-url for a console link)",)
        }
    }
}

async fn spawn_harness(
    config: &StartConfig,
    plan: &LaunchPlan,
) -> Result<std::process::ExitStatus> {
    let mut command = tokio::process::Command::new(config.harness.program());
    command.args(&plan.args);
    command.args(&config.harness_args);
    for (key, value) in &plan.env {
        command.env(key, value);
    }
    command.status().await.context(error::SpawnHarnessSnafu {
        harness: config.harness.program(),
    })
}

fn materialise_config_files(plan: &LaunchPlan) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for file in &plan.config_files {
        if let Some(parent) = file.path.parent() {
            std::fs::create_dir_all(parent).context(error::ConfigFileSnafu {
                path: file.path.clone(),
            })?;
        }
        std::fs::write(&file.path, &file.contents).context(error::ConfigFileSnafu {
            path: file.path.clone(),
        })?;
        written.push(file.path.clone());
    }
    Ok(written)
}

fn remove_config_files(paths: &[PathBuf]) {
    for path in paths {
        if let Err(err) = std::fs::remove_file(path) {
            warn!(path = %path.display(), error = %err, "could not remove launch config");
        }
    }
}

fn claude_sessions_dir() -> Result<PathBuf> {
    claude_session::default_sessions_dir().context(error::NoHomeDirSnafu)
}

fn codex_sessions_dir() -> Result<PathBuf> {
    codex_session::default_sessions_dir().context(error::NoHomeDirSnafu)
}

/// Environment overlay a plan applies, as a map — for tests and diagnostics.
#[must_use]
pub fn plan_env(plan: &LaunchPlan) -> HashMap<String, String> {
    plan.env.iter().cloned().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn args(harness: &str) -> StartArgs {
        StartArgs {
            harness: harness.to_owned(),
            harness_args: Vec::new(),
            tapes_url: Some("http://127.0.0.1:8090".to_owned()),
            upstream: None,
            schema: None,
            web_url: None,
            org_id: None,
            auth_subject: None,
            no_transcripts: false,
        }
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:51000".parse().unwrap()
    }

    #[test]
    fn harness_names_are_case_insensitive_and_trimmed() {
        assert_eq!(Harness::parse("claude").unwrap(), Harness::Claude);
        assert_eq!(Harness::parse("  CODEX ").unwrap(), Harness::Codex);
    }

    #[test]
    fn an_unsupported_harness_is_rejected_by_name() {
        let err = Harness::parse("opencode").unwrap_err();
        assert!(format!("{err}").contains("opencode"), "got: {err}");
    }

    #[test]
    fn the_provider_names_the_captured_wire_format_not_the_vendor() {
        // Codex speaks the OpenAI Responses API, and ingest picks its
        // server-side reducer by this name — calling it "codex" would leave the
        // turn unreduced.
        assert_eq!(Harness::Codex.provider(None), "openai");
        assert_eq!(Harness::Claude.provider(None), "anthropic");
        // And pi, which speaks whichever schema the capture fronts, is named by
        // the schema rather than by itself — "pi" is not a wire format.
        assert_eq!(Harness::Pi.provider(Some(UpstreamSchema::OpenAi)), "openai");
        assert_eq!(
            Harness::Pi.provider(Some(UpstreamSchema::Anthropic)),
            "anthropic"
        );
    }

    #[test]
    fn claude_gets_a_bare_endpoint_and_api_key_codex_gets_a_v1_suffix() {
        // Codex appends `/responses`, so an API-key endpoint must already end
        // at the `/v1` segment for OpenAI's `/v1/responses` route to resolve.
        assert_eq!(
            Harness::Claude.endpoint_for(addr(), None).as_str(),
            "http://127.0.0.1:51000",
        );
        assert_eq!(
            Harness::Codex
                .endpoint_for(addr(), Some(CodexAuth::ApiKey))
                .as_str(),
            "http://127.0.0.1:51000/v1",
        );
    }

    #[test]
    fn chatgpt_codex_gets_no_v1_suffix() {
        // The ChatGPT backend's path has no `/v1` component; adding one sends
        // codex to a route that does not exist.
        assert_eq!(
            Harness::Codex
                .endpoint_for(addr(), Some(CodexAuth::ChatGpt))
                .as_str(),
            "http://127.0.0.1:51000",
        );
    }

    #[test]
    fn the_codex_upstream_follows_the_credential_not_a_preference() {
        // Plan OAuth tokens are honoured only by the ChatGPT backend, and API
        // keys only by api.openai.com — so the credential picks the host.
        assert_eq!(
            Harness::Codex.default_upstream(Some(CodexAuth::ChatGpt), None),
            DEFAULT_CHATGPT_UPSTREAM,
        );
        assert_eq!(
            Harness::Codex.default_upstream(Some(CodexAuth::ApiKey), None),
            DEFAULT_OPENAI_UPSTREAM,
        );
    }

    #[test]
    fn the_claude_plan_points_the_harness_at_the_proxy() {
        let plan = Harness::Claude
            .plan(
                Harness::Claude.endpoint_for(addr(), None),
                "unused",
                None,
                None,
            )
            .unwrap();
        let env = plan_env(&plan);
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:51000"),
        );
    }

    #[test]
    fn the_codex_plan_declares_the_marker_header_and_provider() {
        let provider_id = format!("{CODEX_PROVIDER_PREFIX}-abc");
        let plan = Harness::Codex
            .plan(
                Harness::Codex.endpoint_for(addr(), Some(CodexAuth::ApiKey)),
                &provider_id,
                Some(CodexAuth::ApiKey),
                None,
            )
            .unwrap();
        let joined = plan.args.join(" ");
        assert!(joined.contains(&provider_id), "got: {joined}");
        assert!(
            joined.contains(CODEX_MARKER_HEADER),
            "the proxy cannot tell two codex processes apart without it: {joined}",
        );
    }

    #[test]
    fn the_launched_provider_id_matches_this_clients_filter() {
        // The suffixed id and the filter must agree, or every Codex turn is
        // attributed to nothing.
        let provider_id = format!("{CODEX_PROVIDER_PREFIX}-{}", uuid::Uuid::new_v4());
        let filter = CodexProviderFilter::new(CODEX_PROVIDER_PREFIX);
        assert!(filter.matches(Some(&provider_id)));
    }

    #[test]
    fn a_missing_tapes_url_is_an_error_rather_than_a_silent_no_capture() {
        let mut args = args("claude");
        args.tapes_url = None;
        assert!(StartConfig::resolve(args).is_err());
    }

    #[test]
    fn upstream_defaults_per_harness() {
        // Only Claude is asserted through `resolve` here. Codex's default
        // depends on which credential the *ambient* environment offers, so
        // pinning it in a test that reads the real environment would pass or
        // fail according to whether the machine happens to export
        // `OPENAI_API_KEY`. The credential-to-host mapping is asserted
        // directly, without the environment, in
        // `the_codex_upstream_follows_the_credential_not_a_preference`.
        assert_eq!(
            StartConfig::resolve(args("claude"))
                .unwrap()
                .upstream
                .as_str(),
            "https://api.anthropic.com/",
        );
    }

    #[test]
    fn an_explicit_upstream_wins() {
        let mut args = args("claude");
        args.upstream = Some("http://127.0.0.1:52292".to_owned());
        assert_eq!(
            StartConfig::resolve(args).unwrap().upstream.as_str(),
            "http://127.0.0.1:52292/",
        );
    }

    #[test]
    fn the_default_subject_names_the_local_user() {
        let subject = StartConfig::resolve(args("claude")).unwrap().auth_subject;
        assert!(subject.starts_with("local:"), "got: {subject}");
    }

    #[test]
    fn an_explicit_subject_wins_so_ci_can_name_itself() {
        let mut args = args("claude");
        args.auth_subject = Some("gardener-ci".to_owned());
        assert_eq!(
            StartConfig::resolve(args).unwrap().auth_subject,
            "gardener-ci",
        );
    }

    #[test]
    fn the_org_id_defaults_to_empty_for_the_local_sentinel() {
        assert_eq!(StartConfig::resolve(args("claude")).unwrap().org_id, "");
    }

    // --- pi ------------------------------------------------------------------

    #[test]
    fn pi_resolves_through_the_shared_registry() {
        assert_eq!(Harness::parse("pi").unwrap(), Harness::Pi);
        assert_eq!(Harness::parse(" PI ").unwrap(), Harness::Pi);
        // Resolution goes through the registry, so an alias declared there works
        // here without this file listing it.
        assert_eq!(Harness::parse("claude-code").unwrap(), Harness::Claude);
    }

    #[test]
    fn a_registered_harness_with_no_arm_here_is_still_unsupported() {
        // opencode is in the shared registry and has a crate recipe, but no arm
        // in this binary. Resolving through the registry must not make it look
        // launchable.
        let err = Harness::parse("opencode").unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("opencode"), "got: {message}");
        // And the message advertises exactly the arms that exist.
        assert!(message.contains("claude, codex, pi"), "got: {message}");
    }

    #[test]
    fn the_pi_plan_carries_the_crates_environment_contract_and_no_argv() {
        // The whole launch: pi has no base-URL flag, so what points it at this
        // proxy is the environment its already-installed extension reads.
        let plan = Harness::Pi
            .plan(
                Harness::Pi.endpoint_for(addr(), None),
                "unused",
                None,
                Some(UpstreamSchema::Anthropic),
            )
            .unwrap();
        let env = plan_env(&plan);
        assert_eq!(
            env.get(GATEWAY_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:51000"),
        );
        assert_eq!(
            env.get(GATEWAY_SCHEMA_ENV).map(String::as_str),
            Some("anthropic"),
        );
        assert!(
            plan.args.is_empty(),
            "pi loads the extension from its own auto-discovery directory; \
             argv here would be a second, competing copy: {:?}",
            plan.args,
        );
        assert!(plan.config_files.is_empty());
    }

    #[test]
    fn the_pi_endpoint_has_no_path_suffix() {
        // pi's providers each append their own full path, so a `/v1` here would
        // land every request one segment deep.
        assert_eq!(
            Harness::Pi.endpoint_for(addr(), None).as_str(),
            "http://127.0.0.1:51000",
        );
    }

    #[test]
    fn the_pi_schema_selects_the_upstream_the_provider_and_the_hint_together() {
        // These three must move as one: forwarding to OpenAI while telling
        // ingest "anthropic" would capture turns no reducer can read.
        let mut args = args("pi");
        args.schema = Some("openai".to_owned());
        let config = StartConfig::resolve(args).unwrap();
        assert_eq!(config.schema, Some(UpstreamSchema::OpenAi));
        assert_eq!(config.upstream.as_str(), "https://api.openai.com/");
        assert_eq!(config.harness.provider(config.schema), "openai");
    }

    #[test]
    fn pi_defaults_to_the_anthropic_schema() {
        let config = StartConfig::resolve(args("pi")).unwrap();
        assert_eq!(config.schema, Some(DEFAULT_PI_SCHEMA));
        assert_eq!(config.upstream.as_str(), "https://api.anthropic.com/");
        assert_eq!(config.harness.provider(config.schema), "anthropic");
    }

    #[test]
    fn the_schema_spellings_are_ones_the_extension_recognises() {
        // The asset switches on these strings. A spelling this client invented
        // would leave the extension unable to tell which schema is fronted, and
        // its mismatch warning silently dead.
        let asset = tapes_harnesses::plugin::PI_GATEWAY_EXTENSION.contents();
        for schema in [UpstreamSchema::Anthropic, UpstreamSchema::OpenAi] {
            assert!(
                asset.contains(&format!("\"{}\"", schema.as_str())),
                "the pi extension does not know the schema {:?}",
                schema.as_str(),
            );
        }
    }

    #[test]
    fn a_schema_is_refused_for_a_harness_that_speaks_only_one() {
        let mut args = args("claude");
        args.schema = Some("openai".to_owned());
        let err = StartConfig::resolve(args).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("claude"), "got: {message}");
        assert!(message.contains("anthropic"), "got: {message}");
    }

    #[test]
    fn an_unknown_schema_is_refused_with_the_valid_values() {
        let mut args = args("pi");
        args.schema = Some("gemini".to_owned());
        let err = StartConfig::resolve(args).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("gemini"), "got: {message}");
        assert!(message.contains("anthropic"), "got: {message}");
    }

    #[test]
    fn pi_is_the_self_attributing_harness_and_the_others_are_not() {
        // Drives the proxy's choice of whose session id to file a turn under.
        assert!(Harness::Pi.is_self_attributing());
        assert!(!Harness::Claude.is_self_attributing());
        assert!(!Harness::Codex.is_self_attributing());
    }

    #[test]
    fn launching_pi_without_its_extension_names_the_installer() {
        // The failure a user will actually hit: `start pi` before
        // `plugin install pi`. It must say what to run, not just what is
        // missing.
        let home = tempfile::tempdir().unwrap();
        let err = ensure_plugin_installed(Harness::Pi, home.path()).unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("tapesctl plugin install pi"),
            "got: {message}",
        );
        assert!(message.contains("tapes-gateway.ts"), "got: {message}");
    }

    #[test]
    fn launching_pi_with_its_extension_installed_proceeds() {
        // The same check must pass once the installer has run, or `start pi`
        // could never launch at all.
        let home = tempfile::tempdir().unwrap();
        for artifact in Harness::Pi.required_plugin_artifacts() {
            let path = artifact.install_path(home.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, artifact.contents()).unwrap();
        }
        ensure_plugin_installed(Harness::Pi, home.path()).unwrap();
    }

    #[test]
    fn a_harness_captured_by_redirect_needs_nothing_installed() {
        // Claude has no artifacts, so an empty home must not block its launch.
        let home = tempfile::tempdir().unwrap();
        assert!(Harness::Claude.required_plugin_artifacts().is_empty());
        ensure_plugin_installed(Harness::Claude, home.path()).unwrap();
    }
}
