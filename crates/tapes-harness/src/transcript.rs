//! Transcript tailer.
//!
//! Discovers and packages harness transcripts for the transcript ingest lane
//! (`POST /v1/ingest/transcript`). This crate owns *discovery and packaging*;
//! *delivery, auth, and retry* stay in each client (tapesctl and paperd), which
//! differ in how they authenticate to ingest.
//!
//! Track 1 ports paperd's trigger state machine (30s quiescence / on-exit / 5m
//! periodic) and adds sweep-on-start plus `tapesctl sync` to close paperd's own
//! crash-window gap. Server-side content-hash dedup makes blind re-push safe.

use std::path::PathBuf;

/// A discovered harness transcript ready to package for the transcript lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// Path to the harness's own session/transcript file on disk.
    pub path: PathBuf,
    /// The harness that produced it (e.g. `"claude"`, `"codex"`).
    pub harness: String,
}
