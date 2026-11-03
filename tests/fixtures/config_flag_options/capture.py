#!/usr/bin/env python3
"""Capture tests/fixtures/config_flag_options/reference.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference.json

Copies the workspace to DIR, builds //:top //:plain //:moved once per case
(--batch, no rc files, private HOME and output user root) and records the
exit code and every report file (output path relative to bazel-out and its
content). tests/config_flag_options.rs replays the cases natively.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

FLAGS = [
    '--features=race', '--features=-foo', '--features=bar', '--features=bar',
    '--host_features=hfeat', '--host_features=-hoff',
    '--copt=-DA', '--copt', '-DA2', '--cxxopt=-DB', '--conlyopt=-DC', '--linkopt=-lm',
    '--host_copt=-DH', '--host_cxxopt=-DHX', '--host_conlyopt=-DHC', '--host_linkopt=-lhl',
    '--javacopt=-Xlint:all -Werror', '--javacopt=-g', "--javacopt=it's",
    '--javacopt=-Dx="a b"', '--host_javacopt=-DHJ', '--dynamic_mode=FULLY',
]

CASES = {
    'default': [],
    'flags': FLAGS,
    'dynamic_off': ['--dynamic_mode=off', '--features=-pkg_feature'],
}

ERRORS = {
    'bad_dynamic_mode': ['--dynamic_mode=static'],
    'no_prefix_copt': ['--nocopt'],
    'bad_per_file_copt': ['--per_file_copt=[@-O0'],
}

COMMON = ['build', '--platforms=//:p', '--host_platform=//:p']
TARGETS = ['//:top', '//:plain', '//:moved']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    workspace = os.path.join(scratch, 'ws')
    shutil.rmtree(scratch, ignore_errors=True)
    shutil.copytree(os.path.join(HERE, 'workspace'), workspace)
    home = os.path.join(scratch, 'home')
    os.makedirs(home)
    env = dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home)

    def bazel(flags, targets):
        argv = [args.bazel, '--batch', '--ignore_all_rc_files',
                f'--output_user_root={scratch}/our'] + COMMON + flags + targets
        return subprocess.run(argv, cwd=workspace, env=env, capture_output=True, text=True)

    cases = {}
    for name, flags in CASES.items():
        result = bazel(flags, TARGETS)
        outputs = {}
        out = os.path.join(workspace, 'bazel-out')
        for root, _, files in os.walk(out):
            for file in files:
                path = os.path.join(root, file)
                relative = os.path.relpath(path, out)
                if file.endswith('.txt') and '-status' not in file:
                    outputs[relative] = open(path).read()
        cases[name] = dict(flags=flags, exit=result.returncode, outputs=outputs)
        subprocess.run([args.bazel, '--batch', '--ignore_all_rc_files',
                        f'--output_user_root={scratch}/our', 'clean'], cwd=workspace, env=env,
                       capture_output=True)
    errors = {}
    for name, flags in ERRORS.items():
        result = bazel(flags, ['//:plain'])
        errors[name] = dict(flags=flags, exit=result.returncode,
                            errors=[line for line in result.stderr.splitlines()
                                    if line.startswith('ERROR: ')])
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases, errors=errors), sys.stdout,
              indent=1, sort_keys=True)
    print()


if __name__ == '__main__':
    main()
