#!/usr/bin/env python3
"""Capture tests/fixtures/test_exec_env/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

The command line test-setup.sh gets (TestStrategy.getArgs: the test
executable's runfiles path as PathFragment.getCallablePathString, so a test of
the main repository's root package is `./root_test`) and TEST_BINARY, printed
by the tests of workspace/ themselves. Runs `test --test_output=all //...` once
in a fresh copy (--batch, no rc files, private HOME and output user root) and
records the exit code and the SETUP_ARGS / TEST_BINARY lines of stdout.
tests/test_exec_env.rs replays it natively.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
FLAGS = ['--platforms=//:p', '--host_platform=//:p', '--test_output=all']
TARGETS = ['//...']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    args = parser.parse_args()
    root = os.path.abspath(args.scratch)
    shutil.rmtree(root, ignore_errors=True)
    workspace = os.path.join(root, 'ws')
    shutil.copytree(os.path.join(HERE, 'workspace'), workspace)
    home = os.path.join(root, 'home')
    os.makedirs(home)
    env = dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home)
    argv = [args.bazel, '--batch', '--ignore_all_rc_files', f'--output_user_root={root}/our',
            'test'] + FLAGS + TARGETS
    result = subprocess.run(argv, cwd=workspace, env=env, capture_output=True, text=True)
    lines = sorted(line for line in result.stdout.splitlines()
                   if line.startswith(('SETUP_ARGS ', 'TEST_BINARY ')))
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, flags=FLAGS, targets=TARGETS,
                   exit=result.returncode, lines=lines), sys.stdout, indent=1, sort_keys=True)
    print()


if __name__ == '__main__':
    main()
