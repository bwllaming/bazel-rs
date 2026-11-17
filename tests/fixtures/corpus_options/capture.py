#!/usr/bin/env python3
"""Capture tests/fixtures/corpus_options/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

The rc/command-line options the open-source corpus sets (docs/OSS_CORPUS.md
B2, B11, B12; docs/RC_OPTIONS.md "Options of the OSS corpus"). Each case runs
once in a fresh copy of workspace/ (--batch, no rc files, private HOME and
output user root) and records the exit code, the stdout, the stderr lines
matching the case's patterns (the workspace path replaced by <WS>, a
`(HH:mm:ss) ` prefix by `(TIME) `) and the content of the named outputs by
their path below bazel-out. tests/corpus_options.rs replays the cases natively.
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

ACCEPTED = [
    '--http_timeout_scaling=2.5', '--http_connector_attempts=3', '--terminal_columns=143',
    '--grpc_keepalive_time=30s', '--noexperimental_check_external_repository_files',
    '--noexperimental_merged_skyframe_analysis_execution',
    '--incompatible_use_cc_configure_from_rules_cc', '--incompatible_enforce_starlark_utf8=error',
    '--incompatible_default_to_explicit_init_py', '--module_mirrors=',
]

# name: (startup, command, flags, targets, stderr patterns, outputs)
CASES = {
    'startup_jvm_and_windows': (
        ['--host_jvm_args=-Dx=1', '--host_jvm_args=-DBAZEL_TRACK_SOURCE_DIRECTORIES=1',
         '--windows_enable_symlinks'], 'build', BUILD, ['//:g'], [], ['g.txt']),
    'accepted_options': ([], 'build', BUILD + ACCEPTED, ['//:g'], [], ['g.txt']),
    'default_output_dirs': ([], 'build', BUILD, ['//:g'], [], ['g.txt']),
    'platform_in_output_dir': (
        [], 'build', BUILD + ['--experimental_platform_in_output_dir'], ['//:g'], [], ['g.txt']),
    'show_timestamps': (
        [], 'build', BUILD + ['--show_timestamps'], ['//:g'],
        [r'^\(TIME\) INFO: Current date is \d{4}-\d\d-\d\d$',
         r'^\(TIME\) INFO: Build completed successfully'], ['g.txt']),
    'empty_glob_default': (
        [], 'build', BUILD, ['//empty_default:e'], [r"didn't match anything"], []),
    'empty_glob_off': (
        [], 'build', BUILD + ['--noincompatible_disallow_empty_glob'], ['//empty_default:e'],
        [], []),
    'empty_glob_false': (
        [], 'build', BUILD + ['--incompatible_disallow_empty_glob=false'],
        ['//empty_default:e'], [], []),
    'empty_glob_off_explicit': (
        [], 'build', BUILD + ['--noincompatible_disallow_empty_glob'], ['//empty_explicit:e'],
        [r"didn't match anything"], []),
    'config_setting_public_default': (
        [], 'build', BUILD + ['--nobuild'], ['//vis/b:g'], [], []),
    'config_setting_private_default': (
        [], 'build', BUILD + ['--nobuild', '--incompatible_config_setting_private_default_visibility'],
        ['//vis/b:g'], [r'is not visible from'], []),
    'distdir_and_strip_prefix_slash': (
        [], 'build', BUILD + ['--distdir=missing', '--distdir=dist'], ['//:arch'],
        [r'non-existent distdir'], ['arch.txt']),
    'downloader_config_rewrite': (
        [], 'build', BUILD + ['--downloader_config=rewrite.cfg', '--distdir=dist'], ['//:arch'],
        [], ['arch.txt']),
    'old_names_warn': (
        [], 'build', BUILD + ['--experimental_downloader_config=rewrite.cfg',
                              '--experimental_distdir=dist'], ['//:arch'],
        [r"Option 'experimental_downloader_config' is deprecated",
         r"Option 'experimental_distdir' is deprecated"], ['arch.txt']),
    'downloader_config_blocks': (
        [], 'build', BUILD + ['--downloader_config=block.cfg'], ['//:arch'],
        [r'Cache miss and no url specified'], []),
    'downloader_config_parse_error': (
        [], 'build', BUILD + ['--downloader_config=bad.cfg'], ['//:g'],
        [r'Failed to parse downloader config'], []),
    'downloader_config_missing': (
        [], 'build', BUILD + ['--downloader_config=missing.cfg'], ['//:g'],
        [r'Failed to parse downloader config'], []),
    'module_mirrors_unknown_registry': (
        [], 'build', BUILD + ['--module_mirrors=https://x.invalid=https://m.invalid'], ['//:g'],
        [r'--module_mirrors references registries'], []),
    'unknown_repository_load': (
        [], 'build', BUILD + ['--keep_going'], ['//unknown_repo:u'],
        [r'Unable to find package for'], []),
    'info_starlark_semantics': (
        [], 'info', ['--noincompatible_disallow_empty_glob',
                     '--incompatible_enforce_starlark_utf8=error',
                     '--incompatible_use_cc_configure_from_rules_cc'],
        ['starlark-semantics'], [], []),
    'query_profile_options': (
        [], 'query', ['--noslim_profile', '--experimental_profile_include_target_label',
                      '--experimental_profile_include_primary_output'], ['//:g'], [], []),
}

CONVERTER_ERRORS = [
    '--http_timeout_scaling=x', '--http_connector_attempts=x', '--terminal_columns=x',
    '--grpc_keepalive_time=x', '--incompatible_enforce_starlark_utf8=bogus',
    '--module_mirrors=a,,b', '--experimental_platform_in_output_dir=bogus',
]
for flag in CONVERTER_ERRORS:
    CASES['converter ' + flag] = ([], 'build', BUILD + [flag], ['//:g'], [r'While parsing option'],
                                  [])

TIME = re.compile(r'^\(\d\d:\d\d:\d\d\) ')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    cases = {}
    for index, (name, (startup, command, flags, targets, patterns, outputs)) in enumerate(
            CASES.items()):
        root = os.path.join(scratch, str(index))
        workspace = os.path.join(root, 'ws')
        shutil.copytree(os.path.join(HERE, 'workspace'), workspace)
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
        cases[name] = dict(startup=startup, command=command, flags=flags, targets=targets,
                           exit=result.returncode, stdout=result.stdout, stderr=matched,
                           outputs=files)
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases), sys.stdout, indent=1,
              sort_keys=True)
    print()


if __name__ == '__main__':
    main()
