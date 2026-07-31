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
use std::path::PathBuf;
use std::sync::Arc;

use snafu::{OptionExt, ResultExt};
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, claude_session, codex_session,
    spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::launch::{
    ClaudeRecipe, CodexAuth, CodexRecipe, LaunchPlan, LaunchRecipe, ProxyEndpoint,
    resolve_codex_auth,
};
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

/// Which harness is being launched, and everything that differs between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code, over the Anthropic Messages API.
    Claude,
    /// Codex, over the OpenAI Responses API.
    Codex,
}

impl Harness {
    /// Resolve a user-typed harness name.
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => error::UnsupportedHarnessSnafu {
                harness: other.to_owned(),
            }
            .fail(),
        }
    }

    /// The binary to execute.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The ingest `provider` family this harness's traffic is in. Ingest keys
    /// its server-side reducer on this, so it must name the wire format of the
    /// bytes actually captured — not the vendor of the harness.
    #[must_use]
    pub fn provider(self) -> &'static str {
        match self {
            Self::Claude => "anthropic",
            Self::Codex => "openai",
        }
    }

    /// Default upstream when none is supplied.
    ///
    /// Codex's default depends on how it will authenticate, because the two
    /// credential kinds are accepted by different hosts.
    #[must_use]
    pub fn default_upstream(self, auth: Option<CodexAuth>) -> &'static str {
        match (self, auth) {
            (Self::Claude, _) => DEFAULT_ANTHROPIC_UPSTREAM,
            (Self::Codex, Some(CodexAuth::ChatGpt)) => DEFAULT_CHATGPT_UPSTREAM,
            (Self::Codex, _) => DEFAULT_OPENAI_UPSTREAM,
        }
    }

    /// Whether requests take the Codex attribution lane.
    #[must_use]
    pub fn is_codex(self) -> bool {
        matches!(self, Self::Codex)
    }

    /// Build the endpoint the harness should be pointed at.
    ///
    /// The path suffix is deployment knowledge, so it is decided here rather
    /// than in the recipe. Codex appends `/responses` to whatever it is given,
    /// and the two auth modes reach different paths: OpenAI's responses route
    /// is `/v1/responses`, so an API-key endpoint ends at a `/v1` segment,
    /// while the ChatGPT backend has no `/v1` component and its endpoint ends
    /// at the backend segment. Claude appends `/v1/messages` itself and needs
    /// no suffix at all.
    #[must_use]
    pub fn endpoint_for(self, addr: SocketAddr, auth: Option<CodexAuth>) -> ProxyEndpoint {
        match (self, auth) {
            (Self::Claude, _) | (Self::Codex, Some(CodexAuth::ChatGpt)) => {
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
    ) -> Result<LaunchPlan> {
        match self {
            Self::Claude => ClaudeRecipe::new(endpoint).plan(),
            Self::Codex => {
                CodexRecipe::new(endpoint, auth.unwrap_or(CodexAuth::ChatGpt), provider_id)
                    .with_display_name("tapesctl capture")
                    .with_attribution_header(CODEX_MARKER_HEADER)
                    .plan()
            }
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

/// Resolved configuration for one `tapesctl start` invocation.
#[derive(Debug, Clone)]
pub struct StartConfig {
    /// Harness to launch.
    pub harness: Harness,
    /// How Codex will authenticate, when the harness is Codex.
    pub codex_auth: Option<CodexAuth>,
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
        let tapes_url = args
            .tapes_url
            .as_deref()
            .context(error::MissingTapesUrlSnafu)?;
        let tapes_url = Url::parse(tapes_url).context(error::TapesUrlSnafu)?;
        let upstream = match args.upstream.as_deref() {
            Some(upstream) => upstream,
            None => harness.default_upstream(codex_auth),
        };
        let web_url = match args.web_url.as_deref() {
            Some(raw) => Some(Url::parse(raw).context(error::WebUrlSnafu)?),
            None => None,
        };

        Ok(Self {
            harness,
            codex_auth,
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context(error::BindSnafu)?;
    let addr = listener.local_addr().context(error::BindSnafu)?;

    // A per-process provider id: it is echoed back in the marker header, which
    // is how the attribution pipeline tells two concurrent Codex processes
    // apart on one loopback endpoint.
    let provider_id = format!("{CODEX_PROVIDER_PREFIX}-{}", uuid::Uuid::new_v4());
    let endpoint = config.harness.endpoint_for(addr, config.codex_auth);
    let plan = config
        .harness
        .plan(endpoint.clone(), &provider_id, config.codex_auth)?;

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
        provider: config.harness.provider(),
        codex_marker_header: Arc::new(CODEX_MARKER_HEADER.to_ascii_lowercase()),
        codex_lane: config.harness.is_codex(),
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
        assert_eq!(Harness::Codex.provider(), "openai");
        assert_eq!(Harness::Claude.provider(), "anthropic");
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
            Harness::Codex.default_upstream(Some(CodexAuth::ChatGpt)),
            DEFAULT_CHATGPT_UPSTREAM,
        );
        assert_eq!(
            Harness::Codex.default_upstream(Some(CodexAuth::ApiKey)),
            DEFAULT_OPENAI_UPSTREAM,
        );
    }

    #[test]
    fn the_claude_plan_points_the_harness_at_the_proxy() {
        let plan = Harness::Claude
            .plan(Harness::Claude.endpoint_for(addr(), None), "unused", None)
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
}
