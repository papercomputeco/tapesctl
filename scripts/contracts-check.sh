#!/usr/bin/env bash
# Verify the vendored tapes contracts are exactly the published bytes.
#
# Two gates, in order:
#
#   1. The vendored files still carry the fingerprints recorded in
#      contracts/PROVENANCE.md — a hand-edit of a vendored document fails
#      here even with no tapes checkout on the machine.
#   2. Re-emit both contracts from a tapes checkout and byte-diff them
#      against the vendored copies — a contract bump in tapes that has not
#      been vendored here fails here.
#
# TODO(tag): no tapes release with contract assets exists yet. Once one does,
# gate 2 becomes a fetch of the release assets instead of a local emission:
#
#   base="https://github.com/papercomputeco/tapes/releases/download/${TAPES_CONTRACT_TAG}"
#   curl -fsSL "${base}/tapes-api-${TAPES_CONTRACT_TAG}.yaml"    | diff - "${vendored}/tapes-api.yaml"
#   curl -fsSL "${base}/tapes-ingest-${TAPES_CONTRACT_TAG}.yaml" | diff - "${vendored}/tapes-ingest.yaml"
#
# Until then the emission needs a tapes checkout (and its Go toolchain):
#   TAPES_REPO=/path/to/tapes scripts/contracts-check.sh
# With no TAPES_REPO and no checkout at ../tapes, gate 2 is skipped with a
# notice so gate 1 still runs everywhere.

set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
vendored="${here}/crates/tapesctl/contracts"
provenance="${vendored}/PROVENANCE.md"

# The commit the vendored bytes were emitted from, per PROVENANCE.md.
pinned_commit="dc8a109"

fail=0

# --- gate 1: recorded fingerprints -------------------------------------------
for name in tapes-api.yaml tapes-ingest.yaml; do
  recorded="$(grep -oE "\`${name}\` sha256 \`[0-9a-f]{64}\`" "${provenance}" \
    | grep -oE '[0-9a-f]{64}')"
  actual="$(shasum -a 256 "${vendored}/${name}" | awk '{print $1}')"
  if [ "${recorded}" != "${actual}" ]; then
    echo "FAIL: ${name} does not match the fingerprint recorded in PROVENANCE.md" >&2
    echo "  recorded: ${recorded}" >&2
    echo "  actual:   ${actual}" >&2
    fail=1
  else
    echo "ok: ${name} matches its recorded fingerprint"
  fi
done

# --- gate 2: re-emit and diff ------------------------------------------------
tapes_repo="${TAPES_REPO:-${here}/../tapes}"
if [ ! -f "${tapes_repo}/cli/tapes/main.go" ]; then
  echo "notice: no tapes checkout at ${tapes_repo} (set TAPES_REPO); skipping the re-emission diff"
  exit "${fail}"
fi

head_commit="$(git -C "${tapes_repo}" rev-parse HEAD 2>/dev/null || echo unknown)"
case "${head_commit}" in
  "${pinned_commit}"*) ;;
  *)
    echo "notice: ${tapes_repo} is at ${head_commit}, not the pinned ${pinned_commit};" >&2
    echo "        a diff below may be a pending contract bump rather than corruption" >&2
    ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

(
  cd "${tapes_repo}"
  GOEXPERIMENT=jsonv2 go run ./cli/tapes dev openapi api --docs-root . --out "${tmp}/tapes-api.yaml"
  GOEXPERIMENT=jsonv2 go run ./cli/tapes dev openapi ingest --docs-root . --out "${tmp}/tapes-ingest.yaml"
)

for name in tapes-api.yaml tapes-ingest.yaml; do
  if ! diff -u "${vendored}/${name}" "${tmp}/${name}"; then
    echo "FAIL: vendored ${name} differs from the emission at ${head_commit}" >&2
    fail=1
  else
    echo "ok: ${name} matches the re-emission"
  fi
done

exit "${fail}"
