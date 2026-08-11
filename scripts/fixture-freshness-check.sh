#!/usr/bin/env bash
# fixture-freshness-check.sh — prove every vendored fixture corpus is still
# byte-identical to the upstream commit it claims to be vendored from.
#
# The sync scripts already know how to detect drift (`--check`), but each one
# needs a tapes checkout to compare against, which is why nothing ran them in
# CI: a runner has no such checkout. This script supplies one. For each corpus
# it reads the snapshot SHA recorded in that corpus's SOURCE.md, materialises
# exactly that commit, and hands the result to the corpus's own `--check`.
#
# # Why the pin, and what this does and does not catch
#
# Two different questions get confused as "is the corpus stale":
#
#   1. Do the vendored bytes match the upstream commit SOURCE.md names?
#   2. Is that commit still upstream's latest word on the corpus?
#
# This script answers (1), deliberately. It is a PR gate, so it has to be
# deterministic: comparing against upstream's moving tip would turn an
# unrelated contributor's pull request red the moment somebody edits fixtures
# in another repository, which trains everyone to ignore it. Answering (1) at
# a pinned commit means the job's verdict depends only on the pull request.
#
# (1) is also the failure that actually bites. A vendored corpus that no
# longer matches its own recorded provenance is lying about what it pins, and
# every consumer test that passes against it is passing against a contract
# nobody upstream still has. The seal inside each corpus (DIGEST, recomputed
# by this repo's suite) catches a hand-edited *case*; this catches a corpus
# that was edited consistently — cases and seal together — which is precisely
# the internally-consistent-but-wrong state a seal alone cannot see.
#
# (2) is answered by refreshing the pin, which is a human decision — a refresh
# lands with whatever consumer change it forces, per each SOURCE.md.
#
# # Fail-closed
#
# Every step that could fail to *look* is a failure, not a skip: no SOURCE.md,
# an unparseable pin, an unreachable source, a commit that does not carry the
# fixture path. A check that passes when it could not read its input is worse
# than no check, because it also reports green.
#
# Usage:
#   ./scripts/fixture-freshness-check.sh [corpus ...]   # default: all
#
# Environment:
#   TAPES_SOURCE  Git URL or local path to fetch the pinned commits from.
#                 Defaults to the public tapes repository. Point it at a local
#                 checkout to run this offline:
#                   TAPES_SOURCE=/path/to/tapes ./scripts/fixture-freshness-check.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TAPES_SOURCE="${TAPES_SOURCE:-https://github.com/papercomputeco/tapes}"

# corpus name -> vendored directory under crates/tapesctl/vendor/.
# The sync script and the upstream fixture path are derived from the name, so
# adding a corpus here is the only edit a new one needs — as long as it keeps
# the established naming, which the derivation below asserts rather than
# assumes.
ALL_CORPORA=(content-encoding drop-reason)

usage() {
    awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

case "${1:-}" in
    -h | --help | help) usage 0 ;;
esac

corpora=("$@")
if [[ ${#corpora[@]} -eq 0 ]]; then
    corpora=("${ALL_CORPORA[@]}")
fi

work_root="$(mktemp -d)"
trap 'rm -rf "$work_root"' EXIT

# Materialise one upstream commit into $2. Kept to the smallest transfer that
# still yields a real repository: the sync scripts run `git log` against the
# checkout, so an unpacked tarball would not do.
#
# A URL and a local path need different mechanics. `git fetch <url> <sha>`
# works against GitHub, which serves reachable commits by id; a local
# repository generally refuses the same request, because upload-pack will not
# serve an unadvertised object unless configured to. So a local source is
# cloned (cheap: same filesystem, hardlinked objects) and then checked out at
# the pin.
materialise() {
    local sha="$1" dest="$2"

    if [[ -d "$TAPES_SOURCE/.git" || -d "$TAPES_SOURCE/objects" ]]; then
        git clone -q --no-checkout --local "$TAPES_SOURCE" "$dest" 2>/dev/null \
            || git clone -q --no-checkout "$TAPES_SOURCE" "$dest"
        if ! git -C "$dest" checkout -q "$sha" 2>/dev/null; then
            echo "error: $TAPES_SOURCE has no commit $sha" >&2
            echo "       (a local source must already contain the pinned commit; fetch it there first)" >&2
            return 1
        fi
        return 0
    fi

    git init -q "$dest"
    if ! git -C "$dest" fetch -q --depth 1 "$TAPES_SOURCE" "$sha"; then
        echo "error: could not fetch $sha from $TAPES_SOURCE" >&2
        echo "       this is a failure, not a skip: the vendored copy went unverified" >&2
        return 1
    fi
    git -C "$dest" checkout -q FETCH_HEAD
}

status=0

for corpus in "${corpora[@]}"; do
    echo "==> ${corpus}"

    vendor_dir="$REPO_ROOT/crates/tapesctl/vendor/tapes-${corpus}-fixtures"
    source_md="$vendor_dir/SOURCE.md"
    sync_script="$SCRIPT_DIR/sync-${corpus}-fixtures.sh"

    if [[ ! -f "$source_md" ]]; then
        echo "error: no SOURCE.md at $source_md — cannot learn what this corpus pins" >&2
        status=1
        continue
    fi
    if [[ ! -x "$sync_script" ]]; then
        echo "error: no executable sync script at $sync_script" >&2
        status=1
        continue
    fi

    # The pin is prose in SOURCE.md rather than a machine-readable field, so
    # the parse is strict: exactly one 40-hex id on the "Current snapshot SHA"
    # line. Anything else — absent, abbreviated, two of them after a sloppy
    # edit — is a failure, because guessing which one was meant is how a check
    # ends up verifying the wrong commit and reporting green.
    pin="$(grep -oE '\*\*Current snapshot SHA:\*\* `[0-9a-f]{40}`' "$source_md" \
        | grep -oE '[0-9a-f]{40}' || true)"
    if [[ -z "$pin" ]]; then
        echo "error: no 'Current snapshot SHA' with a full 40-character commit id in $source_md" >&2
        status=1
        continue
    fi
    if [[ "$(printf '%s\n' "$pin" | wc -l | tr -d ' ')" != "1" ]]; then
        echo "error: $source_md records more than one snapshot SHA; cannot tell which is current" >&2
        status=1
        continue
    fi

    echo "    pinned at ${pin} (per $(basename "$vendor_dir")/SOURCE.md)"

    dest="$work_root/$corpus"
    if ! materialise "$pin" "$dest"; then
        status=1
        continue
    fi

    # The pinned commit must actually carry the corpus. If it does not, the
    # pin names a commit from before the fixtures existed (or the path moved
    # upstream), and the sync script's own "is the path right?" error would
    # read as a local mistake rather than as the provenance problem it is.
    if [[ ! -d "$dest/fixtures/${corpus}/cases" ]]; then
        echo "error: commit ${pin} has no fixtures/${corpus}/cases" >&2
        echo "       the recorded pin does not describe this corpus" >&2
        status=1
        continue
    fi

    if "$sync_script" --check "$dest"; then
        echo "ok: vendored ${corpus} fixtures are byte-identical to tapes@${pin}"
    else
        echo "FAIL: vendored ${corpus} fixtures differ from tapes@${pin}" >&2
        echo "      either refresh them (./scripts/sync-${corpus}-fixtures.sh <tapes>)" >&2
        echo "      or, if the pin moved, update SOURCE.md in the same change" >&2
        status=1
    fi
    echo
done

if [[ $status -eq 0 ]]; then
    echo "all vendored fixture corpora match their recorded pins"
fi
exit $status
