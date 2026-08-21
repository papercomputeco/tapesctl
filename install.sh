#!/bin/bash
#
# tapesctl install script — downloads a pre-built `tapesctl` binary from
# https://download.tapes.dev and drops it in a user-owned directory.
# This is the "curl | bash" entry point uploaded to the bucket by the
# release pipeline's UploadInstallSh step.
#
#   curl -sSfL https://download.tapes.dev/tapesctl/install | bash
#
# Installs to $HOME/.local/bin by default — plain user file I/O end to
# end, no sudo. Because that directory is not on every default PATH, the
# installer writes a guarded PATH export into the detected shell's rc
# file, inside a `[|o=o|]` sentinel block. The one place sudo can still
# appear: when an old root-owned install is found in /usr/local/bin, it
# is removed with a single announced sudo — declining is fine, the
# install still succeeds and the manual removal command is printed.
#
# Requirements:
# * curl
# * uname
# * /tmp directory
# * sudo (only to clear an old root-owned install)
#
# Env knobs:
#   TAPESCTL_VERSION       — "latest" (default), "nightly", or "vX.Y.Z"
#   TAPESCTL_INSTALL_DIR   — install target (default: $HOME/.local/bin)
#   TAPESCTL_BASE_URL      — release bucket base URL
#                            (default: https://download.tapes.dev)
#   TAPESCTL_MIGRATE_FROM  — old install dir to clear (default: /usr/local/bin)

set -euo pipefail

# Everything below runs through main(), defined here and invoked on the very
# last line. bash executes a piped script incrementally, command by command,
# as bytes arrive — without this wrapper a transfer dying between the old
# binary's `rm -f` and the new one's `install` would leave no tapesctl at
# all. A truncated stream now fails to parse the unterminated function and
# executes nothing instead of half an install.
main() {

VERSION="${TAPESCTL_VERSION:-latest}"
BASE_URL="${TAPESCTL_BASE_URL:-https://download.tapes.dev}"

# Detect OS — `linux` or `darwin`. Anything else is unsupported; the
# release pipeline only emits these two.
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in
  linux*) OS="linux" ;;
  darwin*) OS="darwin" ;;
  *) echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

# Detect arch and normalize to the bucket layout names (amd64 / arm64).
# On macOS Apple Silicon `uname -m` returns "arm64"; on Linux ARM hosts
# it is usually "aarch64" (some distros still emit "arm64") — both map to
# the same bucket directory.
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# $HOME is dereferenced under `set -u`, and a login shell is not the only
# thing that runs this script — systemd units, minimal containers, and some CI
# runners have no HOME at all. Refuse here, before anything is downloaded,
# rather than aborting after a successful install with an "unbound variable"
# that reads like a bug in the installer.
if [ -z "${TAPESCTL_INSTALL_DIR:-}" ] && [ -z "${HOME:-}" ]; then
  echo "Neither TAPESCTL_INSTALL_DIR nor HOME is set; nowhere to install." >&2
  echo "Set one and re-run, e.g. TAPESCTL_INSTALL_DIR=/opt/bin" >&2
  exit 1
fi
INSTALL_DIR="${TAPESCTL_INSTALL_DIR:-${HOME:-}/.local/bin}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
DOWNLOAD_URL="$BASE_URL/tapesctl/$VERSION/$OS/$ARCH/tapesctl"

# Pick a sudo prefix only if the install dir isn't user-writable — which
# the $HOME/.local/bin default never is, so a stock install creates its
# directory with a plain mkdir and no privilege at all. The check walks
# to the nearest *existing* ancestor because the target (and its parent)
# may not exist yet; `mkdir -p` succeeds exactly when that ancestor is
# writable.
needs_privilege_for() {
  local dir="$1"
  while [ ! -d "$dir" ]; do
    dir="$(dirname "$dir")"
  done
  [ ! -w "$dir" ]
}

SUDO=""
if needs_privilege_for "$INSTALL_DIR"; then
  if command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
  fi
fi

echo "Downloading tapesctl $VERSION for $OS/$ARCH ..."
curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/tapesctl"

# Every artifact is published with a .sha256 sidecar; verify against it when a
# checksum tool exists. A missing sidecar is a hard failure — the release
# process always writes one, so its absence means the download is not what the
# release published.
if command -v sha256sum >/dev/null 2>&1; then
  SHA_TOOL="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA_TOOL="shasum -a 256"
else
  SHA_TOOL=""
  echo "warning: no sha256 tool found; skipping checksum verification"
fi
if [ -n "$SHA_TOOL" ]; then
  echo "Verifying checksum ..."
  curl -fsSL "$DOWNLOAD_URL.sha256" -o "$TMP_DIR/tapesctl.sha256"
  EXPECTED="$(awk '{print $1}' "$TMP_DIR/tapesctl.sha256")"
  ACTUAL="$($SHA_TOOL "$TMP_DIR/tapesctl" | awk '{print $1}')"
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Checksum mismatch: expected $EXPECTED, got $ACTUAL" >&2
    exit 1
  fi
fi

echo "Installing to $INSTALL_DIR ..."
$SUDO mkdir -p "$INSTALL_DIR"
# Unlink before install: writing onto a live binary's inode fails with
# ETXTBSY on Linux while a tapesctl process is running. Removing the old
# entry first makes the install create a fresh inode; the running process
# keeps its old one until it exits.
$SUDO rm -f "$INSTALL_DIR/tapesctl"
$SUDO install -m 0755 "$TMP_DIR/tapesctl" "$INSTALL_DIR/tapesctl"

###############################################################################
# PATH block — the sentinel-guarded rc-file write
###############################################################################
#
# $HOME/.local/bin is not on the default PATH (never on macOS, sometimes
# on Linux), so without this block the install would be unreachable.
# Everything written to the user's rc file lives between the `[|o=o|]`
# sentinel markers, and the block is rewritten in place on re-install,
# never duplicated. The markers must match the Rust rc-block remover
# byte-for-byte; a test in crates/tapesctl reads this script to pin them
# together.
#
# The marker text names tapesctl. Both this script and the Rust remover
# compare whole lines, so a paperctl block in the same rc file — same
# glyph, different text — is neither matched nor disturbed.

RC_BLOCK_BEGIN='# > [|o=o|] > tapesctl path > [|o=o|] >'
RC_BLOCK_END='# < [|o=o|] < tapesctl path < [|o=o|] <'

detect_shell() {
  local shell_name
  shell_name="$(basename "${SHELL:-}")"
  case "$shell_name" in
    bash|zsh|fish) echo "$shell_name" ;;
    *) return 1 ;;
  esac
}

# The rc file for shell $1, or failure when there is no home directory to
# resolve one under — reachable when TAPESCTL_INSTALL_DIR was given but HOME
# was not. With no HOME there is no rc file to edit either, so the caller
# degrades to printing the PATH line instead of failing the install.
shell_rc_file() {
  local shell_name="$1"
  [ -n "${HOME:-}" ] || return 1
  case "$shell_name" in
    bash) echo "$HOME/.bashrc" ;;
    zsh) echo "$HOME/.zshrc" ;;
    fish) echo "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
    *) return 1 ;;
  esac
}

# The PATH lines are double-guarded so the block is inert in every state
# it can be encountered in: the `case ":$PATH:"` membership check makes
# re-sourcing (nested shells, repeated logins) a no-op, and the -x
# binary-existence check keeps a block that outlives an uninstall silent.
posix_path_block_body() {
  local dir_expr="$1"
  printf '%s\n' \
    "if [ -x \"$dir_expr/tapesctl\" ]; then" \
    "  case \":\$PATH:\" in" \
    "    *\":$dir_expr:\"*) ;;" \
    "    *) export PATH=\"$dir_expr:\$PATH\" ;;" \
    "  esac" \
    "fi"
}

fish_path_block_body() {
  local dir_expr="$1"
  printf '%s\n' \
    "if test -x \"$dir_expr/tapesctl\"" \
    "    if not contains -- \"$dir_expr\" \$PATH" \
    "        set -gx PATH \"$dir_expr\" \$PATH" \
    "    end" \
    "end"
}

# Classify rc file $1: `none`, `ok`, or `malformed`.
#
# The three answers the Rust remover gives (`rc_block.rs`), computed the same
# way — exact whole-line comparison against both markers. This has to be its
# own step rather than a `grep` for the begin marker: grep matches substrings,
# while the stripper below matches whole lines, and that disagreement is how a
# marker line carrying a trailing space or a CRLF ending ends up "found" by the
# guard, stripped by nothing, and then appended a second time on every install.
rc_block_state() {
  awk -v b="$RC_BLOCK_BEGIN" -v e="$RC_BLOCK_END" '
    $0 == b { seen = 1; inblock = 1; next }
    inblock && $0 == e { inblock = 0; next }
    END {
      if (!seen) { print "none" }
      else if (inblock) { print "malformed" }
      else { print "ok" }
    }
  ' "$1"
}

# Print rc file $1 with the sentinel block (markers included) removed.
#
# Only ever called on a file `rc_block_state` called `ok`. A begin marker with
# no matching end would make this discard every line after it — the caller
# refuses that case rather than rewriting, exactly as the Rust remover does.
rc_block_strip() {
  awk -v b="$RC_BLOCK_BEGIN" -v e="$RC_BLOCK_END" '
    $0 == b { inblock = 1; next }
    inblock { if ($0 == e) inblock = 0; next }
    { print }
  ' "$1"
}

# Write body $2 into the sentinel block of rc file $1: an existing block
# is removed first (rewrite, never a second copy). Lines outside the
# markers are preserved (awk normalizes a missing trailing newline on the
# final line — content is untouched, only that byte can appear).
write_rc_block() {
  local rc_file="$1" body="$2" tmp state
  mkdir -p "$(dirname "$rc_file")"
  touch "$rc_file"
  state="$(rc_block_state "$rc_file")"
  if [ "$state" = "malformed" ]; then
    # A begin marker with no matching end. Rewriting would drop everything
    # after it — someone's aliases, exports, and whatever else lives below —
    # so the file is left exactly as it is and the user is told what to fix.
    # Refusing here is what keeps the promise that this script only ever
    # touches the lines between its own markers.
    echo
    echo "warning: $rc_file has a tapesctl begin marker with no matching end." >&2
    echo "Left it untouched rather than risk dropping what follows it." >&2
    echo "Remove the stray line and re-run, or add this to your shell config:" >&2
    echo >&2
    echo "  $(manual_path_line "$3" "$4")" >&2
    return 1
  fi
  if [ "$state" = "ok" ]; then
    tmp="$rc_file.tapesctl-tmp.$$"
    rc_block_strip "$rc_file" >"$tmp"
    # cat-over rather than mv: an rc file that is a symlink (dotfiles
    # managers) must stay a symlink — the redirection writes through it
    # to the target and keeps the file's owner/mode, where mv would
    # replace the symlink with a plain file.
    cat "$tmp" >"$rc_file"
    rm -f "$tmp"
  fi
  # Separate the block from existing content without stacking blank
  # lines across re-installs.
  if [ -s "$rc_file" ] && [ -n "$(tail -c 1 "$rc_file")" ]; then
    echo >>"$rc_file"
  fi
  if [ -s "$rc_file" ] && [ -n "$(tail -n 1 "$rc_file")" ]; then
    echo >>"$rc_file"
  fi
  {
    printf '%s\n' "$RC_BLOCK_BEGIN"
    printf '%s\n' "$body"
    printf '%s\n' "$RC_BLOCK_END"
  } >>"$rc_file"
}

# Whether the rc file can be written (or created). A file managed by nix
# or a dotfiles manager is typically a symlink to a read-only store path;
# `-w` follows the symlink and reports the target unwritable, so we can
# fall back to printing manual instructions instead of failing.
rc_file_writable() {
  local rc_file="$1"
  if [ -e "$rc_file" ] || [ -L "$rc_file" ]; then
    [ -w "$rc_file" ]
  else
    mkdir -p "$(dirname "$rc_file")" 2>/dev/null && [ -w "$(dirname "$rc_file")" ]
  fi
}

# One line the user can paste to put $1 on PATH in shell $2. The `$PATH`
# stays literal on purpose — the user pastes the line and their own shell
# expands it.
manual_path_line() {
  local dir_expr="$1" shell_name="$2"
  # shellcheck disable=SC2016
  case "$shell_name" in
    fish) printf 'fish_add_path %s\n' "$dir_expr" ;;
    *) printf 'export PATH="%s:$PATH"\n' "$dir_expr" ;;
  esac
}

install_path_block() {
  local shell_name rc_file dir_expr body
  if ! shell_name="$(detect_shell)"; then
    echo "Could not detect a supported shell (bash/zsh/fish)."
    echo "Add $INSTALL_DIR to your PATH manually to use tapesctl."
    return 0
  fi
  # Checked, not bare: under `set -e` an unchecked assignment from a failing
  # command substitution aborts the script — and this one fails whenever there
  # is no HOME to resolve an rc path under, which is precisely the case that
  # must degrade to printed instructions rather than a failed install.
  if ! rc_file="$(shell_rc_file "$shell_name")"; then
    echo
    echo "No home directory, so there is no shell config to update."
    echo "To use tapesctl, add $INSTALL_DIR to your PATH:"
    echo
    echo "  $(manual_path_line "$INSTALL_DIR" "$shell_name")"
    return 0
  fi
  # Keep the block portable when installing to the default location:
  # reference $HOME symbolically instead of baking in today's value.
  if [ "$INSTALL_DIR" = "$HOME/.local/bin" ]; then
    # Unexpanded by design: the rc block should track $HOME at source
    # time, not bake in today's value.
    # shellcheck disable=SC2016
    dir_expr='$HOME/.local/bin'
  else
    dir_expr="$INSTALL_DIR"
  fi

  # A read-only rc file (nix/home-manager, a dotfiles manager) must not
  # fail the install: tapesctl is already on disk, it just is not on PATH
  # yet. Tell the user exactly what to add, the way Homebrew does, and
  # keep going.
  if ! rc_file_writable "$rc_file"; then
    echo
    echo "Could not update $rc_file — it looks read-only (managed by nix or a dotfiles manager?)."
    echo "To put tapesctl on your PATH, add this to your shell config:"
    echo
    echo "  $(manual_path_line "$dir_expr" "$shell_name")"
    echo
    echo "Then restart your shell."
    return 0
  fi

  case "$shell_name" in
    fish) body="$(fish_path_block_body "$dir_expr")" ;;
    *) body="$(posix_path_block_body "$dir_expr")" ;;
  esac
  if write_rc_block "$rc_file" "$body" "$dir_expr" "$shell_name"; then
    echo "Ensured $dir_expr is on PATH via $rc_file"
    echo "(restart your shell or source $rc_file to pick it up)"
  fi
}

# Point out any OTHER `tapesctl` on PATH that this install now takes over.
# `~/.local/bin` is prepended, so this install wins once the shell
# reloads; the user should know we quietly shadowed something rather than
# be surprised by a stray `tapesctl` from a different location. `-ef`
# compares by inode, so the just-installed binary (reachable under any
# PATH spelling, e.g. a trailing-slash duplicate) is never flagged as a
# collision.
warn_shadowing_tapesctl() {
  local installed="$INSTALL_DIR/tapesctl" dir cand found=""
  local IFS=:
  for dir in $PATH; do
    [ -n "$dir" ] || continue
    cand="$dir/tapesctl"
    if [ -x "$cand" ] && ! [ "$cand" -ef "$installed" ]; then
      case "$found" in
        *"$cand"*) ;;
        *) found="${found:+$found }$cand" ;;
      esac
    fi
  done
  [ -n "$found" ] || return 0
  echo
  echo "Note: another 'tapesctl' is already on your PATH:"
  local p
  for p in $found; do echo "  $p"; done
  echo "tapesctl installed to $INSTALL_DIR and put it first on your PATH, so"
  echo "'tapesctl' runs this build once your shell reloads. The other command"
  echo "stays available at its full path."
}

###############################################################################
# Migration — remove an old root-owned install
###############################################################################
#
# A binary left in /usr/local/bin would shadow the new install in every
# context that still has the default PATH order — scripts, CI,
# GUI-launched tools — forking behavior by context forever. Removing it
# needs root (the directory is root-owned), so this is the one announced
# sudo left in tapesctl; declining degrades to a printed manual command,
# never a failed install.

# Resolve $1 to its physical path when the directory exists (following
# symlinks, collapsing `..` and duplicate slashes); otherwise just strip
# trailing slashes. Keeps the same-directory guard below a real
# comparison instead of a defeatable string match.
canonical_dir() {
  if [ -d "$1" ]; then
    (cd "$1" >/dev/null 2>&1 && pwd -P)
  else
    printf '%s\n' "$1" | sed 's:/*$::'
  fi
}

migrate_old_install() {
  local old_dir="${TAPESCTL_MIGRATE_FROM:-/usr/local/bin}"
  # Normalized comparison: TAPESCTL_INSTALL_DIR=/usr/local/bin/ (trailing
  # slash, symlinked spelling, …) must still be recognized as the old
  # directory itself — a string mismatch here would sudo-rm the binary
  # this very run just installed.
  if [ "$(canonical_dir "$old_dir")" = "$(canonical_dir "$INSTALL_DIR")" ]; then
    return 0
  fi
  if [ ! -e "$old_dir/tapesctl" ] && [ ! -L "$old_dir/tapesctl" ]; then
    return 0
  fi

  echo
  echo "Found an old tapesctl install in $old_dir."

  # Only escalate when the directory actually requires it. /usr/local/bin is
  # root-owned on stock macOS and most Linux, but Homebrew on Intel macOS owns
  # it as the user — prompting for a password there would be exactly the
  # gratuitous sudo this whole change set exists to remove. unlink(2) needs
  # write permission on the containing directory, not on the file.
  if [ -w "$old_dir" ] && rm -f "$old_dir/tapesctl"; then
    echo "Removed the old install from $old_dir."
    return 0
  fi

  echo "Removing it needs root — this is the last sudo tapesctl will ever request."
  if command -v sudo >/dev/null 2>&1 && sudo rm -f "$old_dir/tapesctl"; then
    echo "Removed the old install from $old_dir."
  else
    echo "warning: could not remove the old install. Remove it manually with:" >&2
    echo "  sudo rm -f $old_dir/tapesctl" >&2
  fi
  return 0
}

install_path_block
migrate_old_install

echo
echo "Installed tapesctl:"
# `version`, not a bare invocation: bare tapesctl prints help and exits 2,
# which under `set -e` would report every successful install as a failure.
#
# Advisory, hence `|| true`: the bytes are on disk and checksum-verified by
# now, so a binary that installs correctly but cannot *run* — an older glibc
# than the release was built against, a noexec install dir — must still reach
# the shadow warning and the next-steps below. Aborting here would report a
# successful install as a failure.
"$INSTALL_DIR/tapesctl" version || \
  echo "warning: the installed binary did not run here; see the note above." >&2
warn_shadowing_tapesctl
echo
cat <<'EOF'
Next:

  tapesctl config set api-url <url>   # name your server once
  tapesctl start claude               # capture your first session
  tapesctl upgrade                    # replace this binary with the newest release
EOF
}

main "$@"
