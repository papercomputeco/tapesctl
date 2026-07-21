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

echo "Installing to $INSTALL_DIR ..."
if [ -w "$INSTALL_DIR" ]; then
  install -m 0755 "$TMP_DIR/tapesctl" "$INSTALL_DIR/tapesctl"
else
  sudo install -m 0755 "$TMP_DIR/tapesctl" "$INSTALL_DIR/tapesctl"
fi

echo "Installed tapesctl:"
"$INSTALL_DIR/tapesctl"
