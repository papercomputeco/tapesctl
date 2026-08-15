//! Commands ported from the Go `tapes` CLI.
//!
//! These exist here so a user of the open client never has to install the
//! operator binary to do ordinary things: pull a session out, fill a fresh
//! server with something to look at, find the turn where something happened.
//!
//! `search` is expected to move again: it is a client-shaped surface over the
//! API rather than core client plumbing, and the plan is for it to be served
//! by a cassette, at which point the discovered `cassettes` surface covers it
//! and this port retires. The `skill` commands already made that move: skills
//! are the skills cassette's product, reached through the discovered
//! `tapesctl cassettes skills <method>` surface, and the local authoring port
//! that predated it has been removed rather than kept as a second
//! implementation.
//!
//! They are *thin* ports on purpose. Where the Go command's behaviour is a
//! contract — the export bundle's bytes, the seed route's request — this
//! reproduces it exactly. Where the behaviour was an artifact — a flag that
//! was parsed and ignored, a flag the server now refuses — it is dropped, and
//! the drop is documented at the site.
//!
//! Coverage here does **not** remove anything from the `tapes` CLI. Retiring the
//! duplicated commands is Track 2's decision, and it should be made with both
//! implementations in hand.

pub mod export;
pub mod search;
pub mod seed;
