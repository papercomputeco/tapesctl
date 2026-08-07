//! The locations an install reads, writes, and executes — resolved from the
//! machine exactly once.
//!
//! Every install path here writes somewhere durable: a harness's own
//! `config.toml`, a plugin tree under someone's home, and — the one that is
//! not a file at all — the `codex` binary the plugin manager is driven
//! through. All three used to be recovered wherever they were needed, from
//! `dirs::home_dir`, from `$CODEX_HOME`, and from `PATH`. That is what made
//! the surface untestable in the strict sense: a test could pass a temporary
//! home and still reach a resolver that answered with the developer's own.
//!
//! It is not hypothetical. The `codex` program defaulted to that bare name, so
//! the install tests spawned whichever `codex` the developer had on `PATH`,
//! and that CLI wrote a marketplace registration into the developer's real
//! `~/.codex/config.toml` pointing at a temporary directory the test was about
//! to delete. The suite quietly broke the machine it ran on, and the damage
//! outlived the run.
//!
//! So the three are gathered into one value, resolved by [`Machine::resolve`]
//! at the CLI boundary and passed down from there. Nothing below that boundary
//! consults the environment, which is the property worth having: a caller
//! holding a [`Machine`] built with [`Machine::at`] can write only where that
//! value points.
//!
//! # Why not steer the environment instead
//!
//! Pointing `HOME` and `$CODEX_HOME` at a temporary directory would contain
//! the damage without restructuring anything. It is rejected because
//! environment variables are process-global while tests are not: one test
//! setting them changes what every concurrently running test resolves, and a
//! test that forgets is failed by nothing — it silently escapes. Passing the
//! value makes the escape a compile error instead.

use std::path::{Path, PathBuf};

use crate::error::Result;

/// Where this machine keeps what an install touches.
///
/// Construct it with [`Machine::resolve`] in production and [`Machine::at`]
/// everywhere else. There is deliberately no `Default`: a default would have
/// to name the real home, which is the bug this type exists to make
/// unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    home: PathBuf,
    codex_config_path: PathBuf,
    codex_program: PathBuf,
}

impl Machine {
    /// Resolve every location from the ambient environment.
    ///
    /// The only function in the crate that reads `dirs::home_dir` or
    /// `$CODEX_HOME`, or leaves a program name for `PATH` to resolve, and it
    /// is called from the CLI boundary alone.
    ///
    /// # Panics
    ///
    /// Under `cfg(test)`, always — see the guard below. Production builds are
    /// unaffected.
    // The `panic` lint bans exactly this in production code, and the guard is
    // compiled out of production code; under `cfg(test)` a panic is the
    // intended answer, so the allow is narrowed to that build rather than
    // written over the function unconditionally.
    #[cfg_attr(test, allow(clippy::panic))]
    pub fn resolve() -> Result<Self> {
        // The guard the codex-app install tests needed and did not have. A
        // unit test that reaches ambient resolution is a test that can write
        // outside its temporary root, so it fails here, loudly, naming the
        // constructor it should have used — rather than succeeding and leaving
        // the developer to find the damage in their own `config.toml` days
        // later.
        //
        // A runtime panic rather than `#[cfg(not(test))]` on the function
        // itself, because the CLI dispatch that calls this is compiled under
        // `cfg(test)` too: removing the function would fail the build instead
        // of the offending test. Integration tests link the library without
        // `cfg(test)` and are not covered by this; none reach it today, and
        // the injection below is what actually holds them.
        #[cfg(test)]
        panic!(
            "Machine::resolve reads the developer's real home, $CODEX_HOME, \
             and PATH; a test must build a Machine::at a temporary root instead"
        );

        #[cfg(not(test))]
        {
            use snafu::OptionExt as _;

            use crate::error::error;

            let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
            let codex_config_path = crate::codex_app::codex_config_path(
                &home,
                std::env::var_os("CODEX_HOME").as_deref(),
            );
            Ok(Self {
                home,
                codex_config_path,
                // Bare, so the user's `PATH` resolves the same binary they
                // would have run themselves.
                codex_program: PathBuf::from("codex"),
            })
        }
    }

    /// A machine whose locations are all named outright.
    ///
    /// Every argument is required, including `codex_program`: a caller that
    /// could omit it would fall back to `PATH` and spawn the real CLI, which
    /// is precisely the defect. Point it at a path that does not exist to
    /// assert against an install that finds no CLI, or at a shim to observe
    /// what the installer would have run.
    #[must_use]
    pub fn at(
        home: impl Into<PathBuf>,
        codex_config_path: impl Into<PathBuf>,
        codex_program: impl Into<PathBuf>,
    ) -> Self {
        Self {
            home: home.into(),
            codex_config_path: codex_config_path.into(),
            codex_program: codex_program.into(),
        }
    }

    /// The user's home directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The Codex `config.toml` an install patches and an uninstall cleans.
    #[must_use]
    pub fn codex_config_path(&self) -> &Path {
        &self.codex_config_path
    }

    /// The `codex` binary the plugin manager is driven through.
    #[must_use]
    pub fn codex_program(&self) -> &Path {
        &self.codex_program
    }

    /// The same machine driven through a different `codex`.
    #[must_use]
    pub fn with_codex_program(mut self, codex_program: impl Into<PathBuf>) -> Self {
        self.codex_program = codex_program.into();
        self
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_reports_the_locations_it_was_built_with() {
        let machine = Machine::at("/home/someone", "/elsewhere/config.toml", "/nowhere/codex");
        assert_eq!(machine.home(), Path::new("/home/someone"));
        assert_eq!(
            machine.codex_config_path(),
            Path::new("/elsewhere/config.toml")
        );
        assert_eq!(machine.codex_program(), Path::new("/nowhere/codex"));
    }

    /// The guard that would have caught the defect this type exists for: a
    /// test reaching ambient resolution fails instead of writing to the
    /// developer's real Codex configuration.
    #[test]
    #[should_panic(expected = "temporary root")]
    fn resolving_from_the_ambient_environment_is_refused_under_test() {
        let _ = Machine::resolve();
    }
}
