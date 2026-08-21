//! Surgically removes the installer-written sentinel block from shell rc files.
//!
//! `install.sh` wraps everything it adds to a user's rc file between the
//! [`BLOCK_BEGIN`] and [`BLOCK_END`] marker lines. That sentinel contract is
//! what makes touching user dotfiles defensible: removal deletes exactly the
//! marked block and preserves every byte outside it — no regex over user
//! content, no whole-file reformatting.
//!
//! The marker strings are deliberately duplicated in the bash installer
//! (single-sourcing them through the binary would put binary execution on the
//! installer's critical path before PATH exists); a consistency test reads the
//! script so drift turns into a red build instead of a silent contract break.
//!
//! The markers name tapesctl. paperctl writes a block with the same glyph and
//! different text into the same rc files, and both removers compare whole
//! lines — so the two blocks coexist and neither tool disturbs the other's.

use std::path::{Path, PathBuf};

use snafu::{ResultExt, Snafu};

/// First line of the installer-written rc block. Must match `install.sh`
/// byte-for-byte.
pub const BLOCK_BEGIN: &str = "# > [|o=o|] > tapesctl path > [|o=o|] >";

/// Last line of the installer-written rc block. Must match `install.sh`
/// byte-for-byte.
pub const BLOCK_END: &str = "# < [|o=o|] < tapesctl path < [|o=o|] <";

/// The rc files the installer may have written a block into, resolved against
/// the current user's home directory and `XDG_CONFIG_HOME`.
///
/// Empty when no home directory can be resolved — with no home there is no rc
/// file the installer could have edited either. The environment is read here,
/// at the command boundary; the path construction itself is the injectable
/// [`rc_files_under`].
#[must_use]
pub fn known_rc_files() -> Vec<PathBuf> {
    dirs::home_dir()
        .map(|home| {
            let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
            rc_files_under(&home, xdg.as_deref())
        })
        .unwrap_or_default()
}

/// The rc-file set under an explicit home directory: `.bashrc`, `.zshrc`, and
/// fish's `config.fish` — the same set the installer targets.
///
/// The fish path honors `xdg_config_home` exactly like the installer's
/// `${XDG_CONFIG_HOME:-$HOME/.config}` expression (an empty value falls back
/// like an unset one), so uninstall removes the block from the file the
/// installer actually wrote. Pure path construction; nothing is checked for
/// existence. Split out so tests can drive a tempdir home.
#[must_use]
pub fn rc_files_under(home: &Path, xdg_config_home: Option<&Path>) -> Vec<PathBuf> {
    let config_dir = xdg_config_home
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| home.join(".config"), Path::to_path_buf);
    vec![
        home.join(".bashrc"),
        home.join(".zshrc"),
        config_dir.join("fish").join("config.fish"),
    ]
}

/// What [`remove_block`] did to the rc file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// A block was found and removed; the file was rewritten with every byte
    /// outside the markers intact.
    Removed,
    /// The file does not exist or contains no block; it was not rewritten at
    /// all.
    NoBlock,
    /// A begin marker with no matching end marker. Rewriting would drop
    /// everything after the orphaned marker, so the file was left untouched —
    /// callers surface this so the user knows the block is still there.
    Malformed,
}

/// Remove the sentinel block from the rc file at `rc_path`.
///
/// The file is only rewritten on [`RemoveOutcome::Removed`]; content outside
/// the markers survives byte-for-byte.
pub fn remove_block(rc_path: &Path) -> Result<RemoveOutcome, RemoveBlockError> {
    use remove_block_error::*;

    let contents = match std::fs::read(rc_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RemoveOutcome::NoBlock),
        Err(source) => return Err(source).context(ReadSnafu { path: rc_path }),
    };
    let remaining = match strip_block(&contents) {
        StripOutcome::Stripped(remaining) => remaining,
        StripOutcome::NoBlock => return Ok(RemoveOutcome::NoBlock),
        StripOutcome::Malformed => return Ok(RemoveOutcome::Malformed),
    };
    std::fs::write(rc_path, remaining).context(WriteSnafu { path: rc_path })?;
    Ok(RemoveOutcome::Removed)
}

/// What [`strip_block`] found in raw file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripOutcome {
    /// A well-formed block was present; the payload is the file with the block
    /// removed and every byte outside it intact.
    Stripped(Vec<u8>),
    /// No block is present — the signal that the file must not be rewritten.
    NoBlock,
    /// A begin marker without a matching end marker — the file must not be
    /// rewritten (see [`RemoveOutcome::Malformed`]).
    Malformed,
}

/// Pure core of [`remove_block`]: strip the marked block from raw file
/// contents. Operates on bytes so rc files with non-UTF-8 content pass through
/// undamaged.
#[must_use]
pub fn strip_block(contents: &[u8]) -> StripOutcome {
    let begin = BLOCK_BEGIN.as_bytes();
    let end = BLOCK_END.as_bytes();
    let mut remaining = Vec::with_capacity(contents.len());
    let mut inside_block = false;
    let mut removed = false;
    let mut rest = contents;
    while !rest.is_empty() {
        let line_len = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |i| i + 1);
        let (line, tail) = rest.split_at(line_len);
        rest = tail;
        // Exact whole-line comparison against the markers — the trailing
        // newline is the only byte stripped before comparing, so nothing
        // resembling a pattern ever runs over user content, and a paperctl
        // block's marker line (same glyph, different text) never matches.
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if inside_block {
            if body == end {
                inside_block = false;
            }
            continue;
        }
        if body == begin {
            inside_block = true;
            removed = true;
            continue;
        }
        remaining.extend_from_slice(line);
    }
    // A begin marker with no matching end marker means the block is malformed;
    // refusing to rewrite preserves the user's bytes instead of dropping
    // everything after the orphaned marker.
    match (removed, inside_block) {
        (true, false) => StripOutcome::Stripped(remaining),
        (true, true) => StripOutcome::Malformed,
        (false, _) => StripOutcome::NoBlock,
    }
}

/// Failure modes for [`remove_block`].
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
#[non_exhaustive]
pub enum RemoveBlockError {
    /// Reading the rc file failed (a missing file is
    /// [`RemoveOutcome::NoBlock`], not this variant).
    #[snafu(display("could not read rc file '{}'", path.display()))]
    Read {
        /// The rc file being read.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// Writing the edited rc file back failed (read-only file, full disk).
    #[snafu(display("could not write rc file '{}'", path.display()))]
    Write {
        /// The rc file being rewritten.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn rc_block_removal_preserves_bytes_outside_markers() {
        // Given an rc file whose tapesctl sentinel block sits between user
        // content: aliases and another tool's block before it, more user
        // content after it
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        let before = "# user aliases\n\
                      alias ll='ls -al'\n\
                      # >>> conda initialize >>>\n\
                      . /opt/conda/etc/profile.d/conda.sh\n\
                      # <<< conda initialize <<<\n\
                      export EDITOR=vim\n";
        let after = "\n# added by another tool\neval \"$(direnv hook zsh)\"\n";
        let block = format!("{BLOCK_BEGIN}\nexport PATH=\"$HOME/.local/bin:$PATH\"\n{BLOCK_END}\n");
        std::fs::write(&rc, format!("{before}{block}{after}")).unwrap();

        // When the sentinel block is removed
        let removed = remove_block(&rc).unwrap();

        // Then a block was found, and every byte outside the markers survives
        // untouched
        assert_eq!(removed, RemoveOutcome::Removed);
        let remaining = std::fs::read(&rc).unwrap();
        assert_eq!(remaining, format!("{before}{after}").into_bytes());
    }

    #[test]
    fn a_paperctl_block_in_the_same_file_is_left_alone() {
        // Given an rc file carrying both CLIs' blocks — the state of anyone
        // who installed paperctl and tapesctl. The glyph is shared; only the
        // marker text differs, and removal compares whole lines.
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        let paper_block = "# > [|o=o|] > paper shell init script > [|o=o|] >\n\
                           export PATH=\"$HOME/.local/bin:$PATH\"\n\
                           # < [|o=o|] < paper shell init < [|o=o|] <\n";
        let ours = format!("{BLOCK_BEGIN}\nexport PATH=\"$HOME/.local/bin:$PATH\"\n{BLOCK_END}\n");
        std::fs::write(&rc, format!("{paper_block}{ours}")).unwrap();

        // When tapesctl removes its block
        assert_eq!(remove_block(&rc).unwrap(), RemoveOutcome::Removed);

        // Then paperctl's block survives byte-for-byte
        assert_eq!(std::fs::read(&rc).unwrap(), paper_block.as_bytes());
    }

    #[test]
    fn malformed_block_reports_malformed_and_leaves_file_untouched() {
        // Given an rc file whose block has a begin marker but no end — a
        // hand-edited or truncated file
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        let contents = format!("# user content\n{BLOCK_BEGIN}\nexport PATH=oops\n");
        std::fs::write(&rc, &contents).unwrap();

        // When removal runs
        let outcome = remove_block(&rc).unwrap();

        // Then the malformed state is reported and the file's bytes are
        // untouched — rewriting would drop everything after the marker
        assert_eq!(outcome, RemoveOutcome::Malformed);
        assert_eq!(std::fs::read(&rc).unwrap(), contents.into_bytes());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = remove_block(&dir.path().join("nothing-here")).unwrap();
        assert_eq!(outcome, RemoveOutcome::NoBlock);
    }

    #[test]
    fn non_utf8_content_outside_the_block_survives() {
        // rc files are shell, not necessarily UTF-8 — a latin-1 comment or a
        // stray byte must pass through a removal undamaged, which is why the
        // stripper works on bytes.
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let mut contents = b"# caf\xe9 alias\n".to_vec();
        contents
            .extend_from_slice(format!("{BLOCK_BEGIN}\nexport PATH=x\n{BLOCK_END}\n").as_bytes());
        contents.extend_from_slice(b"# \xff\xfe tail\n");
        std::fs::write(&rc, &contents).unwrap();

        assert_eq!(remove_block(&rc).unwrap(), RemoveOutcome::Removed);

        let mut expected = b"# caf\xe9 alias\n".to_vec();
        expected.extend_from_slice(b"# \xff\xfe tail\n");
        assert_eq!(std::fs::read(&rc).unwrap(), expected);
    }

    #[test]
    fn rc_files_honor_xdg_config_home_for_fish() {
        // Given a home dir and a distinct XDG_CONFIG_HOME — the state in which
        // the installer writes fish's config under the XDG dir
        let home = Path::new("/home/u");
        let xdg = Path::new("/xdg/config");

        // When the rc-file set is derived
        let files = rc_files_under(home, Some(xdg));

        // Then the fish path lives under XDG_CONFIG_HOME while bash/zsh stay
        // under home
        assert!(files.contains(&home.join(".bashrc")));
        assert!(files.contains(&home.join(".zshrc")));
        assert!(files.contains(&xdg.join("fish").join("config.fish")));
        assert!(!files.iter().any(|f| f.starts_with(home.join(".config"))));
    }

    #[test]
    fn rc_files_fall_back_to_dot_config_when_xdg_unset_or_empty() {
        // Given no XDG_CONFIG_HOME (or an empty one — the shell `:-`
        // expansion treats both alike)
        let home = Path::new("/home/u");
        for xdg in [None, Some(Path::new(""))] {
            let files = rc_files_under(home, xdg);
            assert!(
                files.contains(&home.join(".config").join("fish").join("config.fish")),
                "fish path missing for xdg={xdg:?}: {files:?}"
            );
        }
    }

    /// The value of a single-quoted shell assignment `name='...'`, from the
    /// first line that makes one.
    fn shell_assignment(contents: &str, name: &str) -> Option<String> {
        contents.lines().find_map(|line| {
            let rest = line.trim_start().strip_prefix(name)?.strip_prefix("='")?;
            rest.strip_suffix('\'').map(str::to_owned)
        })
    }

    #[test]
    fn sentinel_markers_match_the_installer_script() {
        // Given the installer — the one writer of the sentinel block — it must
        // agree with the Rust remover byte-for-byte.
        //
        // Equality against the parsed assignment, NOT `contents.contains(..)`:
        // a substring check passes for every drift that matters. Append one
        // space to the shell's marker and `contains` still succeeds, while
        // both the shell's whole-line awk and the Rust stripper below stop
        // matching it — the exact state that leaves a block in a user's
        // dotfile that neither tool can ever remove. It would also happily
        // match a stale marker sitting in a comment while the live assignment
        // had moved on.
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let script = repo_root.join("install.sh");
        let contents = std::fs::read_to_string(&script)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", script.display()));

        let begin = shell_assignment(&contents, "RC_BLOCK_BEGIN")
            .unwrap_or_else(|| panic!("{} has no RC_BLOCK_BEGIN='..'", script.display()));
        let end = shell_assignment(&contents, "RC_BLOCK_END")
            .unwrap_or_else(|| panic!("{} has no RC_BLOCK_END='..'", script.display()));

        assert_eq!(
            begin,
            BLOCK_BEGIN,
            "begin marker drifted from {}",
            script.display()
        );
        assert_eq!(
            end,
            BLOCK_END,
            "end marker drifted from {}",
            script.display()
        );
    }

    #[test]
    fn the_marker_pin_catches_a_trailing_space() {
        // The pin's own regression test: the drift it exists to catch is
        // invisible to a substring check, so prove the parse-and-compare
        // notices it.
        let drifted = "RC_BLOCK_BEGIN='# > [|o=o|] > tapesctl path > [|o=o|] > '\n";
        let parsed = shell_assignment(drifted, "RC_BLOCK_BEGIN").unwrap();
        assert!(
            drifted.contains(BLOCK_BEGIN),
            "a substring check would pass here"
        );
        assert_ne!(parsed, BLOCK_BEGIN, "the parse-and-compare must not");
    }
}
