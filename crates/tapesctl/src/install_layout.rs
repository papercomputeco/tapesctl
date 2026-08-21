//! Resolves the install layout from the running executable's real location.
//!
//! [`InstallLayout`] canonicalizes `std::env::current_exe()` so a PATH
//! invocation, an absolute one, and one through a symlink all collapse to the
//! directory the real binary lives in. `upgrade` and `uninstall` both derive
//! every install path from this one type, so the two commands can never
//! disagree about where "the install" is — and no runtime code needs to
//! hardcode an install directory.

use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt, Snafu};

/// Basename of the staged replacement binary written by `tapesctl upgrade`.
///
/// Dotted so a crashed run cannot leave a PATH-visible half-binary behind.
const STAGING_FILE_NAME: &str = ".tapesctl.new";

/// Every install path upgrade and uninstall touch, derived from one
/// canonicalized executable path.
///
/// Constructed, never assumed: the paths describe the directory the running
/// binary actually resides in — not a guessed install prefix — so operations
/// act on the binary the user is really running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLayout {
    /// Directory containing the canonicalized binary.
    install_dir: PathBuf,
    /// The canonicalized `tapesctl` binary itself.
    tapesctl_path: PathBuf,
    /// Staging file for upgrade downloads. Lives in `install_dir` so the final
    /// rename over `tapesctl_path` stays on a single filesystem.
    staging_path: PathBuf,
}

impl InstallLayout {
    /// Resolve the layout from `std::env::current_exe()`.
    pub fn from_current_exe() -> Result<Self, InstallLayoutError> {
        use install_layout_error::*;

        let exe = std::env::current_exe().context(CurrentExeSnafu)?;
        Self::from_exe_path(exe)
    }

    /// Resolve the layout from an explicit executable path.
    ///
    /// Canonicalizes `exe` (following symlinks, so an invocation through one
    /// lands on the real file) and derives every other path from the result.
    /// Test-friendly entry point: production code goes through
    /// [`InstallLayout::from_current_exe`].
    pub fn from_exe_path(exe: impl AsRef<Path>) -> Result<Self, InstallLayoutError> {
        use install_layout_error::*;

        let exe = exe.as_ref();
        let tapesctl_path = exe
            .canonicalize()
            .context(CanonicalizeSnafu { path: exe })?;
        let install_dir =
            tapesctl_path
                .parent()
                .map(Path::to_path_buf)
                .context(NoInstallDirSnafu {
                    path: &tapesctl_path,
                })?;
        Ok(Self {
            staging_path: install_dir.join(STAGING_FILE_NAME),
            tapesctl_path,
            install_dir,
        })
    }

    /// Directory containing the real binary.
    #[must_use]
    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    /// The canonicalized `tapesctl` binary path.
    #[must_use]
    pub fn tapesctl_path(&self) -> &Path {
        &self.tapesctl_path
    }

    /// The upgrade staging file (`.tapesctl.new`) inside the install directory.
    #[must_use]
    pub fn staging_path(&self) -> &Path {
        &self.staging_path
    }

    /// Probe whether the invoking user can mutate the install directory.
    ///
    /// `unlink(2)` and `rename(2)` require write permission on the *containing
    /// directory*, so this one check gates both upgrade's swap and uninstall's
    /// binary removal. No escalation is ever attempted; on failure the caller
    /// prints remediation (the installer command) and exits nonzero.
    pub fn ensure_writable(&self) -> Result<(), EnsureWritableError> {
        use ensure_writable_error::*;

        // Creating (and immediately dropping, which deletes) a uniquely named
        // file exercises exactly the directory-write permission that unlink and
        // rename need — unlike a mode-bit inspection, it also respects ACLs and
        // read-only mounts.
        match tempfile::Builder::new()
            .prefix(".tapesctl-writable-probe.")
            .tempfile_in(&self.install_dir)
        {
            Ok(_probe) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                NotWritableSnafu {
                    dir: &self.install_dir,
                }
                .fail()
            }
            Err(source) => Err(source).context(ProbeSnafu {
                dir: &self.install_dir,
            }),
        }
    }
}

/// Failure modes for constructing an [`InstallLayout`].
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum InstallLayoutError {
    /// `std::env::current_exe()` itself failed (exe deleted mid-run, procfs
    /// unavailable).
    #[snafu(display("could not resolve the current executable path"))]
    CurrentExe {
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// Canonicalizing the executable path failed (dangling symlink, unreadable
    /// path component).
    #[snafu(display("could not canonicalize executable path '{}'", path.display()))]
    Canonicalize {
        /// The path we tried to canonicalize.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// The canonicalized executable has no containing directory (`/`).
    #[snafu(display("executable path '{}' has no containing directory", path.display()))]
    NoInstallDir {
        /// The canonicalized executable path.
        path: PathBuf,
    },
}

/// Failure modes for [`InstallLayout::ensure_writable`].
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum EnsureWritableError {
    /// The install directory rejects mutation by the invoking user — the
    /// unmigrated root-owned-install state.
    #[snafu(display("install directory '{}' is not writable", dir.display()))]
    NotWritable {
        /// The unwritable install directory.
        dir: PathBuf,
    },
    /// Probing the directory failed outright (an I/O error distinct from a
    /// clean permission refusal).
    #[snafu(display("could not probe install directory '{}'", dir.display()))]
    Probe {
        /// The directory being probed.
        dir: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn install_layout_canonicalizes_a_symlinked_invocation() {
        // Given a real binary with a symlink pointing at it — the shape a
        // shim, or a package manager's bin directory, produces
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("tapesctl");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        let link = dir.path().join("tapesctl-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // When the layout is built from the symlink path
        let layout = InstallLayout::from_exe_path(&link).unwrap();

        // Then every path resolves to the real file's directory
        // (canonicalized, so the tempdir's own symlinkness — /var →
        // /private/var on macOS — collapses too)
        let real_dir = dir.path().canonicalize().unwrap();
        assert_eq!(layout.install_dir(), real_dir);
        assert_eq!(layout.tapesctl_path(), real_dir.join("tapesctl"));
        assert_eq!(layout.staging_path(), real_dir.join(STAGING_FILE_NAME));
    }

    #[test]
    fn the_staging_file_shares_the_install_directory() {
        // The invariant the whole crash-safe swap rests on: `rename(2)` is
        // only atomic within one filesystem, so the staged file must be a
        // sibling of the binary it will replace — never in /tmp.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("tapesctl");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();

        let layout = InstallLayout::from_exe_path(&real).unwrap();

        assert_eq!(layout.staging_path().parent(), Some(layout.install_dir()));
    }

    #[test]
    fn a_writable_directory_passes_the_probe_and_leaves_nothing_behind() {
        // Given an ordinary user-owned install directory
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("tapesctl");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        let layout = InstallLayout::from_exe_path(&real).unwrap();

        // When writability is probed
        assert!(layout.ensure_writable().is_ok());

        // Then the probe file is gone — only the binary remains
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("tapesctl")]);
    }

    #[test]
    fn an_unwritable_directory_refuses_by_name() {
        use std::os::unix::fs::PermissionsExt;

        // Given an install directory the user cannot write — what an
        // unmigrated root-owned install looks like from inside the binary
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("bin");
        std::fs::create_dir(&nested).unwrap();
        let real = nested.join("tapesctl");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        let layout = InstallLayout::from_exe_path(&real).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes through 0o555 (CAP_DAC_OVERRIDE), so the unwritable
        // precondition cannot be constructed from mode bits alone. Skip
        // rather than assert a refusal the kernel will never produce —
        // containerized CI runs as root.
        if std::fs::write(nested.join(".root-probe"), b"").is_ok() {
            let _ = std::fs::remove_file(nested.join(".root-probe"));
            return;
        }

        // When writability is probed
        let result = layout.ensure_writable();

        // Then it is a clean, typed refusal naming the directory — never an
        // escalation attempt
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(result, Err(EnsureWritableError::NotWritable { .. })),
            "got: {result:?}"
        );
    }
}
