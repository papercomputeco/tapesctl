//! Path safety shared by every command that writes a skill file.
//!
//! Two commands write into a skills directory — `skill sync` copies one in, and
//! `skill generate` authors one — and both take the file stem from user input.
//! The checks live here rather than in either command because a security check
//! with two copies is a security check that will eventually have two behaviours,
//! and the copy that drifts is the one nobody is reading.

use std::path::{Path, PathBuf};

use snafu::ResultExt;

use crate::error::{Result, error};

/// A skill name is a bare file stem. Anything that could steer a join outside
/// the skills directory — separators, `..`, an empty or all-dots name — is
/// rejected before it touches the filesystem, because the name feeds a read, a
/// write, AND a chmod.
pub fn validate_name(name: &str) -> Result<()> {
    let simple = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.chars().all(|c| c == '.');
    snafu::ensure!(simple, error::SkillNameSnafu { name });
    Ok(())
}

/// Resolve the file `<dir>/<name>.md` may be written to, refusing any path that
/// leaves `base`.
///
/// `dir` must already exist — the caller creates it — because containment is
/// decided by what is actually on disk, not by what the path string looks like.
/// A repository can pre-create the project-local skills directory as a symlink
/// pointing anywhere; following it would put the write AND the chmod on a file
/// outside the tree the user selected. The final component gets the same
/// treatment: an existing symlink at the target would route the write through
/// it.
pub fn contained_target(dir: &Path, base: &Path, name: &str) -> Result<PathBuf> {
    // Re-checked here even though callers validate too: this function is what
    // performs the join, so it is what has to be sure of the input.
    validate_name(name)?;

    let resolved_dir = dir.canonicalize().context(error::SkillWriteSnafu {
        path: dir.to_path_buf(),
    })?;
    let resolved_base = base.canonicalize().context(error::SkillWriteSnafu {
        path: base.to_path_buf(),
    })?;
    snafu::ensure!(
        resolved_dir.starts_with(&resolved_base),
        error::SkillDestinationSnafu {
            path: dir.to_path_buf(),
        }
    );

    let target = resolved_dir.join(format!("{name}.md"));
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        snafu::ensure!(
            !meta.file_type().is_symlink(),
            error::SkillDestinationSnafu {
                path: target.clone(),
            }
        );
    }
    Ok(target)
}

/// Narrow a written skill to owner-only.
///
/// A skill can carry prompt text a user would not want world-readable, and a
/// writer that silently widened permissions relative to the tool it replaces
/// would be a regression nobody would notice.
#[cfg(unix)]
fn restrict_permissions(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context(error::SkillWriteSnafu {
            path: path.to_path_buf(),
        })
}

/// No-op off Unix, where the mode bits have no equivalent.
#[cfg(not(unix))]
fn restrict_permissions(_file: &std::fs::File, _path: &Path) -> Result<()> {
    Ok(())
}

/// Resolve a contained target and write `contents` there, safely.
///
/// The final component cannot be handled with a look-then-write: a racer
/// replacing the target with a symlink between the check and the write would
/// route both the bytes and the chmod elsewhere. Instead any old file is
/// removed and the new one is created with O_EXCL — which never follows a
/// symlink — so a racer's link makes the create FAIL rather than redirect, and
/// the permissions are set through the open handle, never by path. Both skill
/// writers (`sync` and `generate`) funnel here: a security check with two
/// copies eventually has two behaviours.
pub fn write_contained(dir: &Path, base: &Path, name: &str, contents: &[u8]) -> Result<PathBuf> {
    let target = contained_target(dir, base, name)?;
    match std::fs::remove_file(&target) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return error::SkillDestinationSnafu {
                path: target.clone(),
            }
            .fail();
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .context(error::SkillWriteSnafu {
            path: target.clone(),
        })?;
    restrict_permissions(&file, &target)?;
    use std::io::Write as _;
    file.write_all(contents).context(error::SkillWriteSnafu {
        path: target.clone(),
    })?;
    Ok(target)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_stems_pass() {
        for name in ["review", "code-review", "v1.2_final"] {
            assert!(validate_name(name).is_ok(), "{name:?} should be valid");
        }
    }

    #[test]
    fn a_traversal_name_is_refused() {
        for name in [
            "../secret",
            "..",
            "a/b",
            "a\\b",
            ".",
            "",
            "sub/../../secret",
        ] {
            let err = validate_name(name).unwrap_err();
            assert!(
                err.to_string().contains("invalid skill name"),
                "{name:?} produced the wrong error: {err}",
            );
        }
    }

    #[test]
    fn a_contained_target_is_the_file_inside_the_directory() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();

        let target = contained_target(&dir, base.path(), "review").unwrap();

        assert_eq!(target.file_name().unwrap(), "review.md");
        assert!(target.starts_with(base.path().canonicalize().unwrap()));
    }

    #[test]
    fn a_traversal_name_never_reaches_the_join() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();

        let err = contained_target(&dir, base.path(), "../escape").unwrap_err();
        assert!(err.to_string().contains("invalid skill name"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dir = base.path().join("skills");
        std::os::unix::fs::symlink(elsewhere.path(), &dir).unwrap();

        let err = contained_target(&dir, base.path(), "review").unwrap_err();
        assert!(err.to_string().contains("resolves outside"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_file_is_refused() {
        let base = tempfile::tempdir().unwrap();
        let victim = base.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        let dir = base.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("review.md")).unwrap();

        let err = contained_target(&dir, base.path(), "review").unwrap_err();
        assert!(err.to_string().contains("resolves outside"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    }
}
