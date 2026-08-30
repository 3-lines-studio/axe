#!/bin/sh
set -eu

cd "$(dirname "$0")/.."

os=$(uname -s | tr '[:upper:]' '[:lower:]')
case $os in
    linux | darwin) ;;
    *) echo "package: unsupported os: $os" >&2; exit 1 ;;
esac

arch=$(uname -m)
case $arch in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) echo "package: unsupported arch: $arch" >&2; exit 1 ;;
esac

triple="$arch-unknown-$os-gnu"
[ "$os" = darwin ] && triple="$arch-apple-darwin"

flags='["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always"]'
[ "$os" = linux ] && flags='["-Cforce-unwind-tables=no","-Cllvm-args=-enable-machine-outliner=always","-Clink-args=-no-pie"]'

cargo +nightly build --release --target "$triple" --config "build.rustflags=$flags"

out="axe-$os-$arch"
cp "target/$triple/release/axe" "$out"

if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$out" > "$out.sha256"
else
    shasum -a 256 "$out" > "$out.sha256"
fi

echo "$out"
