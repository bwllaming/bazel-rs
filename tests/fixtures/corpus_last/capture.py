#!/usr/bin/env python3
"""Capture tests/fixtures/corpus_last/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

Parity cases of the open-source corpus bugs fixed on branch corpus-last
(docs/OSS_CORPUS.md). Each case runs once in a fresh copy of its workspace
(--batch, no rc files, private HOME and output user root) and records the
exit code, the stdout, the stderr lines matching the case's patterns (the
workspace path replaced by <WS>), the content of the named outputs by their
path below bazel-out, and whether each listed path below bazel-bin exists.
tests/corpus_last.rs replays the cases natively.
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
BUILD = ['--platforms=//:p', '--host_platform=//:p']

# name: (command, flags, targets, stderr patterns, outputs, bazel-bin paths[, workspace])
CASES = {
    # B43: a rule forwarding an executable's OutputGroupInfo builds that
    # executable's runfiles tree (its HIDDEN_TOP_LEVEL group is the tree).
    'forwarded_runfiles_tree': (
        'build', BUILD, ['//fwd:f'], [r'HIDDEN f '], ['b'],
        ['fwd/b.runfiles_manifest', 'fwd/b.repo_mapping', 'fwd/b.runfiles/MANIFEST']),
    'forwarded_library_runfiles': (
        'build', BUILD, ['//fwd:fl'], [r'HIDDEN fl '], ['l.txt', 'data.txt'], []),
    'files_only_no_tree': (
        'build', BUILD, ['//fwd:only'], [], ['b'],
        ['fwd/b.runfiles_manifest', 'fwd/b.repo_mapping', 'fwd/b.runfiles/MANIFEST']),
    'forwarded_group_seen': (
        'build', BUILD + ['--nobuild'], ['//fwd:seen'], [r'SEEN seen '], [], []),
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
    for index, (name, case) in enumerate(CASES.items()):
        command, flags, targets, patterns, outputs, bin_paths = case[:6]
        source = case[6] if len(case) > 6 else 'workspace'
        root = os.path.join(scratch, str(index))
        workspace = os.path.join(root, 'ws')
        shutil.copytree(os.path.join(HERE, source), workspace)
        home = os.path.join(root, 'home')
        os.makedirs(home)
        env = dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home)
        argv = ([args.bazel, '--batch', '--ignore_all_rc_files', f'--output_user_root={root}/our']
                + [command] + flags + targets)
        result = subprocess.run(argv, cwd=workspace, env=env, capture_output=True, text=True)
        lines = [TIME.sub('(TIME) ', line.replace(workspace, '<WS>'))
                 for line in result.stderr.splitlines()]
        matched = []
        for pattern in patterns:
            found = [line for line in lines if re.search(pattern, line)]
            matched.append(dict(pattern=pattern, line=found[0] if found else None))
        files = {}
        out = os.path.join(workspace, 'bazel-out')
        for top, dirs, names in os.walk(out):
            # Runfiles trees repeat the outputs.
            dirs[:] = [d for d in dirs if not d.endswith('.runfiles')]
            for file in names:
                if file in outputs:
                    path = os.path.join(top, file)
                    files[os.path.relpath(path, out)] = open(path).read()
        exists = {relative: os.path.exists(os.path.join(workspace, 'bazel-bin', relative))
                  for relative in bin_paths}
        cases[name] = dict(workspace=source, command=command, flags=flags, targets=targets,
                           exit=result.returncode, stdout=result.stdout, stderr=matched,
                           outputs=files, exists=exists)
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases), sys.stdout, indent=1,
              sort_keys=True)
    print()


if __name__ == '__main__':
    main()
