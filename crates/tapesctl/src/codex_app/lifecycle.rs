//! The lifecycle receiver: the trust boundary a desktop session crosses.
//!
//! A request arriving at the capture proxy from the Codex desktop app carries
//! Codex's own `thread-id` and `session-id` headers, and nothing else that
//! names a session. Those headers are unauthenticated — the proxy is a
//! loopback listener that every process on the machine can reach — so on their
//! own they cannot decide which session a turn belongs to, or whether it
//! belongs to a Codex session at all.
//!
//! What makes them usable is this module: the **set of session ids a turn may
//! be filed under is closed**, and only an authenticated lifecycle report can
//! add to it. A report presents the handoff secret, and a report that does not
//! is refused outright — nothing is recorded, and traffic it would have named
//! files under `unknown` instead. That is the whole fail-closed rule for this
//! harness, and it lives here.
//!
//! # What an attacker can and cannot do
//!
//! * **Handoff file but no running capture** — nothing. There is no listener
//!   to report to.
//! * **A running capture but no secret** — cannot introduce a session id.
//!   They can still post bytes to the proxy and have them forwarded, exactly
//!   as they could to `start claude`'s proxy, but the turn files under
//!   `unknown` unless it names a session some authenticated report already
//!   introduced.
//! * **The secret** — can introduce arbitrary session ids and then file
//!   traffic under them. This is the same authority the real hook has, which
//!   is why the handoff is owner-only and why every install rotates it.
//!
//! The wire lane is deliberately *not* authenticated beyond that. Requiring a
//! per-request secret from the app would mean smuggling one through
//! `config.toml`'s static header table, where it would sit in a file the app
//! reads and would be sent upstream on every request — strictly worse than the
//! closed-set property this buys instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use http::{HeaderMap, StatusCode};
use tapes_capture::envelope::{HARNESS_ID_CODEX_APP, TapesAttribution};
use tapes_harnesses::attribution::codex::session::CODEX_ROLLOUT_ID_HEADERS;
use tapes_harnesses::plugin::nonce_matches;
use tracing::{debug, warn};
use url::Url;

use super::{LIFECYCLE_SECRET_HEADER, LifecycleReport};

/// How many root sessions one capture remembers.
///
/// A bound rather than a policy: reports are authenticated, so this is not
/// defending against a flood, it is keeping a process that runs for days from
/// growing without limit. Eviction is oldest-first and takes the evicted
/// root's children with it, so a session that falls out becomes unattributable
/// rather than misattributed.
pub const MAX_REMEMBERED_SESSIONS: usize = 1024;

/// A desktop session a report introduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSession {
    /// The root session id — what a turn is filed under.
    pub session_id: String,
    /// Working directory Codex reported, or `None` if it reported none.
    pub cwd: Option<String>,
}

impl DesktopSession {
    /// The envelope a turn belonging to this session carries.
    ///
    /// Nothing is invented. The harness id is the crate's constant for this
    /// harness — the same one paper re-keys desktop sessions to, so one
    /// session captured through either client lands under one harness — and
    /// every other field is either something a report actually said or
    /// absent.
    #[must_use]
    pub fn envelope(&self) -> TapesAttribution {
        TapesAttribution {
            harness_id: HARNESS_ID_CODEX_APP.to_owned(),
            session_id: Some(self.session_id.clone()),
            version: None,
            cwd: self.cwd.clone(),
            name: None,
            // A subagent's traffic is joined to its root through the root
            // session id it is filed under and the `meta.thread_id` the proxy
            // reads off the request, which is how Codex sessions carry lineage
            // everywhere else. A parent *session* id would say something
            // different and untrue: that this session forked from another.
            parent_sid: None,
            metadata: serde_json::Map::new(),
        }
    }
}

/// Every desktop session this capture has been told about, and the secret that
/// authenticates being told.
pub struct DesktopSessions {
    /// The handoff secret. Compared with the crate's constant-time-in-the-
    /// matching-prefix rule, which also refuses an empty expectation — so a
    /// capture that somehow started without a secret authenticates nothing
    /// rather than everything.
    secret: String,
    /// Console base URL, for the link printed when a session first appears.
    web_url: Option<Url>,
    registry: Mutex<Registry>,
}

/// The identity map, behind one lock.
#[derive(Debug, Default)]
struct Registry {
    /// Root session id to what is known about it.
    roots: HashMap<String, DesktopSession>,
    /// Child thread id to the root it runs under.
    children: HashMap<String, String>,
    /// Root ids in the order they were first seen, for eviction.
    order: Vec<String>,
}

impl DesktopSessions {
    /// A registry authenticated by `secret`.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            web_url: None,
            registry: Mutex::new(Registry::default()),
        }
    }

    /// Print a console link when a session first appears.
    #[must_use]
    pub fn with_web_url(mut self, web_url: Option<Url>) -> Self {
        self.web_url = web_url;
        self
    }

    /// Which session, if any, a request naming these identities belongs to.
    ///
    /// Every candidate is matched by **exact equality** against something an
    /// authenticated report introduced. There is no prefix rule, no recency
    /// heuristic, and no fallback to "the only session we know about" — an
    /// identity nobody reported resolves to nothing, and the turn files under
    /// `unknown`.
    pub fn resolve<'a>(
        &self,
        identities: impl IntoIterator<Item = &'a str>,
    ) -> Option<DesktopSession> {
        let registry = self.registry.lock().ok()?;
        identities.into_iter().find_map(|identity| {
            registry
                .roots
                .get(identity)
                .or_else(|| {
                    registry
                        .children
                        .get(identity)
                        .and_then(|root| registry.roots.get(root))
                })
                .cloned()
        })
    }

    /// How many root sessions are known. Diagnostic; also what the exit
    /// summary counts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.registry
            .lock()
            .map_or(0, |registry| registry.roots.len())
    }

    /// Whether any session has been introduced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record one authenticated report.
    fn record(&self, report: &LifecycleReport) {
        let Ok(mut registry) = self.registry.lock() else {
            warn!("the desktop session registry is poisoned; this report is dropped");
            return;
        };
        let cwd = (!report.cwd.trim().is_empty()).then(|| report.cwd.clone());
        let fresh = !registry.roots.contains_key(&report.session_id);
        if fresh {
            registry.order.push(report.session_id.clone());
        }
        registry.roots.insert(
            report.session_id.clone(),
            DesktopSession {
                session_id: report.session_id.clone(),
                cwd,
            },
        );
        if let Some(agent_id) = &report.agent_id {
            registry
                .children
                .insert(agent_id.clone(), report.session_id.clone());
        }
        registry.evict_to_bound();
        drop(registry);

        if fresh {
            self.announce(&report.session_id);
        }
    }

    /// Say a session appeared, once, on the terminal this command owns.
    ///
    /// Unlike `start`, a capture is not holding a harness's TUI — the terminal
    /// stays ours for the whole run — so a session can be named the moment it
    /// is introduced rather than only after the process exits.
    fn announce(&self, session_id: &str) {
        match self
            .web_url
            .as_ref()
            .and_then(|base| base.join(&format!("/sessions/{session_id}")).ok())
        {
            Some(url) => println!("tapesctl: capturing codex-app session {session_id} — {url}"),
            None => println!("tapesctl: capturing codex-app session {session_id}"),
        }
    }
}

impl Registry {
    /// Drop the oldest roots, and any child pointing at one, until the bound
    /// holds.
    ///
    /// Children go with their root rather than being left dangling: a child
    /// whose root is gone would otherwise resolve to nothing on the next
    /// lookup anyway, and keeping it would make the map the thing that grows.
    fn evict_to_bound(&mut self) {
        while self.roots.len() > MAX_REMEMBERED_SESSIONS {
            if self.order.is_empty() {
                break;
            }
            let evicted = self.order.remove(0);
            self.roots.remove(&evicted);
            self.children.retain(|_, root| root != &evicted);
        }
    }
}

/// `POST` handler for [`super::LIFECYCLE_PATH`].
///
/// Answers a bare status, never a body. Whatever this refuses, it refuses to a
/// caller that may not be the hook, and an explanatory body would be free
/// information about what a valid report looks like.
pub async fn receive(
    State(sessions): State<Arc<DesktopSessions>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    let presented = headers
        .get(LIFECYCLE_SECRET_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !nonce_matches(&sessions.secret, presented) {
        // Logged without either the expected or the presented value: this is
        // the one place both exist, and the log is the easiest place to leak
        // a secret from. Whoever reads it needs to know a refusal happened,
        // not what was tried.
        warn!(
            "a lifecycle report did not present this installation's secret; \
             it was refused and its session was not registered",
        );
        return StatusCode::UNAUTHORIZED;
    }

    let Ok(report) = serde_json::from_slice::<LifecycleReport>(&body) else {
        warn!("an authenticated lifecycle report was not a report; nothing recorded");
        return StatusCode::BAD_REQUEST;
    };
    // A blank session id would key the whole registry on the empty string and
    // make every unattributable request match it.
    if report.session_id.trim().is_empty() {
        warn!("a lifecycle report named no session; nothing recorded");
        return StatusCode::BAD_REQUEST;
    }

    sessions.record(&report);
    debug!(session_id = %report.session_id, "desktop session registered");
    StatusCode::NO_CONTENT
}

/// The identities a Codex request offers, in the order they should be tried.
///
/// The spellings are the crate's — the same pair its rollout lane narrows on —
/// so a Codex header rename moves both readers at once. Thread first: on a
/// subagent's request that is the child's own id, which resolves through the
/// child map; on a root turn the two headers are equal, so the order is
/// immaterial there.
#[must_use]
pub fn request_identities(headers: &HeaderMap) -> Vec<&str> {
    CODEX_ROLLOUT_ID_HEADERS
        .iter()
        .filter_map(|name| {
            headers
                .get(*name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn sessions() -> Arc<DesktopSessions> {
        Arc::new(DesktopSessions::new(SECRET))
    }

    fn report(session_id: &str, agent_id: Option<&str>) -> axum::body::Bytes {
        serde_json::to_vec(&LifecycleReport {
            session_id: session_id.to_owned(),
            cwd: "/tmp/work".to_owned(),
            agent_id: agent_id.map(str::to_owned),
        })
        .unwrap()
        .into()
    }

    fn headers(secret: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(secret) = secret {
            headers.insert(LIFECYCLE_SECRET_HEADER, secret.parse().unwrap());
        }
        headers
    }

    fn request(thread: Option<&str>, session: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(thread) = thread {
            headers.insert("thread-id", thread.parse().unwrap());
        }
        if let Some(session) = session {
            headers.insert("session-id", session.parse().unwrap());
        }
        headers
    }

    #[tokio::test]
    async fn an_authenticated_report_introduces_the_session() {
        let sessions = sessions();
        assert_eq!(
            receive(
                State(Arc::clone(&sessions)),
                headers(Some(SECRET)),
                report("root", None),
            )
            .await,
            StatusCode::NO_CONTENT,
        );

        let resolved = sessions.resolve(["root"]).unwrap();
        assert_eq!(resolved.session_id, "root");
        assert_eq!(resolved.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(resolved.envelope().harness_id, HARNESS_ID_CODEX_APP);
    }

    /// The trust boundary, proved from both directions: a report with no
    /// secret and a report with the wrong one are both refused, and neither
    /// leaves anything behind that a later request could be filed under.
    #[tokio::test]
    async fn an_unauthenticated_report_is_refused_and_records_nothing() {
        for presented in [None, Some(""), Some("wrong"), Some(&SECRET[..63])] {
            let sessions = sessions();
            let status = receive(
                State(Arc::clone(&sessions)),
                headers(presented),
                report("root", None),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "presented: {presented:?}");
            assert!(
                sessions.resolve(["root"]).is_none(),
                "a refused report must not introduce a session: {presented:?}",
            );
            assert!(sessions.is_empty());
        }
    }

    /// The rotation case: an install rewrites the secret while a capture is
    /// still holding the previous one. Reports must start being refused, not
    /// keep being believed — and the session then files as `unknown`, which is
    /// the fail-closed direction and is recoverable by restarting the capture.
    #[tokio::test]
    async fn a_report_signed_with_a_rotated_secret_is_refused() {
        let sessions = Arc::new(DesktopSessions::new(SECRET));
        let rotated = "f".repeat(64);
        assert_eq!(
            receive(
                State(Arc::clone(&sessions)),
                headers(Some(&rotated)),
                report("root", None),
            )
            .await,
            StatusCode::UNAUTHORIZED,
        );
        assert!(sessions.resolve(["root"]).is_none());
    }

    /// A capture that somehow held no secret must authenticate nothing rather
    /// than everything — the crate's comparison refuses an empty expectation
    /// and this is the consequence that matters.
    #[tokio::test]
    async fn a_capture_with_no_secret_accepts_no_report() {
        let sessions = Arc::new(DesktopSessions::new(""));
        for presented in [None, Some("")] {
            assert_eq!(
                receive(
                    State(Arc::clone(&sessions)),
                    headers(presented),
                    report("root", None),
                )
                .await,
                StatusCode::UNAUTHORIZED,
            );
        }
    }

    #[tokio::test]
    async fn an_authenticated_but_unreadable_report_records_nothing() {
        let sessions = sessions();
        for body in [
            axum::body::Bytes::from_static(b"not json"),
            // A well-formed report naming no session: it would otherwise key
            // the registry on the empty string.
            report("   ", None),
            // An extra field means this was written by something that is not
            // the hook this install wrote.
            axum::body::Bytes::from(
                serde_json::json!({"session_id": "root", "cwd": "/tmp", "extra": 1}).to_string(),
            ),
        ] {
            assert_eq!(
                receive(State(Arc::clone(&sessions)), headers(Some(SECRET)), body).await,
                StatusCode::BAD_REQUEST,
            );
        }
        assert!(sessions.is_empty());
    }

    /// A subagent's request names the child's own thread, so the child map is
    /// what joins its traffic to the session that spawned it.
    #[tokio::test]
    async fn a_child_thread_resolves_to_the_root_that_reported_it() {
        let sessions = sessions();
        receive(
            State(Arc::clone(&sessions)),
            headers(Some(SECRET)),
            report("root", Some("child")),
        )
        .await;

        assert_eq!(sessions.resolve(["child"]).unwrap().session_id, "root");
        assert_eq!(
            sessions
                .resolve(request_identities(&request(Some("child"), Some("root"))))
                .unwrap()
                .session_id,
            "root",
        );
    }

    /// The closed-set property, stated as a test: an identity nobody reported
    /// resolves to nothing, so its traffic files as `unknown` rather than
    /// falling back to whatever session happens to be known.
    #[tokio::test]
    async fn an_identity_no_report_introduced_resolves_to_nothing() {
        let sessions = sessions();
        receive(
            State(Arc::clone(&sessions)),
            headers(Some(SECRET)),
            report("root", None),
        )
        .await;

        assert!(sessions.resolve(["some-other-session"]).is_none());
        // Including the request that names no identity at all.
        assert!(
            sessions
                .resolve(request_identities(&request(None, None)))
                .is_none()
        );
        // And a prefix of a known id: matching is equality, never a prefix.
        assert!(sessions.resolve(["roo"]).is_none());
    }

    #[test]
    fn request_identities_are_read_thread_first_in_the_crates_spelling() {
        assert_eq!(
            request_identities(&request(Some("child"), Some("root"))),
            vec!["child", "root"],
        );
        assert_eq!(
            request_identities(&request(None, Some("root"))),
            vec!["root"]
        );
        // Blank values are absent, not empty candidates that could match a
        // registry keyed on the empty string.
        assert!(request_identities(&request(Some("  "), None)).is_empty());
    }

    #[tokio::test]
    async fn the_registry_stays_bounded_and_evicts_children_with_their_root() {
        let sessions = sessions();
        for index in 0..=MAX_REMEMBERED_SESSIONS {
            receive(
                State(Arc::clone(&sessions)),
                headers(Some(SECRET)),
                report(&format!("root-{index}"), Some(&format!("child-{index}"))),
            )
            .await;
        }
        assert_eq!(sessions.len(), MAX_REMEMBERED_SESSIONS);
        // The oldest root fell out, and took its child with it.
        assert!(sessions.resolve(["root-0"]).is_none());
        assert!(sessions.resolve(["child-0"]).is_none());
        assert!(sessions.resolve(["root-1"]).is_some());
    }

    /// Re-reporting a session — five hooks fire per turn — must update it in
    /// place rather than push it through the eviction queue again.
    #[tokio::test]
    async fn repeating_a_session_does_not_grow_the_registry() {
        let sessions = sessions();
        for _ in 0..10 {
            receive(
                State(Arc::clone(&sessions)),
                headers(Some(SECRET)),
                report("root", None),
            )
            .await;
        }
        assert_eq!(sessions.len(), 1);
    }
}
