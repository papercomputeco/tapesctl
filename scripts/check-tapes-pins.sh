#!/usr/bin/env bash
# check-tapes-pins.sh — assert this repository's tapes crates pins are sane.
#
# This repository consumes several packages from one upstream repository, each
# pinned by git revision. Three questions are asked about those pins, in order:
#
#   1. Does every pin in Cargo.toml name the SAME revision?
#   2. Does Cargo.lock resolve every one of them to that same revision?
#   3. Has that revision landed on the upstream default branch?
#
# Why each one is worth a job:
#
# (1) The packages are one repository and are pinned as one. Two revisions of
#     one repository is how `tapes_client::Call` stops being
#     `tapes_client::Call`: Cargo keys a git source on the URL *and* the rev,
#     so the same type vendored twice is two types, and the compiler reports it
#     by naming two identical-looking types. Short of that, it is silent — one
#     package's fix present, another's absent, every test still green.
#
# (2) A manifest and a lockfile that disagree mean the build resolves something
#     the manifest does not say. `bump-harnesses.sh` refreshes both and proves
#     they agree; this catches the tree where that refresh was interrupted or a
#     `rev =` was hand-edited without one.
#
# (3) A pin that is not on the upstream default branch is a pin at a revision
#     nobody else can reach: a pull-request head, an amended commit, a branch
#     that was force-pushed away. It builds here, today, from a cached checkout
#     — and stops building for everyone else the moment that object is
#     collected, with no change to this repository to explain why. This has
#     happened, which is why the gate is here rather than described in a
#     comment.
#
# # Where this check belongs
#
# Upstream used to run a version of (1) that compared its consumers' manifests
# against each other. The placement could not work: reading a private
# consumer's manifest needs a token scoped beyond the crate repository, and the
# job never had one, so every run failed on an unreadable source rather than on
# a comparison — a permanently red check, which teaches everyone to stop
# reading that repository's status.
#
# Consumer-side, the token direction is the easy one. The crates repository is
# public, so this script needs no credential at all to answer (3): a checkout's
# own default token can already read it. That dissolves the blocker rather than
# working around it.
#
# # What this script does NOT check
#
# Agreement with the sibling CLI's pin — the other client built on these
# crates. That comparison is deliberately one-directional, and it is the
# sibling that performs it, because it can: it is the one that can read both
# manifests. This repository is public and must stay runnable by anyone who
# clones it, so a gate here that depended on reading a private repository would
# be a gate most contributors could never pass. Questions (1)-(3) are
# answerable from this checkout plus a public URL, and that is the whole set
# this script asks.
#
# The gateway asset is not checked here either, and needs no check. It is not
# rendered per consumer any more: `tapes-gateway.ts` is one file owned by the
# crate, compiled into it with `include_str!` (see the crate's `plugin` module,
# `PI_GATEWAY_EXTENSION` / `OPENCODE_GATEWAY_EXTENSION`), and every client
# installs those bytes to that one path — `crates/tapesctl/src/plugin.rs`
# asserts byte-for-byte identity with the crate's asset on the install path.
# What a product says for itself it says through the environment of its own
# launch, not through a rendering. So once the pins agree, the installed asset
# bytes agree by construction, and a digest comparison between clients would be
# a second statement of what `include_str!` already guarantees. The pins are
# the thing that can drift; they are what is gated.
#
# # When these become crates.io versions
#
# The extraction is deliberately the only part that knows about revisions.
# `manifest_pins` and `lock_pins` each emit `name<TAB>revision`, and everything
# downstream compares opaque strings. Published versions replace git revisions
# by rewriting those two functions to emit `name<TAB>version`; questions (1)
# and (2) then hold unchanged, and question (3) — "does this exist where others
# can get it" — becomes an index lookup rather than an ancestry test.
#
# Usage:
#   ./scripts/check-tapes-pins.sh
#   make check-tapes-pins
#
# Exit status: 0 when every question is answered yes, 1 when one is answered
# no, 2 when the script could not ask (unreadable manifest, no network).
# A question that could not be asked is never reported as a pass: a check that
# passes when it could not look reports a verification it never performed.

set -euo pipefail

# The upstream repository whose pins are checked. Both spellings are matched
# because the repository was renamed and its consumers adopted the new name at
# different times; the check must keep working against whichever name this
# repository's manifest currently uses, and must notice a pin under the old
# name rather than walking past it. Anchored to the papercomputeco path so no
# other git dependency is ever caught by it.
#
# The literal dot is bracketed rather than backslash-escaped because this
# string is handed to awk via -v, where `\.` is a string escape first and draws
# a warning before it ever reaches the regex engine.
UPSTREAM_REPO_RE='https://github[.]com/papercomputeco/(tapes-crates|tapes-harnesses)'

# The canonical URL and branch the ancestry question is asked against.
UPSTREAM_URL="https://github.com/papercomputeco/tapes-crates"
UPSTREAM_BRANCH="main"

MANIFEST="Cargo.toml"
LOCKFILE="Cargo.lock"

die() {
    echo "check-tapes-pins: $*" >&2
    exit 2
}

case "${1:-}" in
    -h | --help | help)
        # Reprint this file's leading comment block as the help text, so help
        # cannot drift from the docs.
        awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    "") ;;
    *) die "unexpected argument '$1' (this script takes none; try --help)" ;;
esac

cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ -f "$MANIFEST" ]] || die "no $MANIFEST at repo root"
[[ -f "$LOCKFILE" ]] || die "no $LOCKFILE at repo root"

# --- extraction ---------------------------------------------------------------
# The seam described under "When these become crates.io versions". Both
# functions emit `name<TAB>revision`, one line per pinned package.

# Every dependency line in the manifest that pins the upstream repository.
# Anchored at the start of a line so the several comments in this manifest that
# mention these crates by name cannot be mistaken for a pin.
manifest_pins() {
    awk -v re="$UPSTREAM_REPO_RE" '
        /^[a-zA-Z0-9_-]+[[:space:]]*=/ && $0 ~ re {
            name = $1
            if (match($0, /rev[[:space:]]*=[[:space:]]*"[0-9a-fA-F]+"/)) {
                line = substr($0, RSTART, RLENGTH)
                match(line, /[0-9a-fA-F]+"$/)
                print name "\t" substr(line, RSTART, RLENGTH - 1)
            }
        }
    ' "$MANIFEST"
}

# Every locked package resolved from the upstream repository, with the
# revision Cargo actually resolved. A lock source reads
# `git+<url>?rev=<request>#<resolved>`; the fragment is the answer and is
# always a full 40-character SHA, which is why it — not the request — is what
# gets compared and what the ancestry question is asked about.
lock_pins() {
    awk -v re="$UPSTREAM_REPO_RE" '
        /^name = / { name = $3; gsub(/"/, "", name) }
        /^source = / && $0 ~ re {
            if (match($0, /#[0-9a-fA-F]+"/)) {
                print name "\t" substr($0, RSTART + 1, RLENGTH - 2)
            }
        }
    ' "$LOCKFILE"
}

# Collected with read loops rather than `mapfile`, which is bash 4+ and so
# absent from the bash macOS still ships as /bin/bash.
manifest=()
while IFS= read -r line; do
    [[ -n "$line" ]] && manifest+=("$line")
done < <(manifest_pins)

locked=()
while IFS= read -r line; do
    [[ -n "$line" ]] && locked+=("$line")
done < <(lock_pins)

if [[ ${#manifest[@]} -eq 0 ]]; then
    die "no git pins matching $UPSTREAM_REPO_RE found in $MANIFEST (have the dependencies moved to crates.io? see the header)"
fi

echo "Pinned packages from ${UPSTREAM_URL}:"
for pin in "${manifest[@]}"; do
    printf '  %-28s %s  (%s)\n' "${pin%%$'\t'*}" "${pin##*$'\t'}" "$MANIFEST"
done
for pin in "${locked[@]}"; do
    printf '  %-28s %s  (%s)\n' "${pin%%$'\t'*}" "${pin##*$'\t'}" "$LOCKFILE"
done
echo

fail=0

# --- question 1: one repository, one revision ---------------------------------

declared="${manifest[0]##*$'\t'}"
declared_ok=1
for pin in "${manifest[@]}"; do
    if [[ "${pin##*$'\t'}" != "$declared" ]]; then
        cat >&2 <<EOF
FAIL: $MANIFEST pins one repository at more than one revision.

Packages from ${UPSTREAM_URL} are a single source and are pinned as one. Two
revisions of one repository means one repository vendored twice at two points
in its history: a type from one is not the same type as its twin from the
other, and short of a compile error nothing reports it — one package carries a
fix the other does not while every test stays green.

Re-point every pin at one revision with:

    make bump-harnesses REV=<sha>

EOF
        fail=1
        declared_ok=0
        break
    fi
done
if [[ "$declared_ok" -eq 1 ]]; then
    echo "ok: all ${#manifest[@]} pin(s) in $MANIFEST name $declared"
fi

# Questions 2 and 3 both ask about "the pinned revision", which the manifest
# has just failed to name. Asking them anyway against whichever pin happened to
# be read first would answer a question nobody posed, and would print an "ok"
# next to a tree that is not ok. They are declared unasked instead, on the same
# principle as every other could-not-look path here.
if [[ "$declared_ok" -eq 0 ]]; then
    echo
    echo "not asked: lockfile agreement and ${UPSTREAM_BRANCH} ancestry, because $MANIFEST does not name a single revision to ask about" >&2
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi

# --- question 2: the lockfile resolves what the manifest declares -------------

if [[ ${#locked[@]} -eq 0 ]]; then
    echo "FAIL: $MANIFEST pins ${UPSTREAM_URL} but $LOCKFILE resolves nothing from it" >&2
    echo "  run \`cargo update\` (or \`make bump-harnesses REV=$declared\`) and commit the lockfile" >&2
    fail=1
else
    lock_ok=1
    for pin in "${locked[@]}"; do
        name="${pin%%$'\t'*}"
        resolved="${pin##*$'\t'}"
        # The declared pin may be an abbreviated SHA; the resolved one never
        # is. Prefix rather than equality is what makes a short pin legal
        # without making a *different* revision legal.
        if [[ "$resolved" != "$declared"* ]]; then
            cat >&2 <<EOF
FAIL: $LOCKFILE resolves $name to a revision $MANIFEST does not declare.

  $MANIFEST declares:  $declared
  $LOCKFILE resolved:  $resolved  ($name)

The build uses the lockfile, so this tree compiles something the manifest does
not say. Refresh the lockfile and commit it:

    make bump-harnesses REV=$declared

EOF
            lock_ok=0
            fail=1
            break
        fi
    done
    if [[ "$lock_ok" -eq 1 ]]; then
        echo "ok: all ${#locked[@]} locked package(s) resolve to $declared"
    fi
fi

# --- question 3: the revision is on the upstream default branch ---------------
#
# Answered against the public repository over the network, with no credential.
# The resolved (full) revision is the subject where one is available, because
# ancestry needs a complete object name.

subject="$declared"
if [[ ${#locked[@]} -gt 0 ]]; then
    subject="${locked[0]##*$'\t'}"
fi

command -v git >/dev/null 2>&1 || die "git is required to check that a pin is on ${UPSTREAM_BRANCH}"

# `|| true` so a failed lookup falls through to the die below. Without it the
# assignment's non-zero status trips `set -e` and the script exits 1 — the code
# for "an invariant is broken" — silently, on what is only a machine that could
# not reach the network or has no working git. The distinction is the whole
# point of having two failing exit codes.
tip="$(git ls-remote "$UPSTREAM_URL" "refs/heads/${UPSTREAM_BRANCH}" 2>/dev/null | awk '{print $1}' || true)"
[[ -n "$tip" ]] ||
    die "could not read ${UPSTREAM_BRANCH} from ${UPSTREAM_URL} (no network, or the branch is gone) — the pin's reachability is unverified"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Blob-filtered rather than shallow. `--depth` would fetch a truncated history
# in which almost every real pin looks unreachable — the check would fail on
# its own fetch options. The filter drops file contents, which this question
# never reads, and keeps the complete commit graph, which is the whole subject.
# The clone is a few hundred kilobytes.
git clone --quiet --bare --filter=blob:none \
    --single-branch --branch "$UPSTREAM_BRANCH" \
    "$UPSTREAM_URL" "$work/upstream.git" 2>/dev/null ||
    die "could not clone ${UPSTREAM_BRANCH} from ${UPSTREAM_URL} — the pin's reachability is unverified"

# Only ${UPSTREAM_BRANCH} was fetched, so a commit that never landed on it is
# simply not here. Absence is checked first: `merge-base` errors on an unknown
# object, and that error says "Not a valid object name" where what a reader
# needs to be told is which revision, on which branch, is missing.
if ! git -C "$work/upstream.git" cat-file -e "${subject}^{commit}" 2>/dev/null ||
    ! git -C "$work/upstream.git" merge-base --is-ancestor "$subject" "$tip" 2>/dev/null; then
    cat >&2 <<EOF
FAIL: the pinned revision is not on ${UPSTREAM_BRANCH}.

  pinned revision:  $subject
  upstream branch:  ${UPSTREAM_BRANCH} (tip $tip)
  upstream repo:    ${UPSTREAM_URL}

Nothing that is not on ${UPSTREAM_BRANCH} is reachable by anyone else. A pull-request
head, an amended commit, or a branch that has since been force-pushed builds
here today out of a cached checkout and stops building for every other clone —
and for this one, once that object is collected — with no change in this
repository to explain it.

Land the revision on ${UPSTREAM_BRANCH} upstream, then re-point this repository at the
landed commit:

    make bump-harnesses REV=<sha landed on ${UPSTREAM_BRANCH}>

EOF
    fail=1
else
    echo "ok: $subject is on ${UPSTREAM_BRANCH} (tip $tip)"
fi

echo
if [[ "$fail" -ne 0 ]]; then
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi
echo "check-tapes-pins: OK"
