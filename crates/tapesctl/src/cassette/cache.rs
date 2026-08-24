//! On-disk cache of a server's cassette surface — tapesctl's naming of it.
//!
//! The machinery lives in [`tapes_client::cassettes::cache`]; see that module
//! for why the cache is not optional and for the degradation ladder. What
//! stays here is everything that must not move for an existing install: the
//! `tapesctl/cassettes` directory under the platform cache dir, the
//! [`CACHE_DIR_ENV`] override, the [`REVALIDATE_AFTER`] window, and the
//! base-URL key — so a cache file written before the machinery moved out
//! resolves byte-identically after it.
//!
//! Discovery and spec fetches go through the shared transport, adapted onto
//! the cache's narrow seam by [`tapes_client::Wire`]. That adapter is why
//! there is no fetching code here: the requests, the `If-None-Match`
//! revalidation, and the failure mapping are the same ones every consumer of
//! the crate makes.

use std::time::Duration;

pub use tapes_client::cassettes::cache::{Cached, CachedSpec};
use tapes_client::{CacheConfig, DirectHttp, Wire};

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
    tapes_client::cassettes::cache::read(&config(base))
}

/// Write a surface to the cache, best effort.
pub fn write(cached: &Cached) {
    tapes_client::cassettes::cache::write(&config(&cached.base), cached);
}

/// How long live discovery may run before the cache stands in. ETag
/// revalidation keeps the common case to one cheap request per document,
/// and a refused or unroutable connection fails well inside this — only a
/// black-holed host pays the whole deadline. Hitting it is a signal worth
/// warning about, not routine.
pub const LIVE_DEADLINE: Duration = Duration::from_millis(2500);

pub use tapes_client::cassettes::cache::Provenance;

/// Get the cassette surface for a server, live-first.
///
/// The listing this feeds is how a user validates that a cassette is being
/// vended, so the server is always asked, under [`LIVE_DEADLINE`]; the cache
/// only stands in — labeled through the returned [`Provenance`] — when the
/// server cannot answer.
pub async fn load_live(transport: &DirectHttp) -> (Surface, Provenance) {
    let base = transport.base().to_string();
    tapes_client::cassettes::cache::load_live(
        &Wire::new(transport.clone()),
        &config(&base),
        &spec::REDUCER,
        LIVE_DEADLINE,
    )
    .await
}

/// Get the cassette surface for a server, from cache or from the network.
///
/// Never fails. See [`tapes_client::cassettes::cache`] for the degradation
/// ladder.
pub async fn load(transport: &DirectHttp) -> Surface {
    let base = transport.base().to_string();
    tapes_client::cassettes::cache::load(
        &Wire::new(transport.clone()),
        &config(&base),
        &spec::REDUCER,
    )
    .await
}
