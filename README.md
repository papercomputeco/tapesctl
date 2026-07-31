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

See the "Tapes and Cassettes" RFC for the full design. The *generated*
`<cassette> <method>` surface is still to come, with `/v1/cassettes` discovery
and OpenAPI client generation (Track 4).

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
takes `--tapes-url`, falling back to `TAPES_URL`.

```bash
tapesctl export <session-id> -o bundle.jsonl
tapesctl seed                              # demo data for a fresh server
tapesctl skill sync <name> --claude        # copy an authored skill into place
```

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
  - `ports/` — commands ported from the Go `tapes` CLI.

Shared client-side harness knowledge lives in its own repository,
[`tapes-harnesses`](https://github.com/papercomputeco/tapes-harnesses), and is
consumed by both tapesctl and paperd.
