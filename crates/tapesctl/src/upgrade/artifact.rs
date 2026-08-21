//! Artifact-facing mechanics for `tapesctl upgrade`: the target type, the
//! bucket's platform/URL layout, and the local file steps — staging, digest
//! verification, probing, and the atomic swap.
//!
//! The pipeline orchestration — step ordering, refusal-before-network, outcome
//! reporting — lives in [`super`], and the network side (client construction,
//! target resolution, object transfer) lives in [`super::http`]; this module
//! owns the rest. Every function upholds the pipeline's abort contract: a
//! failure leaves the installed binary byte-identical, and the staging file's
//! lifetime is owned by the caller's [`StagingGuard`].

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt, Snafu};
use versions::Versioning;

use super::UpgradeError;
use super::http::parse_comparable_version;
use crate::install_layout::InstallLayout;

/// Basename of the released binary object inside `<prefix>/<os>/<arch>/`.
const BINARY_OBJECT: &str = "tapesctl";

/// Suffix appended to [`BINARY_OBJECT`] for its published digest object.
const CHECKSUM_SUFFIX: &str = ".sha256";

/// Bound on retries when exec of the freshly staged binary races an overlay
/// filesystem's post-close writeback (see [`probe_staged_binary`]). Capped so a
/// genuinely un-execable artifact still fails promptly.
const MAX_PROBE_SPAWN_ATTEMPTS: u32 = 50;

/// Delay between [`probe_staged_binary`] spawn retries — 50 × 20ms bounds the
/// transient-ETXTBSY wait at ~1s.
const PROBE_SPAWN_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Wall-clock bound on the sanity probe.
///
/// The probe is the one step that *executes code the bucket supplied*, so it is
/// the one step that must not be allowed to run forever: an artifact that
/// blocks — an infinite loop, a `version` path that dials a black-holed host —
/// would otherwise hang the upgrade with an executable staging file on disk,
/// where a Ctrl-C skips `Drop` and leaves it behind. `version` is a constant
/// print, so anything beyond a couple of seconds is already pathological.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on probe output retained for the version check and error messages.
///
/// `Command::output()` buffers without limit, so a chatty artifact could
/// exhaust memory in the updater. The version line is the first thing printed;
/// 64 KiB is far past any honest `version` output.
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;

/// Which published build `tapesctl upgrade` should install.
///
/// Parse-don't-validate: a pinned version travels as a parsed [`Versioning`],
/// never as a raw string, so downstream code can compare and format it without
/// re-validating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeTarget {
    /// The newest published release (`latest/` prefix). The default when the
    /// user names no target.
    Latest,
    /// The rolling nightly build (`nightly/` prefix). Nightly is not an
    /// orderable version, so this target skips version comparison and always
    /// downloads.
    Nightly,
    /// An explicit release version (`v<version>/` prefix). Downgrades are
    /// allowed — the pipeline is direction-agnostic.
    Pinned(Versioning),
}

impl UpgradeTarget {
    /// Parse a user-supplied target spec.
    ///
    /// `latest` and `nightly` select the corresponding rolling targets;
    /// anything else must parse as a version (a leading `v` is accepted and
    /// stripped) and becomes [`UpgradeTarget::Pinned`]. Non-version strings are
    /// a typed parse error, so an [`UpgradeTarget`] can never hold junk.
    pub fn parse(spec: &str) -> Result<Self, ParseTargetError> {
        use parse_target_error::*;

        let trimmed = spec.trim();
        match trimmed {
            "latest" => return Ok(Self::Latest),
            "nightly" => return Ok(Self::Nightly),
            _ => {}
        }
        // `versions` would hold junk like "not-a-version" as a `Mess`, so
        // require a numeric first component before treating the spec as a
        // version.
        let bare = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let version = Versioning::new(bare)
            .filter(|v| v.nth(0).is_some())
            .context(InvalidSpecSnafu { spec })?;
        Ok(Self::Pinned(version))
    }

    /// The bucket prefix this target downloads from: `latest`, `nightly`, or
    /// `v<version>`.
    ///
    /// The `v` is re-added after parsing, matching how release artifacts are
    /// uploaded — so `--version 0.7.0` and `--version v0.7.0` reach the same
    /// objects.
    pub(super) fn bucket_prefix(&self) -> String {
        match self {
            Self::Latest => "latest".to_owned(),
            Self::Nightly => "nightly".to_owned(),
            Self::Pinned(version) => format!("v{version}"),
        }
    }
}

/// Build the [`UpgradeTarget`] the clap arguments select: `--nightly` →
/// [`UpgradeTarget::Nightly`], `--version <spec>` → the parsed spec, neither →
/// [`UpgradeTarget::Latest`]. clap's `conflicts_with` keeps both from arriving
/// together.
pub(super) fn target_from_args(
    version: Option<&str>,
    nightly: bool,
) -> Result<UpgradeTarget, ParseTargetError> {
    match version {
        Some(spec) => UpgradeTarget::parse(spec),
        None if nightly => Ok(UpgradeTarget::Nightly),
        None => Ok(UpgradeTarget::Latest),
    }
}

/// Failure modes for [`UpgradeTarget::parse`].
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum ParseTargetError {
    /// The spec was neither `latest`, `nightly`, nor a parseable version.
    #[snafu(display("'{spec}' is not 'latest', 'nightly', or a version like 'v0.7.0'"))]
    InvalidSpec {
        /// The offending user input.
        spec: String,
    },
}

/// The bucket's normalized `<os>/<arch>` directory names for the running host
/// (`linux`/`darwin`, `amd64`/`arm64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BucketPlatform {
    /// Bucket OS directory name.
    os: &'static str,
    /// Bucket architecture directory name.
    arch: &'static str,
}

/// Map the running platform onto [`BucketPlatform`], normalizing Rust's
/// `x86_64`/`aarch64` arch names to the bucket's `amd64`/`arm64`. Hosts the
/// release pipeline does not publish for are a typed error.
pub(super) fn bucket_platform() -> Result<BucketPlatform, UpgradeError> {
    use super::upgrade_error::*;

    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => {
            return UnsupportedPlatformSnafu {
                os: other,
                arch: std::env::consts::ARCH,
            }
            .fail();
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        // `os`, not `std::env::consts::OS`: the OS arm above reports the
        // bucket's spelling (`darwin`), so reporting Rust's (`macos`) here
        // would give one host two different names depending on which half was
        // unsupported.
        other => return UnsupportedPlatformSnafu { os, arch: other }.fail(),
    };
    Ok(BucketPlatform { os, arch })
}

/// Build the URL of one published object:
/// `<base_url>/<prefix>/<os>/<arch>/<object>`.
fn artifact_url(base_url: &str, prefix: &str, platform: BucketPlatform, object: &str) -> String {
    format!(
        "{}/{prefix}/{}/{}/{object}",
        base_url.trim_end_matches('/'),
        platform.os,
        platform.arch,
    )
}

/// The `(binary, digest)` URL pair for one published build: the `tapesctl`
/// object and its `.sha256` beside it.
pub(super) fn artifact_urls(
    base_url: &str,
    prefix: &str,
    platform: BucketPlatform,
) -> (String, String) {
    (
        artifact_url(base_url, prefix, platform, BINARY_OBJECT),
        artifact_url(
            base_url,
            prefix,
            platform,
            &format!("{BINARY_OBJECT}{CHECKSUM_SUFFIX}"),
        ),
    )
}

/// Name of the advisory lock file that serializes upgrade pipelines, inside
/// the install directory next to the staging file.
pub(crate) const UPGRADE_LOCK_FILE_NAME: &str = ".tapesctl.upgrade.lock";

/// The upgrade lock file's path for `layout`'s install directory.
pub(crate) fn upgrade_lock_path(layout: &InstallLayout) -> PathBuf {
    layout.install_dir().join(UPGRADE_LOCK_FILE_NAME)
}

/// Held for the lifetime of one upgrade pipeline: an exclusive advisory
/// `flock(2)` on [`UPGRADE_LOCK_FILE_NAME`].
///
/// Two pipelines sharing one staging path must never interleave: each run's
/// stale-staging sweep would unlink the other run's live staging file, and the
/// path-resolved commit could then rename a partial, unverified download over
/// the installed binary while printing success. The kernel releases the lock
/// when the holding process exits — however it exits — so a crashed upgrade
/// can never leave the lock stuck.
///
/// The lock file itself is never unlinked here: removing a lock file another
/// process may be about to open reintroduces the race the lock closes (holder
/// A keeps the old inode while process B creates and locks a fresh one).
/// Uninstall removes it along with the binary.
#[derive(Debug)]
pub(super) struct UpgradeLock {
    _file: std::fs::File,
}

/// Take the exclusive upgrade lock, or refuse because another upgrade holds
/// it. Non-blocking on purpose: the holding run will either finish the job
/// this run was asked to do or leave a state worth looking at — queueing
/// behind it silently helps nobody.
pub(super) fn acquire_upgrade_lock(layout: &InstallLayout) -> Result<UpgradeLock, UpgradeError> {
    use super::upgrade_error::*;
    use snafu::IntoError;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let path = upgrade_lock_path(layout);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .context(LockSnafu { path: &path })?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return AnotherUpgradeRunningSnafu { path }.fail();
        }
        return Err(LockSnafu { path }.into_error(err));
    }
    Ok(UpgradeLock { _file: file })
}

/// Remove a stale staging file left behind by a previous crashed run, so
/// repeated failed upgrades converge instead of accumulating debris. A missing
/// staging file is the normal case and not an error.
pub(super) fn clean_stale_staging(layout: &InstallLayout) -> Result<(), UpgradeError> {
    use super::upgrade_error::*;

    // remove_file, not remove_dir_all: the staging path is a file this pipeline
    // owns. Anything else squatting there (a directory, say) is not ours to
    // destroy recursively — fail instead.
    match std::fs::remove_file(layout.staging_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(source).context(CleanStagingSnafu {
            path: layout.staging_path(),
        }),
    }
}

/// Hash the staged file with SHA-256 and compare against `expected` (lowercase
/// hex). A mismatch is the integrity gate tripping: the staged file is never
/// executed or installed, and the caller's staging guard removes it.
pub(super) fn verify_staged_digest(staging: &Path, expected: &str) -> Result<(), UpgradeError> {
    use super::upgrade_error::*;

    let mut file = std::fs::File::open(staging).context(ReadStagingSnafu { path: staging })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context(ReadStagingSnafu { path: staging })?;
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    snafu::ensure!(
        actual == expected,
        ChecksumMismatchSnafu { expected, actual }
    );
    Ok(())
}

/// Mark the verified staged file executable. Called strictly after
/// [`verify_staged_digest`] succeeds — unverified bytes never gain execute
/// permission.
///
/// The mode is inherited from the binary being replaced, with the execute bits
/// forced on, rather than hardcoded: the rename installs whatever mode the
/// staged file carries, so a fixed `0o755` would silently widen a deliberately
/// private install (`0o700` on a shared host) to world-readable on every
/// upgrade. `0o755` remains the fallback when the current mode cannot be read,
/// which is what a fresh install would have had anyway.
pub(super) fn make_executable(path: &Path, installed: &Path) -> Result<(), UpgradeError> {
    use super::upgrade_error::*;

    let mode = std::fs::metadata(installed)
        .map(|meta| meta.permissions().mode() & 0o7777)
        .map_or(0o755, |mode| mode | 0o100 | (mode & 0o044) >> 2);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .context(MakeExecutableSnafu { path })
}

/// Read a probe pipe to [`MAX_PROBE_OUTPUT_BYTES`], then drain the rest
/// without buffering it.
///
/// The cap is what bounds the updater's memory against a chatty artifact; the
/// drain is what keeps a chatty-but-terminating artifact from blocking on a
/// full pipe the probe stopped reading.
async fn read_capped<R>(pipe: Option<R>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut captured = Vec::new();
    if let Some(mut pipe) = pipe {
        let mut limited = (&mut pipe).take(MAX_PROBE_OUTPUT_BYTES as u64);
        let _ = limited.read_to_end(&mut captured).await;
        let _ = tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await;
    }
    captured
}

/// Run the staged binary with the `version` argument and return its trimmed
/// stdout.
///
/// The digest proves the bytes match what was published; the probe catches what
/// the digest can't — a correctly-published artifact for the wrong OS or
/// architecture, which fails to exec or exits nonzero here instead of after it
/// has replaced the install.
pub(super) async fn probe_staged_binary(path: &Path) -> Result<String, UpgradeError> {
    use super::upgrade_error::*;
    use snafu::IntoError;

    // A file that was written, closed, made executable, and immediately exec'd
    // can transiently fail with ETXTBSY on overlay filesystems: the kernel
    // briefly still counts a writer reference after the descriptor closed.
    // Container/CI layers hit this; a real install onto a normal filesystem
    // never does. Retry the spawn under a bounded delay so the probe is robust
    // either way, while a genuinely un-execable artifact still fails once the
    // bound is exhausted. Only the spawn is retried here; the running probe
    // is bounded separately below.
    let mut attempt = 0;
    let mut child = loop {
        // The probe runs as the leader of its own process group. SIGKILLing
        // the direct child reaches exactly one process; an artifact that
        // forked would leave descendants running unsupervised after the
        // upgrade reported failure and swept the staging file. Group
        // membership is inherited, so the group signal in the timeout arm
        // reaches them too. (A descendant that re-groups itself — setsid, a
        // double fork — is beyond what any parent can contain without
        // OS-level isolation; the group kill is the strongest containment a
        // CLI has. Leading its own group also keeps the probe out of the
        // terminal's foreground group, so a Ctrl-C reaches tapesctl but not
        // the artifact — the cost of a group the parent can kill without
        // killing itself.) `kill_on_drop` stays on as the backstop for paths that
        // drop the child without reaching that arm, e.g. the whole upgrade
        // future being cancelled.
        let spawned = tokio::process::Command::new(path)
            .arg("version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        match spawned {
            Ok(child) => break child,
            Err(source)
                if source.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt + 1 < MAX_PROBE_SPAWN_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(PROBE_SPAWN_RETRY_DELAY).await;
            }
            Err(source) => return Err(ProbeSpawnSnafu { path }.into_error(source)),
        }
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let ran = tokio::time::timeout(PROBE_TIMEOUT, async {
        let (stdout, stderr) = tokio::join!(read_capped(stdout_pipe), read_capped(stderr_pipe));
        (stdout, stderr, child.wait().await)
    })
    .await;
    let (stdout, stderr, status) = match ran {
        Ok((stdout, stderr, Ok(status))) => (stdout, stderr, status),
        // A wait that itself errors is as good as a binary that would not
        // run; it shares the spawn variant rather than growing one for a
        // path no platform is known to take.
        Ok((_, _, Err(source))) => return Err(ProbeSpawnSnafu { path }.into_error(source)),
        Err(_) => {
            // The child is still owned here, unreaped, so its pid — which
            // `process_group(0)` made the group id — cannot have been
            // recycled. Kill the whole group, then reap the direct child so
            // no zombie outlives the probe; SIGKILL cannot be ignored, so
            // the reap completes promptly. A negative pid addresses the
            // process group.
            if let Some(pid) = child.id() {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            return ProbeTimedOutSnafu {
                path,
                timeout: PROBE_TIMEOUT,
            }
            .fail();
        }
    };
    let truncate = |bytes: &[u8]| {
        String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PROBE_OUTPUT_BYTES)])
            .trim()
            .to_owned()
    };
    snafu::ensure!(
        status.success(),
        ProbeFailedSnafu {
            path,
            status: status.code(),
            stderr: truncate(&stderr),
        }
    );
    Ok(truncate(&stdout))
}

/// Check that the probed binary reports the version the pipeline resolved.
///
/// The digest proves the bytes match what the bucket published; this catches
/// what the digest can't — a prefix serving the wrong build (a stale `latest/`,
/// a mispublished pinned version). The probe output is scanned by whitespace
/// token because `tapesctl version` prints a multi-line block —
/// `tapesctl <version>`, the sha, the build date, then the canary — and any
/// token that parses to the same version as `expected` passes. The stamped
/// version carries the commit as build metadata (`v0.7.0+3f2a1b9`), which
/// `versions` ignores for equality, so it matches the bucket's bare `v0.7.0`.
///
/// Callers skip this for Nightly, which carries no orderable version to compare
/// against — and if `expected` itself is unorderable (impossible for today's
/// Latest/Pinned targets, both parse-gated upstream) there is likewise nothing
/// to compare, so the check passes.
pub(super) fn verify_probed_version(
    path: &Path,
    probed: &str,
    expected: &str,
) -> Result<(), UpgradeError> {
    use super::upgrade_error::*;

    let Some(expected_version) = parse_comparable_version(expected) else {
        return Ok(());
    };
    let matched = probed
        .split_whitespace()
        .any(|token| parse_comparable_version(token).is_some_and(|v| v == expected_version));
    snafu::ensure!(
        matched,
        ProbeVersionMismatchSnafu {
            path,
            expected,
            probed: probed.trim(),
        }
    );
    Ok(())
}

/// Flush the staged file to disk, then atomically rename it over the installed
/// binary.
///
/// The fsync-before-rename ordering means a crash at any point leaves either
/// the old binary or the complete new one — never a torn file. A running
/// process keeps executing its old inode through the swap.
pub(super) fn commit_staged_binary(layout: &InstallLayout) -> Result<(), UpgradeError> {
    use super::upgrade_error::*;

    let staging = layout.staging_path();
    let file = std::fs::File::open(staging).context(FsyncSnafu { path: staging })?;
    file.sync_all().context(FsyncSnafu { path: staging })?;
    drop(file);
    std::fs::rename(staging, layout.tapesctl_path()).context(RenameSnafu {
        from: staging,
        to: layout.tapesctl_path(),
    })
}

/// Removes the staging file on drop unless the upgrade committed it.
///
/// Constructed right after the download begins and disarmed only once the
/// staged file has been renamed over the target, so every abort path — early
/// return, `?`, panic — funnels through one cleanup and can never leave a stale
/// `.tapesctl.new` behind. Removal is best-effort: cleanup failure must not
/// mask the error that aborted the upgrade.
#[derive(Debug)]
pub(super) struct StagingGuard {
    /// The staging file to remove on drop.
    path: PathBuf,
    /// Whether drop should still remove the file.
    armed: bool,
}

impl StagingGuard {
    /// Guard the staging file at `path`, armed.
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            armed: true,
        }
    }

    /// Disarm after the staged file has been renamed over the target — the file
    /// no longer exists under the staging name, and the rename's destination
    /// must not be touched.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const PLATFORM: BucketPlatform = BucketPlatform {
        os: "darwin",
        arch: "arm64",
    };

    #[test]
    fn the_upgrade_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tapesctl");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        let layout = InstallLayout::from_exe_path(&exe).unwrap();

        let held = acquire_upgrade_lock(&layout).unwrap();
        let contested = acquire_upgrade_lock(&layout);
        assert!(
            matches!(contested, Err(UpgradeError::AnotherUpgradeRunning { .. })),
            "got: {contested:?}"
        );

        drop(held);
        // Reacquisition can transiently fail under a parallel test run: a
        // sibling test's fork duplicates this process's descriptors between
        // our flock and the close, and the duplicated description holds the
        // lock until that child execs (CLOEXEC then closes it). Bounded
        // retry, because the property under test is eventual release, which
        // that microsecond window does not contradict.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match acquire_upgrade_lock(&layout) {
                Ok(_) => break,
                Err(UpgradeError::AnotherUpgradeRunning { .. })
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(other) => panic!("lock not released after drop: {other:?}"),
            }
        }
    }

    #[test]
    fn the_rolling_targets_parse_by_name() {
        assert_eq!(
            UpgradeTarget::parse("latest").unwrap(),
            UpgradeTarget::Latest
        );
        assert_eq!(
            UpgradeTarget::parse("nightly").unwrap(),
            UpgradeTarget::Nightly
        );
    }

    #[test]
    fn a_version_parses_with_or_without_its_leading_v() {
        // Both spellings must reach the same bucket objects — a user who types
        // the tag as they see it in a release page and one who types the bare
        // number are asking for the same build.
        let with = UpgradeTarget::parse("v0.7.0").unwrap();
        let without = UpgradeTarget::parse("0.7.0").unwrap();
        assert_eq!(with, without);
        assert_eq!(with.bucket_prefix(), "v0.7.0");
    }

    #[test]
    fn junk_is_refused_rather_than_held_as_a_mess() {
        // `versions` would happily keep "not-a-version" as a Mess and let it
        // travel all the way to a 404. The numeric-first-component gate is what
        // turns it into an error the user can read.
        assert!(UpgradeTarget::parse("not-a-version").is_err());
        assert!(UpgradeTarget::parse("").is_err());
    }

    #[test]
    fn the_arguments_select_the_target() {
        assert_eq!(
            target_from_args(None, false).unwrap(),
            UpgradeTarget::Latest
        );
        assert_eq!(
            target_from_args(None, true).unwrap(),
            UpgradeTarget::Nightly
        );
        assert_eq!(
            target_from_args(Some("v0.6.0"), false).unwrap(),
            UpgradeTarget::parse("v0.6.0").unwrap()
        );
    }

    #[test]
    fn artifact_urls_address_the_binary_and_its_digest_sidecar() {
        let (binary, digest) =
            artifact_urls("https://download.tapes.dev/tapesctl", "latest", PLATFORM);
        assert_eq!(
            binary,
            "https://download.tapes.dev/tapesctl/latest/darwin/arm64/tapesctl"
        );
        assert_eq!(format!("{binary}.sha256"), digest);
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_does_not_double_up() {
        let (binary, _) =
            artifact_urls("https://download.tapes.dev/tapesctl/", "nightly", PLATFORM);
        assert_eq!(
            binary,
            "https://download.tapes.dev/tapesctl/nightly/darwin/arm64/tapesctl"
        );
    }

    #[test]
    fn a_digest_mismatch_is_reported_with_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, b"the wrong bytes").unwrap();

        let err = verify_staged_digest(&staged, &"0".repeat(64)).unwrap_err();

        assert!(
            matches!(err, UpgradeError::ChecksumMismatch { .. }),
            "got: {err:?}"
        );
        // Both digests belong in the message: "mismatch" alone leaves the user
        // unable to tell a corrupted download from a mispublished sidecar.
        let rendered = err.to_string();
        assert!(rendered.contains(&"0".repeat(64)), "got: {rendered}");
    }

    #[test]
    fn a_matching_digest_passes() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, b"payload").unwrap();
        let expected: String = Sha256::digest(b"payload")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        assert!(verify_staged_digest(&staged, &expected).is_ok());
    }

    #[test]
    fn the_probed_version_matches_through_its_build_metadata() {
        // What a stamped release actually prints: the version carries the
        // commit as semver build metadata, and the block continues with the
        // sha, the date, and the canary. The bucket publishes the bare tag.
        let probed = "tapesctl v0.7.0+3f2a1b9\nSha: 3f2a1b9c0d\nBuilt at: unknown\n\
                      All in all, just another tape in the stereo";
        assert!(verify_probed_version(Path::new("/staged"), probed, "v0.7.0").is_ok());
    }

    #[test]
    fn a_prefix_serving_the_wrong_build_is_caught() {
        // The failure the digest cannot see: the bytes are exactly what the
        // bucket published, but the bucket published the wrong build behind
        // this prefix.
        let probed = "tapesctl v0.6.0+aaaaaaa";
        let err = verify_probed_version(Path::new("/staged"), probed, "v0.7.0").unwrap_err();
        assert!(
            matches!(err, UpgradeError::ProbeVersionMismatch { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn an_unorderable_expectation_skips_the_comparison() {
        // Nightly never reaches here, but an expectation that cannot be parsed
        // has nothing to compare against — passing is the only honest answer.
        assert!(verify_probed_version(Path::new("/staged"), "anything at all", "nightly").is_ok());
    }

    #[test]
    fn a_stale_staging_file_is_swept_and_a_missing_one_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tapesctl");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let layout = InstallLayout::from_exe_path(&binary).unwrap();

        // Given debris from a previous crashed run
        std::fs::write(layout.staging_path(), b"half a download").unwrap();
        clean_stale_staging(&layout).unwrap();
        assert!(!layout.staging_path().exists());

        // And sweeping again, with nothing there, is not an error — repeated
        // failures have to converge rather than accumulate.
        assert!(clean_stale_staging(&layout).is_ok());
    }

    #[test]
    fn the_staging_guard_cleans_up_unless_disarmed() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join(".tapesctl.new");

        // An aborted run: the guard drops armed and takes the file with it
        std::fs::write(&staged, b"partial").unwrap();
        drop(StagingGuard::new(&staged));
        assert!(!staged.exists(), "an armed guard should remove the file");

        // A committed run: the name now belongs to the rename's destination,
        // so a disarmed guard must not touch it
        std::fs::write(&staged, b"committed").unwrap();
        let mut guard = StagingGuard::new(&staged);
        guard.disarm();
        drop(guard);
        assert!(staged.exists(), "a disarmed guard must leave the file");
    }

    #[test]
    fn committing_replaces_the_binary_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tapesctl");
        std::fs::write(&binary, b"old").unwrap();
        let layout = InstallLayout::from_exe_path(&binary).unwrap();
        std::fs::write(layout.staging_path(), b"new").unwrap();

        commit_staged_binary(&layout).unwrap();

        assert_eq!(std::fs::read(layout.tapesctl_path()).unwrap(), b"new");
        assert!(
            !layout.staging_path().exists(),
            "the staged name is consumed by the rename"
        );
    }

    #[test]
    fn the_swap_keeps_a_private_install_private() {
        // Given a deliberately private install — 0o700 on a shared host
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("tapesctl");
        std::fs::write(&installed, b"old").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o700)).unwrap();
        let staged = dir.path().join(".tapesctl.new");
        std::fs::write(&staged, b"new").unwrap();

        // When the staged replacement is marked executable
        make_executable(&staged, &installed).unwrap();

        // Then it carries the install's mode, not a hardcoded 0o755 — the
        // rename installs this mode, so inventing one would quietly publish a
        // private binary to every user on the box.
        let mode = std::fs::metadata(&staged).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700, "got {mode:o}");
    }

    #[test]
    fn a_readable_install_gains_the_matching_execute_bits() {
        // The ordinary case: 0o644 on disk (a download that was never chmod'd)
        // must come out executable for everyone who can read it.
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("tapesctl");
        std::fs::write(&installed, b"old").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o644)).unwrap();
        let staged = dir.path().join(".tapesctl.new");
        std::fs::write(&staged, b"new").unwrap();

        make_executable(&staged, &installed).unwrap();

        let mode = std::fs::metadata(&staged).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755, "got {mode:o}");
    }

    #[tokio::test]
    async fn a_hanging_artifact_does_not_hang_the_upgrade() {
        // The probe executes bucket-supplied code. An artifact that blocks
        // forever must not park the upgrade with an executable staging file on
        // disk, where a Ctrl-C would skip Drop and leave it behind.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let pid_file = dir.path().join("probe.pid");
        let descendant_file = dir.path().join("descendant.pid");
        std::fs::write(
            &staged,
            format!(
                "#!/bin/sh\necho $$ > \"{}\"\nsleep 300 &\necho $! > \"{}\"\nexec sleep 300\n",
                pid_file.display(),
                descendant_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            probe_staged_binary(&staged),
        )
        .await
        .expect("the probe must return on its own, not via this outer bound")
        .unwrap_err();

        assert!(
            matches!(err, UpgradeError::ProbeTimedOut { .. }),
            "got: {err:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(30));

        // And the bucket-supplied processes must be dead, not merely
        // abandoned: the dropped future only borrowed the child, so nothing
        // dies with the drop — both the direct child and the backgrounded
        // descendant die because the timeout arm SIGKILLs the process group
        // the probe was spawned into. Reaping is asynchronous, so poll until
        // both pids are gone rather than asserting on the first look.
        let read_pid = |file: &std::path::Path, who: &str| -> i32 {
            std::fs::read_to_string(file)
                .unwrap_or_else(|_| panic!("{who} should have started"))
                .trim()
                .parse()
                .unwrap()
        };
        let pid = read_pid(&pid_file, "the probe child");
        let descendant = read_pid(&descendant_file, "the probe child's descendant");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            // Signal 0 probes existence without an external `kill` binary,
            // which a minimal container may not ship. A zombie still counts
            // as existing until it is reaped.
            let alive: Vec<i32> = [pid, descendant]
                .into_iter()
                .filter(|&p| unsafe { libc::kill(p, 0) } == 0)
                .collect();
            if alive.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "probe processes {alive:?} still running after the probe timed out"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn a_chatty_artifact_is_capped_and_drained_not_deadlocked() {
        // Past the cap, output is discarded rather than buffered or left in
        // the pipe: a probe that stopped reading would fill the pipe and
        // deadlock a chatty-but-terminating artifact into the timeout path.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(
            &staged,
            "#!/bin/sh\nyes chatty | head -c 200000\nprintf 'and then some'\n",
        )
        .unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = probe_staged_binary(&staged).await.unwrap();

        assert!(out.len() <= MAX_PROBE_OUTPUT_BYTES, "len: {}", out.len());
        assert!(out.starts_with("chatty"), "unexpected start of output");
    }

    #[tokio::test]
    async fn a_binary_that_will_not_execute_fails_the_probe() {
        // The wrong-architecture signature: correctly published bytes that
        // this host cannot run. Caught before the swap, not after.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, b"\x7fELF not really").unwrap();
        make_executable(&staged, &staged).unwrap();

        let err = probe_staged_binary(&staged).await.unwrap_err();

        assert!(
            matches!(
                err,
                UpgradeError::ProbeSpawn { .. } | UpgradeError::ProbeFailed { .. }
            ),
            "got: {err:?}"
        );
    }
}
