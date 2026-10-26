#!/usr/bin/env python3
"""Genuine (or native) capture of the signals a spawn starts with ignored.

Runs tests/fixtures/signal_dispositions/workspace in a scratch copy with the
client started either with default dispositions or with SIGHUP, SIGINT,
SIGQUIT, SIGPIPE and SIGTERM ignored (a background job of a non-interactive
shell ignores SIGINT and SIGQUIT, nohup ignores SIGHUP), and records which
of those signals are ignored (/proc/self/status SigIgn) in
- a repository rule's `execute`, a genrule action and a test, per spawn
  strategy (`bazel test //pkg:probe //pkg:probe_test`);
- the --workspace_status_command (started by the server itself, not through
  the process-wrapper);
- a `bazel run` target.
Genuine runs both in --batch and in server mode (one output root per case).
Prints JSON on stdout.

  capture.py --bazel <genuine bazel 9.3.0> --scratch <dir>     (genuine)
  capture.py --native <bazel-rs> --scratch <dir>               (native)
"""
import argparse, json, os, shutil, signal, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
SIGNALS = {"SIGHUP": 1, "SIGINT": 2, "SIGQUIT": 3, "SIGPIPE": 13, "SIGTERM": 15}


def ignored(line):
    """The names of SIGNALS set in a `SigIgn:\t<hex>` line."""
    mask = int(line.split()[-1], 16)
    return [name for name, number in SIGNALS.items() if mask >> (number - 1) & 1]


def probes(text, prefix):
    return [ignored(line) for line in text.splitlines() if line.startswith(prefix + " SigIgn:")]


def ignore_signals():
    for number in SIGNALS.values():
        signal.signal(number, signal.SIG_IGN)


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
    status_out = os.path.join(scratch, "status.txt")
    status_script = os.path.join(scratch, "status.sh")
    with open(status_script, "w") as script:
        script.write(f"#!/bin/sh\ngrep SigIgn /proc/self/status > {status_out}\n")
    os.chmod(status_script, 0o755)
    env = {"PATH": "/usr/bin:/bin", "HOME": home}
    modes = ("batch", "server") if args.bazel else ("native",)
    rows = []
    for mode in modes:
        for client in ("default", "ignored"):
            for strategy in ("standalone", "sandboxed"):
                case = os.path.join(scratch, f"{mode}-{client}-{strategy}")

                def bazel(*command):
                    if args.bazel:
                        argv = [args.bazel, "--nosystem_rc", "--nohome_rc", "--noworkspace_rc",
                                f"--output_user_root={case}"]
                        if mode == "batch":
                            argv.append("--batch")
                        argv += [command[0], f"--repository_cache={scratch}/repocache"]
                    else:
                        argv = [args.native, "--ignore_all_rc_files", f"--cache-dir={case}",
                                command[0]]
                    argv += [f"--spawn_strategy={strategy}",
                             f"--workspace_status_command={status_script}", *command[1:]]
                    run = subprocess.run(
                        argv, cwd=ws, env=env, capture_output=True, text=True,
                        preexec_fn=ignore_signals if client == "ignored" else None)
                    if run.returncode != 0:
                        sys.exit(f"{mode}/{client}/{strategy} {command[0]}: exit "
                                 f"{run.returncode}\n{run.stderr}")
                    return run

                if os.path.exists(status_out):
                    os.unlink(status_out)
                test = bazel("test", "--test_output=all", "//pkg:probe", "//pkg:probe_test")
                status = open(status_out).read()
                run = bazel("run", "//pkg:probe_bin")
                if args.bazel:
                    genrule = open(os.path.join(ws, "bazel-bin/pkg/probe.txt")).read()
                    if mode == "server":
                        subprocess.run([args.bazel, f"--output_user_root={case}", "shutdown"],
                                       cwd=ws, env=env, capture_output=True)
                else:
                    genrule = next(
                        open(os.path.join(root, "probe.txt")).read()
                        for root, _, files in os.walk(os.path.join(case, "actions"))
                        if "probe.txt" in files)
                rows.append({
                    "mode": mode, "client": client, "strategy": strategy,
                    "repository_execute": probes(genrule, "repository"),
                    "action": probes(genrule, "action"),
                    "test": probes(test.stdout + test.stderr, "probe"),
                    "workspace_status": [ignored(status)],
                    "run": probes(run.stdout, "probe"),
                })
    version = subprocess.run([args.bazel, "--version"], capture_output=True,
                             text=True).stdout.strip() if args.bazel else "native"
    json.dump({"reference": version, "rows": rows}, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
