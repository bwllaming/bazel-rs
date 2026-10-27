#!/usr/bin/env bash
# Installs the pinned zig into .direnv/zig (zig is only the C compiler of the
# static musl release builds, docs/RELEASING.md; the normal build does not
# need it). Verifies the download's sha256 against the pins below.
set -euo pipefail
cd "$(dirname "$0")/.."
version=0.16.0
case "$(uname -sm)" in
  'Linux x86_64')
    arch=x86_64
    sha256=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00 ;;
  'Linux aarch64')
    arch=aarch64
    sha256=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17 ;;
  *) echo 'setup-zig.sh supports Linux x86_64 and aarch64 hosts.' >&2; exit 1 ;;
esac
name="zig-$arch-linux-$version"
if test -x .direnv/zig/zig && test "$(.direnv/zig/zig version)" = "$version"; then
  echo "zig $version already in .direnv/zig"
  exit 0
fi
mkdir -p .direnv
tmp=$(mktemp -d "$PWD/.direnv/zig-download.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
curl --fail --location --proto '=https' --tlsv1.2 "https://ziglang.org/download/$version/$name.tar.xz" -o "$tmp/$name.tar.xz"
echo "$sha256  $tmp/$name.tar.xz" | sha256sum --check --quiet
tar -C "$tmp" -xJf "$tmp/$name.tar.xz"
rm -rf .direnv/zig
mv "$tmp/$name" .direnv/zig
.direnv/zig/zig version
