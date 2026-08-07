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
use tapes_harnesses::harness::{self, Harness, PluginDelivery};
use tapes_harnesses::plugin::{GATEWAY_URL_ENV, PluginArtifact};
use tracing::info;

use crate::cli::{PluginInstallArgs, PluginUninstallArgs};
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
        // Removal comes between the staging and the rename, and the order is
        // the whole point. Writing is only half of an install: pi loads *every*
        // file in its extension directory into one process, so a copy an older
        // client wrote under a different name is not residue but a second
        // reader, contending over the same launch nonce and the same provider
        // registrations and unattributing both products' sessions. Removing it
        // *after* the rename would mean a failed removal returned an error with
        // both extensions sitting where the harness looks — the exact state
        // this is here to prevent, reached by the code preventing it. Removing
        // it while the new bytes are still under a name the harness's glob
        // cannot match means every failure leaves at most one extension: the
        // staged copy is discarded and the user keeps the one that at least
        // works.
        .and_then(|()| remove_superseded(artifact, &resolved_dir))
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

/// Remove the copies this artifact supersedes from the directory it installs
/// into.
///
/// The crate owns the list because no client can be expected to know what its
/// competitor installed, and removing only one's own leaves the collision
/// intact from the other direction.
///
/// A superseded file that is absent is the requested end state. One that exists
/// and cannot be removed fails the install: the harness would go on loading it,
/// and reporting a clean install over that is how the fix ships without
/// reaching anybody.
fn remove_superseded(artifact: &PluginArtifact, resolved_dir: &Path) -> Result<()> {
    for superseded in superseded_targets(artifact, resolved_dir) {
        match std::fs::remove_file(&superseded) {
            Ok(()) => {
                info!(path = %superseded.display(), "superseded plugin removed");
                println!("tapesctl: removed superseded {}", superseded.display());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context(error::PluginWriteSnafu { path: superseded }),
        }
    }
    Ok(())
}

/// The superseded copies to remove, inside a directory already checked for
/// containment.
///
/// The crate's names are `&'static str` components it tests cannot traverse, so
/// re-joining them onto the *resolved* directory only re-states that; what it
/// actually buys is that a symlinked `~/.pi/agent/extensions` cannot make a
/// `remove_file` land outside the home the caller named — the same check the
/// write is contained by, applied to the more dangerous of the two operations.
fn superseded_targets(artifact: &PluginArtifact, resolved_dir: &Path) -> Vec<PathBuf> {
    artifact
        .superseded_file_names()
        .iter()
        .map(|name| resolved_dir.join(name))
        .collect()
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

    // A harness whose plugin is a set of manifest *templates* is not installed
    // by copying anything: the manifests have to be rendered around this
    // client's identity and hook command, the harness's own config has to be
    // pointed at a durable address, and the two have to be reconciled through a
    // handoff. That is a different enough shape to be its own module — and it
    // has to be checked before the artifact list, because templates
    // deliberately yield no artifacts and would otherwise read as "needs no
    // plugin".
    if matches!(harness.plugin(), PluginDelivery::HookManifestTemplates(_)) {
        return crate::codex_app::install::run(args, home);
    }
    // The flags that shape that install describe a durable endpoint and a
    // credential mode, neither of which a copied artifact has.
    if args.port.is_some() {
        return error::PluginFlagNotApplicableSnafu {
            flag: "--port",
            harness: harness.id(),
        }
        .fail();
    }
    if args.codex_auth.is_some() {
        return error::PluginFlagNotApplicableSnafu {
            flag: "--codex-auth",
            harness: harness.id(),
        }
        .fail();
    }

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
            // The removal is the half of an install a user cannot infer from
            // "would install", and it is the half that deletes something they
            // may not know they have. A dry run that named only the write would
            // be describing a different operation than the one it stands in
            // for. Only what is actually there is listed — an absent superseded
            // copy is nothing this run would do.
            for superseded in artifact.superseded_paths(home) {
                if superseded.exists() {
                    println!(
                        "tapesctl: would remove superseded {} (loaded alongside {} by {})",
                        superseded.display(),
                        artifact.file_name(),
                        harness.id(),
                    );
                }
            }
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

/// Run one `plugin uninstall`.
pub fn uninstall(args: PluginUninstallArgs) -> Result<()> {
    let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
    uninstall_in(&args, &home)
}

/// The body of [`uninstall`], against an explicit home.
///
/// Removal is the mirror of installation and inherits its asymmetry: a copied
/// artifact is a file to delete, while a hook plugin also wrote configuration
/// into a file the *harness* owns. Only the second can leave a machine broken
/// if it is half-undone, which is why it has its own path.
fn uninstall_in(args: &PluginUninstallArgs, home: &Path) -> Result<()> {
    let harness = resolve(&args.harness)?;

    if matches!(harness.plugin(), PluginDelivery::HookManifestTemplates(_)) {
        return crate::codex_app::install::uninstall(args, home);
    }

    let artifacts = harness.plugin_artifacts();
    if artifacts.is_empty() {
        println!(
            "tapesctl: {} has no capture plugin to remove — its traffic is \
             captured by redirecting it.",
            harness.id(),
        );
        return Ok(());
    }

    for artifact in artifacts {
        let path = artifact.install_path(home);
        if args.dry_run {
            println!("tapesctl: would remove {}", path.display());
            continue;
        }
        // `remove_file` rather than a look-then-delete: a link at the path is
        // removed as the link it is, never followed to whatever it points at.
        match std::fs::remove_file(&path) {
            Ok(()) => {
                info!(harness = harness.id(), path = %path.display(), "plugin removed");
                println!("tapesctl: removed {}", path.display());
            }
            // Absent is the requested end state, not a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context(error::PluginWriteSnafu { path }),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::plugin::PI_GATEWAY_EXTENSION;
    use tapes_harnesses::plugin::pi;

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

    /// **The presentation this CLI ships.** The asset is no longer a template
    /// with slots a consumer fills — there is one file, installed by everyone,
    /// and what a product says differently it says by setting three variables
    /// in the environment of a launch it owns. tapesctl sets none of them: it
    /// *is* tapes, so the asset's own fallbacks are already the strings it
    /// would have supplied.
    ///
    /// That makes those fallbacks this CLI's user-visible surface, arriving
    /// from a pinned crate revision, so they are asserted here rather than
    /// taken on trust. A revision that renamed the status entry would otherwise
    /// change what a user sees in pi after `tapesctl plugin install pi` with
    /// nothing in this repository's CI to notice.
    #[test]
    fn the_status_entry_this_cli_installs_is_the_neutral_one() {
        assert_eq!(pi::DEFAULT_LABEL, "tapes");
        assert!(
            PI_GATEWAY_EXTENSION
                .contents()
                .contains(&format!("const DEFAULT_LABEL = \"{}\";", pi::DEFAULT_LABEL)),
            "the asset's fallback label is not the constant the crate publishes",
        );
    }

    /// …and the fallback *remedy* names the variable a user can act on, since
    /// a tapesctl-installed extension has no product command to point at.
    ///
    /// The remedy is the one string here with a job beyond looking right: it is
    /// what pi shows when the proxy is fronting a schema the chosen model does
    /// not speak, and a sentence naming a command tapesctl does not have would
    /// be worse than the neutral one.
    #[test]
    fn the_fallback_remedy_names_the_variable_that_fixes_the_problem() {
        let contents = PI_GATEWAY_EXTENSION.contents();
        let remedy = contents
            .split_once("const DEFAULT_REMEDY =")
            .expect("the asset declares no DEFAULT_REMEDY")
            .1;
        let remedy = &remedy[..remedy.find(';').expect("unterminated DEFAULT_REMEDY")];
        assert!(
            remedy.contains(GATEWAY_URL_ENV),
            "the fallback remedy does not name {GATEWAY_URL_ENV}: {remedy}",
        );
    }

    /// tapesctl leaves uncaptured pi sessions alone. The extension installs
    /// into pi's *global* auto-discovery directory, so it loads for every pi
    /// session on the machine; a built-in endpoint would redirect all of them
    /// at a port tapesctl does not even keep open between runs.
    #[test]
    fn the_installed_extension_points_nowhere_until_a_launch_configures_it() {
        let contents = PI_GATEWAY_EXTENSION.contents();
        for literal in ["127.0.0.1", "localhost:", "DEFAULT_GATEWAY_URL"] {
            assert!(
                !contents.contains(literal),
                "the asset carries {literal:?}; it must be inert without {GATEWAY_URL_ENV}",
            );
        }
    }

    /// **The other half of installing.** A user who ran an older `paperctl` has
    /// `paper-gateway.ts` in pi's extension directory, and pi loads *every*
    /// file there into one process: writing `tapes-gateway.ts` beside it leaves
    /// two extensions contending over one launch's nonce and over the same
    /// provider registrations, which unattributes both. An install that only
    /// wrote would leave `tapesctl plugin install pi` reporting success while
    /// creating exactly that.
    #[test]
    fn installing_removes_a_copy_another_client_left_in_the_same_directory() {
        let home = tempfile::tempdir().unwrap();
        let dir = PI_GATEWAY_EXTENSION.install_dir(home.path());
        std::fs::create_dir_all(&dir).unwrap();
        let superseded = dir.join("paper-gateway.ts");
        std::fs::write(&superseded, "// another client's rendering\n").unwrap();

        let written = install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();

        assert!(
            !superseded.exists(),
            "the superseded extension survived the install; pi would load both",
        );
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            PI_GATEWAY_EXTENSION.contents(),
        );
        // …and nothing else went with it: the removal is the crate's named
        // list, not a sweep of a directory the user also keeps their own
        // extensions in.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// **The ordering, not just the removal.** The staging name is chosen so
    /// the harness's glob cannot match it, which is what lets the removal
    /// happen while the new bytes are not yet installed. Pinned because the
    /// property is invisible in the success path: an install that renamed first
    /// and removed second passes every other test here, and fails only when a
    /// removal fails — leaving both extensions where the harness looks, which
    /// is the state the whole artifact exists to prevent.
    #[test]
    fn the_staged_name_is_not_something_the_harness_would_load() {
        let staged = format!(
            ".{}.tapesctl-install-{}",
            PI_GATEWAY_EXTENSION.file_name(),
            std::process::id()
        );
        assert!(
            !staged.ends_with(".ts"),
            "pi globs *.ts; {staged} would be loaded as an extension while staged",
        );
        assert!(staged.starts_with('.'), "got: {staged}");
    }

    /// The list has to actually name the file the bug is about — the test above
    /// would pass just as happily against an empty list if it staged no
    /// superseded file. This is the crate's claim, restated where this command
    /// depends on it, so a crate revision that emptied the list fails here.
    #[test]
    fn the_removal_list_names_what_another_client_actually_installed() {
        assert!(
            PI_GATEWAY_EXTENSION
                .superseded_file_names()
                .contains(&"paper-gateway.ts"),
            "nothing removes the file an older paperctl installed",
        );
    }

    /// A user's own extensions are untouched: only the crate's named list is
    /// removed, so an install is never a reason to lose unrelated work.
    #[test]
    fn installing_leaves_the_users_own_extensions_alone() {
        let home = tempfile::tempdir().unwrap();
        let dir = PI_GATEWAY_EXTENSION.install_dir(home.path());
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("my-extension.ts");
        std::fs::write(&mine, "// mine\n").unwrap();

        install(&PI_GATEWAY_EXTENSION, home.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "// mine\n");
    }

    /// A dry run must describe the operation it stands in for, and the removal
    /// is the half of it that *deletes* something. It still writes nothing.
    #[test]
    fn a_dry_run_names_the_superseded_copy_it_would_remove() {
        let home = tempfile::tempdir().unwrap();
        let dir = PI_GATEWAY_EXTENSION.install_dir(home.path());
        std::fs::create_dir_all(&dir).unwrap();
        let superseded = dir.join("paper-gateway.ts");
        std::fs::write(&superseded, "// another client's rendering\n").unwrap();

        let args = PluginInstallArgs {
            harness: "pi".to_owned(),
            dry_run: true,
            port: None,
            codex_auth: None,
        };
        run_in(&args, home.path()).unwrap();

        assert!(
            superseded.exists(),
            "a dry run that removed the file would have changed the machine",
        );
        assert!(!PI_GATEWAY_EXTENSION.install_path(home.path()).exists());
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
            port: None,
            codex_auth: None,
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
            port: None,
            codex_auth: None,
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
            port: None,
            codex_auth: None,
        };
        run_in(&args, home.path()).unwrap();
        assert!(PI_GATEWAY_EXTENSION.install_path(home.path()).exists());
    }
}
