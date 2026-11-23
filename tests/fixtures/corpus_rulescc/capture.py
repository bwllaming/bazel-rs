#!/usr/bin/env python3
"""Capture tests/fixtures/corpus_rulescc/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

Parity cases of the open-source corpus bug B32 fixed on branch corpus-rulescc
(docs/OSS_CORPUS.md): the FDO/Propeller/malloc/libc/ThinLTO CppOptions. Each case runs once in a fresh copy of workspace/
(--batch, no rc files, private HOME and output user root) and records the
exit code, the stdout, the stderr lines matching the case's patterns (the
workspace path replaced by <WS>) and the content of the named outputs by
their path below bazel-out. tests/corpus_rulescc.rs replays the cases natively.
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

# name: (startup, command, flags, targets, stderr patterns, outputs[, workspace])
CASES = {
    'fdo_label_options': (
        [], 'build', BUILD, ['//fdo:labels'], [], ['leaf.txt']),
    'fdo_label_values': (
        [], 'build', BUILD, ['//fdo:label_values'], [r'misplaced here'], ['leaf.txt']),
    'fdo_absolute_paths': (
        [], 'build', BUILD, ['//fdo:paths'], [], ['leaf.txt']),
    'lto_option_lists': (
        [], 'build', BUILD, ['//fdo:lists'], [], ['leaf.txt']),
    'options_read_back': (
        [], 'build', BUILD, ['//fdo:read_set', '//fdo:read'], [r'READ \[None, None', r'READ \[None, "/abs'], ['read.txt']),
    'options_on_the_command_line': (
        [], 'build', BUILD + ['--custom_malloc=//fdo:malloc', '--grte_top=//fdo:libc',
                              '--fdo_optimize=/abs/profile.afdo', '--ltobackendopt=-q',
                              '--ltoindexopt=-r', '--per_file_ltobackendopt=.*@-z',
                              '--experimental_cpp_modules', '--xbinary_fdo=',
                              '--fdo_prefetch_hints=//fdo:hints'],
        ['//fdo:leaf'], [], ['leaf.txt']),
    'xbinary_fdo_on_the_command_line': (
        [], 'build', BUILD + ['--xbinary_fdo=//fdo:x.xfdo', '--memprof_profile=//fdo:mem'],
        ['//fdo:leaf'], [r'misplaced here'], ['leaf.txt']),
    'grte_top_not_a_label': (
        [], 'build', BUILD + ['--grte_top=fdo:libc'], ['//fdo:leaf'], [r'grte_top'], []),
    'action_env_transition': (
        [], 'build', BUILD + ['--action_env=X=0'], ['//fdo:env', '//fdo:env_leaf'], [r'READ_ENV'], ['env_leaf.txt']),
    'prerequisite_failures_allowed': (
        [], 'build', BUILD, ['//failures:missing_test', '//failures:present_test'],
        [r'CAUSES .*missing', r'CAUSES .*present', r'PROBE'], ['missing_test.sh', 'present_test.sh']),
    'exec_grte_top': (
        [], 'build', BUILD + ['--host_grte_top=//fdo:libc', '--grte_top=//fdo:prof'],
        ['//fdo:exec_user', '//fdo:leaf'], [], ['leaf.txt']),
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
        startup, command, flags, targets, patterns, outputs = case[:6]
        source = case[6] if len(case) > 6 else 'workspace'
        bin_paths = case[7] if len(case) > 7 else []
        root = os.path.join(scratch, str(index))
        workspace = os.path.join(root, 'ws')
        shutil.copytree(os.path.join(HERE, source), workspace)
        home = os.path.join(root, 'home')
        os.makedirs(home)
        env = dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home)
        argv = ([args.bazel, '--batch', '--ignore_all_rc_files', f'--output_user_root={root}/our']
                + startup + [command] + flags + targets)
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
            # Runfiles trees repeat the outputs (bazel-rs reports none).
            dirs[:] = [d for d in dirs if not d.endswith('.runfiles')]
            for file in names:
                if file in outputs:
                    path = os.path.join(top, file)
                    files[os.path.relpath(path, out)] = open(path).read()
        # Paths below bazel-bin: a symlink's text, a file's content (through
        # symlinks), or None when nothing resolves there.
        tree = {}
        for relative in bin_paths:
            path = os.path.join(workspace, 'bazel-bin', relative)
            if os.path.islink(path):
                tree[relative] = ['symlink', os.readlink(path)]
            elif os.path.isfile(path):
                tree[relative] = ['file', open(path).read()]
            else:
                tree[relative] = None
        cases[name] = dict(workspace=source, startup=startup, command=command, flags=flags,
                           bin_paths=tree,
                           targets=targets,
                           exit=result.returncode, stdout=result.stdout, stderr=matched,
                           outputs=files)
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases), sys.stdout, indent=1,
              sort_keys=True)
    print()


if __name__ == '__main__':
    main()
