#!/usr/bin/env python3
"""Capture tests/fixtures/package_errors/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

A package whose BUILD file fails part-way (`broken`: `fail()` after two
targets, like gRPC's examples/objective-c/BUILD and its removed
`objc_library`) is a package with errors: genuine keeps the targets defined
before the failure (Package.containsErrors), so `--keep_going` patterns and
queries see them, and a package whose load() fails (`loadfail`) keeps none.
Each case runs in a fresh copy of workspace/ (--batch, no rc files, private
HOME and output user root) and records the exit code, the stdout and every
ERROR/WARNING line of stderr (the workspace path replaced by <WS>).
tests/package_errors.rs replays the cases natively.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PLATFORM = ['--platforms=//:p', '--host_platform=//:p']
K = ['--keep_going']

# name: (command, flags, targets)
CASES = {
    'query_all': ('query', [], ['//...']),
    'query_all_keep_going': ('query', K, ['//...']),
    'query_package_keep_going': ('query', K, ['//broken:all']),
    'query_package_files_keep_going': ('query', K, ['//broken:*']),
    'query_package': ('query', [], ['//broken:all']),
    'query_before_keep_going': ('query', K, ['//broken:before']),
    'query_before': ('query', [], ['//broken:before']),
    'query_after_keep_going': ('query', K, ['//broken:after']),
    'query_deps_keep_going': ('query', K, ['deps(//uses:uses)']),
    'query_deps': ('query', [], ['deps(//uses:uses)']),
    'query_deps_after_keep_going': ('query', K, ['deps(//uses:uses_after)']),
    'query_rdeps_keep_going': ('query', K, ['rdeps(//..., //broken:before)']),
    'query_label_kind_keep_going': ('query', K + ['--output=label_kind'], ['//broken:all']),
    'query_xml_keep_going': ('query', K + ['--output=xml'], ['//broken:group']),
    'query_loadfail_keep_going': ('query', K, ['//loadfail:all']),
    'query_xml_build_file_keep_going': ('query', K + ['--output=xml'], ['//broken:BUILD + //good:BUILD']),
    'query_jsonproto_build_file_keep_going': ('query', K + ['--output=streamed_jsonproto'], ['//broken:BUILD + //good:BUILD']),
    'build_before_keep_going': ('build', K + ['--nobuild'] + PLATFORM, ['//broken:before']),
    'build_all_keep_going': ('build', K + ['--nobuild'] + PLATFORM, ['//...']),
    'build_uses_keep_going': ('build', K + ['--nobuild'] + PLATFORM, ['//uses:uses', '//good:good']),
    'build_uses_data_keep_going': ('build', K + ['--nobuild'] + PLATFORM, ['//uses:uses_data']),
    'build_before': ('build', ['--nobuild'] + PLATFORM, ['//broken:before']),
    'cquery_package_keep_going': ('cquery', K + PLATFORM, ['//broken:all']),
    'cquery_deps_keep_going': ('cquery', K + PLATFORM, ['deps(//uses:uses)']),
}

TIME = re.compile(r'^\(\d\d:\d\d:\d\d\) ')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    cases = {}
    for index, (name, (command, flags, targets)) in enumerate(CASES.items()):
        root = os.path.join(scratch, str(index))
        workspace = os.path.join(root, 'ws')
        shutil.copytree(os.path.join(HERE, 'workspace'), workspace)
        home = os.path.join(root, 'home')
        os.makedirs(home)
        env = dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home)
        argv = ([args.bazel, '--batch', '--ignore_all_rc_files', f'--output_user_root={root}/our',
                 command] + flags + targets)
        result = subprocess.run(argv, cwd=workspace, env=env, capture_output=True, text=True)
        lines = [TIME.sub('(TIME) ', line.replace(workspace, '<WS>'))
                 for line in result.stderr.splitlines()
                 if line.startswith(('ERROR:', 'WARNING:'))]
        cases[name] = dict(command=command, flags=flags, targets=targets,
                           exit=result.returncode, stdout=result.stdout, messages=lines)
        print(name, result.returncode, file=sys.stderr)
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases), sys.stdout, indent=1,
              sort_keys=True)
    print()


if __name__ == '__main__':
    main()
