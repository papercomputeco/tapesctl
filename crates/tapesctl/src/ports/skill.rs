//! `tapesctl skill sync <name>` — ported from `tapes skill sync`.
//!
//! A local file copy, and nothing more: it reads `~/.tapes/skills/<name>.md` and
//! writes it into whichever agent skills directory the flags select. No server
//! is involved — this is the one ported command that makes no HTTP call at all.
//! (Its sibling `tapes skill generate` *is* the API-touching one, and is not
//! part of this port.)
//!
//! The four destinations are a 2×2 of two independent questions — which agent
//! runtime reads the skill, and whether it applies to this project or the whole
//! user — so they are modelled as two flags rather than four command variants:
//!
//! | flags | destination |
//! |---|---|
//! | *(none)* | `~/.agents/skills` |
//! | `--local` | `./.agents/skills` |
//! | `--claude` | `~/.claude/skills` |
//! | `--claude --local` | `./.claude/skills` |
//!
//! The written file keeps mode `0600`, matching the Go implementation: a skill
//! can carry prompt text a user would not want world-readable, and a copy that
//! silently widened permissions relative to the tool it replaces would be a
//! regression nobody would notice.

use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt};
use tracing::info;

use crate::cli::SkillSyncArgs;
use crate::error::{Result, error};
use crate::ports::skill_paths::{validate_name, write_contained};

/// Where authored skills live.
#[must_use]
pub fn default_source_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".tapes").join("skills"))
}

/// A resolved sync destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Directory the skill is written into.
    pub dir: PathBuf,
    /// The root (home or project cwd) the write must stay beneath.
    pub base: PathBuf,
    /// How the destination is described to the user.
    pub label: &'static str,
}

/// Resolve the destination for the given flag combination.
///
/// `home` and `cwd` are parameters rather than ambient lookups so the whole
/// table is testable without touching the real filesystem.
#[must_use]
pub fn destination(home: &Path, cwd: &Path, claude: bool, local: bool) -> Destination {
    match (claude, local) {
        (false, false) => Destination {
            dir: home.join(".agents").join("skills"),
            base: home.to_path_buf(),
            label: "global",
        },
        (false, true) => Destination {
            dir: cwd.join(".agents").join("skills"),
            base: cwd.to_path_buf(),
            label: "project",
        },
        (true, false) => Destination {
            dir: home.join(".claude").join("skills"),
            base: home.to_path_buf(),
            label: "global, claude",
        },
        (true, true) => Destination {
            dir: cwd.join(".claude").join("skills"),
            base: cwd.to_path_buf(),
            label: "project, claude",
        },
    }
}

/// Copy `<source_dir>/<name>.md` into `destination`, returning the written path.
pub fn sync(name: &str, source_dir: &Path, destination: &Destination) -> Result<PathBuf> {
    validate_name(name)?;
    let source = source_dir.join(format!("{name}.md"));
    let contents = std::fs::read(&source).context(error::SkillReadSnafu {
        path: source.clone(),
    })?;

    std::fs::create_dir_all(&destination.dir).context(error::SkillWriteSnafu {
        path: destination.dir.clone(),
    })?;
    let target = write_contained(&destination.dir, &destination.base, name, &contents)?;
    Ok(target)
}

/// Run one `skill sync`.
pub fn run(args: SkillSyncArgs) -> Result<()> {
    let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
    let cwd = std::env::current_dir().context(error::WorkingDirSnafu)?;
    let source_dir = match args.source_dir {
        Some(dir) => dir,
        None => default_source_dir().context(error::NoHomeDirSnafu)?,
    };
    let destination = destination(&home, &cwd, args.claude, args.local);

    if args.dry_run {
        println!(
            "tapesctl: would sync {} to {} ({})",
            args.name,
            destination.dir.display(),
            destination.label,
        );
        return Ok(());
    }

    let written = sync(&args.name, &source_dir, &destination)?;
    info!(skill = %args.name, path = %written.display(), "skill synced");
    println!(
        "tapesctl: synced {} to {} ({})",
        args.name,
        written.display(),
        destination.label,
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn home_and_cwd() -> (PathBuf, PathBuf) {
        (PathBuf::from("/home/u"), PathBuf::from("/work/proj"))
    }

    #[test]
    fn the_destination_table_is_a_two_by_two_of_runtime_and_scope() {
        let (home, cwd) = home_and_cwd();
        assert_eq!(
            destination(&home, &cwd, false, false).dir,
            PathBuf::from("/home/u/.agents/skills"),
        );
        assert_eq!(
            destination(&home, &cwd, false, true).dir,
            PathBuf::from("/work/proj/.agents/skills"),
        );
        assert_eq!(
            destination(&home, &cwd, true, false).dir,
            PathBuf::from("/home/u/.claude/skills"),
        );
        assert_eq!(
            destination(&home, &cwd, true, true).dir,
            PathBuf::from("/work/proj/.claude/skills"),
        );
    }

    #[test]
    fn each_destination_is_labelled_for_the_user() {
        let (home, cwd) = home_and_cwd();
        assert_eq!(destination(&home, &cwd, false, false).label, "global");
        assert_eq!(
            destination(&home, &cwd, true, true).label,
            "project, claude"
        );
    }

    #[test]
    fn a_traversal_name_is_refused_before_any_filesystem_touch() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        // A real file sitting one level above the skills dir: the exact
        // thing a `../` name would reach.
        std::fs::write(source.path().join("secret.md"), "leak").unwrap();
        let skills = source.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();

        let destination = Destination {
            dir: target.path().to_path_buf(),
            base: target.path().to_path_buf(),
            label: "global",
        };
        for name in [
            "../secret",
            "..",
            "a/b",
            "a\\b",
            ".",
            "",
            "sub/../../secret",
        ] {
            let err = sync(name, &skills, &destination).unwrap_err();
            assert!(
                err.to_string().contains("invalid skill name"),
                "{name:?} produced the wrong error: {err}"
            );
        }
        // Nothing was written anywhere.
        assert!(std::fs::read_dir(target.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_skills_directory_is_refused() {
        // The repo pre-created `.agents/skills` as a symlink out of the
        // project: the write and the chmod must not follow it.
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("review.md"), "body").unwrap();
        std::fs::create_dir_all(project.path().join(".agents")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            project.path().join(".agents").join("skills"),
        )
        .unwrap();

        let destination = Destination {
            dir: project.path().join(".agents").join("skills"),
            base: project.path().to_path_buf(),
            label: "project",
        };
        let err = sync("review", source.path(), &destination).unwrap_err();
        assert!(
            err.to_string().contains("resolves outside"),
            "wrong error: {err}"
        );
        assert!(
            std::fs::read_dir(elsewhere.path())
                .unwrap()
                .next()
                .is_none(),
            "nothing may land behind the symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_is_replaced_never_followed() {
        // The pre-planted link is the racer's move; O_EXCL after the
        // remove means the link itself is what dies — the file it pointed
        // at must survive untouched, with its original permissions.
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let victim = target.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        std::fs::write(source.path().join("review.md"), "body").unwrap();
        let skills = target.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&victim, skills.join("review.md")).unwrap();

        let destination = Destination {
            dir: skills.clone(),
            base: target.path().to_path_buf(),
            label: "global",
        };
        let written = sync("review", source.path(), &destination).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert!(
            !std::fs::symlink_metadata(&written)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the target must be a regular file after sync"
        );
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "body");
    }

    #[test]
    fn ordinary_stems_still_pass_validation() {
        for name in ["review", "code-review", "v1.2_final"] {
            assert!(validate_name(name).is_ok(), "{name:?} should be valid");
        }
    }

    #[test]
    fn a_skill_is_copied_byte_for_byte() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let body = "---\nname: review\n---\n\nDo the thing.\n";
        std::fs::write(source.path().join("review.md"), body).unwrap();

        let destination = Destination {
            dir: target.path().join("skills"),
            base: target.path().to_path_buf(),
            label: "global",
        };
        let written = sync("review", source.path(), &destination).unwrap();

        assert_eq!(std::fs::read_to_string(&written).unwrap(), body);
        assert_eq!(written.file_name().unwrap(), "review.md");
    }

    #[test]
    fn the_destination_directory_is_created_if_missing() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("s.md"), "x").unwrap();

        let destination = Destination {
            dir: target.path().join("deep").join("nested").join("skills"),
            base: target.path().to_path_buf(),
            label: "project",
        };
        assert!(sync("s", source.path(), &destination).is_ok());
    }

    #[test]
    fn a_missing_skill_names_the_path_it_looked_in() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let destination = Destination {
            dir: target.path().to_path_buf(),
            base: target.path().to_path_buf(),
            label: "global",
        };

        let err = sync("absent", source.path(), &destination).unwrap_err();
        assert!(format!("{err}").contains("absent.md"), "got: {err}");
    }

    #[test]
    fn an_existing_skill_is_overwritten_rather_than_appended() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let destination = Destination {
            dir: target.path().to_path_buf(),
            base: target.path().to_path_buf(),
            label: "global",
        };
        std::fs::write(source.path().join("s.md"), "first").unwrap();
        sync("s", source.path(), &destination).unwrap();
        std::fs::write(source.path().join("s.md"), "second").unwrap();
        let written = sync("s", source.path(), &destination).unwrap();

        assert_eq!(std::fs::read_to_string(&written).unwrap(), "second");
    }

    #[cfg(unix)]
    #[test]
    fn the_written_skill_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("s.md"), "x").unwrap();
        let destination = Destination {
            dir: target.path().to_path_buf(),
            base: target.path().to_path_buf(),
            label: "global",
        };

        let written = sync("s", source.path(), &destination).unwrap();
        let mode = std::fs::metadata(&written).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got: {:o}", mode & 0o777);
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("s.md"), "x").unwrap();

        let args = SkillSyncArgs {
            name: "s".to_owned(),
            claude: false,
            local: false,
            dry_run: true,
            source_dir: Some(source.path().to_path_buf()),
        };
        run(args).unwrap();

        assert!(!target.path().join("s.md").exists());
    }
}
