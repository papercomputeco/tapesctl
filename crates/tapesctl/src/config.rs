//! The answers a user should only have to give once.
//!
//! Everything tapesctl talks to is named per invocation: `--tapes-url`, or
//! `TAPES_URL` in the environment. Both are fine for a one-off and wrong for a
//! habit — a shell without the export gets a tool that cannot list a session,
//! and, worse, one whose `--help` quietly stops listing the cassette commands
//! the deployment serves, because there is no server to discover them from.
//! Nothing is broken and nothing says so.
//!
//! So there is a third source, consulted last: `~/.tapes/config.toml`, written
//! by `tapesctl config set`. The precedence is the ordinary one —
//!
//! 1. `--tapes-url` on the command line,
//! 2. `TAPES_URL` in the environment,
//! 3. `tapes-url` in the config file.
//!
//! — and it is implemented by *being* that ordering rather than by
//! re-implementing it: the configured value is installed as clap's default for
//! the flag, and clap already ranks a default below an environment variable and
//! both below an explicit argument. See [`crate::parser`].
//!
//! # This does not weaken "never guess a host"
//!
//! With no source at all, tapesctl still refuses to run rather than assuming a
//! localhost port — see [`crate::Error::MissingTapesUrl`], which now teaches
//! this file as the durable fix. The invariant was never "always make the user
//! retype it"; it was "never send a capture somewhere nobody chose". A value in
//! this file was chosen, once, on purpose.
//!
//! # Where it lives
//!
//! `~/.tapes/config.toml`, beside the logs, the authored skills, and the
//! codex-app state this client already keeps in `~/.tapes` — one directory to
//! inspect, back up, or delete. It is deliberately not `$XDG_CONFIG_HOME`: a
//! user's tapesctl state is already in one place, and splitting configuration
//! away from it would mean two locations to explain and a migration for the
//! directory that already exists.
//!
//! The path is never resolved here. It comes from [`crate::machine::Machine`],
//! which is the crate's one ambient read, so a test writes into a temporary
//! home rather than over the developer's own configured server.

use std::path::Path;

use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use url::Url;

use crate::cli::ConfigCommand;
use crate::error::{Result, error};
use crate::machine::Machine;

/// The configured default server. The one key today.
pub const TAPES_URL_KEY: &str = "tapes-url";

/// Every key `tapesctl config` accepts, in the order help lists them.
pub const KEYS: [&str; 1] = [TAPES_URL_KEY];

/// The contents of `~/.tapes/config.toml`.
///
/// Unknown keys are ignored rather than refused: a file written by a newer
/// tapesctl must not stop an older one from starting, which is the whole reason
/// the file is a map of independent settings and not a versioned document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The server every command falls back to. TOML key `tapes-url`, spelled
    /// exactly like the flag and the `config set` key so there is one name to
    /// learn.
    #[serde(rename = "tapes-url", default, skip_serializing_if = "Option::is_none")]
    pub tapes_url: Option<String>,
}

impl Config {
    /// Read one key, as the string `config get` prints.
    ///
    /// `None` distinguishes "known key, not set" from an unknown key, which is
    /// an error.
    pub fn get(&self, key: &str) -> Result<Option<&str>> {
        match key {
            TAPES_URL_KEY => Ok(self.tapes_url.as_deref()),
            other => error::UnknownConfigKeySnafu {
                key: other,
                known: KEYS.join(", "),
            }
            .fail(),
        }
    }

    /// Set one key from its command-line spelling.
    pub fn set(&mut self, key: &str, value: String) -> Result<()> {
        match key {
            TAPES_URL_KEY => {
                self.tapes_url = Some(value);
                Ok(())
            }
            other => error::UnknownConfigKeySnafu {
                key: other,
                known: KEYS.join(", "),
            }
            .fail(),
        }
    }

    /// Every set key, in help order, for a `config get` with no key named.
    #[must_use]
    pub fn entries(&self) -> Vec<(&'static str, &str)> {
        KEYS.iter()
            .filter_map(|key| match *key {
                TAPES_URL_KEY => self
                    .tapes_url
                    .as_deref()
                    .map(|value| (TAPES_URL_KEY, value)),
                _ => None,
            })
            .collect()
    }
}

/// Read the configuration, failing on a file that exists and will not parse.
///
/// An absent file is an empty configuration, not an error: no configuration is
/// the normal state, and a first `config set` has to be able to run.
pub fn read(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(source) => return Err(source).context(error::ConfigReadSnafu { path }),
    };
    toml::from_str(&text).context(error::ConfigParseSnafu { path })
}

/// Read the configuration for the paths that cannot fail on it.
///
/// Command *resolution* runs before anything is dispatched and has no way to
/// report a problem except by refusing to run at all — and refusing to run is
/// the one outcome that would also block `tapesctl config` itself, which is the
/// only way to fix a broken file. So a bad file degrades to no configuration
/// here, loudly enough to find (`-v`) and harmlessly: the flag and the
/// environment still work, and the missing-URL error still teaches the fix.
/// [`read`] is what the config commands use, and it does fail.
#[must_use]
pub fn load_or_default(path: &Path) -> Config {
    match read(path) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "ignoring an unreadable config");
            Config::default()
        }
    }
}

/// Write the configuration, creating `~/.tapes` if this is the first key.
pub fn write(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context(error::ConfigWriteSnafu { path: parent })?;
    }
    let rendered = toml::to_string_pretty(config).context(error::ConfigRenderSnafu)?;
    std::fs::write(path, rendered).context(error::ConfigWriteSnafu { path })
}

/// Dispatch `tapesctl config <method>`.
///
/// The one ambient read, at the CLI boundary and nowhere else — the same shape
/// `plugin::run` takes, and for the same reason: `config set` writes a file
/// under a home directory, and a test that could reach the real one would
/// overwrite the developer's own configured server.
pub fn run(command: &ConfigCommand) -> Result<()> {
    run_in(command, Machine::resolve()?.tapes_config_path())
}

/// The body of [`run`], against an explicit path.
pub fn run_in(command: &ConfigCommand, path: &Path) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            // Printed whether or not the file exists: the question is where to
            // edit, and "nowhere yet" is not a useful answer.
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::Get(args) => {
            let config = read(path)?;
            match args.key.as_deref() {
                Some(key) => {
                    // A key that is known but unset prints nothing and
                    // succeeds, so `$(tapesctl config get tapes-url)` is empty
                    // rather than an error a script has to special-case.
                    if let Some(value) = config.get(key)? {
                        println!("{value}");
                    }
                }
                None => {
                    for (key, value) in config.entries() {
                        println!("{key} = {value}");
                    }
                }
            }
            Ok(())
        }
        ConfigCommand::Set(args) => {
            // Validated before it is stored: a value that is not a URL fails
            // here, once, naming the flag it would have fed — rather than on
            // every command run afterwards, where it would look like the
            // server was at fault.
            if args.key == TAPES_URL_KEY {
                let _ = Url::parse(&args.value).context(error::TapesUrlSnafu)?;
            }
            // Read before write: an unparseable file is an error rather than an
            // empty configuration to overwrite, so `config set` can never be the
            // command that loses the keys it was not asked about.
            let mut config = read(path)?;
            config.set(&args.key, args.value.clone())?;
            write(path, &config)?;
            println!("{} = {}", args.key, args.value);
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::{ConfigGetArgs, ConfigSetArgs};

    fn set(key: &str, value: &str) -> ConfigCommand {
        ConfigCommand::Set(ConfigSetArgs {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }

    #[test]
    fn setting_the_server_writes_it_where_the_machine_says() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".tapes").join("config.toml");

        run_in(&set(TAPES_URL_KEY, "http://tapes.example"), &path).unwrap();

        assert_eq!(
            read(&path).unwrap().tapes_url.as_deref(),
            Some("http://tapes.example"),
        );
        run_in(
            &ConfigCommand::Get(ConfigGetArgs {
                key: Some(TAPES_URL_KEY.to_owned()),
            }),
            &path,
        )
        .unwrap();
    }

    #[test]
    fn a_value_that_is_not_a_url_is_refused_before_it_is_stored() {
        // Stored, it would fail every later command in a way that looks like
        // the server's fault rather than the typo's.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        assert!(run_in(&set(TAPES_URL_KEY, "tapes.example"), &path).is_err());
        assert!(!path.exists(), "nothing should have been written");
    }

    #[test]
    fn setting_a_key_over_a_file_that_will_not_parse_is_refused() {
        // Otherwise the read would degrade to an empty configuration and the
        // write would drop every other key the user had set.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "tapes-url = ").unwrap();

        assert!(run_in(&set(TAPES_URL_KEY, "http://tapes.example"), &path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tapes-url = ");
    }

    #[test]
    fn the_path_is_printed_before_the_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(run_in(&ConfigCommand::Path, &path).is_ok());
        assert!(!path.exists());
    }

    #[test]
    fn getting_an_unset_key_succeeds_and_getting_an_unknown_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let get = |key: Option<&str>| {
            run_in(
                &ConfigCommand::Get(ConfigGetArgs {
                    key: key.map(str::to_owned),
                }),
                &path,
            )
        };
        assert!(get(Some(TAPES_URL_KEY)).is_ok());
        assert!(get(None).is_ok());
        assert!(get(Some("tapes-erl")).is_err());
    }

    #[test]
    fn an_absent_file_is_an_empty_configuration_rather_than_an_error() {
        // The state every user starts in, and the state `config set` has to be
        // able to run from.
        let dir = tempfile::tempdir().unwrap();
        let config = read(&dir.path().join("nothing.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn a_written_key_reads_back_under_the_name_it_was_set_by() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");

        let mut config = Config::default();
        config
            .set(TAPES_URL_KEY, "http://tapes.example".to_owned())
            .unwrap();
        write(&path, &config).unwrap();

        // The flag, the `config set` key, and the TOML key are one spelling.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tapes-url"), "got: {text}");

        assert_eq!(read(&path).unwrap(), config);
    }

    #[test]
    fn an_unknown_key_is_named_rather_than_silently_dropped() {
        let mut config = Config::default();
        let err = config.set("tapes-erl", "http://x".to_owned()).unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("tapes-erl"), "got: {rendered}");
        assert!(rendered.contains(TAPES_URL_KEY), "got: {rendered}");
        assert!(config.get("tapes-erl").is_err());
    }

    #[test]
    fn a_key_a_newer_build_added_does_not_stop_an_older_one_from_reading_the_file() {
        // The reason the file is a map of independent settings: a user who runs
        // two versions must not have one of them refuse to start.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "tapes-url = \"http://tapes.example\"\nsomething-later = 3\n",
        )
        .unwrap();

        let config = read(&path).unwrap();
        assert_eq!(config.tapes_url.as_deref(), Some("http://tapes.example"));
    }

    #[test]
    fn a_malformed_file_fails_the_config_commands_and_degrades_the_rest() {
        // Two different needs: `config set` must not read a broken file as
        // "empty" and rewrite over whatever was in it, while `tapesctl sessions
        // list` must still run and still be able to take its server from a flag.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "tapes-url = ").unwrap();

        assert!(read(&path).is_err());
        assert_eq!(load_or_default(&path), Config::default());
    }

    #[test]
    fn entries_lists_only_what_was_actually_set() {
        assert!(Config::default().entries().is_empty());
        let mut config = Config::default();
        config.set(TAPES_URL_KEY, "http://x".to_owned()).unwrap();
        assert_eq!(config.entries(), vec![(TAPES_URL_KEY, "http://x")]);
    }
}
