#!/usr/bin/env bash
# check-tapes-pins.sh — assert this repository's tapes crates versions are sane.
#
# This repository consumes several packages published from one upstream
# repository (tapes-crates), each taken from crates.io by version. One
# question is asked about those versions:
#
#   1. Does Cargo.lock resolve exactly one version of each tapes crate?
#
# These were git-revision questions once — same rev everywhere, lockfile
# agreement, and "has that rev landed on the upstream default branch". The
# move to published versions dissolved most of what made them hard; what
# survives is the drift the question existed to catch:
#
# (1) One crate at two versions is how `tapes_client::Call` stops being
#     `tapes_client::Call`. Cargo unifies semver-COMPATIBLE requirements, but a
#     semver-incompatible split vendors one crate twice at two points in its
#     history, and a type from one is not the same type as its twin from the
#     other. Short of a compile error, it is silent: one copy's fix present,
#     the other's absent, every test still green.
#
# Questions this script once asked and no longer does:
#
#   * Does Cargo.lock agree with Cargo.toml? Cargo itself is the judge of
#     that, and the build jobs already ask it: every CI cargo invocation
#     that resolves runs with `--locked`, which refuses a stale lockfile
#     rather than silently re-resolving it. Asking again here duplicated
#     that verdict without strengthening it.
#
#   * Does every resolved version exist, unyanked, on crates.io? Missing
#     and yanked versions surface through Dependabot alerts and through
#     cargo's own yanked handling at resolution time; a CI probe of a
#     remote registry bought little beyond those and cost real network
#     edge-case handling on every run.
#
# # The escape hatches
#
# During a burst of tight co-development an entry in Cargo.toml may
# temporarily become a git or path source again, to build against crate work
# that has not been published yet. That is intended, not a regression (see
# .github/dependabot.yml) — a loan, repaid by re-pointing at the next
# published version once the burst lands. Either hatch is reported loudly (a
# NOTICE, so CI logs show it engaged) and held to what the lockfile records,
# not to judgments about how the manifest spells it: for a git source, the
# lock's source line must name the tapes-crates repository — the hatch is a
# loan against the real crates' history, never permission to take a
# same-named crate from elsewhere. That the lock is this manifest's intent is
# the build jobs' `--locked` to prove, here as everywhere. A path source is
# this checkout's neighbor, nothing published to name and nothing resolved to
# record, so the NOTICE is the whole of what is said. A MIX of git and
# registry sources is a failure outright — a git tapes crate carries its
# repository's siblings with it, and they meet their registry twins as
# duplicate crates: question (1)'s hazard — and path plus git at once is
# refused the same way. Engage one hatch.
#
# # What this script does NOT check
#
# Agreement with the sibling CLI's versions — the other client built on these
# crates. That comparison is deliberately one-directional, and it is the
# sibling that performs it: this repository is public and must stay runnable
# by anyone who clones it, and question (1) is answerable from this checkout
# alone, which is the whole of what this script asks.
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
# Requires: cargo, jq.
#
# Exit status: 0 when the question is answered yes, 1 when it is answered
# no, 2 when the script could not ask at all (unreadable manifest, missing
# tool). A question that could not be asked is never reported as a pass.

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

# The one repository the git escape hatch covers, as Cargo records it in the
# lockfile's `source = "git+..."` line.
TAPES_CRATES_REPO_RE='^git[+]https://github[.]com/papercomputeco/tapes-crates([.]git)?([?#]|$)'

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

for tool in cargo jq; do
    command -v "$tool" >/dev/null 2>&1 ||
        die "$tool is required to read and check the versions — nothing here was verified"
done

# --- extraction ---------------------------------------------------------------

# Every tapes-crates dependency each workspace member declares, as
# `name<TAB>kind<TAB>detail`: `registry` with the version requirement, `git`
# with the source URL, or `path`. Cargo reports a null source for a path
# override, because nothing resolvable is behind it but this filesystem.
manifest_deps() {
    jq -r --arg name_re "$TAPES_NAME_RE" --arg reg_re "$REGISTRY_RE" '
        [ .packages[].dependencies[]
          | select(.name | test($name_re))
          | if .source == null then
              { name: .name, kind: "path", detail: (.path // "local path") }
            elif (.source | test($reg_re)) then
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

# Every locked tapes crate on stdin, as `name<TAB>kind<TAB>detail` — the
# version for a registry source, the recorded `git+` URL (with its resolved
# commit) for a git source. The lock is what the build actually resolved, in
# the one canonical spelling Cargo writes; reading a stream rather than a
# path keeps the function usable on a lockfile that never touches disk.
lock_facts() {
    awk -v name_re="$TAPES_NAME_RE" -v reg_re="$REGISTRY_RE" '
        /^name = / {
            name = $3; gsub(/"/, "", name)
            matched = (name ~ name_re)
        }
        /^version = / && matched { version = $3; gsub(/"/, "", version) }
        /^source = / && matched {
            if ($0 ~ reg_re) { print name "\tregistry\t" version }
            else if ($0 ~ /git[+]/) {
                src = $3; gsub(/"/, "", src)
                print name "\tgit\t" src
            }
            matched = 0
        }
    '
}

# The one exit that reports: FAILED when any question answered no, OK (with
# the engaged hatch named) when none did.
finish() {
    echo
    if [[ "$fail" -ne 0 ]]; then
        echo "check-tapes-pins: FAILED" >&2
        exit 1
    fi
    echo "check-tapes-pins: OK${1:+ ($1)}"
    exit 0
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

# --- the escape hatches, before any question ----------------------------------

path_deps=()
git_deps=()
registry_deps=()
for dep in "${declared[@]}"; do
    IFS=$'\t' read -r name kind detail <<<"$dep"
    case "$kind" in
        path) path_deps+=("$name") ;;
        git) git_deps+=("$name") ;;
        registry) registry_deps+=("$name"$'\t'"$detail") ;;
    esac
done

fail=0

if [[ ${#path_deps[@]} -gt 0 ]]; then
    echo "NOTICE: PATH OVERRIDE ENGAGED — ${path_deps[*]} taken from a local path, not from crates.io (the tightest co-development loan; see the header)."
    echo
fi

# One hatch at a time: a path crate's version requirements on its siblings
# cannot unify with a git-pinned twin — the duplicate-crate hazard by another
# road.
if [[ ${#path_deps[@]} -gt 0 && ${#git_deps[@]} -gt 0 ]]; then
    echo "FAIL: $MANIFEST takes tapes crates from a local path (${path_deps[*]}) and by git revision (${git_deps[*]}) at once — one escape hatch at a time" >&2
    fail=1
    finish
fi

# Git and registry at once is question (1)'s hazard by construction: Cargo
# treats the two sources as different packages, so the git copy's siblings
# meet their registry twins as duplicate crates. All of them or none.
if [[ ${#git_deps[@]} -gt 0 && ${#registry_deps[@]} -gt 0 ]]; then
    echo "FAIL: $MANIFEST takes tapes crates by git revision (${git_deps[*]}) and from crates.io ($(for d in "${registry_deps[@]}"; do printf '%s ' "${d%%$'\t'*}"; done | sed 's/ $//')) at once — the escape hatch is every tapes crate or none" >&2
    fail=1
    finish
fi

if [[ ${#git_deps[@]} -gt 0 ]]; then
    echo "NOTICE: ESCAPE HATCH ENGAGED — ${git_deps[*]} taken by git revision, not from crates.io (the documented co-development loan; see .github/dependabot.yml)."
    echo

    # The provenance fact, read from the lock rather than the manifest: the
    # hatch is a loan against the real crates' history, never permission to
    # take a same-named crate from anywhere with a commit hash.
    while IFS=$'\t' read -r name kind detail; do
        [[ "$kind" == "git" ]] || continue
        if [[ "$detail" =~ $TAPES_CRATES_REPO_RE ]]; then
            echo "ok: ${name} is locked to the tapes-crates repository"
        else
            echo "FAIL: ${name} is locked to a git source that is not the tapes-crates repository (${detail}) — the escape hatch pins a revision of the real crates, never a same-named crate from elsewhere" >&2
            fail=1
        fi
    done < <(lock_facts <"$LOCKFILE")

    finish "escape hatch"
fi

if [[ ${#registry_deps[@]} -eq 0 ]]; then
    # Every tapes crate is a path override; nothing is resolved to record,
    # and the NOTICE above is the whole report.
    finish "path override"
fi

# --- question 1: one version of each crate ------------------------------------

locked=()
while IFS= read -r line; do
    [[ -n "$line" ]] && locked+=("$line")
done < <(lock_facts <"$LOCKFILE")

if [[ ${#locked[@]} -eq 0 ]]; then
    echo "FAIL: $MANIFEST declares tapes crates but $LOCKFILE resolves none of them — refresh the lockfile (cargo update) and commit it" >&2
    fail=1
    finish
fi

echo "Tapes crates in $LOCKFILE:"
for entry in "${locked[@]}"; do
    IFS=$'\t' read -r name kind detail <<<"$entry"
    printf '  %-28s %s  (%s)\n' "$name" "$detail" "$kind"
done
echo

locked_names="$(printf '%s\n' "${locked[@]}" | cut -f1)"
lock_git="$(printf '%s\n' "${locked[@]}" | awk -F'\t' '$2 == "git" { print $1 }' | xargs)"
dupes="$(sort <<<"$locked_names" | uniq -d | xargs)"

if [[ -n "$lock_git" ]]; then
    echo "FAIL: $LOCKFILE resolves ${lock_git} from a git source while $MANIFEST names only crates.io versions — the lockfile was written from a different manifest; refresh it (cargo update ${lock_git}) and commit it" >&2
    fail=1
fi

if [[ -n "$dupes" ]]; then
    echo "FAIL: $LOCKFILE resolves more than one version of: ${dupes} — one crate vendored twice, and a type from one is not the same type as its twin from the other" >&2
    echo "  find the semver-incompatible requirement (cargo tree --invert --package <name>) and bring both onto one version" >&2
    fail=1
fi

if [[ "$fail" -eq 0 ]]; then
    echo "ok: exactly one version of each of ${#locked[@]} tapes crate(s), all from crates.io"
fi

# Manifest-lock agreement at large is the build jobs' `--locked` to enforce;
# what is reported here is the one case these two files show on their face —
# a declared crate the lockfile never resolved.
for dep in "${registry_deps[@]}"; do
    name="${dep%%$'\t'*}"
    if ! grep -Fxq "$name" <<<"$locked_names"; then
        echo "FAIL: $MANIFEST declares $name but $LOCKFILE does not resolve it — refresh the lockfile (cargo update $name) and commit it" >&2
        fail=1
    fi
done

finish
