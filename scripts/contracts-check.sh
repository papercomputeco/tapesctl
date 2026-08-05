#!/usr/bin/env bash
# Verify the vendored tapes contracts are exactly the published bytes.
#
# Two gates, in order:
#
#   1. The vendored files still carry the fingerprints recorded in
#      contracts/PROVENANCE.md — a hand-edit of a vendored document fails
#      here even with no network and no tapes checkout on the machine.
#   2. Fetch the contract assets from the pinned tapes release and byte-diff
#      them against the vendored copies — a vendoring that does not match the
#      published release fails here.
#
# Gate 2 prefers the release assets (the published source of truth):
#
#   https://github.com/papercomputeco/tapes/releases/download/<tag>/tapes-api-<tag>.yaml
#   https://github.com/papercomputeco/tapes/releases/download/<tag>/tapes-ingest-<tag>.yaml
#
# The tag defaults to the pin recorded in PROVENANCE.md; override with
# TAPES_CONTRACT_TAG when checking a bump before the docs are updated.
#
# Fallback for offline work or development against an unreleased tapes commit:
# set TAPES_REPO=/path/to/tapes to re-emit both contracts from that checkout
# (needs its Go toolchain) and diff the emission instead. When the fetch fails
# and no checkout is available, gate 2 is skipped with a notice so gate 1
# still runs everywhere.

set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
vendored="${here}/crates/tapesctl/contracts"
provenance="${vendored}/PROVENANCE.md"

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

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

# --- gate 2 (fallback): re-emit from a tapes checkout ------------------------
# Explicit TAPES_REPO opts into the local-emission path — development against
# a tapes commit that has no release yet.
emit_and_diff() {
  local tapes_repo="$1"

  # The tag the vendored bytes were published under, per PROVENANCE.md.
  local pinned_commit
  pinned_commit="$(grep -oE 'commit \`[0-9a-f]{7,40}\`' "${provenance}" \
    | head -n1 | grep -oE '[0-9a-f]{7,40}' || true)"

  local head_commit
  head_commit="$(git -C "${tapes_repo}" rev-parse HEAD 2>/dev/null || echo unknown)"
  case "${head_commit}" in
    "${pinned_commit}"*) ;;
    *)
      echo "notice: ${tapes_repo} is at ${head_commit}, not the pinned ${pinned_commit};" >&2
      echo "        a diff below may be a pending contract bump rather than corruption" >&2
      ;;
  esac

  (
    cd "${tapes_repo}"
    GOEXPERIMENT=jsonv2 go run ./cli/tapes dev openapi api --docs-root . --out "${tmp}/tapes-api.yaml"
    GOEXPERIMENT=jsonv2 go run ./cli/tapes dev openapi ingest --docs-root . --out "${tmp}/tapes-ingest.yaml"
  )

  local name
  for name in tapes-api.yaml tapes-ingest.yaml; do
    if ! diff -u "${vendored}/${name}" "${tmp}/${name}"; then
      echo "FAIL: vendored ${name} differs from the emission at ${head_commit}" >&2
      fail=1
    else
      echo "ok: ${name} matches the re-emission"
    fi
  done
}

if [ -n "${TAPES_REPO:-}" ]; then
  if [ ! -f "${TAPES_REPO}/cli/tapes/main.go" ]; then
    echo "FAIL: TAPES_REPO=${TAPES_REPO} is not a tapes checkout" >&2
    exit 1
  fi
  emit_and_diff "${TAPES_REPO}"
  exit "${fail}"
fi

# --- gate 2 (preferred): fetch the pinned release assets ---------------------
pinned_tag="$(grep -oE 'Release tag: \*\*v[0-9]+\.[0-9]+\.[0-9]+\*\*' "${provenance}" \
  | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+' || true)"
tag="${TAPES_CONTRACT_TAG:-${pinned_tag}}"
if [ -z "${tag}" ]; then
  echo "FAIL: could not determine the pinned release tag from PROVENANCE.md (set TAPES_CONTRACT_TAG)" >&2
  exit 1
fi

base="https://github.com/papercomputeco/tapes/releases/download/${tag}"
fetched=1
for surface in api ingest; do
  if ! curl -fsSL --retry 2 -o "${tmp}/tapes-${surface}.yaml" \
    "${base}/tapes-${surface}-${tag}.yaml"; then
    fetched=0
    break
  fi
done

if [ "${fetched}" = 1 ]; then
  for name in tapes-api.yaml tapes-ingest.yaml; do
    if ! diff -u "${vendored}/${name}" "${tmp}/${name}"; then
      echo "FAIL: vendored ${name} differs from the ${tag} release asset" >&2
      fail=1
    else
      echo "ok: ${name} matches the ${tag} release asset"
    fi
  done
  exit "${fail}"
fi

# Fetch failed (offline, or the tag's assets are missing): fall back to a
# checkout beside this repo when one exists.
fallback_repo="${here}/../tapes"
if [ -f "${fallback_repo}/cli/tapes/main.go" ]; then
  echo "notice: could not fetch the ${tag} release assets; falling back to re-emission from ${fallback_repo}"
  emit_and_diff "${fallback_repo}"
else
  echo "notice: could not fetch the ${tag} release assets and no tapes checkout at ${fallback_repo} (set TAPES_REPO); skipping the byte diff"
fi

exit "${fail}"
