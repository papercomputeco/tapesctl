//! `tapesctl uninstall` — remove the binary, the installer's rc block, and
//! this tool's local state.
//!
//! The installer writes into two places a user did not choose by hand: a
//! directory on their `PATH` and a sentinel block in their shell rc file.
//! Neither should be something only a hand-edit can take back, which is what
//! this command exists for.
//!
//! Destruction is sequenced so an *interruption* leaves a tool that can
//! retry: the self-unlink is dead last — safe on Unix because the running
//! process keeps its inode until it exits. A step that *fails* is a warning
//! naming the leftover path, not an abort: the run continues to the end,
//! trading a guaranteed retry tool for finishing everything that can finish.
//!
//! What is deliberately *not* removed is anything a harness owns: a
//! Codex plugin registration lives in the harness's own config file, and
//! `tapesctl plugin uninstall` is the command that speaks that contract.

use std::io::Write;
use std::path::{Path, PathBuf};

use snafu::{ResultExt, Snafu};

use crate::install_layout::InstallLayout;
use crate::rc_block::{self, RemoveOutcome};

/// The installer command, printed as the remediation when the install
/// directory cannot be mutated. Matches the documented one-liner.
pub const INSTALL_COMMAND: &str = "curl -sSfL https://download.tapes.dev/tapesctl/install | bash";

/// The local state directories uninstall removes, injected rather than
/// resolved.
///
/// `Machine::resolve` refuses to read the real environment under `cfg(test)` —
/// for good reason, see `crate::machine` — so the ambient lookup happens once
/// at the command boundary in [`run`] and every step below takes explicit
/// paths. A `None` means "this machine names no such location", which is a
/// nothing-to-do, not a failure.
#[derive(Debug, Clone, Default)]
pub struct PurgePaths {
    /// `~/.tapes` — the configuration directory.
    pub config_dir: Option<PathBuf>,
    /// The cassette surface cache directory — the one this crate derived, never
    /// an environment-supplied one (see [`crate::cassette::cache::owned_cache_dir`]).
    pub cache_dir: Option<PathBuf>,
    /// A cache location the environment overrode us to. Reported, never
    /// removed: it names a directory the user chose, which may hold anything.
    pub cache_dir_override: Option<PathBuf>,
    /// Shell rc files that may carry the installer's sentinel block.
    pub rc_files: Vec<PathBuf>,
}

/// Run `tapesctl uninstall`, resolving every ambient location first.
pub fn run(assume_yes: bool) -> Result<(), UninstallError> {
    // Optional, not a precondition. `from_current_exe` fails when
    // `current_exe()` errors (no /proc in a minimal container) or when
    // canonicalization does (an unreadable path component; on Linux, a binary
    // whose file was already unlinked). None of that is a reason to leave the
    // config, the cache, and the user's rc block in place — there is simply no
    // binary location to act on, so the rest of the teardown still runs.
    let layout = InstallLayout::from_current_exe().ok();
    let paths = PurgePaths {
        config_dir: crate::machine::Machine::resolve()
            .ok()
            .and_then(|machine| machine.tapes_config_path().parent().map(Path::to_path_buf)),
        cache_dir: crate::cassette::cache::owned_cache_dir(),
        cache_dir_override: crate::cassette::cache::cache_dir_override(),
        rc_files: rc_block::known_rc_files(),
    };
    run_with(
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &mut std::io::stdin().lock(),
        layout.as_ref(),
        &paths,
        assume_yes,
    )
}

/// Command core with every seam injected: the output writer, the confirmation
/// reader, the install layout, and the state paths.
///
/// Step order is deliberate — state first, dotfile next, binary last — so an
/// interruption at any point leaves a working `tapesctl` that can be run again
/// to finish the job.
// Six clears clippy's stock threshold (7) but not the forest dev
// environment's clippy.toml (too-many-arguments-threshold = 5). Every
// parameter is a deliberate test seam — the two output streams are separate
// precisely so a test can assert the prompt is not on stdout — and a struct
// would only rename them.
#[allow(clippy::too_many_arguments)]
pub fn run_with<W, E, R>(
    out: &mut W,
    err: &mut E,
    input: &mut R,
    layout: Option<&InstallLayout>,
    paths: &PurgePaths,
    assume_yes: bool,
) -> Result<(), UninstallError>
where
    W: Write,
    E: Write,
    R: std::io::BufRead,
{
    use uninstall_error::WriteSnafu;

    if !assume_yes && !confirm(err, input, layout, paths)? {
        writeln!(err, "Nothing was removed.").context(WriteSnafu)?;
        return Ok(());
    }

    remove_dir_step(out, paths.config_dir.as_deref(), "configuration")?;
    remove_dir_step(out, paths.cache_dir.as_deref(), "cassette cache")?;
    if let Some(overridden) = paths.cache_dir_override.as_deref() {
        // Named, not removed. The variable points at a directory the user
        // chose; recursively deleting whatever it happens to name is not a
        // liberty an uninstall gets to take.
        writeln!(
            out,
            "Note: {} is set, so a cache may also live at {}.\n\
             Left alone — remove it yourself if you want it gone.",
            crate::cassette::cache::CACHE_DIR_ENV,
            overridden.display()
        )
        .context(WriteSnafu)?;
    }
    remove_rc_blocks_step(out, &paths.rc_files)?;
    writeln!(
        out,
        "\nHarness-side capture plugins are not touched — a plugin\n\
         registration lives in the harness's own config. Remove one with:\n\
         \x20 tapesctl plugin uninstall <harness>"
    )
    .context(WriteSnafu)?;
    match layout {
        Some(layout) => remove_own_binary_step(out, layout)?,
        // No resolvable binary location. Everything else is gone, and saying
        // so is better than a silent partial teardown the user cannot see.
        None => writeln!(
            out,
            "! could not work out where this binary lives, so it was left in \
             place; remove it yourself."
        )
        .context(WriteSnafu)?,
    }
    writeln!(out, "\nAll local tapesctl state removed.").context(WriteSnafu)
}

/// Ask before destroying anything, naming the binary that will go.
///
/// A reader that is at EOF (a pipe, CI) yields no line, which is treated as a
/// decline: an uninstall that proceeds because nobody was there to say no is
/// the one outcome this prompt exists to prevent.
fn confirm<E, R>(
    err: &mut E,
    input: &mut R,
    layout: Option<&InstallLayout>,
    paths: &PurgePaths,
) -> Result<bool, UninstallError>
where
    E: Write,
    R: std::io::BufRead,
{
    use uninstall_error::WriteSnafu;

    // Every target enumerated, because two of them are recursive deletes and
    // "tapesctl's local state" is not a description anyone can check before
    // typing y.
    writeln!(err, "This will remove:").context(WriteSnafu)?;
    for (what, path) in [
        ("binary", layout.map(InstallLayout::tapesctl_path)),
        ("configuration", paths.config_dir.as_deref()),
        ("cassette cache", paths.cache_dir.as_deref()),
    ] {
        if let Some(path) = path {
            writeln!(err, "  {what:<15} {}", path.display()).context(WriteSnafu)?;
        }
    }
    for rc in &paths.rc_files {
        writeln!(
            err,
            "  {:<15} the tapesctl block in {}",
            "PATH block",
            rc.display()
        )
        .context(WriteSnafu)?;
    }
    write!(err, "Continue? [y/N] ").context(WriteSnafu)?;
    err.flush().context(WriteSnafu)?;

    let mut answer = String::new();
    if input.read_line(&mut answer).unwrap_or(0) == 0 {
        writeln!(err).context(WriteSnafu)?;
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Remove one state directory, warn-and-continue.
///
/// A missing directory is silent success — that is the state uninstall is
/// trying to reach, and reporting it as a removal would be a lie.
fn remove_dir_step<W>(out: &mut W, dir: Option<&Path>, what: &str) -> Result<(), UninstallError>
where
    W: Write,
{
    use uninstall_error::WriteSnafu;

    let Some(dir) = dir else {
        return Ok(());
    };
    match std::fs::remove_dir_all(dir) {
        Ok(()) => writeln!(out, "Removed {what} at {}", dir.display()).context(WriteSnafu),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            writeln!(out, "! could not remove {what} at {}: {e}", dir.display()).context(WriteSnafu)
        }
    }
}

/// Remove the installer's sentinel block from every candidate rc file.
///
/// A read-only rc file (nix, home-manager) is a warning line, not an abort: the
/// binary still goes, and the block is inert once the binary it guards on is
/// gone. A malformed block is reported specifically, because leaving it is the
/// deliberate choice — rewriting would drop everything after the orphaned
/// marker.
fn remove_rc_blocks_step<W>(out: &mut W, rc_files: &[PathBuf]) -> Result<(), UninstallError>
where
    W: Write,
{
    use uninstall_error::WriteSnafu;

    for rc_path in rc_files {
        match rc_block::remove_block(rc_path) {
            Ok(RemoveOutcome::Removed) => {
                writeln!(out, "Removed the PATH block from {}", rc_path.display())
                    .context(WriteSnafu)?;
            }
            Ok(RemoveOutcome::NoBlock) => {}
            Ok(RemoveOutcome::Malformed) => {
                writeln!(
                    out,
                    "! {} has a begin marker with no matching end; left it alone \
                     rather than risk dropping what follows it",
                    rc_path.display()
                )
                .context(WriteSnafu)?;
            }
            Err(e) => {
                writeln!(out, "! could not edit {}: {e}", rc_path.display()).context(WriteSnafu)?;
            }
        }
    }
    Ok(())
}

/// Unlink the running `tapesctl` binary itself — the final destructive act.
///
/// When the install directory is not writable (an unmigrated root-owned
/// install), this refuses without escalating and prints both the installer
/// command and the manual `rm`. Reinstalling must not be the only path out.
fn remove_own_binary_step<W>(out: &mut W, layout: &InstallLayout) -> Result<(), UninstallError>
where
    W: Write,
{
    use uninstall_error::WriteSnafu;

    let binary = layout.tapesctl_path();
    // unlink needs write permission on the containing directory, not the file —
    // an unwritable directory means an unmigrated root-owned install, and the
    // fix is the installer, never sudo from inside the binary.
    if let Err(e) = layout.ensure_writable() {
        writeln!(
            out,
            "! could not remove tapesctl at {}: {e}",
            binary.display()
        )
        .context(WriteSnafu)?;
        writeln!(
            out,
            "  Re-run the installer to migrate to a user-owned install: {INSTALL_COMMAND}"
        )
        .context(WriteSnafu)?;
        writeln!(out, "  Or remove the binary yourself:").context(WriteSnafu)?;
        writeln!(out, "    sudo rm {}", binary.display()).context(WriteSnafu)?;
        return Ok(());
    }
    // The upgrade machinery's residue goes with the binary: a staging file an
    // interrupted upgrade left behind (its sweep runs only on the *next*
    // upgrade, and there will not be one) and the pipeline lock file. Both
    // removals are quiet best-effort — neither file existing is the normal
    // case.
    for residue in [
        layout.staging_path().to_path_buf(),
        crate::upgrade::artifact::upgrade_lock_path(layout),
    ] {
        match std::fs::remove_file(&residue) {
            Ok(()) => writeln!(out, "Removed upgrade residue at {}", residue.display())
                .context(WriteSnafu)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => writeln!(out, "! could not remove {}: {e}", residue.display())
                .context(WriteSnafu)?,
        }
    }
    match std::fs::remove_file(binary) {
        Ok(()) => writeln!(out, "Removed tapesctl at {}", binary.display()).context(WriteSnafu),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => writeln!(
            out,
            "! could not remove tapesctl at {}: {e}",
            binary.display()
        )
        .context(WriteSnafu),
    }
}

/// Failure modes for [`run`].
///
/// Deliberately short: every removal step warns and continues, so only failures
/// that make the command impossible to *start* — resolving the layout — or that
/// make its report unreadable are representable here.
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum UninstallError {
    /// The install layout could not be resolved from the running executable.
    #[snafu(display("could not resolve the install layout"))]
    ResolveLayout {
        /// Underlying layout-construction failure.
        source: crate::install_layout::InstallLayoutError,
    },
    /// The output writer rejected our bytes.
    #[snafu(display("could not write uninstall output"))]
    Write {
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::rc_block::{BLOCK_BEGIN, BLOCK_END};

    /// Drive `run_with` with the streams tests care about, returning
    /// `(stdout, stderr)`.
    fn run_capturing(
        layout: Option<&InstallLayout>,
        paths: &PurgePaths,
        input: &str,
        assume_yes: bool,
    ) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = std::io::Cursor::new(input.as_bytes().to_vec());
        run_with(&mut out, &mut err, &mut input, layout, paths, assume_yes).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// A tempdir install: a real binary file plus the layout over it.
    fn layout_in(dir: &Path) -> InstallLayout {
        let binary = dir.join("tapesctl");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        InstallLayout::from_exe_path(&binary).unwrap()
    }

    fn rc_with_block(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("# mine\n{BLOCK_BEGIN}\nexport PATH=x\n{BLOCK_END}\n"),
        )
        .unwrap();
    }

    #[test]
    fn a_full_uninstall_removes_state_the_block_and_the_binary() {
        // Given an install with configuration, a cache, and a shell rc file
        // carrying the installer's block
        let home = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());
        let config_dir = home.path().join(".tapes");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), b"api-url = 'x'").unwrap();
        let cache_dir = home.path().join("cache").join("tapesctl").join("cassettes");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let rc = home.path().join(".zshrc");
        rc_with_block(&rc);
        // Plus the upgrade machinery's residue: the staging and lock files an
        // interrupted upgrade could leave in the install directory.
        let staging = layout.staging_path().to_path_buf();
        let lock = crate::upgrade::artifact::upgrade_lock_path(&layout);
        std::fs::write(&staging, b"partial").unwrap();
        std::fs::write(&lock, b"").unwrap();
        let paths = PurgePaths {
            config_dir: Some(config_dir.clone()),
            cache_dir: Some(cache_dir.clone()),
            cache_dir_override: None,
            rc_files: vec![rc.clone()],
        };

        // When uninstall runs with confirmation skipped
        let (out, _) = run_capturing(Some(&layout), &paths, "", /* assume_yes */ true);

        // Then every artifact is gone, the user's own rc content survives, and
        // the report names the harness-plugin caveat
        assert!(!config_dir.exists(), "config dir should be gone");
        assert!(!cache_dir.exists(), "cache dir should be gone");
        assert!(!layout.tapesctl_path().exists(), "binary should be gone");
        assert!(!staging.exists(), "staging residue should be gone");
        assert!(!lock.exists(), "lock residue should be gone");
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "# mine\n");
        let s = out;
        assert!(s.contains("plugin uninstall"), "stdout: {s}");
        assert!(
            s.contains("All local tapesctl state removed"),
            "stdout: {s}"
        );
    }

    #[test]
    fn declining_the_prompt_removes_nothing() {
        // Given an install and a user who answers no
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".zshrc");
        rc_with_block(&rc);
        let paths = PurgePaths {
            rc_files: vec![rc.clone()],
            ..PurgePaths::default()
        };

        let (_, err) = run_capturing(Some(&layout), &paths, "n\n", false);

        // Then the binary and the block are both still there
        assert!(layout.tapesctl_path().exists());
        assert!(std::fs::read_to_string(&rc).unwrap().contains(BLOCK_BEGIN));
        assert!(err.contains("Nothing was removed"), "stderr: {err}");
    }

    #[test]
    fn no_answer_at_all_is_a_decline() {
        // A pipe with nothing in it — CI, or a `</dev/null` invocation. An
        // uninstall must not proceed because nobody was there to say no.
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());

        let _ = run_capturing(Some(&layout), &PurgePaths::default(), "", false);

        assert!(layout.tapesctl_path().exists(), "binary should survive EOF");
    }

    #[test]
    fn missing_state_is_not_an_error() {
        // Given paths that name locations which do not exist — the ordinary
        // state of a user who never wrote a config
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());
        let gone = install.path().join("nowhere");
        let paths = PurgePaths {
            config_dir: Some(gone.join("config")),
            cache_dir: Some(gone.join("cache")),
            cache_dir_override: None,
            rc_files: vec![gone.join(".zshrc")],
        };

        let (s, _) = run_capturing(Some(&layout), &paths, "", true);

        // Then nothing is reported as removed but the run still completes
        assert!(!s.contains("Removed configuration"), "stdout: {s}");
        assert!(
            s.contains("All local tapesctl state removed"),
            "stdout: {s}"
        );
    }

    #[test]
    fn an_unwritable_install_dir_refuses_and_names_the_installer() {
        use std::os::unix::fs::PermissionsExt;

        // Given the unmigrated root-owned layout: a directory the user cannot
        // write, holding the binary
        let outer = tempfile::tempdir().unwrap();
        let bin_dir = outer.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let layout = layout_in(&bin_dir);
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes through 0o555 (CAP_DAC_OVERRIDE), so the unwritable
        // precondition cannot be constructed from mode bits alone. Skip
        // rather than assert a refusal the kernel will never produce —
        // containerized CI runs as root.
        if std::fs::write(bin_dir.join(".root-probe"), b"").is_ok() {
            let _ = std::fs::remove_file(bin_dir.join(".root-probe"));
            return;
        }

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = run_with(
            &mut out,
            &mut err,
            &mut std::io::empty(),
            Some(&layout),
            &PurgePaths::default(),
            true,
        );

        // Then it refuses without escalating, names both ways out, and the
        // binary survives
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        let s = String::from_utf8(out).unwrap();
        let _ = String::from_utf8(err).unwrap();
        assert!(s.contains(INSTALL_COMMAND), "stdout: {s}");
        assert!(s.contains("sudo rm"), "stdout: {s}");
        assert!(layout.tapesctl_path().exists(), "binary should survive");
    }

    #[test]
    fn an_overridden_cache_location_is_named_but_never_deleted() {
        // Given TAPESCTL_CACHE_DIR pointing somewhere the user cares about.
        // The variable is documented as a way to pin the cache in CI, so it is
        // routinely set to a directory holding more than our cache — and
        // `remove_dir_all` on it would be unrecoverable.
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());
        let precious = tempfile::tempdir().unwrap();
        std::fs::write(precious.path().join("not-ours.txt"), b"keep me").unwrap();
        let paths = PurgePaths {
            cache_dir_override: Some(precious.path().to_path_buf()),
            ..PurgePaths::default()
        };

        let (out, _) = run_capturing(Some(&layout), &paths, "", true);

        // Then it survives untouched, and the report says where it is
        assert!(
            precious.path().join("not-ours.txt").exists(),
            "an overridden cache dir must never be recursively deleted"
        );
        assert!(
            out.contains(crate::cassette::cache::CACHE_DIR_ENV),
            "stdout: {out}"
        );
        assert!(out.contains("Left alone"), "stdout: {out}");
    }

    #[test]
    fn an_unresolvable_binary_location_still_purges_everything_else() {
        // Given no resolvable install layout — `current_exe()` failing in a
        // minimal container, or a canonicalize that cannot traverse. The
        // binary cannot be removed, but that is no reason to strand the
        // config, the cache, and a block in the user's dotfile.
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join(".tapes");
        std::fs::create_dir_all(&config_dir).unwrap();
        let rc = home.path().join(".zshrc");
        rc_with_block(&rc);
        let paths = PurgePaths {
            config_dir: Some(config_dir.clone()),
            rc_files: vec![rc.clone()],
            ..PurgePaths::default()
        };

        let (out, _) = run_capturing(/* layout */ None, &paths, "", true);

        // Then the teardown ran, and the one thing it could not do is stated
        assert!(!config_dir.exists(), "config should still be removed");
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "# mine\n");
        assert!(
            out.contains("could not work out where this binary lives"),
            "stdout: {out}"
        );
    }

    #[test]
    fn the_prompt_goes_to_stderr_and_names_every_target() {
        // The prompt is diagnostic; the report is stdout. Merging them means
        // `tapesctl uninstall | tee log` blocks on stdin with the question
        // invisible. And every recursive-delete target has to be readable
        // before the user types y — "local state" is not something anyone can
        // check.
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());
        let home = tempfile::tempdir().unwrap();
        let paths = PurgePaths {
            config_dir: Some(home.path().join(".tapes")),
            cache_dir: Some(home.path().join("cache")),
            rc_files: vec![home.path().join(".zshrc")],
            ..PurgePaths::default()
        };

        let (out, err) = run_capturing(Some(&layout), &paths, "n\n", false);

        assert!(err.contains("Continue? [y/N]"), "stderr: {err}");
        assert!(err.contains(&layout.tapesctl_path().display().to_string()));
        assert!(err.contains(&home.path().join(".tapes").display().to_string()));
        assert!(err.contains(&home.path().join("cache").display().to_string()));
        assert!(err.contains(&home.path().join(".zshrc").display().to_string()));
        assert!(
            out.is_empty(),
            "nothing belongs on stdout for a decline: {out}"
        );
    }

    #[test]
    fn a_malformed_rc_block_is_reported_and_left_intact() {
        // Given a hand-truncated rc file: a begin marker with no end
        let install = tempfile::tempdir().unwrap();
        let layout = layout_in(install.path());
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".bashrc");
        let contents = format!("# mine\n{BLOCK_BEGIN}\nexport PATH=x\n");
        std::fs::write(&rc, &contents).unwrap();
        let paths = PurgePaths {
            rc_files: vec![rc.clone()],
            ..PurgePaths::default()
        };

        let (s, _) = run_capturing(Some(&layout), &paths, "", true);

        // Then the file is untouched and the user is told why
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), contents);
        assert!(s.contains("no matching end"), "stdout: {s}");
        // ...and the uninstall still finished: a dotfile we declined to edit
        // must not strand the binary.
        assert!(!layout.tapesctl_path().exists(), "binary should still go");
    }
}
