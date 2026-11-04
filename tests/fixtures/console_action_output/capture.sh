#!/bin/bash
# The genuine captures of this fixture (genuine/NN.txt): Bazel 9.3.1 in
# --batch mode (a cold server per command, the fair comparison for a
# serverless tool) with a fresh --output_user_root, `clean` before each
# command, on a copy of workspace/.
#   capture.sh SCRATCH_DIR [BAZEL_9.3.1_BINARY]
# Each file: the command, `rc=N`, then `--- stderr` and `--- stdout` with
# the workspace path as <WS> and the output base as <OB>.
set -u
F=$(cd "$(dirname "$0")" && pwd)
W=${1:?scratch directory}
B=${2:-$F/../../../.direnv/reference/bazel-9.3.1-linux-x86_64}
rm -rf $W; mkdir -p $W/home $W/tmp; cp -r $F/workspace $W/ws; cd $W/ws
G() { env -i PATH=/usr/bin:/bin HOME=$W/home TMPDIR=$W/tmp LANG=C.UTF-8 \
  $B --batch --ignore_all_rc_files --output_user_root=$W/our "$@"; }
rm -f $F/genuine/*.txt
n=0
while IFS= read -r args; do
  n=$((n + 1)); name=$(printf '%02d' $n)
  G clean >/dev/null 2>&1
  G $args --curses=no --color=no >$W/o 2>$W/e
  rc=$?
  ob=$(G info output_base 2>/dev/null)
  {
    echo "### $args"; echo "rc=$rc"; echo "--- stderr"
    sed -e "s#$ob#<OB>#g" -e "s#$W/ws#<WS>#g" $W/e
    echo "--- stdout"
    sed -e "s#$ob#<OB>#g" -e "s#$W/ws#<WS>#g" $W/o
  } > $F/genuine/$name.txt
done <<'ARGS'
build //ok:g
build //ok:gm
build //ok:t
build //ok:n
build //ok:partial
build //ok:ut
build //ok:usetool
build --output_filter=^//nothing //ok:g
build --experimental_ui_max_stdouterr_bytes=5 //ok:t
build //fail:exit3
build --verbose_failures //fail:exit3
build //fail:signal
build //fail:noout
build --verbose_failures //fail:sfail
build -k //fail:exit3 //ok:quiet
build -s //ok:quiet
build --subcommands=pretty_print //ok:quiet
ARGS
chmod -R u+w $W; rm -rf $W
