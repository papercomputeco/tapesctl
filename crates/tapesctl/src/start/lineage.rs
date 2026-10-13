//! Launch lineage: which captured session a nested `tapesctl start` runs inside.
//!
//! A captured harness can start another one — a shell tool in a Claude Code
//! session runs `tapesctl start hermes`. Without help the two land as unrelated
//! sessions. [`tapes_capture::launch`] is the shared half of the fix: every
//! launcher exports `TAPES_LAUNCH_ID` / `TAPES_LAUNCH_HARNESS_ID` to its child,
//! and a launcher that finds them in its own environment knows which *launch* it
//! runs inside. Turning that launch into a parent *session* is the launcher's
//! half, and this module is tapesctl's.
//!
//! # Why a state file per launch
//!
//! Every `tapesctl start` runs its own just-in-time proxy and keeps no daemon,
//! so the outer launch's knowledge — "the session my harness is using right
//! now" — lives in a different process from the nested launch that needs it.
//! There is no control socket to ask. The two options were to add one, or to
//! have each launch publish that one fact where a nested launch can read it.
//!
//! The file wins on every axis that matters here. It needs no listener, no
//! protocol, and no liveness handshake; the reader cannot block or hang the
//! writer; and it works when the outer proxy is busy, slow, or already exiting.
//! A socket would add a second loopback surface to authenticate for the sake of
//! one string. So each launch keeps `~/.tapes/launches/<launch-id>.json`
//! holding its harness and its current session, rewrites it when that session
//! changes, and deletes it when the harness exits. `~/.tapes` is already this
//! client's state directory, and `tapesctl uninstall` already removes it.
//!
//! # What this is not
//!
//! Not a trust signal. The launch variables are inherited by every descendant
//! and trivially forged, and a record file is readable by anything running as
//! this user. A forged lineage can at worst name a wrong *parent* on a turn
//! whose own session was attributed by the usual proofs; it can never change
//! whose session a turn is filed under. Two guards keep even that narrow: a
//! launch id must be a plain token before it is joined onto a path, and the
//! record's harness must agree with the harness the environment claims.
//!
//! A parent is only ever added, never substituted: a turn whose own envelope
//! already names a parent — a hermes or Codex subagent — keeps it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tapes_capture::envelope::{self, TapesAttribution};
use tapes_capture::launch::LaunchContext;
use tracing::{debug, warn};

/// Where launch records live, relative to the home directory.
pub const LAUNCHES_DIR: [&str; 2] = [".tapes", "launches"];

/// The longest launch id this client will look up. A UUID is 36 characters;
/// the shared crate's own minted ids are shorter than this too.
const MAX_LAUNCH_ID_LEN: usize = 128;

/// What one launch publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecord {
    /// The launch this record describes — repeated inside the file so a
    /// renamed or copied file cannot answer for a launch it is not.
    pub launch_id: String,
    /// The harness that launch started.
    pub harness_id: String,
    /// The harness session most recently attributed under that launch.
    pub session_id: String,
}

/// The directory of launch records.
#[derive(Debug, Clone)]
pub struct LaunchRegistry {
    dir: PathBuf,
}

impl LaunchRegistry {
    /// A registry rooted at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `~/.tapes/launches` under `home`.
    #[must_use]
    pub fn default_dir(home: &Path) -> PathBuf {
        LAUNCHES_DIR
            .iter()
            .fold(home.to_path_buf(), |path, part| path.join(part))
    }

    /// The record path for `launch_id`, or `None` when the id is not a plain
    /// token. The id arrives from an inherited environment variable, so it is
    /// validated before it gets anywhere near a path join: `../x` must not
    /// name a file outside this directory.
    fn path_for(&self, launch_id: &str) -> Option<PathBuf> {
        is_plain_launch_id(launch_id).then(|| self.dir.join(format!("{launch_id}.json")))
    }

    /// Publish `record`, replacing any earlier one for the same launch.
    ///
    /// Written to a private temporary file in the same directory and renamed
    /// over the target, so a concurrent reader sees the old record or the new
    /// one and never a torn write.
    pub fn record(&self, record: &LaunchRecord) -> std::io::Result<()> {
        let path = self.path_for(&record.launch_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "launch id is not a token")
        })?;
        create_private_dir(&self.dir)?;
        let body = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        // `NamedTempFile` creates with mode 0600 on unix.
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(&body)?;
        file.persist(&path).map_err(|err| err.error)?;
        Ok(())
    }

    /// The record for `launch_id`, if one exists, parses, and describes that
    /// launch.
    #[must_use]
    pub fn lookup(&self, launch_id: &str) -> Option<LaunchRecord> {
        let path = self.path_for(launch_id)?;
        let bytes = std::fs::read(&path).ok()?;
        let record: LaunchRecord = serde_json::from_slice(&bytes).ok()?;
        (record.launch_id == launch_id && !record.session_id.trim().is_empty()).then_some(record)
    }

    /// Delete the record for `launch_id`. A missing record is not an error.
    pub fn remove(&self, launch_id: &str) {
        let Some(path) = self.path_for(launch_id) else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(path = %path.display(), error = %err, "could not remove launch record")
            }
        }
    }
}

/// A launch id fit to name a file: ASCII alphanumerics, `-` and `_`, bounded.
fn is_plain_launch_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LAUNCH_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// One launch's lineage: its own context, the registry it publishes to, and
/// the parent session it resolves to, once known.
#[derive(Debug)]
pub struct LaunchLineage {
    context: LaunchContext,
    registry: LaunchRegistry,
    /// The session id last published, so a record is rewritten only when the
    /// live session changes rather than on every request.
    published: Mutex<Option<String>>,
    /// The parent session, once resolved. Resolution is retried until it
    /// succeeds and then held: the parent session that was live when this
    /// launch's first turn landed is the one it runs inside.
    parent_session: Mutex<Option<String>>,
}

impl LaunchLineage {
    /// Lineage for `context`, publishing to and reading from `registry`.
    #[must_use]
    pub fn new(context: LaunchContext, registry: LaunchRegistry) -> Self {
        Self {
            context,
            registry,
            published: Mutex::new(None),
            parent_session: Mutex::new(None),
        }
    }

    /// This launch's context.
    #[must_use]
    pub fn context(&self) -> &LaunchContext {
        &self.context
    }

    /// Note an attributed request, publishing its session as this launch's
    /// live one when it belongs to the harness this launch started.
    ///
    /// Best-effort: a failed write is logged and the request proceeds — a
    /// missing record costs a nested launch its parent, never a turn.
    pub fn observe(&self, attribution: &TapesAttribution) {
        if attribution.harness_id != self.context.harness_id {
            return;
        }
        let Some(session_id) = attribution
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|sid| !sid.is_empty())
        else {
            return;
        };
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if published.as_deref() == Some(session_id) {
            return;
        }
        let record = LaunchRecord {
            launch_id: self.context.launch_id.clone(),
            harness_id: self.context.harness_id.clone(),
            session_id: session_id.to_owned(),
        };
        match self.registry.record(&record) {
            Ok(()) => *published = Some(record.session_id),
            Err(err) => warn!(error = %err, "could not publish this launch's session"),
        }
    }

    /// The parent session this launch runs inside, and the harness it belongs
    /// to — `None` when this launch has no parent, or the parent's launcher
    /// published nothing this client can read.
    #[must_use]
    pub fn parent(&self) -> Option<(String, String)> {
        let parent = self.context.parent.as_ref()?;
        let mut cached = self
            .parent_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(session_id) = cached.as_ref() {
            return Some((session_id.clone(), parent.harness_id.clone()));
        }
        let record = self.registry.lookup(&parent.launch_id)?;
        if record.harness_id != parent.harness_id {
            debug!(
                claimed = %parent.harness_id,
                recorded = %record.harness_id,
                "the parent launch's record names a different harness; ignoring it",
            );
            return None;
        }
        *cached = Some(record.session_id.clone());
        Some((record.session_id, parent.harness_id.clone()))
    }

    /// Add the parent session to `attribution` when it has none of its own.
    ///
    /// Returns the attribution and whether a parent was added. Unchanged when
    /// the turn is unattributed, already names a parent, or the parent would
    /// be the turn's own session.
    #[must_use]
    pub fn apply(&self, attribution: TapesAttribution) -> (TapesAttribution, bool) {
        if attribution.harness_id == envelope::HARNESS_ID_UNKNOWN {
            return (attribution, false);
        }
        let own_session = attribution
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|sid| !sid.is_empty());
        let has_parent = attribution
            .parent_sid
            .as_deref()
            .is_some_and(|sid| !sid.trim().is_empty());
        let Some(own_session) = own_session else {
            return (attribution, false);
        };
        if has_parent {
            return (attribution, false);
        }
        let Some((parent_session, parent_harness)) = self.parent() else {
            return (attribution, false);
        };
        if parent_session == own_session && parent_harness == attribution.harness_id {
            return (attribution, false);
        }
        let mut attribution = attribution;
        attribution.parent_sid = Some(parent_session);
        (attribution.with_parent_harness_id(parent_harness), true)
    }

    /// Withdraw this launch's record. Called once the harness has exited.
    pub fn finish(&self) {
        self.registry.remove(&self.context.launch_id);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(ctx: &LaunchContext) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = ctx
            .child_env()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn session(harness: &str, sid: &str) -> TapesAttribution {
        let mut attribution = TapesAttribution::unknown();
        attribution.harness_id = harness.to_owned();
        attribution.session_id = Some(sid.to_owned());
        attribution
    }

    #[test]
    fn a_nested_launch_names_the_outer_launchs_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LaunchRegistry::new(dir.path());

        let outer = LaunchLineage::new(
            LaunchContext::from_lookup("L-outer", "claude", |_| None),
            registry.clone(),
        );
        outer.observe(&session("claude", "S-outer"));

        let inner_ctx = LaunchContext::from_lookup("L-inner", "hermes", env_of(outer.context()));
        let inner = LaunchLineage::new(inner_ctx, registry);
        let (got, applied) = inner.apply(session("hermes", "S-inner"));
        assert!(applied);
        assert_eq!(got.parent_sid.as_deref(), Some("S-outer"));
        assert_eq!(got.parent_harness_id.as_deref(), Some("claude"));
        assert_eq!(got.session_id.as_deref(), Some("S-inner"));
    }

    #[test]
    fn a_launch_without_a_parent_leaves_attribution_alone() {
        let dir = tempfile::tempdir().unwrap();
        let lineage = LaunchLineage::new(
            LaunchContext::from_lookup("L1", "codex", |_| None),
            LaunchRegistry::new(dir.path()),
        );
        let (got, applied) = lineage.apply(session("codex", "S1"));
        assert!(!applied);
        assert_eq!(got.parent_sid, None);
        assert_eq!(got.parent_harness_id, None);
    }

    #[test]
    fn a_parent_the_envelope_already_names_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LaunchRegistry::new(dir.path());
        let outer = LaunchLineage::new(
            LaunchContext::from_lookup("L-outer", "pi", |_| None),
            registry.clone(),
        );
        outer.observe(&session("pi", "S-outer"));
        let inner = LaunchLineage::new(
            LaunchContext::from_lookup("L-inner", "hermes", env_of(outer.context())),
            registry,
        );
        let mut delegated = session("hermes", "S-child");
        delegated.parent_sid = Some("S-hermes-root".to_owned());
        let (got, applied) = inner.apply(delegated.with_parent_harness_id("hermes"));
        assert!(!applied);
        assert_eq!(got.parent_sid.as_deref(), Some("S-hermes-root"));
        assert_eq!(got.parent_harness_id.as_deref(), Some("hermes"));
    }

    #[test]
    fn an_unattributed_turn_gets_no_parent() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LaunchRegistry::new(dir.path());
        registry
            .record(&LaunchRecord {
                launch_id: "L-outer".to_owned(),
                harness_id: "claude".to_owned(),
                session_id: "S-outer".to_owned(),
            })
            .unwrap();
        let inner = LaunchLineage::new(
            LaunchContext::from_lookup("L-inner", "codex", |name| {
                Some(
                    match name {
                        "TAPES_LAUNCH_ID" => "L-outer",
                        _ => "claude",
                    }
                    .to_owned(),
                )
            }),
            registry,
        );
        let (got, applied) = inner.apply(TapesAttribution::unknown());
        assert!(!applied);
        assert_eq!(got.parent_sid, None);
    }

    #[test]
    fn a_record_for_a_different_harness_than_claimed_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LaunchRegistry::new(dir.path());
        registry
            .record(&LaunchRecord {
                launch_id: "L-outer".to_owned(),
                harness_id: "codex".to_owned(),
                session_id: "S-outer".to_owned(),
            })
            .unwrap();
        let ctx = LaunchContext::from_lookup("L-inner", "hermes", |name| {
            Some(
                match name {
                    "TAPES_LAUNCH_ID" => "L-outer",
                    _ => "claude",
                }
                .to_owned(),
            )
        });
        let inner = LaunchLineage::new(ctx, registry);
        assert_eq!(inner.parent(), None);
    }

    #[test]
    fn an_unreadable_parent_launch_yields_no_parent() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LaunchContext::from_lookup("L-inner", "hermes", |name| {
            Some(
                match name {
                    "TAPES_LAUNCH_ID" => "L-nobody-published",
                    _ => "claude",
                }
                .to_owned(),
            )
        });
        let inner = LaunchLineage::new(ctx, LaunchRegistry::new(dir.path()));
        let (got, applied) = inner.apply(session("hermes", "S"));
        assert!(!applied);
        assert_eq!(got.parent_sid, None);
    }

    #[test]
    fn a_launch_id_that_is_not_a_token_never_reaches_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LaunchRegistry::new(dir.path().join("launches"));
        std::fs::write(
            dir.path().join("evil.json"),
            r#"{"launch_id":"../evil","harness_id":"claude","session_id":"S"}"#,
        )
        .unwrap();
        assert_eq!(registry.lookup("../evil"), None);
        assert_eq!(registry.lookup(""), None);
        assert_eq!(registry.lookup("a/b"), None);
        assert!(
            registry
                .record(&LaunchRecord {
                    launch_id: "../evil".to_owned(),
                    harness_id: "claude".to_owned(),
                    session_id: "S".to_owned(),
                })
                .is_err()
        );
    }

    #[test]
    fn the_record_follows_the_live_session_and_is_withdrawn_at_exit() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LaunchRegistry::new(dir.path());
        let lineage = LaunchLineage::new(
            LaunchContext::from_lookup("L1", "claude", |_| None),
            registry.clone(),
        );
        // Another harness's session, or none, is not this launch's.
        lineage.observe(&session("codex", "S-other"));
        lineage.observe(&TapesAttribution::unknown());
        assert_eq!(registry.lookup("L1"), None);

        lineage.observe(&session("claude", "S1"));
        assert_eq!(registry.lookup("L1").unwrap().session_id, "S1");
        lineage.observe(&session("claude", "S2"));
        assert_eq!(registry.lookup("L1").unwrap().session_id, "S2");

        lineage.finish();
        assert_eq!(registry.lookup("L1"), None);
        lineage.finish();
    }

    #[cfg(unix)]
    #[test]
    fn records_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("launches");
        let registry = LaunchRegistry::new(&root);
        registry
            .record(&LaunchRecord {
                launch_id: "L1".to_owned(),
                harness_id: "claude".to_owned(),
                session_id: "S1".to_owned(),
            })
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("L1.json")), 0o600);
    }

    #[test]
    fn the_default_directory_is_under_the_tapes_state_dir() {
        assert_eq!(
            LaunchRegistry::default_dir(Path::new("/h")),
            Path::new("/h/.tapes/launches")
        );
    }
}
