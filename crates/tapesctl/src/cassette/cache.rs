//! On-disk cache of a server's cassette surface — tapesctl's naming of it.
//!
//! The machinery lives in [`tapes_cassette_client::cache`] since the PCC-1104
//! split; see that module for why the cache is not optional and for the
//! degradation ladder. What stays here is everything that must not move for
//! an existing install: the `tapesctl/cassettes` directory under the platform
//! cache dir, the [`CACHE_DIR_ENV`] override, the [`REVALIDATE_AFTER`]
//! window, and the base-URL key — so a cache file written before the split
//! resolves byte-identically after it.
//!
//! Discovery and spec fetches go through [`ApiClient`], which implements the
//! crate's `SpecTransport` on top of the vendored core contract — the same
//! requests the in-tree cache made.

use std::time::Duration;

use tapes_cassette_client::CacheConfig;
pub use tapes_cassette_client::cache::{Cached, CachedSpec};

use crate::api::client::ApiClient;
use crate::cassette::spec::{self, Surface};

/// How long a cached surface is used without asking the server about it.
///
/// Cassette sets change when an operator redeploys, which is rare next to how
/// often a CLI runs. Ten minutes keeps `--help` instant through a working
/// session while still picking up a new cassette without anyone clearing a
/// cache.
pub const REVALIDATE_AFTER: Duration = Duration::from_secs(600);

/// Overrides where the cache lives. Set by tests, and useful for pinning the
/// location in CI.
pub const CACHE_DIR_ENV: &str = "TAPESCTL_CACHE_DIR";

/// tapesctl's cache parameterization for one base URL.
fn config(key: &str) -> CacheConfig<'_> {
    CacheConfig {
        app_dir_name: "tapesctl/cassettes",
        env_override_var: CACHE_DIR_ENV,
        revalidate_after: REVALIDATE_AFTER,
        key,
    }
}

/// Read the cached surface for a base URL, if there is a usable one.
#[must_use]
pub fn read(base: &str) -> Option<Cached> {
    tapes_cassette_client::cache::read(&config(base))
}

/// Write a surface to the cache, best effort.
pub fn write(cached: &Cached) {
    tapes_cassette_client::cache::write(&config(&cached.base), cached);
}

/// Get the cassette surface for a server, from cache or from the network.
///
/// Never fails. See [`tapes_cassette_client::cache`] for the degradation
/// ladder.
pub async fn load(client: &ApiClient) -> Surface {
    let base = client.base().to_string();
    tapes_cassette_client::cache::load(client, &config(&base), &spec::REDUCER).await
}
