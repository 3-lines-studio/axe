#!/bin/sh
set -eu

# axe installer — curl -fsSL <url>/install.sh | sh
# Installs the prebuilt binary to $AXE_PREFIX/bin (default ~/.local/bin).
# Overrides: AXE_REPO (github owner/repo), AXE_VERSION (pinned tag, default latest),
# AXE_BASE_URL (full download base, for mirrors and testing), AXE_PREFIX.

repo="${AXE_REPO:-3-lines-studio/axe}"
if [ -n "${AXE_BASE_URL:-}" ]; then
    base="$AXE_BASE_URL"
elif [ -n "${AXE_VERSION:-}" ]; then
    base="https://github.com/$repo/releases/download/$AXE_VERSION"
else
    base="https://github.com/$repo/releases/latest/download"
fi

prefix="${AXE_PREFIX:-${PREFIX:-$HOME/.local}}"
bindir="$prefix/bin"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
case $os in
    linux | darwin) ;;
    *) echo "install: unsupported os: $os" >&2; exit 1 ;;
esac

arch=$(uname -m)
case $arch in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) echo "install: unsupported arch: $arch" >&2; exit 1 ;;
esac

if ! command -v curl >/dev/null 2>&1; then
    echo "install: curl is required" >&2
    exit 1
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

name="axe-$os-$arch"
url="$base/$name"
tmp="${TMPDIR:-/tmp}/axe-install-$$"
trap 'rm -f "$tmp" "$tmp.sha256"' EXIT HUP INT TERM

echo "downloading $name"
curl -fsSL "$url" -o "$tmp" || { echo "install: download failed: $url" >&2; exit 1; }
curl -fsSL "$url.sha256" -o "$tmp.sha256" || { echo "install: checksum fetch failed: $url.sha256" >&2; exit 1; }

want=$(awk '{print $1}' "$tmp.sha256")
got=$(sha256 "$tmp")
if [ "$want" != "$got" ]; then
    echo "install: checksum mismatch (want $want, got $got)" >&2
    exit 1
fi

mkdir -p "$bindir"
install -m 0755 "$tmp" "$bindir/axe"

case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "install: $bindir is not on PATH — add it, e.g. export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac

echo "installed axe ($name) to $bindir/axe"
echo "next: set OPENAI_API_KEY and run axe"
