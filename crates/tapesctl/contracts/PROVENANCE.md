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

- Source: papercomputeco/tapes, commit `dc8a109` ("feat: seal and publish the
  OpenAPI contracts (#292)").
- Release tag: **TODO(tag)** — no tapes release with contract assets exists
  yet. The first release after `dc8a109` will attach
  `tapes-api-<tag>.yaml` / `tapes-ingest-<tag>.yaml`; when it does, replace
  this marker with the tag and point `scripts/contracts-check.sh` at the
  release asset URLs instead of a local emission.
- Emitted with: `tapes dev openapi <surface> --docs-root . --out <file>` — the
  exact command `dagger call contracts` (`make contracts` in tapes) runs, so
  these bytes are what the release will attach.

## Fingerprints

Vendored file bytes (what `scripts/contracts-check.sh` verifies):

- `tapes-api.yaml` sha256 `c21e62f8e8e83fea32a8542ea624e8dbf646950240f1fb2e9433043fffecedc2`
- `tapes-ingest.yaml` sha256 `cf911335ce8ce1b5c774d4032f68eb85ee3c35cb84e99f5246f12d2ae9b4f13e`

Prose-included document fingerprints (`CompiledDoc.Fingerprint()` as printed by
`tapes dev openapi`; the ETag a server would serve for the same document):

- api `sha256:2ecf90bd299960336be07198cb75b0a19620809792a80ee3cedcda40832201f5`
- ingest `sha256:9a19d7e188442acbf111b98b67e1741262585ffb503803a7e4e693346d2837c9`

Prose-stripped contract seals (the values in tapes `api/CONTRACT` and
`ingest/CONTRACT` at the pinned commit; these change only when the contract
*shape* changes, so they are the identity a doc-comment edit does not move):

- api `sha256:c966a65a6c3ab126a908e9a7db55905323686e50077441f52f5675752c9ff8ea`
- ingest `sha256:b51bebaecb5e0942cd2fba11d694126e36c6a4e33088a77b8171eb8c3acecfb7`

## Updating

1. Run `make contracts` in a tapes checkout (or `tapes dev openapi ...` as
   above) at the commit/tag you are bumping to.
2. Copy the two YAMLs here verbatim — never hand-edit them.
3. Update every fingerprint above and the pin (commit, and the tag once one
   exists).
4. Run `make contracts-check` and `cargo test` — the operation coverage gate
   will list any operation the new contract added so it can be mapped or
   deliberately allow-listed.
