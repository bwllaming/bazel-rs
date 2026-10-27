#!/bin/bash
# The genuine capture of this fixture (genuine.txt): one Bazel 9.3.0 server
# with a fresh output_user_root, on a copy of workspace/.
#   capture.sh SCRATCH_DIR [BAZEL_9.3.0_BINARY]
set -u
F=$(cd "$(dirname "$0")" && pwd)
W=${1:?scratch directory}
B=${2:-$F/../../../.direnv/reference/bazel-9.3.0-linux-x86_64}
rm -rf $W; mkdir -p $W; cp -r $F/workspace $W/ws; cd $W/ws
out=$F/genuine.txt; : > $out
while IFS= read -r args; do
  $B --output_user_root=$W/our --ignore_all_rc_files $args --curses=no --color=no >$W/o 2>$W/e
  rc=$?
  echo "### $args" >> $out; echo "rc=$rc" >> $out
  # result lines (paths only under an `up-to-date:` header), the summary
  awk '/^(Target |Aspect )/ {print; p = /up-to-date:$/; next}
       /^  / {if (p) print; next}
       {p = 0}
       /^(Use --verbose_failures|INFO: [0-9]+ process|INFO: Analyzed|INFO: Elapsed)/ {print}' $W/e \
    | sed -E 's/Elapsed time: [0-9.]+s, Critical Path: [0-9.]+s/Elapsed time: <T>s, Critical Path: <T>s/' >> $out
done <<'ARGS'
build //:a
build //:a //:b
build //:a //:b //sub:c //:empty //:src //:b.txt --show_result=10
build //:a.txt //:a
build --nobuild //:b
build -k //:fail //:a
build -k //:fail //:a --show_result=5
build -k //:fail //:a --show_result=5 --verbose_failures
build //:tool --aspects=//:defs.bzl%asp --output_groups=+asp_out --show_result=5
build //:tool --aspects=//:defs.bzl%asp --output_groups=asp_out --show_result=5
test //:t
run //:tool
ARGS
$B --output_user_root=$W/our shutdown 2>/dev/null
