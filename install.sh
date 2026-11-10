#!/bin/sh
# Installs MemoryOS on this machine and connects Claude Code to it:
#
#   curl -fsSL https://raw.githubusercontent.com/feryadialoi/memoryos/main/install.sh | bash
#
# Downloads the latest release into ~/.local/bin, checks its SHA-256, then runs
# `memoryos setup --local`: SQLite in ~/.local/share/memoryos, the server on 127.0.0.1:8080, the
# bge-reranker-base reranker, two keys, and Claude Code's hooks and MCP in ~/.claude.
#
# MEMORYOS_BIN_DIR   where to put the program (default ~/.local/bin)
# MEMORYOS_AGENT     claude-code (default), opencode or pi
# MEMORYOS_VERSION   a release tag such as v0.1.3 (default: the latest)
#
# Run it again to repair an install; `memoryos update` updates one.
set -eu

repo="feryadialoi/memoryos"
bin_dir="${MEMORYOS_BIN_DIR:-$HOME/.local/bin}"
agent="${MEMORYOS_AGENT:-claude-code}"
version="${MEMORYOS_VERSION:-latest}"

fail() {
    echo "memoryos install: $*" >&2
    exit 1
}

case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) fail "$(uname -s) is not supported; MemoryOS runs on macOS and Linux" ;;
esac
case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) fail "$(uname -m) is not supported" ;;
esac
if [ "$os" = macos ] && [ "$arch" = x86_64 ]; then
    fail "there is no build for Intel Macs; build from source with: cargo install --git https://github.com/$repo memoryos-server"
fi

asset="memoryos-$os-$arch.tar.gz"
if [ "$version" = latest ]; then
    url="https://github.com/$repo/releases/latest/download/$asset"
else
    url="https://github.com/$repo/releases/download/$version/$asset"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "Downloading $asset"
curl -fsSL "$url" -o "$work/$asset" || fail "cannot download $url"
curl -fsSL "$url.sha256" -o "$work/$asset.sha256" || fail "cannot download $url.sha256"

expected=$(cut -d ' ' -f 1 < "$work/$asset.sha256")
if command -v sha256sum > /dev/null 2>&1; then
    actual=$(sha256sum "$work/$asset" | cut -d ' ' -f 1)
else
    actual=$(shasum -a 256 "$work/$asset" | cut -d ' ' -f 1)
fi
[ "$actual" = "$expected" ] || fail "SHA-256 mismatch: got $actual, the release says $expected"

tar -xzf "$work/$asset" -C "$work"
mkdir -p "$bin_dir"
install -m 755 "$work/memoryos" "$bin_dir/memoryos"
echo "Installed $("$bin_dir/memoryos" --version) at $bin_dir/memoryos"

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *)
        # The agent starts `memoryos mcp` by name, so the program must be on PATH.
        export PATH="$bin_dir:$PATH"
        echo "Add $bin_dir to PATH in your shell profile, for example:"
        echo "  echo 'export PATH=\"$bin_dir:\$PATH\"' >> ~/.zshrc"
        ;;
esac

"$bin_dir/memoryos" setup --local --agent "$agent" --global
