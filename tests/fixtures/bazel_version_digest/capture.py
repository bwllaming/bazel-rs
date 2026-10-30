"""Capture genuine Bazel MODULE.bazel.lock files for tests/bazel_version_digest.rs.

An extension that loads a .bzl of a repository whose bytes hold
native.bazel_version (bazel_features' @bazel_features_version//:version.bzl
shape) records a bzlTransitiveDigest that depends on the running release.
Genuine 9.3.1 (the compatibility target) and, for contrast, 9.3.0 are run on
a private copy of workspace/ with the offline builtin-module registry.
Scratch only: --batch --ignore_all_rc_files, private output roots, HOME and
TMPDIR under /tmp/opencode/w. Run: python3 tests/fixtures/bazel_version_digest/capture.py
"""
import hashlib
import json
import os
import shutil
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
REFERENCES = {
    "9.3.1": "/home/coder/bazel/.direnv/reference/bazel-9.3.1-linux-x86_64",
    "9.3.0": "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64",
}
REGISTRY = os.path.join(REPO, "tests/fixtures/builtin_modules/registry")
SCRATCH = "/tmp/opencode/w/bazel-version-digest-capture"
QUERY = "@probe//:all"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


shutil.rmtree(SCRATCH, ignore_errors=True)
receipt = {
    "about": "Genuine Bazel lockfiles of tests/fixtures/bazel_version_digest/workspace "
             "(capture.py): `query @probe//:all --lockfile_mode=update` with "
             "--registry=file://<builtin_modules registry> --repository_disable_download, "
             "--batch --ignore_all_rc_files, private output root, env -i PATH=/usr/bin:/bin "
             "HOME=<private> TMPDIR=<private> LC_ALL=C.UTF-8. The 9.3.1 lockfile is the "
             "expected native output; the 9.3.0 one differs only in the bzlTransitiveDigest "
             "of //:ext.bzl%ext.",
    "references": {},
}
for version, binary in REFERENCES.items():
    root = os.path.join(SCRATCH, version)
    ws = os.path.join(root, "ws")
    shutil.copytree(os.path.join(HERE, "workspace"), ws)
    for name in ("home", "tmp"):
        os.makedirs(os.path.join(root, name))
    env = {"PATH": "/usr/bin:/bin", "HOME": os.path.join(root, "home"),
           "TMPDIR": os.path.join(root, "tmp"), "LC_ALL": "C.UTF-8"}
    argv = [binary, "--batch", "--ignore_all_rc_files", "--output_user_root=" + os.path.join(root, "our"),
            "--host_jvm_args=-Xmx768m", "query", QUERY, "--registry=file://" + REGISTRY,
            "--lockfile_mode=update", "--repository_cache=" + os.path.join(root, "rc"),
            "--repository_disable_download", "--noshow_progress", "--color=no", "--curses=no"]
    p = subprocess.run(argv, cwd=ws, env=env, capture_output=True, timeout=900)
    if p.returncode != 0:
        raise SystemExit(f"{version}: exit {p.returncode}\n{p.stderr.decode()}")
    with open(os.path.join(binary), "rb") as f:
        binary_sha = sha256(f.read())
    lock = open(os.path.join(ws, "MODULE.bazel.lock"), "rb").read()
    receipt["references"][version] = {
        "binary": os.path.basename(binary), "sha256": binary_sha,
        "stdout": p.stdout.decode(), "lockfile": lock.decode(), "lockfile_sha256": sha256(lock),
    }
shutil.rmtree(SCRATCH)
with open(os.path.join(HERE, "reference_receipt.json"), "w") as out:
    json.dump(receipt, out, indent=1, sort_keys=True)
    out.write("\n")
