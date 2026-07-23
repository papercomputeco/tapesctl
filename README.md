# tapesctl

The command-line client for [Tapes](https://tapes.dev), written in Rust.

`tapesctl` launches coding-agent harnesses under a just-in-time capture proxy,
ships the captured turns to a tapes ingest server, and provides API access to
the tapes data model. It is built on the shared [`tapes-harness`](crates/tapes-harness)
crate — the single, open-source home for client-side harness knowledge (launch,
attribution, transcript tailing, and the capture envelope) — which paperd also
consumes, so capture fidelity is identical between `tapesctl start` and
`paper start`.

See the "Tapes and Cassettes" RFC for the full design. This repository is an
early bootstrap: the CLI skeleton and crate seams are in place; the JIT proxy,
attribution extraction from paperd, and the generated cassette client surface
are the next steps (Track 1 / Track 4).

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
- `crates/tapes-harness` — shared client-side harness knowledge (consumed by
  both tapesctl and paperd).
