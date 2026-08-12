//! The generated `tapesctl cassettes <name> <method>` surface.
//!
//! A tapes deployment serves *cassettes*: independently built API extensions
//! that core reverse-proxies under `/v1/cassettes/<name>`. This module turns the
//! set a server actually serves into subcommands, so `tapesctl cassettes summary
//! reports` works against a server whose cassettes this binary has never heard
//! of. `paperctl` mounts the same generated surface under the same noun, so the
//! spelling transfers between the two clients.
//!
//! # Generated at runtime, not at build time
//!
//! "Generated" here means *discovered when the process starts*, not *code
//! emitted by a build script*. That is forced by the contract on both ends:
//!
//! - **The cassette set is deployment configuration.** An operator lists
//!   cassette OpenAPI URLs (`cassettes = [...]`, `TAPES_CASSETTES`,
//!   `--cassettes`), and core fetches and admits them at runtime; nothing about
//!   the set is known to core at *its* build time, let alone to a client's.
//!   `tapesctl` ships as a prebuilt binary, so a compiled-in list would be one
//!   deployment's cassettes frozen into every user's install — and the users
//!   most likely to run a custom cassette are exactly the ones a stale list
//!   would fail.
//! - **Discovery is shaped for polling clients.** `/v1/cassettes` references
//!   each OpenAPI document rather than inlining it, and publishes a digest
//!   precisely so a client can decide whether a fetch is worth making. The
//!   per-cassette route answers `If-None-Match` with a 304, and keeps serving a
//!   cached document while the cassette itself is down. None of that machinery
//!   has a purpose if the consumer is a code generator run once.
//! - **Build-time generation would put a live server in the build graph.** This
//!   workspace builds under Nix and cross-compiles four targets through Dagger;
//!   a `cargo build` that must reach a running tapes API to emit its CLI is not
//!   a build that reproduces.
//!
//! It is also the position [`crate::api::client`] already takes for responses —
//! the server owns the shape, and a second hand-maintained copy only drifts.
//! This module applies the same rule to the request side.
//!
//! The machinery itself — discovery decode, the reducer, the surface cache,
//! and command synthesis — lives in the shared `tapes-client` crate, consumed
//! by every capture client, alongside the sealed core contract it shares a
//! transport and an error taxonomy with. The submodules here are tapesctl's
//! parameterization of it: the reserved flag names, the cache's on-disk
//! identity, and the `--tapes-url` decoration and dispatch that make a
//! generated command a tapesctl command.
//!
//! # What the server hands us
//!
//! Core republishes a fetched cassette document onto the paths a client can
//! actually call, so the `paths` in `/v1/cassettes/<name>/openapi.json` are
//! already `/v1/cassettes/<name>/...` and are used verbatim. Operation ids are
//! left bare there (only the merged `/openapi` aggregate namespaces them), which
//! is what makes them usable as method names.
//!
//! # Failure is not fatal
//!
//! Every step here degrades instead of failing: no server configured, an
//! unreachable one, a spec that does not parse — each costs the cassette nouns
//! and nothing else. The hand-written core surface must keep working on a
//! machine that cannot reach any tapes server at all.

pub mod cache;
pub mod command;
pub mod discovery;
pub mod spec;

pub use discovery::{Discovery, DiscoveryEntry};
pub use spec::{Cassette, Location, Method, Param, Surface};
