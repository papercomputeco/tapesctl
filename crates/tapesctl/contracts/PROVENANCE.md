# Vendored tapes ingest contract

`tapes-ingest.yaml` beside this file is the published tapes ingest OpenAPI
contract — the write surface (`POST /v1/ingest`, `POST /v1/ingest/transcript`) —
vendored byte-for-byte.

Nothing at runtime reads it: the capture path keeps its hand-written request
construction. `tests/ingest_conformance.rs` reads it and asserts that the
request tapesctl actually constructs — path, method, content type, envelope
field names — matches what the contract declares.

## Why the read contract is not here any more

`tapes-api.yaml` moved to the `tapes-client` crate in the tapes-crates
repository, which now owns its provenance and its seal check. Both clients that
speak the read API build against a published release asset rather than the
tapes working tree, so one vendored copy serves both — and a re-pin is one
change in one repository instead of two that nothing compares.

The ingest contract stays here because this is its only consumer: no other
client vendors it, and its one reader is a test in this crate.

## Pin

- **Unreleased** — papercomputeco/tapes, commit `0b41c3e` ("feat(sessions):
  resolve a fork parent in the parent's own harness"), which adds
  `parent_harness_id` to the ingest envelope. No release carries it yet, so
  there is no asset to vendor from: the file is the local emission
  `tapes dev openapi ingest --docs-root . --out <file>` at that commit.
- This is a loan, like a git pin of the tapes crates. Before it lands, re-pin
  to the first tapes release that contains `0b41c3e`: restore a
  `Release tag: **vX.Y.Z**` line with its commit and asset URL, re-vendor from
  the asset, and update the fingerprints below. Until then the release-asset
  gate has no tag to fetch, and `make contracts-check` is verified with
  `TAPES_REPO=<tapes checkout at 0b41c3e>`.
- Previous pin: release v0.34.0, commit `94b2ec7`.

Keep this pin in step with the read contract's, in
`read-contract/contracts/PROVENANCE.md` of the tapes-crates repository. The two
surfaces are emitted from one tapes commit, and vendoring them from different
releases would mean this CLI's capture path and its read path describe two
different servers.

## Fingerprints

Vendored file bytes (what `scripts/contracts-check.sh` verifies):

- `tapes-ingest.yaml` sha256 `6a887c807d63abda3f9c1b3333a06dd530fad1381110caa37c7d63a10ffba16b`

Prose-included document fingerprint (`CompiledDoc.Fingerprint()` as printed by
`tapes dev openapi`; the ETag a server would serve for the same document):

- ingest `sha256:bd7c0a623719524e2982f442e76909deab6d9ba08ed83ba92278a56702ea8c72`

Prose-stripped contract seal (the value in tapes `ingest/CONTRACT` at the pinned
commit; this changes only when the contract *shape* changes, so it is the identity
a doc-comment edit does not move):

- ingest `sha256:0125ac324043cea54759d2ada6d7663609c4b07983e41a32a23b07e8f4f17da5`

## Updating

1. Pick the tapes release tag you are bumping to and download its ingest
   contract asset:

   ```sh
   tag=v0.34.0   # the new tag
   base="https://github.com/papercomputeco/tapes/releases/download/${tag}"
   curl -fsSL -o tapes-ingest.yaml "${base}/tapes-ingest-${tag}.yaml"
   ```

2. Copy the YAML here verbatim — never hand-edit it. To confirm the copy
   matches the release before touching this file, run
   `TAPES_CONTRACT_TAG=<tag> make contracts-check` now: while the override
   names a tag other than the recorded pin, the fingerprint gate reports its
   expected mismatch informationally and only the release-asset byte diff
   decides.
3. Update the pin above (tag, commit, asset URL) and every fingerprint: the
   file-byte sha256 (`shasum -a 256`), the prose-included fingerprint (printed
   by `tapes dev openapi`, or served as the ETag), and the seal (tapes
   `ingest/CONTRACT` at the tag).
4. Run `make contracts-check` (strict again now that the pin matches) and
   `cargo test`.
5. Bump the read contract to the same tag in tapes-crates and re-pin this repo
   at the resulting revision. The read side's coverage gate will list any
   operation the new contract added so it can be mapped or deliberately
   allow-listed in `src/api/contract.rs`.

For offline work, or when developing against an unreleased tapes commit,
`scripts/contracts-check.sh` can instead re-emit the contract from a local tapes
checkout (`TAPES_REPO=/path/to/tapes`) — but a vendoring bump that lands here
must always pin a published tag and its asset.
