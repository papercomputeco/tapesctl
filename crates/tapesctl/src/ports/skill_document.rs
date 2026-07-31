//! The skill file format: frontmatter plus a markdown body.
//!
//! This is a *contract*, not an internal representation. `tapes skill generate`
//! wrote these files, `tapesctl skill sync` copies them byte-for-byte into an
//! agent's skills directory, and agent runtimes read the frontmatter. So the
//! renderer here reproduces the Go one exactly — same key order, same
//! `[a, b]` list spelling, same omit-when-empty rules — and the parser is
//! deliberately the same lenient line-splitter rather than a real YAML parser,
//! because a stricter reader would reject files the previous tool wrote.
//!
//! One deliberate difference: [`list`] sorts by file name. Go's `os.ReadDir`
//! returns entries already sorted, while `std::fs::read_dir` promises no order
//! at all — without the sort the same directory would print differently between
//! runs on the same machine.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use snafu::ResultExt;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{Result, error};
use crate::ports::skill_paths::write_contained;

/// The skill types the generator accepts.
pub const SKILL_TYPES: [&str; 3] = ["workflow", "domain-knowledge", "prompt-template"];

/// The version stamped on a freshly generated skill.
pub const INITIAL_VERSION: &str = "0.1.0";

/// Whether `value` is a recognized skill type.
#[must_use]
pub fn valid_skill_type(value: &str) -> bool {
    SKILL_TYPES.contains(&value)
}

/// A skill: frontmatter fields plus the markdown body.
///
/// Every field carries `serde(default)` because this type is also how the
/// extraction model's JSON is read, and a model that omits `tags` should
/// produce a skill with no tags rather than a failed parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Skill {
    /// Kebab-case identifier; also the file stem.
    #[serde(default)]
    pub name: String,
    /// Trigger description — when an agent should reach for this skill.
    #[serde(default)]
    pub description: String,
    /// Semver, `0.1.0` for a generated skill.
    #[serde(default)]
    pub version: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// One of [`SKILL_TYPES`].
    #[serde(default, rename = "type")]
    pub skill_type: String,
    /// The markdown body: the instructions themselves.
    #[serde(default)]
    pub content: String,
    /// Sessions the skill was extracted from.
    #[serde(default)]
    pub sessions: Vec<String>,
    /// When it was generated.
    ///
    /// Not deserialized: the generator stamps this itself, and accepting a
    /// model-supplied timestamp would mean parsing a field that is overwritten
    /// a line later anyway.
    #[serde(skip)]
    pub created_at: Option<OffsetDateTime>,
}

/// Where authored skills live.
#[must_use]
pub fn skills_dir(home: &Path) -> PathBuf {
    home.join(".tapes").join("skills")
}

/// Render a skill to its on-disk representation.
#[must_use]
pub fn render(skill: &Skill) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", skill.name));
    out.push_str(&format!("description: {}\n", skill.description));
    out.push_str(&format!("version: {}\n", skill.version));
    if !skill.tags.is_empty() {
        out.push_str(&format!("tags: [{}]\n", skill.tags.join(", ")));
    }
    if !skill.skill_type.is_empty() {
        out.push_str(&format!("type: {}\n", skill.skill_type));
    }
    if !skill.sessions.is_empty() {
        out.push_str(&format!("sessions: [{}]\n", skill.sessions.join(", ")));
    }
    // A timestamp that cannot be rendered is omitted rather than failed on —
    // the same branch an unset timestamp takes.
    if let Some(stamp) = skill.created_at.and_then(|at| at.format(&Rfc3339).ok()) {
        out.push_str(&format!("created_at: {stamp}\n"));
    }
    out.push_str("---\n\n");
    out.push_str(&skill.content);
    if !skill.content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Parse a skill file. Returns `None` when the document has no frontmatter,
/// which is how [`list`] skips files that are not skills.
#[must_use]
pub fn parse(document: &str) -> Option<Skill> {
    let rest = document.strip_prefix("---\n")?;
    let (frontmatter, body) = rest.split_once("\n---\n")?;

    let mut skill = Skill {
        content: body.trim().to_owned(),
        version: INITIAL_VERSION.to_owned(),
        ..Skill::default()
    };

    for line in frontmatter.split('\n') {
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" => skill.name = value.to_owned(),
            "description" => skill.description = value.to_owned(),
            "version" => skill.version = value.to_owned(),
            "type" => skill.skill_type = value.to_owned(),
            "tags" => skill.tags = parse_bracket_list(value),
            "sessions" => skill.sessions = parse_bracket_list(value),
            "created_at" => skill.created_at = OffsetDateTime::parse(value, &Rfc3339).ok(),
            _ => {}
        }
    }
    Some(skill)
}

/// Split `[a, b, c]` into its members.
fn parse_bracket_list(value: &str) -> Vec<String> {
    let inner = value.trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Keep the `.md` files, in name order.
///
/// Split out from [`list`] so the ordering is testable at all: it takes the
/// entries as an argument, where a test can hand it an unsorted list. Asserting
/// on a real directory instead would prove nothing, because `read_dir` is free
/// to return already-sorted entries — and on the common filesystems it usually
/// does, so such a test passes whether or not the sort is there.
#[must_use]
pub fn ordered_skill_files(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = files
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    files
}

/// Read every skill in `dir`, sorted by file name.
///
/// A missing directory is an empty list, not an error — it just means nothing
/// has been authored yet. A file that fails to read or parse is skipped rather
/// than failing the listing, so one malformed file cannot hide the rest.
pub fn list(dir: &Path) -> Result<Vec<Skill>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).context(error::SkillReadSnafu {
                path: dir.to_path_buf(),
            });
        }
    };

    let files = ordered_skill_files(
        entries
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
            .map(|entry| entry.path())
            .collect(),
    );

    let mut skills = Vec::new();
    for path in files {
        let Ok(document) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(mut skill) = parse(&document) else {
            continue;
        };
        // The file stem is the name of record: it is what `skill sync` takes
        // and what the file is addressed by, so a frontmatter `name` that
        // disagrees with the filename would be a name nothing can resolve.
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            skill.name = stem.to_owned();
        }
        skills.push(skill);
    }
    Ok(skills)
}

/// Write a skill into `dir`, returning the written path.
///
/// `base` is the root the write must stay beneath — see
/// [`crate::ports::skill_paths::contained_target`]. Mode `0600` matches what
/// `skill sync` writes: the two commands produce the same file, so they must
/// produce the same permissions.
pub fn write(skill: &Skill, dir: &Path, base: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).context(error::SkillWriteSnafu {
        path: dir.to_path_buf(),
    })?;
    let target = write_contained(dir, base, &skill.name, render(skill).as_bytes())?;
    Ok(target)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample() -> Skill {
        Skill {
            name: "debug-hooks".to_owned(),
            description: "Use when debugging React hooks".to_owned(),
            version: INITIAL_VERSION.to_owned(),
            tags: vec!["react".to_owned(), "debugging".to_owned()],
            skill_type: "workflow".to_owned(),
            content: "## Steps\n\n1. Look".to_owned(),
            sessions: vec!["s-1".to_owned()],
            created_at: OffsetDateTime::parse("2026-07-31T12:00:00Z", &Rfc3339).ok(),
        }
    }

    #[test]
    fn the_rendered_frontmatter_matches_the_format_agents_read() {
        // Key order and list spelling are the contract; an agent runtime and
        // `skill sync` both read what this writes.
        assert_eq!(
            render(&sample()),
            "---\n\
             name: debug-hooks\n\
             description: Use when debugging React hooks\n\
             version: 0.1.0\n\
             tags: [react, debugging]\n\
             type: workflow\n\
             sessions: [s-1]\n\
             created_at: 2026-07-31T12:00:00Z\n\
             ---\n\n\
             ## Steps\n\n1. Look\n",
        );
    }

    #[test]
    fn empty_optional_fields_are_omitted_rather_than_rendered_blank() {
        let skill = Skill {
            name: "bare".to_owned(),
            description: "d".to_owned(),
            version: INITIAL_VERSION.to_owned(),
            content: "body\n".to_owned(),
            ..Skill::default()
        };
        let rendered = render(&skill);
        assert!(!rendered.contains("tags:"), "got: {rendered}");
        assert!(!rendered.contains("type:"), "got: {rendered}");
        assert!(!rendered.contains("sessions:"), "got: {rendered}");
        assert!(!rendered.contains("created_at:"), "got: {rendered}");
    }

    #[test]
    fn a_body_without_a_trailing_newline_gets_one() {
        let skill = Skill {
            content: "no newline".to_owned(),
            ..Skill::default()
        };
        assert!(render(&skill).ends_with("no newline\n"));
    }

    #[test]
    fn rendering_and_parsing_round_trip() {
        let parsed = parse(&render(&sample())).unwrap();
        assert_eq!(parsed, sample());
    }

    #[test]
    fn a_document_without_frontmatter_is_not_a_skill() {
        assert!(parse("just markdown\n").is_none());
        assert!(parse("---\nname: x\nno closing delimiter\n").is_none());
    }

    #[test]
    fn a_parsed_skill_defaults_to_the_initial_version() {
        let skill = parse("---\nname: x\n---\n\nbody\n").unwrap();
        assert_eq!(skill.version, INITIAL_VERSION);
    }

    #[test]
    fn an_empty_bracket_list_is_no_tags_rather_than_one_empty_tag() {
        let skill = parse("---\nname: x\ntags: []\n---\n\nbody\n").unwrap();
        assert!(skill.tags.is_empty());
    }

    #[test]
    fn entries_are_ordered_by_name_whatever_order_the_filesystem_gave_them() {
        // The guarantee `list` relies on, asserted where an unsorted input can
        // actually be supplied. A real directory would not prove it: read_dir
        // commonly returns sorted entries already, so the assertion would hold
        // with the sort deleted.
        let unsorted = vec![
            PathBuf::from("/s/zebra.md"),
            PathBuf::from("/s/alpha.md"),
            PathBuf::from("/s/middle.md"),
        ];
        assert_eq!(
            ordered_skill_files(unsorted),
            vec![
                PathBuf::from("/s/alpha.md"),
                PathBuf::from("/s/middle.md"),
                PathBuf::from("/s/zebra.md"),
            ],
        );
    }

    #[test]
    fn non_markdown_entries_are_dropped_before_ordering() {
        let mixed = vec![
            PathBuf::from("/s/notes.txt"),
            PathBuf::from("/s/skill.md"),
            PathBuf::from("/s/no-extension"),
        ];
        assert_eq!(
            ordered_skill_files(mixed),
            vec![PathBuf::from("/s/skill.md")],
        );
    }

    #[test]
    fn listing_is_sorted_and_names_come_from_the_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        for stem in ["zebra", "alpha", "middle"] {
            std::fs::write(
                dir.path().join(format!("{stem}.md")),
                "---\nname: wrong-name\ndescription: d\n---\n\nbody\n",
            )
            .unwrap();
        }

        let skills = list(dir.path()).unwrap();

        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn listing_skips_non_skills_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("good.md"), "---\nname: g\n---\n\nb\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a skill").unwrap();
        std::fs::write(dir.path().join("broken.md"), "no frontmatter").unwrap();
        std::fs::create_dir(dir.path().join("subdir.md")).unwrap();

        let skills = list(dir.path()).unwrap();

        assert_eq!(skills.len(), 1, "got: {skills:?}");
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn a_missing_directory_lists_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("never-created");
        assert!(list(&absent).unwrap().is_empty());
    }

    #[test]
    fn a_written_skill_reads_back_as_itself() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("skills");

        let path = write(&sample(), &dir, base.path()).unwrap();

        assert_eq!(path.file_name().unwrap(), "debug-hooks.md");
        let listed = list(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].description, sample().description);
    }

    #[cfg(unix)]
    #[test]
    fn a_written_skill_is_owner_only_like_the_one_sync_writes() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("skills");
        let path = write(&sample(), &dir, base.path()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got: {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_skills_directory_is_refused_on_write() {
        // Same containment guarantee `skill sync` has: generate must not be
        // the softer door into the same directory.
        let base = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dir = base.path().join("skills");
        std::os::unix::fs::symlink(elsewhere.path(), &dir).unwrap();

        let err = write(&sample(), &dir, base.path()).unwrap_err();

        assert!(err.to_string().contains("resolves outside"), "got: {err}");
        assert!(
            std::fs::read_dir(elsewhere.path())
                .unwrap()
                .next()
                .is_none(),
            "nothing may land behind the symlink",
        );
    }

    #[test]
    fn only_the_three_documented_types_are_valid() {
        assert!(valid_skill_type("workflow"));
        assert!(valid_skill_type("domain-knowledge"));
        assert!(valid_skill_type("prompt-template"));
        assert!(!valid_skill_type("Workflow"));
        assert!(!valid_skill_type("anything-else"));
    }
}
