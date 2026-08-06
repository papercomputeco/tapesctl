//! The Codex desktop app — the one harness tapesctl captures without
//! launching, and the different root of trust that costs.
//!
//! Every other harness on this surface is a child of `tapesctl start`. That
//! parentage is what makes attribution provable: the launcher picks an
//! ephemeral port, seeds [`tapes_harnesses::plugin::GATEWAY_NONCE_ENV`] into
//! the child's environment, and afterwards believes a session claim only from
//! a peer that echoes a secret only the launcher and the child ever held.
//!
//! The desktop app is started from the dock. There is no launch, so there is
//! no launch environment, so there is no nonce — and there is no ephemeral
//! port either, because the endpoint the app talks to is written into
//! `$CODEX_HOME/config.toml`, a file that outlives every capture. Both halves
//! of the launch-time contract have to be re-established somewhere earlier.
//!
//! # The install-time contract
//!
//! `tapesctl plugin install codex-app` establishes both, and records them in a
//! **handoff file**:
//!
//! * a **stable loopback address** the app's `config.toml` is patched to point
//!   at, which a later `tapesctl capture codex-app` binds; and
//! * a **locally generated secret**, which authenticates the app's lifecycle
//!   reports to that proxy.
//!
//! The secret is what the nonce was: proof of possession of a value that only
//! the installer and the hook it installed can read. It differs in exactly one
//! respect, and the difference is the whole reason this module exists — it is
//! *persistent*. A launch nonce dies with its session; this one lives in a
//! file until the next install rotates it. So it is written owner-only, it is
//! never logged, it never leaves this machine, and it is generated fresh on
//! every install rather than derived from anything.
//!
//! # What the secret does and does not buy
//!
//! It authenticates the **lifecycle lane** — the reports that say "session
//! `S` exists, and agent `A` runs under it". Without a matching secret a
//! report is refused outright and nothing is recorded, which means traffic it
//! would have named files under `unknown` instead. That is the fail-closed
//! rule this harness is held to: an unattributable desktop turn is `unknown`,
//! never a guess.
//!
//! It does **not** authenticate the **wire lane**. The proxy is a loopback
//! listener, and any local process can post bytes to it — exactly as for
//! `start claude` and `start codex`, whose redirected harnesses prove nothing
//! either. What the secret does buy on that lane is that the *set of session
//! ids a turn may be filed under* is closed: it contains only ids some
//! authenticated report introduced. A local process that has the handoff file
//! but not a running capture can do nothing at all; one that has neither can
//! at most add traffic to a session that genuinely exists.

pub mod hook;
pub mod install;
pub mod lifecycle;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use url::Url;

use crate::error::{Result, error};

/// Directory, relative to the user's home, holding everything this harness's
/// integration owns. Under `~/.tapes` beside the skills directory rather than
/// inside `~/.codex`, because these are tapesctl's files: an uninstall must be
/// able to delete the whole tree without wondering what else lives there.
pub const STATE_DIR: [&str; 2] = [".tapes", "codex-app"];

/// The handoff file's name within [`STATE_DIR`].
pub const HANDOFF_FILE: &str = "handoff.json";

/// Directory within [`STATE_DIR`] holding the packaged plugin source that
/// Codex's own plugin manager installs from.
pub const PLUGIN_DIR: &str = "plugin";

/// Schema version of the handoff document.
///
/// Read strictly: a file written by a different version is refused rather than
/// best-guessed, because every field in it is either a secret or an address
/// this process is about to trust.
pub const HANDOFF_VERSION: u32 = 1;

/// Route the capture proxy serves lifecycle reports on.
///
/// Namespaced under `_tapes` so it cannot collide with an upstream API path
/// the proxy also forwards — every other path on this listener is the
/// harness's traffic.
pub const LIFECYCLE_PATH: &str = "/_tapes/codex-app/lifecycle";

/// Header a lifecycle report presents the handoff secret in.
///
/// Lower-case for the same reason the `X-Tapes-*` names are: HTTP/2 lowercases
/// header names on the wire, so the canonical spelling is the wire spelling.
pub const LIFECYCLE_SECRET_HEADER: &str = "x-tapes-lifecycle-secret";

/// Codex provider id this client declares in `config.toml`.
///
/// Distinct from `start`'s per-process `tapesctl-openai-<uuid>`: that one is
/// suffixed because several `start codex` processes can share a machine and
/// the suffix is how the attribution pipeline tells them apart. This provider
/// is written once into a file the desktop app reads, so a stable name is what
/// makes reinstall idempotent and uninstall able to find what it wrote.
pub const PROVIDER_ID: &str = "tapesctl-codex-app";

/// Display name Codex shows for the patched provider.
pub const PROVIDER_DISPLAY_NAME: &str = "tapesctl capture";

/// Plugin id Codex records hook trust and enablement against.
pub const PLUGIN_NAME: &str = "tapesctl-codex-app";

/// Marketplace name the packaged plugin is offered under, and the second half
/// of the `<plugin>@<marketplace>` spec `codex plugin add` takes.
pub const MARKETPLACE_NAME: &str = "tapesctl";

/// What the installer recorded so that a later capture, and every hook
/// invocation in between, agree on where the proxy is and how to prove a
/// report came from this installation.
///
/// Deserialized with `deny_unknown_fields`: an unrecognised key means the file
/// was written by something that is not this installer, and the safe reading
/// of that is "refuse", not "ignore the parts I don't know".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    /// [`HANDOFF_VERSION`] at the time of writing.
    pub version: u32,

    /// The harness this handoff configures, so a misplaced file is caught by
    /// name rather than by the first field that fails to make sense.
    pub harness_id: String,

    /// Loopback address the app's `config.toml` points at and a capture binds.
    pub proxy_addr: SocketAddr,

    /// The shared secret a lifecycle report must present. Owner-only on disk;
    /// never logged, never forwarded, never included in a captured turn.
    pub secret: String,

    /// Codex provider id patched into `config.toml`, so uninstall removes the
    /// provider this install actually wrote rather than a name recompiled into
    /// a later binary.
    pub provider_id: String,

    /// When the install ran, RFC 3339. Diagnostic only — nothing branches on
    /// it, and a capture must never expire a handoff behind the user's back.
    pub installed_at: String,
}

impl Handoff {
    /// Where the handoff for `home` lives.
    #[must_use]
    pub fn path(home: &Path) -> PathBuf {
        state_dir(home).join(HANDOFF_FILE)
    }

    /// Read and validate the handoff at `path`.
    ///
    /// Every failure mode is the same user-facing problem — "this machine is
    /// not set up, or is set up by a different version" — and every one names
    /// the installer, because re-running it is the only fix and it is
    /// non-destructive.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context(error::CodexAppHandoffReadSnafu {
            path: path.to_path_buf(),
        })?;
        let handoff: Self =
            serde_json::from_str(&text).context(error::CodexAppHandoffParseSnafu {
                path: path.to_path_buf(),
            })?;
        snafu::ensure!(
            handoff.version == HANDOFF_VERSION,
            error::CodexAppHandoffVersionSnafu {
                path: path.to_path_buf(),
                found: handoff.version,
                expected: HANDOFF_VERSION,
            }
        );
        snafu::ensure!(
            handoff.harness_id == tapes_harnesses::envelope::HARNESS_ID_CODEX_APP,
            error::CodexAppHandoffHarnessSnafu {
                path: path.to_path_buf(),
                found: handoff.harness_id.clone(),
            }
        );
        // A blank secret would authenticate nothing: `nonce_matches` refuses an
        // empty expectation, so a capture holding one would refuse every report
        // and file every turn as `unknown` while looking configured. Catch it
        // here, where the message can name the fix.
        snafu::ensure!(
            !handoff.secret.trim().is_empty(),
            error::CodexAppHandoffSecretSnafu {
                path: path.to_path_buf(),
            }
        );
        Ok(handoff)
    }

    /// The base URL the harness's config points at.
    pub fn proxy_base_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}", self.proxy_addr)).context(error::UpstreamUrlSnafu)
    }

    /// Where a hook posts its reports.
    pub fn lifecycle_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}{LIFECYCLE_PATH}", self.proxy_addr))
            .context(error::UpstreamUrlSnafu)
    }
}

/// One lifecycle boundary, as it crosses the loopback between the hook and the
/// capture proxy.
///
/// This is *not* the crate's
/// [`tapes_harnesses::attribution::codex_app::LifecycleObservation`], and the
/// difference is deliberate twice over.
///
/// **It is narrower.** An observation exists to attribute traffic, so the hook
/// projects it down to the fields that actually bind a request to a session
/// and sends nothing else. The transcript path and the event kind are dropped
/// here because nothing on the receiving side reads them; adding a field is a
/// decision to transport it, not the default.
///
/// **It is tapesctl's.** The vocabulary — what a session id is, what an agent
/// id is, which boundaries exist — stays the crate's, and
/// [`Self::from_observation`] is the only place it is read. But
/// `LifecycleObservation` is `Serialize` without `Deserialize`, so a consumer
/// that puts one on a wire cannot take it off again and has to name a mirror
/// type. paper hit the same wall and answered it with its own control-protocol
/// frames; this is the same answer at a smaller scale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleReport {
    /// The **root** Codex session id, exactly as Codex spelled it. On a
    /// subagent boundary this still names the parent — that is what makes it
    /// the key a whole desktop session files under.
    pub session_id: String,

    /// Working directory Codex reported for the session.
    pub cwd: String,

    /// The child thread's own id, present only on subagent boundaries. It is
    /// the lifecycle counterpart of a sub-thread request's `thread-id` header,
    /// which is what lets a child's traffic find its way back to the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl LifecycleReport {
    /// Project one crate observation onto the wire.
    #[must_use]
    pub fn from_observation(
        observation: &tapes_harnesses::attribution::codex_app::LifecycleObservation,
    ) -> Self {
        use tapes_harnesses::attribution::codex_app::LifecycleEvent;
        let agent_id = match &observation.event {
            LifecycleEvent::SubagentStart { agent_id, .. }
            | LifecycleEvent::SubagentStop { agent_id, .. } => Some(agent_id.clone()),
            LifecycleEvent::SessionStart { .. }
            | LifecycleEvent::UserPromptSubmit { .. }
            | LifecycleEvent::Stop { .. } => None,
            // The crate's event enum is `#[non_exhaustive]`: a boundary added
            // there arrives here as a report with no child, which is the
            // conservative reading — it introduces the root and claims no
            // lineage it was not told about.
            _ => None,
        };
        Self {
            session_id: observation.session_id.clone(),
            cwd: observation.cwd.clone(),
            agent_id,
        }
    }
}

/// The directory holding this harness's integration state under `home`.
#[must_use]
pub fn state_dir(home: &Path) -> PathBuf {
    STATE_DIR
        .iter()
        .fold(home.to_path_buf(), |path, segment| path.join(segment))
}

/// The packaged plugin source directory under `home` — what Codex's plugin
/// manager is pointed at.
#[must_use]
pub fn plugin_root(home: &Path) -> PathBuf {
    state_dir(home).join(PLUGIN_DIR)
}

/// `$CODEX_HOME`, or `~/.codex` when it is unset or empty.
///
/// NOTE: this rule is harness knowledge and it is spelled a second time here.
/// The shared crate resolves the same variable for the rollout tree
/// ([`tapes_harnesses::attribution::codex::session::default_sessions_dir`]),
/// explicitly so there is only one spelling of it — but it declares
/// `config.toml` resolution to be the consumer's, so there is no
/// `codex_home()` to call and paper carries its own copy of these four lines
/// too. Two consumers with the same duplicate is the signal that the boundary
/// is drawn one field too far downstream.
#[must_use]
pub fn codex_home(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(".codex"), PathBuf::from)
}

/// The Codex config file this installer patches.
#[must_use]
pub fn codex_config_path(home: &Path) -> PathBuf {
    codex_home(home).join("config.toml")
}

/// A fresh handoff secret.
///
/// Two v4 UUIDs, hyphens stripped: 244 bits from the OS RNG, rendered as 64
/// hex characters. The launch nonce takes one UUID and says so; this takes two
/// because it is the same kind of value with a much longer life — it sits in a
/// file across reboots rather than in one process's memory across one session
/// — and doubling a free secret is cheaper than reasoning about whether 122
/// bits stays comfortable for a value an attacker can grind at offline.
#[must_use]
pub fn generate_secret() -> String {
    let mut secret = String::with_capacity(64);
    for _ in 0..2 {
        secret.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    secret
}

/// Resolve the `--codex-auth` flag.
///
/// Defaults to the plan login, because that is what the desktop app uses: it
/// signs in to ChatGPT and has no `OPENAI_API_KEY` to read. `start codex`
/// resolves this from the ambient environment instead, which would be the
/// wrong question here — the environment at install time is a terminal's, and
/// the app runs under the window server's.
pub fn resolve_auth(flag: Option<&str>) -> Result<tapes_harnesses::launch::CodexAuth> {
    use tapes_harnesses::launch::CodexAuth;
    match flag.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("chatgpt") => Ok(CodexAuth::ChatGpt),
        Some("api-key" | "api_key" | "apikey") => Ok(CodexAuth::ApiKey),
        Some(other) => error::InvalidCodexAuthSnafu {
            value: other.to_owned(),
        }
        .fail(),
    }
}

/// The registry entry for this harness, or a refusal naming what the flag
/// applies to.
///
/// Resolution goes through the shared registry rather than a string compare,
/// so "which harness is captured by lifecycle hooks" stays the crate's answer.
pub fn resolve_hook_harness(name: &str) -> Result<&'static tapes_harnesses::harness::Harness> {
    use tapes_harnesses::harness::{self, AttributionStrategy, Harness};
    let resolved = harness::find(name).context(error::UnknownHarnessSnafu {
        harness: name.to_owned(),
        known: harness::all()
            .iter()
            .map(|harness| harness.id())
            .collect::<Vec<_>>()
            .join(", "),
    })?;
    snafu::ensure!(
        resolved.attribution() == AttributionStrategy::LifecycleHooks,
        error::NotAHookHarnessSnafu {
            harness: resolved.id(),
            hook_harnesses: harness::all()
                .iter()
                .filter(|h| h.attribution() == AttributionStrategy::LifecycleHooks)
                .map(Harness::id)
                .collect::<Vec<_>>()
                .join(", "),
        }
    );
    Ok(resolved)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::launch::CodexAuth;

    fn handoff() -> Handoff {
        Handoff {
            version: HANDOFF_VERSION,
            harness_id: tapes_harnesses::envelope::HARNESS_ID_CODEX_APP.to_owned(),
            proxy_addr: "127.0.0.1:51520".parse().unwrap(),
            secret: generate_secret(),
            provider_id: PROVIDER_ID.to_owned(),
            installed_at: "2026-08-05T00:00:00Z".to_owned(),
        }
    }

    fn write(dir: &Path, handoff: &Handoff) -> PathBuf {
        let path = dir.join(HANDOFF_FILE);
        std::fs::write(&path, serde_json::to_string(handoff).unwrap()).unwrap();
        path
    }

    #[test]
    fn a_handoff_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let written = handoff();
        let path = write(dir.path(), &written);
        assert_eq!(Handoff::read(&path).unwrap(), written);
    }

    /// Everything a capture is about to trust — the address it binds and the
    /// secret it compares against — comes out of this file, so a document
    /// this reader does not fully understand must not be half-honoured.
    #[test]
    fn a_handoff_from_another_version_is_refused_rather_than_guessed_at() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = handoff();
        stale.version = HANDOFF_VERSION + 1;
        let path = write(dir.path(), &stale);

        let err = Handoff::read(&path).unwrap_err().to_string();
        assert!(err.contains("tapesctl plugin install"), "got: {err}");
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HANDOFF_FILE);
        let mut document = serde_json::to_value(handoff()).unwrap();
        document["surprise"] = serde_json::json!("from somewhere else");
        std::fs::write(&path, document.to_string()).unwrap();

        assert!(Handoff::read(&path).is_err());
    }

    /// A blank secret authenticates nothing — `nonce_matches` refuses an empty
    /// expectation — so a capture holding one would look configured while
    /// filing every desktop turn as `unknown`.
    #[test]
    fn a_blank_secret_is_refused_at_read_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut blank = handoff();
        blank.secret = "   ".to_owned();
        let path = write(dir.path(), &blank);

        assert!(Handoff::read(&path).is_err());
    }

    #[test]
    fn a_handoff_for_another_harness_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut foreign = handoff();
        foreign.harness_id = "claude".to_owned();
        let path = write(dir.path(), &foreign);

        let err = Handoff::read(&path).unwrap_err().to_string();
        assert!(err.contains("claude"), "got: {err}");
    }

    #[test]
    fn a_missing_handoff_names_the_installer() {
        let dir = tempfile::tempdir().unwrap();
        let err = Handoff::read(&dir.path().join(HANDOFF_FILE))
            .unwrap_err()
            .to_string();
        assert!(err.contains("tapesctl plugin install"), "got: {err}");
    }

    #[test]
    fn the_lifecycle_url_is_the_proxy_plus_the_route() {
        let handoff = handoff();
        assert_eq!(
            handoff.lifecycle_url().unwrap().as_str(),
            "http://127.0.0.1:51520/_tapes/codex-app/lifecycle",
        );
        assert_eq!(
            handoff.proxy_base_url().unwrap().as_str(),
            "http://127.0.0.1:51520/",
        );
    }

    /// A secret an attacker can grind at offline: worth asserting it is both
    /// long and not a constant.
    #[test]
    fn secrets_are_long_and_never_repeat() {
        let first = generate_secret();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, generate_secret());
    }

    #[test]
    fn the_auth_flag_defaults_to_the_plan_login_the_desktop_app_uses() {
        assert_eq!(resolve_auth(None).unwrap(), CodexAuth::ChatGpt);
        assert_eq!(resolve_auth(Some(" ChatGPT ")).unwrap(), CodexAuth::ChatGpt);
        assert_eq!(resolve_auth(Some("api-key")).unwrap(), CodexAuth::ApiKey);
        let err = resolve_auth(Some("oauth")).unwrap_err().to_string();
        assert!(err.contains("oauth"), "got: {err}");
    }

    /// The registry decides which harness this surface is for, so a harness
    /// that gains lifecycle hooks in the crate is accepted here without this
    /// file changing — and one that has not is refused by name.
    #[test]
    fn only_a_lifecycle_hook_harness_resolves() {
        assert_eq!(resolve_hook_harness("codex-app").unwrap().id(), "codex-app");
        assert_eq!(
            resolve_hook_harness("codex-desktop").unwrap().id(),
            "codex-app",
        );

        let err = resolve_hook_harness("pi").unwrap_err().to_string();
        assert!(err.contains("codex-app"), "got: {err}");
        assert!(resolve_hook_harness("gemini").is_err());
    }

    #[test]
    fn the_codex_home_override_wins_over_the_default() {
        let home = Path::new("/home/someone");
        // Asserted through the public helper rather than by reading the
        // variable here, because this is the second spelling of a rule the
        // crate also owns and the duplicate is the thing worth pinning.
        let resolved = codex_config_path(home);
        assert!(resolved.ends_with("config.toml"), "got: {resolved:?}");
    }
}
