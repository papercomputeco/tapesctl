//! Crash-safe self-update: download, verify, probe, and atomically swap the
//! running binary.
//!
//! Resolves an [`UpgradeTarget`] against the release bucket, downloads the
//! published binary and its `.sha256` digest into a staging dotfile, verifies
//! the digest, sanity-probes the staged file, and atomically renames it over
//! the running executable. Every path is derived from [`InstallLayout`], so the
//! pipeline always acts on the binary the user is actually running.
//!
//! Invariants the pipeline is built around:
//!
//! * The staging file lives **inside the install directory**
//!   ([`InstallLayout::staging_path`]), never `/tmp`: `rename(2)` is only
//!   atomic within one filesystem, and a cross-device "rename" degrades to the
//!   corruptible copy+delete this design exists to close off.
//! * Nothing executes the staged file before its digest verifies, and the
//!   staged file is only marked executable after verification. The installed
//!   binary is only ever replaced by the atomic rename — no truncate-and-write,
//!   no copy over the live inode.
//! * A missing published `.sha256` object aborts the upgrade. There is no
//!   unverified-download fallback.
//! * Every abort leaves the installed binary byte-identical and the staging
//!   file removed; a fresh run also sweeps any staging file a crashed previous
//!   run left behind, so repeated failures converge.
//! * No escalation, ever. An unwritable install directory is a clean, typed
//!   refusal whose message names the installer as the remediation.
//! * One pipeline at a time: an exclusive `flock(2)` on a lock file in the
//!   install directory serializes concurrent `tapesctl upgrade` runs. Two
//!   runs sharing one staging path would otherwise unlink each other's
//!   verified bytes and could commit a partial download as the live binary.
//! * The probe is a sanity check, not a sandbox. Its containment of the
//!   staged binary is best-effort by design: an adversarial bucket able to
//!   serve hostile *verified* bytes defeats the pipeline at the swap, not at
//!   the probe, so the security boundary is the digest gate — the probe's job
//!   is catching honest mistakes (wrong arch, wrong build) before they are
//!   installed.

pub mod artifact;
mod http;

use std::io::Write;
use std::path::PathBuf;

use snafu::{ResultExt, Snafu};

use crate::install_layout::{EnsureWritableError, InstallLayout, InstallLayoutError};
use crate::uninstall::INSTALL_COMMAND;
use artifact::{
    StagingGuard, acquire_upgrade_lock, artifact_urls, bucket_platform, clean_stale_staging,
    commit_staged_binary, make_executable, probe_staged_binary, target_from_args,
    verify_probed_version, verify_staged_digest,
};
use http::{Resolution, build_client, download_to_staging, fetch_expected_digest, resolve_target};

pub use artifact::{ParseTargetError, UpgradeTarget};

/// Production release bucket, including the `tapesctl` namespace the artifacts
/// live under.
///
/// Layout: `<prefix>/<os>/<arch>/tapesctl` and
/// `<prefix>/<os>/<arch>/tapesctl.sha256`, where `<prefix>` is `latest`,
/// `nightly`, or `v<version>`; `<os>` is `linux` or `darwin`; `<arch>` is the
/// normalized `amd64` or `arm64`. `latest/version` is the plain-text newest
/// release version, published by the release pipeline in the same call as the
/// binaries.
const DOWNLOAD_BASE_URL: &str = "https://download.tapes.dev/tapesctl";

/// Env override for the release bucket base URL. Internal seam, not user
/// surface: it lets the end-to-end test point a real binary at a localhost
/// bucket. Deliberately read from the environment in the production entry
/// ([`run`]) and never a clap flag, so it stays out of help output and out of
/// the supported CLI contract.
pub const DOWNLOAD_BASE_URL_ENV: &str = "TAPESCTL_DOWNLOAD_BASE_URL";

/// Path under a base URL of the plain-text latest-version object.
pub(super) const LATEST_VERSION_PATH: &str = "latest/version";

/// Successful result of an upgrade run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// The installed binary already matches the target; nothing was downloaded
    /// and nothing on disk changed.
    AlreadyCurrent {
        /// The version that is both installed and targeted.
        version: String,
    },
    /// The installed binary was atomically replaced.
    Upgraded {
        /// Version label the binary reported before the swap.
        from: String,
        /// Version label now installed.
        to: String,
    },
}

/// Run `tapesctl upgrade`: resolve the target from the CLI arguments, drive the
/// swap pipeline, and report the outcome.
///
/// The bucket base URL is [`DOWNLOAD_BASE_URL`] unless the hidden
/// [`DOWNLOAD_BASE_URL_ENV`] override is set; the current version is the one
/// this binary was stamped with. Already current → prints "already up to date",
/// exits 0, nothing touched. Upgraded → prints `old → new`.
///
/// Unlike paperctl's, this pipeline ends at the swap: tapesctl runs no daemon
/// and installs no symlink, so there is nothing to re-point or bounce
/// afterwards. A capture already in flight keeps running on its old inode and
/// the next invocation is the new binary.
pub async fn run(version: Option<String>, nightly: bool) -> Result<(), UpgradeCliError> {
    use upgrade_cli_error::*;

    let target = target_from_args(version.as_deref(), nightly).context(InvalidTargetSnafu)?;
    let layout = InstallLayout::from_current_exe().context(ResolveLayoutSnafu)?;
    let base_url =
        std::env::var(DOWNLOAD_BASE_URL_ENV).unwrap_or_else(|_| DOWNLOAD_BASE_URL.to_owned());

    run_reporting(
        &mut std::io::stdout(),
        &layout,
        &base_url,
        crate::build_info::version(),
        &target,
    )
    .await
}

/// Command core with every seam injected: the output writer, the install
/// layout, the bucket base URL, the version the running binary reports, and the
/// parsed target.
pub async fn run_reporting<W>(
    out: &mut W,
    layout: &InstallLayout,
    base_url: &str,
    current_version: &str,
    target: &UpgradeTarget,
) -> Result<(), UpgradeCliError>
where
    W: Write + Send,
{
    use upgrade_cli_error::*;

    match run_at(layout, base_url, current_version, target)
        .await
        .context(PipelineSnafu)?
    {
        UpgradeOutcome::AlreadyCurrent { version } => {
            writeln!(out, "tapesctl {version} is already up to date.").context(WriteSnafu)
        }
        UpgradeOutcome::Upgraded { from, to } => {
            writeln!(out, "tapesctl upgraded: {from} → {to}").context(WriteSnafu)
        }
    }
}

/// Pipeline core with the production seams injected: the install `layout` (a
/// tempdir layout in tests), the bucket `base_url` (wiremock in tests), and
/// `current_version`, the version the running binary reports.
///
/// Step order — every step aborts the run with the installed binary
/// byte-identical and the staging file removed:
///
/// 1. refuse an unwritable install directory (the message names the installer
///    as the fix; no escalation is attempted),
/// 2. take the exclusive pipeline lock — concurrent upgrades sharing one
///    staging path would unlink each other's verified bytes — then sweep any
///    stale staging file a crashed previous run left behind,
/// 3. resolve `target` against the bucket, which may short-circuit to
///    [`UpgradeOutcome::AlreadyCurrent`] without downloading,
/// 4. download the binary to the staging dotfile and fetch its published
///    `.sha256` digest,
/// 5. verify the digest — only then mark the staged file executable,
/// 6. probe the staged file (`<staging> version`) to catch artifacts the digest
///    can't (a correctly-published wrong-OS/arch build), and — for targets with
///    an orderable version — check the probed version against the resolved one
///    (a prefix serving the wrong build),
/// 7. fsync the staged file and atomically rename it over the installed binary.
pub async fn run_at(
    layout: &InstallLayout,
    base_url: &str,
    current_version: &str,
    target: &UpgradeTarget,
) -> Result<UpgradeOutcome, UpgradeError> {
    use upgrade_error::*;

    // The writability refusal happens before any network I/O: an unmigrated
    // root-owned install gets its instructions instantly, without a byte of
    // download it could never apply.
    match layout.ensure_writable() {
        Ok(()) => {}
        Err(EnsureWritableError::NotWritable { dir }) => return NotWritableSnafu { dir }.fail(),
        Err(source) => return Err(source).context(WritableCheckSnafu),
    }
    // One pipeline at a time. Taken before the sweep: the sweep unlinks the
    // shared staging path, which is only safe while this run is its sole
    // owner. The kernel drops the lock however this process ends.
    let _lock = acquire_upgrade_lock(layout)?;
    clean_stale_staging(layout)?;
    // Hoisted above the client: a host the release pipeline does not publish
    // for can never be served, so asking the bucket first would only add a
    // round-trip before the same refusal.
    let platform = bucket_platform()?;

    let client = build_client()?;
    let (prefix, to_version) =
        match resolve_target(&client, base_url, current_version, target).await? {
            Resolution::AlreadyCurrent { version } => {
                return Ok(UpgradeOutcome::AlreadyCurrent { version });
            }
            Resolution::Download { prefix, version } => (prefix, version),
        };

    let (binary_url, digest_url) = artifact_urls(base_url, &prefix, platform);

    let mut guard = StagingGuard::new(layout.staging_path());
    download_to_staging(&client, &binary_url, layout.staging_path()).await?;
    let expected = fetch_expected_digest(&client, &digest_url).await?;
    verify_staged_digest(layout.staging_path(), &expected)?;
    make_executable(layout.staging_path(), layout.tapesctl_path())?;
    let probed = probe_staged_binary(layout.staging_path()).await?;
    // Nightly carries no orderable version to compare the probe against.
    if !matches!(target, UpgradeTarget::Nightly) {
        verify_probed_version(layout.staging_path(), &probed, &to_version)?;
    }
    commit_staged_binary(layout)?;
    guard.disarm();

    Ok(UpgradeOutcome::Upgraded {
        from: current_version.to_owned(),
        to: to_version,
    })
}

/// Failure modes for the `tapesctl upgrade` command entry ([`run`]).
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum UpgradeCliError {
    /// The `--version` argument was not a recognizable target.
    #[snafu(display("invalid upgrade target"))]
    InvalidTarget {
        /// Underlying parse failure.
        source: ParseTargetError,
    },

    /// The install layout could not be resolved from the running executable.
    #[snafu(display("could not resolve the install layout"))]
    ResolveLayout {
        /// Underlying layout-construction failure.
        source: InstallLayoutError,
    },

    /// The swap pipeline failed; the installed binary is untouched.
    #[snafu(display("the upgrade pipeline aborted; the installed binary is untouched"))]
    Pipeline {
        /// Underlying pipeline failure.
        source: UpgradeError,
    },

    /// Could not write progress output.
    #[snafu(display("write failed"))]
    Write {
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

/// Failure modes for the upgrade pipeline ([`run_at`]).
///
/// Every variant means the installed binary is still the pre-upgrade file:
/// nothing destructive happens before the final rename, and the rename is
/// atomic.
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum UpgradeError {
    /// The install directory refuses mutation by the invoking user — the
    /// unmigrated root-owned-install state. The remediation lives in the
    /// message because this refusal *is* the migration path for old installs;
    /// no escalation is ever attempted.
    #[snafu(display(
        "install directory '{}' is not writable; re-run the installer to migrate: {}",
        dir.display(),
        INSTALL_COMMAND
    ))]
    NotWritable {
        /// The unwritable install directory.
        dir: PathBuf,
    },

    /// Probing install-directory writability failed outright (an I/O error
    /// distinct from a clean permission refusal).
    #[snafu(display("could not check whether the install directory is writable"))]
    WritableCheck {
        /// Underlying probe failure.
        source: EnsureWritableError,
    },

    /// The running OS/arch has no published artifacts in the bucket.
    #[snafu(display("no published builds for {os}/{arch}"))]
    UnsupportedPlatform {
        /// The running operating system.
        os: String,
        /// The running architecture.
        arch: String,
    },

    /// The HTTP client could not be constructed (TLS backend init — effectively
    /// unreachable, but never worth a panic).
    #[snafu(display("could not build the HTTP client"))]
    BuildHttpClient {
        /// Underlying reqwest failure.
        source: reqwest::Error,
    },

    /// Removing a stale staging file from a previous crashed run failed.
    #[snafu(display("could not remove stale staging file '{}'", path.display()))]
    CleanStaging {
        /// The staging file being removed.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },

    /// The latest-version lookup request failed at the transport level.
    #[snafu(display("could not fetch the latest published version from '{url}'"))]
    FetchLatestVersion {
        /// The latest-version endpoint URL.
        url: String,
        /// Underlying HTTP failure.
        source: reqwest::Error,
    },

    /// The latest-version lookup answered a non-success status.
    #[snafu(display("latest-version endpoint '{url}' answered {status}"))]
    LatestVersionStatus {
        /// The latest-version endpoint URL.
        url: String,
        /// The HTTP status received.
        status: reqwest::StatusCode,
    },

    /// The latest-version body did not parse as a version.
    #[snafu(display("latest-version endpoint returned unparseable version {value:?}"))]
    MalformedLatestVersion {
        /// The unparseable response body.
        value: String,
    },

    /// Downloading an artifact failed at the transport level.
    #[snafu(display("could not download '{url}'"))]
    Download {
        /// The artifact URL.
        url: String,
        /// Underlying HTTP failure.
        source: reqwest::Error,
    },

    /// An artifact download answered a non-success status.
    #[snafu(display("download of '{url}' answered {status}"))]
    DownloadStatus {
        /// The artifact URL.
        url: String,
        /// The HTTP status received.
        status: reqwest::StatusCode,
    },

    /// The published `.sha256` object is missing. Its absence aborts the
    /// upgrade — an unverifiable binary is never installed or executed.
    #[snafu(display(
        "no published checksum at '{url}' (answered {status}) — refusing to install \
         an unverifiable binary"
    ))]
    MissingChecksum {
        /// The checksum object URL that did not serve a digest.
        url: String,
        /// What it answered instead.
        status: reqwest::StatusCode,
    },

    /// The artifact exceeded the sanity ceiling mid-transfer.
    #[snafu(display("'{url}' exceeded the {cap}-byte artifact ceiling"))]
    ArtifactTooLarge {
        /// The artifact URL.
        url: String,
        /// The ceiling it exceeded.
        cap: u64,
    },

    /// The `.sha256` object's contents were not a sha256sum-format digest line.
    #[snafu(display("published checksum is malformed: {value:?}"))]
    MalformedChecksum {
        /// The unparseable checksum body.
        value: String,
    },

    /// Writing downloaded bytes to the staging file failed.
    #[snafu(display("could not write staging file '{}'", path.display()))]
    WriteStaging {
        /// The staging file being written.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },

    /// Reading the staged file back for digest verification failed.
    #[snafu(display("could not read staging file '{}'", path.display()))]
    ReadStaging {
        /// The staging file being read.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },

    /// The staged file's digest does not match the published digest. The staged
    /// file was never executed and is removed; the installed binary is
    /// untouched.
    #[snafu(display("sha256 mismatch: expected {expected}, downloaded file hashes to {actual}"))]
    ChecksumMismatch {
        /// The digest the bucket published.
        expected: String,
        /// The digest the downloaded bytes actually hash to.
        actual: String,
    },

    /// Marking the verified staged file executable failed.
    #[snafu(display("could not mark staged file '{}' executable", path.display()))]
    MakeExecutable {
        /// The staged file.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },

    /// The upgrade lock file could not be opened or locked.
    #[snafu(display("could not take the upgrade lock at '{}'", path.display()))]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },

    /// Another upgrade holds the pipeline lock right now.
    #[snafu(display(
        "another tapesctl upgrade is already running (lock '{}' is held); let it finish and retry",
        path.display()
    ))]
    AnotherUpgradeRunning {
        /// The held lock file.
        path: PathBuf,
    },

    /// The staged binary could not be spawned for the sanity probe (the
    /// signature of a wrong-architecture artifact).
    #[snafu(display("staged binary '{}' would not execute", path.display()))]
    ProbeSpawn {
        /// The staged file that failed to exec.
        path: PathBuf,
        /// Underlying spawn failure.
        source: std::io::Error,
    },

    /// The staged binary was spawned but did not finish within the probe
    /// bound. The installed binary is untouched and the staged file removed —
    /// an artifact that will not answer `version` promptly is not one to
    /// install.
    #[snafu(display(
        "staged binary '{}' did not answer `version` within {timeout:?}",
        path.display()
    ))]
    ProbeTimedOut {
        /// The staged file that hung.
        path: PathBuf,
        /// The bound it exceeded.
        timeout: std::time::Duration,
    },

    /// The staged binary ran but did not behave like `tapesctl version`.
    #[snafu(display(
        "staged binary '{}' failed its sanity probe (exit {status:?}): {stderr}",
        path.display()
    ))]
    ProbeFailed {
        /// The staged file that failed the probe.
        path: PathBuf,
        /// The probe process's exit code, when it exited at all.
        status: Option<i32>,
        /// The probe process's captured stderr.
        stderr: String,
    },

    /// The staged binary ran but reported a version other than the one the
    /// pipeline resolved — the artifact behind the prefix is not the build it
    /// claims to be. The installed binary is untouched.
    #[snafu(display(
        "staged binary '{}' reports version {probed:?}, expected {expected}",
        path.display()
    ))]
    ProbeVersionMismatch {
        /// The staged file that mis-reported its version.
        path: PathBuf,
        /// The version the pipeline resolved for the target.
        expected: String,
        /// The probe's actual (trimmed) stdout.
        probed: String,
    },

    /// Flushing the staged file to disk before the swap failed.
    #[snafu(display("could not fsync staging file '{}'", path.display()))]
    Fsync {
        /// The staging file being flushed.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },

    /// The atomic rename over the installed binary failed. The installed binary
    /// is untouched — the rename either happened completely or not at all.
    #[snafu(display("could not rename '{}' over '{}'", from.display(), to.display()))]
    Rename {
        /// The staged file being renamed.
        from: PathBuf,
        /// The installed binary being replaced.
        to: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}
