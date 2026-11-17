#!/usr/bin/env python3
"""Capture tests/fixtures/corpus_parity2/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

Parity cases of the open-source corpus bugs fixed on branch corpus-parity2
(docs/OSS_CORPUS.md). Each case runs once in a fresh copy of workspace/
(--batch, no rc files, private HOME and output user root) and records the
exit code, the stdout, the stderr lines matching the case's patterns (the
workspace path replaced by <WS>) and the content of the named outputs by
their path below bazel-out. tests/corpus_parity2.rs replays the cases natively.
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
TESTONLY = '--incompatible_check_testonly_for_output_files'

# name: (startup, command, flags, targets, stderr patterns, outputs[, workspace])
CASES = {
    'testonly_output_default': (
        [], 'build', BUILD, ['//testonly:use_output', '//testonly:use_alias'], [],
        ['use_output.txt', 'use_alias.txt']),
    'testonly_output_checked': (
        [], 'build', BUILD + [TESTONLY], ['//testonly:use_output'],
        [r'depends on the output file'], []),
    'testonly_output_alias_checked': (
        [], 'build', BUILD + [TESTONLY], ['//testonly:use_alias'],
        [r'depends on the output file'], []),
    'testonly_output_testonly_consumer': (
        [], 'build', BUILD + [TESTONLY], ['//testonly:use_test_output'], [],
        ['use_test_output.txt']),
    'minimum_os_version_default': ([], 'build', BUILD, ['//minos:show'], [], ['show.txt']),
    'minimum_os_version_flag': (
        [], 'build', BUILD + ['--minimum_os_version=10.1'], ['//minos:show'], [], ['show.txt']),
    'minimum_os_version_transition': (
        [], 'build', BUILD, ['//minos:set', '//minos:unset', '//minos:read'], [], ['show.txt']),
    'minimum_os_version_transition_from_flag': (
        [], 'build', BUILD + ['--minimum_os_version=7'], ['//minos:unset', '//minos:read'], [],
        ['show.txt']),
    'self_source_label': (
        [], 'build', BUILD, ['//selfsrc:self', '//selfsrc:self_label', '//selfsrc:via_filegroup'],
        [], ['self.txt', 'self_label.txt', 'via_filegroup.txt']),
    'self_source_label_query': ([], 'query', [], ['//selfsrc:all + //selfsrc:*'], [], []),
    'flag_alias_agreeing_values': (
        [], 'build', BUILD, ['//alias:false_false', '//alias:true_one', '//alias:false_zero'], [],
        ['show.txt']),
    'flag_alias_disagreeing_values': (
        [], 'build', BUILD, ['//alias:mismatch'], [r'have different values'], []),
    'local_repository_symlinked_root': (
        [], 'build', BUILD, ['@ext//:g'], [], ['g.out'], 'local_repo'),
    'local_repository_query': (
        [], 'query', [], ['@ext//...'], [], [], 'local_repo'),
    'android_common_toplevel': ([], 'build', BUILD, ['//android:show'], [], ['show.txt']),
    'exec_starlark_flags_default': (
        [], 'build', BUILD + ['--//execflags:plain=p', '--//execflags:universal=u',
                              '--//execflags:target=t'], ['//execflags:use'], [], ['tool.txt']),
    'exec_starlark_flags_excluded': (
        [], 'build', BUILD + ['--//execflags:plain=p', '--//execflags:universal=u',
                              '--//execflags:target=t',
                              '--incompatible_exclude_starlark_flags_from_exec_config'],
        ['//execflags:use'], [], ['tool.txt']),
    'exec_starlark_flags_excluded_old_name': (
        [], 'build', BUILD + ['--//execflags:plain=p',
                              '--experimental_exclude_starlark_flags_from_exec_config'],
        ['//execflags:use'], [], ['tool.txt']),
    'apple_toolchain_resolution': (
        [], 'build', BUILD + ['--incompatible_enable_apple_toolchain_resolution'],
        ['//execflags:use'], [], ['tool.txt']),
    'explicit_java_test_deps_default': (
        [], 'build', BUILD, ['//third_party/bazel_rules/rules_java/java:explicit'], [],
        ['explicit.txt']),
    'explicit_java_test_deps': (
        [], 'build', BUILD + ['--explicit_java_test_deps'],
        ['//third_party/bazel_rules/rules_java/java:explicit'], [], ['explicit.txt']),
    'implicit_file_export_default': (
        [], 'build', BUILD, ['//files/b:exported', '//files/b:implicit', '//files/a:local'], [],
        ['exported.out', 'implicit.out']),
    'no_implicit_file_export_exported': (
        [], 'build', BUILD + ['--incompatible_no_implicit_file_export'],
        ['//files/b:exported', '//files/a:local'], [], ['exported.out']),
    'no_implicit_file_export_implicit': (
        [], 'build', BUILD + ['--incompatible_no_implicit_file_export'], ['//files/b:implicit'],
        [r'is not visible from'], []),
    'no_implicit_file_export_info': (
        [], 'info', ['--incompatible_no_implicit_file_export'], ['starlark-semantics'], [], []),
    'rules_cc_main_private_initializer': (
        [], 'build', BUILD, ['//use:t'], [], ['t.txt'], 'rules_cc_main'),
    'other_main_private_initializer': (
        [], 'build', BUILD, ['//use:t'], [r'cannot use private API'], [], 'other_main'),
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
        for top, _, names in os.walk(out):
            for file in names:
                if file in outputs:
                    path = os.path.join(top, file)
                    files[os.path.relpath(path, out)] = open(path).read()
        cases[name] = dict(workspace=source, startup=startup, command=command, flags=flags,
                           targets=targets,
                           exit=result.returncode, stdout=result.stdout, stderr=matched,
                           outputs=files)
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases), sys.stdout, indent=1,
              sort_keys=True)
    print()


if __name__ == '__main__':
    main()
