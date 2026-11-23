#!/usr/bin/env python3
"""Genuine (or native) capture: can a test spawn write into the source tree
through its runfile symlinks (Python's __pycache__ next to a resolved
source), per strategy, on this host?

The workspace (`workspace/` next to this script) is copied fresh per case into
the scratch directory; the probe test resolves its runfile `pkg/mod.py`,
imports it with the host python3 from the resolved directory (Python writes
`__pycache__` there when it can), then tries a mkdir and an append there.
Recorded: exit code, the process summary, the probe's lines (from
--test_output=all) and the files that changed in the workspace copy.

  capture.py --bazel <genuine bazel 9.3.1> --scratch <dir>
  capture.py --native <bazel-rs> --scratch <dir>
"""
import argparse, hashlib, json, os, shutil, subprocess

CASES = [
    ("default", ["test", "//pkg:probe"]),
    ("processwrapper", ["test", "--spawn_strategy=processwrapper-sandbox", "//pkg:probe"]),
    ("linux-sandbox", ["test", "--spawn_strategy=linux-sandbox", "//pkg:probe"]),
    ("standalone", ["test", "--spawn_strategy=local", "//pkg:probe"]),
    ("no-sandbox-tag", ["test", "//pkg:probe_no_sandbox"]),
    ("local-tag", ["test", "//pkg:probe_local"]),
]
KEYS = ("resolved=", "pycache=", "mkdir=", "append=")


def snapshot(root):
    out = {}
    for base, dirs, files in os.walk(root):
        dirs[:] = [d for d in dirs if not d.startswith("bazel-")]
        rel = os.path.relpath(base, root)
        if rel.startswith("bazel-"):
            continue
        for name in dirs:
            out[os.path.normpath(os.path.join(rel, name)) + "/"] = "dir"
        for name in files:
            path = os.path.join(base, name)
            if os.path.islink(path):
                continue
            with open(path, "rb") as handle:
                out[os.path.normpath(os.path.join(rel, name))] = hashlib.sha256(
                    handle.read()).hexdigest()
    return out


def changed(before, after):
    rows = []
    for key in sorted(set(before) | set(after)):
        if key == "MODULE.bazel.lock":
            continue
        if before.get(key) != after.get(key):
            kind = "added" if key not in before else "removed" if key not in after else "modified"
            # __pycache__ contents carry the python version in the name.
            if "__pycache__/" in key and not key.endswith("__pycache__/"):
                key = key.split("__pycache__/")[0] + "__pycache__/*.pyc"
            rows.append(f"{kind} {key}")
    return sorted(set(rows))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    parser.add_argument("--only", action="append")
    args = parser.parse_args()
    here = os.path.dirname(os.path.abspath(__file__))
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    rows = []
    for name, argv in CASES:
        if args.only and name not in args.only:
            continue
        ws = os.path.join(scratch, name, "ws")
        home = os.path.join(scratch, name, "home")
        shutil.copytree(os.path.join(here, "workspace"), ws)
        os.makedirs(home)
        env = {"PATH": "/usr/bin:/bin", "HOME": home}
        argv = argv[:1] + ["--test_output=all"] + argv[1:]
        if args.bazel:
            full = [args.bazel, "--nosystem_rc", "--nohome_rc", "--batch",
                    f"--output_user_root={scratch}/{name}/root"] + argv
        else:
            full = [args.native, "--nosystem_rc", "--nohome_rc",
                    f"--cache-dir={scratch}/{name}/cache"] + argv
        before = snapshot(ws)
        run = subprocess.run(full, cwd=ws, env=env, capture_output=True, text=True)
        after = snapshot(ws)
        text = run.stdout + run.stderr
        rows.append({
            "id": name, "args": argv, "exit": run.returncode,
            "errors": [l for l in text.splitlines() if l.startswith("ERROR: ")],
            "processes": [l for l in text.splitlines()
                          if l.startswith("INFO: ") and (" processes: " in l or " process: " in l)],
            "probe": [l for l in text.splitlines() if l.startswith(KEYS)],
            "workspace_changes": changed(before, after),
        })
    json.dump({"engine": "genuine" if args.bazel else "native", "rows": rows}, os.sys.stdout,
              indent=1)
    print()


if __name__ == "__main__":
    main()
