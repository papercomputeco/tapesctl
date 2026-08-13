#!/usr/bin/env bash
# check-tapes-pins.sh — assert this repository's tapes crates pins are sane.
#
# This repository consumes several packages from one upstream repository, each
# pinned by git revision. Three questions are asked about those pins, in order:
#
#   1. Does every pin in Cargo.toml name the SAME revision?
#   2. Does Cargo.lock resolve exactly those packages, each to that revision?
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
#     "Exactly those packages" is the whole of it, in both directions. A loop
#     that walks the lock and checks each entry against the declared revision
#     passes a lock that is simply missing a package: every entry it *does*
#     have agrees, and the one the manifest declares and the lock never
#     mentions is never visited. So the comparison is set equality — every
#     declared package present in the lock, and no package from that repository
#     in the lock that the manifest does not declare — and only then a revision
#     comparison per package.
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
# # How the pins are read
#
# Through Cargo, not through a regex over the manifest text. `cargo metadata
# --no-deps` reports every dependency each workspace member declares, with the
# source Cargo itself resolved the declaration to — so a pin is found wherever
# TOML permits it to be written: an inline table, a `[workspace.dependencies.x]`
# section, a member's own `[dependencies]`, an inline table split across lines.
# A line-oriented parser sees only the spelling it was written for, and the
# failure is the bad kind: a pin it cannot parse is a pin it reports nothing
# about, so reformatting a dependency silently narrows the gate.
#
# Two properties survive the change and are load-bearing. The set of checked
# packages is still discovered by matching the repository URL rather than
# listed here, so a fourth package from that repository is checked the day it
# is added. And both spellings of the URL are still matched, for the reason
# given at UPSTREAM_REPO_RE.
#
# One property is new, and is a consequence of asking Cargo rather than the
# file: what is checked is what this workspace actually builds with. An entry
# catalogued in `[workspace.dependencies]` that no crate references yet is
# invisible to Cargo, so it is invisible here, and it is absent from the
# lockfile too — which is what keeps question (2)'s set comparison from failing
# on it. It comes under the gate the moment a crate references it, which is
# also the moment it can vendor anything.
#
# The lockfile is read with awk, and that is not the same risk: Cargo writes
# it, in one canonical form, one key per line.
#
# # When these become crates.io versions
#
# The extraction is deliberately the only part that knows about revisions.
# `manifest_pins` and `lock_pins` each read a stream and emit `name<TAB>
# revision`, and everything downstream compares opaque strings. Published
# versions replace git revisions by rewriting those two functions to emit
# `name<TAB>version`; questions (1) and (2) then hold unchanged, and question
# (3) — "does this exist where others can get it" — becomes an index lookup
# rather than an ancestry test.
#
# Usage:
#   ./scripts/check-tapes-pins.sh
#   make check-tapes-pins
#
# Requires: cargo, jq, git.
#
# Exit status: 0 when every question is answered yes, 1 when one is answered
# no, 2 when the script could not ask (unreadable manifest, missing tool, no
# network). A question that could not be asked is never reported as a pass: a
# check that passes when it could not look reports a verification it never
# performed.

set -euo pipefail

# The upstream repository whose pins are checked. Both spellings are matched
# because the repository was renamed and its consumers adopted the new name at
# different times; the check must keep working against whichever name this
# repository's manifest currently uses, and must notice a pin under the old
# name rather than walking past it. Anchored to the papercomputeco path so no
# other git dependency is ever caught by it.
#
# The literal dot is bracketed rather than backslash-escaped because this one
# string is handed to two regex engines — awk's via -v, jq's via --arg — and in
# both of them `\.` is a string escape before it is ever a pattern. Bracketing
# is what makes one constant serve both.
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

for tool in cargo jq git; do
    command -v "$tool" >/dev/null 2>&1 ||
        die "$tool is required to read and check the pins — nothing here was verified"
done

# --- extraction ---------------------------------------------------------------
# The seam described under "When these become crates.io versions". Both
# functions read a stream and emit `name<TAB>revision`, one line per pinned
# package.

# Every dependency in `cargo metadata` output on stdin whose resolved source is
# the upstream repository.
#
# A git source reads `git+<url>?rev=<request>`, and the `rev=` is what makes a
# pin a pin: a dependency taken from that repository by branch or by tag
# resolves to a source with no `rev=` at all, and is emitted here with an empty
# revision so the caller can say so rather than skip it. Silently dropping it
# is what a regex that only looks for `rev` does, and a dependency the gate
# cannot see is the one worth naming out loud.
#
# `unique_by` collapses the same package declared by several workspace members,
# which is one pin stated repeatedly — but only when they agree. Two members
# naming two revisions survive as two entries, which is exactly what question
# (1) exists to catch.
manifest_pins() {
    jq -r --arg re "$UPSTREAM_REPO_RE" '
        [ .packages[].dependencies[]
          | select(.source != null and (.source | test($re)))
          | { name: .name,
              rev: ([ .source
                      | match("[?&]rev=([0-9a-fA-F]+)(?:&|$)")
                      | .captures[0].string ] | first // "")
            }
        ]
        | unique_by([.name, .rev])
        | .[]
        | [.name, .rev]
        | @tsv
    '
}

# Every locked package on stdin that resolved from the upstream repository,
# with the revision Cargo actually resolved. A lock source reads
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
    '
}

# Read once, up front, so a Cargo that could not parse the manifest is reported
# as "could not ask" rather than reaching the pin loop as an empty result and
# being reported as "this repository pins nothing".
#
# `--no-deps` keeps this to the workspace's own declarations: no resolution, no
# fetch, no network, and no write to the lockfile this script is about to read.
metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$MANIFEST" 2>/dev/null || true)"
[[ -n "$metadata" ]] ||
    die "\`cargo metadata\` could not read $MANIFEST — the pins are unread, so nothing here was verified"

# Collected with read loops rather than `mapfile`, which is bash 4+ and so
# absent from the bash macOS still ships as /bin/bash.
manifest=()
while IFS= read -r line; do
    [[ -n "$line" ]] && manifest+=("$line")
done < <(printf '%s\n' "$metadata" | manifest_pins)

locked=()
while IFS= read -r line; do
    [[ -n "$line" ]] && locked+=("$line")
done < <(lock_pins <"$LOCKFILE")

if [[ ${#manifest[@]} -eq 0 ]]; then
    die "no git dependency on $UPSTREAM_REPO_RE found in $MANIFEST (have the dependencies moved to crates.io? see the header)"
fi

# A dependency on that repository that names no revision — taken by branch or
# by tag — is asked about before anything else, because the three questions
# below are all about "the pinned revision" and there is not one. It is also
# the failure the old line-oriented reader could not report: no `rev` on the
# line meant no line, which meant no pin, which meant nothing said.
unpinned=()
for pin in "${manifest[@]}"; do
    [[ "${pin##*$'\t'}" == "" ]] && unpinned+=("${pin%%$'\t'*}")
done
if [[ ${#unpinned[@]} -gt 0 ]]; then
    cat >&2 <<EOF
FAIL: $MANIFEST takes ${unpinned[*]} from ${UPSTREAM_URL} without pinning a revision.

A dependency on that repository by branch or by tag is a dependency that moves
under this repository without a commit here to say so — the build is no longer
described by the tree that produced it, and neither the lockfile agreement nor
the ancestry question below has a revision to be asked about.

Pin every one of them to one revision with:

    make bump-harnesses REV=<sha>

EOF
    echo "check-tapes-pins: FAILED" >&2
    exit 1
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

# --- question 2: the lockfile resolves exactly what the manifest declares -----
#
# Set equality first, then revisions. Walking only the lock's entries answers
# "is everything the lock has correct", which a lock that has lost a package
# answers yes — the missing one is never an entry, so it is never wrong.

# bash 3.2 has no associative arrays, so membership is a scan. The sets are
# three or four packages; the loop is the clearest thing that works everywhere
# this script runs.
contains() {
    local needle="$1" item
    shift
    for item in "$@"; do
        [[ "$item" == "$needle" ]] && return 0
    done
    return 1
}

declared_names=()
for pin in "${manifest[@]}"; do
    declared_names+=("${pin%%$'\t'*}")
done
locked_names=()
for pin in "${locked[@]}"; do
    locked_names+=("${pin%%$'\t'*}")
done

if [[ ${#locked[@]} -eq 0 ]]; then
    echo "FAIL: $MANIFEST pins ${UPSTREAM_URL} but $LOCKFILE resolves nothing from it" >&2
    echo "  run \`cargo update\` (or \`make bump-harnesses REV=$declared\`) and commit the lockfile" >&2
    fail=1
else
    lock_ok=1

    missing=()
    for name in "${declared_names[@]}"; do
        contains "$name" "${locked_names[@]}" || missing+=("$name")
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
        cat >&2 <<EOF
FAIL: $LOCKFILE does not resolve ${missing[*]}, which $MANIFEST declares.

  $MANIFEST declares:  ${declared_names[*]}
  $LOCKFILE resolves:  ${locked_names[*]}

A package the lockfile never mentions is not a package whose revision agrees —
it is a package this check has nothing to compare, and a lockfile written from
a different manifest than the one in this tree. Refresh it and commit it:

    make bump-harnesses REV=$declared

EOF
        lock_ok=0
        fail=1
    fi

    undeclared=()
    for name in "${locked_names[@]}"; do
        contains "$name" "${declared_names[@]}" || undeclared+=("$name")
    done
    if [[ ${#undeclared[@]} -gt 0 ]]; then
        cat >&2 <<EOF
FAIL: $LOCKFILE resolves ${undeclared[*]} from ${UPSTREAM_URL}, which $MANIFEST does not declare.

  $MANIFEST declares:  ${declared_names[*]}
  $LOCKFILE resolves:  ${locked_names[*]}

The build vendors a package from that repository which nothing in this tree
asks for. Usually that is a stale lockfile — a dependency dropped from the
manifest without the lockfile being refreshed — in which case refreshing it is
the fix:

    make bump-harnesses REV=$declared

If instead the package arrived transitively, because a crate this repository
does declare grew a dependency on a sibling crate in its own repository, then
declaring it here is the fix: it is being vendored either way, and the manifest
should say so.

EOF
        lock_ok=0
        fail=1
    fi

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
        echo "ok: $LOCKFILE resolves all ${#declared_names[@]} declared package(s), each to $declared"
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

# A probe, and only a probe. It answers "is that branch reachable from here at
# all", which the clone below cannot distinguish from "the host is down" — but
# its answer is deliberately NOT what the ancestry test compares against. The
# upstream branch moves; a revision that landed a moment ago would be tested
# against a tip read before the clone that contains it, and would be reported
# unreachable for having landed too recently. The graph and the tip must come
# from the same snapshot, so the tip is read back out of the clone.
#
# `|| true` so a failed lookup falls through to the die below. Without it the
# assignment's non-zero status trips `set -e` and the script exits 1 — the code
# for "an invariant is broken" — silently, on what is only a machine that could
# not reach the network or has no working git. The distinction is the whole
# point of having two failing exit codes.
probe="$(git ls-remote "$UPSTREAM_URL" "refs/heads/${UPSTREAM_BRANCH}" 2>/dev/null | awk '{print $1}' || true)"
[[ -n "$probe" ]] ||
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

# The tip as this clone has it: one snapshot for both the commit graph and the
# ref the graph is measured against. See the probe above for why it is not read
# over the wire a second time.
tip="$(git -C "$work/upstream.git" rev-parse "refs/heads/${UPSTREAM_BRANCH}" 2>/dev/null || true)"
[[ -n "$tip" ]] ||
    die "cloned ${UPSTREAM_BRANCH} from ${UPSTREAM_URL} but the clone has no ${UPSTREAM_BRANCH} to measure against — the pin's reachability is unverified"

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
