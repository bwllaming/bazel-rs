#!/bin/bash
# Regenerate genuine/<case>.{out,rc,stderr} from cases.tsv with genuine Bazel 9.3.1.
# usage: capture.sh <bazel-9.3.1 ELF> <scratch dir under /tmp/opencode/w>
set -eu
bazel=$1 scratch=$2
here=$(cd "$(dirname "$0")" && pwd)
rm -rf "$scratch/ws"; mkdir -p "$scratch/home" "$scratch/tmp"; cp -a "$here/workspace" "$scratch/ws"
cd "$scratch/ws"
while IFS=$'\t' read -r -a fields; do
  name=${fields[0]}
  set +e
  HOME=$scratch/home TMPDIR=$scratch/tmp PATH=/usr/local/bin:/usr/bin:/bin nice -n 10 "$bazel" \
    --ignore_all_rc_files --batch --output_user_root="$scratch/userroot" cquery \
    --incompatible_autoload_externally= --lockfile_mode=off --repository_disable_download \
    --platforms=//:linux --host_platform=//:linux "${fields[@]:1}" \
    > "$here/genuine/$name.out" 2> "$scratch/$name.err"
  echo $? > "$here/genuine/$name.rc"
  set -e
  grep -h "^ERROR\|^Error\|^	File\|^Traceback" "$scratch/$name.err" | grep -v "Build did NOT" > "$here/genuine/$name.stderr" || true
done < "$here/cases.tsv"
