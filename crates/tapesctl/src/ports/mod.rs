//! Commands ported from the Go `tapes` CLI.
//!
//! These three exist here so a user of the open client never has to install the
//! operator binary to do ordinary things: pull a session out, fill a fresh
//! server with something to look at, put a skill where an agent will find it.
//!
//! They are *thin* ports on purpose. Where the Go command's behaviour is a
//! contract — the export bundle's bytes, the seed route's request, the skill
//! file's mode — this reproduces it exactly. Where the behaviour was an artifact
//! — a flag that was parsed and ignored, a flag the server now refuses — it is
//! dropped, and the drop is documented at the site.
//!
//! Coverage here does **not** remove anything from the `tapes` CLI. Retiring the
//! duplicated commands is Track 2's decision, and it should be made with both
//! implementations in hand.

pub mod export;
pub mod seed;
pub mod skill;
