# `tapes-envelope-fixtures/` — vendoring + sync

`cases/*.json` and `README.upstream.md` are a **verbatim copy** of the shared
envelope fixture corpus authored in the `tapes` repository. Do not hand-edit
them here — a change belongs upstream, and this copy is refreshed with
`scripts/sync-envelope-fixtures.sh`.

## Source

* **Repo:** `tapes` (Paper Compute).
* **Path within repo:** `fixtures/envelope/`.
* **Current snapshot SHA:** `9cc71917af25b9a1d6a411b2691bc011d8d32610`
  ("✨ feat: ingest fidelity, published contracts, and the first fixture-grade
  wire recordings (#267)") — the last commit to touch `fixtures/envelope`. It
  revised three cases (`metadata-oversize-dropped`, `session-name-ascii-cap`,
  `session-name-truncated-utf8`) after the corpus was established in `39a98df`
  (#263). TODO: replace with a tagged fixture-cut id once tapes publishes
  versioned cuts (`fixtures/manifest.json` reserves the `cut` block for exactly
  that).

The snapshot is the contract: it is pinned to a specific upstream commit, and a
refresh lands in the same PR as whatever consumer change it forces.

## What it pins

The `X-Tapes-*` header ↔ session-envelope contract. The contract has sides that
live in different repositories and different languages, and drift between them
is invisible until a session lands mis-attributed:

* **Producer** (this crate) — `envelope::inject_tapes_attribution` turns a
  resolved session identity into the on-wire header set: percent-encoding, the
  256-byte session-name cap, base64url metadata, and the 8 KiB budget.
* **Parser** — `tapes-extproc`'s `ParseSessionEnvelope`, and the tapes ingest
  reader, read that header set back into an envelope.

Every side table-tests against this one corpus. There are three vendored copies
in total and **all must be refreshed from the same upstream SHA**:

| Repo | Path |
| --- | --- |
| `tapesctl` (here) | `crates/tapes-harness/vendor/tapes-envelope-fixtures/` |
| `platform/paper` | `crates/paper-daemon/vendor/tapes-envelope-fixtures/` |
| `tapes-extproc` | `internal/headers/testdata/envelope/` |

## What consumes it here

`crates/tapes-harness/src/envelope_fixtures.rs` — the producer-side oracle. For
each case whose `direction` is `roundtrip` or `encode` it builds the logical
envelope (`encode_from` when the case is lossy, else `envelope`), emits the
headers, and asserts they match the case byte for byte.

`direction: decode` cases are parser-only — malformed or missing-header input a
well-behaved producer never emits — and are skipped here by design. They are
covered by the parser-side oracles.

## How to refresh

```sh
# from a tapes checkout at the target commit
./scripts/sync-envelope-fixtures.sh /path/to/tapes

# or detect drift without writing anything
./scripts/sync-envelope-fixtures.sh --check /path/to/tapes
```

Then update the snapshot SHA above, run `cargo test -p tapes-harness`, and
commit the fixture change with any producer change it forced.
