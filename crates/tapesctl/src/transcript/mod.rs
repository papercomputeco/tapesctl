//! The transcript lane — the second capture lane, and the only source of a
//! session's causal skeleton.
//!
//! Wire capture ([`crate::start`]) records every LLM call a harness made. It
//! cannot record which of those calls was a *subagent* of which, because the
//! harness never puts that on the wire — it writes it to disk, in
//! `~/.claude/projects/<cwd-encoded>/<sid>.jsonl` and the session's
//! `subagents/` directory. `POST /v1/ingest/transcript` carries those files, and
//! the deriver reconciles them against the wire lane to rebuild the fork tree
//! the console renders.
//!
//! A client that runs only the wire lane produces sessions that look complete
//! and are not: the calls are all there, but subagent work renders as flat
//! dispatch text instead of nested rows.
//!
//! # The three pieces here
//!
//! * [`client`] — delivery. The HTTP call, the response parsing, the error
//!   shape.
//! * [`tailer`] — the live lane, running for the duration of a
//!   `tapesctl start` session on the shared trigger state machine.
//! * [`sync`] — the backstop, sweeping transcripts that no live tailer saw.
//!
//! Discovery, packaging, and the *decision* to push all come from
//! `tapes_harnesses::transcript`, which paperd also consumes — so the two
//! clients' transcript lanes are the same code reaching the same server, not two
//! implementations of one spec.

pub mod client;
pub mod sync;
pub mod tailer;
