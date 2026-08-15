#!/bin/sh
# Install readout — usage statistics for Claude Code and Codex.
#
#   curl -fsSL https://github.com/atoz03/readout-cli/releases/latest/download/install.sh | sh
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
need awk
need mktemp

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
  # Git Bash, MSYS and Cygwin can run this script but not the binary it would
  # fetch; there is a native installer for that, and it is worth naming rather
  # than leaving as "unsupported OS: MINGW64_NT-10.0".
  MINGW*|MSYS*|CYGWIN*)
    die "this is the Unix installer. On Windows run:
  powershell -c \"irm https://raw.githubusercontent.com/${REPO}/main/install.ps1 | iex\"" ;;
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
if ! printf '%s\n' "$version" | awk '/^v[0-9]+\.[0-9]+\.[0-9]+$/ { ok = 1 } END { exit !ok }'; then
  die "invalid release version: $version (expected vX.Y.Z)"
fi

archive="readout-${version}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${version}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "readout ${version} (${target})"
fetch "${base}/${archive}" "${tmp}/${archive}" || die "no build for ${target} in ${version}"

# Verify against the checksums published with the release. A silent mismatch
# is the one failure mode worth spending a second on.
fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" 2>/dev/null \
  || die "could not download SHA256SUMS for ${version}"
expected="$(awk -v name="$archive" '
  { file = $2; sub(/^\*/, "", file); if (file == name) { print $1; exit } }
' "${tmp}/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS has no entry for ${archive}"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${archive}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${tmp}/${archive}" | awk '{print $1}')"
else
  die "sha256sum or shasum is required to verify the download"
fi
if [ "$actual" != "$expected" ]; then
  die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"
fi
say "  checksum ok"

# 只把精确匹配的二进制成员输出到普通文件，不信任归档里的路径、链接、属主或权限。
member="readout-${version}-${target}/readout"
binary="${tmp}/readout"
tar xOzf "${tmp}/${archive}" "$member" > "$binary" \
  || die "the archive did not contain a readable readout binary"
[ -s "$binary" ] || die "the archive contained an empty readout binary"

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
