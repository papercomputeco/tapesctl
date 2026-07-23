//! The capture envelope.
//!
//! The `X-Tapes-*` request-header contract carries attribution and provenance
//! from a capture transport (tapesctl JIT proxy, paperd, or tapes-extproc) into
//! the tapes ingest server. It is the narrow Rust↔Go waist: metadata, not
//! parsing, and it rarely changes.
//!
//! The authoritative spelling and full field set live in the envelope spec and
//! its shared cross-language fixtures. Until that spec is extracted (Track 1),
//! treat paperd's `headers.rs` (Rust producer) and tapes-extproc's `headers.go`
//! (Go consumer) as the source of truth and port constants from there — do not
//! invent header names here.

/// Common prefix for every capture-envelope request header.
pub const HEADER_PREFIX: &str = "x-tapes-";

/// The capture envelope: attribution + provenance stamped onto every captured
/// turn before it is POSTed to ingest. Fields are added as the spec is ported;
/// ingest is the only component that mints identity, so nothing here hashes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The harness that produced the turn (e.g. `"claude"`, `"codex"`).
    pub harness: Option<String>,
    /// The acting subject (see [`crate::attribution::Attribution::auth_subject`]).
    pub auth_subject: Option<String>,
}
