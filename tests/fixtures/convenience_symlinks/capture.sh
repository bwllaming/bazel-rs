#!/bin/bash
# The genuine capture of this fixture (genuine.txt; README.md): Bazel 9.3.1 in batch mode
# with one fresh output_user_root, the steps below in order on a copy of
# workspace/ (named `ws`).
#   capture.sh SCRATCH_DIR [BAZEL_9.3.1_BINARY]
# Steps: `!CMD` runs a shell command in the workspace, `@DIR ARGS` runs Bazel
# in the workspace subdirectory DIR, anything else runs Bazel with ARGS.
# Per Bazel step: the exit code, the stderr lines about convenience symlinks
# (WARNING), the results (`Target …` and their paths), `INFO: Running command
# line`, test log paths, the BEP ConvenienceSymlinksIdentified event when
# `--build_event_json_file=bep.json` is given; then the workspace's
# convenience-link-like entries (`ls`), with the output base as <OB> and the
# workspace as <WS>.
set -u
F=$(cd "$(dirname "$0")" && pwd)
W=${1:?scratch directory}
B=${2:-$F/../../../.direnv/reference/bazel-9.3.1-linux-x86_64}
rm -rf "$W"; mkdir -p "$W"; cp -r "$F/workspace" "$W/ws"; cd "$W/ws"
WS=$(pwd)
out=$F/genuine.txt; : > "$out"
norm() { sed -E -e "s#$W/our/[0-9a-f]{32}#<OB>#g" -e "s#$WS#<WS>#g"; }
listing() {
  find . -maxdepth 2 \( -name 'bazel-*' -o -name 'foo-*' -o -path './out*' -o -name 'bin' -o -name 'testlogs' \) \
       -not -path './pkg*' -not -path './bazel-*/*' -not -path './foo-*/*' | sort | while read -r e; do
    if [ -L "$e" ]; then echo "  $e -> $(readlink "$e")"
    elif [ -d "$e" ]; then echo "  $e/ (dir)"
    else echo "  $e (file)"; fi
  done | norm
}
while IFS= read -r step; do
  [ -z "$step" ] && continue
  echo "### $step" >> "$out"
  case "$step" in
    '!'*) (eval "${step#!}") >> "$out" 2>&1; continue ;;
  esac
  dir=.; args=$step
  case "$step" in '@'*) dir=${step%% *}; dir=${dir#@}; args=${step#* } ;; esac
  rm -f "$W/bep.json"
  (cd "$dir" && $B --batch --output_user_root="$W/our" --ignore_all_rc_files ${args//bep.json/$W\/bep.json} --curses=no --color=no >"$W/o" 2>"$W/e")
  echo "rc=$?" >> "$out"
  awk '/^(Target |Aspect )/ {print; p = /up-to-date:$/; next}
       /^WARNING: failed to (create|remove)/ {print; p = 1; next}
       /^\/\/:t/ {print; p = 1; next}
       /^  / {if (p) print; next}
       {p = 0}
       /WARNING|convenience|^INFO: Running command line|test\.log|^ERROR: Build did NOT|^ERROR: Analysis/ {print}' "$W/e" "$W/o" \
    | grep -v 'Xverify' | norm >> "$out"
  if [ -f "$W/bep.json" ]; then
    python3 - "$W/bep.json" <<'PY' | norm >> "$out"
import json, sys
for line in open(sys.argv[1]):
    ev = json.loads(line)
    if "convenienceSymlinksIdentified" in ev.get("id", {}):
        print("  BEP", json.dumps(ev.get("convenienceSymlinksIdentified", {}), sort_keys=True))
PY
  fi
  echo "links:" >> "$out"; listing >> "$out"
done <<'STEPS'
build //:a
build //:a //pkg:p
@pkg build //:a
build //:tree
build //:a --symlink_prefix=foo-
build //:a --symlink_prefix=out/x-
build //:a --symlink_prefix=/
!rm -rf foo-* out
build //:a --experimental_convenience_symlinks=log_only
build //:a --experimental_convenience_symlinks=ignore
build //:a --experimental_convenience_symlinks=clean
build //:a --experimental_convenience_symlinks=log_only
build //:a --noexperimental_convenience_symlinks
build //:a --experimental_convenience_symlinks
!ln -sfn /tmp/elsewhere-bin bazel-bin && ln -sfn ../relative bazel-testlogs
build //:a
!rm bazel-bin && mkdir bazel-bin && echo keep > bazel-bin/file && rm bazel-out && echo keep > bazel-out
build //:a
!rm -rf bazel-bin bazel-out
build //:a --noincompatible_skip_genfiles_symlink
build //:a
build //:opt
build //:opt //:a
build //:opt //:dbg
build //:a
build //:fail
build //:opt
build //:fail --keep_going
!rm -f bazel-*
build //:nonexistent
build //:bad
build //:opt //:bad --keep_going
build --nobuild //:opt
!rm -f bazel-*
build --nobuild //:a
cquery //:a
aquery //:a
query //:a
fetch //:a
info bazel-bin
test //:t
test //:t --print_relative_test_log_paths
test //:t_fail
test //:t_fail --print_relative_test_log_paths
@pkg test //:t_fail --print_relative_test_log_paths
run //:tool
build //:opt
coverage //:t
build //:a --build_event_json_file=bep.json
build //:a --build_event_json_file=bep.json --symlink_prefix=foo-
build //:a --build_event_json_file=bep.json --experimental_convenience_symlinks=clean
build //:a --build_event_json_file=bep.json --experimental_convenience_symlinks=log_only
build //:a --build_event_json_file=bep.json --experimental_convenience_symlinks=ignore
build //:opt //:dbg --build_event_json_file=bep.json
build //:a --build_event_json_file=bep.json --noexperimental_convenience_symlinks_bep_event
build //:a --build_event_json_file=bep.json --symlink_prefix=/
build //:a
build //:a --symlink_prefix=foo-
clean --symlink_prefix=foo-
build //:a --symlink_prefix=out/x-
clean --symlink_prefix=out/x-
!rm -rf out; mkdir bazel-genfiles
clean
!rmdir bazel-genfiles
build //:a
clean --expunge
STEPS
