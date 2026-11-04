#!/bin/sh
# Genuine 9.3.1 captures of tests/fixtures/aquery/test_runner (reference_receipt.json).
# usage: capture.sh BAZEL OUTPUT_USER_ROOT
set -e
B=$1; ROOT=$2; HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/ws"
while IFS='	' read -r name args; do
  [ -z "$name" ] && continue
  eval "set -- $args"
  "$B" --batch --ignore_all_rc_files --output_user_root="$ROOT" aquery "$@" \
    >"$HERE/genuine/$name" 2>"$HERE/genuine/$name.err" && echo 0 >"$HERE/genuine/$name.exit" || echo $? >"$HERE/genuine/$name.exit"
  sed -i -n '/^ERROR/p' "$HERE/genuine/$name.err"
done <"$HERE/cases.tsv"
find . -maxdepth 1 -name "bazel-*" -type l -delete
