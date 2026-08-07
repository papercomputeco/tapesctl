#!/usr/bin/env bash
# sync-drop-reason-fixtures.sh — refresh the vendored copy of the shared
# drop-reason fixture corpus under
# crates/tapesctl/vendor/tapes-drop-reason-fixtures/.
#
# The corpus is authored in the `tapes` repository at fixtures/drop-reason/
# and vendored here so this repo's tests need no cross-repo checkout.
# Vendoring means it can drift, so this script is both the refresher and
# the drift detector.
#
# This is a manual procedure, not automation — same shape as (and kept in
# step with) scripts/sync-content-encoding-fixtures.sh. It takes a local
# checkout path; no network is involved.
#
# Unlike the envelope corpus this one is sealed by a DIGEST that travels with
# the cases, so a hand-edit here is caught by this repo's own test suite even
# when nobody runs --check. This script is the way to make an intentional
# refresh; the seal is the way a stale one is found.
#
# Usage:
#   ./scripts/sync-drop-reason-fixtures.sh <path-to-tapes-checkout>
#   ./scripts/sync-drop-reason-fixtures.sh --check <path-to-tapes-checkout>
#
# --check writes nothing and exits non-zero if the vendored copy differs
# from upstream, printing the diff. Use it to decide whether a refresh is
# needed; the plain form performs it.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
VENDOR_DIR="$REPO_ROOT/crates/tapesctl/vendor/tapes-drop-reason-fixtures"

usage() {
    # Reprint this file's leading comment block as the help text: everything
    # from line 2 until the first non-comment line. Derived rather than
    # duplicated so help can't drift from the docs, and boundary-free so
    # editing the comment doesn't silently spill code into --help.
    awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

check_only=false
case "${1:-}" in
    -h | --help | help | "")
        usage 0
        ;;
    --check)
        check_only=true
        shift
        ;;
esac

checkout_path="${1:-}"
if [[ -z "$checkout_path" ]]; then
    echo "error: missing path to a tapes checkout" >&2
    usage 2
fi

src_dir="$checkout_path/fixtures/drop-reason"
if [[ ! -d "$src_dir/cases" ]]; then
    echo "error: no drop-reason fixtures at $src_dir/cases" >&2
    echo "       (expected a tapes checkout; is the path right?)" >&2
    exit 2
fi

# Pin the commit that last TOUCHED the fixtures, not the checkout's HEAD.
# HEAD moves for unrelated upstream work, which would churn the recorded SHA
# in SOURCE.md on every refresh even when not a byte of the corpus changed.
upstream_sha="$(git -C "$checkout_path" log -1 --format=%H -- fixtures/drop-reason 2>/dev/null || true)"
if [[ -z "$upstream_sha" ]]; then
    upstream_sha="$(git -C "$checkout_path" rev-parse HEAD 2>/dev/null || echo "unknown")"
fi

if $check_only; then
    status=0
    # Compare the case corpus as a directory so an upstream ADDITION or
    # DELETION is caught, not just an edit to a file we already have.
    if ! diff -ru "$VENDOR_DIR/cases" "$src_dir/cases"; then
        status=1
    fi
    # The DIGEST is compared as a file of its own rather than recomputed here:
    # this script's job is "does the vendored copy match upstream", and the
    # seal's job is "does the copy match its own cases". Recomputing here would
    # make a corrupted upstream DIGEST look like agreement.
    if ! diff -u "$VENDOR_DIR/DIGEST" "$src_dir/DIGEST"; then
        status=1
    fi
    if ! diff -u "$VENDOR_DIR/README.upstream.md" "$src_dir/README.md"; then
        status=1
    fi
    if [[ $status -eq 0 ]]; then
        echo "vendored drop-reason fixtures match $checkout_path @ $upstream_sha"
    else
        echo >&2
        echo "error: vendored drop-reason fixtures differ from $checkout_path @ $upstream_sha" >&2
        echo "       run: $0 $checkout_path" >&2
    fi
    exit $status
fi

# Refresh via stage-then-swap. The vendored copy has to be *replaced* rather
# than merged — an upstream deletion must propagate instead of leaving a stale
# case behind that nothing upstream describes any more, and a stale case would
# additionally break the DIGEST seal in a way that reads as corruption rather
# than as a missed sync — but deleting first means any failure after that point
# leaves no corpus at all, and the fixture tests red, until someone restores it
# by hand.
#
# So every fallible step happens in a staging directory, and the live one is
# only touched once the replacement is known-good. On failure the old copy is
# put back.
src_cases=()
for f in "$src_dir"/cases/*.json; do
    [[ -f "$f" ]] || continue
    src_cases+=("$f")
done
if [[ ${#src_cases[@]} -eq 0 ]]; then
    echo "error: no case files at $src_dir/cases" >&2
    echo "       refusing to refresh; the vendored copy is left untouched" >&2
    exit 2
fi
if [[ ! -f "$src_dir/DIGEST" ]]; then
    echo "error: no DIGEST at $src_dir/DIGEST" >&2
    echo "       refusing to refresh; an unsealed copy cannot detect staleness" >&2
    exit 2
fi

# Stage beside the target, NOT in TMPDIR. mv is only atomic within a
# filesystem; across one it degrades to copy-then-delete, which can fail
# partway and leave a half-built directory where the vendored corpus should
# be. The rollback below would then move the backup *into* that directory
# rather than restoring it, turning a failed refresh into a corrupted tree.
# Same parent, same filesystem, real rename.
#
# A SIGKILL can leave one of these behind; anything less is covered by the
# trap.
mkdir -p "$(dirname "$VENDOR_DIR")"
staging="$(mktemp -d "$(dirname "$VENDOR_DIR")/.sync-drop-reason-fixtures.XXXXXX")"
previous=""

# The swap below is two renames, and an interrupt landing between them would
# otherwise leave no vendored corpus at all: the live directory moved aside,
# the replacement not yet installed. So cleanup restores rather than only
# tidying, and runs on signals as well as on exit.
#
# Idempotent by construction — it only acts when the live directory is absent
# and a backup exists — so running it from both an INT handler and the EXIT
# trap is harmless. SIGKILL is still unrecoverable; nothing in shell can help
# there, and the backup is left in place for a human.
cleanup() {
    if [[ -n "$previous" && -d "$previous" && ! -e "$VENDOR_DIR" ]]; then
        mv "$previous" "$VENDOR_DIR" || true
    fi
    [[ -n "$staging" && -d "$staging" ]] && rm -rf "$staging"
    return 0
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

mkdir -p "$staging/cases"
cp "${src_cases[@]}" "$staging/cases/"
cp "$src_dir/DIGEST" "$staging/DIGEST"
cp "$src_dir/README.md" "$staging/README.upstream.md"
# SOURCE.md is authored here, not upstream: carry it across so the swap does
# not drop it.
if [[ -f "$VENDOR_DIR/SOURCE.md" ]]; then
    cp "$VENDOR_DIR/SOURCE.md" "$staging/SOURCE.md"
fi

# Swap. Renames only from here on, so the window where neither copy is in
# place is as small as it can be made in shell, and it is recoverable.
if [[ -d "$VENDOR_DIR" ]]; then
    previous="${VENDOR_DIR}.previous.$$"
    mv "$VENDOR_DIR" "$previous"
fi
if ! mv "$staging" "$VENDOR_DIR"; then
    echo "error: could not install the refreshed corpus" >&2
    # Only restore onto empty ground. If the failed rename somehow left
    # something behind, moving the backup would nest it inside rather than
    # replace it — say so and leave both in place for a human.
    if [[ -e "$VENDOR_DIR" ]]; then
        echo "       $VENDOR_DIR still exists; previous copy preserved at $previous" >&2
    elif [[ -d "$previous" ]]; then
        mv "$previous" "$VENDOR_DIR"
        echo "       previous copy restored" >&2
    fi
    exit 1
fi
rm -rf "$previous"

echo "Synced $(find "$VENDOR_DIR/cases" -name '*.json' | wc -l | tr -d ' ') cases from $src_dir"
echo "Upstream SHA: $upstream_sha"
echo
echo "Now:"
echo "  1. Record that SHA in $VENDOR_DIR/SOURCE.md."
echo "  2. Run: cargo test -p tapesctl --test drop_reason_corpus"
echo "     (the DIGEST seal fails first if the copy is inconsistent with itself,"
echo "      the oracle after it if the policy actually changed)"
echo "  3. If an eligibility rule changed, land the fixture bump and the"
echo "     gate change in the same PR — the corpus is the contract."
echo "  4. The authored home (tapes extproc/dropreason_corpus_test.go) must be"
echo "     green at the SAME SHA."
