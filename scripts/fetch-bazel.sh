#!/usr/bin/env bash
# Download the independent reference binary, never used as a native fallback.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(cat .bazelversion)
case "$(uname -sm)" in
  'Linux x86_64') platform=linux-x86_64 ;;
  'Linux aarch64') platform=linux-arm64 ;;
  *) echo 'Reference bootstrap currently supports Linux only.' >&2; exit 1 ;;
esac
dest="$PWD/.direnv/reference"
mkdir -p "$dest"
name="bazel-$version-$platform"
tmp=$(mktemp -d "$dest/download.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
url="https://releases.bazel.build/$version/release/$name"
curl --fail --location --proto '=https' --tlsv1.2 "$url.sha256" -o "$tmp/$name.sha256"
if test -f "$dest/$name" && (cd "$dest" && sha256sum --check "$tmp/$name.sha256" >&2); then
  printf '%s\n' "$dest/$name"
  exit 0
fi
curl --fail --location --proto '=https' --tlsv1.2 "$url" -o "$tmp/$name"
(cd "$tmp" && sha256sum --check "$name.sha256" >&2)
chmod +x "$tmp/$name"
mv "$tmp/$name" "$dest/$name"
printf '%s\n' "$dest/$name"
