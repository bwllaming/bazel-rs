#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export RUSTUP_HOME="$PWD/.direnv/rustup"
export CARGO_HOME="$PWD/.direnv/cargo"
export PATH="$CARGO_HOME/bin:$PATH"

if ! test -x "$CARGO_HOME/bin/rustup"; then
  case "$(uname -sm)" in
    'Linux x86_64') host=x86_64-unknown-linux-gnu ;;
    'Linux aarch64') host=aarch64-unknown-linux-gnu ;;
    *) echo 'Bootstrap currently supports Linux x86_64 and aarch64.' >&2; exit 1 ;;
  esac
  mkdir -p .direnv
  tmp=$(mktemp -d "$PWD/.direnv/rustup-download.XXXXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  url="https://static.rust-lang.org/rustup/dist/$host/rustup-init"
  curl --fail --location --proto '=https' --tlsv1.2 "$url" -o "$tmp/rustup-init"
  curl --fail --location --proto '=https' --tlsv1.2 "$url.sha256" -o "$tmp/rustup-init.sha256"
  (cd "$tmp" && sha256sum --check rustup-init.sha256)
  chmod +x "$tmp/rustup-init"
  "$tmp/rustup-init" -y --no-modify-path --default-toolchain none
fi
# rustup reads the pinned rust-toolchain.toml and installs its components.
rustup toolchain install --no-self-update
rustup show
rustc --version
cargo --version
printf '\nToolchain ready. Run: direnv allow\n'
