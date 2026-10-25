"""Capture genuine Bazel 9.3.0's repository directory for tests/repository_slots.rs.

Scratch only: a private workspace, HOME and --output_user_root under /tmp/opencode.
Run: python3 tests/fixtures/repository_slots/capture.py (writes genuine_capture.json here).

Workspace: tests/fixtures/repository_slots/workspace (copied). Its repository rules
record what a rule sees as its directory: `probe` the last two segments of
rctx.path(".") and the whole path; `cgo_like` derives `${SRCDIR}` exactly as
gazelle's go_repository mode does (language/go/generate.go:130-144: `external/<name>`
when the parent directory is `external`, else `../<name>`) and a genrule checks
`test -f <srcdir>/libfoo.a` from the execution root, as the duckdb Go bindings'
`-L${SRCDIR}/` link flag needs.
"""
import hashlib
import json
import os
import shutil
import subprocess

GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
HERE = os.path.dirname(os.path.abspath(__file__))
SCRATCH = "/tmp/opencode/repository-slots-capture"
WS = SCRATCH + "/ws"
OUR = SCRATCH + "/our"
HOME = SCRATCH + "/home"
TARGET = os.path.join(HERE, "genuine_capture.json")

shutil.rmtree(SCRATCH, ignore_errors=True)
shutil.copytree(os.path.join(HERE, "workspace"), WS)
os.makedirs(HOME)
OUTPUT_BASE = OUR + "/" + hashlib.md5(WS.encode()).hexdigest()


def bazel(*args):
    return subprocess.run(
        ["env", "-i", "PATH=/usr/bin:/bin", "HOME=" + HOME, GENUINE, "--batch",
         "--ignore_all_rc_files", "--output_user_root=" + OUR] + list(args),
        cwd=WS, capture_output=True, text=True)


def normalize(text):
    return text.replace(OUTPUT_BASE, "<OUTPUT_BASE>").replace(WS, "<WORKSPACE>")


out = {
    "about": "Genuine Bazel 9.3.0 (tests/fixtures/repository_slots/capture.py): --batch "
             "--ignore_all_rc_files, env -i PATH=/usr/bin:/bin HOME=<private>, workspace "
             "tests/fixtures/repository_slots/workspace. Outputs are the built files' "
             "bytes; <OUTPUT_BASE> is the output base.",
    "genuine_sha256": hashlib.sha256(open(GENUINE, "rb").read()).hexdigest(),
    "cases": {},
}
for name, target, output in [
    ("repopath", "@probed//:show", "bazel-bin/external/+probe+probed/show.txt"),
    ("repopath_full", "@probed//:full", "bazel-bin/external/+probe+probed/full.txt"),
    ("cgo_srcdir", "@cgo//:link_flags", "bazel-bin/external/+cgo_like+cgo/link_flags.txt"),
]:
    p = bazel("build", target)
    path = os.path.join(WS, output)
    out["cases"][name] = {
        "target": target,
        "exit": p.returncode,
        "output": normalize(open(path).read()) if p.returncode == 0 else None,
    }
    assert p.returncode == 0, p.stderr
bazel("shutdown")
subprocess.run(["chmod", "-R", "u+w", SCRATCH])
shutil.rmtree(SCRATCH, ignore_errors=True)
json.dump(out, open(TARGET, "w"), indent=2)
open(TARGET, "a").write("\n")
