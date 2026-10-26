#!/usr/bin/env python3
"""Genuine (or native) capture of --sandbox_add_mount_pair (SandboxOptions.java:173-180,
MountPairConverter :50-80) as ~/code's `build:linux_* --sandbox_add_mount_pair=/tmp` uses it.

A one-genrule workspace is built/queried per case in a scratch directory with a private
HOME; genuine runs `--batch` with a scratch --output_user_root, native a scratch --cache-dir.
Output: JSON on stdout (exit code, first ERROR/bazel-rs line, genrule output text).

  capture.py --bazel <genuine bazel 9.3.0> --scratch <dir>
  capture.py --native <bazel-rs> --scratch <dir>
"""
import argparse, json, os, shutil, subprocess

BUILD = 'genrule(name = "ok", outs = ["ok.txt"], cmd = "echo ok > $@")\n'
RC = "build:cross --sandbox_add_mount_pair=/tmp\n"
CASES = [
    ("identity-single", ["build", "--sandbox_add_mount_pair=/tmp", "//:ok"]),
    ("identity-pair", ["build", "--sandbox_add_mount_pair=/tmp:/tmp", "//:ok"]),
    ("rc-config", ["build", "--config=cross", "//:ok"]),
    ("test-inherits", ["test", "--sandbox_add_mount_pair=/tmp", "//:ok"]),
    ("info-inherits", ["info", "--sandbox_add_mount_pair=/tmp", "release"]),
    ("query-unrecognized", ["query", "--sandbox_add_mount_pair=/tmp", "//:ok"]),
    ("empty-path", ["build", "--sandbox_add_mount_pair=/a::/b", "//:ok"]),
    ("three-paths", ["build", "--sandbox_add_mount_pair=/a:/b:/c", "//:ok"]),
    ("blank", ["build", "--sandbox_add_mount_pair= ", "//:ok"]),
    ("remount", ["build", "--sandbox_add_mount_pair=/usr:/opt/mp", "//:ok"]),
]


def first_error(text):
    for line in text.splitlines():
        for prefix in ("ERROR: ", "FATAL: ", "bazel-rs: "):
            if line.startswith(prefix) and "memory limit defaulted" not in line \
                    and "actions executed" not in line:
                return line[len(prefix):]
    return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    rows = []
    for name, argv in CASES:
        ws = os.path.join(scratch, name, "ws")
        home = os.path.join(scratch, name, "home")
        os.makedirs(ws)
        os.makedirs(home)
        for file, text in (("MODULE.bazel", 'module(name = "mp")\n'), ("BUILD.bazel", BUILD),
                           (".bazelrc", RC)):
            with open(os.path.join(ws, file), "w") as handle:
                handle.write(text)
        env = {"PATH": "/usr/bin:/bin", "HOME": home}
        if args.bazel:
            full = [args.bazel, "--nosystem_rc", "--nohome_rc", "--batch",
                    f"--output_user_root={scratch}/{name}/root"] + argv[:1] + \
                   [f"--repository_cache={scratch}/repocache"] + argv[1:]
        else:
            full = [args.native, "--nosystem_rc", "--nohome_rc",
                    f"--cache-dir={scratch}/{name}/cache"] + argv
        run = subprocess.run(full, cwd=ws, env=env, capture_output=True, text=True)
        output = None
        if run.returncode == 0 and argv[0] in ("build", "test"):
            if args.bazel:
                path = os.path.join(ws, "bazel-bin/ok.txt")
            else:
                path = next(os.path.join(root, "ok.txt") for root, _, files in
                            os.walk(os.path.join(scratch, name, "cache", "actions"))
                            if "ok.txt" in files)
            output = open(path).read()
        rows.append({"id": name, "args": argv, "exit": run.returncode,
                     "error": first_error(run.stderr), "output": output,
                     "stdout": run.stdout if argv[0] in ("info", "query") else None})
    json.dump({"engine": "genuine" if args.bazel else "native", "rows": rows}, os.sys.stdout,
              indent=1)
    print()


if __name__ == "__main__":
    main()
