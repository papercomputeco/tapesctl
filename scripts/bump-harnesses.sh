#!/usr/bin/env bash
# bump-harnesses.sh — re-point every tapes crates git dependency at one revision.
#
# The tapes crates live in a single upstream repository and this repo consumes
# several packages from it. They are one unit: a bump moves ALL of them to the
# same revision, because two packages from the same repo at different revisions
# is the drift that `check-pin-parity.sh` upstream exists to catch. Passing one
# REV and rewriting every pin from it is what makes that structural here rather
# than remembered.
#
# This is the whole repin. It was previously typed by hand — edit each `rev =`
# line, refresh the lockfile, recompute a nix hash — about ten times in nine
# days, and the hand version went wrong in both of the ways hand versions do:
# one pin left behind, or the nix hash not recomputed. There is no nix step
# below anymore; see "Why no nix step" at the bottom.
#
# Usage:
#   ./scripts/bump-harnesses.sh <REV>
#   make bump-harnesses REV=<REV>
#
# REV is a git revision of the upstream crates repo — normally a full 40-char
# commit SHA that has landed on its main branch.
#
# Idempotent: bumping to the revision already pinned rewrites nothing and
# leaves the tree clean, which is what lets CI run it unconditionally and open
# a PR only when something actually moved.

set -euo pipefail

# The upstream repository whose pins this script rewrites. Both spellings are
# matched because the repository was renamed and the two consumers adopted the
# new name at different times; a bump must keep working against whichever name
# this repo's manifest currently uses. Anchored to the papercomputeco path so
# no other git dependency — libproc, notably — is ever touched.
# The literal dot is bracketed rather than backslash-escaped because this
# string is handed to awk via -v, where `\.` is a string escape first and draws
# a warning before it ever reaches the regex engine.
UPSTREAM_REPO_RE='https://github[.]com/papercomputeco/(tapes-crates|tapes-harnesses)'

MANIFEST="Cargo.toml"

die() {
    echo "bump-harnesses: $*" >&2
    exit 1
}

case "${1:-}" in
    -h | --help | help)
        awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
        exit 0
        ;;
esac

REV="${1:-}"
[[ -n "$REV" ]] || die "usage: $0 <REV>   (or: make bump-harnesses REV=<REV>)"

# Hex-only, and long enough to be a real revision rather than a branch name
# that would silently pin something that moves. Full 40-char SHAs are what CI
# passes and what belongs in a committed manifest; shorter prefixes are allowed
# for hand use but the manifest then records exactly what was given.
[[ "$REV" =~ ^[0-9a-fA-F]{7,40}$ ]] ||
    die "'$REV' is not a git SHA (expected 7-40 hex characters)"

cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ -f "$MANIFEST" ]] || die "no $MANIFEST at repo root"

# Every dependency line pinning the upstream repo, with its current rev.
# Anchored at the start of a line so the several comments in this manifest that
# mention these crates by name cannot be mistaken for a pin.
#
# Collected with a read loop rather than `mapfile`, which is bash 4+ and so
# absent from the bash macOS still ships as /bin/bash.
pins=()
while IFS= read -r line; do
    [[ -n "$line" ]] && pins+=("$line")
done < <(
    awk -v re="$UPSTREAM_REPO_RE" '
        /^[a-zA-Z0-9_-]+[[:space:]]*=/ && $0 ~ re {
            name = $1
            if (match($0, /rev[[:space:]]*=[[:space:]]*"[0-9a-fA-F]+"/)) {
                line = substr($0, RSTART, RLENGTH)
                match(line, /[0-9a-fA-F]+"$/)
                print name, substr(line, RSTART, RLENGTH - 1)
            }
        }
    ' "$MANIFEST"
)

[[ ${#pins[@]} -gt 0 ]] ||
    die "no git pins matching $UPSTREAM_REPO_RE found in $MANIFEST (has the dependency moved to crates.io?)"

echo "Pinned packages in $MANIFEST:"
changed=0
for pin in "${pins[@]}"; do
    name="${pin%% *}"
    old="${pin##* }"
    if [[ "$old" == "$REV" ]]; then
        printf '  %-28s %s (unchanged)\n' "$name" "$old"
    else
        printf '  %-28s %s -> %s\n' "$name" "$old" "$REV"
        changed=1
    fi
done

if [[ "$changed" -eq 1 ]]; then
    # Rewrite in place. Only the `rev = "..."` of a matched line changes, so
    # comments, formatting, and every other dependency survive untouched.
    tmp="$(mktemp)"
    trap 'rm -f "$tmp"' EXIT
    awk -v re="$UPSTREAM_REPO_RE" -v rev="$REV" '
        /^[a-zA-Z0-9_-]+[[:space:]]*=/ && $0 ~ re {
            sub(/rev[[:space:]]*=[[:space:]]*"[0-9a-fA-F]+"/, "rev = \"" rev "\"")
        }
        { print }
    ' "$MANIFEST" >"$tmp"
    mv "$tmp" "$MANIFEST"
fi

# Whether or not the manifest moved just now, the lockfile has to agree with it
# before this script claims success. Checking even on the unchanged path is what
# makes a re-run repair a previous run that died between the rewrite and the
# lockfile refresh — the state where the manifest says one revision, the lock
# says another, and a naive "already at REV, nothing to do" would walk past it.
lock_in_sync() {
    cargo metadata --format-version 1 --locked >/dev/null 2>&1
}

if [[ "$changed" -eq 0 ]] && lock_in_sync; then
    echo
    echo "Already at $REV and Cargo.lock agrees; nothing to do."
    exit 0
fi

if [[ "$changed" -eq 0 ]]; then
    echo
    echo "Manifest already at $REV but Cargo.lock disagrees; refreshing it."
fi

# Refresh the lockfile for exactly the packages whose source moved. Named
# packages rather than a bare `cargo update`, which would also drag in
# unrelated registry upgrades and turn a one-line repin into an unreviewable
# diff.
update_args=()
for pin in "${pins[@]}"; do
    update_args+=(--package "${pin%% *}")
done

echo
echo "Refreshing Cargo.lock..."
cargo update "${update_args[@]}"

# Prove the manifest and lockfile agree before handing back. `--locked` fails
# rather than silently re-resolving, so a lockfile the update left stale is
# caught here and not three CI minutes later in a build.
echo
echo "Verifying Cargo.lock is in sync..."
cargo metadata --format-version 1 --locked >/dev/null

echo
echo "OK: ${#pins[@]} package(s) now pinned at $REV"
echo "Next: cargo build, and commit Cargo.toml + Cargo.lock."

# --- Why no nix step ----------------------------------------------------------
#
# flake.nix used to carry a `cargoLock.outputHashes` entry per git dependency,
# and a bump was only half done until each was recomputed with
# `nix-prefetch-git`. Cargo never reads those hashes, so a bump that skipped the
# recompute passed `cargo build` and `cargo test` and failed only under
# `nix build` — which is how stale hashes reached main more than once.
#
# The flake now sets `cargoLock.allowBuiltinFetchGit = true`, so nix fetches git
# dependencies by the revision already written in Cargo.lock. Rewriting the revs
# above is therefore the entire bump: there is no second source of truth left to
# keep in step, and nothing for this script to recompute.
