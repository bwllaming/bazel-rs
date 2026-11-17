#!/usr/bin/env python3
"""Genuine (or native) capture of what a test's and a `bazel run` target's
runfiles are: for the probe's own executable and each runfile (a source, a
source in a subdirectory, a generated file, a tree artifact's file and an
actions.symlink alias of a source), whether the path is a symlink, its
`readlink` and its `realpath`, plus the runfiles directory and the working
directory. Two tests (one sandboxed, one tagged `no-sandbox`) and `run` of a
binary with the same runfiles.

Absolute paths are normalized: an execution root (genuine: the output base's
`execroot/_main` or a sandbox's `execroot/_main`; native: a test's
`<cache>/tests/work/<key>-<pid>/execroot` or an action entry's
`<cache>/actions/<key>/outputs`) becomes EXECROOT, the workspace WORKSPACE.

  capture.py --bazel <genuine bazel 9.3.1> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
KEYS = ("test_", "run_")


def normalize(text, workspace, base):
    base = re.escape(base)
    text = re.sub(base + r"/sandbox/[^/\s]+/\d+/execroot/_main", "EXECROOT", text)
    text = re.sub(base + r"/execroot/_main", "EXECROOT", text)
    text = re.sub(base + r"/tests/work/[0-9a-f]+-\d+/execroot", "EXECROOT", text)
    text = re.sub(base + r"/actions/[0-9a-f]+/outputs", "EXECROOT", text)
    return text.replace(workspace, "WORKSPACE")


def probe_lines(text):
    return [line for line in text.splitlines() if line.startswith(KEYS)]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    if os.path.exists(scratch):
        subprocess.run(["chmod", "-R", "u+w", scratch], check=True)
        shutil.rmtree(scratch)
    os.makedirs(scratch)
    ws = os.path.join(scratch, "ws")
    shutil.copytree(os.path.join(HERE, "workspace"), ws)
    ws = os.path.realpath(ws)
    home = os.path.join(scratch, "home")
    os.makedirs(home)
    env = {"PATH": "/usr/bin:/bin", "HOME": home}
    out = os.path.join(scratch, "out")
    if args.bazel:
        prefix = [args.bazel, "--nosystem_rc", "--nohome_rc", "--noworkspace_rc",
                  f"--output_user_root={out}", "--batch"]
        extra = [f"--repository_cache={scratch}/repocache"]
    else:
        prefix = [args.native, "--ignore_all_rc_files", f"--cache-dir={out}"]
        extra = []
    rows = {}
    test = subprocess.run(prefix + ["test"] + extra + ["--test_output=all", "//:all"],
                          cwd=ws, env=env, capture_output=True, text=True)
    if test.returncode != 0:
        sys.exit(f"test: exit {test.returncode}\n{test.stdout}{test.stderr}")
    run = subprocess.run(prefix + ["run"] + extra + ["//:probe"], cwd=ws, env=env,
                         capture_output=True, text=True)
    if run.returncode != 0:
        sys.exit(f"run: exit {run.returncode}\n{run.stdout}{run.stderr}")
    if args.bazel:
        base = subprocess.run(prefix + ["info", "output_base"], cwd=ws, env=env,
                              capture_output=True, text=True, check=True).stdout.strip()
    else:
        base = os.path.realpath(out)
    for name, text in (("test", test.stdout + test.stderr), ("run", run.stdout)):
        rows[name] = probe_lines(normalize(text, ws, base))
    version = subprocess.run([args.bazel, "--version"], capture_output=True,
                             text=True).stdout.strip() if args.bazel else "native"
    json.dump({"reference": version, "rows": rows}, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
