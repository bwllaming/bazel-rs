#!/usr/bin/env python3
"""Genuine Bazel 9.3.1's parsing of --experimental_remote_repo_contents_cache.

usage: capture.py BAZEL_9_3_1 SCRATCH_DIR > reference_receipt.json
Runs genuine (--batch, private output user root, a scratch workspace, no
network) and records exit codes and error lines.
"""
import json, os, subprocess, sys

bazel, scratch = sys.argv[1], os.path.abspath(sys.argv[2])
ws = os.path.join(scratch, "ws")
os.makedirs(ws, exist_ok=True)
open(os.path.join(ws, "MODULE.bazel"), "w").write('module(name = "rrc")\n')
open(os.path.join(ws, "BUILD.bazel"), "w").write("")
rc = os.path.join(scratch, "rc")
open(rc, "w").write("startup --experimental_remote_repo_contents_cache\n")
base = [bazel, "--batch", "--output_user_root=" + os.path.join(scratch, "root")]
cases = {
    "startup": ["--ignore_all_rc_files", "--experimental_remote_repo_contents_cache", "build", "--nobuild", "//..."],
    "startup_negated": ["--ignore_all_rc_files", "--noexperimental_remote_repo_contents_cache", "build", "--nobuild", "//..."],
    "startup_value": ["--ignore_all_rc_files", "--experimental_remote_repo_contents_cache=yes", "build", "--nobuild", "//..."],
    "command_option": ["--ignore_all_rc_files", "build", "--experimental_remote_repo_contents_cache", "//..."],
    "rc_startup_line": ["--nosystem_rc", "--nohome_rc", "--noworkspace_rc", "--bazelrc=" + rc, "build", "--nobuild", "//..."],
}
receipt = {"bazel": "9.3.1", "cases": {}}
for name, args in cases.items():
    env = {"PATH": "/usr/bin:/bin", "HOME": os.path.join(scratch, "home")}
    p = subprocess.run(base + args, cwd=ws, env=env, capture_output=True, text=True)
    lines = [l for l in p.stderr.splitlines()
             if "ERROR" in l or "does not take a value" in l or "Build completed" in l]
    receipt["cases"][name] = {"args": args, "exit_code": p.returncode, "lines": lines}
json.dump(receipt, sys.stdout, indent=1, sort_keys=True)
print()
