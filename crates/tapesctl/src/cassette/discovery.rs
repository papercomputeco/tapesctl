//! The `/v1/cassettes` discovery document.
//!
//! The model lives in [`tapes_client::cassettes::discovery`] — one tolerant
//! decode shared by every capture client — and is re-exported here so
//! crate-internal paths do not move.

pub use tapes_client::cassettes::discovery::{Discovery, DiscoveryEntry, Problem};
