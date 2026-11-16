#!/bin/bash
# The genuine captures of this fixture (genuine/NN.txt): Bazel 9.3.1 in
# --batch mode with a fresh --output_user_root, `clean` before each command,
# on a copy of workspace/, stderr into a file (no terminal: Bazel's
# non-curses progress lines).
#   capture.sh SCRATCH_DIR [BAZEL_9.3.1_BINARY]
# Each file: the command, `rc=N`, then the stderr with the workspace path as
# <WS>, the output base as <OB>, and each line prefixed by the seconds since
# the command started (`  1.23 | `).
set -u
F=$(cd "$(dirname "$0")" && pwd)
W=${1:?scratch directory}
B=${2:-$F/../../../.direnv/reference/bazel-9.3.1-linux-x86_64}
rm -rf $W; mkdir -p $W/home $W/tmp; cp -r $F/workspace $W/ws; cd $W/ws
G() { env -i PATH=/usr/bin:/bin HOME=$W/home TMPDIR=$W/tmp LANG=C.UTF-8 \
  $B --batch --ignore_all_rc_files --output_user_root=$W/our "$@"; }
# The times: a reader stamping each stderr line as it arrives.
stamp() {
  python3 -c '
import sys, time
t0 = time.time()
for line in sys.stdin.buffer:
    sys.stdout.write("%6.2f | %s\n" % (time.time() - t0, line.decode(errors="replace").rstrip("\n")))
'
}
rm -f $F/genuine/*.txt
G version >/dev/null 2>&1
n=0
while IFS= read -r args; do
  n=$((n + 1)); name=$(printf '%02d' $n)
  G clean >/dev/null 2>&1
  { G $args 2>&1 >/dev/null; echo "rc=$?" >&2; } 2>$W/rc | stamp >$W/e
  ob=$(G info output_base 2>/dev/null)
  {
    echo "### $args"; cat $W/rc
    sed -e "s#$ob#<OB>#g" -e "s#$W/ws#<WS>#g" $W/e
  } > $F/genuine/$name.txt
done <<'ARGS'
build //:all
test //:t1 //:t2 //:t3
build //:long
build //:all --noshow_progress
build //:all --show_progress_rate_limit=3
ARGS
chmod -R u+w $W; rm -rf $W
