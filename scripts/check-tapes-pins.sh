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
#     (`cargo metadata --locked`), and ANY refusal fails: classifying Cargo's
#     stderr into drift-versus-environment was a losing game (each new failure
#     arrived spelled differently), and this check runs beside build jobs that
#     need the same network and sources — an environment that cannot answer
#     this question cannot build the crate either.
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
# regression (see .github/dependabot.yml). This script reports it loudly (a
# NOTICE, so CI logs show the hatch engaged) and holds it to the git-era
# guarantees instead of the registry ones: every git entry must carry
# `rev = <commit sha>` — a branch moves under the lockfile, so a branch is
# not a pin — every tapes crate must name the SAME revision, and question (2)
# still runs, because a git pin and its lockfile can disagree exactly like a
# version can. Only question (3) is skipped, because a git source has no
# version on crates.io to ask about. A MIX of git and registry sources for
# the tapes crates is a failure outright, because a git tapes-client carries
# its repository's own siblings with it, and those meet their registry twins
# as duplicate crates: exactly the two-types hazard of question (1).
#
# A `path = "..."` override is the third source kind and the tightest
# co-development loop of all: the crate is this checkout's neighbor, so there
# is no published version to probe and no revision to hold still. It is
# reported with the same NOTICE loudness and the registry questions are
# skipped for that crate — never failed, and never mistaken for a registry
# dependency.
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
# for a reason that is not evidence — see question 3), 1 when one is
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

# The registry endpoint question (3) probes, one crate-version at a time.
CRATES_IO_API='https://crates.io/api/v1/crates'

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
# `name<TAB>kind<TAB>detail`: kind `registry` with the version requirement,
# kind `git` with the source URL (the escape hatch, or a mistake — the caller
# decides which), or kind `path` with the local path. A null source is a path
# override, not a registry dependency: Cargo reports no source for it because
# there is nothing resolvable behind it but this filesystem.
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

# Question 2's whole body — the lockfile agrees with the manifest — as a
# function, because the escape hatch asks it too: a git pin and its lockfile
# can disagree exactly like a version can.
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
# Any failure fails, and Cargo's own words say why. This deliberately does
# NOT classify the error: sorting drift from environment by pattern-matching
# stderr was a losing game (stale-lock phrasing, unsatisfiable requirements,
# and unfetchable revisions each arrived spelled differently), and the check
# runs beside build jobs that need the same network and the same sources —
# an environment that cannot answer this question cannot build the crate
# either, so a red here never hides a green that mattered.
check_lock_agreement() {
    if locked_err="$(cargo metadata --format-version 1 --locked --manifest-path "$MANIFEST" 2>&1 >/dev/null)"; then
        echo "ok: $LOCKFILE satisfies $MANIFEST (cargo --locked)"
    else
        echo "FAIL: \`cargo metadata --locked\` refused — $LOCKFILE, $MANIFEST, and their sources do not line up:" >&2
        printf '%s\n' "$locked_err" | sed 's/^/  /' >&2
        echo "  if the manifest moved, refresh the lockfile (cargo update <crate>) and commit it" >&2
        fail=1
    fi
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
warned=0

if [[ ${#path_deps[@]} -gt 0 ]]; then
    cat <<EOF
NOTICE: PATH OVERRIDE ENGAGED — ${path_deps[*]} taken from a local path, not from crates.io.

This is the same co-development loan as the git escape hatch, one step
tighter — the crate is this checkout's neighbor, so there is no published
version to probe and no revision to hold still — and it is repaid the same
way: re-point at the next published version once the burst lands. The
registry questions are not asked for these crates while it is in place.

EOF
fi

# Both escape hatches at once is not a supported state: a path crate's own
# version requirements on its siblings cannot unify with a git-pinned twin,
# which is the duplicate-crate hazard by another road. Engage one hatch.
if [[ ${#path_deps[@]} -gt 0 && ${#git_deps[@]} -gt 0 ]]; then
    echo "FAIL: $MANIFEST takes tapes crates from a local path (${path_deps[*]}) and by git revision (${git_deps[*]}) at once — one escape hatch at a time" >&2
    echo
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi

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
NOTICE: ESCAPE HATCH ENGAGED — ${git_deps[*]} taken by git revision, not from crates.io.

This is the documented co-development loan (see .github/dependabot.yml): fine
while unpublished crate work is being built against, and repaid by re-pointing
at the next published version once it lands. The registry questions cannot be
asked of a git source, but the git-era guarantees still hold and are checked
here: one immovable revision for every tapes crate, and a lockfile that
agrees with the manifest.

EOF

    # `rev = <commit sha>` or it is not a pin: a branch (or a bare git URL)
    # moves under the lockfile, and Cargo accepts a branch name in `rev` too —
    # a name is a pointer, not a revision. And one revision for all of them,
    # or the git siblings meet each other at two points of one history: the
    # mix hazard again, one source kind in.
    #
    # The source must also BE the tapes-crates repository: a rev proves
    # immovability, not provenance, and the hatch is a loan against the real
    # crates' history — not permission to take a same-named crate from
    # anywhere with a commit hash.
    repo_re='^git\+https://github\.com/papercomputeco/tapes-crates(\.git)?([?#]|$)'
    # Whatever was written in `rev = ...`, as Cargo echoes it into the source
    # string. The requirement is the FULL forty-hex commit id, equal to the
    # resolution Cargo wrote after `#` in the lock's source line. Nothing
    # shorter or ref-shaped: a full commit id makes git fetch the object
    # itself, so no ref by any name — not even a branch named after its own
    # target — is ever consulted, and there is nothing left that can move.
    rev_re='[?]rev=([^#&]+)'
    hatch_rev=""
    for dep in "${declared[@]}"; do
        IFS=$'\t' read -r name kind detail <<<"$dep"
        [[ "$kind" == "git" ]] || continue
        if ! [[ "$detail" =~ $repo_re ]]; then
            echo "FAIL: ${name} is taken from a git source that is not the tapes-crates repository (source: ${detail}) — the escape hatch pins a revision of the real crates, never a same-named crate from elsewhere" >&2
            fail=1
            continue
        fi
        if [[ "$detail" =~ $rev_re ]]; then
            rev="${BASH_REMATCH[1]}"
            resolved="$(awk -v crate="$name" '
                $0 == "name = \"" crate "\"" { found = 1; next }
                found && /^source = "git\+/ {
                    if (match($0, /#[0-9a-f]{40}"$/)) {
                        print substr($0, RSTART + 1, RLENGTH - 2)
                    }
                    exit
                }
                found && /^\[\[package\]\]/ { exit }
            ' "$LOCKFILE")"
            if [[ -z "$resolved" ]]; then
                echo "FAIL: ${name} has no resolved git commit in ${LOCKFILE} — the lockfile does not carry this pin; refresh it (cargo update ${name}) and commit it" >&2
                fail=1
            elif [[ "${rev,,}" != "$resolved" ]]; then
                echo "FAIL: ${name} names rev '${rev}' but the pin the escape hatch takes is the full forty-hex commit id — ${LOCKFILE} resolved this to ${resolved}; write that" >&2
                fail=1
            else
                echo "ok: ${name} is pinned to commit ${resolved}"
            fi
            if [[ -z "$hatch_rev" ]]; then
                hatch_rev="$rev"
            elif [[ "$rev" != "$hatch_rev" ]]; then
                echo "FAIL: ${name} is pinned to ${rev} while another tapes crate is pinned to ${hatch_rev} — the escape hatch is one revision for all of them" >&2
                fail=1
            fi
        else
            echo "FAIL: ${name} is taken from git without \`rev = <commit sha>\` (source: ${detail}) — a branch or tag moves under the lockfile; pin the exact revision being built against" >&2
            fail=1
        fi
    done
    echo

    check_lock_agreement

    echo
    if [[ "$fail" -ne 0 ]]; then
        echo "check-tapes-pins: FAILED" >&2
        exit 1
    fi
    echo "check-tapes-pins: OK (escape hatch)"
    exit 0
fi

if [[ ${#registry_deps[@]} -eq 0 ]]; then
    # Every tapes crate is a path override. The registry questions have
    # nothing left to ask; what remains askable is question 2.
    check_lock_agreement

    echo
    if [[ "$fail" -ne 0 ]]; then
        echo "check-tapes-pins: FAILED" >&2
        exit 1
    fi
    echo "check-tapes-pins: OK (path override)"
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
# The body lives in check_lock_agreement, defined with the extraction helpers,
# because the escape hatch asks this question too.

check_lock_agreement

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
        "${CRATES_IO_API}/${name}/${version}" 2>/dev/null || echo 000)"

    case "$code" in
        200)
            # A 200 whose body does not carry a `yanked` boolean is not an
            # answer: the registry was reached, but what it said could not be
            # read, and an unreadable answer is the same non-evidence as no
            # answer at all. Only a parsed boolean settles the question —
            # `false` is the pass, `true` is the failure, anything else is
            # the advisory path.
            yanked="$(jq -er '.version.yanked | select(type == "boolean") | tostring' <"$body" 2>/dev/null || echo unreadable)"
            case "$yanked" in
                true)
                    echo "FAIL: ${name} ${version} is YANKED on crates.io — it builds here out of a cached registry and nowhere clean; move to a live version" >&2
                    fail=1
                    ;;
                false)
                    echo "ok: ${name} ${version} exists on crates.io"
                    ;;
                *)
                    echo "WARNING (not a failure): crates.io answered 200 for ${name} ${version} but the body did not parse to a yanked verdict — existence unverified on this run" >&2
                    warned=$((warned + 1))
                    ;;
            esac
            ;;
        404)
            echo "FAIL: ${name} ${version} does not exist on crates.io — it resolves here out of a cached or private registry and will not resolve for a clean clone" >&2
            fail=1
            ;;
        *)
            echo "WARNING (not a failure): could not ask crates.io about ${name} ${version} (HTTP ${code}) — existence unverified on this run" >&2
            warned=$((warned + 1))
            ;;
    esac
    rm -f "$body"
done

echo
if [[ "$fail" -ne 0 ]]; then
    echo "check-tapes-pins: FAILED" >&2
    exit 1
fi
# An unanswered question is not a pass, and the summary line says so: OK
# means every question answered yes; OK-with-warnings means nothing was
# answered no, and names how many answers are missing.
if [[ "$warned" -ne 0 ]]; then
    echo "check-tapes-pins: OK (${warned} question(s) unanswered — see warnings above)"
else
    echo "check-tapes-pins: OK"
fi
