#!/bin/sh
# playpen installer: fetch the latest release tarball for this machine's
# architecture, verify the checksum, install to ~/.local/bin.
#
# Usage: curl -fsSL https://github.com/aaberg/playpen/releases/latest/download/install.sh | sh
set -eu

REPO="aaberg/playpen"
BASE="https://github.com/$REPO/releases/latest/download"
BIN_DIR="${HOME}/.local/bin"

fail() {
  echo "playpen: $*" >&2
  exit 1
}

os=$(uname -s)
case "$os" in
  Linux) ;;
  Darwin) fail "macOS support is phase 4 (roadmap) - run inside WSL2/Linux or build from source" ;;
  *) fail "unsupported OS: $os" ;;
esac

arch=$(uname -m)
case "$arch" in
  x86_64 | amd64) target="x86_64-linux-musl" ;;
  aarch64 | arm64) target="aarch64-linux-musl" ;;
  *) fail "unsupported architecture: $arch (supported: x86_64, aarch64)" ;;
esac

asset="playpen-$target.tar.gz"
tmp=$(mktemp -d) || fail "could not create a temp directory"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "playpen: downloading $asset"
curl -fsSL -o "$tmp/$asset" "$BASE/$asset" || fail "could not download $asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$BASE/SHA256SUMS" || fail "could not download SHA256SUMS"

# Verify against the release's checksum file (both come over TLS from the
# same release, guarding against corrupted downloads).
grep " $asset\$" "$tmp/SHA256SUMS" > "$tmp/expected" || fail "$asset missing from SHA256SUMS"
(cd "$tmp" && sha256sum -c expected) || fail "checksum mismatch - the download is corrupted"

command -v playpen > /dev/null && echo "playpen: replacing $(command -v playpen)"
mkdir -p "$BIN_DIR" || fail "could not create $BIN_DIR"
tar -xzf "$tmp/$asset" -C "$BIN_DIR" || fail "could not extract $asset"
chmod 755 "$BIN_DIR/playpen"

case ":$PATH:" in
  *":$BIN_DIR:"*) echo "playpen: installed $BIN_DIR/playpen" ;;
  *)
    echo "playpen: installed $BIN_DIR/playpen"
    echo "playpen: add it to your PATH:  export PATH=\"\$HOME/.local/bin:\$PATH\""
    ;;
esac
echo "playpen: next: playpen playpen-jvm   (pulls the flavor, builds the harness layer, runs)"
