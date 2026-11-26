#!/usr/bin/env python3
"""Capture tests/fixtures/corpus_parity7/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

Parity cases of the open-source corpus bugs fixed on branch parity-b83
(docs/OSS_CORPUS.md). Each case runs once in a fresh copy of workspace/
(--batch, no rc files, private HOME and output user root) and records the
exit code, the stdout, the stderr lines matching the case's patterns (the
workspace path replaced by <WS>) and the content of the named outputs by
their path below bazel-out. tests/corpus_parity7.rs replays the cases natively.
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
# name: (startup, command, flags, targets, stderr patterns, outputs[, workspace[, bin_paths]])
CASES = {
    # B83: a repository rule's REPO.bazel left as a symlink (and a dangling one).
    'repository_boundary_symlink': (
        [], 'build', [], ['@linked//:t', '@linked//:g', '@dangling//:t', '@dangling//:g', '@foreign//:t', '@foreign//:g'], [], ['g.txt']),
    # B86: an output below a source directory linked from another repository.
    'output_below_linked_source_directory': (
        [], 'build', [], ['@overlay//:o'], [], ['gen.txt']),
    # B84: platform(flags = ...) on transitioned configurations.
    'platform_flags_after_transition': (
        [], 'build', [], ['//pf:on_glibc', '//pf:on_child', '//pf:on_reset'], [], ['show.txt']),
    # B84: the top-level target platform's flags.
    'platform_flags_top_level': (
        [], 'build', ['--platforms=//pf:glibc'], ['//pf:show'], [], ['show.txt']),
    'platform_flags_inherited_top_level': (
        [], 'build', ['--platforms=//pf:child'], ['//pf:show'], [], ['show.txt']),
    # B67: an aspect reads ctx.var in a configuration whose target platform
    # nothing resolved before it (the default host platform).
    'aspect_reads_unresolved_target_platform': (
        [], 'build', [], ['//aspectvar:report', '//aspectvar:check'], [], ['report.txt', 'check.sh']),
    # B85: an aspect's ctx.rule.toolchains under automatic exec groups (and
    # without them).
    'aspect_rule_toolchains_auto_exec_groups': (
        [], 'build', ['--incompatible_auto_exec_groups'], ['//aegtc:report'], [], ['report.txt']),
    'aspect_rule_toolchains': (
        [], 'build', ['--noincompatible_auto_exec_groups'], ['//aegtc:report'], [], ['report.txt']),
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
        # symlinks), or None when nothing resolves there; `P?`: whether P exists.
        tree = {}
        for relative in bin_paths:
            path = os.path.join(workspace, 'bazel-bin', relative.rstrip('?'))
            if relative.endswith('?'):
                # Existence only (a manifest names absolute paths).
                tree[relative] = 'exists' if os.path.exists(path) else None
            elif os.path.islink(path):
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
