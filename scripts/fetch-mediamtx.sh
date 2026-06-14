#!/usr/bin/env bash
# Fetch the mediamtx binary for this platform into ./binaries/mediamtx.
# mediamtx (bluenviron/mediamtx, MIT) is the bundled WHIP/WHEP SFU. It is NOT
# committed — run this once after cloning, and the Tauri build bundles it as a
# sidecar (per-platform, like ffmpeg in SundayRec).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${MEDIAMTX_VERSION:-v1.9.3}"   # bump deliberately; verify config schema on change
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Darwin) plat="darwin" ;;
  Linux)  plat="linux" ;;
  *) echo "unsupported OS: $os (Windows: download the _windows_amd64.zip manually)"; exit 1 ;;
esac
case "$arch" in
  arm64|aarch64) a="arm64" ;;
  x86_64|amd64)  a="amd64" ;;
  *) echo "unsupported arch: $arch"; exit 1 ;;
esac

asset="mediamtx_${VERSION}_${plat}_${a}.tar.gz"
url="https://github.com/bluenviron/mediamtx/releases/download/${VERSION}/${asset}"
echo "→ $url"
tmp="$(mktemp -d)"
curl -fsSL "$url" -o "$tmp/m.tar.gz"
tar -xzf "$tmp/m.tar.gz" -C "$tmp"
mkdir -p binaries
mv "$tmp/mediamtx" binaries/mediamtx
chmod +x binaries/mediamtx
rm -rf "$tmp"
echo "✓ binaries/mediamtx ($("./binaries/mediamtx" --version 2>/dev/null || echo "$VERSION"))"
