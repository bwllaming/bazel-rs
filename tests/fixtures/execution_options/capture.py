#!/usr/bin/env python3
"""Capture tests/fixtures/execution_options/reference.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference.json

The `--toolchain_resolution_debug` messages of `build --nobuild //:u //:v`
(the INFO events and their continuation lines, as a sorted set: genuine
repeats some on Skyframe restarts), the `--check_up_to_date` failure of a
fresh workspace and the exit codes; --batch, no rc files, private HOME.
"""
import argparse, hashlib, json, os, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    workspace = os.path.join(scratch, 'ws')
    shutil.copytree(os.path.join(HERE, 'workspace'), workspace)
    home = os.path.join(scratch, 'home')
    os.makedirs(home)

    def bazel(*argv):
        return subprocess.run([args.bazel, '--batch', '--ignore_all_rc_files',
                               f'--output_user_root={scratch}/our', 'build',
                               '--platforms=//:p', '--host_platform=//:p'] + list(argv),
                              cwd=workspace, env=dict(PATH='/usr/bin:/bin', HOME=home),
                              capture_output=True, text=True)

    debug = bazel('--nobuild', '--toolchain_resolution_debug=//:t', '//:u', '//:v')
    trace = sorted({line for line in debug.stderr.splitlines()
                    if 'ToolchainResolution: ' in line})
    up = bazel('--check_up_to_date', '//:h')
    json.dump(dict(bazel='9.3.1', elf_sha256=hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest(),
                   toolchain_debug=dict(exit=debug.returncode, lines=trace),
                   check_up_to_date=dict(exit=up.returncode,
                                         errors=[line for line in up.stderr.splitlines()
                                                 if 'is not up-to-date' in line])),
              sys.stdout, indent=1)
    print()


if __name__ == '__main__':
    main()
