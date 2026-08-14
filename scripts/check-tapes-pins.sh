#!/usr/bin/env bash
# check-tapes-pins.sh — assert this repository's tapes crates versions are sane.
#
# This repository consumes several packages published from one upstream
# repository (tapes-crates), each taken from crates.io by version. Three
# questions are asked about those versions, in order:
#
#   1. Does Cargo.lock resolve exactly one version of each tapes crate?
#   2. Does Cargo.lock agree with Cargo.toml?
#   3. Does every resolved version exist, unyanked, on crates.io?
#
# These were git-revision questions once — same rev everywhere, lockfile
# agreement, and "has that rev landed on the upstream default branch". The
# move to published versions dissolved most of what made them hard: the
# registry is content-addressed and public, so there is no unreachable pin, no
# ancestry to test, and no clone to make. What survives is the drift each
# question existed to catch:
#
# (1) One crate at two versions is how `tapes_client::Call` stops being
#     `tapes_client::Call`. Cargo unifies semver-COMPATIBLE requirements, but a
#     semver-incompatible split — this manifest on 0.1 while a transitive
#     dependency asks for 0.2 — vendors one crate twice at two points in its
#     history, and a type from one is not the same type as its twin from the
#     other. Short of a compile error, it is silent: one copy's fix present,
#     the other's absent, every test still green.
#
# (2) A manifest and a lockfile that disagree mean the build resolves
#     something the manifest does not say. Cargo itself is the judge here
#     (`cargo metadata --locked`), so satisfaction of a version requirement is
#     decided by the one implementation whose opinion counts. Resolving under
#     `--locked` may need the network for the registry index and any git
#     dependencies, and — as with question (3) — a network that is away is
#     not evidence: an environment that cannot resolve is reported as a
#     WARNING, and what fails is Cargo resolving and saying no.
#
# (3) A version that resolves locally but is absent or yanked on crates.io is
#     the version-era cousin of a pin at an unreachable revision: it builds
#     here, today, out of a cached registry — and stops building for the next
#     clean clone, with no change to this repository to explain why. This
#     question needs the network, and a network that is away is not evidence
#     about the versions — so an unreachable registry is reported as a
#     WARNING, never a failure. What fails is reaching the registry and
#     finding the version missing or yanked.
#
# # The escape hatch
#
# During a burst of tight co-development an entry in Cargo.toml may
# temporarily become a `git = "...", rev = "..."` pin again, to build against
# crate work that has not been published yet. That is intended, not a
# regression (see .github/dependabot.yml). This script reports it loudly and
# skips the version questions for the duration — but a MIX of git and registry
# sources for the tapes crates is a failure outright, because a git
# tapes-client carries its repository's own siblings with it, and those meet
# their registry twins as duplicate crates: exactly the two-types hazard of
# question (1).
#
# # What this script does NOT check
#
# Agreement with the sibling CLI's versions — the other client built on these
# crates. That comparison is deliberately one-directional, and it is the
# sibling that performs it: this repository is public and must stay runnable
# by anyone who clones it, and questions (1)-(3) are answerable from this
# checkout plus the public registry, which is the whole set this script asks.
#
# # How the versions are read
#
# Through Cargo, not through a regex over the manifest text. `cargo metadata
# --no-deps` reports every dependency each workspace member declares, with the
# source Cargo itself resolved the declaration to — so a dependency is found
# wherever TOML permits it to be written. The lockfile is read with awk, which
# is not the same risk: Cargo writes it, in one canonical form, one key per
# line.
#
# The set of checked packages is discovered by name prefix (`tapes-`) rather
# than listed here, so a fourth crate from the family is checked the day a
# workspace member references it.
#
# Usage:
#   ./scripts/check-tapes-pins.sh
#   make check-tapes-pins
#
# Requires: cargo, jq, curl.
#
# Exit status: 0 when every question is answered yes (or could not be asked
# for a reason that is not evidence — see questions 2 and 3), 1 when one is
# answered no, 2 when the script could not ask at all (unreadable manifest,
# missing tool). A question that could not be asked is never reported as a
# pass.

set -euo pipefail

# The crate-name prefix that marks a package as one of the shared tapes
# crates. Names rather than a repository URL, because a registry source is the
# same string for every crates.io package and identifies nothing.
TAPES_NAME_RE='^tapes-'

# Registry source as Cargo spells it in metadata and lockfiles. The `+` and
# each literal dot are bracketed rather than backslash-escaped because this
# one string is handed to two regex engines — awk's via -v, jq's via --arg —
# and in both of them a backslash is a string escape before it is ever a
# pattern. Bracketing is what makes one constant serve both.
REGISTRY_RE='registry[+]https://github[.]com/rust-lang/crates[.]io-index'

# crates.io asks robots to say who they are; a descriptive User-Agent is the
# polite half of question (3) being cheap.
CRATES_IO_UA='tapesctl-check-tapes-pins (https://github.com/papercomputeco/tapesctl)'

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

for tool in cargo jq curl; do
    command -v "$tool" >/dev/null 2>&1 ||
        die "$tool is required to read and check the versions — nothing here was verified"
done

# --- extraction ---------------------------------------------------------------

# Every tapes-crates dependency each workspace member declares, as
# `name<TAB>kind<TAB>detail`: kind `registry` with the version requirement, or
# kind `git` with the source URL (the escape hatch, or a mistake — the caller
# decides which).
manifest_deps() {
    jq -r --arg name_re "$TAPES_NAME_RE" --arg reg_re "$REGISTRY_RE" '
        [ .packages[].dependencies[]
          | select(.name | test($name_re))
          | if .source == null or (.source | test($reg_re)) then
              { name: .name, kind: "registry", detail: .req }
            else
              { name: .name, kind: "git", detail: .source }
            end
        ]
        | unique_by([.name, .kind, .detail])
        | .[]
        | [.name, .kind, .detail]
        | @tsv
    '
}

# Every locked tapes crate on stdin, as `name<TAB>kind<TAB>version`. Reads a
# stream rather than a path so the same function can read a lockfile that is
# never written to disk.
lock_versions() {
    awk -v name_re="$TAPES_NAME_RE" -v reg_re="$REGISTRY_RE" '
        /^name = / {
            name = $3; gsub(/"/, "", name)
            matched = (name ~ name_re)
        }
        /^version = / && matched { version = $3; gsub(/"/, "", version) }
        /^source = / && matched {
            if ($0 ~ reg_re) { print name "\tregistry\t" version }
            else if ($0 ~ /git[+]/) { print name "\tgit\t" version }
            matched = 0
        }
    '
}

# Read once, up front, so a Cargo that could not parse the manifest is
# reported as "could not ask" rather than as "this repository declares
# nothing". `--no-deps` resolves nothing: no fetch, no network, no write to
# the lockfile this script is about to read.
metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$MANIFEST" 2>/dev/null || true)"
[[ -n "$metadata" ]] ||
    die "\`cargo metadata\` could not read $MANIFEST — the versions are unread, so nothing here was verified"

# Collected with read loops rather than `mapfile`, which is bash 4+ and so
# absent from the bash macOS still ships as /bin/bash.
declared=()
while IFS= read -r line; do
    [[ -n "$line" ]] && declared+=("$line")
done < <(printf '%s\n' "$metadata" | manifest_deps)

[[ ${#declared[@]} -gt 0 ]] ||
    die "no tapes crate dependency found in $MANIFEST (checked names matching ${TAPES_NAME_RE})"

# --- the escape hatch, before any question ------------------------------------

git_deps=()
registry_deps=()
for dep in "${declared[@]}"; do
    IFS=$'\t' read -r name kind detail <<<"$dep"
    case "$kind" in
        git) git_deps+=("$name") ;;
        registry) registry_deps+=("$name"$'\t'"$detail") ;;
    esac
done

if [[ ${#git_deps[@]} -gt 0 && ${#registry_deps[@]} -gt 0 ]]; then
    cat >&2 <<EOF
FAIL: $MANIFEST takes the tapes crates from two kinds of source at once.

  by git revision:  ${git_deps[*]}
  from crates.io:   $(for d in "${registry_deps[@]}"; do printf '%s ' "${d%%$'\t'*}"; done)

A git tapes crate carries its repository's sibling crates with it, and Cargo
treats a git source and a registry source as different packages — so the git
copy's siblings meet their registry twins as duplicate crates, and a type from
one is not the same type as its twin from the other. The escape hatch is all
of them or none: pin every tapes crate to the same git revision for the
duration, or take every one from crates.io.
EOF
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi

if [[ ${#git_deps[@]} -gt 0 ]]; then
    cat <<EOF
ESCAPE HATCH ENGAGED: ${git_deps[*]} taken by git revision, not from crates.io.

This is the documented co-development loan (see .github/dependabot.yml): fine
while unpublished crate work is being built against, and repaid by re-pointing
at the next published version once it lands. The version questions are not
asked while it is engaged.

check-tapes-pins: OK (escape hatch)
EOF
    exit 0
fi

# --- question 1: one version of each crate ------------------------------------

locked=()
while IFS= read -r line; do
    [[ -n "$line" ]] && locked+=("$line")
done < <(lock_versions <"$LOCKFILE")

if [[ ${#locked[@]} -eq 0 ]]; then
    echo "FAIL: $MANIFEST declares tapes crates but $LOCKFILE resolves none of them" >&2
    echo "  refresh the lockfile (cargo update) and commit it" >&2
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi

echo "Tapes crates in $LOCKFILE:"
for entry in "${locked[@]}"; do
    IFS=$'\t' read -r name kind version <<<"$entry"
    printf '  %-28s %s  (%s)\n' "$name" "$version" "$kind"
done
echo

fail=0

# bash 3.2 has no associative arrays, so membership is a scan. The sets are a
# few packages; the loop is the clearest thing that works everywhere this
# script runs.
contains() {
    local needle="$1" item
    shift
    for item in "$@"; do
        [[ "$item" == "$needle" ]] && return 0
    done
    return 1
}

lock_git=()
seen=()
dupes=()
for entry in "${locked[@]}"; do
    IFS=$'\t' read -r name kind version <<<"$entry"
    [[ "$kind" == "git" ]] && lock_git+=("$name")
    if contains "$name" "${seen[@]+"${seen[@]}"}"; then
        contains "$name" "${dupes[@]+"${dupes[@]}"}" || dupes+=("$name")
    fi
    seen+=("$name")
done

if [[ ${#lock_git[@]} -gt 0 ]]; then
    echo "FAIL: $LOCKFILE resolves ${lock_git[*]} from a git source while $MANIFEST names only crates.io versions" >&2
    echo "  the lockfile was written from a different manifest than the one in this tree; refresh it:" >&2
    echo "      cargo update ${lock_git[*]}" >&2
    fail=1
fi

if [[ ${#dupes[@]} -gt 0 ]]; then
    cat >&2 <<EOF
FAIL: $LOCKFILE resolves more than one version of: ${dupes[*]}

One crate at two versions is one crate vendored twice at two points in its
history — a type from one is not the same type as its twin from the other, and
short of a compile error nothing reports it. Some dependency asks for a
semver-incompatible version of a tapes crate; find it with:

    cargo tree --invert --package <name>

and bring this manifest and that dependency onto one version.
EOF
    fail=1
fi

if [[ "$fail" -eq 0 ]]; then
    echo "ok: exactly one version of each of ${#locked[@]} tapes crate(s), all from crates.io"
fi

# A set mismatch between manifest and lock is question 2's to catch; what is
# reported here is only the case question 2 cannot see, because Cargo would
# resolve the missing package rather than report it.
for dep in "${registry_deps[@]}"; do
    name="${dep%%$'\t'*}"
    if ! contains "$name" "${seen[@]+"${seen[@]}"}"; then
        echo "FAIL: $MANIFEST declares $name but $LOCKFILE does not resolve it — refresh the lockfile (cargo update $name) and commit it" >&2
        fail=1
    fi
done

# --- question 2: the lockfile agrees with the manifest ------------------------
#
# Cargo is the judge: `--locked` fails rather than silently re-resolving, so a
# lockfile that no longer satisfies the manifest is caught here and not three
# CI minutes later in a build. The verdict is cargo's exit status, never its
# stderr: on a cold cache a succeeding cargo narrates every index and git
# fetch to stderr ("Updating crates.io index", "Downloading crates ..."), and
# narration is not failure. What stderr is for is telling a non-zero exit's
# two meanings apart — a lockfile that does not satisfy the manifest is
# evidence about this tree, while a resolver that could not reach the
# registry or a git source is a network away and, like question 3's
# unreachable registry, is reported as a WARNING rather than a failure.
if locked_err="$(cargo metadata --format-version 1 --locked --manifest-path "$MANIFEST" 2>&1 >/dev/null)"; then
    echo "ok: $LOCKFILE satisfies $MANIFEST (cargo --locked)"
elif printf '%s' "$locked_err" | grep -qi 'lock file.*needs to be updated\|--locked\|failed to select a version'; then
    echo "FAIL: $LOCKFILE no longer agrees with $MANIFEST:" >&2
    printf '%s\n' "$locked_err" | sed 's/^/  /' >&2
    echo "  refresh the lockfile (cargo update <crate>) and commit it" >&2
    fail=1
else
    cat >&2 <<EOF
WARNING (not a failure): \`cargo metadata --locked\` could not run —
manifest/lockfile agreement is unverified on this run:

$(printf '%s\n' "$locked_err" | sed 's/^/  /')

Resolving under --locked may need the network for the registry index and any
git dependencies, so a resolver that cannot reach them is a network away, not
a drift found. What fails this question is Cargo resolving and saying no.
EOF
fi

# --- question 3: every resolved version exists, unyanked, on crates.io --------
#
# Advisory on an unreachable network, by design: curl's exit status is folded
# into the "000" pseudo-code below so a connection failure lands in a warning
# rather than tripping \`set -e\` and failing the step. What fails is a registry
# that answered and said no.

for entry in "${locked[@]}"; do
    IFS=$'\t' read -r name kind version <<<"$entry"
    [[ "$kind" == "registry" ]] || continue

    body="$(mktemp)"
    code="$(curl -sS -o "$body" -w '%{http_code}' --max-time 15 \
        -A "$CRATES_IO_UA" \
        "https://crates.io/api/v1/crates/${name}/${version}" 2>/dev/null || echo 000)"

    case "$code" in
        200)
            yanked="$(jq -r '.version.yanked // false' <"$body" 2>/dev/null || echo unknown)"
            if [[ "$yanked" == "true" ]]; then
                echo "FAIL: ${name} ${version} is YANKED on crates.io — it builds here out of a cached registry and nowhere clean; move to a live version" >&2
                fail=1
            else
                echo "ok: ${name} ${version} exists on crates.io"
            fi
            ;;
        404)
            echo "FAIL: ${name} ${version} does not exist on crates.io — it resolves here out of a cached or private registry and will not resolve for a clean clone" >&2
            fail=1
            ;;
        *)
            echo "WARNING (not a failure): could not ask crates.io about ${name} ${version} (HTTP ${code}) — existence unverified on this run" >&2
            ;;
    esac
    rm -f "$body"
done

echo
if [[ "$fail" -ne 0 ]]; then
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi
echo "check-tapes-pins: OK"
