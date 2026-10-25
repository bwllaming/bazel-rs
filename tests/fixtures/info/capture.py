"""Capture genuine Bazel 9.3.0 `info` outputs for tests/info_command.rs.

Scratch only: a private workspace and --output_user_root under /tmp/opencode.
Run: python3 tests/fixtures/info/capture.py (writes genuine_capture.json here).
"""
import hashlib
import json
import os
import subprocess
GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64"
WS = "/tmp/opencode/info-scratch/ws"
OUR = "/tmp/opencode/info-scratch/our"
HOME = "/tmp/opencode/info-scratch/home"
TARGET = os.path.join(os.path.dirname(os.path.abspath(__file__)), "genuine_capture.json")
os.makedirs(WS + "/pkg", exist_ok=True)
with open(WS + "/MODULE.bazel", "w") as f:
    f.write('module(name="scratch")\n')
with open(WS + "/pkg/BUILD", "w") as f:
    f.write('genrule(name="g", outs=["g.txt"], cmd="echo hi > $@")\n')
cases = [
    ("all", []),
    ("single", ["bazel-bin"]),
    ("single_opt", ["bazel-bin", "-c", "opt"]),
    ("multi_dbg", ["--compilation_mode=dbg", "bazel-bin", "bazel-testlogs"]),
    ("cpu", ["--cpu=foo", "bazel-bin"]),
    ("repeated", ["workspace", "workspace"]),
    ("unknown", ["workspace", "bazel-binn", "foo", "foo"]),
    ("make_env_unknown", ["COMPILATION_MODE"]),
    ("show_make_env", ["--show_make_env"]),
    ("show_make_env_keys", ["--show_make_env", "--define", "FOO=bar", "--define=ABC=1", "-c", "opt", "COMPILATION_MODE", "FOO", "ABC", "TARGET_CPU", "BINDIR", "GENDIR"]),
    ("hidden", ["install_md5", "starlark-semantics", "default-package-path", "defaults-package", "client-env", "worker_metrics", "peak-heap-size"]),
    ("starlark_semantics", ["starlark-semantics", "--experimental_google_legacy_api", "--incompatible_disable_starlark_host_transitions"]),
    ("client_env", ["client-env", "--action_env=PATH", "--action_env=FOO=bar", "--test_env=HOME"]),
]
OUTPUT_BASE = OUR + "/" + hashlib.md5(WS.encode()).hexdigest()
def normalize(text):
    text = text.replace(OUTPUT_BASE, "<OUTPUT_BASE>").replace(HOME, "<HOME>")
    text = text.replace(WS, "<WORKSPACE>").replace(OUR, "<OUTPUT_USER_ROOT>")
    return text
out = {
    "about": "Genuine Bazel 9.3.0 `info` captures (tests/fixtures/info/capture.py, docs/INFO.md): --batch --ignore_all_rc_files --output_user_root=<OUTPUT_USER_ROOT>, env -i PATH=/usr/bin:/bin HOME=<private>, workspace = MODULE.bazel module(name='scratch') + pkg/BUILD genrule. Paths are normalized; stderr keeps only ERROR lines.",
    "genuine_sha256": hashlib.sha256(open(GENUINE, "rb").read()).hexdigest(),
    "cases": {},
}
for name, args in cases:
    p = subprocess.run(["env", "-i", "PATH=/usr/bin:/bin", "HOME=" + HOME, GENUINE, "--batch", "--ignore_all_rc_files", "--output_user_root=" + OUR, "info"] + args, cwd=WS, capture_output=True, text=True)
    out["cases"][name] = {
        "args": args,
        "exit": p.returncode,
        "stdout": normalize(p.stdout),
        "stderr": [normalize(l) for l in p.stderr.splitlines() if l.startswith("ERROR")],
    }
json.dump(out, open(TARGET, "w"), indent=2)
open(TARGET, "a").write("\n")
