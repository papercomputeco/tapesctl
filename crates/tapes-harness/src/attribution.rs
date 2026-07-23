//! Session attribution.
//!
//! Ported from paperd's `proxy::session::*` during Track 1. Attribution is how
//! a captured harness session acquires a real identity — session id, fork
//! parent, cwd, and the acting subject — rather than a synthetic root hash.
//!
//! Per-harness sources this module will cover (from paperd today):
//! - Claude: peer-PID → `~/.claude/sessions/<pid>.json` (session id, cwd,
//!   version, name); fork-parent recovery from the transcript dir
//!   (`~/.claude/projects/<cwd-encoded>/<sid>.jsonl` `parentUuid`).
//! - Codex: `session_meta` rollout watcher.
//!
//! Standalone `auth_subject` defaults to `local:<os-username>` and is
//! overridable via config (agents/CI set e.g. `gardener-ci`); empty is allowed
//! (NULL). Nothing parses the prefix — it is an opaque attribution string, the
//! same envelope field in both the standalone and platform worlds.

/// Attribution facts discovered for a captured harness session.
///
/// Fields are optional because discovery is best-effort and time-budgeted; an
/// absent field means "unknown", never a sentinel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// The harness's own session identifier, if recovered.
    pub session_id: Option<String>,
    /// The parent session id for a forked/resumed session, if recovered.
    pub parent_session_id: Option<String>,
    /// The working directory the harness was launched in.
    pub cwd: Option<String>,
    /// The acting subject (`local:<user>` standalone; gateway-stamped on the
    /// platform). Empty/None is allowed.
    pub auth_subject: Option<String>,
}
