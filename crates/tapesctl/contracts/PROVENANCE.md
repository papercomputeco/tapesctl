# Vendored tapes ingest contract

`tapes-ingest.yaml` beside this file is the published tapes ingest OpenAPI
contract — the write surface (`POST /v1/ingest`, `POST /v1/ingest/transcript`) —
vendored byte-for-byte.

Nothing at runtime reads it: the capture path keeps its hand-written request
construction. `tests/ingest_conformance.rs` reads it and asserts that the
request tapesctl actually constructs — path, method, content type, envelope
field names — matches what the contract declares.

## Why the read contract is not here any more

`tapes-api.yaml` moved to the `tapes-read-contract` crate in the tapes-crates
repository (PCC-1146), which now owns its provenance and its seal check. Both
clients that speak the read API build against a published release asset rather
than the tapes working tree, so one vendored copy serves both — and a re-pin is
one change in one repository instead of two that nothing compares.

The ingest contract stays here because this is its only consumer: no other
client vendors it, and its one reader is a test in this crate.

## Pin

- Release tag: **v0.34.0** — papercomputeco/tapes, commit `94b2ec7`
  ("feat: MCP cassettes (#289)"). This is the first tapes release that
  attaches the compiled contracts as assets.
- Vendored from the release asset, byte-for-byte:
  - <https://github.com/papercomputeco/tapes/releases/download/v0.34.0/tapes-ingest-v0.34.0.yaml>
- The asset is what `tapes dev openapi ingest --docs-root . --out <file>` emits
  at the tag — the exact command `dagger call contracts` (`make contracts` in
  tapes) runs; a local emission at `94b2ec7` was verified byte-identical to the
  asset.

Keep this pin in step with the read contract's, in
`read-contract/contracts/PROVENANCE.md` of the tapes-crates repository. The two
surfaces are emitted from one tapes commit, and vendoring them from different
releases would mean this CLI's capture path and its read path describe two
different servers.

## Fingerprints

Vendored file bytes (what `scripts/contracts-check.sh` verifies):

- `tapes-ingest.yaml` sha256 `cf911335ce8ce1b5c774d4032f68eb85ee3c35cb84e99f5246f12d2ae9b4f13e`

Prose-included document fingerprint (`CompiledDoc.Fingerprint()` as printed by
`tapes dev openapi`; the ETag a server would serve for the same document):

- ingest `sha256:9a19d7e188442acbf111b98b67e1741262585ffb503803a7e4e693346d2837c9`

Prose-stripped contract seal (the value in tapes `ingest/CONTRACT` at the pinned
tag; this changes only when the contract *shape* changes, so it is the identity
a doc-comment edit does not move):

- ingest `sha256:b51bebaecb5e0942cd2fba11d694126e36c6a4e33088a77b8171eb8c3acecfb7`

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
