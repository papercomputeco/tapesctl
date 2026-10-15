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

- **Unreleased** — papercomputeco/tapes, commit `91ce34e` (papercomputeco/tapes#393) ("feat(ingest):
  advertise capabilities on the ingest ping"). It carries `parent_harness_id`
  on the ingest envelope (from `40c9034`) and the `capabilities` list on
  `GET /ping` that tapesctl probes before sending it. No release carries them
  yet, so there is no asset to vendor from: the file is the local emission
  `tapes dev openapi ingest --docs-root . --out <file>` at that commit.
- This is a loan, like a git pin of the tapes crates. Before it lands, re-pin
  to the first tapes release that contains `91ce34e` (papercomputeco/tapes#393): restore a
  `Release tag: **vX.Y.Z**` line with its commit and asset URL, re-vendor from
  the asset, and update the fingerprints below. Until then the release-asset
  gate has no tag to fetch, and `make contracts-check` is verified with
  `TAPES_REPO=<tapes checkout at 91ce34e>`.
- Previous pin: release v0.34.0, commit `94b2ec7`.

Keep this pin in step with the read contract's, in
`read-contract/contracts/PROVENANCE.md` of the tapes-crates repository. The two
surfaces are emitted from one tapes commit, and vendoring them from different
releases would mean this CLI's capture path and its read path describe two
different servers.

## Fingerprints

Vendored file bytes (what `scripts/contracts-check.sh` verifies):

- `tapes-ingest.yaml` sha256 `873435d162b247bb84d180544994e9f6ed1de42b72bfcdc8bab15b76e56484e0`

Prose-included document fingerprint (`CompiledDoc.Fingerprint()` as printed by
`tapes dev openapi`; the ETag a server would serve for the same document):

- ingest `sha256:f4e40a055c3001356cdcde64b00e56d79b13bba864aab648d41a4ba1b70d292d`

Prose-stripped contract seal (the value in tapes `ingest/CONTRACT` at the pinned
commit; this changes only when the contract *shape* changes, so it is the identity
a doc-comment edit does not move):

- ingest `sha256:5f10421256e7d2cc9c281d1ec11fde04fed744edd78cc06ee0dd699a06f468a3`

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
