#!/usr/bin/env bash
# rules_nodejs's bin/node wrapper: follow this script's own symlinks, then
# run the sibling installation (tests/fixtures/runfiles_repo_links).
SOURCE="${BASH_SOURCE[0]}"
while [ -h "$SOURCE" ]; do
  DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null && pwd)"
  SOURCE="$(readlink "$SOURCE")"
  [[ $SOURCE != /* ]] && SOURCE="$DIR/$SOURCE"
done
SCRIPT_DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null && pwd)"
exec "$SCRIPT_DIR/real/tool" "$@"
