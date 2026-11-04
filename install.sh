#!/bin/sh
# Installs the latest Scarlet release for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/joakimunge/scarlet/main/install.sh | sh
#
# SCARLET_INSTALL_DIR picks the folder (default ~/.local/bin); SCARLET_VERSION
# picks a release such as v0.1.0 (default: the latest).
set -eu

repo="joakimunge/scarlet"
dir="${SCARLET_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "scarlet: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) fail "there is no release for $(uname -s); build it with: cargo install --git https://github.com/$repo --locked" ;;
esac
case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) fail "there is no release for $(uname -m) processors" ;;
esac
# A shell under Rosetta reports x86_64 on Apple silicon; the native build is better.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
    arch=aarch64
fi
target="$arch-$os"

if [ -n "${SCARLET_VERSION:-}" ]; then
    base="https://github.com/$repo/releases/download/$SCARLET_VERSION"
else
    base="https://github.com/$repo/releases/latest/download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading Scarlet for $target"
curl -fsSL "$base/scarlet-$target.tar.gz" -o "$tmp/scarlet.tar.gz" || fail "download failed"
curl -fsSL "$base/scarlet-$target.tar.gz.sha256" -o "$tmp/scarlet.sha256" || fail "checksum download failed"

expected=$(cut -d' ' -f1 "$tmp/scarlet.sha256")
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/scarlet.tar.gz" | cut -d' ' -f1)
else
    actual=$(shasum -a 256 "$tmp/scarlet.tar.gz" | cut -d' ' -f1)
fi
[ "$expected" = "$actual" ] || fail "the download is damaged (checksum mismatch); try again"

tar -xzf "$tmp/scarlet.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/scarlet-$target/scarlet" "$dir/scarlet"
echo "Installed $("$dir/scarlet" --version) to $dir/scarlet"

case ":$PATH:" in
    *":$dir:"*) echo "Run: scarlet" ;;
    *) echo "$dir is not on your PATH. Add it, for example: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc" ;;
esac
