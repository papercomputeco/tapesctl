//! tapesctl's parameterization of the shared cassette reducer.
//!
//! The reducer — an OpenAPI document down to the five things a CLI needs per
//! operation — lives in [`tapes_client::cassettes::spec`]. What stays here is
//! tapesctl's own configuration of it: the flag
//! names this binary's subcommands define themselves, which the reducer must
//! not hand to a cassette parameter. Both generated surfaces — the runtime
//! cassette surface and the vendored core contract (see
//! [`crate::api::contract`]) — reduce through these wrappers, so the two
//! cannot read OpenAPI differently.

use serde_json::Value;
use tapes_client::cassettes::spec::ReducerConfig;
pub use tapes_client::cassettes::spec::{Cassette, Location, Method, Param, Surface};

/// Flag names the generated surface cannot hand to a cassette parameter,
/// because the cassette subcommand defines them itself.
const RESERVED_FLAGS: [&str; 4] = ["api-url", "body", "help", "verbose"];

/// tapesctl's reducer parameterization: exactly the reserved list the
/// in-tree reducer hard-coded before the extraction.
pub(crate) const REDUCER: ReducerConfig<'static> = ReducerConfig {
    reserved_flags: &RESERVED_FLAGS,
};

/// Reduce one cassette's OpenAPI document to its methods, under tapesctl's
/// reserved flags. See [`tapes_client::cassettes::spec::reduce`].
#[must_use]
pub fn reduce(entry_name: &str, description: Option<String>, document: &Value) -> Cassette {
    tapes_client::cassettes::spec::reduce(entry_name, description, document, &REDUCER)
}

/// Reduce any OpenAPI document to its operations, under tapesctl's reserved
/// flags. See [`tapes_client::cassettes::spec::reduce_methods`].
#[must_use]
pub fn reduce_methods(document: &Value) -> Vec<Method> {
    tapes_client::cassettes::spec::reduce_methods(document, &REDUCER)
}
