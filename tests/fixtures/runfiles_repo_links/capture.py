#!/usr/bin/env python3
"""Genuine (or native) capture of how a file of an external repository is
presented to a spawn: a test's runfile, an action input run at its execution
path and an action tool's runfile, each a readlink-resolving wrapper
(rules_nodejs's bin/node) that runs its undeclared sibling bin/real/tool.

Builds tests/fixtures/runfiles_repo_links/workspace in a scratch copy under
each strategy (standalone, sandboxed), one output base / cache per case, and
writes the probe lines as JSON on stdout.

  capture.py --bazel <genuine bazel 9.3.0> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
KEYS = ("test_runfile_", "input_", "tool_runfile_")


def probe_lines(text):
    return [line for line in text.splitlines() if line.startswith(KEYS)]


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
    home = os.path.join(scratch, "home")
    os.makedirs(home)
    rows = []
    summaries = {}
    for strategy in ("standalone", "sandboxed"):
        env = {"PATH": "/usr/bin:/bin", "HOME": home}
        case = os.path.join(scratch, strategy)
        if args.bazel:
            argv = [args.bazel, "--nosystem_rc", "--nohome_rc", "--noworkspace_rc",
                    f"--output_user_root={case}", "--batch", "test",
                    f"--repository_cache={scratch}/repocache"]
        else:
            argv = [args.native, "--ignore_all_rc_files", f"--cache-dir={case}", "test"]
        argv += [f"--spawn_strategy={strategy}", "--test_output=all", "//:all"]
        run = subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True)
        if run.returncode != 0:
            sys.exit(f"{strategy}: exit {run.returncode}\n{run.stdout}{run.stderr}")
        outputs = []
        for name in ("input.txt", "tool_runfiles.txt"):
            if args.bazel:
                outputs.append(open(os.path.join(ws, "bazel-bin", name)).read())
            else:
                outputs.append(next(
                    open(os.path.join(root, name)).read()
                    for root, _, files in os.walk(os.path.join(case, "actions"))
                    if name in files))
        summaries[strategy] = [
            line for line in run.stderr.splitlines() if re.match(r"INFO: \d+ process", line)]
        rows.append({"strategy": strategy,
                     "lines": probe_lines(run.stdout + run.stderr + "".join(outputs))})
    version = subprocess.run([args.bazel, "--version"], capture_output=True,
                             text=True).stdout.strip() if args.bazel else "native"
    json.dump({"reference": version, "process_summaries": summaries, "rows": rows},
              sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
