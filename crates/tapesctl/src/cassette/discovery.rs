//! The `/v1/cassettes` discovery document.
//!
//! The model lives in [`tapes_cassette_client::discovery`] since the PCC-1104
//! split — one tolerant decode shared by every capture client — and is
//! re-exported here so crate-internal paths do not move.

pub use tapes_cassette_client::discovery::{Discovery, DiscoveryEntry, Problem};
