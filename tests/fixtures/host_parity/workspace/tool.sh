#!/bin/bash
set -eu
echo "tool: $(basename "$0")"
runfiles="$0.runfiles"
echo "data: $(cat "$runfiles/_main/data.txt")"
echo "lib_data: $(cat "$runfiles/_main/lib_data.txt")"
(cd "$runfiles" && find . | LC_ALL=C sort)
