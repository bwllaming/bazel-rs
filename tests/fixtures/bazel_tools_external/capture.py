"""Capture genuine Bazel 9.3.1's `external/bazel_tools` for tests/bazel_tools_external.rs.

~/code's .bazelrc has `build --repo_env=JAVA_HOME=../bazel_tools/jdk`: relative to a
repository's directory `<output_base>/external/<name>`, genuine resolves it to
`<output_base>/external/bazel_tools/jdk`, the JDK of the embedded @bazel_tools (the
link `external/bazel_tools -> <install_base>/embedded_tools`). workspace/probe.bzl
runs that JDK as rules_jvm_external's coursier finds it (`probe`) and as rules_java's
local_java_repository links it (`jdk_like`); neither loads anything from @bazel_tools.

Scratch only: a private copy of workspace/, --batch --ignore_all_rc_files, private
output root, HOME and TMPDIR under /tmp/opencode/w, the offline builtin-module
registry, --repository_disable_download.
Run: python3 tests/fixtures/bazel_tools_external/capture.py (writes reference_receipt.json).
"""
import hashlib
import json
import os
import shutil
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
GENUINE = "/home/coder/bazel/.direnv/reference/bazel-9.3.1-linux-x86_64"
REGISTRY = os.path.join(REPO, "tests/fixtures/builtin_modules/registry")
SCRATCH = "/tmp/opencode/w/bazel-tools-external-capture"
QUERY = "@probe//:all + @jdk//:all"
JAVA_HOME = "--repo_env=JAVA_HOME=../bazel_tools/jdk"

shutil.rmtree(SCRATCH, ignore_errors=True)
ws = os.path.join(SCRATCH, "ws")
shutil.copytree(os.path.join(HERE, "workspace"), ws)
for name in ("home", "tmp"):
    os.makedirs(os.path.join(SCRATCH, name))
our = os.path.join(SCRATCH, "our")
env = {"PATH": "/usr/bin:/bin", "HOME": os.path.join(SCRATCH, "home"),
       "TMPDIR": os.path.join(SCRATCH, "tmp"), "LC_ALL": "C.UTF-8"}
argv = [GENUINE, "--batch", "--ignore_all_rc_files", "--output_user_root=" + our,
        "--host_jvm_args=-Xmx768m", "query", QUERY, JAVA_HOME,
        "--registry=file://" + REGISTRY, "--repository_cache=" + os.path.join(SCRATCH, "rc"),
        "--repository_disable_download", "--noshow_progress", "--color=no", "--curses=no"]
p = subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True, timeout=900)
if p.returncode != 0:
    raise SystemExit(f"exit {p.returncode}\n{p.stderr}")
output_base = os.path.join(our, hashlib.md5(ws.encode()).hexdigest())
external = os.path.join(output_base, "external")
link = os.path.join(external, "bazel_tools")
java = os.path.join(link, "jdk/bin/java")
receipt = {
    "about": "Genuine Bazel 9.3.1 (capture.py): `query '" + QUERY + "' " + JAVA_HOME + "` on "
             "workspace/ with --registry=file://<builtin_modules registry> "
             "--repository_disable_download, --batch --ignore_all_rc_files, private output root, "
             "env -i PATH=/usr/bin:/bin HOME=<private> TMPDIR=<private> LC_ALL=C.UTF-8. "
             "results: each repository's result.txt (<EXTERNAL> is <output_base>/external). "
             "layout: what <output_base>/external/bazel_tools is.",
    "binary": os.path.basename(GENUINE),
    "sha256": hashlib.sha256(open(GENUINE, "rb").read()).hexdigest(),
    "stdout": p.stdout,
    "results": {
        name: open(os.path.join(external, canonical, "result.txt")).read()
        for name, canonical in (("probe", "+probe+probe"), ("jdk", "+jdk_like+jdk"))
    },
    "layout": {
        "bazel_tools_is_symlink": os.path.islink(link),
        "bazel_tools_link_basename": os.path.basename(os.readlink(link)),
        "bazel_tools_marker": os.path.exists(os.path.join(external, "@bazel_tools.marker")),
        "java_mode": oct(os.stat(java).st_mode & 0o7777),
        "jdk_bin_link": os.readlink(os.path.join(external, "+jdk_like+jdk", "bin"))
        .replace(external, "<EXTERNAL>"),
    },
}
shutil.rmtree(SCRATCH, ignore_errors=True)
with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
    json.dump(receipt, out, indent=1, sort_keys=True)
    out.write("\n")
