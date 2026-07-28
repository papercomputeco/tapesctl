//! Session attribution.
//!
//! Attribution is how a captured harness session acquires a real identity —
//! session id, fork parent, cwd, and the acting subject — rather than a
//! synthetic root hash. This module is the extracted form of paperd's
//! `proxy::session::*`, which validated peer-PID attribution and fork-parent
//! discovery against real Claude and Codex traffic; it now backs both
//! `tapesctl start` and `paper start`, so the two capture paths attribute
//! identically by construction rather than by review.
//!
//! Responsibilities:
//!
//! * [`watcher`] — polls `~/.claude/sessions/` every 1 s and maintains the
//!   candidate-PID set and parsed metadata snapshots a request handler reads
//!   wait-free.
//! * [`peer_pid`] — maps an accepted loopback connection to one of the
//!   candidate PIDs via per-OS kernel APIs.
//! * [`claude_session`] — verbatim shape of `~/.claude/sessions/<pid>.json`
//!   and its read helper.
//! * [`fork_parent`] — bounded scan of `~/.claude/projects/<cwd>/*.jsonl` to
//!   recover fork-parent lineage. Callers are expected to cache the result
//!   per-sid; discovery is deliberately time-budgeted, not free.
//! * [`codex_session`] / [`codex_process`] / [`codex_watcher`] — the Codex
//!   equivalents, which recover identity from the rollout file a live `codex`
//!   process holds open rather than from a sessions directory.
//!
//! Every lookup here is best-effort and time-budgeted: an absent field means
//! "unknown", never a sentinel. A capture client that cannot attribute a
//! request still emits a well-formed envelope (see [`crate::envelope`]) — it
//! just marks the harness `unknown`.

pub mod claude_session;
pub mod codex_process;
pub mod codex_session;
pub mod codex_watcher;
pub mod fork_parent;
pub mod peer_pid;
pub mod watcher;

pub use claude_session::{ClaudeSessionFile, default_sessions_dir};
pub use codex_process::open_jsonl_sessions_by_pid;
pub use codex_session::CodexSessionFile;
pub use codex_watcher::{
    CodexWatcherSnapshot, Snapshot as CodexWatcherSnapshotHandle, spawn as spawn_codex_watcher,
};
pub use peer_pid::{PeerPidLookup, lookup as peer_pid_lookup};
pub use watcher::{Snapshot as WatcherSnapshotHandle, WatcherSnapshot, spawn as spawn_watcher};

/// Attribution facts discovered for a captured harness session.
///
/// Fields are optional because discovery is best-effort and time-budgeted; an
/// absent field means "unknown", never a sentinel.
///
/// This is the harness-agnostic summary a capture client carries once the
/// per-harness lookups above have run. `auth_subject` has no equivalent in the
/// per-harness session files: standalone clients default it to
/// `local:<os-username>` and allow an override (agents and CI set e.g.
/// `gardener-ci`), while on the platform the cloud edge stamps it from
/// validated JWT claims. Nothing parses the prefix — it is an opaque
/// attribution string, the same envelope field in both worlds.
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
