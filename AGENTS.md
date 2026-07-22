# Contributing

`tapesctl` is a Rust Cargo workspace. The Nix flake dev shell is the
recommended development environment; it pins the Rust toolchain via
`rust-toolchain.toml`.

```bash
nix develop
make build
make run -- version
```

Before opening a pull request, run:

```bash
make lint   # cargo fmt --all --check + cargo clippy --workspace -D warnings
make test   # cargo test --workspace
```

The workspace denies `unwrap`, `expect`, and `panic` via `[workspace.lints]`;
return `Result` and surface errors through the crate error types instead.

## Layout

- `crates/tapesctl` — the CLI binary.
- `crates/tapes-harness` — shared client-side harness knowledge (launch,
  attribution, transcript tailing, capture envelope), consumed by both tapesctl
  and paperd.

## Pull requests

Pull request titles must use one of the repository's accepted contribution
labels, such as `✨ feat:`, `🔧 fix:`, `🧹 chore:`, or `📚 docs:`, and reference
the relevant Linear issue with a magic word (e.g. `fixes PCC-123` or
`related to PCC-123`).
