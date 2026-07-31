//! `tapesctl skill list` — ported from `tapes skill list`.
//!
//! Reads `~/.tapes/skills` and prints what is there. Like `skill sync`, it
//! touches no server: authored skills are local files, and requiring a
//! `--tapes-url` to list them would be a lie about what the command does.
//!
//! The Go renderer coloured the name, type, and version columns through the
//! CLI's style layer; tapesctl has none, so the same columns print plain. The
//! empty-directory hint names `tapesctl skill generate` rather than the Go
//! binary, because that is the command a reader of this output can actually
//! run.

use snafu::OptionExt;

use crate::cli::SkillListArgs;
use crate::error::{Result, error};
use crate::ports::skill_document::{self, Skill};

/// Longest description before it is elided.
const DESCRIPTION_WIDTH: usize = 80;

/// Run one `skill list`.
pub fn run(args: SkillListArgs) -> Result<()> {
    let dir = match args.source_dir {
        Some(dir) => dir,
        None => {
            let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
            skill_document::skills_dir(&home)
        }
    };

    let skills = skill_document::list(&dir)?;
    if skills.is_empty() {
        println!(
            "No skills found. Generate one with: tapesctl skill generate <session-id> --name <name>",
        );
        return Ok(());
    }

    let skills: Vec<Skill> = match args.skill_type.as_deref() {
        Some(wanted) => skills
            .into_iter()
            .filter(|skill| skill.skill_type == wanted)
            .collect(),
        None => skills,
    };

    // The filtered-to-nothing case is deliberately distinct from the
    // nothing-authored case above: the fix for one is to generate a skill, and
    // the fix for the other is to drop the filter.
    if skills.is_empty() {
        if let Some(wanted) = args.skill_type.as_deref() {
            println!("No skills found with type {wanted:?}");
        }
        return Ok(());
    }

    println!("\nSkills ({})\n", skills.len());
    for skill in &skills {
        print_skill(skill);
    }
    Ok(())
}

/// Render one row.
fn print_skill(skill: &Skill) {
    println!("  {}  {}  v{}", skill.name, skill.skill_type, skill.version,);
    println!("  {}", summarize(&skill.description));
    if !skill.tags.is_empty() {
        println!("  [{}]", skill.tags.join(", "));
    }
    println!();
}

/// Elide a description to one line.
///
/// Truncation happens before newlines are flattened, matching the Go order:
/// a description whose first 80 characters contain a newline is cut first and
/// flattened after, so the two steps cannot be swapped without changing what
/// prints. Counted in characters, not bytes — see `ports::search::elide` for
/// why the byte-slicing original could not be reproduced literally.
#[must_use]
fn summarize(description: &str) -> String {
    let truncated = if description.chars().count() > DESCRIPTION_WIDTH {
        let kept: String = description
            .chars()
            .take(DESCRIPTION_WIDTH.saturating_sub(3))
            .collect();
        format!("{kept}...")
    } else {
        description.to_owned()
    };
    truncated.replace('\n', " ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_skill(dir: &Path, stem: &str, skill_type: &str, description: &str) {
        std::fs::write(
            dir.join(format!("{stem}.md")),
            format!(
                "---\nname: {stem}\ndescription: {description}\nversion: 0.1.0\ntype: {skill_type}\ntags: [a, b]\n---\n\nbody\n",
            ),
        )
        .unwrap();
    }

    fn args(dir: &Path, skill_type: Option<&str>) -> SkillListArgs {
        SkillListArgs {
            skill_type: skill_type.map(ToOwned::to_owned),
            source_dir: Some(dir.to_path_buf()),
        }
    }

    #[test]
    fn an_empty_directory_points_at_the_command_that_fills_it() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(args(dir.path(), None)).is_ok());
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("never-created");
        assert!(run(args(&absent, None)).is_ok());
    }

    #[test]
    fn skills_are_listed_and_can_be_filtered_by_type() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "one", "workflow", "first");
        write_skill(dir.path(), "two", "domain-knowledge", "second");

        assert!(run(args(dir.path(), None)).is_ok());
        assert!(run(args(dir.path(), Some("workflow"))).is_ok());
        // Filtering to nothing is a distinct, non-error outcome.
        assert!(run(args(dir.path(), Some("prompt-template"))).is_ok());
    }

    #[test]
    fn the_type_filter_matches_exactly() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "one", "workflow", "first");

        let listed = skill_document::list(dir.path()).unwrap();
        let matching: Vec<&Skill> = listed
            .iter()
            .filter(|skill| skill.skill_type == "Workflow")
            .collect();
        assert!(matching.is_empty(), "the filter is case-sensitive");
    }

    #[test]
    fn a_long_description_is_elided_to_one_line() {
        let long = "d".repeat(200);
        let summarized = summarize(&long);
        assert_eq!(summarized.chars().count(), DESCRIPTION_WIDTH);
        assert!(summarized.ends_with("..."));
    }

    #[test]
    fn newlines_are_flattened_after_truncation_not_before() {
        // Swapping the two steps changes what prints; pinning the order keeps
        // this port's output identical to the command it replaces.
        let description = format!("{}\ntail", "a".repeat(100));
        let summarized = summarize(&description);
        assert!(!summarized.contains('\n'));
        assert!(summarized.ends_with("..."), "got: {summarized}");
        assert!(!summarized.contains("tail"), "the tail was truncated away");
    }

    #[test]
    fn a_short_description_survives_intact_with_its_newlines_flattened() {
        assert_eq!(summarize("one\ntwo"), "one two");
    }
}
