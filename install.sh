#!/bin/sh
# Install readout — usage statistics for Claude Code and Codex.
#
#   curl -fsSL https://raw.githubusercontent.com/atoz03/readout-cli/main/install.sh | sh
#
# Downloads the release binary for this platform, verifies its published
# SHA-256, and installs it. Nothing else on the system is touched.
#
#   READOUT_VERSION=v0.1.0   pin a version instead of taking the latest
#   READOUT_INSTALL_DIR=...  install somewhere other than ~/.local/bin
set -eu

REPO="atoz03/readout-cli"
INSTALL_DIR="${READOUT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"; }

need uname
need mkdir
need tar

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  fetch_stdout() { wget -qO- "$1"; }
else
  die "neither curl nor wget is available"
fi

# Platform triple. Linux builds are static musl, so the distro does not matter.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  os_part="unknown-linux-musl" ;;
  Darwin) os_part="apple-darwin" ;;
  *) die "unsupported OS: $os. Windows binaries are on the releases page; or build with 'cargo install readout'." ;;
esac
case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) die "unsupported architecture: $arch. Try 'cargo install readout' to build from source." ;;
esac
target="${arch_part}-${os_part}"

version="${READOUT_VERSION:-}"
if [ -z "$version" ]; then
  need sed
  version="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$version" ] || die "could not determine the latest release; set READOUT_VERSION to pin one"
fi

archive="readout-${version}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${version}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "readout ${version} (${target})"
fetch "${base}/${archive}" "${tmp}/${archive}" || die "no build for ${target} in ${version}"

# Verify against the checksums published with the release. A silent mismatch
# is the one failure mode worth spending a second on.
if fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" 2>/dev/null; then
  expected="$(grep " ${archive}\$" "${tmp}/SHA256SUMS" | awk '{print $1}' || true)"
  if [ -n "$expected" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "${tmp}/${archive}" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      actual="$(shasum -a 256 "${tmp}/${archive}" | awk '{print $1}')"
    else
      actual=""
      say "  no sha256 tool found — skipping checksum verification"
    fi
    if [ -n "$actual" ] && [ "$actual" != "$expected" ]; then
      die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"
    fi
    [ -n "$actual" ] && say "  checksum ok"
  fi
fi

tar xzf "${tmp}/${archive}" -C "$tmp"
binary="${tmp}/readout-${version}-${target}/readout"
[ -f "$binary" ] || die "the archive did not contain a readout binary"

mkdir -p "$INSTALL_DIR"
chmod +x "$binary"
mv "$binary" "${INSTALL_DIR}/readout"
say "  installed ${INSTALL_DIR}/readout"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) say "" ; say "Run 'readout' to open the dashboard." ;;
  *)
    say ""
    say "${INSTALL_DIR} is not on your PATH. Add it:"
    say "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile"
    say "Then run 'readout' to open the dashboard."
    ;;
esac
