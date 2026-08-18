# tapesctl

The command-line client for [Tapes](https://tapes.dev), written in Rust.

Tapes records what coding agents actually did: every LLM call an agent made, the
tools it ran, and the shape of the work — as sessions, traces, and spans you can
read, search, and export. `tapesctl` is the client. It launches a coding-agent
harness under a just-in-time capture proxy, ships the captured turns to a tapes
server, and gives you a command line over the data model that comes back.

You bring your own tapes server. Read commands use `--api-url`; capture
commands use `--ingest-url`. [Naming your server](#naming-your-server) is the
one-time way to stop typing either.

This README is the tour. The reference — every command, its flags, the capture
matrix, and what each failure mode means — is at
[tapes.dev/docs/tapesctl](https://tapes.dev/docs/tapesctl/).

## Install

```bash
curl -sSfL https://download.tapes.dev/tapesctl/install | bash
```

Every published artifact carries a `.sha256` sidecar. Where `sha256sum` or
`shasum` is available, the installer verifies the download against that sidecar
before installing, and a missing sidecar is a hard failure rather than a skipped
check; with neither tool present it warns and installs unverified. Binaries land
in `/usr/local/bin` (via `sudo` only if that directory is not writable). Set
`TAPESCTL_VERSION` to install a specific release or nightly, and
`TAPESCTL_INSTALL_DIR` to install somewhere else.

Confirm it landed:

```bash
tapesctl version
```

Supported platforms are Linux and macOS, on x86-64 and arm64.

## Your first capture

`start` launches a harness the way you normally would, with a capture proxy in
front of it. The harness behaves exactly as it would unproxied — traffic is
forwarded to its own provider API by default — and the proxy dies with it.

```bash
tapesctl start claude --ingest-url http://localhost:8082
```

The supported harnesses are `claude`, `codex`, and `pi`. Anything after the
harness name is passed through verbatim, so your usual flags still work:

```bash
tapesctl start claude --ingest-url http://localhost:8082 -- --model opus
```

A capture records **two lanes**, and both matter:

- the **wire lane** — every LLM call, forwarded byte-for-byte through a
  loopback proxy;
- the **transcript lane** — the harness's own on-disk transcripts, tailed live
  and pushed as they settle.

Only the transcript lane carries a session's causal skeleton: which `Task`
tool_use forked which subagent. A capture without it records every call a
subagent made but renders that work as flat dispatch text instead of nested
rows. Pass `--no-transcripts` only when another capture client is already
tailing the same tree.

While a harness is running it owns the terminal, so `start` writes its
diagnostics to a file instead of the screen — a stray log line lands in the
middle of a TUI frame. The path is printed before the harness launches and
again when it exits:

```bash
~/.tapes/logs/start-<timestamp>-<pid>.log
```

`RUST_LOG` sets the level as usual. Pass `-v` (before the subcommand) to stream
to stderr instead of a file, accepting what that does to the display:

```bash
tapesctl -v start claude --ingest-url http://localhost:8082
```

Every other command logs to stderr as before.

```bash
tapesctl sync    # backstop: sweep transcripts no live tailer saw
```

`sync` is safe to run repeatedly — the ingest endpoint keys rows on a content
hash, so re-offering an unchanged transcript is a cheap `deduped`. It sweeps
`~/.claude/projects` by default (`--projects-root` to point elsewhere), and
`--since-days` bounds how far back it looks.

### Capturing `pi`

`pi` needs its capture plugin installed once before `start` can capture it:

```bash
tapesctl plugin install pi
tapesctl start pi --ingest-url http://localhost:8082 -- --provider anthropic --model <model-id>
```

**Pass both `--provider` and `--model`, or neither.** Those are `pi`'s own
flags, not `tapesctl`'s, which is why they come after `--`. `pi` only honours
them as a pair; given just one it ignores it and falls back to your saved
default or the first provider it finds a key for — which may be a provider this
capture does not cover, so the session runs and records nothing. `pi` warns
inside the harness when the selected model's provider is not covered.

A plain `tapesctl start pi` routes each of pi's Anthropic, OpenAI, and OpenAI
Codex providers to its own upstream, so all three are captured. `--schema`
(`anthropic`, the default, or `openai`) picks which schema the capture fronts;
an explicit `--upstream` sends everything to one place instead. A harness that
speaks exactly one schema takes it from the harness, and passing `--schema`
there is an error rather than a silent no-op.

## Naming your server

Every command that talks to a tapes deployment resolves its endpoint in this
order: flag, environment, config, local default.

```bash
tapesctl --api-url http://localhost:8081 sessions list   # read API flag
export TAPES_API_URL=http://localhost:8081                 # read API environment
tapesctl config set api-url http://localhost:8081        # persist read API
tapesctl start claude --ingest-url http://localhost:8082   # ingest flag
export TAPES_INGEST_URL=http://localhost:8082              # ingest environment
tapesctl config set ingest-url http://localhost:8082       # persist ingest
```

`--api-url` is global: give it before the subcommand, as above, or after it.
Config writes `~/.tapes/config.toml` and is useful for non-local deployments in
every new shell, without an export.

```bash
tapesctl config get           # every key that is set
tapesctl config get api-url # one of them, bare, for scripts
tapesctl config path          # where the file is, whether or not it exists
```

`config set` edits the file in place rather than rewriting it, so your comments,
your ordering, and any keys this build does not know about — a key a newer
tapesctl wrote, say — all survive. The server must be an `http` or `https` URL;
anything else is refused when you set it rather than on every command afterwards.

Without configuration, read commands use `http://localhost:8081` and capture
commands use `http://localhost:8082`.

## Your first read

```bash
tapesctl sessions list --limit 20
tapesctl sessions get <session-id>
tapesctl sessions traces <session-id>     # what the console renders
tapesctl sessions raw-turns <session-id>  # the wire turns behind the derivation
tapesctl traces list <session-id>
tapesctl traces get <trace-id>
tapesctl spans list <trace-id>
tapesctl spans get <trace-id> <span-id>
```

Each prints the server's JSON verbatim, so it composes with `jq`. `sessions
list` pages with `--limit`/`--cursor` and narrows with `--sort`,
`--direction`, `--since`, `--until`, and `--auth-subject`; a cursor is only
valid with the `--sort` and `--direction` it was minted under. `sessions
traces` and `spans list` take `--payload preview` to truncate payload strings
server-side.

```bash
tapesctl export <session-id> -o bundle.jsonl   # --detail spans (default) or traces
tapesctl seed                                  # demo data for a fresh server
```

## Capturing the Codex desktop app

An app you launch from the dock starts itself, so there is no process for
`start` to own. Install a plugin once, then run a proxy for as long as you want
the app captured.

```bash
tapesctl plugin install codex-app --api-url http://localhost:8081
tapesctl capture codex-app --ingest-url http://localhost:8082
```

`plugin install` packages a hook plugin under `~/.tapes/codex-app/`, points
`~/.codex/config.toml` at a loopback port recorded at install time, and
registers the plugin with the `codex` CLI. Because that endpoint outlives any
one capture, the port cannot be ephemeral the way `start`'s is — pass `--port`
to pin it, or re-run with an explicit one to move off a port something else has
taken. `--dry-run` reports exactly what would be written, and where, without
writing anything. `--codex-auth` selects which credential is presented upstream:
`chatgpt` (the default, what the app uses after a plan login) or `api-key`.

Two steps are yours, and the command prints them:

1. Restart the Codex app, then enable the plugin in the app's Installed list.
2. **In the `codex` CLI**, run `/hooks` and trust the plugin's hooks. The app
   has no `/hooks` command and its Hooks settings page does not list plugin
   hooks, but trust is shared state, so trusting once in the CLI covers the app
   too. Trust binds to the exact hook-definition hash, so a reinstall requires
   trusting again.

`tapesctl plugin uninstall codex-app` removes the configuration and state it
wrote, but leaves the plugin registered with Codex; it prints a
`codex plugin remove ...` command to run for that last step. It also takes
`--dry-run`.

Harnesses captured by redirection alone need no plugin and say so:

```
$ tapesctl plugin install claude
tapesctl: claude needs no capture plugin — its traffic is captured by
redirecting it, which `tapesctl start claude` does.
```

`plugin install` knows `claude`, `codex`, `codex-app`, `opencode`, and `pi`.

## Searching

```bash
tapesctl search "how to configure logging"
tapesctl search "error handling patterns" --top 10
tapesctl search "gum glow charm" --quiet   # session ids, one per line
```

Hits are individual main-conversation LLM spans with their trace and turn
context — "find the turn where X happened". This needs a server with span
embeddings written; a deployment without them answers `503` rather than an
empty result set.

`--quiet` prints bare session ids in score order, ready to compose into other
commands through a shell substitution.

## Skills

Skills are served by the **skills cassette** — a tapes API extension that
stores, versions, and generates skills server-side. When a deployment serves
it, `tapesctl` discovers it like any other cassette and the whole surface
appears as generated commands, always in step with what the server actually
runs:

```bash
tapesctl cassettes skills list-skills
tapesctl cassettes skills generate-skill \
  --body '{"sessionIds": ["<session-id>"], "hint": {"name": "debug-react-hooks"}}'
tapesctl cassettes skills get-skill-markdown <id>
```

Generation runs on the server, against the LLM the deployment configured — no
client-side provider keys. A deployment without the skills cassette has no
skills surface; there is no local fallback. (Earlier tapesctl versions
authored skills locally under `~/.tapes/skills/`; that second implementation
is gone, and any files there are yours to keep or delete.)

## Cassettes

A tapes deployment can serve **cassettes** — independently built API extensions
mounted under `/v1/cassettes/<name>`. `tapesctl` discovers whichever ones your
server serves and mounts them under `tapesctl cassettes`, so the noun and its
`--help` *are* the cassette listing:

```bash
tapesctl cassettes                                     # what this server serves
tapesctl cassettes hello-world --help                  # that cassette's methods
tapesctl cassettes hello-world get-hello
tapesctl cassettes hello-world create-hello --body '{"hello":"hi"}'
tapesctl cassettes hello-world create-hello --body @row.json
```

Method names are each operation's `operationId`, kebab-cased. Path parameters
become positional arguments and query parameters become flags, both taken from
the cassette's own OpenAPI document — so a cassette this binary has never heard
of still gets a correct, typed-ish command line.

Discovery is a **runtime** step, not a build-time one: which cassettes exist is
deployment configuration, so a compiled-in list would be one deployment's
cassettes frozen into everyone's binary. The discovered surface is cached per
server and revalidated on a timer (`ETag`/`If-None-Match`), so `--help` stays
instant and keeps working offline. Override the cache location with
`TAPESCTL_CACHE_DIR`.

Because the listing comes from a server, `tapesctl cassettes` on a machine that
names none lists nothing at all — which is the strongest reason to run
`tapesctl config set api-url` once. Everything above this section is
unaffected. Deploying and configuring cassettes is an operator task and is not
part of this surface.

Cassettes used to mount as top-level nouns (`tapesctl hello-world get-hello`).
That spelling shipped one release as a hidden alias and has been removed: it
now fails like any other unknown command. Write `tapesctl cassettes <name>
<method>`. Retiring it is also what makes every non-cassette command start
without touching the discovery cache or the network at all.

## Develop

The Nix flake dev shell pins the Rust toolchain (via `rust-toolchain.toml`):

```bash
nix develop
make build
make run ARGS=version
```

Run `make help` for all targets. Before opening a pull request:

```bash
make lint   # cargo fmt --check + clippy -D warnings
make test
```

See [AGENTS.md](AGENTS.md) for repository layout, the conventions the workspace
enforces, and the traps worth knowing before your first change.

## CI & release

CI runs through Dagger (`.dagger/`), so it reproduces locally:

```bash
make ci     # dagger call lint + test (the PR gates)
make dist   # cross-compile all four release targets into ./build
```

Release binaries are cross-compiled from Linux with `cargo-zigbuild` — a pure
CLI with no Apple frameworks needs no macOS SDK. Targets: `linux/{amd64,arm64}`
(static musl) and `darwin/{amd64,arm64}` (Mach-O). Tagged releases and nightlies
publish to `download.tapes.dev` via the `release` / `nightly` Dagger functions.
A release publishes `install.sh` in the same pipeline call as the binaries,
after them. The object-store syncs are still separate — a late failure can
leave new binaries public with a stale installer — but that failure fails the
release, so a cut never reports success while the served installer is stale.

## Layout

- `crates/tapesctl` — the CLI binary.
  - `start/` — the just-in-time capture proxy (the wire lane).
  - `transcript/` — the transcript lane: live tailer and `sync` sweep.
  - `codex_app/` — the plugin and proxy for a harness that launches itself.
  - `api/` — the `<resource> <method>` read client.
  - `cassette/` — the generated `cassettes <name> <method>` surface: discovery,
    the spec reducer, the cache, and clap synthesis.
  - `config.rs` — `~/.tapes/config.toml`: the answers you give once.
  - `machine.rs` — the crate's one ambient read of the environment.
  - `ports/` — search, skills, and seed.

Shared client-side code — launch recipes, session attribution, transcript
discovery, the capture envelope, and the tapes read client itself (its
vendored contract, its response models, and the transport they travel over) —
lives in its own repository,
[`tapes-crates`](https://github.com/papercomputeco/tapes-crates), and is
consumed here as a pinned dependency. What stays in `api/` is what is a
command line's rather than a client's: which operations this CLI exposes, and
how their answers are printed.

## Contributing

Contributions are welcome — see [AGENTS.md](AGENTS.md) for how to build, test,
and shape a pull request.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
