//! Stamps the build identity that `tapesctl version` reports.
//!
//! Three values reach the crate as environment variables: the release tag, the
//! commit, and the build date. A release pipeline injects all three; a laptop
//! injects none and this script fills the commit in from git. Nothing here
//! invents the other two — a build that was not cut by a release pipeline has
//! no release tag and no build date, and says so.
//!
//! # Why a build script rather than a plain `option_env!`
//!
//! `option_env!` alone reads the environment of whichever compilation happened
//! to produce the current artifact, and cargo will not recompile because an
//! environment variable changed unless something asks it to. Injecting a tag
//! into a tree that had already been built would then produce a binary
//! carrying the *previous* build's identity, which is the failure this whole
//! change exists to remove. The `rerun-if-env-changed` lines below are what
//! make an injected value take effect.

use std::path::Path;
use std::process::Command;

/// Release tag, or channel name, of the artifact being built (`v0.4.0`,
/// `nightly`). Empty for a build nobody cut.
const RELEASE_TAG: &str = "TAPESCTL_RELEASE_TAG";

/// Full commit the artifact was built from. Falls back to git.
const BUILD_SHA: &str = "TAPESCTL_BUILD_SHA";

/// RFC 3339 timestamp of the build. Injected or absent: a laptop's build date
/// would be the date this script last *ran*, which drifts from the date the
/// binary was built and so would be a field that quietly lies.
const BUILD_DATE: &str = "TAPESCTL_BUILD_DATE";

fn main() {
    for variable in [RELEASE_TAG, BUILD_SHA, BUILD_DATE] {
        println!("cargo::rerun-if-env-changed={variable}");
    }
    println!("cargo::rerun-if-changed=build.rs");

    // Injection wins over git: in a release container the source arrives
    // without a `.git` directory, and where both exist the pipeline's answer is
    // the one describing the artifact.
    let sha = injected(BUILD_SHA).or_else(git_head_sha);

    emit(RELEASE_TAG, injected(RELEASE_TAG));
    emit(BUILD_SHA, sha);
    emit(BUILD_DATE, injected(BUILD_DATE));
}

/// An injected value, treating unset and empty as the same thing.
///
/// Empty matters: a workflow that interpolates a missing input passes `""`
/// rather than omitting the variable, and an empty tag must mean "unstamped",
/// not "the release named the empty string".
fn injected(variable: &str) -> Option<String> {
    let value = std::env::var(variable).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Hand a value to the crate under the same name the pipeline used, so the
/// variable a workflow sets and the variable the source reads are one name.
fn emit(variable: &str, value: Option<String>) {
    println!("cargo::rustc-env={variable}={}", value.unwrap_or_default());
}

/// The commit of the working tree being built, when there is one.
fn git_head_sha() -> Option<String> {
    let sha = git(&["rev-parse", "HEAD"])?;
    watch_head();
    Some(sha)
}

/// Ask cargo to rerun this script when HEAD moves.
///
/// Necessary because the `rerun-if` lines above replace cargo's default of
/// rescanning the package directory: without these, a commit or a branch
/// switch would leave the previous commit stamped into every later build.
///
/// Only paths that exist are watched. A `rerun-if-changed` naming a missing
/// file reads as "changed" on every check, which would rerun the script — and
/// so relink the crate — on every single build.
fn watch_head() {
    watch(git(&["rev-parse", "--git-path", "HEAD"]));

    // The ref HEAD points at, resolved through git so this holds in a worktree,
    // where HEAD is per-worktree but refs live in the common directory.
    if let Some(reference) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        watch(git(&["rev-parse", "--git-path", &reference]));
    }

    // Where that ref lives once it has been packed away.
    watch(git(&["rev-parse", "--git-path", "packed-refs"]));
}

fn watch(path: Option<String>) {
    if let Some(path) = path {
        if Path::new(&path).exists() {
            println!("cargo::rerun-if-changed={path}");
        }
    }
}

/// One git invocation, where any failure — no git, no repository, no output —
/// is the same answer: nothing to stamp.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}
