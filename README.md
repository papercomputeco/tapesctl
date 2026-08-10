# tapesctl

The command-line client for [Tapes](https://tapes.dev), written in Rust.

`tapesctl` launches coding-agent harnesses under a just-in-time capture proxy,
ships the captured turns to a tapes ingest server, and provides API access to
the tapes data model. It is built on the shared
[`tapes-harnesses`](https://github.com/papercomputeco/tapes-harnesses) crate —
the single, open-source home for client-side harness knowledge (launch,
attribution, transcript tailing, and the capture envelope) — which paperd also
consumes, so capture fidelity is identical between `tapesctl start` and
`paper start`.

See the "Tapes and Cassettes" RFC for the full design.

## Naming your server

Every command that talks to a tapes deployment needs to know where it is. There
are three ways to say so, and they are consulted in this order:

```bash
tapesctl --tapes-url http://localhost:8081 sessions list   # 1. the flag
export TAPES_URL=http://localhost:8081                     # 2. the environment
tapesctl config set tapes-url http://localhost:8081        # 3. once, for good
```

`--tapes-url` is global: give it before the subcommand, as above, or after it,
where it has always worked. The third form writes `~/.tapes/config.toml` and is
the one worth doing — a configured server is what makes `tapesctl --help` list
the cassette commands your deployment serves, in every new shell, without an
export.

```bash
tapesctl config get           # every key that is set
tapesctl config get tapes-url # one of them, bare, for scripts
tapesctl config path          # where the file is, whether or not it exists
```

With none of the three, commands that need a server refuse to run and say so.
They do not fall back to a guessed `localhost` port: a capture pointed at
whatever happened to be listening is worse than one that did not start.

## Capture

```bash
tapesctl start claude --tapes-url http://localhost:8081
```

This captures **two lanes**, and both matter:

- the **wire lane** — every LLM call, forwarded byte-for-byte through a
  loopback proxy that dies with the harness;
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
tapesctl -v start claude --tapes-url http://localhost:8081
```

Every other command logs to stderr as before.

```bash
tapesctl sync    # backstop: sweep transcripts no live tailer saw
```

`sync` is safe to run repeatedly — the ingest endpoint keys rows on a content
hash, so re-offering an unchanged transcript is a cheap `deduped`.

## Reading the data model

```bash
tapesctl sessions list --limit 20
tapesctl sessions get <session-id>
tapesctl sessions traces <session-id>     # what the console renders
tapesctl traces list <session-id>
tapesctl traces get <trace-id>
tapesctl spans list <trace-id>
tapesctl spans get <trace-id> <span-id>
```

Each prints the server's JSON verbatim, so it composes with `jq`. Every command
takes `--tapes-url`; see [Naming your server](#naming-your-server) for the two
ways not to have to pass it.

```bash
tapesctl export <session-id> -o bundle.jsonl
tapesctl seed                              # demo data for a fresh server
```

## Searching

```bash
tapesctl search "how to configure logging"
tapesctl search "error handling patterns" --top 10
tapesctl search "gum glow charm" --quiet   # session ids, one per line
```

Hits are individual main-conversation LLM spans with their trace and turn
context — "find the turn where X happened". This needs a server with span
embeddings written (`tapes serve`, its embed worker, or the `tapes dev
embed-spans` backfill); a deployment without them answers `503` rather than an
empty result set.

`--quiet` prints bare session ids in score order, which is what `skill generate`
takes as arguments:

```bash
tapesctl skill generate $(tapesctl search "charm CLI" --quiet --top 1) --name charm-patterns
```

## Skills

A skill is a markdown file with frontmatter under `~/.tapes/skills/`. Generate
one from captured sessions, list what you have, and install it where an agent
will look:

```bash
tapesctl skill generate <session-id> --name debug-react-hooks
tapesctl skill generate --search "react hooks" --search-top 3 --name react-debug
tapesctl skill generate <session-id> --name morning-work --since 2026-02-17
tapesctl skill list --type workflow
tapesctl skill sync debug-react-hooks --claude   # copy it into place
```

`generate` talks to two servers: `--tapes-url` for the session transcript, and
an LLM provider for the extraction. The provider is `--provider`
(`openai`, `anthropic`, or `ollama`), keyed from `--api-key` or the provider's
own environment variable — prefer the variable, since an argument is visible in
the process list. `--preview` renders the skill without writing it.

Skill files are written `0600`, and a skills path that resolves outside the
directory you selected is refused rather than followed.
## Cassettes

A tapes deployment can serve **cassettes** — independently built API extensions
mounted under `/v1/cassettes/<name>`. `tapesctl` discovers whichever ones your
server serves and turns them into commands, so the generated nouns and their
`--help` *are* the cassette listing:

```bash
tapesctl --help                            # lists the cassettes this server serves
tapesctl hello-world --help                # lists that cassette's methods
tapesctl hello-world get-hello
tapesctl hello-world create-hello --body '{"hello":"hi"}'
tapesctl hello-world create-hello --body @row.json
```

Method names are each operation's `operationId`, kebab-cased. Path parameters
become positional arguments and query parameters become flags, both taken from
the cassette's own OpenAPI document — so a cassette this binary has never heard
of still gets a correct, typed-ish command line.

Discovery is a **runtime** step, not a build-time one: which cassettes exist is
deployment configuration, so a compiled-in list would be one deployment's
cassettes frozen into everyone's binary. The discovered surface is cached per
server and revalidated on a timer (`ETag`/`If-None-Match`), so `--help` stays
instant and keeps working offline. Point it elsewhere with any of the three
sources in [Naming your server](#naming-your-server); override the cache
location with `TAPESCTL_CACHE_DIR`.

Because the listing comes from a server, `tapesctl --help` on a machine that
names none is a shorter help page than the same binary would print with one
configured — which is the strongest reason to run `tapesctl config set
tapes-url` once.

Without a reachable server there are simply no cassette nouns — the commands
above this section are unaffected. Deploying and configuring cassettes is an
operator task and is not part of this surface.

## Install

```bash
curl -sSfL https://download.tapes.dev/tapesctl/install | bash
```

Set `TAPESCTL_VERSION` to install a specific release or nightly build, and
`TAPESCTL_INSTALL_DIR` to override `/usr/local/bin`.

## Develop

The Nix flake dev shell pins the Rust toolchain (via `rust-toolchain.toml`):

```bash
nix develop
make build
make run ARGS=version
```

Run `make help` for all targets. Before opening a PR:

```bash
make lint   # cargo fmt --check + clippy -D warnings
make test
```

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

## Layout

- `crates/tapesctl` — the CLI binary.
  - `start/` — the just-in-time capture proxy (the wire lane).
  - `transcript/` — the transcript lane: live tailer and `sync` sweep.
  - `api/` — the `<resource> <method>` read client.
  - `cassette/` — the generated `<cassette> <method>` surface: discovery, the
    spec reducer, the cache, and clap synthesis.
  - `config.rs` — `~/.tapes/config.toml`: the answers you give once.
  - `ports/` — commands ported from the Go `tapes` CLI.

Shared client-side harness knowledge lives in its own repository,
[`tapes-harnesses`](https://github.com/papercomputeco/tapes-harnesses), and is
consumed by both tapesctl and paperd.
