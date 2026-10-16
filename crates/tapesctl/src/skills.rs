//! `tapesctl skills install` — put the recall skills where agents read them.
//!
//! A recall skill is a directory holding a `SKILL.md` (and anything it ships
//! beside it) that teaches an agent to find prior sessions with
//! `tapesctl search` and read them with `tapesctl sessions transcript`. The
//! harnesses discover skills from their own directories:
//!
//! | harness | detected by | installs into |
//! |---|---|---|
//! | Claude Code | `~/.claude` exists | `~/.claude/skills/<name>` |
//! | Codex | `~/.codex` exists | `~/.agents/skills/<name>` |
//!
//! # Where the skills come from
//!
//! Not from this binary. Which skill collection a deployment recommends is
//! that deployment's choice, and a collection that changes weekly should not
//! need a tapesctl release — so the source is named by the caller
//! (`--source` / `TAPESCTL_SKILLS_SOURCE`): a local directory, or a git URL
//! cloned shallowly into a temporary directory. A source may be laid out as a
//! repository (`skills/<name>/SKILL.md`) or as a flat directory
//! (`<name>/SKILL.md`).
//!
//! # Why `skills` is not a built-in noun
//!
//! `tapesctl skills <method>` is also the top-level spelling of a deployment's
//! skills cassette. A compiled-in `skills` command would shadow it, so the
//! `install` verb is grafted onto whichever `skills` command the parser has —
//! the generated one when the cassette is served, a bare one otherwise — by
//! [`mount`], and recognised before the cassette dispatch by [`matched`].
//!
//! # Writes
//!
//! Each skill is copied into a staging directory beside its destination and
//! renamed into place, so a failed copy never leaves a half-written skill
//! where a harness would load it. Re-installing replaces the previous copy.
//! Symbolic links in the source are refused rather than followed: a skill
//! that reached outside its own directory would install whatever it pointed
//! at.

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::{ArgMatches, Args, FromArgMatches};
use snafu::{OptionExt, ResultExt};

use crate::cli::{DEFAULT_SKILLS, SkillsInstallArgs};
use crate::error::{Result, error};
use crate::machine::Machine;

/// The noun the verb is mounted under.
pub const NOUN: &str = "skills";
/// The verb.
pub const VERB: &str = "install";

/// A harness this command installs skills for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl Target {
    const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The harness's name, as `--harness` spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The directory whose existence says the harness is used on this machine.
    #[must_use]
    pub fn home_marker(self, home: &Path) -> PathBuf {
        match self {
            Self::Claude => home.join(".claude"),
            Self::Codex => home.join(".codex"),
        }
    }

    /// Where the harness discovers user skills.
    #[must_use]
    pub fn skills_dir(self, home: &Path) -> PathBuf {
        match self {
            Self::Claude => home.join(".claude").join("skills"),
            Self::Codex => home.join(".agents").join("skills"),
        }
    }
}

/// Mount `skills install` onto `command`.
///
/// Onto the existing `skills` command when discovery mounted the skills
/// cassette there — unless that cassette already has an `install` method of
/// its own, which then wins, as a cassette's own method does everywhere — and
/// as a bare `skills` command otherwise.
#[must_use]
pub fn mount(command: clap::Command) -> clap::Command {
    let verb = || {
        SkillsInstallArgs::augment_args(clap::Command::new(VERB))
            .about("Install the recall skills (search, recall, digest) for Claude Code and Codex")
    };
    if command.find_subcommand(NOUN).is_some() {
        command.mut_subcommand(NOUN, |noun| {
            if noun.find_subcommand(VERB).is_some() {
                noun
            } else {
                noun.subcommand(verb())
            }
        })
    } else {
        command.subcommand(
            clap::Command::new(NOUN)
                .about("Install agent skills that use this CLI")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(verb()),
        )
    }
}

/// The `skills install` arguments, when these matches are that command.
pub fn matched(
    matches: &ArgMatches,
) -> Option<std::result::Result<SkillsInstallArgs, clap::Error>> {
    let (noun, noun_matches) = matches.subcommand()?;
    if noun != NOUN {
        return None;
    }
    let (verb, verb_matches) = noun_matches.subcommand()?;
    if verb != VERB {
        return None;
    }
    Some(SkillsInstallArgs::from_arg_matches(verb_matches))
}

/// One skill placed (or, in a dry run, to be placed) for one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The harness it is for.
    pub target: Target,
    /// The skill's name.
    pub skill: String,
    /// Its directory once installed.
    pub path: PathBuf,
    /// Whether an earlier copy was (or would be) replaced.
    pub replaced: bool,
}

/// Run `tapesctl skills install`.
pub fn run(args: &SkillsInstallArgs) -> Result<()> {
    let machine = Machine::resolve()?;
    let fetched = fetch(args)?;
    let placements = install_in(args, fetched.root(), machine.home())?;
    print!("{}", report(&placements, args.dry_run, machine.home()));
    Ok(())
}

/// The harnesses to install for: those named, else those detected.
pub fn targets(args: &SkillsInstallArgs, home: &Path) -> Result<Vec<Target>> {
    let named: Vec<Target> = args
        .harnesses
        .iter()
        .filter_map(|name| Target::parse(name))
        .fold(Vec::new(), |mut acc, target| {
            if !acc.contains(&target) {
                acc.push(target);
            }
            acc
        });
    if !named.is_empty() {
        return Ok(named);
    }
    let detected: Vec<Target> = Target::ALL
        .into_iter()
        .filter(|target| target.home_marker(home).is_dir())
        .collect();
    snafu::ensure!(!detected.is_empty(), error::SkillsNoHarnessSnafu);
    Ok(detected)
}

/// Install the requested skills from `source_root` for every target under
/// `home`.
pub fn install_in(
    args: &SkillsInstallArgs,
    source_root: &Path,
    home: &Path,
) -> Result<Vec<Placement>> {
    let available = available_skills(source_root)?;
    let wanted: Vec<String> = if args.skills.is_empty() {
        DEFAULT_SKILLS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    } else {
        args.skills.clone()
    };
    let mut sources = Vec::new();
    for name in &wanted {
        let dir = available
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, dir)| dir.clone())
            .context(error::SkillsUnknownSnafu {
                name: name.clone(),
                available: available
                    .iter()
                    .map(|(known, _)| known.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
        sources.push((name.clone(), dir));
    }

    let mut placements = Vec::new();
    for target in targets(args, home)? {
        let root = target.skills_dir(home);
        for (name, dir) in &sources {
            let path = root.join(name);
            let replaced = path.symlink_metadata().is_ok();
            if !args.dry_run {
                place(dir, &root, name)?;
            }
            placements.push(Placement {
                target,
                skill: name.clone(),
                path,
                replaced,
            });
        }
    }
    Ok(placements)
}

/// Every skill a source carries, by name: each subdirectory holding a
/// `SKILL.md`, under `skills/` when the source is laid out as a repository.
pub fn available_skills(source_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let nested = source_root.join("skills");
    let base = if nested.is_dir() {
        nested
    } else {
        source_root.to_path_buf()
    };
    let unreadable = |detail: String| {
        error::SkillsSourceSnafu {
            source_ref: source_root.display().to_string(),
            detail,
        }
        .build()
    };
    let entries = std::fs::read_dir(&base).map_err(|err| unreadable(err.to_string()))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| unreadable(err.to_string()))?;
        let path = entry.path();
        // `file_type` does not follow a symlink, so a linked directory is not
        // offered as a skill.
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_dir && path.join("SKILL.md").is_file() {
            found.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }
    found.sort();
    snafu::ensure!(
        !found.is_empty(),
        error::SkillsSourceSnafu {
            source_ref: source_root.display().to_string(),
            detail: "no <name>/SKILL.md directories found".to_owned(),
        }
    );
    Ok(found)
}

/// Copy one skill into `root/name` through a staging directory.
///
/// Two installs into the same directory must not interleave: the replacement
/// is a remove followed by a rename, and a second run removing the first run's
/// freshly renamed skill — or its staging directory — would leave the skill
/// missing. Each run stages into its own directory, and the remove-and-rename
/// happens under an exclusive lock on the skills directory.
fn place(source: &Path, root: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(root).context(error::SkillsWriteSnafu { path: root })?;
    let staged = tempfile::Builder::new()
        .prefix(&format!(".{name}.tapesctl-staging-"))
        .tempdir_in(root)
        .context(error::SkillsWriteSnafu { path: root })?;
    // On any error below, dropping `staged` removes the partial copy.
    copy_tree(source, staged.path())?;

    let _lock = lock_install(root)?;
    let destination = root.join(name);
    // Residue from an interrupted run that predates per-run staging.
    remove_any(&root.join(format!(".{name}.tapesctl-staging")))?;
    remove_any(&destination)?;
    std::fs::rename(staged.path(), &destination).context(error::SkillsWriteSnafu {
        path: destination.clone(),
    })?;
    // The directory now lives at `destination`; nothing is left to clean up.
    let _ = staged.keep();
    Ok(())
}

/// Take an exclusive advisory `flock(2)` on the skills directory itself,
/// waiting for a concurrent install to finish. Locking the directory's own
/// descriptor rather than a lock file leaves nothing behind in a directory the
/// harness reads skills from. The kernel releases the lock when the handle is
/// dropped or the process exits, however it exits.
fn lock_install(root: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;

    let dir = std::fs::File::open(root).context(error::SkillsWriteSnafu { path: root })?;
    if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context(error::SkillsWriteSnafu { path: root });
    }
    Ok(dir)
}

/// Remove a file, symlink, or directory tree; absent is fine.
fn remove_any(path: &Path) -> Result<()> {
    let Ok(meta) = path.symlink_metadata() else {
        return Ok(());
    };
    let removed = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    removed.context(error::SkillsWriteSnafu { path })
}

/// Copy a directory tree, refusing symbolic links.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).context(error::SkillsWriteSnafu { path: to })?;
    let entries = std::fs::read_dir(from).context(error::SkillsWriteSnafu { path: from })?;
    for entry in entries {
        let entry = entry.context(error::SkillsWriteSnafu { path: from })?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .context(error::SkillsWriteSnafu { path: path.clone() })?;
        let target = to.join(entry.file_name());
        if kind.is_symlink() {
            return error::SkillsSourceSnafu {
                source_ref: path.display().to_string(),
                detail: "symbolic links are not installed".to_owned(),
            }
            .fail();
        } else if kind.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).context(error::SkillsWriteSnafu { path: target })?;
        }
    }
    Ok(())
}

/// A resolved source: a local directory, or a clone that is deleted on drop.
pub enum Fetched {
    /// A directory the caller named.
    Local(PathBuf),
    /// A shallow clone in a temporary directory, removed when this drops.
    Cloned {
        /// Keeps the temporary directory alive.
        _dir: tempfile::TempDir,
        /// The checkout inside it.
        checkout: PathBuf,
    },
}

impl Fetched {
    /// The source's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        match self {
            Self::Local(path) => path,
            Self::Cloned { checkout, .. } => checkout,
        }
    }
}

/// Resolve `--source`: an existing directory is used in place; anything else
/// is cloned with `git`.
pub fn fetch(args: &SkillsInstallArgs) -> Result<Fetched> {
    let source = args.source.trim();
    snafu::ensure!(!source.is_empty(), error::SkillsNoSourceSnafu);
    let local = Path::new(source);
    if local.is_dir() {
        return Ok(Fetched::Local(local.to_path_buf()));
    }
    let failed = |detail: String| {
        error::SkillsSourceSnafu {
            source_ref: source.to_owned(),
            detail,
        }
        .build()
    };
    let dir =
        tempfile::TempDir::with_prefix("tapesctl-skills").map_err(|err| failed(err.to_string()))?;
    let checkout = dir.path().join("source");
    let mut git = Process::new("git");
    git.args(["clone", "--quiet", "--depth", "1"]);
    if let Some(git_ref) = args.git_ref.as_deref() {
        git.args(["--branch", git_ref]);
    }
    // `--` so a source spelled like a flag is a URL, never an option.
    git.arg("--").arg(source).arg(&checkout);
    let output = git
        .output()
        .map_err(|err| failed(format!("could not run git: {err}")))?;
    if !output.status.success() {
        let said = crate::render::text::one_line(&String::from_utf8_lossy(&output.stderr));
        return Err(failed(if said.is_empty() {
            format!("git clone exited with {}", output.status)
        } else {
            said
        }));
    }
    Ok(Fetched::Cloned {
        _dir: dir,
        checkout,
    })
}

/// What `skills install` prints.
#[must_use]
pub fn report(placements: &[Placement], dry_run: bool, home: &Path) -> String {
    let home = home.to_string_lossy();
    let mut out = String::new();
    for placement in placements {
        let shown = placement.path.to_string_lossy();
        let shown = shown
            .strip_prefix(&*home)
            .map_or_else(|| shown.to_string(), |rest| format!("~{rest}"));
        let verb = match (dry_run, placement.replaced) {
            (true, true) => "would replace",
            (true, false) => "would install",
            (false, true) => "replaced",
            (false, false) => "installed",
        };
        out.push_str(&format!(
            "{verb} {} for {} at {shown}\n",
            placement.skill,
            placement.target.name()
        ));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn args(source: &Path) -> SkillsInstallArgs {
        SkillsInstallArgs {
            source: source.display().to_string(),
            git_ref: None,
            harnesses: Vec::new(),
            skills: Vec::new(),
            dry_run: false,
        }
    }

    /// A repository-shaped source with the three recall skills and one more.
    fn source() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in ["search", "recall", "digest", "dagger-check"] {
            let skill = dir.path().join("skills").join(name);
            std::fs::create_dir_all(skill.join("refs")).unwrap();
            std::fs::write(skill.join("SKILL.md"), format!("---\nname: {name}\n---\n")).unwrap();
            std::fs::write(skill.join("refs").join("notes.md"), "notes").unwrap();
        }
        dir
    }

    #[test]
    fn concurrent_installs_of_one_skill_leave_it_installed() {
        // Each run stages separately and replaces under the lock, so no
        // interleaving can remove the skill another run just put in place.
        let src = source();
        let skill = src.path().join("skills").join("search");
        let root = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..10 {
                        place(&skill, root.path(), "search").unwrap();
                    }
                });
            }
        });
        let installed = root.path().join("search");
        assert!(installed.join("SKILL.md").is_file());
        assert!(installed.join("refs").join("notes.md").is_file());
        let residue: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("staging"))
            .collect();
        assert!(residue.is_empty(), "staging left behind: {residue:?}");
    }

    #[test]
    fn detected_harnesses_get_the_default_skills_in_their_own_directories() {
        let src = source();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();

        let placed = install_in(&args(src.path()), src.path(), home.path()).unwrap();
        assert_eq!(placed.len(), 6);
        for name in DEFAULT_SKILLS {
            for root in [".claude/skills", ".agents/skills"] {
                let skill = home.path().join(root).join(name);
                assert!(skill.join("SKILL.md").is_file(), "{}", skill.display());
                assert!(skill.join("refs/notes.md").is_file());
            }
        }
        assert!(!home.path().join(".claude/skills/dagger-check").exists());
        // No staging residue next to the installed skills.
        let left: Vec<_> = std::fs::read_dir(home.path().join(".claude/skills"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(left.iter().all(|name| !name.starts_with('.')), "{left:?}");
    }

    #[test]
    fn only_harnesses_that_exist_are_detected() {
        let src = source();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        let placed = install_in(&args(src.path()), src.path(), home.path()).unwrap();
        assert!(placed.iter().all(|p| p.target == Target::Claude));
        assert!(!home.path().join(".agents").exists());
    }

    #[test]
    fn no_detected_harness_is_an_error_that_names_the_flag() {
        let src = source();
        let home = tempfile::tempdir().unwrap();
        let err = install_in(&args(src.path()), src.path(), home.path()).unwrap_err();
        assert!(err.to_string().contains("--harness"), "{err}");
    }

    #[test]
    fn a_named_harness_and_skill_narrow_the_install_and_a_dry_run_writes_nothing() {
        let src = source();
        let home = tempfile::tempdir().unwrap();
        let mut a = args(src.path());
        a.harnesses = vec!["codex".into()];
        a.skills = vec!["search".into()];
        a.dry_run = true;
        let placed = install_in(&a, src.path(), home.path()).unwrap();
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].path, home.path().join(".agents/skills/search"));
        assert!(!home.path().join(".agents").exists());
        assert_eq!(
            report(&placed, true, home.path()),
            "would install search for codex at ~/.agents/skills/search\n"
        );
    }

    #[test]
    fn reinstalling_replaces_the_previous_copy() {
        let src = source();
        let home = tempfile::tempdir().unwrap();
        let mut a = args(src.path());
        a.harnesses = vec!["claude".into()];
        a.skills = vec!["search".into()];
        let stale = home.path().join(".claude/skills/search");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("stale.md"), "old").unwrap();

        let placed = install_in(&a, src.path(), home.path()).unwrap();
        assert!(placed[0].replaced);
        assert!(!stale.join("stale.md").exists());
        assert!(stale.join("SKILL.md").is_file());
    }

    #[test]
    fn an_unknown_skill_is_refused_naming_what_the_source_has() {
        let src = source();
        let home = tempfile::tempdir().unwrap();
        let mut a = args(src.path());
        a.harnesses = vec!["claude".into()];
        a.skills = vec!["nope".into()];
        let err = install_in(&a, src.path(), home.path()).unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("nope") && shown.contains("recall"),
            "{shown}"
        );
        assert!(!home.path().join(".claude").exists());
    }

    #[test]
    fn a_flat_source_works_too() {
        let src = source();
        let flat = src.path().join("skills");
        let home = tempfile::tempdir().unwrap();
        let mut a = args(&flat);
        a.harnesses = vec!["claude".into()];
        let placed = install_in(&a, &flat, home.path()).unwrap();
        assert_eq!(placed.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_skill_is_refused_and_leaves_no_partial_install() {
        let src = source();
        std::os::unix::fs::symlink("/etc/passwd", src.path().join("skills/search/leak")).unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut a = args(src.path());
        a.harnesses = vec!["claude".into()];
        a.skills = vec!["search".into()];
        let err = install_in(&a, src.path(), home.path()).unwrap_err();
        assert!(err.to_string().contains("symbolic"), "{err}");
        let root = home.path().join(".claude/skills");
        assert!(!root.join("search").exists());
        assert!(!root.join(".search.tapesctl-staging").exists());
    }

    #[test]
    fn an_empty_source_is_its_own_error() {
        let a = SkillsInstallArgs {
            source: "  ".into(),
            ..args(Path::new("x"))
        };
        assert!(matches!(fetch(&a), Err(crate::Error::SkillsNoSource)));
    }

    #[test]
    fn the_verb_mounts_with_or_without_a_skills_cassette() {
        use clap::CommandFactory;
        // No cassette: a bare `skills` noun carries the verb.
        let bare = mount(crate::cli::Cli::command());
        let matches = bare
            .try_get_matches_from([
                "tapesctl",
                "skills",
                "install",
                "--source",
                "/s",
                "--harness",
                "claude",
            ])
            .unwrap();
        let parsed = matched(&matches).unwrap().unwrap();
        assert_eq!(parsed.source, "/s");
        assert_eq!(parsed.harnesses, ["claude"]);

        // A generated `skills` command keeps its own methods and gains the verb.
        let with_cassette = crate::cli::Cli::command()
            .subcommand(clap::Command::new(NOUN).subcommand(clap::Command::new("list-skills")));
        let mounted = mount(with_cassette);
        let noun = mounted.find_subcommand(NOUN).unwrap();
        assert!(noun.find_subcommand("list-skills").is_some());
        assert!(noun.find_subcommand(VERB).is_some());
    }
}
