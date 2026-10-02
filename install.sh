#!/bin/sh
# powerqueue installer — downloads the latest release binary for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/aleandros/powerqueue/main/install.sh | sh
#
# Re-run the same command to update. Environment overrides:
#   POWERQUEUE_VERSION=v0.4.0   install a specific tag instead of the latest
#   POWERQUEUE_INSTALL_DIR=...  install somewhere other than /usr/local/bin or ~/.local/bin
set -e

REPO="aleandros/powerqueue"
BIN="powerqueue"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)      echo "Unsupported OS: $OS (build from source: cargo install --git https://github.com/${REPO})" >&2; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64)  arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

TARGET="${arch}-${os}"

if [ -n "${POWERQUEUE_VERSION:-}" ]; then
  LATEST="$POWERQUEUE_VERSION"
else
  LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')
fi
if [ -z "$LATEST" ]; then
  echo "Could not determine the latest release of ${REPO}" >&2
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST}/${BIN}-${TARGET}.tar.gz"
echo "Downloading ${BIN} ${LATEST} for ${TARGET}..."

if [ -n "${POWERQUEUE_INSTALL_DIR:-}" ]; then
  INSTALL_DIR="$POWERQUEUE_INSTALL_DIR"
  mkdir -p "$INSTALL_DIR"
elif [ -w /usr/local/bin ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="${HOME}/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "$URL" | tar xz -C "$TMP"
if [ ! -f "$TMP/$BIN" ]; then
  echo "Archive did not contain ${BIN}" >&2
  exit 1
fi

# Replace atomically so a running daemon keeps its old inode until restart.
mv "$TMP/$BIN" "$INSTALL_DIR/$BIN.new"
chmod +x "$INSTALL_DIR/$BIN.new"
mv -f "$INSTALL_DIR/$BIN.new" "$INSTALL_DIR/$BIN"

echo "Installed ${BIN} ${LATEST} to ${INSTALL_DIR}/${BIN}"

case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Note: add ${INSTALL_DIR} to your PATH" ;;
esac

if "$INSTALL_DIR/$BIN" --version >/dev/null 2>&1; then
  echo "Next: run \`${BIN} init\` inside your repository (or \`${BIN} doctor\` if already set up)."
fi
