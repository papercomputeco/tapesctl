//! What this binary says it is.
//!
//! Three fields — version, commit, build date — assembled from what `build.rs`
//! stamped in. The same three the tapes server prints, in the same order, so
//! one sentence of documentation covers both.
//!
//! # Why none of this comes from the manifest
//!
//! It used to: `--version` printed `CARGO_PKG_VERSION`, which no release has
//! ever bumped, so every binary ever shipped claimed to be the same one. The
//! manifest cannot answer the question in the first place — releases are cut by
//! tagging a commit that has already merged, so at the moment the source is
//! compiled the tag it will be released under does not exist yet in the source.
//! Only the build that produces the artifact knows, which is why the tag
//! arrives as an environment variable at that build and the manifest version
//! stays a placeholder. `Cargo.toml` says `0.0.0` and the test at the bottom of
//! this file keeps it saying that.

use std::sync::LazyLock;

/// What an unstamped build calls itself.
///
/// Deliberately not a plausible release number. A developer build reporting
/// `0.1.0` is worse than one reporting nothing, because it is indistinguishable
/// from an installed release — that confusion is the bug this replaced.
const DEV_VERSION: &str = "0.0.0-dev";

/// Printed for a field nothing supplied. Absent is a fact worth stating; a
/// blank line after a label reads as a rendering failure.
const UNKNOWN: &str = "unknown";

/// How much of the commit rides along in the version string. Enough to identify
/// a build by eye; the full commit is its own field for anyone pasting it into
/// a `git show`.
const SHORT_SHA_LEN: usize = 7;

/// Release tag or channel name of this artifact; empty when unstamped.
const RELEASE_TAG: &str = env!("TAPESCTL_RELEASE_TAG");

/// Commit this artifact was built from; empty when unknown.
const BUILD_SHA: &str = env!("TAPESCTL_BUILD_SHA");

/// When this artifact was built; empty unless a pipeline said so.
const BUILD_DATE: &str = env!("TAPESCTL_BUILD_DATE");

/// This build's version: a name plus the commit that produced it.
///
/// One rule covers every build the project produces, which is what keeps the
/// three of them comparable:
///
/// * a release — `v0.4.0+3f2a1b9`
/// * a nightly — `nightly+3f2a1b9`
/// * a laptop  — `0.0.0-dev+3f2a1b9`
///
/// The commit is semver build metadata, so a release still compares equal to
/// its tag under any tool that follows the specification, while a moving name
/// like `nightly` — which identifies no particular build on its own — is still
/// pinned to one commit.
#[must_use]
pub fn version() -> &'static str {
    static VERSION: LazyLock<String> = LazyLock::new(|| version_string(RELEASE_TAG, BUILD_SHA));
    &VERSION
}

/// The full build identity, as `--version` and `tapesctl version` print it.
///
/// The first line is the version alone, because clap prints this after the
/// binary name: `tapesctl v0.4.0+3f2a1b9`, then the remaining fields.
#[must_use]
pub fn long_version() -> &'static str {
    static LONG_VERSION: LazyLock<String> =
        LazyLock::new(|| long_version_string(version(), BUILD_SHA, BUILD_DATE));
    &LONG_VERSION
}

/// Compose the version string. Split out from [`version`] because the inputs
/// are fixed at compile time — this is the only seam at which both a stamped
/// and an unstamped build can be exercised by one test run.
fn version_string(tag: &str, sha: &str) -> String {
    let name = if tag.is_empty() { DEV_VERSION } else { tag };

    match short_sha(sha) {
        Some(short) => format!("{name}+{short}"),
        None => name.to_owned(),
    }
}

/// Compose the three-field block. Split out for the same reason as
/// [`version_string`].
fn long_version_string(version: &str, sha: &str, date: &str) -> String {
    format!(
        "{version}\nSha: {}\nBuilt at: {}",
        or_unknown(sha),
        or_unknown(date)
    )
}

/// The leading characters of a commit, by character rather than by byte: a
/// stamped value is whatever the pipeline passed, and slicing an unexpected one
/// mid-character would panic in the middle of `--version`.
fn short_sha(sha: &str) -> Option<String> {
    (!sha.is_empty()).then(|| sha.chars().take(SHORT_SHA_LEN).collect())
}

fn or_unknown(value: &str) -> &str {
    if value.is_empty() { UNKNOWN } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A commit long enough to be shortened, so a test can tell the two forms
    /// apart.
    const SHA: &str = "3f2a1b9c0d4e5f60718293a4b5c6d7e8f9012345";

    #[test]
    fn a_stamped_build_reports_the_tag_it_was_released_under() {
        assert_eq!(version_string("v9.9.9", SHA), "v9.9.9+3f2a1b9");
    }

    #[test]
    fn a_stamped_channel_build_reports_the_channel_and_its_commit() {
        // What the nightly pipeline produces. The name is the same every night,
        // so without the commit it would identify nothing.
        assert_eq!(version_string("nightly", SHA), "nightly+3f2a1b9");
    }

    #[test]
    fn an_unstamped_build_reports_a_development_version() {
        // The property that matters is not the exact string but that it cannot
        // be mistaken for a release: no release is ever cut as `0.0.0`.
        assert_eq!(version_string("", SHA), "0.0.0-dev+3f2a1b9");
    }

    #[test]
    fn a_build_from_no_repository_still_reports_a_version() {
        // A source tarball, or a container that was handed the sources without
        // the `.git` directory. Nothing to shorten, so nothing is appended.
        assert_eq!(version_string("", ""), "0.0.0-dev");
    }

    #[test]
    fn the_stamped_block_carries_all_three_fields() {
        assert_eq!(
            long_version_string("v9.9.9+3f2a1b9", SHA, "2026-08-13T18:22:04Z"),
            format!("v9.9.9+3f2a1b9\nSha: {SHA}\nBuilt at: 2026-08-13T18:22:04Z")
        );
    }

    #[test]
    fn the_unstamped_block_names_what_it_does_not_know() {
        assert_eq!(
            long_version_string("0.0.0-dev", "", ""),
            "0.0.0-dev\nSha: unknown\nBuilt at: unknown"
        );
    }

    #[test]
    fn the_version_leads_the_block() {
        // clap prints this block after the binary name, so anything but the
        // version on the first line would render as `tapesctl Sha: ...`.
        let block = long_version_string("0.0.0-dev", SHA, "");
        assert_eq!(block.lines().next(), Some("0.0.0-dev"), "got: {block}");
    }

    #[test]
    fn this_binary_reports_a_version_of_some_kind() {
        // Whatever this particular build was stamped with, the accessors have
        // to produce something — an empty `--version` would be the original bug
        // wearing different clothes.
        assert!(!version().is_empty());
        assert!(
            long_version().starts_with(version()),
            "got: {}",
            long_version()
        );
    }

    #[test]
    fn the_manifest_version_stays_a_placeholder() {
        // The invariant behind this module: the manifest is not where a release
        // number lives, so it holds one that cannot be mistaken for one. If a
        // future change wants the manifest to carry the release version again,
        // it also owes the release process a step that bumps it — this
        // assertion is the reminder.
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.0.0");
    }
}
