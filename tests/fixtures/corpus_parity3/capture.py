#!/usr/bin/env python3
"""Capture tests/fixtures/corpus_parity3/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

Parity cases of the open-source corpus bugs fixed on branch corpus-parity3
(docs/OSS_CORPUS.md). Each case runs once in a fresh copy of workspace/
(--batch, no rc files, private HOME and output user root) and records the
exit code, the stdout, the stderr lines matching the case's patterns (the
workspace path replaced by <WS>) and the content of the named outputs by
their path below bazel-out. tests/corpus_parity3.rs replays the cases natively.
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
    'feature_flag_transition': (
        [], 'build', BUILD, ['//ff:use', '//ff:unset', '//ff:empty'], [],
        ['unset.txt', 'empty.txt', 'use.txt']),
    'feature_flag_transition_query': ([], 'query', [], ['deps(//ff:unset)'], [], []),
    'write_mnemonic_summary': ([], 'aquery', BUILD + ['--output=summary'], ['//mnem:w'], [], []),
    'write_mnemonic_text': ([], 'aquery', BUILD, ['//mnem:w'], [], []),
    'write_mnemonic_filter': ([], 'aquery', BUILD, ['mnemonic("CppModuleMap|MyParams", //mnem:w)'], [], []),
    'write_mnemonic_type': (
        [], 'build', BUILD, ['//mnem:bad'], [r"parameter 'mnemonic'"], []),
    'declared_outputs_below_subpackage': (
        [], 'build', BUILD, ['//boundary:build', '//boundary/public:own'], [],
        ['index.txt', 'build.txt', 'own.txt']),
    'predeclared_output_below_subpackage': (
        [], 'build', BUILD, ['//boundary:pre'], [r'subpackage'], [], 'predeclared'),
    'private_attribute_native_names': (
        [], 'build', BUILD, ['//privkw:all'], [],
        ['libwith.txt', 'with_extra.txt', 'libwithout.txt']),
    'private_attribute_starlark_name': (
        [], 'build', BUILD, ['//privkw_bad:bad'], [r"no such attribute '_other'"], []),
    'relative_symlink_outputs_in_bazel_bin': (
        [], 'build', BUILD, ['//links'], [], [], 'workspace',
        ['links/app/node_modules/pkg', 'links/app/node_modules/pkg/index.js',
         'links/app/dangling']),
    'java_toolchain_declared': (
        [], 'build', BUILD + ['--extra_toolchains=//third_party/bazel_rules/rules_java/java:fake_java'],
        ['//third_party/bazel_rules/rules_java/java:declared',
         '//third_party/bazel_rules/rules_java/java:optional'], [], ['declared.txt', 'optional.txt']),
    'java_toolchain_undeclared': (
        [], 'build', BUILD + ['--extra_toolchains=//third_party/bazel_rules/rules_java/java:fake_java'],
        ['//third_party/bazel_rules/rules_java/java:undeclared'], [r'must declare'], []),
    'fragment_lists_are_mutable_copies': ([], 'build', BUILD, ['//jvmopts:show'], [], ['show.txt']),
    'target_actions_through_alias': (
        [], 'build', BUILD, ['//aliased:direct_test', '//aliased:alias_test'], [],
        ['direct_test.sh', 'alias_test.sh']),
    'aspect_view_per_attribute': (
        [], 'build', BUILD, ['//aliased:both_test'], [], ['both_test.sh']),
    'genquery_buildfiles_in_scope': ([], 'build', BUILD, ['//gq:loadgraph'], [], ['loadgraph']),
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
