#!/usr/bin/env python3
"""Genuine (or native) capture of the inode identity of distinct artifacts
with identical content: two sources, two generated files, a copy of a
source, a tree artifact's file (all the bytes "same\\n") and an
actions.symlink alias of a source, as an action's inputs, an action tool's
runfiles and a test's runfiles see them (each line: the path, the first
listed path with the same device:inode after following links, and whether
the path is a symlink), and the output tree's files (bazel-bin).

Builds tests/fixtures/distinct_inodes/workspace in a scratch copy under
each strategy (standalone, sandboxed), one output base / cache per case, and
writes the probe lines as JSON on stdout.

  capture.py --bazel <genuine bazel 9.3.0> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, json, os, re, shutil, stat, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
KEYS = ("test_runfile_", "input_", "tool_runfile_")
OUTPUT_TREE = ("a.out", "b.out", "c.out", "dir/sub/f.txt", "link.txt")


def probe_lines(text):
    return [line for line in text.splitlines() if line.startswith(KEYS)]


def output_tree_lines(bin_dir):
    first = {}
    lines = []
    for name in OUTPUT_TREE:
        st = os.stat(os.path.join(bin_dir, name))
        first.setdefault((st.st_dev, st.st_ino), name)
        lines.append(f"output_tree_{name}={first[(st.st_dev, st.st_ino)]}")
    return lines


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
    home = os.path.join(scratch, "home")
    os.makedirs(home)
    rows = []
    summaries = {}
    for strategy in ("standalone", "sandboxed"):
        env = {"PATH": "/usr/bin:/bin", "HOME": home}
        case = os.path.join(scratch, strategy)
        if args.bazel:
            prefix = [args.bazel, "--nosystem_rc", "--nohome_rc", "--noworkspace_rc",
                      f"--output_user_root={case}", "--batch"]
            argv = prefix + ["test", f"--repository_cache={scratch}/repocache"]
        else:
            prefix = [args.native, "--ignore_all_rc_files", f"--cache-dir={case}"]
            argv = prefix + ["test"]
        argv += [f"--spawn_strategy={strategy}", "--test_output=all", "//:all"]
        run = subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True)
        if run.returncode != 0:
            sys.exit(f"{strategy}: exit {run.returncode}\n{run.stdout}{run.stderr}")
        bin_dir = subprocess.run(prefix + ["info", "bazel-bin"], cwd=ws, env=env,
                                 capture_output=True, text=True, check=True).stdout.strip()
        outputs = "".join(open(os.path.join(bin_dir, name)).read()
                          for name in ("input_probe.txt", "tool_probe.txt"))
        summaries[strategy] = [
            line for line in run.stderr.splitlines() if re.match(r"INFO: \d+ process", line)]
        rows.append({"strategy": strategy,
                     "lines": probe_lines(run.stdout + run.stderr + outputs)
                     + output_tree_lines(bin_dir)})
    version = subprocess.run([args.bazel, "--version"], capture_output=True,
                             text=True).stdout.strip() if args.bazel else "native"
    json.dump({"reference": version, "process_summaries": summaries, "rows": rows},
              sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
