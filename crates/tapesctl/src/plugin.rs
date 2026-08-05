//! `tapesctl plugin install <harness>` — put a harness's capture plugin where
//! the harness will find it.
//!
//! Most harnesses need nothing installed: `start` points their base-URL knob at
//! the proxy and that is the whole integration. A harness with no such knob
//! needs code running inside it, and this command is how that code gets there.
//!
//! # Thin on purpose
//!
//! Nothing about *what* is installed is decided here. `tapes-harnesses` owns the
//! artifacts and the environment contract they read; this command resolves the
//! name the user typed against that crate's registry, takes the artifacts the
//! resolved harness declares, and writes them. Adding a harness that needs a
//! plugin is therefore a change in the crate alone — no arm is added here, and
//! the closed-source client installing the same crate's artifacts installs the
//! same bytes.
//!
//! # Writing into someone's home
//!
//! The destinations are dot-directories in the user's home that the harness —
//! not tapesctl — created, which is a hostile enough place to write that the
//! containment discipline from [`crate::ports::skill`] applies unchanged: the
//! resolved destination must still sit beneath the home it was derived from, and
//! the final create is `O_EXCL` after an unlink so a symlink planted at the
//! target makes the write *fail* rather than redirect. The one thing that does
//! not carry over is name validation — a skill name is user input, whereas these
//! path components are `&'static str` constants in the crate, and the crate
//! tests that none of them can traverse.

use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt};
use tapes_harnesses::harness::{self, Harness};
use tapes_harnesses::plugin::{GATEWAY_URL_ENV, PluginArtifact};
use tracing::info;

use crate::cli::PluginInstallArgs;
use crate::error::{Result, error};

/// Resolve a user-typed harness name against the shared registry.
///
/// The registry, not a local list: a harness that gains a plugin in the crate
/// becomes installable here without this file being edited.
fn resolve(name: &str) -> Result<&'static Harness> {
    harness::find(name).context(error::UnknownHarnessSnafu {
        harness: name.to_owned(),
        known: known_harnesses(),
    })
}

/// Every name the registry answers to, for the error message.
fn known_harnesses() -> String {
    harness::all()
        .iter()
        .map(|harness| harness.id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Write one artifact beneath `home`, returning the path written.
///
/// `home` is a parameter rather than an ambient lookup so the containment
/// behaviour is testable against a temporary directory — the interesting cases
/// here are all about what happens when the destination is not what it appears
/// to be.
pub fn install(artifact: &PluginArtifact, home: &Path) -> Result<PathBuf> {
    let dir = artifact.install_dir(home);
    // Created rather than required. `paperctl init` deliberately refuses to
    // create `~/.pi` because it installs implicitly, as a side effect of an
    // unrelated command; this command exists only because the user asked for it
    // by name, so materialising the directory is the requested action, not a
    // surprise.
    std::fs::create_dir_all(&dir).context(error::PluginWriteSnafu { path: dir.clone() })?;

    // The harness owns these directories, and any of them may already be a
    // symlink pointing anywhere. Resolve what is actually on disk and require
    // it to still sit beneath the home the caller named.
    let resolved_dir = dir
        .canonicalize()
        .context(error::PluginWriteSnafu { path: dir.clone() })?;
    let resolved_home = home.canonicalize().context(error::PluginWriteSnafu {
        path: home.to_path_buf(),
    })?;
    snafu::ensure!(
        resolved_dir.starts_with(&resolved_home),
        error::PluginDestinationSnafu { path: dir.clone() }
    );

    let target = resolved_dir.join(artifact.file_name());
    // Reinstalling must replace the file — that is how an artifact is upgraded
    // — but neither a look-then-write nor an unlink-then-write will do. The
    // first can be raced through a planted symlink; the second removes a
    // WORKING plugin before its replacement exists, so any later failure
    // leaves the harness with nothing (or a partial file). So: write the
    // whole replacement to a sibling temp file created with `O_EXCL` (which
    // never follows a symlink), set its permissions through the handle, and
    // only then rename it over the target — an atomic swap with no window in
    // which the plugin is absent or half-written. A rename replaces even a
    // planted symlink itself rather than writing through it.
    let staged = resolved_dir.join(format!(
        ".{}.tapesctl-install-{}",
        artifact.file_name(),
        std::process::id()
    ));
    let mut file = match open_staging(&staged) {
        Ok(file) => file,
        // Crash residue: a previous install died between staging and rename,
        // and PID reuse landed a later run on the same name. The name is ours
        // by construction (dot-prefixed, tapesctl-install-suffixed), so the
        // leftover is removed and the exclusive create retried exactly once —
        // a racer re-planting between the two still loses the O_EXCL.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&staged);
            open_staging(&staged).context(error::PluginWriteSnafu {
                path: staged.clone(),
            })?
        }
        Err(err) => {
            return Err(err).context(error::PluginWriteSnafu {
                path: staged.clone(),
            });
        }
    };
    restrict_permissions(&file, &staged)?;
    use std::io::Write as _;
    let staged_result = file
        .write_all(artifact.contents().as_bytes())
        .and_then(|()| file.sync_all())
        .context(error::PluginWriteSnafu {
            path: staged.clone(),
        })
        .and_then(|()| {
            std::fs::rename(&staged, &target).context(error::PluginWriteSnafu {
                path: target.clone(),
            })
        });
    if staged_result.is_err() {
        // Best-effort cleanup: the working plugin was never touched.
        let _ = std::fs::remove_file(&staged);
    }
    staged_result?;
    Ok(target)
}

/// Open the staging file exclusively — never through an existing entry.
fn open_staging(staged: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged)
}

/// Narrow the open file to owner-only, through its handle rather than by path.
///
/// The artifact's contents are public source, so this is not about secrecy: it
/// is code that the user's agent will load and execute, landing in a directory
/// only that user's agent reads. Owner-only is the conservative default, and it
/// matches what `skill sync` writes.
#[cfg(unix)]
fn restrict_permissions(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context(error::PluginWriteSnafu {
            path: path.to_path_buf(),
        })
}

/// No-op off Unix, where the mode bits have no equivalent.
#[cfg(not(unix))]
fn restrict_permissions(_file: &std::fs::File, _path: &Path) -> Result<()> {
    Ok(())
}

/// Run one `plugin install`.
pub fn run(args: PluginInstallArgs) -> Result<()> {
    let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
    run_in(&args, &home)
}

/// The body of [`run`], against an explicit home.
///
/// Split out for the same reason [`install`] takes one: every case worth
/// testing — the dry run that must write nothing, the harness with nothing to
/// install — is about what does or does not appear on disk, and a test that
/// asserted against the developer's real home would be both unreliable and
/// destructive.
fn run_in(args: &PluginInstallArgs, home: &Path) -> Result<()> {
    let harness = resolve(&args.harness)?;
    let artifacts = harness.plugin_artifacts();

    // Not an error. "This harness needs no plugin" is the ordinary answer —
    // capture by redirection is the norm and an in-harness extension the
    // exception — and a non-zero exit would make a correct setup script look
    // broken. No count is given: the registry grows, and a number here would
    // become a lie without anything failing.
    if artifacts.is_empty() {
        println!(
            "tapesctl: {} needs no capture plugin — its traffic is captured by \
             redirecting it, which `tapesctl start {}` does.",
            harness.id(),
            harness.id(),
        );
        return Ok(());
    }

    if args.dry_run {
        for artifact in artifacts {
            println!(
                "tapesctl: would install {} to {}",
                artifact.file_name(),
                artifact.install_path(home).display(),
            );
        }
        return Ok(());
    }

    for artifact in artifacts {
        let written = install(artifact, home)?;
        info!(harness = harness.id(), path = %written.display(), "plugin installed");
        println!("tapesctl: installed {}", written.display());
    }
    // The artifact is inert until this is set, and it is set by whoever launches
    // the harness — so an install alone captures nothing, and saying so here is
    // cheaper than the user discovering it from an empty session list.
    println!(
        "tapesctl: {} is loaded by every {} session but stays inactive until \
         {GATEWAY_URL_ENV} names a capture proxy.",
        if artifacts.len() == 1 {
            "it"
        } else {
            "they are"
        },
        harness.id(),
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::plugin::PI_GATEWAY_EXTENSION;

    /// The command's whole reason to exist: the bytes the crate owns land where
    /// the harness looks for them. A regression that wrote an empty file, or
    /// wrote to the wrong place, passes every containment test below.
    #[test]
    fn the_crate_owned_artifact_lands_where_the_harness_looks_for_it() {
        let home = tempfile::tempdir().unwrap();
        let written = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();

        assert_eq!(
            written,
            home.path()
                .canonicalize()
                .unwrap()
                .join(".pi")
                .join("agent")
                .join("extensions")
                .join("tapes-gateway.ts"),
        );
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            PI_GATEWAY_EXTENSION.contents(),
            "the installed file must be the crate's asset byte for byte",
        );
    }

    /// The harness's extension directory need not exist yet — a user may run
    /// this before ever starting the harness.
    #[test]
    fn the_extension_directory_is_created_when_absent() {
        let home = tempfile::tempdir().unwrap();
        assert!(!home.path().join(".pi").exists());
        install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();
        assert!(
            home.path()
                .join(".pi")
                .join("agent")
                .join("extensions")
                .is_dir()
        );
    }

    /// Reinstalling is how an artifact is upgraded, so a stale copy must be
    /// replaced outright rather than appended to or left in place.
    #[test]
    fn reinstalling_replaces_a_stale_copy() {
        let home = tempfile::tempdir().unwrap();
        let first = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();
        std::fs::write(&first, "// an older, different extension").unwrap();

        let second = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            PI_GATEWAY_EXTENSION.contents(),
        );
        // The staged temp file must not survive a successful swap: the
        // extension directory is auto-loaded, and a leftover would be one
        // upgrade away from being executed.
        let strays: Vec<_> = std::fs::read_dir(second.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(strays.is_empty(), "staging residue: {strays:?}");
    }

    #[test]
    fn crash_residue_at_the_staging_name_does_not_block_a_retry() {
        // A previous install died between staging and rename; PID reuse lands
        // this run on the same staging name. The residue is ours — the retry
        // must clear it and complete.
        let home = tempfile::tempdir().unwrap();
        let artifact = &PI_GATEWAY_EXTENSION;
        let dir = artifact.install_dir(home.path());
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join(format!(
            ".{}.tapesctl-install-{}",
            artifact.file_name(),
            std::process::id()
        ));
        std::fs::write(&staged, "// residue from a dead installer").unwrap();

        let written = install(artifact, home.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            artifact.contents()
        );
        assert!(!staged.exists(), "the residue must be gone after the swap");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_extension_directory_is_refused() {
        // The harness's own config directory is a plausible thing for a user to
        // symlink into a dotfiles repo — or for something else to have replaced.
        // Either way the write must not follow it out of the home.
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".pi").join("agent")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            home.path().join(".pi").join("agent").join("extensions"),
        )
        .unwrap();

        let err = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap_err();
        assert!(err.to_string().contains("resolves outside"), "got: {err}");
        assert!(
            std::fs::read_dir(elsewhere.path())
                .unwrap()
                .next()
                .is_none(),
            "nothing may land behind the symlink",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_is_replaced_never_followed() {
        // The racer's move: a link at the exact path the install targets. The
        // link is what dies; the file it pointed at must be untouched.
        let home = tempfile::tempdir().unwrap();
        let victim_dir = tempfile::tempdir().unwrap();
        let victim = victim_dir.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        let dir = home.path().join(".pi").join("agent").join("extensions");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("tapes-gateway.ts")).unwrap();

        let written = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert!(
            !std::fs::symlink_metadata(&written)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the installed artifact must be a regular file",
        );
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            PI_GATEWAY_EXTENSION.contents(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_artifact_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let written = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();
        let mode = std::fs::metadata(&written).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got: {:o}", mode & 0o777);
    }

    #[test]
    fn a_name_the_registry_does_not_know_is_refused_with_the_ones_it_does() {
        let err = resolve("gemini").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("gemini"), "got: {message}");
        // The list is derived, so a harness added to the registry appears here
        // without this file changing.
        assert!(message.contains("claude"), "got: {message}");
        assert!(message.contains("pi"), "got: {message}");
    }

    #[test]
    fn a_name_resolves_through_the_registrys_aliases_and_casing() {
        assert_eq!(resolve("PI").unwrap().id(), "pi");
        assert_eq!(resolve("  claude-code ").unwrap().id(), "claude");
    }

    /// A harness captured by redirection alone has nothing to install, and
    /// saying so must not look like a failure — nor leave a stray directory
    /// behind in the user's home.
    #[test]
    fn installing_for_a_harness_with_no_plugin_succeeds_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let args = PluginInstallArgs {
            harness: "claude".to_owned(),
            dry_run: false,
        };
        run_in(&args, home.path()).unwrap();
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let args = PluginInstallArgs {
            harness: "pi".to_owned(),
            dry_run: true,
        };
        run_in(&args, home.path()).unwrap();

        assert!(!PI_GATEWAY_EXTENSION.install_path(home.path()).exists());
        // Not even the directory: a dry run that created `~/.pi` would have
        // changed the machine while claiming not to.
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    /// The same invocation without `--dry-run` does write — otherwise the test
    /// above would pass just as well against an installer that never worked.
    #[test]
    fn the_same_invocation_without_dry_run_installs() {
        let home = tempfile::tempdir().unwrap();
        let args = PluginInstallArgs {
            harness: "pi".to_owned(),
            dry_run: false,
        };
        run_in(&args, home.path()).unwrap();
        assert!(PI_GATEWAY_EXTENSION.install_path(home.path()).exists());
    }
}
