//! `tapesctl capture <harness>` — capture a harness that launches itself.
//!
//! The difference from [`crate::start`] is one process: there is no child
//! here. `start` binds an ephemeral port, spawns a harness pointed at it, and
//! dies when that harness does. This binds the *installed* address, serves
//! until interrupted, and captures whichever of the harness's sessions happen
//! to run in that window.
//!
//! That inversion drives everything else:
//!
//! * **The address is not ours to choose.** It was decided at install time and
//!   written into a config file the harness has already read, so it comes out
//!   of the handoff and a bind failure is fatal rather than something to route
//!   around — re-picking a port would silently orphan the config.
//! * **There is no launched PID**, so no peer-ancestry check is possible, and
//!   the capture nonce has nothing to seed. The lifecycle secret does that job
//!   instead; see [`crate::codex_app::lifecycle`].
//! * **The terminal stays ours** for the whole run. Nothing is handed a TTY,
//!   so sessions can be named the moment they appear rather than only at exit.
//! * **The Codex open-rollout lane is off.** It would resolve some desktop
//!   requests to `harness_id: codex` — the CLI, not the app — and a desktop
//!   session must not land under two harnesses depending on which lane
//!   answered first.
//!
//! There is no transcript lane either. This harness writes Codex rollouts, and
//! the tailer walks the Claude project tree; `start codex` has the same gap for
//! the same reason.

use std::net::SocketAddr;
use std::sync::Arc;

use snafu::{OptionExt, ResultExt};
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, claude::session as claude_session,
    codex::session as codex_session, spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::config::codex as codex_config;
use tapes_harnesses::harness::RegistryUserAgents;
use tapes_harnesses::launch::CodexAuth;
use tracing::{info, warn};
use url::Url;

use crate::cli::CaptureArgs;
use crate::codex_app::{self, Handoff, install, lifecycle::DesktopSessions};
use crate::error::{Result, error};
use crate::start::ingest::IngestClient;
use crate::start::proxy::{self, ProxyState};
use crate::start::{DEFAULT_CHATGPT_UPSTREAM, DEFAULT_OPENAI_UPSTREAM, local_username};
use crate::transcript::tailer::SessionTracker;

/// The ingest `provider` family desktop traffic is in.
///
/// The wire format, not the vendor: ingest keys its server-side reducer on
/// this, and the app speaks the OpenAI Responses API exactly as the CLI does.
const PROVIDER: &str = "openai";

/// Run one capture until interrupted.
pub async fn run(args: CaptureArgs) -> Result<()> {
    let harness = codex_app::resolve_hook_harness(&args.harness)?;
    let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
    let handoff = Handoff::read(&Handoff::path(&home))?;

    // Before binding anything: is the harness still pointed here? A config
    // that names some other address produces a capture that runs perfectly and
    // records nothing, which is the failure users cannot diagnose. Refused
    // rather than warned, because the fix is one non-destructive command.
    let auth = verify_config(&home, &handoff)?;

    let tapes_url = args
        .tapes_url
        .as_deref()
        .context(error::MissingTapesUrlSnafu)?;
    let tapes_url = Url::parse(tapes_url).context(error::TapesUrlSnafu)?;
    let upstream = match args.upstream.as_deref() {
        Some(upstream) => upstream,
        // The credential decides the host: plan OAuth tokens are honoured only
        // by the ChatGPT backend and API keys only by api.openai.com, which is
        // why the auth mode is read back from the config rather than guessed.
        None => match auth {
            CodexAuth::ChatGpt => DEFAULT_CHATGPT_UPSTREAM,
            CodexAuth::ApiKey => DEFAULT_OPENAI_UPSTREAM,
        },
    };
    let upstream = Url::parse(upstream).context(error::UpstreamUrlSnafu)?;
    let web_url = match args.web_url.as_deref() {
        Some(raw) => Some(Url::parse(raw).context(error::WebUrlSnafu)?),
        None => None,
    };

    let listener = bind(handoff.proxy_addr).await?;
    let sessions = Arc::new(DesktopSessions::new(handoff.secret.clone()).with_web_url(web_url));
    let state = ProxyState {
        upstream: upstream.clone(),
        ingest: IngestClient::new(&tapes_url)?,
        attribution: Arc::new(AttributionState::new(
            spawn_watcher(claude_session::default_sessions_dir().context(error::NoHomeDirSnafu)?),
            spawn_codex_watcher(
                codex_session::default_sessions_dir().context(error::NoHomeDirSnafu)?,
            ),
        )),
        attribution_config: Arc::new(AttributionConfig::new(
            CodexProviderFilter::new(handoff.provider_id.clone()),
            // Which harness a `User-Agent` names is registry knowledge, so the
            // registry answers it. A local table here would be a second set of
            // rules for the same question.
            RegistryUserAgents::default(),
        )),
        provider: PROVIDER,
        // The Codex desktop app is configured to one endpoint and speaks one
        // provider through it, so there is nothing here to label and nothing to
        // route between. Labelling is a property of the *launched* extension,
        // and this command launches nothing.
        provider_routes: None,
        codex_marker_header: Arc::new(crate::start::CODEX_MARKER_HEADER.to_ascii_lowercase()),
        codex_lane: false,
        self_attributing: false,
        // Nothing was launched, so there is no PID any request could be
        // required to descend from. The identity a turn is filed under has
        // already been authenticated by the time it is reachable here.
        launched_pid: Arc::new(std::sync::atomic::AtomicI32::new(
            crate::start::NO_LAUNCHED_PID,
        )),
        // Likewise no per-launch nonce. Empty is the fail-closed value: the
        // crate's comparison refuses an empty expectation, so no inbound
        // envelope can be believed on this proxy at all.
        gateway_nonce: Arc::new(String::new()),
        org_id: Arc::new(args.org_id.unwrap_or_default()),
        auth_subject: Arc::new(
            args.auth_subject
                .unwrap_or_else(|| format!("local:{}", local_username())),
        ),
        session_seen: Arc::new(tokio::sync::Mutex::new(None)),
        desktop_sessions: Some(Arc::clone(&sessions)),
        transcript_tracker: SessionTracker::new(),
    };

    let app = axum::Router::new()
        .route(
            codex_app::LIFECYCLE_PATH,
            axum::routing::post(codex_app::lifecycle::receive),
        )
        .with_state(Arc::clone(&sessions))
        .merge(
            axum::Router::new()
                .fallback(proxy::forward_handler)
                .with_state(state),
        );

    info!(
        harness = harness.id(),
        proxy = %handoff.proxy_addr,
        upstream = %upstream,
        ingest = %tapes_url,
        "capture proxy listening",
    );
    println!(
        "tapesctl: capturing {} on {} — start a session in the app; Ctrl-C to stop",
        harness.id(),
        handoff.proxy_addr,
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!(error = %err, "could not listen for an interrupt; serving until killed");
            // Never resolving is deliberate: returning here would shut the
            // proxy down immediately and take the user's capture with it.
            std::future::pending::<()>().await;
        }
    })
    .await
    .context(error::BindSnafu)?;

    println!("tapesctl: stopped after {} session(s)", sessions.len());
    Ok(())
}

/// Bind the installed address, or explain how to move it.
///
/// The address is in a config file the harness has already read, so this is
/// the one place a port collision can surface — and the only safe answer is to
/// re-run the installer, which rewrites both halves together.
async fn bind(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => Ok(listener),
        Err(source) => {
            warn!(
                %addr,
                "the installed capture address is taken; \
                 re-run `tapesctl plugin install codex-app --port <port>` to move it",
            );
            Err(source).context(error::BindSnafu)
        }
    }
}

/// Confirm the harness's own config still routes to this handoff's address,
/// and report which credential it will present.
///
/// Read through the crate's `installed_provider`, which is the neutral
/// read-back half of the same grammar the installer wrote with — so "what the
/// config says" and "what an install would write" cannot be two different
/// answers.
fn verify_config(home: &std::path::Path, handoff: &Handoff) -> Result<CodexAuth> {
    verify_config_at(&codex_app::codex_config_path(home), handoff)
}

/// The body of [`verify_config`], against an explicit config path — resolving
/// `$CODEX_HOME` reads the ambient environment, and a test that did so would
/// answer differently on a machine where that variable happens to be set.
fn verify_config_at(path: &std::path::Path, handoff: &Handoff) -> Result<CodexAuth> {
    let path = path.to_path_buf();
    // Absent reads as empty, which the read-back reports as "no such provider"
    // — the same answer, through the same code, as a config that simply never
    // had one.
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let installed = codex_config::installed_provider(&text, &handoff.provider_id)
        .context(error::CodexConfigSnafu { path: path.clone() })?
        .context(error::CodexAppNotConfiguredSnafu {
            path: path.clone(),
            provider_id: handoff.provider_id.clone(),
        })?;

    // The read-back gives the keys; deciding what they mean about a route is
    // this client's, which is why the crate stops here.
    let auth = if installed.requires_openai_auth == Some(true) {
        CodexAuth::ChatGpt
    } else {
        CodexAuth::ApiKey
    };
    let expected = install::expected_base_url(handoff.proxy_addr, auth);
    snafu::ensure!(
        installed.base_url.as_deref() == Some(expected.as_str()),
        error::CodexAppConfigDriftSnafu {
            path,
            expected,
            found: installed.base_url.unwrap_or_default(),
        }
    );
    Ok(auth)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::codex_app::HANDOFF_VERSION;
    use std::path::Path;

    fn handoff(port: u16) -> Handoff {
        Handoff {
            version: HANDOFF_VERSION,
            harness_id: "codex-app".to_owned(),
            proxy_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            secret: "a".repeat(64),
            provider_id: codex_app::PROVIDER_ID.to_owned(),
            installed_at: "2026-08-05T00:00:00Z".to_owned(),
        }
    }

    /// A config written by the installer, for `port` under `auth`.
    fn installed_config(dir: &Path, port: u16, auth: CodexAuth) -> std::path::PathBuf {
        let patch = codex_config::CodexProviderPatch::new(
            codex_app::PROVIDER_ID,
            "tapesctl capture",
            install::expected_base_url(SocketAddr::from(([127, 0, 0, 1], port)), auth),
            auth,
        );
        let path = dir.join("config.toml");
        std::fs::write(&path, codex_config::apply_provider("", &patch).unwrap()).unwrap();
        path
    }

    /// The healthy case, and the one that decides the upstream: the auth mode
    /// is read back out of the config rather than remembered, because the
    /// credential is what picks the host.
    #[test]
    fn a_config_written_by_the_installer_verifies_and_names_its_credential() {
        let dir = tempfile::tempdir().unwrap();
        for auth in [CodexAuth::ChatGpt, CodexAuth::ApiKey] {
            let path = installed_config(dir.path(), 51520, auth);
            assert_eq!(verify_config_at(&path, &handoff(51520)).unwrap(), auth);
        }
    }

    /// The diagnosis-proof failure this check exists for: the capture would
    /// bind a port the app no longer talks to, run perfectly, and record
    /// nothing.
    #[test]
    fn a_config_pointing_somewhere_else_is_refused_and_names_both_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let path = installed_config(dir.path(), 51521, CodexAuth::ChatGpt);

        let err = verify_config_at(&path, &handoff(51520))
            .unwrap_err()
            .to_string();
        assert!(err.contains("51520"), "got: {err}");
        assert!(err.contains("51521"), "got: {err}");
        assert!(err.contains("tapesctl plugin install"), "got: {err}");
    }

    #[test]
    fn a_config_with_no_provider_at_all_names_the_installer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[tui]\ntheme = \"dark\"\n").unwrap();

        let err = verify_config_at(&path, &handoff(51520))
            .unwrap_err()
            .to_string();
        assert!(err.contains("tapesctl plugin install"), "got: {err}");
    }

    /// A machine that never ran the installer reads the same as one whose
    /// provider was removed — both are "not set up", and both are fixed the
    /// same way.
    #[test]
    fn an_absent_config_is_refused_the_same_way_as_an_unconfigured_one() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_config_at(&dir.path().join("config.toml"), &handoff(51520))
            .unwrap_err()
            .to_string();
        assert!(err.contains("tapesctl plugin install"), "got: {err}");
    }

    fn args(harness: &str) -> CaptureArgs {
        CaptureArgs {
            harness: harness.to_owned(),
            tapes_url: Some("http://127.0.0.1:8090".to_owned()),
            upstream: None,
            web_url: None,
            org_id: None,
            auth_subject: None,
        }
    }

    #[tokio::test]
    async fn a_harness_with_no_hook_surface_is_refused_before_anything_is_read() {
        // Named before the home directory or the handoff, so a machine with no
        // install at all still gets the message about the harness.
        let err = run(args("claude")).await.unwrap_err().to_string();
        assert!(err.contains("codex-app"), "got: {err}");
    }

    #[tokio::test]
    async fn an_unknown_harness_is_refused_with_the_registry_s_names() {
        let err = run(args("gemini")).await.unwrap_err().to_string();
        assert!(err.contains("gemini"), "got: {err}");
    }
}
