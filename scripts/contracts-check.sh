#!/usr/bin/env bash
# Verify the vendored tapes ingest contract is exactly the published bytes.
#
# Only the ingest surface is checked here. The read contract moved to the
# `tapes-read-contract` crate in the tapes-crates repository (PCC-1146), which
# owns its provenance and runs the same two gates on it in its own CI; this
# repository inherits that check through its pin.
#
# Two gates, in order:
#
#   1. The vendored file still carries the fingerprint recorded in
#      contracts/PROVENANCE.md — a hand-edit of the vendored document fails
#      here even with no network and no tapes checkout on the machine.
#   2. Fetch the contract asset from the pinned tapes release and byte-diff
#      it against the vendored copy — a vendoring that does not match the
#      published release fails here.
#
# Gate 2 prefers the release asset (the published source of truth):
#
#   https://github.com/papercomputeco/tapes/releases/download/<tag>/tapes-ingest-<tag>.yaml
#
# The tag defaults to the pin recorded in PROVENANCE.md; override with
# TAPES_CONTRACT_TAG when checking a bump before the docs are updated. While
# the override names a tag other than the recorded pin — a refresh in
# progress — gate 1's mismatches are reported as expected rather than
# latched as failures, and gate 2 (against the override tag's assets) is
# the authoritative verdict.
#
# Fallback for offline work or development against an unreleased tapes commit:
# set TAPES_REPO=/path/to/tapes to re-emit the contract from that checkout
# (needs its Go toolchain) and diff the emission instead. When the fetch fails
# and no checkout is available, gate 2 is skipped with a notice so gate 1
# still runs everywhere — except mid-refresh, where a skipped gate 2 is a
# failure because nothing authoritative ran.
#
# That skip is right on a laptop on a train and wrong in CI, where "could not
# look" is indistinguishable from "looked and agreed" in the only place anyone
# reads: the job's green tick. Set TAPES_CONTRACTS_STRICT=1 to make a skipped
# gate 2 a failure. CI sets it; leaving it unset keeps the offline path usable
# for humans.

set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
vendored="${here}/crates/tapesctl/contracts"
provenance="${vendored}/PROVENANCE.md"

# The tag the recorded fingerprints belong to, per PROVENANCE.md.
pinned_tag="$(grep -oE 'Release tag: \*\*v[0-9]+\.[0-9]+\.[0-9]+\*\*' "${provenance}" \
  | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+' || true)"

# A TAPES_CONTRACT_TAG naming a different tag than the recorded pin means a
# refresh is in progress: the vendored bytes are (or are about to be) the new
# tag's, while PROVENANCE.md still records the old one.
refresh=0
if [ -n "${TAPES_CONTRACT_TAG:-}" ] && [ "${TAPES_CONTRACT_TAG}" != "${pinned_tag}" ]; then
  refresh=1
  echo "notice: TAPES_CONTRACT_TAG=${TAPES_CONTRACT_TAG} differs from the recorded pin (${pinned_tag:-none});"
  echo "        treating this as a refresh in progress — gate 1 is informational, gate 2 decides"
fi

fail=0

# --- gate 1: recorded fingerprints -------------------------------------------
name=tapes-ingest.yaml
recorded="$(grep -oE "\`${name}\` sha256 \`[0-9a-f]{64}\`" "${provenance}" \
  | grep -oE '[0-9a-f]{64}')"
actual="$(shasum -a 256 "${vendored}/${name}" | awk '{print $1}')"
if [ "${recorded}" != "${actual}" ]; then
  if [ "${refresh}" = 1 ]; then
    echo "notice: ${name} does not match the fingerprint recorded in PROVENANCE.md" >&2
    echo "  recorded: ${recorded}" >&2
    echo "  actual:   ${actual}" >&2
    echo "  expected during a refresh to ${TAPES_CONTRACT_TAG}; update PROVENANCE.md before landing" >&2
  else
    echo "FAIL: ${name} does not match the fingerprint recorded in PROVENANCE.md" >&2
    echo "  recorded: ${recorded}" >&2
    echo "  actual:   ${actual}" >&2
    fail=1
  fi
else
  echo "ok: ${name} matches its recorded fingerprint"
fi

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
    GOEXPERIMENT=jsonv2 go run ./cli/tapes dev openapi ingest --docs-root . --out "${tmp}/${name}"
  )

  if ! diff -u "${vendored}/${name}" "${tmp}/${name}"; then
    echo "FAIL: vendored ${name} differs from the emission at ${head_commit}" >&2
    fail=1
  else
    echo "ok: ${name} matches the re-emission"
  fi
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
tag="${TAPES_CONTRACT_TAG:-${pinned_tag}}"
if [ -z "${tag}" ]; then
  echo "FAIL: could not determine the pinned release tag from PROVENANCE.md (set TAPES_CONTRACT_TAG)" >&2
  exit 1
fi

base="https://github.com/papercomputeco/tapes/releases/download/${tag}"
if curl -fsSL --retry 2 -o "${tmp}/${name}" "${base}/tapes-ingest-${tag}.yaml"; then
  if ! diff -u "${vendored}/${name}" "${tmp}/${name}"; then
    echo "FAIL: vendored ${name} differs from the ${tag} release asset" >&2
    fail=1
  else
    echo "ok: ${name} matches the ${tag} release asset"
  fi
  exit "${fail}"
fi

# Fetch failed (offline, or the tag's asset is missing): fall back to a
# checkout beside this repo when one exists.
fallback_repo="${here}/../tapes"
if [ -f "${fallback_repo}/cli/tapes/main.go" ]; then
  echo "notice: could not fetch the ${tag} release asset; falling back to re-emission from ${fallback_repo}"
  emit_and_diff "${fallback_repo}"
elif [ "${refresh}" = 1 ]; then
  echo "FAIL: mid-refresh, but the ${tag} release asset could not be fetched and no tapes" >&2
  echo "      checkout exists at ${fallback_repo} (set TAPES_REPO) — nothing authoritative ran" >&2
  fail=1
elif [ "${TAPES_CONTRACTS_STRICT:-0}" = 1 ]; then
  echo "FAIL: could not fetch the ${tag} release asset and no tapes checkout at ${fallback_repo};" >&2
  echo "      TAPES_CONTRACTS_STRICT=1, so an unverifiable contract is a failure rather than" >&2
  echo "      a green tick that only proves gate 1 (the vendored file matches a fingerprint" >&2
  echo "      recorded in the same commit that could have changed both)" >&2
  fail=1
else
  echo "notice: could not fetch the ${tag} release asset and no tapes checkout at ${fallback_repo} (set TAPES_REPO); skipping the byte diff"
fi

exit "${fail}"
