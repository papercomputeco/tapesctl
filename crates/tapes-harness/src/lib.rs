//! `tapes-harness` — shared, open-source client-side harness knowledge for
//! Tapes capture.
//!
//! This crate is the single home for everything a capture client needs to know
//! about a coding-agent harness on the client side. It is consumed by both
//! `tapesctl` (open) and paperd (closed), so ingest parity between
//! `tapesctl start` and `paper start` is **structural, not policed** — the same
//! code runs in both.
//!
//! Per the "Tapes and Cassettes" RFC, exactly three places hold harness
//! knowledge; this crate is one of them (the deriver and the envelope
//! spec/fixtures are the other two). It owns four responsibilities:
//!
//! - [`launch`] — per-harness env/config injection to run a harness under a
//!   capture proxy.
//! - [`attribution`] — session-file reads, fork-parent recovery, peer-PID
//!   lookup, and the codex session watcher.
//! - [`transcript`] — discovering and packaging harness transcripts for the
//!   `POST /v1/ingest/transcript` lane.
//! - [`envelope`] — the `X-Tapes-*` header contract that carries attribution
//!   from any capture transport into ingest.
//!
//! [`attribution`] and [`envelope`] are extracted from paperd's
//! `proxy::session::*` and `proxy::headers` — the code that validated peer-PID
//! attribution, fork-parent discovery, and the `X-Tapes-*` producer against
//! real Claude and Codex traffic. The envelope's on-wire behaviour is pinned
//! by the shared cross-language fixture corpus vendored at
//! `vendor/tapes-envelope-fixtures/`, which the Go parsers table-test against
//! too, so producer and parser cannot drift silently.
//!
//! [`launch`] and [`transcript`] are still documented seeds; they arrive later
//! in Track 1 from `start.rs` and the transcript uploader's
//! discovery/packaging half.

pub mod attribution;
pub mod envelope;
pub mod launch;
pub mod transcript;
