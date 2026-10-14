#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
direnv exec . cargo fmt --all -- --check
direnv exec . rustfmt --edition 2024 --check tests/support/*.rs
direnv exec . cargo clippy --locked --all-targets -- -D warnings
direnv exec . cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
direnv exec . cargo build --locked --release
