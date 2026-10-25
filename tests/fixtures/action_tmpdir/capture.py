#!/usr/bin/env python3
"""Genuine (or native) capture of what a spawn observes about TMPDIR.

Builds tests/fixtures/action_tmpdir/workspace (genrule //pkg:probe and test
//pkg:probe_test) in a scratch copy under each strategy (standalone,
sandboxed) with the client TMPDIR unset or set to a scratch directory, one
output base / cache per case, and writes the observed probe lines (the
client TMPDIR spelled <client>) as JSON on stdout.

  capture.py --bazel <genuine bazel 9.3.0> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
MARKER = "/tmp/action-tmpdir-host-marker"


def probe_lines(text):
    keys = ("TMPDIR=", "TMPDIR_dir=", "host_tmp_visible=", "cwd_parent_tmp=", "socket_bind=")
    return [line for line in text.splitlines() if line.startswith(keys)]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    os.makedirs(scratch)
    ws = os.path.join(scratch, "ws")
    shutil.copytree(os.path.join(HERE, "workspace"), ws)
    client = os.path.join(scratch, "client-tmp")
    home = os.path.join(scratch, "home")
    os.makedirs(client)
    os.makedirs(home)
    open(MARKER, "w").close()
    rows = []
    summaries = {}
    for strategy in ("standalone", "sandboxed"):
        for tmp in ("unset", "client"):
            env = {"PATH": "/usr/bin:/bin", "HOME": home}
            if tmp == "client":
                env["TMPDIR"] = client
            case = os.path.join(scratch, f"{strategy}-{tmp}")
            if args.bazel:
                argv = [args.bazel, "--nosystem_rc", "--nohome_rc", "--noworkspace_rc",
                        f"--output_user_root={case}", "--batch", "test",
                        f"--repository_cache={scratch}/repocache"]
            else:
                argv = [args.native, "--ignore_all_rc_files", f"--cache-dir={case}", "test"]
            argv += [f"--spawn_strategy={strategy}", "--test_output=all",
                     "//pkg:probe", "//pkg:probe_test"]
            run = subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True)
            if run.returncode != 0:
                sys.exit(f"{strategy}/{tmp}: exit {run.returncode}\n{run.stderr}")
            if args.bazel:
                genrule = open(os.path.join(ws, "bazel-bin/pkg/probe.txt")).read()
            else:
                genrule = next(
                    open(os.path.join(root, "probe.txt")).read()
                    for root, _, files in os.walk(os.path.join(case, "actions"))
                    if "probe.txt" in files)
            test = run.stdout + run.stderr
            summaries[f"{strategy}/{tmp}"] = [
                line for line in run.stderr.splitlines()
                if re.match(r"INFO: \d+ process", line)]
            for kind, text in (("genrule", genrule), ("test", test)):
                lines = [line.replace(client, "<client>") for line in probe_lines(text)]
                rows.append({"kind": kind, "strategy": strategy, "client_tmpdir": tmp,
                             "lines": lines})
    os.unlink(MARKER)
    version = subprocess.run([args.bazel or args.native, "--version"], capture_output=True,
                             text=True).stdout.strip() if args.bazel else "native"
    json.dump({"reference": version, "process_summaries": summaries, "rows": rows},
              sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
