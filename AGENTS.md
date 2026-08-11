# Contributing

`tapesctl` is the command-line client for [Tapes](https://tapes.dev): it runs a
coding-agent harness under a capture proxy, ships the captured turns to a tapes
server, and provides a command line over the data model that comes back. See
[README.md](README.md) for what the tool does from a user's side.

This file is the orientation a contributor — or a contributor's coding agent —
needs before the first change.

## Build and test

The Nix flake dev shell pins the Rust toolchain (via `rust-toolchain.toml`;
stable, edition 2024, minimum 1.85). It is the recommended environment, but
nothing here requires it — a matching stable toolchain works.

```bash
nix develop
make build              # cargo build --workspace
make run ARGS=version   # cargo run -p tapesctl -- version
```

Before opening a pull request:

```bash
make lint   # cargo fmt --all --check + cargo clippy --workspace --all-targets -D warnings
make test   # cargo test --workspace
```

`make check` runs build, clippy, and test together. `make help` lists every
target.

### Reproducing CI locally

CI runs through Dagger, so the PR gates reproduce on your machine. This needs a
container engine, and is slower than the cargo-native targets — use it when you
want to reproduce a CI failure, not for ordinary iteration.

```bash
make ci     # dagger call lint + test — the same two gates CI runs
make dist   # cross-compile all four release targets into ./build
```

CI additionally cross-compiles for `linux/{amd64,arm64}` and
`darwin/{amd64,arm64}` and smoke-tests each binary: `tapesctl version` must
print its canary line, and a bare `tapesctl` must print help and exit `2`.

## Layout

The workspace has exactly one member, `crates/tapesctl`. (Shared client-side
harness knowledge — launch recipes, session attribution, transcript discovery,
and the capture envelope — lives in a separate repository,
[`tapes-crates`](https://github.com/papercomputeco/tapes-crates), and is
consumed here as a revision-pinned dependency.)

Inside `crates/tapesctl/src`:

- `cli.rs` — the clap surface: every command, flag, and help string.
- `machine.rs` — the crate's one ambient read of the environment.
- `config.rs` — `~/.tapes/config.toml`, the persisted default server.
- `start/` — the just-in-time capture proxy (the wire lane).
- `transcript/` — the transcript lane: live tailer and the `sync` sweep.
- `codex_app/` — plugin install and the capture proxy for a harness that
  launches itself.
- `api/` — the `<resource> <method>` read client.
- `cassette/` — the runtime-discovered `cassettes <name> <method>` surface:
  discovery, the spec reducer, the cache, and clap synthesis.
- `plugin.rs`, `capture.rs`, `logging.rs`, `error.rs` — the remaining
  command entry points and cross-cutting support.
- `ports/` — search, skills, and seed.

## Conventions

**No `unwrap`, `expect`, or `panic` in library code.** The workspace denies all
three via `[workspace.lints.clippy]`; return `Result` and surface errors through
the crate error types. Test modules opt out explicitly, and that attribute is
the marker for "this is test code":

```rust
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests { /* ... */ }
```

**Formatting is enforced**: `rustfmt.toml` sets `max_width = 100`. Run
`make fmt` before `make lint`.

**Help text is user-facing documentation.** Flags and subcommands carry long
help that the README quotes; if you change behavior, change the doc comment in
`cli.rs` in the same commit.

## Traps worth knowing

These are the mistakes that are easy to make and slow to diagnose.

### Ambient environment reads belong at the CLI boundary

`Machine::resolve()` is the crate's *one* read of the real environment (home
directory, `~/.codex/config.toml`, the `codex` program). It happens at the CLI
boundary; every function beneath it takes an explicit path, and
`Machine::at(..)` / `with_codex_program(..)` / `with_tapes_config_path(..)`
construct one pointing anywhere you like.

Keep it that way. This is not hypothetical: the install tests once spawned
whichever `codex` was on the developer's `PATH`, and that CLI wrote a
registration into their real `~/.codex/config.toml` pointing at a temp directory
the test was about to delete. The suite broke the machine it ran on, and the
damage outlived the run.

`Machine::resolve` therefore panics under `#[cfg(test)]` rather than reading the
real environment. Build a `Machine::at(..)` over a `tempfile::tempdir()`
instead, and use `with_codex_program(..)` / `with_tapes_config_path(..)` to aim
the rest.

Two things that guard does **not** cover, so check them by hand:

- **Integration tests in `crates/tapesctl/tests/` link the library without
  `cfg(test)`**, so the panic never fires there. They must isolate themselves —
  the existing ones spawn the binary with an overridden `HOME` and clear the
  `TAPES_*` variables.
- **`TAPESCTL_CACHE_DIR`**, if a test exercising the cassette cache does not set
  it, falls back to the developer's real cache directory. Cosmetic rather than
  destructive, but still a write outside the tempdir.

There is deliberately **no** environment-variable override for the config path:
environment variables are process-global while tests are not. Passing the value
makes an escape a compile error instead. Don't add one.

When you need to see what an install would do, `--dry-run` on
`tapesctl plugin install` / `uninstall` reports every path and writes nothing.

### `pi` needs **both** `--provider` and `--model`, or it captures nothing

This one fails silently, which is what makes it worth writing down.

`--provider` and `--model` are `pi`'s own flags, passed through after `--`;
`tapesctl` passes no argv of its own to `pi`. `pi` only takes the pair — its
initial-model resolution is gated on `cliProvider && cliModel`. Given just one,
it ignores that flag and falls through to the saved default, then to the first
provider it finds a key for. That can land on a provider the capture does not
cover, and then the session runs normally and records nothing.

```bash
tapesctl plugin install pi     # once; writes pi's capture extension
tapesctl start pi --tapes-url http://localhost:8081 -- --provider anthropic --model <model-id>
```

There is no `tapesctl` error for this — do not go looking for one to improve.
The observable symptom is a completed session with no captured turns. When
triaging "pi captured nothing", check the provider/model pair before anything
else. (`pi` does warn from inside the harness when the selected model's provider
is not covered.)

Also note `tapesctl start pi` with no `--upstream` routes each of pi's
Anthropic, OpenAI, and OpenAI Codex providers to its own upstream, so all three
are captured; an explicit `--upstream` collapses that to one.

### `--schema` applies to some harnesses and is an error on the rest

`tapesctl start` accepts `claude`, `codex`, and `pi`. `--schema` picks which
upstream API schema the proxy fronts, and it is only meaningful for a harness
that redirects several providers to one endpoint — of `start`'s three, that is
`pi`, which defaults to `anthropic` because that is the provider it ships
selected. A harness that speaks exactly one schema takes it from the harness,
and passing `--schema` there is a hard error, deliberately, rather than a silent
no-op:

```
$ tapesctl start claude --schema openai
tapesctl: --schema does not apply to claude, which speaks anthropic only (it is
for a harness that redirects several providers to one endpoint, such as pi)
```

### The harness registry is wider than what `start` accepts

Three surfaces, three different sets — don't unify them:

- the shared **registry** (in the `tapes-crates` dependency) knows `claude`,
  `codex`, `codex-app`, `opencode`, and `pi`, plus the aliases `claude-code`
  and `codex-desktop`; name matching is case-insensitive and trims whitespace;
- **`plugin install`** accepts the whole registry deliberately, so a harness
  that gains a plugin upstream becomes installable here without this repo being
  edited;
- **`start`** accepts only `claude`, `codex`, and `pi`.

`opencode` is *withdrawn* from `start`, not missing from it: it keeps its
registry entry, its plugin, and its arms in `start/`, because on the OAuth path
its plugin captures nothing, and a `start` that runs an agent while recording
none of it is worse than one that refuses. Those arms are pending work, not dead
code — resist tidying them away, and reinstating the verb is moving one entry
back into `SUPPORTED` in `start/mod.rs`.

### Codex desktop-app trust lives in the `codex` CLI

`tapesctl plugin install codex-app` registers a hook plugin and repoints
`~/.codex/config.toml`, but it cannot grant the plugin's hooks trust. The user
must run `/hooks` **in the `codex` CLI** — the desktop app has no `/hooks`
command and its Hooks settings page does not list plugin hooks, but trust is
shared state, so trusting once in the CLI covers the app. Trust binds to the
exact hook-definition hash, so any reinstall requires trusting again. Don't
document or automate this as an in-app step; it isn't one.

### `start` swallows unknown flags into the harness

`harness_args` is `trailing_var_arg = true, allow_hyphen_values = true`, so
anything after the harness name that `tapesctl` does not recognize is passed
through verbatim. That is the point — but it means a misspelled `tapesctl` flag
after the harness name is silently handed to the harness instead of rejected.
Put `tapesctl` flags before the harness name, or `--` before the harness's own:

```bash
tapesctl -v start claude --tapes-url http://localhost:8081 -- --model opus
```

### Vendored corpora and contracts are byte-for-byte copies

`crates/tapesctl/contracts/` and `crates/tapesctl/vendor/*/` are vendored from
published upstream artifacts and are **not** hand-editable. The fixture corpora
are sealed by a `DIGEST` that this repo's own suite recomputes, so an edited
case fails here rather than quietly making a red test green. Refresh them with
the `scripts/sync-*.sh` scripts, and check a contract pin with
`make contracts-check`. A change to the shared behavior belongs upstream first.

### The cassette surface is discovered, not compiled in

Which cassettes exist is deployment configuration, so `tapesctl` builds those
commands at runtime from the server's OpenAPI documents and caches the result
per server. Never add a hard-coded cassette list — it would freeze one
deployment's extensions into everyone's binary. Point the cache elsewhere with
`TAPESCTL_CACHE_DIR`.

### Commands refuse rather than guess a server

With no `--tapes-url`, no `TAPES_URL`, and no configured default, a command that
needs a server exits with an error instead of trying `localhost`. Preserve that:
a capture pointed at whatever happened to be listening is worse than one that
never started.

## Pull requests

Pull request titles must use one of the repository's accepted conventional
prefixes — for example `:sparkles: feat:`, `:wrench: fix:`, `:broom: chore:`,
`:recycle: refactor:`, or `:books: docs:` — optionally scoped, as in
`:wrench: fix(start): ...`.

A separate check looks for a maintainers' issue-tracker reference. It is
**not** expected on a pull request from a fork: that tracker is not readable
from outside the organization, so the check reports as not-applicable and
maintainers link the issue when they triage. Fork pull requests run the full
lint, test, build, and smoke matrix on GitHub-hosted runners.

By contributing you agree that your contribution is dual-licensed under MIT and
Apache-2.0, matching [the license on this repository](README.md#license).
