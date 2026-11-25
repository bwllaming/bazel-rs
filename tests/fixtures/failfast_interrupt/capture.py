#!/usr/bin/env python3
"""Genuine (or native) capture of a build's first failure without --keep_going
interrupting the actions in flight (tests/fixtures/failfast_interrupt).

Runs tests/fixtures/failfast_interrupt/workspace in a scratch copy. Each case
starts three 15-second sleepers (genrules that leave a partial output first),
a 15-second test or a 15-second persistent-worker request next to an action
that fails after a second, and records
- the exit code and whether the command ended promptly after the failure
  (`prompt`: within 5 s of its ERROR line, i.e. the running actions were
  interrupted, not waited for);
- the console (stderr) without the progress, analysis and timing lines;
- the processes of the interrupted actions still alive afterwards;
- what the interrupted actions left: the files of bazel-bin/pkg (the
  standalone case leaves the sleepers' partial outputs: they ran in the
  execroot) and the disk cache entries;
- a rerun of one sleeper (`rerun`): not cached, it executes again.
Prints JSON on stdout.

  capture.py --bazel <genuine bazel 9.3.1> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, json, os, re, shutil, subprocess, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
REGISTRY = os.path.join(os.path.dirname(HERE), "builtin_modules", "registry")

DROPPED = (
    "Computing main repo mapping",
    "Loading:",
    "Analyzing:",
    "INFO: Analyzed",
    "INFO: Found",
    "INFO: Invocation ID",
    "INFO: Elapsed time",
    "INFO: Build completed",
    "OpenJDK",
    "Extracting Bazel installation",
    "bazel-rs:",
    "[",
)

SLEEPERS = ["//pkg:sleeper0", "//pkg:sleeper1", "//pkg:sleeper2"]

CASES = [
    ("build", ["build"] + SLEEPERS + ["//pkg:fail", "//pkg:after"]),
    # Reuses the build case's workspace and caches.
    ("rerun", ["build", SLEEPERS[0]]),
    ("standalone", ["build", "--spawn_strategy=standalone"] + SLEEPERS + ["//pkg:fail"]),
    ("test", ["test", "//pkg:slow_test", "//pkg:fail_late"]),
    ("worker", ["build", "//pkg:slow_work", "//pkg:fail"]),
]


def console(text, ws):
    lines = []
    for line in text.replace(ws, "<WS>").splitlines():
        if line.startswith(DROPPED) or not line.strip():
            continue
        lines.append(line)
    return lines


def survivors():
    """The sleepers', test's and worker's processes still running."""
    out = subprocess.run(["ps", "-eo", "args"], capture_output=True, text=True).stdout
    return sorted(
        line.strip()
        for line in out.splitlines()
        if re.search(r"sleep 15\.073|slow_worker\.py", line) and "ps -eo" not in line
    )


def listing(directory):
    if not os.path.isdir(directory):
        return None
    return {
        name: open(os.path.join(directory, name)).read()
        for name in sorted(os.listdir(directory))
        if os.path.isfile(os.path.join(directory, name))
    }


def disk_cache_files(directory):
    count = 0
    for _, _, files in os.walk(directory):
        count += len([name for name in files if not name.startswith(".")])
    return count


def run(argv, ws, env):
    """Runs `argv`; its exit code, stdout, stderr and the seconds from the
    first ERROR line on stderr to its exit (None without one)."""
    process = subprocess.Popen(argv, cwd=ws, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True)
    stdout = []
    reader = threading.Thread(target=lambda: stdout.append(process.stdout.read()))
    reader.start()
    lines = []
    failed_at = None
    for line in process.stderr:
        if failed_at is None and line.startswith("ERROR:"):
            failed_at = time.monotonic()
        lines.append(line)
    returncode = process.wait()
    ended = time.monotonic()
    reader.join()
    after = None if failed_at is None else round(ended - failed_at, 1)
    return returncode, "".join(stdout), "".join(lines), after


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bazel")
    parser.add_argument("--native")
    parser.add_argument("--scratch", required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    if os.path.exists(scratch):
        # Output trees are read-only.
        subprocess.run(["chmod", "-R", "u+w", scratch])
        shutil.rmtree(scratch)
    os.makedirs(scratch)
    home = os.path.join(scratch, "home")
    os.makedirs(home)
    env = {"PATH": "/usr/bin:/bin", "HOME": home}
    rows = []
    for name, command in CASES:
        if name != "rerun":
            ws = os.path.join(scratch, "ws-" + name)
            shutil.copytree(os.path.join(HERE, "workspace"), ws)
            disk_cache = os.path.join(scratch, "disk-cache-" + name)
            root = os.path.join(scratch, "root-" + name)
        if args.bazel:
            argv = [args.bazel, "--batch", "--ignore_all_rc_files",
                    "--output_user_root=" + root]
        else:
            argv = [args.native, "--ignore_all_rc_files",
                    "--cache-dir=" + os.path.join(root, "cache")]
        argv += command[:1]
        if not args.bazel:
            # The built-in modules from the offline registry fixture.
            argv += ["--experimental-builtin-modules", "--registry=file://" + REGISTRY]
        argv += ["--disk_cache=" + disk_cache] + command[1:]
        returncode, stdout, stderr, after_failure = run(argv, ws, env)
        with open(os.path.join(scratch, name + ".stderr"), "w") as log:
            log.write(stderr)
        with open(os.path.join(scratch, name + ".stdout"), "w") as log:
            log.write(stdout)
        alive = survivors()
        row = {
            "case": name,
            "exit_code": returncode,
            # The command ended within 5 s of printing the failure: the
            # actions in flight were interrupted, not waited for.
            "prompt": after_failure is None or after_failure < 5,
            "console": console(stderr, ws),
            "stdout": console(stdout, ws),
            "survivors": alive,
            "bin_pkg": listing(os.path.join(ws, "bazel-bin", "pkg")),
            "disk_cache_files": disk_cache_files(disk_cache),
        }
        print(f"{name}: exit {returncode}, {after_failure} s after the failure", file=sys.stderr)
        rows.append(row)
        # Nothing of one case may linger into the next.
        for line in alive:
            print("survivor: " + line, file=sys.stderr)
        subprocess.run(["pkill", "-f", r"sleep 15\.073|slow_worker\.py"])
    json.dump({"rows": rows}, sys.stdout, indent=1, sort_keys=True)
    print()


if __name__ == "__main__":
    main()
