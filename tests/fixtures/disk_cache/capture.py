"""Paired capture for tests/disk_cache.rs (docs/REPOSITORY_CACHE.md, "--disk_cache").

Genuine Bazel 9.3.1 and bazel-rs share one `--disk_cache` on workspace/ (a
genrule and a run_shell action, the N41 remote cache probe):
  1. genuine, disk cache G (empty): executes, writes G (copied to genuine/).
  2. bazel-rs, fresh cache, G: must hit both actions (`2 disk cache hit`).
  3. bazel-rs, fresh cache, disk cache N (empty): executes, writes N.
  4. genuine, fresh output root, N: must hit both actions.
The receipt records each step's `INFO: N processes: …` line and the file
lists of G and N.

Scratch only: --batch --ignore_all_rc_files, private output roots, HOME and
TMPDIR under /tmp/opencode/w; the offline builtin-module registry.
Run: python3 tests/fixtures/disk_cache/capture.py BAZEL_RS_BINARY
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.1-linux-x86_64"
BUILTIN = os.path.join(REPO, "tests/fixtures/builtin_modules/registry")
SCRATCH = "/tmp/opencode/w/disk-cache-capture"
NATIVE = os.path.abspath(sys.argv[1])

shutil.rmtree(SCRATCH, ignore_errors=True)
for name in ("home", "tmp"):
    os.makedirs(os.path.join(SCRATCH, name))
ENV = {"PATH": "/usr/bin:/bin", "HOME": os.path.join(SCRATCH, "home"),
       "TMPDIR": os.path.join(SCRATCH, "tmp"), "LC_ALL": "C.UTF-8"}
COMMON = ["build", "//pkg:all", "--registry=file://" + BUILTIN]


def run(engine, ws, disk, store):
    path = os.path.join(SCRATCH, ws)
    if not os.path.exists(path):
        shutil.copytree(os.path.join(HERE, "workspace"), path)
    if engine == "genuine":
        argv = [GENUINE, "--batch", "--ignore_all_rc_files",
                "--output_user_root=" + os.path.join(SCRATCH, store),
                "--host_jvm_args=-Xmx768m"] + COMMON + [
                "--disk_cache=" + os.path.join(SCRATCH, disk),
                "--noshow_progress", "--color=no", "--curses=no"]
    else:
        argv = [NATIVE, "--ignore_all_rc_files", "--cache-dir=" + os.path.join(SCRATCH, store)
                ] + COMMON + ["--disk_cache=" + os.path.join(SCRATCH, disk)]
    env = dict(ENV, BAZEL_RS_RESULT_LINES="1") if engine == "native" else ENV
    p = subprocess.run(["nice", "-n", "10"] + argv, cwd=path, env=env,
                       capture_output=True, text=True, timeout=900)
    if p.returncode != 0:
        raise SystemExit(f"{engine} exit {p.returncode}\n{p.stderr}")
    lines = [line for line in p.stderr.splitlines() if " processes: " in line]
    if engine == "native":
        paths = [line.split("\t")[1] for line in p.stdout.splitlines()]
    else:
        paths = [os.path.join(path, "bazel-bin/pkg", name) for name in ("gen.txt", "sh.out")]
    outputs = {os.path.basename(f): open(f).read() for f in paths}
    return {"processes": lines, "outputs": outputs}


def files(disk):
    root = os.path.join(SCRATCH, disk)
    return sorted(os.path.relpath(os.path.join(d, f), root)
                  for d, _, names in os.walk(root) for f in names)


steps = {"1_genuine_cold": run("genuine", "ws_genuine", "G", "our1")}
genuine_files = files("G")
copy = os.path.join(HERE, "genuine")
shutil.rmtree(copy, ignore_errors=True)
for name in genuine_files:
    os.makedirs(os.path.dirname(os.path.join(copy, name)), exist_ok=True)
    shutil.copyfile(os.path.join(SCRATCH, "G", name), os.path.join(copy, name))
steps["2_native_fresh_cache_on_genuine_written"] = run("native", "ws_native", "G", "nc1")
steps["3_native_cold"] = run("native", "ws_native2", "N", "nc2")
native_files = files("N")
steps["4_genuine_fresh_output_root_on_native_written"] = run("genuine", "ws_genuine2", "N", "our2")
receipt = {
    "about": __doc__.strip().splitlines()[0] + " Steps: see capture.py; genuine/ is step 1's "
             "disk cache.",
    "binary": os.path.basename(GENUINE),
    "sha256": hashlib.sha256(open(GENUINE, "rb").read()).hexdigest(),
    "steps": steps,
    "genuine_files": genuine_files,
    "native_files": native_files,
}
shutil.rmtree(SCRATCH, ignore_errors=True)
with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
    json.dump(receipt, out, indent=1, sort_keys=True)
    out.write("\n")
print(json.dumps(receipt, indent=1, sort_keys=True))
