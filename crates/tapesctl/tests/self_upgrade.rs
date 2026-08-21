//! Self-upgrade end to end against the real compiled binary.
//!
//! The unit tests in `upgrade::artifact` and `upgrade::http` drive the pipeline
//! piece by piece; this suite is where the REAL `tapesctl` binary upgrades
//! itself. The compiled binary (`CARGO_BIN_EXE_tapesctl`) is copied into a
//! tempdir install layout, a localhost bucket serves the published objects, and
//! `tapesctl upgrade` runs as a child process with the hidden
//! `TAPESCTL_DOWNLOAD_BASE_URL` seam pointed at the bucket. Assertions observe
//! what a user would: the bytes on disk change and the swapped-in binary still
//! answers `tapesctl version`.
//!
//! Two properties keep the suite hermetic and host-safe:
//!
//! * The served artifact is the compiled binary itself with distinguishing
//!   bytes appended (Unix executables ignore trailing data), so the post-verify
//!   sanity probe passes while the swap stays observable on disk.
//! * The `nightly` target is the natural fit: it downloads unconditionally, so
//!   nothing depends on how the dev build's stamped version compares to a fake
//!   "latest".
//!
//! HOME points into a tempdir for every child, so a run can never read or write
//! the developer's real `~/.tapes`, and no request ever leaves localhost.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Env var the production entry reads as the bucket base URL override.
///
/// Duplicated from `upgrade` on purpose: this suite observes the binary's
/// contract from the outside, so a rename in the library must break this string
/// — and this suite — loudly.
const DOWNLOAD_BASE_URL_ENV: &str = "TAPESCTL_DOWNLOAD_BASE_URL";

/// Basename of the staging dotfile the pipeline writes beside the install.
/// Every abort must leave none behind.
const STAGING_FILE_NAME: &str = ".tapesctl.new";

/// Trailing bytes appended to the compiled binary to build the served artifact
/// — present on disk after the swap, absent before it.
const ARTIFACT_MARKER: &[u8] = b"\n#tapesctl-self-upgrade-e2e-marker\n";

/// Absolute path of the compiled `tapesctl` binary under test, as cargo stamps
/// it for integration tests.
fn compiled_tapesctl() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_tapesctl"))
}

/// A tempdir "install": a copy of the real compiled binary named `tapesctl`,
/// executable, alone in its own directory — the shape `InstallLayout` derives
/// every other path from.
struct TestInstall {
    /// Owns the directory for the test's duration.
    dir: tempfile::TempDir,
    /// The installed copy that child processes execute.
    tapesctl: PathBuf,
}

impl TestInstall {
    /// Copy [`compiled_tapesctl`] into a fresh tempdir, mode 0o755.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let tapesctl = dir.path().join("tapesctl");
        fs::copy(compiled_tapesctl(), &tapesctl).unwrap();
        fs::set_permissions(&tapesctl, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, tapesctl }
    }

    /// The staging dotfile's path beside the install.
    fn staging(&self) -> PathBuf {
        self.dir.path().join(STAGING_FILE_NAME)
    }
}

/// The compiled binary's bytes with a distinguishing suffix appended: still
/// executable (trailing data is ignored on Unix), byte-distinct from the
/// installed copy, and honestly probe-able — no stand-ins.
fn distinguishable_artifact() -> Vec<u8> {
    let mut bytes = fs::read(compiled_tapesctl()).unwrap();
    bytes.extend_from_slice(ARTIFACT_MARKER);
    bytes
}

/// The bucket's normalized `<os>/<arch>` directory names for the host running
/// the tests.
fn host_platform() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => panic!("tests only run on published platforms, not {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("tests only run on published architectures, not {other}"),
    };
    (os, arch)
}

/// Lowercase-hex SHA-256 of `bytes`, matching the digest field of the published
/// sha256sum-format objects.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the bucket serves for the `.sha256` sidecar.
enum Sidecar {
    /// The digest of the served bytes — the healthy case.
    Correct,
    /// A syntactically valid digest of something else — a corrupted or
    /// tampered download.
    Wrong,
    /// No object at all: 404, the case that must abort rather than degrade to
    /// an unverified install.
    Missing,
}

/// Serve a release bucket over localhost: `<prefix>/<os>/<arch>/tapesctl` plus
/// its sha256sum-format `.sha256` beside it, for the host's normalized
/// platform.
async fn serve_bucket(prefix: &str, binary: &[u8], sidecar: Sidecar) -> MockServer {
    let (os, arch) = host_platform();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{prefix}/{os}/{arch}/tapesctl")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(binary.to_vec()))
        .mount(&server)
        .await;
    let digest_response = match sidecar {
        Sidecar::Correct => ResponseTemplate::new(200)
            .set_body_string(format!("{}  tapesctl\n", sha256_hex(binary))),
        Sidecar::Wrong => ResponseTemplate::new(200)
            .set_body_string(format!("{}  tapesctl\n", sha256_hex(b"other bytes"))),
        Sidecar::Missing => ResponseTemplate::new(404),
    };
    Mock::given(method("GET"))
        .and(path(format!("/{prefix}/{os}/{arch}/tapesctl.sha256")))
        .respond_with(digest_response)
        .mount(&server)
        .await;
    server
}

/// Run `<tapesctl> <args…>` to completion as a child process with the bucket
/// override set to `base_url` and HOME isolated into `home`.
fn run_tapesctl(tapesctl: &Path, base_url: &str, home: &Path, args: &[&str]) -> Output {
    std::process::Command::new(tapesctl)
        .args(args)
        .env("HOME", home)
        // Linux resolves config under XDG_CONFIG_HOME when set; pin it inside
        // the isolated HOME so no host value leaks through.
        .env("XDG_CONFIG_HOME", home.join(".config"))
        // The cassette cache is the other thing a run can write; keep it in the
        // tempdir too rather than in the developer's real cache directory.
        .env("TAPESCTL_CACHE_DIR", home.join("cache"))
        .env(DOWNLOAD_BASE_URL_ENV, base_url)
        .output()
        .unwrap()
}

/// The blocking child-process waits in the test bodies must not park the same
/// thread the wiremock bucket serves from, hence the multi-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn the_binary_replaces_itself_end_to_end() {
    // Given a tempdir install of the real compiled binary
    let install = TestInstall::new();
    let original = fs::read(&install.tapesctl).unwrap();

    // Given a localhost bucket publishing a byte-distinct nightly artifact with
    // its digest beside it
    let artifact = distinguishable_artifact();
    assert_ne!(
        artifact, original,
        "the served artifact must be distinguishable from the install"
    );
    let server = serve_bucket("nightly", &artifact, Sidecar::Correct).await;
    let home = tempfile::tempdir().unwrap();

    // When `tapesctl upgrade --nightly` runs as a child pointed at the bucket
    let output = run_tapesctl(
        &install.tapesctl,
        &server.uri(),
        home.path(),
        &["upgrade", "--nightly"],
    );

    // Then it exits 0 and says what it did
    assert!(
        output.status.success(),
        "upgrade --nightly failed ({:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("upgraded"), "stdout: {stdout}");

    // Then the on-disk binary is exactly the served artifact
    let swapped = fs::read(&install.tapesctl).unwrap();
    assert_ne!(swapped, original, "binary on disk was not replaced");
    assert_eq!(
        swapped, artifact,
        "swapped binary must be exactly the served artifact"
    );

    // Then the swap kept it executable, and left no staging file behind
    let mode = fs::metadata(&install.tapesctl)
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "swapped binary must stay executable");
    assert!(!install.staging().exists(), "staging file should be gone");

    // Then the machine is left with a working tool — the whole point of the
    // crash-safe pipeline
    let version = run_tapesctl(&install.tapesctl, &server.uri(), home.path(), &["version"]);
    assert!(
        version.status.success(),
        "swapped binary failed `tapesctl version`\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr),
    );
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("tapesctl"),
        "swapped binary printed no version"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checksum_mismatch_leaves_the_install_untouched() {
    // Given an install, and a bucket whose sidecar disagrees with the bytes it
    // serves — a corrupted or tampered download
    let install = TestInstall::new();
    let original = fs::read(&install.tapesctl).unwrap();
    let server = serve_bucket("nightly", &distinguishable_artifact(), Sidecar::Wrong).await;
    let home = tempfile::tempdir().unwrap();

    // When the upgrade runs
    let output = run_tapesctl(
        &install.tapesctl,
        &server.uri(),
        home.path(),
        &["upgrade", "--nightly"],
    );

    // Then it fails, names the mismatch, and the installed bytes are identical
    assert!(!output.status.success(), "a bad digest must not succeed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sha256 mismatch"), "stderr: {stderr}");
    assert_eq!(
        fs::read(&install.tapesctl).unwrap(),
        original,
        "the installed binary must be byte-identical after an abort"
    );
    // And the unverified bytes are not left lying around next to it.
    assert!(
        !install.staging().exists(),
        "the staging file must be cleaned up on abort"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_checksum_aborts_rather_than_installing_unverified() {
    // Given a bucket serving a binary with no `.sha256` beside it
    let install = TestInstall::new();
    let original = fs::read(&install.tapesctl).unwrap();
    let server = serve_bucket("nightly", &distinguishable_artifact(), Sidecar::Missing).await;
    let home = tempfile::tempdir().unwrap();

    // When the upgrade runs
    let output = run_tapesctl(
        &install.tapesctl,
        &server.uri(),
        home.path(),
        &["upgrade", "--nightly"],
    );

    // Then it refuses — there is no unverified-download fallback — and the
    // install is untouched
    assert!(
        !output.status.success(),
        "a missing checksum must not succeed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unverifiable"), "stderr: {stderr}");
    assert_eq!(fs::read(&install.tapesctl).unwrap(), original);
    assert!(!install.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_install_already_at_the_published_version_downloads_nothing() {
    // Given a bucket whose `latest/version` names exactly what this build
    // reports. The binary under test is a dev build, so the object is written
    // to match whatever it says it is — the property under test is the
    // comparison, not any particular version string.
    let install = TestInstall::new();
    let original = fs::read(&install.tapesctl).unwrap();
    let home = tempfile::tempdir().unwrap();

    let server = MockServer::start().await;
    let reported = {
        let out = run_tapesctl(&install.tapesctl, &server.uri(), home.path(), &["version"]);
        let stdout = String::from_utf8(out.stdout).unwrap();
        // First line is `tapesctl <version>`; the version is the second token.
        stdout
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap()
            .to_owned()
    };
    Mock::given(method("GET"))
        .and(path("/latest/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{reported}\n")))
        .mount(&server)
        .await;
    // Deliberately no artifact mounts: reaching for one would 404, so this test
    // fails loudly if the short-circuit ever stops short-circuiting.

    // When a bare `tapesctl upgrade` runs
    let output = run_tapesctl(&install.tapesctl, &server.uri(), home.path(), &["upgrade"]);

    // Then it exits 0, says so, and touches nothing
    assert!(
        output.status.success(),
        "already-current must exit 0\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("already up to date"), "stdout: {stdout}");
    assert_eq!(fs::read(&install.tapesctl).unwrap(), original);
    assert!(!install.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_staging_file_from_a_crashed_run_is_swept() {
    // Given debris beside the install: a partial download a previous crashed
    // run left behind. Repeated failures have to converge, not accumulate.
    let install = TestInstall::new();
    fs::write(install.staging(), b"half a download from last time").unwrap();
    let artifact = distinguishable_artifact();
    let server = serve_bucket("nightly", &artifact, Sidecar::Correct).await;
    let home = tempfile::tempdir().unwrap();

    // When an upgrade runs
    let output = run_tapesctl(
        &install.tapesctl,
        &server.uri(),
        home.path(),
        &["upgrade", "--nightly"],
    );

    // Then it succeeds — the stale file did not get in the way of the fresh
    // download — and nothing is left over
    assert!(
        output.status.success(),
        "stale staging file blocked the upgrade\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&install.tapesctl).unwrap(), artifact);
    assert!(!install.staging().exists());
}

/// A stand-in artifact that reports `version` as `label`.
///
/// The compiled binary under test reports whatever *it* was stamped with, so a
/// pinned-version upgrade — where the point is that the artifact's version and
/// the requested one must agree — cannot be exercised with a copy of it. The
/// probe runs `<staged> version` and reads stdout, which a script satisfies
/// honestly.
fn artifact_reporting(label: &str) -> Vec<u8> {
    format!("#!/bin/sh\n[ \"$1\" = version ] && {{ echo 'tapesctl {label}'; exit 0; }}\nexit 2\n")
        .into_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_version_installs_that_exact_release() {
    // The half of the target surface no other end-to-end test covers: an
    // explicit `--version`, resolved without a `latest/version` lookup, whose
    // artifact must satisfy the probed-version check to land.
    let install = TestInstall::new();
    let artifact = artifact_reporting("v9.9.9+abc1234");
    let server = serve_bucket("v9.9.9", &artifact, Sidecar::Correct).await;
    let home = tempfile::tempdir().unwrap();

    let output = run_tapesctl(
        &install.tapesctl,
        &server.uri(),
        home.path(),
        &["upgrade", "--version", "v9.9.9"],
    );

    assert!(
        output.status.success(),
        "pinned upgrade failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("9.9.9"), "stdout: {stdout}");
    assert_eq!(fs::read(&install.tapesctl).unwrap(), artifact);
    assert!(!install.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prefix_serving_the_wrong_build_aborts_before_the_swap() {
    // The failure the digest structurally cannot see: the bytes are exactly
    // what the bucket published and hash correctly, but the bucket published
    // the wrong build behind this prefix — a stale `latest/`, a mispublished
    // tag. Only the probed-version check catches it, and until now nothing
    // exercised that check end to end.
    let install = TestInstall::new();
    let original = fs::read(&install.tapesctl).unwrap();
    let artifact = artifact_reporting("v0.1.0+0000000");
    let server = serve_bucket("v9.9.9", &artifact, Sidecar::Correct).await;
    let home = tempfile::tempdir().unwrap();

    let output = run_tapesctl(
        &install.tapesctl,
        &server.uri(),
        home.path(),
        &["upgrade", "--version", "v9.9.9"],
    );

    assert!(
        !output.status.success(),
        "a mispublished prefix must not land"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("reports version"), "stderr: {stderr}");
    assert_eq!(
        fs::read(&install.tapesctl).unwrap(),
        original,
        "the installed binary must survive a probe-version mismatch"
    );
    assert!(!install.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_upgrade_refuses_while_the_first_holds_the_lock() {
    // Two overlapping upgrades sharing one staging path must never interleave
    // — the loser refuses up front, before any network I/O, with the binary
    // untouched. The lock here is held exactly the way a concurrent run would
    // hold it: an exclusive flock on the lock file in the install directory.
    let outer = tempfile::tempdir().unwrap();
    let bin_dir = outer.path().join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let installed = bin_dir.join("tapesctl");
    fs::copy(compiled_tapesctl(), &installed).unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::read(&installed).unwrap();

    let lock_path = bin_dir.join(".tapesctl.upgrade.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
    let rc = unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    assert_eq!(rc, 0, "test could not take the lock it means to hold");

    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();
    let output = run_tapesctl(
        &installed,
        &server.uri(),
        home.path(),
        &["upgrade", "--nightly"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "stderr: {stderr}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "the refusal must happen before any network I/O"
    );
    assert_eq!(fs::read(&installed).unwrap(), before, "binary untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unwritable_install_directory_refuses_before_any_download() {
    // Given the unmigrated root-owned shape: a directory the invoking user
    // cannot write. The bucket is mounted with NO routes at all, so any network
    // request the pipeline made would fail differently than the refusal we
    // expect — the assertion below is what proves the refusal came first.
    let outer = tempfile::tempdir().unwrap();
    let bin_dir = outer.path().join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let installed = bin_dir.join("tapesctl");
    fs::copy(compiled_tapesctl(), &installed).unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&bin_dir, fs::Permissions::from_mode(0o555)).unwrap();

    // Root writes through 0o555 (CAP_DAC_OVERRIDE), so the unwritable
    // precondition cannot be constructed from mode bits alone. Skip
    // rather than assert a refusal the kernel will never produce —
    // containerized CI runs as root.
    if fs::write(bin_dir.join(".root-probe"), b"").is_ok() {
        let _ = fs::remove_file(bin_dir.join(".root-probe"));
        return;
    }

    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();

    // When an upgrade runs
    let output = run_tapesctl(
        &installed,
        &server.uri(),
        home.path(),
        &["upgrade", "--nightly"],
    );

    // Then it refuses with the installer named as the way out, and never
    // escalates
    fs::set_permissions(&bin_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not writable"), "stderr: {stderr}");
    assert!(
        stderr.contains("download.tapes.dev/tapesctl/install"),
        "the refusal must name the installer: {stderr}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "the refusal must happen before any network I/O"
    );
}
