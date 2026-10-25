#!/bin/bash
# What a spawn observes about its temporary directory (tests/sandbox.rs,
# tests/fixtures/action_tmpdir/capture.py).
echo "TMPDIR=${TMPDIR-<unset>}"
if [ -n "$TMPDIR" ] && [ -d "$TMPDIR" ]; then echo "TMPDIR_dir=yes"; else echo "TMPDIR_dir=no"; fi
if [ -e /tmp/action-tmpdir-host-marker ]; then echo "host_tmp_visible=yes"; else echo "host_tmp_visible=no"; fi
if [ -e ../tmp ]; then echo "cwd_parent_tmp=yes"; else echo "cwd_parent_tmp=no"; fi
# A Unix socket below TMPDIR, the way tsx names its IPC pipe
# (<TMPDIR>/tsx-<uid>/<pid>.pipe); sun_path holds ~108 bytes.
python3 - <<'PY'
import os, socket
d = os.path.join(os.environ.get("TMPDIR", "/tmp"), "tsx-%d" % os.getuid())
os.makedirs(d, exist_ok=True)
p = os.path.join(d, "%d.pipe" % os.getpid())
s = socket.socket(socket.AF_UNIX)
try:
    s.bind(p)
    print("socket_bind=ok")
except OSError as e:
    print("socket_bind=error:%s" % e.strerror)
finally:
    s.close()
    if os.path.exists(p):
        os.unlink(p)
PY
