# Vendored tapes contracts

The two YAML documents beside this file are the published tapes OpenAPI
contracts, vendored byte-for-byte:

- `tapes-api.yaml` — the read API (`tapesctl sessions|traces|spans|search|export|seed`)
- `tapes-ingest.yaml` — the ingest write surface (`POST /v1/ingest`,
  `POST /v1/ingest/transcript`)

They are consumed two ways:

- `src/api/contract.rs` embeds `tapes-api.yaml` and reduces it with the same
  OpenAPI→CLI reducer the runtime-discovered cassette surface uses; the core
  read commands build their requests from it rather than from hand-written URL
  builders.
- `tests/ingest_conformance.rs` reads `tapes-ingest.yaml` and asserts the
  request tapesctl's capture path actually constructs — path, method,
  content type, envelope field names — matches it.

## Pin

- Release tag: **v0.34.0** — papercomputeco/tapes, commit `94b2ec7`
  ("feat: MCP cassettes (#289)"). This is the first tapes release that
  attaches the compiled contracts as assets.
- Vendored from the release assets, byte-for-byte:
  - <https://github.com/papercomputeco/tapes/releases/download/v0.34.0/tapes-api-v0.34.0.yaml>
  - <https://github.com/papercomputeco/tapes/releases/download/v0.34.0/tapes-ingest-v0.34.0.yaml>
- The assets are what `tapes dev openapi <surface> --docs-root . --out <file>`
  emits at the tag — the exact command `dagger call contracts`
  (`make contracts` in tapes) runs; a local emission at `94b2ec7` was verified
  byte-identical to both assets.

## Fingerprints

Vendored file bytes (what `scripts/contracts-check.sh` verifies):

- `tapes-api.yaml` sha256 `e6b358bdb5169475f24ea946cbf8e8567ca85240e863b744e8e2f33320a29bab`
- `tapes-ingest.yaml` sha256 `cf911335ce8ce1b5c774d4032f68eb85ee3c35cb84e99f5246f12d2ae9b4f13e`

Prose-included document fingerprints (`CompiledDoc.Fingerprint()` as printed by
`tapes dev openapi`; the ETag a server would serve for the same document):

- api `sha256:9da9223f51ab7d3c0333725f6abfd8279465c7d5e970e1e8a589fce789362853`
- ingest `sha256:9a19d7e188442acbf111b98b67e1741262585ffb503803a7e4e693346d2837c9`

Prose-stripped contract seals (the values in tapes `api/CONTRACT` and
`ingest/CONTRACT` at the pinned tag; these change only when the contract
*shape* changes, so they are the identity a doc-comment edit does not move):

- api `sha256:c966a65a6c3ab126a908e9a7db55905323686e50077441f52f5675752c9ff8ea`
- ingest `sha256:b51bebaecb5e0942cd2fba11d694126e36c6a4e33088a77b8171eb8c3acecfb7`

## Updating

1. Pick the tapes release tag you are bumping to and download its contract
   assets:

   ```sh
   tag=v0.34.0   # the new tag
   base="https://github.com/papercomputeco/tapes/releases/download/${tag}"
   curl -fsSL -o tapes-api.yaml    "${base}/tapes-api-${tag}.yaml"
   curl -fsSL -o tapes-ingest.yaml "${base}/tapes-ingest-${tag}.yaml"
   ```

2. Copy the two YAMLs here verbatim — never hand-edit them. To confirm the
   copies match the release before touching this file, run
   `TAPES_CONTRACT_TAG=<tag> make contracts-check` now: while the override
   names a tag other than the recorded pin, the fingerprint gate reports its
   expected mismatches informationally and only the release-asset byte diff
   decides.
3. Update the pin above (tag, commit, asset URLs) and every fingerprint:
   the file-byte sha256s (`shasum -a 256`), the prose-included fingerprints
   (printed by `tapes dev openapi`, or served as the ETag), and the seals
   (tapes `api/CONTRACT` / `ingest/CONTRACT` at the tag).
4. Run `make contracts-check` (strict again now that the pin matches) and
   `cargo test` — the operation coverage gate will list any operation the new
   contract added so it can be mapped or deliberately allow-listed.

For offline work, or when developing against an unreleased tapes commit,
`scripts/contracts-check.sh` can instead re-emit the contracts from a local
tapes checkout (`TAPES_REPO=/path/to/tapes`) — but a vendoring bump that
lands here must always pin a published tag and its assets.
