#!/usr/bin/env bash
# Installer: download a Stratz release binary (or build from source) and
# install it onto PATH. Usage:
#   ./scripts/install.sh                 # build from source (requires cargo)
#   ./scripts/install.sh --from-release  # download latest GitHub release
set -euo pipefail

BIN_DIR="${STRATZ_BIN_DIR:-$HOME/.local/bin}"
REPO="${STRATZ_REPO:-DeLaser123/celis-stratz}"

case "${1:-}" in
  --from-release)
    echo "Downloading latest release from https://github.com/$REPO/releases/latest"
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m)"
    case "$OS" in
      linux) asset="stratz-linux-${ARCH}.tar.gz" ;;
      darwin) asset="stratz-macos-${ARCH}.tar.gz" ;;
      *) echo "unsupported OS: $OS (use --source build or download manually)"; exit 1 ;;
    esac
    url="https://github.com/$REPO/releases/latest/download/$asset"
    tmp="$(mktemp -d)"
    curl -fsSL "$url" -o "$tmp/$asset"
    tar -xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$BIN_DIR"
    install -m 0755 "$tmp/stratz" "$BIN_DIR/stratz"
    echo "installed: $BIN_DIR/stratz"
    echo "auto-update: stratz checks GitHub releases daily (stratz self-update --check)"
    ;;
  *)
    echo "building from source..."
    cargo build --release
    mkdir -p "$BIN_DIR"
    install -m 0755 target/release/stratz "$BIN_DIR/stratz"
    mkdir -p "$HOME/.stratz-cli"
    printf '%s' "$(cd . && pwd)" > "$HOME/.stratz-cli/source.txt"
    echo "source marker: $HOME/.stratz/source.txt (dev auto-update enabled)"
    echo "installed: $BIN_DIR/stratz"
    ;;
esac

echo
echo "add to PATH if needed:  export PATH=\"$BIN_DIR:\$PATH\""
echo "next: mkdir my-strategies && cd my-strategies && stratz init"
