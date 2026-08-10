#!/bin/bash

# tapesctl install script for Linux and macOS.
# Requirements:
# * curl
# * uname
# * install
# * sudo (when the install directory is not writable)
# * /tmp directory

set -e

VERSION="${TAPESCTL_VERSION:-latest}"
BASE_URL="${TAPESCTL_BASE_URL:-https://download.tapes.dev}"

# Detect OS
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$OS" in
  linux*) OS="linux" ;;
  darwin*) OS="darwin" ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

# Detect architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

INSTALL_DIR="${TAPESCTL_INSTALL_DIR:-/usr/local/bin}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
DOWNLOAD_URL="$BASE_URL/tapesctl/$VERSION/$OS/$ARCH/tapesctl"

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
if [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$TMP_DIR/tapesctl" "$INSTALL_DIR/tapesctl"
else
  sudo install -m 0755 "$TMP_DIR/tapesctl" "$INSTALL_DIR/tapesctl"
fi

echo "Installed tapesctl:"
# `version`, not a bare invocation: bare tapesctl prints help and exits 2,
# which under `set -e` would report every successful install as a failure.
"$INSTALL_DIR/tapesctl" version
