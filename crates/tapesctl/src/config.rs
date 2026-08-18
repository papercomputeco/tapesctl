//! The answers a user should only have to give once.
//!
//! The read API and ingest API are named independently: `--api-url` / `TAPES_API_URL`
//! and `--ingest-url` / `TAPES_INGEST_URL`. Both are fine for a one-off and wrong for a
//! habit — a shell without the export gets a tool that cannot list a session,
//! and, worse, one whose `--help` quietly stops listing the cassette commands
//! the deployment serves, because there is no server to discover them from.
//! Nothing is broken and nothing says so.
//!
//! So there is a third source, consulted last: `~/.tapes/config.toml`, written
//! by `tapesctl config set`. The precedence is the ordinary one —
//!
//! 1. its command-line flag,
//! 2. its environment variable,
//! 3. its config-file key.
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
//! `~/.tapes/config.toml`, beside the logs and the
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

/// The configured default read API.
pub const TAPES_URL_KEY: &str = "tapes-url";
/// The configured default ingest API.
pub const INGEST_URL_KEY: &str = "ingest-url";

/// Every key `tapesctl config` accepts, in the order help lists them.
pub const KEYS: [&str; 2] = [TAPES_URL_KEY, INGEST_URL_KEY];

/// The contents of `~/.tapes/config.toml`.
///
/// Unknown keys are ignored rather than refused: a file written by a newer
/// tapesctl must not stop an older one from starting, which is the whole reason
/// the file is a map of independent settings and not a versioned document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The read API every query falls back to.
    #[serde(rename = "tapes-url", default, skip_serializing_if = "Option::is_none")]
    pub tapes_url: Option<String>,
    /// The ingest API every capture command falls back to.
    #[serde(
        rename = "ingest-url",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ingest_url: Option<String>,
}

/// Refuse a key this build does not have.
///
/// Separate from [`Config`] because the write path needs it without a model:
/// `config set` never deserializes the file it is about to edit, so the key
/// check cannot ride along on a struct field.
pub fn check_key(key: &str) -> Result<()> {
    if KEYS.contains(&key) {
        return Ok(());
    }
    error::UnknownConfigKeySnafu {
        key,
        known: KEYS.join(", "),
    }
    .fail()
}

impl Config {
    /// Read one key, as the string `config get` prints.
    ///
    /// `None` distinguishes "known key, not set" from an unknown key, which is
    /// an error.
    pub fn get(&self, key: &str) -> Result<Option<&str>> {
        check_key(key)?;
        match key {
            TAPES_URL_KEY => Ok(self.tapes_url.as_deref()),
            INGEST_URL_KEY => Ok(self.ingest_url.as_deref()),
            _ => Ok(None), // check_key above rejects unknown keys.
        }
    }

    /// Set one key from its command-line spelling, in memory.
    ///
    /// The in-memory half only. Persisting goes through [`write_key`], which
    /// edits the file rather than re-serializing this model — see there for why
    /// that distinction is the whole point.
    pub fn set(&mut self, key: &str, value: String) -> Result<()> {
        check_key(key)?;
        match key {
            TAPES_URL_KEY => self.tapes_url = Some(value),
            INGEST_URL_KEY => self.ingest_url = Some(value),
            _ => {} // check_key above rejects unknown keys.
        }
        Ok(())
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
                INGEST_URL_KEY => self
                    .ingest_url
                    .as_deref()
                    .map(|value| (INGEST_URL_KEY, value)),
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

/// Set one key in the file, leaving everything else in it exactly as it was.
///
/// # Why this is not "serialize [`Config`] back"
///
/// Because [`Config`] is deliberately not the whole file. Reading tolerates
/// keys this build has never heard of, so that a file written by a newer
/// tapesctl cannot stop an older one from starting — but a write that rendered
/// the model would emit only the fields the model has, and every one of those
/// tolerated keys would be gone. The forward-compatibility promise would then
/// hold for reading and break for writing, which is the same as not holding: a
/// user running two versions would silently lose the newer one's settings the
/// first time the older one set anything.
///
/// So the document is parsed, one key is replaced in place, and the rest —
/// unknown keys, ordering, comments, the user's own formatting — is carried
/// through untouched.
pub fn write_key(path: &Path, key: &str, value: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        // No file yet is an empty document to add the first key to, not an
        // error: this is the state every user starts in.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(source).context(error::ConfigReadSnafu { path }),
        Ok(text) => text,
    };

    // The guard against clobbering. A file that will not parse is refused
    // rather than replaced, because "replace" here means "delete whatever the
    // user had".
    let mut document: toml_edit::DocumentMut =
        existing.parse().context(error::ConfigEditSnafu { path })?;
    document[key] = toml_edit::value(value);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context(error::ConfigWriteSnafu { path: parent })?;
    }
    std::fs::write(path, document.to_string()).context(error::ConfigWriteSnafu { path })
}

/// The URL schemes a configured server can actually be called over.
const CALLABLE_SCHEMES: [&str; 2] = ["http", "https"];

/// Refuse a URL that parses but could never be dialled.
///
/// `Url::parse` is a syntax check, not a usability one: `ftp://tapes.example`
/// and `file:///etc/passwd` are both perfectly well-formed URLs and neither is
/// something this client can send a request to. Caught here, the user is told
/// once, at the moment they typed it; accepted here, every later command fails
/// in a way that reads like the server being unreachable.
fn check_url(key: &str, value: &str) -> Result<()> {
    let parsed = Url::parse(value).context(error::TapesUrlSnafu)?;
    if CALLABLE_SCHEMES.contains(&parsed.scheme()) {
        return Ok(());
    }
    error::ConfigUrlSchemeSnafu {
        key,
        scheme: parsed.scheme(),
    }
    .fail()
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
            // Both checks run before anything is written: an unknown key and an
            // uncallable URL are each a typo the user should hear about once,
            // now — not on every command afterwards, where a stored bad value
            // reads like the server being at fault.
            check_key(&args.key)?;
            check_url(&args.key, &args.value)?;
            // Deliberately not `read` first. The file is edited in place rather
            // than re-serialized (see `write_key`), which both preserves keys
            // this build does not know and lets `config set` repair a known key
            // holding a wrong-typed value — the file being unreadable must not
            // disable the command that fixes it. Structurally broken TOML is
            // still refused, by the parse inside `write_key`.
            write_key(path, &args.key, &args.value)?;
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

        for key in [TAPES_URL_KEY, INGEST_URL_KEY] {
            assert!(run_in(&set(key, "tapes.example"), &path).is_err());
            assert!(!path.exists(), "nothing should have been written");
        }
    }

    #[test]
    fn a_url_this_client_could_never_call_is_refused_and_the_scheme_is_named() {
        // Parsing is a syntax check, not a usability one: these are all
        // well-formed URLs and none of them is something a tapes request can be
        // sent to. The scheme is in the message because it is the part the user
        // has to change.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        for (value, scheme) in [
            ("ftp://tapes.example", "ftp"),
            ("file:///etc/passwd", "file"),
            ("ws://tapes.example", "ws"),
        ] {
            let err = run_in(&set(TAPES_URL_KEY, value), &path).unwrap_err();
            let rendered = format!("{err}");
            assert!(rendered.contains(scheme), "got: {rendered}");
            assert!(rendered.contains("http"), "got: {rendered}");
            assert!(!path.exists(), "nothing should have been written");
        }

        // And the two that work still do.
        for value in ["http://tapes.example", "https://tapes.example"] {
            assert!(run_in(&set(TAPES_URL_KEY, value), &path).is_ok());
        }
    }

    #[test]
    fn setting_one_key_leaves_a_newer_builds_keys_alone() {
        // The forward-compatibility promise the read path makes, kept on the
        // write path too. Reading tolerates unknown keys; a write that rendered
        // the parsed model would emit only the fields this build has and delete
        // the rest — so a user running two versions would lose the newer one's
        // settings the first time the older one set anything.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# a comment the user wrote\n\
             tapes-url = \"http://old.example\"\n\
             something-later = 3\n\
             \n\
             [a-table-from-the-future]\n\
             nested = true\n",
        )
        .unwrap();

        run_in(&set(TAPES_URL_KEY, "http://new.example"), &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("something-later = 3"), "got: {text}");
        assert!(text.contains("[a-table-from-the-future]"), "got: {text}");
        assert!(text.contains("nested = true"), "got: {text}");
        assert!(
            text.contains("# a comment the user wrote"),
            "editing in place keeps the user's own text too: {text}",
        );
        assert_eq!(
            read(&path).unwrap().tapes_url.as_deref(),
            Some("http://new.example"),
            "and the key that was set is the one that changed",
        );
    }

    #[test]
    fn setting_a_key_can_repair_a_value_of_the_wrong_type() {
        // A file that is structurally fine but holds a wrong-typed value makes
        // `read` fail — so if `set` read first, the file being broken would
        // disable the only command that fixes it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "tapes-url = 3\n").unwrap();
        assert!(read(&path).is_err());

        run_in(&set(TAPES_URL_KEY, "http://tapes.example"), &path).unwrap();
        assert_eq!(
            read(&path).unwrap().tapes_url.as_deref(),
            Some("http://tapes.example"),
        );
    }

    #[test]
    fn setting_a_key_over_a_file_that_will_not_parse_is_refused() {
        // "Replace it" would mean "delete whatever the user had", and a file
        // that does not parse is exactly the file whose contents cannot be
        // recovered afterwards.
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

        write_key(&path, TAPES_URL_KEY, "http://tapes.example").unwrap();

        // The flag, the `config set` key, and the TOML key are one spelling.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tapes-url"), "got: {text}");

        assert_eq!(
            read(&path).unwrap().tapes_url.as_deref(),
            Some("http://tapes.example"),
        );
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
        config.set(TAPES_URL_KEY, "http://api".to_owned()).unwrap();
        config
            .set(INGEST_URL_KEY, "http://ingest".to_owned())
            .unwrap();
        assert_eq!(
            config.entries(),
            vec![
                (TAPES_URL_KEY, "http://api"),
                (INGEST_URL_KEY, "http://ingest")
            ]
        );
    }
}
