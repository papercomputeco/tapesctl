//! `tapesctl plugin install codex-app` — and its inverse.
//!
//! Installing a redirect-captured harness is a no-op and installing pi is a
//! file copy. This one is neither, because capturing an app nobody launches
//! takes three things to line up at once:
//!
//! 1. a **packaged hook plugin**, rendered from the crate's templates around
//!    this client's identity and this client's hook command line;
//! 2. a **patched `config.toml`**, so the app's provider traffic reaches a
//!    loopback address that will still mean the same thing tomorrow; and
//! 3. a **handoff file**, so the hook installed in (1) and the capture that
//!    binds (2) agree on the address and on the secret that authenticates a
//!    report between them.
//!
//! All three are written here, and all three are removed by `uninstall`.
//!
//! # What this deliberately does not do
//!
//! It does not run `codex plugin marketplace add` / `codex plugin add`. paper
//! does, and the ~200 lines it takes to do it well — collision detection
//! against an existing marketplace of the same name, force-refresh on a
//! changed source, and matching the CLI's own stderr phrasings to tell
//! "already there" from "failed" — are *Codex plugin-manager knowledge*, not
//! delivery. That knowledge belongs in `tapes-harnesses` beside the manifest
//! templates it completes; it is not there yet, and copying it here would
//! create the second drifting implementation the shared crate exists to
//! prevent. So the installer writes a source tree in the layout the plugin
//! manager consumes and prints the two commands. When the crate grows the
//! invocation, this becomes a call.
//!
//! # Writing under the user's home
//!
//! Unlike pi's extension directory, this tree is tapesctl's own — but it is
//! still someone's home, so the containment discipline from [`crate::plugin`]
//! applies unchanged: the resolved destination must sit beneath the home it
//! was derived from, and every file is staged with `O_EXCL` and renamed into
//! place, so a symlink planted at a target is replaced rather than written
//! through.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt};
use tapes_harnesses::config::codex as codex_config;
use tapes_harnesses::launch::CodexAuth;
use tapes_harnesses::plugin::codex_app::{
    HookPluginIdentity, render_hooks_manifest, render_plugin_manifest,
};
use tracing::info;

use super::{
    Handoff, MARKETPLACE_NAME, PLUGIN_NAME, PROVIDER_DISPLAY_NAME, PROVIDER_ID, codex_config_path,
    generate_secret, plugin_root, resolve_auth, state_dir,
};
use crate::cli::{PluginInstallArgs, PluginUninstallArgs};
use crate::error::{Result, error};

/// Version stamped into the rendered plugin manifest.
///
/// The binary's own version, which is how a reinstall from a newer tapesctl
/// invalidates the app's cached copy of the plugin: Codex keys its cache on
/// the manifest version, so an unchanged number means an unchanged plugin even
/// when the hook command line moved.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The hint Codex prints when the API key is missing, in API-key mode.
const ENV_KEY_INSTRUCTIONS: &str =
    "Set OPENAI_API_KEY to an OpenAI API key; tapesctl forwards it upstream unchanged.";

/// Everything one install decided, before anything is written.
///
/// Separated from the writing so `--dry-run` reports exactly what a real run
/// would do rather than a parallel description of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Loopback address the app is configured against and a capture binds.
    pub proxy_addr: SocketAddr,
    /// How the app will authenticate upstream.
    pub auth: CodexAuth,
    /// `$CODEX_HOME/config.toml`.
    pub config_path: PathBuf,
    /// Root of the packaged plugin source.
    pub plugin_root: PathBuf,
    /// The handoff file.
    pub handoff_path: PathBuf,
    /// The command line each hook runs, already shell-quoted.
    pub hook_command: String,
}

impl Plan {
    /// The `<plugin>@<marketplace>` spec `codex plugin add` takes.
    #[must_use]
    pub fn plugin_spec(&self) -> String {
        format!("{PLUGIN_NAME}@{MARKETPLACE_NAME}")
    }

    /// The directory `codex plugin marketplace add` is pointed at.
    #[must_use]
    pub fn marketplace_root(&self) -> &Path {
        &self.plugin_root
    }
}

/// Decide what an install would do, without touching anything.
///
/// `executable` is the absolute path a hook will exec — passed in rather than
/// read from [`std::env::current_exe`] here so the decision is testable
/// against a path that is not the test runner's own binary.
pub fn plan(
    home: &Path,
    config_path: PathBuf,
    port: Option<u16>,
    auth: CodexAuth,
    executable: &Path,
    harness_id: &str,
) -> Result<Plan> {
    let proxy_addr = match port {
        Some(port) => SocketAddr::from(([127, 0, 0, 1], port)),
        None => free_loopback_addr()?,
    };
    let handoff_path = Handoff::path(home);
    Ok(Plan {
        proxy_addr,
        auth,
        config_path,
        plugin_root: plugin_root(home),
        hook_command: hook_command(executable, &handoff_path, harness_id),
        handoff_path,
    })
}

/// A loopback port nothing currently holds.
///
/// Bound and released, so the answer is the kernel's rather than a guess at a
/// number nobody else uses. It is a snapshot, not a reservation — something
/// can take the port between here and the first capture — which is why the
/// capture's bind failure names `--port` as the way out rather than trying to
/// re-pick behind the user's back. Re-picking would silently invalidate the
/// `config.toml` the app has already read.
fn free_loopback_addr() -> Result<SocketAddr> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context(error::BindSnafu)?;
    listener.local_addr().context(error::BindSnafu)
}

/// The command line Codex runs at every lifecycle boundary.
///
/// The executable is named by absolute path rather than by `tapesctl`: a hook
/// runs under the desktop app's environment, whose `PATH` is the window
/// server's and generally does not contain the user's shell profile
/// directories. Every path is single-quoted for `/bin/sh`, because a home
/// directory containing a space or a quote is ordinary and the crate's
/// renderer escapes for JSON, not for a shell.
fn hook_command(executable: &Path, handoff: &Path, harness_id: &str) -> String {
    format!(
        "{} plugin hook {harness_id} --handoff {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&handoff.to_string_lossy()),
    )
}

/// `value` as a single POSIX shell word.
///
/// Single quotes suppress every expansion `/bin/sh` performs, so the only
/// character needing care is the closing quote itself — spliced out, escaped,
/// and spliced back in.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The identity Codex shows the user when asking them to trust these hooks.
fn identity() -> HookPluginIdentity<'static> {
    HookPluginIdentity::new(PLUGIN_NAME, PLUGIN_VERSION)
        .with_description("Reports Codex lifecycle metadata to a local tapesctl capture proxy.")
        .with_display_name("tapesctl capture")
        .with_short_description("Capture this app's sessions with tapesctl.")
        .with_long_description(
            "Sends allowlisted session, prompt, and subagent boundaries to a tapesctl \
             capture proxy running on this machine, so the app's traffic can be filed \
             under the session it belongs to. Prompts and assistant output are never \
             read.",
        )
        .with_developer_name("tapesctl")
}

/// The marketplace manifest that offers the packaged plugin as a local source.
///
/// NOTE: this shape is Codex plugin-manager knowledge and the crate does not
/// own it — it ships the plugin's own two manifests as templates and stops
/// there, so the wrapper that makes them *installable* is authored twice, here
/// and in paper. Rendered rather than embedded so the two names it carries
/// come from the constants everything else uses.
fn marketplace_manifest() -> String {
    format!(
        r#"{{
  "name": "{MARKETPLACE_NAME}",
  "interface": {{
    "displayName": "tapesctl"
  }},
  "plugins": [
    {{
      "name": "{PLUGIN_NAME}",
      "source": {{
        "source": "local",
        "path": "./plugins/{PLUGIN_NAME}"
      }},
      "policy": {{
        "installation": "AVAILABLE",
        "authentication": "ON_INSTALL"
      }},
      "category": "Productivity"
    }}
  ]
}}
"#
    )
}

/// The files a packaged plugin consists of, relative to [`Plan::plugin_root`].
fn plugin_files(hook_command: &str) -> Vec<(PathBuf, String)> {
    let plugin_dir = Path::new("plugins").join(PLUGIN_NAME);
    vec![
        (
            Path::new(".agents")
                .join("plugins")
                .join("marketplace.json"),
            marketplace_manifest(),
        ),
        (
            plugin_dir.join(".codex-plugin").join("plugin.json"),
            render_plugin_manifest(&identity()),
        ),
        (
            plugin_dir.join("hooks").join("hooks.json"),
            render_hooks_manifest(hook_command),
        ),
    ]
}

/// Run one `plugin install` for a lifecycle-hook harness.
pub fn run(args: &PluginInstallArgs, home: &Path) -> Result<()> {
    run_at(args, home, codex_config_path(home))
}

/// The body of [`run`], against an explicit `config.toml`.
///
/// The config path is a parameter rather than resolved inside because
/// [`super::codex_home`] reads `$CODEX_HOME` from the ambient environment: a
/// test that called the resolver would patch the developer's own Codex
/// configuration on any machine where that variable happens to be set.
pub fn run_at(args: &PluginInstallArgs, home: &Path, config_path: PathBuf) -> Result<()> {
    let harness = super::resolve_hook_harness(&args.harness)?;
    let auth = resolve_auth(args.codex_auth.as_deref())?;
    let executable = std::env::current_exe().context(error::CurrentExeSnafu)?;
    let plan = plan(
        home,
        config_path,
        args.port,
        auth,
        &executable,
        harness.id(),
    )?;

    if args.dry_run {
        report_plan(&plan);
        return Ok(());
    }

    for (relative, contents) in plugin_files(&plan.hook_command) {
        write_private(&plan.plugin_root.join(relative), contents.as_bytes(), home)?;
    }

    println!(
        "tapesctl: packaged the hook plugin at {}",
        plan.plugin_root.display(),
    );

    // `config.toml` is patched before the handoff is replaced, and the two
    // together are the installation: the hook authenticates with the handoff's
    // secret while the app reaches the address the config names, so a state
    // where one is new and the other is old captures nothing and reports
    // nothing. Parsing and rewriting someone else's TOML is much the likelier
    // of the two to fail, so it runs while rollback is still free — a failure
    // here has touched nothing but the plugin tree, whose contents are a pure
    // function of the plan.
    //
    // A reinstall is what makes the order load-bearing. It rotates the secret,
    // so committing the handoff first and then failing would leave a running
    // capture unable to authenticate any report and, if the address moved, a
    // config pointing at a port nothing will serve.
    let restore = ConfigRestore::capture(&plan.config_path)?;
    patch_config(&plan)?;

    let handoff = Handoff {
        version: super::HANDOFF_VERSION,
        harness_id: harness.id().to_owned(),
        proxy_addr: plan.proxy_addr,
        // Fresh on every install: rotating the secret is what makes a
        // reinstall a repair rather than a no-op, and a capture still holding
        // the old one fails closed until it is restarted.
        secret: generate_secret(),
        provider_id: PROVIDER_ID.to_owned(),
        installed_at: now_rfc3339(),
    };
    let write = serde_json::to_vec_pretty(&handoff)
        .context(error::CodexAppHandoffWriteSnafu {
            path: plan.handoff_path.clone(),
        })
        .and_then(|serialized| write_private(&plan.handoff_path, &serialized, home));
    if let Err(error) = write {
        // The config now names an address whose secret was never written.
        // Putting it back is what keeps a failed reinstall a no-op instead of
        // a half-install that silently captures nothing.
        //
        // When the rollback itself fails the reported error changes, because
        // what the user needs to know changes. The write failure alone is
        // recoverable by retrying and says so; a machine whose config and
        // handoff disagree is not something either error implies, and the app
        // will dial a port nothing serves.
        return Err(match restore.rollback() {
            Ok(()) => error,
            Err(config_path) => error::CodexAppInstallNotRolledBackSnafu {
                path: config_path,
                cause: error.to_string(),
            }
            .build(),
        });
    }
    println!(
        "tapesctl: wrote the handoff at {}",
        plan.handoff_path.display(),
    );

    info!(
        harness = harness.id(),
        proxy = %plan.proxy_addr,
        "codex-app capture installed",
    );
    report_next_steps(&plan, harness.id());
    Ok(())
}

/// Run one `plugin uninstall` for a lifecycle-hook harness.
///
/// Ordered so a partial failure leaves the machine *less* captured rather than
/// more: the provider goes first, because a `config.toml` still pointing at a
/// port nothing serves is the one state that breaks the app itself.
pub fn uninstall(args: &PluginUninstallArgs, home: &Path) -> Result<()> {
    uninstall_at(args, home, codex_config_path(home))
}

/// The body of [`uninstall`], against an explicit `config.toml`. Split out for
/// the reason [`run_at`] is.
pub fn uninstall_at(args: &PluginUninstallArgs, home: &Path, config_path: PathBuf) -> Result<()> {
    let harness = super::resolve_hook_harness(&args.harness)?;
    let state = state_dir(home);
    // The provider the *installer* wrote, when it is still recorded; the
    // compiled-in name otherwise, so a handoff removed by hand does not strand
    // a provider declaration nothing will ever clean up.
    let provider_id = Handoff::read(&Handoff::path(home))
        .map_or_else(|_| PROVIDER_ID.to_owned(), |handoff| handoff.provider_id);

    if args.dry_run {
        println!(
            "tapesctl: would remove the {provider_id:?} provider from {}",
            config_path.display()
        );
        println!("tapesctl: would remove {}", state.display());
        println!(
            "tapesctl: would leave the plugin registered with Codex; remove it with \
             `codex plugin remove {PLUGIN_NAME}@{MARKETPLACE_NAME}`",
        );
        return Ok(());
    }

    if let Some(existing) = read_config(&config_path)? {
        let cleaned = codex_config::remove_provider(&existing, &provider_id).context(
            error::CodexConfigSnafu {
                path: config_path.clone(),
            },
        )?;
        if cleaned != existing {
            write_config(&config_path, &cleaned)?;
            println!(
                "tapesctl: removed the {provider_id:?} provider from {}",
                config_path.display(),
            );
        }
    }

    match std::fs::remove_dir_all(&state) {
        Ok(()) => println!("tapesctl: removed {}", state.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).context(error::PluginWriteSnafu { path: state });
        }
    }

    info!(harness = harness.id(), "codex-app capture uninstalled");
    println!(
        "tapesctl: the plugin is still registered with Codex — remove it with \
         `codex plugin remove {PLUGIN_NAME}@{MARKETPLACE_NAME}`",
    );
    Ok(())
}

/// Patch the provider into `config.toml`, reporting whether anything changed.
///
/// A patch that changes nothing is reported as such rather than silently: the
/// user's next question is always "do I have to restart the app", and the
/// answer is exactly "only if this line said something changed".
fn patch_config(plan: &Plan) -> Result<()> {
    let existing = read_config(&plan.config_path)?.unwrap_or_default();
    let patch = provider_patch(plan);
    let patched =
        codex_config::apply_provider(&existing, &patch).context(error::CodexConfigSnafu {
            path: plan.config_path.clone(),
        })?;
    if patched == existing {
        println!(
            "tapesctl: {} already routes {PROVIDER_ID} through {}",
            plan.config_path.display(),
            plan.proxy_addr,
        );
        return Ok(());
    }
    write_config(&plan.config_path, &patched)?;
    println!(
        "tapesctl: pointed {} at {} — restart the Codex app for it to take effect",
        plan.config_path.display(),
        plan.proxy_addr,
    );
    Ok(())
}

/// The provider declaration this client writes.
///
/// No attribution header, unlike `start codex`. That header exists so the
/// crate's open-rollout lane can tell two concurrent `codex` processes apart on
/// one endpoint; this harness is not attributed that way at all — its identity
/// arrives through authenticated lifecycle reports — so the header would be a
/// value nothing reads, sent on every request the app makes.
fn provider_patch(plan: &Plan) -> codex_config::CodexProviderPatch {
    let patch = codex_config::CodexProviderPatch::new(
        PROVIDER_ID,
        PROVIDER_DISPLAY_NAME,
        base_url(plan),
        plan.auth,
    );
    match plan.auth {
        CodexAuth::ApiKey => patch.with_env_key_instructions(ENV_KEY_INSTRUCTIONS),
        CodexAuth::ChatGpt => patch,
    }
}

fn base_url(plan: &Plan) -> String {
    expected_base_url(plan.proxy_addr, plan.auth)
}

/// The `base_url` a provider patched for `addr` under `auth` carries.
///
/// Shared with the capture side, which reads the value back to check that the
/// app is still pointed here: two spellings of this rule would let a capture
/// declare a config healthy that the installer would rewrite.
///
/// Codex appends `/responses` to whatever it is given. OpenAI's route is
/// `/v1/responses`, so an API-key endpoint has to end at the `/v1` segment;
/// the ChatGPT backend has no `/v1` component at all. Same split
/// `Harness::endpoint_for` makes for `start codex`.
#[must_use]
pub fn expected_base_url(addr: SocketAddr, auth: CodexAuth) -> String {
    match auth {
        CodexAuth::ChatGpt => format!("http://{addr}"),
        CodexAuth::ApiKey => format!("http://{addr}/v1"),
    }
}

/// The current config text, or `None` when the file does not exist yet.
///
/// Absence is not a failure: the patch grammar takes an empty document as a
/// fresh one, which is what a machine that has run the app but never edited
/// its config looks like.
/// The `config.toml` bytes from before the installer touched them, so a later
/// step's failure can put the file back.
///
/// Restoring reports its own failure to the caller rather than handling it:
/// a rollback that worked leaves nothing to say, so the original error stands
/// alone, while a rollback that failed leaves the machine in a state that
/// error does not describe.
struct ConfigRestore {
    path: PathBuf,
    /// `None` when the installer created the file, in which case putting it
    /// back means removing it.
    previous: Option<String>,
}

impl ConfigRestore {
    fn capture(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            previous: read_config(path)?,
        })
    }

    /// `Err(path)` when the file could not be put back, naming it so the
    /// caller can say what is now inconsistent.
    fn rollback(self) -> std::result::Result<(), PathBuf> {
        let restored = match &self.previous {
            Some(previous) => write_config(&self.path, previous),
            None => std::fs::remove_file(&self.path).or_else(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(err).context(error::CodexConfigWriteSnafu {
                        path: self.path.clone(),
                    })
                }
            }),
        };
        if restored.is_err() {
            return Err(self.path);
        }
        Ok(())
    }
}

fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context(error::CodexConfigReadSnafu {
            path: path.to_path_buf(),
        }),
    }
}

/// Replace `config.toml` atomically, preserving the mode it already had.
///
/// This is the user's file, and the only file here that is not tapesctl's, so
/// the permissions question is answered by what is already on disk. A file
/// created here gets owner-only, matching what codex itself writes.
fn write_config(path: &Path, contents: &str) -> Result<()> {
    let mode = existing_mode(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context(error::CodexConfigWriteSnafu {
            path: path.to_path_buf(),
        })?;
    }
    atomic_write(path, contents.as_bytes(), mode).context(error::CodexConfigWriteSnafu {
        path: path.to_path_buf(),
    })
}

/// Write one tapesctl-owned file, owner-only, beneath `home`.
fn write_private(path: &Path, contents: &[u8], home: &Path) -> Result<()> {
    let parent = path.parent().context(error::PluginDestinationSnafu {
        path: path.to_path_buf(),
    })?;
    std::fs::create_dir_all(parent).context(error::PluginWriteSnafu {
        path: parent.to_path_buf(),
    })?;

    // The tree is tapesctl's, but it lives in someone's home and any component
    // of it may have been replaced by a link. Resolve what is actually there
    // and require it to still sit beneath the home the caller named.
    let resolved_parent = parent.canonicalize().context(error::PluginWriteSnafu {
        path: parent.to_path_buf(),
    })?;
    let resolved_home = home.canonicalize().context(error::PluginWriteSnafu {
        path: home.to_path_buf(),
    })?;
    snafu::ensure!(
        resolved_parent.starts_with(&resolved_home),
        error::PluginDestinationSnafu {
            path: parent.to_path_buf(),
        }
    );

    let target = resolved_parent.join(path.file_name().context(error::PluginDestinationSnafu {
        path: path.to_path_buf(),
    })?);
    atomic_write(&target, contents, Some(0o600)).context(error::PluginWriteSnafu {
        path: target.clone(),
    })?;
    Ok(())
}

/// Stage beside the target, then rename over it.
///
/// The staging file is created with `O_EXCL`, which never follows a symlink,
/// and the rename replaces even a link planted at the target rather than
/// writing through it. Nothing is unlinked first, so a failure anywhere leaves
/// the previous file intact — which matters most for `config.toml`, where the
/// previous file is the user's working configuration.
fn atomic_write(target: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("destination has no parent directory"))?;
    let staged = parent.join(format!(
        ".{}.tapesctl-install-{}",
        target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
    ));
    let mut file = match open_staging(&staged) {
        Ok(file) => file,
        // Crash residue from an install that died between staging and rename,
        // landed on again through PID reuse. The name is ours by construction,
        // so clear it and retry the exclusive create exactly once.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&staged);
            open_staging(&staged)?
        }
        Err(err) => return Err(err),
    };
    let result = set_mode(&file, mode)
        .and_then(|()| file.write_all(contents))
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&staged, target));
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn open_staging(staged: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged)
}

#[cfg(unix)]
fn set_mode(file: &std::fs::File, mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match mode {
        Some(mode) => file.set_permissions(std::fs::Permissions::from_mode(mode)),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn set_mode(_file: &std::fs::File, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
        .or(Some(0o600))
}

#[cfg(not(unix))]
fn existing_mode(_path: &Path) -> Option<u32> {
    None
}

/// Now, RFC 3339, or the empty string if the clock cannot be formatted —
/// which never fails in practice and is diagnostic-only if it ever did.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn report_plan(plan: &Plan) {
    println!(
        "tapesctl: would package the hook plugin under {}",
        plan.plugin_root.display(),
    );
    println!(
        "tapesctl: would write the handoff at {}",
        plan.handoff_path.display()
    );
    println!(
        "tapesctl: would point {} at {}",
        plan.config_path.display(),
        plan.proxy_addr,
    );
    println!("tapesctl: each hook would run {}", plan.hook_command);
}

fn report_next_steps(plan: &Plan, harness_id: &str) {
    println!();
    println!("Next, register the plugin with Codex and let it run the hooks:");
    println!(
        "  codex plugin marketplace add {}",
        plan.marketplace_root().display()
    );
    println!("  codex plugin add {}", plan.plugin_spec());
    println!();
    println!("Then capture:");
    println!("  tapesctl capture {harness_id} --tapes-url <url>");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::attribution::codex_app::LIFECYCLE_EVENTS;

    fn install_args(dry_run: bool) -> PluginInstallArgs {
        PluginInstallArgs {
            harness: "codex-app".to_owned(),
            dry_run,
            port: Some(51520),
            codex_auth: None,
        }
    }

    /// Where an installer under a temporary home writes codex's config.
    ///
    /// Spelled here rather than taken from [`codex_config_path`] so no test
    /// can be steered onto the developer's real `$CODEX_HOME`.
    fn config_path(home: &Path) -> PathBuf {
        home.join(".codex").join("config.toml")
    }

    fn install_at(args: &PluginInstallArgs, home: &Path) -> Result<()> {
        run_at(args, home, config_path(home))
    }

    fn uninstall_from(home: &Path) -> Result<()> {
        uninstall_at(
            &PluginUninstallArgs {
                harness: "codex-app".to_owned(),
                dry_run: false,
            },
            home,
            config_path(home),
        )
    }

    fn a_plan(home: &Path) -> Plan {
        plan(
            home,
            config_path(home),
            Some(51520),
            CodexAuth::ChatGpt,
            Path::new("/opt/tapes ctl/bin/tapesctl"),
            "codex-app",
        )
        .unwrap()
    }

    /// The install's whole point on the plugin side: the bytes Codex reads are
    /// the crate's templates with this client's strings in them, and no slot
    /// survives into the plugin UI.
    #[test]
    fn the_packaged_plugin_is_the_crates_templates_rendered() {
        let home = tempfile::tempdir().unwrap();
        let plan = a_plan(home.path());
        for (relative, contents) in plugin_files(&plan.hook_command) {
            write_private(
                &plan.plugin_root.join(relative),
                contents.as_bytes(),
                home.path(),
            )
            .unwrap();
        }

        let manifest = plan
            .plugin_root
            .join("plugins")
            .join(PLUGIN_NAME)
            .join(".codex-plugin")
            .join("plugin.json");
        let rendered = std::fs::read_to_string(&manifest).unwrap();
        assert!(!rendered.contains("__TAPES_"), "got: {rendered}");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["name"], PLUGIN_NAME);
        assert_eq!(parsed["version"], PLUGIN_VERSION);
    }

    /// The hooks file has to subscribe the command to exactly the boundaries
    /// the crate parses — the crate pins the template against its own event
    /// list, and this pins that the *installed* file inherited that.
    #[test]
    fn every_lifecycle_boundary_runs_this_clients_hook_command() {
        let plan = a_plan(Path::new("/home/someone"));
        let rendered = render_hooks_manifest(&plan.hook_command);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let hooks = parsed["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), LIFECYCLE_EVENTS.len());
        for event in LIFECYCLE_EVENTS {
            assert_eq!(
                hooks[*event][0]["hooks"][0]["command"], plan.hook_command,
                "{event} does not run the installed command",
            );
        }
    }

    /// A hook runs under the desktop app's environment, so the executable is
    /// named absolutely and every path is a single shell word — a home with a
    /// space in it is ordinary, and the crate escapes for JSON, not for sh.
    #[test]
    fn the_hook_command_survives_paths_that_need_quoting() {
        let command = hook_command(
            Path::new("/opt/tapes ctl/bin/tapesctl"),
            Path::new("/home/o'brien/.tapes/codex-app/handoff.json"),
            "codex-app",
        );
        assert_eq!(
            command,
            "'/opt/tapes ctl/bin/tapesctl' plugin hook codex-app \
             --handoff '/home/o'\\''brien/.tapes/codex-app/handoff.json'",
        );
        assert!(!command.starts_with("tapesctl "), "got: {command}");
    }

    #[test]
    fn shell_quoting_closes_every_quote_it_opens() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    /// The three artefacts have to agree, or a hook reports to an address no
    /// capture binds — the failure mode that looks exactly like "capture is
    /// broken".
    #[test]
    fn install_writes_a_handoff_the_config_and_the_hook_all_agree_with() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY-of-intent: the config lives under the temp home, not the
        // developer's own `~/.codex`.
        let codex_home = home.path().join(".codex");
        std::fs::create_dir_all(&codex_home).unwrap();

        install_at(&install_args(false), home.path()).unwrap();

        let handoff = Handoff::read(&Handoff::path(home.path())).unwrap();
        assert_eq!(handoff.proxy_addr.port(), 51520);
        assert_eq!(handoff.provider_id, PROVIDER_ID);

        let config = std::fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(
            config.contains(&format!("model_provider = \"{PROVIDER_ID}\"")),
            "got: {config}"
        );
        assert!(
            config.contains(&format!("base_url = \"http://{}\"", handoff.proxy_addr)),
            "got: {config}",
        );

        let hooks = std::fs::read_to_string(
            plugin_root(home.path())
                .join("plugins")
                .join(PLUGIN_NAME)
                .join("hooks")
                .join("hooks.json"),
        )
        .unwrap();
        assert!(
            hooks.contains(&Handoff::path(home.path()).to_string_lossy().into_owned()),
            "the hooks file must name the handoff the capture will read: {hooks}",
        );
    }

    /// Reinstalling is the repair path, so it must rotate the secret — a
    /// capture still holding the old one then fails closed rather than
    /// continuing to trust a value that may have leaked.
    #[test]
    fn reinstalling_rotates_the_secret_and_leaves_the_config_idempotent() {
        let home = tempfile::tempdir().unwrap();
        install_at(&install_args(false), home.path()).unwrap();
        let first = Handoff::read(&Handoff::path(home.path())).unwrap();
        let config_after_first = std::fs::read_to_string(config_path(home.path())).unwrap();

        install_at(&install_args(false), home.path()).unwrap();
        let second = Handoff::read(&Handoff::path(home.path())).unwrap();

        assert_ne!(first.secret, second.secret);
        assert_eq!(
            std::fs::read_to_string(config_path(home.path())).unwrap(),
            config_after_first,
            "the same install must not keep rewriting config.toml",
        );
    }

    /// A rollback that cannot put the file back has to say so, because the
    /// caller's error changes when it happens: the config keeps naming an
    /// address whose secret was never written, which is not something the
    /// original write failure implies.
    #[test]
    fn a_rollback_that_cannot_write_reports_the_file_it_left() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("codex");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, "model = \"gpt-5-codex\"\n").unwrap();

        let restore = ConfigRestore::capture(&config).unwrap();

        // Replace the captured file with a directory. Restoring renames a
        // temporary file over that path, which cannot succeed against a
        // directory — a rule of the filesystem rather than of permissions,
        // so it holds for root too. Denying writes to the parent instead
        // would prove nothing under CI, which runs as root and is not
        // subject to the permission bits at all.
        std::fs::remove_file(&config).unwrap();
        std::fs::create_dir(&config).unwrap();

        let left = restore.rollback().expect_err("the rollback cannot succeed");

        assert_eq!(left, config);
    }

    /// The message a user is left with has to name both the file and the way
    /// out; a double failure is the one case where neither is guessable.
    #[test]
    fn the_double_failure_names_the_file_and_the_recovery() {
        let rendered = error::CodexAppInstallNotRolledBackSnafu {
            path: PathBuf::from("/home/someone/.codex/config.toml"),
            cause: "no space left on device".to_owned(),
        }
        .build()
        .to_string();

        assert!(
            rendered.contains("/home/someone/.codex/config.toml"),
            "{rendered}"
        );
        assert!(
            rendered.contains("tapesctl plugin install codex-app"),
            "{rendered}",
        );
        assert!(rendered.contains("no space left on device"), "{rendered}");
    }

    /// The failure the ordering exists for: `config.toml` is someone else's
    /// file and may not parse. If the secret had already been rotated by then,
    /// a capture already running would authenticate nothing — every report
    /// refused, every turn filed as `unknown` — while the config still named
    /// the old address, so nothing would announce the breakage.
    #[test]
    fn a_config_that_cannot_be_parsed_does_not_rotate_the_secret() {
        let home = tempfile::tempdir().unwrap();
        install_at(&install_args(false), home.path()).unwrap();
        let working = Handoff::read(&Handoff::path(home.path())).unwrap();

        std::fs::write(config_path(home.path()), "this is not = = valid toml\n").unwrap();

        install_at(&install_args(false), home.path()).unwrap_err();

        assert_eq!(
            Handoff::read(&Handoff::path(home.path())).unwrap().secret,
            working.secret,
            "a failed install must leave a running capture's secret alone",
        );
    }

    /// A reinstall rotates the secret, so a failure between patching the config
    /// and writing the handoff would leave the app dialling an address whose
    /// secret was never written — a capture that authenticates nothing and
    /// files every turn as `unknown`. The config must go back instead.
    #[test]
    fn a_failed_reinstall_leaves_the_working_install_alone() {
        let home = tempfile::tempdir().unwrap();
        install_at(&install_args(false), home.path()).unwrap();
        let working_config = std::fs::read_to_string(config_path(home.path())).unwrap();

        // Make the handoff unwritable by putting a directory in its place, so
        // the second install fails *after* the config has been patched.
        let handoff_path = Handoff::path(home.path());
        std::fs::remove_file(&handoff_path).unwrap();
        std::fs::create_dir(&handoff_path).unwrap();

        let moved = PluginInstallArgs {
            port: Some(51521),
            ..install_args(false)
        };
        install_at(&moved, home.path()).unwrap_err();

        assert_eq!(
            std::fs::read_to_string(config_path(home.path())).unwrap(),
            working_config,
            "a failed install must not leave config.toml naming the new address",
        );
    }

    /// The user's file, and everything in it that is not ours, survives.
    #[test]
    fn the_users_own_config_survives_the_patch() {
        let home = tempfile::tempdir().unwrap();
        let config_path = config_path(home.path());
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "# my notes\nmodel = \"gpt-5-codex\"\n\n[tui]\ntheme = \"dark\"\n",
        )
        .unwrap();

        install_at(&install_args(false), home.path()).unwrap();

        let patched = std::fs::read_to_string(&config_path).unwrap();
        assert!(patched.contains("# my notes"), "got: {patched}");
        assert!(patched.contains("theme = \"dark\""), "got: {patched}");
    }

    #[test]
    fn a_dry_run_writes_nothing_at_all() {
        let home = tempfile::tempdir().unwrap();
        install_at(&install_args(true), home.path()).unwrap();
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_handoff_is_owner_only_because_it_holds_a_secret() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        install_at(&install_args(false), home.path()).unwrap();
        let mode = std::fs::metadata(Handoff::path(home.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_destination_is_refused_rather_than_followed() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".tapes")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            home.path().join(".tapes").join("codex-app"),
        )
        .unwrap();

        let err = install_at(&install_args(false), home.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("resolves outside"), "got: {err}");
    }

    /// Uninstall must undo the one change that can break the app — a provider
    /// pointing at a port nothing serves — and leave everything else alone.
    #[test]
    fn uninstall_removes_the_provider_and_the_state_but_not_the_users_settings() {
        let home = tempfile::tempdir().unwrap();
        let config_path = config_path(home.path());
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[tui]\ntheme = \"dark\"\n").unwrap();
        install_at(&install_args(false), home.path()).unwrap();

        uninstall_from(home.path()).unwrap();

        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(!config.contains(PROVIDER_ID), "got: {config}");
        assert!(config.contains("theme = \"dark\""), "got: {config}");
        assert!(!state_dir(home.path()).exists());
    }

    #[test]
    fn uninstalling_a_machine_that_was_never_installed_is_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        uninstall_from(home.path()).unwrap();
    }

    #[test]
    fn the_api_key_endpoint_ends_at_the_v1_segment_and_the_plan_login_does_not() {
        let home = Path::new("/home/someone");
        let chatgpt = plan(
            home,
            config_path(home),
            Some(1),
            CodexAuth::ChatGpt,
            Path::new("/x"),
            "codex-app",
        )
        .unwrap();
        let api_key = plan(
            home,
            config_path(home),
            Some(1),
            CodexAuth::ApiKey,
            Path::new("/x"),
            "codex-app",
        )
        .unwrap();
        assert_eq!(base_url(&chatgpt), "http://127.0.0.1:1");
        assert_eq!(base_url(&api_key), "http://127.0.0.1:1/v1");
    }

    #[test]
    fn the_api_key_mode_names_the_variable_codex_reads() {
        let home = tempfile::tempdir().unwrap();
        let mut args = install_args(false);
        args.codex_auth = Some("api-key".to_owned());
        install_at(&args, home.path()).unwrap();

        let config = std::fs::read_to_string(config_path(home.path())).unwrap();
        assert!(
            config.contains(tapes_harnesses::launch::CODEX_API_KEY_ENV),
            "got: {config}",
        );
    }

    /// The provider declaration carries no attribution header: this harness is
    /// not attributed by the open-rollout lane, so the header would be a value
    /// nothing reads riding on every request the app makes.
    #[test]
    fn the_patched_provider_sends_no_attribution_header() {
        let home = tempfile::tempdir().unwrap();
        install_at(&install_args(false), home.path()).unwrap();
        let config = std::fs::read_to_string(config_path(home.path())).unwrap();
        assert!(!config.contains("http_headers"), "got: {config}");
    }

    #[test]
    fn a_port_is_chosen_when_none_is_named() {
        let home = Path::new("/home/someone");
        let plan = plan(
            home,
            config_path(home),
            None,
            CodexAuth::ChatGpt,
            Path::new("/x"),
            "codex-app",
        )
        .unwrap();
        assert!(plan.proxy_addr.ip().is_loopback());
        assert_ne!(plan.proxy_addr.port(), 0, "a bound port is never zero");
    }

    #[test]
    fn a_harness_with_no_hook_surface_is_refused_by_the_registry() {
        let home = tempfile::tempdir().unwrap();
        let mut args = install_args(false);
        args.harness = "claude".to_owned();
        let err = install_at(&args, home.path()).unwrap_err().to_string();
        assert!(err.contains("codex-app"), "got: {err}");
    }

    #[test]
    fn the_marketplace_manifest_points_at_the_packaged_plugin() {
        let parsed: serde_json::Value = serde_json::from_str(&marketplace_manifest()).unwrap();
        assert_eq!(parsed["name"], MARKETPLACE_NAME);
        assert_eq!(parsed["plugins"][0]["name"], PLUGIN_NAME);
        assert_eq!(
            parsed["plugins"][0]["source"]["path"],
            format!("./plugins/{PLUGIN_NAME}"),
        );
    }
}
