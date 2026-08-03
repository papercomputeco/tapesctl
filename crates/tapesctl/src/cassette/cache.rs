//! On-disk cache of a server's cassette surface.
//!
//! # Why a cache is not optional here
//!
//! The generated nouns *are* the cassette listing, so they have to be present
//! for `tapesctl --help` — which means the surface is needed on essentially
//! every invocation, including the ones that never make a request. Discovering
//! it costs one call to `/v1/cassettes` plus one per cassette; paying that to
//! print a help screen would make the CLI feel broken on a slow link and unusable
//! on a plane.
//!
//! So the surface is cached per server and revalidated on a timer. Inside
//! [`REVALIDATE_AFTER`] nothing touches the network at all. After it, discovery
//! is re-fetched and each spec is revalidated with `If-None-Match` — the route
//! answers a match with a 304 and no body, which is what the `ETag` is there
//! for. A cassette whose document has not changed costs one conditional request
//! and no parsing.
//!
//! The cache is keyed by base URL, because two servers have two different
//! cassette sets and a shared cache would offer one server's nouns for the
//! other's data.
//!
//! # Failure is always survivable
//!
//! Nothing in this module returns an error. An unreadable cache is a cache miss,
//! an unwritable one is a lost optimization, and an unreachable server falls
//! back to whatever is on disk *regardless of age* — a stale surface is far more
//! useful than none when the network is the thing that is broken. Only when
//! there is neither a server nor a cache does the CLI go without cassette nouns,
//! and even then the hand-written core surface is untouched.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::client::{ApiClient, SpecFetch};
use crate::cassette::discovery::Discovery;
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

/// One cassette's cached document and the validator to revalidate it with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSpec {
    /// The `ETag` the document arrived with, when the server sent one.
    #[serde(default)]
    pub etag: Option<String>,
    /// The OpenAPI document, stored verbatim.
    pub document: Value,
}

/// A server's cached surface.
///
/// The raw documents are stored rather than the reduced surface: the reduction
/// is this binary's interpretation, and a newer build that reads more of the
/// document would otherwise keep serving the older build's reading of it until
/// the entry expired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cached {
    /// The server this was discovered from, so a hash collision cannot serve
    /// one server's cassettes for another's.
    pub base: String,
    /// When it was last revalidated, in seconds since the epoch.
    pub revalidated_at: u64,
    /// The discovery document.
    pub discovery: Discovery,
    /// Each cassette's document, keyed by cassette name.
    pub specs: BTreeMap<String, CachedSpec>,
}

impl Cached {
    /// Reduce to the command surface.
    #[must_use]
    pub fn surface(&self) -> Surface {
        let cassettes = self
            .discovery
            .cassettes
            .iter()
            .filter_map(|entry| {
                let cached = self.specs.get(&entry.name)?;
                Some(spec::reduce(
                    &entry.name,
                    entry.description.clone(),
                    &cached.document,
                ))
            })
            .collect();
        Surface { cassettes }
    }

    /// Whether this entry is inside its revalidation window.
    #[must_use]
    pub fn is_fresh(&self, now: u64) -> bool {
        // A `revalidated_at` in the future means the clock moved backwards
        // between runs. Treating that as "fresh forever" would pin a stale
        // surface until the clock caught up, so it counts as expired.
        now >= self.revalidated_at && now - self.revalidated_at < REVALIDATE_AFTER.as_secs()
    }
}

/// Seconds since the epoch, or 0 if the clock is before it.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Where cached surfaces live.
fn cache_dir() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(CACHE_DIR_ENV) {
        if !raw.trim().is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    Some(dirs::cache_dir()?.join("tapesctl").join("cassettes"))
}

/// The cache file for one base URL.
///
/// The URL is both sanitized (so the name is readable when someone looks in the
/// directory) and hashed (so two URLs that sanitize alike cannot share a file).
fn cache_path(base: &str) -> Option<PathBuf> {
    let readable: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let trimmed: String = readable.chars().take(48).collect();
    Some(cache_dir()?.join(format!("{trimmed}-{:016x}.json", fnv1a(base))))
}

/// FNV-1a, written out rather than taken from `DefaultHasher` because this value
/// names a file that outlives the process: `DefaultHasher` makes no stability
/// promise across toolchains, and a hash that moved would silently orphan every
/// cached surface on a compiler upgrade.
fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Read the cached surface for a base URL, if there is a usable one.
#[must_use]
pub fn read(base: &str) -> Option<Cached> {
    let path = cache_path(base)?;
    let raw = std::fs::read(&path).ok()?;
    let cached: Cached = serde_json::from_slice(&raw).ok()?;
    // A file written by a different base URL under the same name is not this
    // server's surface, whatever the hash says.
    (cached.base == base).then_some(cached)
}

/// Write a surface to the cache, best effort.
///
/// Written to a temporary file and renamed, so a process that dies mid-write
/// leaves the previous entry intact rather than a truncated one that every later
/// run has to fail to parse.
pub fn write(cached: &Cached) {
    let Some(path) = cache_path(&cached.base) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        tracing::debug!(%error, "could not create the cassette cache directory");
        return;
    }
    let Ok(encoded) = serde_json::to_vec(cached) else {
        return;
    };

    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&temporary, &encoded) {
        tracing::debug!(%error, "could not write the cassette cache");
        return;
    }
    if let Err(error) = std::fs::rename(&temporary, &path) {
        tracing::debug!(%error, "could not install the cassette cache");
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Get the cassette surface for a server, from cache or from the network.
///
/// Never fails. See the module docs for the degradation ladder.
pub async fn load(client: &ApiClient) -> Surface {
    let base = client.base().to_string();
    let existing = read(&base);

    if let Some(cached) = &existing {
        if cached.is_fresh(now()) {
            return cached.surface();
        }
    }

    match revalidate(client, existing.as_ref()).await {
        Some(fresh) => {
            write(&fresh);
            fresh.surface()
        }
        None => {
            // The server could not be reached or did not answer with a document
            // we understand. Whatever is on disk is better than nothing, however
            // old it is.
            existing.map(|cached| cached.surface()).unwrap_or_default()
        }
    }
}

/// Re-fetch discovery and revalidate each cassette's document against it.
async fn revalidate(client: &ApiClient, existing: Option<&Cached>) -> Option<Cached> {
    let document = match client.list_cassettes().await {
        Ok(document) => document,
        Err(error) => {
            tracing::debug!(%error, "could not reach cassette discovery");
            return None;
        }
    };
    let discovery: Discovery = match serde_json::from_value(document) {
        Ok(discovery) => discovery,
        Err(error) => {
            tracing::debug!(%error, "could not read the cassette discovery document");
            return None;
        }
    };

    for problem in &discovery.problems {
        // An operator's broken cassette URL is otherwise indistinguishable from
        // the cassette not existing, and the user running tapesctl is often the
        // one who can fix it.
        tracing::debug!(
            subject = %problem.subject,
            reason = %problem.reason,
            "the server refused a configured cassette",
        );
    }

    let mut specs: BTreeMap<String, CachedSpec> = BTreeMap::new();
    for entry in &discovery.cassettes {
        if !entry.has_spec() {
            continue;
        }
        let previous = existing.and_then(|cached| cached.specs.get(&entry.name));
        let etag = previous.and_then(|spec| spec.etag.as_deref());

        match client.fetch_cassette_spec(&entry.openapi_path, etag).await {
            Ok(SpecFetch::Unchanged) => {
                if let Some(previous) = previous {
                    specs.insert(entry.name.clone(), previous.clone());
                }
            }
            Ok(SpecFetch::Fetched { document, etag }) => {
                specs.insert(entry.name.clone(), CachedSpec { etag, document });
            }
            Err(error) => {
                // One cassette being down must not cost the others their
                // commands, so keep whatever was cached for it and move on.
                tracing::debug!(
                    cassette = %entry.name,
                    %error,
                    "could not fetch a cassette's OpenAPI document",
                );
                if let Some(previous) = previous {
                    specs.insert(entry.name.clone(), previous.clone());
                }
            }
        }
    }

    Some(Cached {
        base: client.base().to_string(),
        revalidated_at: now(),
        discovery,
        specs,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cassette::discovery::DiscoveryEntry;
    use serde_json::json;

    fn entry(name: &str) -> DiscoveryEntry {
        DiscoveryEntry {
            name: name.to_owned(),
            route_prefix: format!("/v1/cassettes/{name}"),
            openapi_path: format!("/v1/cassettes/{name}/openapi.json"),
            openapi_status: "fresh".to_owned(),
            ..Default::default()
        }
    }

    fn hello_document(name: &str) -> Value {
        json!({"paths": {format!("/v1/cassettes/{name}/hello"): {
            "get": {"operationId": "getHello"}
        }}})
    }

    fn cached(base: &str, name: &str, at: u64) -> Cached {
        Cached {
            base: base.to_owned(),
            revalidated_at: at,
            discovery: Discovery {
                contract_version: "v1".to_owned(),
                cassettes: vec![entry(name)],
                problems: Vec::new(),
            },
            specs: BTreeMap::from([(
                name.to_owned(),
                CachedSpec {
                    etag: Some("\"sha256:abc\"".to_owned()),
                    document: hello_document(name),
                },
            )]),
        }
    }

    #[test]
    fn a_cached_entry_reduces_to_the_generated_surface() {
        let surface = cached("http://a", "hello-world", 0).surface();
        assert_eq!(surface.cassettes.len(), 1);
        assert_eq!(surface.cassettes[0].methods[0].name, "get-hello");
    }

    #[test]
    fn a_cassette_with_no_cached_document_generates_no_noun() {
        // Rather than an empty noun whose every method is missing.
        let mut entry = cached("http://a", "hello-world", 0);
        entry.specs.clear();
        assert!(entry.surface().is_empty());
    }

    #[test]
    fn freshness_expires_after_the_revalidation_window() {
        let entry = cached("http://a", "hello-world", 1_000);
        assert!(entry.is_fresh(1_000));
        assert!(entry.is_fresh(1_000 + REVALIDATE_AFTER.as_secs() - 1));
        assert!(!entry.is_fresh(1_000 + REVALIDATE_AFTER.as_secs()));
    }

    #[test]
    fn a_clock_that_moved_backwards_expires_rather_than_pinning_the_surface() {
        let entry = cached("http://a", "hello-world", 5_000);
        assert!(!entry.is_fresh(1_000));
    }

    #[test]
    fn two_base_urls_get_two_cache_files() {
        // A shared file would offer one server's nouns for another's data.
        let a = cache_path("http://one.example").unwrap();
        let b = cache_path("http://two.example").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn urls_that_sanitize_alike_still_get_different_files() {
        // Every non-alphanumeric becomes `_`, so the readable part collides;
        // the hash is what keeps them apart.
        let a = cache_path("http://a-b.example").unwrap();
        let b = cache_path("http://a.b-example").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_file_name_hash_is_stable_across_builds() {
        // Pinned so a toolchain upgrade cannot silently orphan every cached
        // surface by changing where they are looked up.
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(
            fnv1a("http://127.0.0.1:8081/"),
            fnv1a("http://127.0.0.1:8081/")
        );
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }
}
