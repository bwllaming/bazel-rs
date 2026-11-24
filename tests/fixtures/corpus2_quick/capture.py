#!/usr/bin/env python3
"""Capture tests/fixtures/corpus2_quick/reference_receipt.json with genuine Bazel.

Usage: capture.py --bazel /path/to/bazel-9.3.1 --scratch DIR > reference_receipt.json

Parity cases of the second-batch open-source corpus bugs fixed on branch
corpus2-quick (docs/OSS_CORPUS.md, B59-B71). Each case runs once in a fresh
copy of its workspace (--batch, private HOME and output user root; no rc
files unless the case reads the workspace's .bazelrc) and records the exit
code, the stdout, the stderr lines matching the case's patterns (the
workspace path replaced by <WS>) and the content of the named outputs by
their path below bazel-out. tests/corpus2_quick.rs replays the cases natively.
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


def case(command, flags, targets, patterns=(), outputs=(), workspace='workspace', env=None, rc=False,
         module_file=None, lockfile=False):
    return dict(command=command, flags=flags, targets=targets, patterns=list(patterns),
                outputs=list(outputs), workspace=workspace, env=env or {}, rc=rc, module_file=module_file,
                lockfile=lockfile)


CASES = {
    # B59: --macos_minimum_os/--host_macos_minimum_os, --protocopt.
    'apple_and_proto_options': case(
        'build', BUILD + ['--macos_minimum_os=10.15', '--protocopt=--fatal_warnings', '--protocopt=-a'],
        ['//opts:f', '//opts:s', '//opts:t'], outputs=['f.txt', 's.txt']),
    'host_macos_minimum_os': case(
        'build', BUILD + ['--host_macos_minimum_os=12.1'], ['//opts:f'], outputs=['f.txt']),
    # B59: --client_env from an rc file under the client environment, from
    # the command line over it.
    'client_env_rc': case(
        'build', [], ['//:e'], outputs=['e.txt'], workspace='rcws', env={'FOO': 'real'}, rc=True),
    'client_env_command_line': case(
        'build', ['--client_env=BAR=cli', '--client_env=FOO=cli'], ['//:e'], outputs=['e.txt'],
        workspace='rcws', env={'FOO': 'real'}, rc=True),
    'client_env_invalid': case(
        'build', ['--client_env=CC'], ['//:e'], patterns=[r'client_env'], workspace='rcws', rc=True),
    # B59: --noincompatible_enable_proto_toolchain_resolution.
    'proto_toolchain_resolution_default': case(
        'build', BUILD + ['--nobuild'], ['//proto:p'], patterns=[r'PROTO']),
    'proto_toolchain_resolution_off': case(
        'build', BUILD + ['--nobuild', '--noincompatible_enable_proto_toolchain_resolution'], ['//proto:p'],
        patterns=[r'PROTO']),
    # B60: config.string(allow_multiple = True).
    'allow_multiple_default': case(
        'build', BUILD, ['//am:s', '//am:ap', '//am:re', '//am:ap2'], patterns=[r'IN \['], outputs=['s.txt', 's2.txt']),
    'allow_multiple_repeated': case(
        'build', BUILD + ['--//am:f=a', '--//am:f=b'], ['//am:s', '//am:ap', '//am:re', '//am:ap2'],
        patterns=[r'IN \['], outputs=['s.txt', 's2.txt']),
    'allow_multiple_comma': case(
        'build', BUILD + ['--//am:f=a,b'], ['//am:s'], outputs=['s.txt']),
    'allow_multiple_at_default': case(
        'build', BUILD + ['--//am:f=d'], ['//am:s', '//am:re'], outputs=['s.txt']),
    'allow_multiple_single_value_transition': case(
        'build', BUILD, ['//am/err:si'], patterns=[r'allows multiple values']),
    # B64: ctx.resolve_command.
    'resolve_command': case(
        'build', BUILD, ['//rc:t1', '//rc:t2', '//rc:t3'], outputs=['t1.txt', 't2.txt', 't3.txt']),
    'resolve_command_errors': case(
        'build', BUILD + ['--keep_going'], ['//rc:t4', '//rc:t5', '//rc:t6'],
        patterns=[r'in cmd attribute of r rule //rc:t4', r'in r rule //rc:t5', r'in r rule //rc:t6']),
    # B65: Args formats a bool as Java's Boolean.toString (and the action key).
    'args_booleans': case('build', BUILD, ['//ab:t'], outputs=['t.txt']),
    'args_booleans_action_key': case('aquery', BUILD, ['//ab:t']),
    # B68: --compiler/--host_compiler (config_setting, transitions, exec).
    'compiler_default': case('build', BUILD, ['//cmp:s'], outputs=['s.txt']),
    'compiler_set': case('build', BUILD + ['--compiler=clang'], ['//cmp:s'], outputs=['s.txt']),
    'compiler_transition_and_exec': case(
        'build', BUILD + ['--compiler=clang', '--host_compiler=hostc'], ['//cmp:f'], patterns=[r'READ'],
        outputs=['s.txt']),
    # B70: a testonly violation under --allow_analysis_failures is an
    # AnalysisFailureInfo cause, with the edge's other prerequisite errors.
    'testonly_analysis_failure': case(
        'test', BUILD, ['//to:check'], patterns=[r'CAUSE //to:lib non-test']),
    'testonly_analysis_failure_with_other_errors': case(
        'test', BUILD, ['//to:check2'], patterns=[r'CAUSE //to:lib2 non-test', r'CAUSE //to:lib2 source file',
                                                 r'CAUSE //to:lib2 .//to:a.y. does not produce']),
    # B71: the test summary names a test in the main repository's display form.
    'test_summary_display_form': case('test', [], ['//:all'], workspace='tlws'),
    # B72: an extension-generated repository with a large attribute (no
    # native host budget on extension results).
    'extension_repository_large_attribute': case('build', [], ['//:t'], workspace='bigws'),
    # B63: include() in the root MODULE.bazel; the lockfile's extension
    # entry (usages digest) is compared too.
    'include_segments': case(
        'build', [], ['//:t', '@r//:x'], patterns=[r'in a$', r'in b$', r'in dev$', r'TAG incws from-a',
                                                   r'TAG incws from-root'],
        workspace='incws', lockfile=True),
    'include_ignore_dev_dependency': case(
        'build', ['--ignore_dev_dependency'], ['//:t', '@r//:x'], patterns=[r'in a$', r'TAG incws from-a'],
        workspace='incws'),
    'include_bad_label': case(
        'build', [], ['//:t'], patterns=[r'bad include label'], workspace='incws',
        module_file='module(name = "incws")\ninclude("sub:a.MODULE.bazel")\n'),
    'include_no_package': case(
        'build', [], ['//:t'], patterns=[r'unable to load package'], workspace='incws',
        module_file='module(name = "incws")\ninclude("//nob:x.MODULE.bazel")\n'),
    'include_not_a_file': case(
        'build', [], ['//:t'], patterns=[r'error reading'], workspace='incws',
        module_file='module(name = "incws")\ninclude("//sub:none.MODULE.bazel")\n'),
    'include_misused': case(
        'build', [], ['//:t'], patterns=[r'MUST be called directly at the top-level'], workspace='incws',
        module_file='module(name = "incws")\nx = include\n'),
}

TIME = re.compile(r'^\(\d\d:\d\d:\d\d\) ')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    parser.add_argument('--only', default='')
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    cases = {}
    for index, (name, spec) in enumerate(CASES.items()):
        if args.only and name not in args.only.split(','):
            continue
        root = os.path.join(scratch, str(index))
        workspace = os.path.join(root, 'ws')
        shutil.copytree(os.path.join(HERE, spec['workspace']), workspace)
        if spec['module_file'] is not None:
            with open(os.path.join(workspace, 'MODULE.bazel'), 'w') as module_file:
                module_file.write(spec['module_file'])
        home = os.path.join(root, 'home')
        os.makedirs(home)
        env = dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home, **spec['env'])
        rc = ['--nohome_rc', '--nosystem_rc'] if spec['rc'] else ['--ignore_all_rc_files']
        lockfile = [] if spec['lockfile'] else ['--lockfile_mode=off']
        argv = ([args.bazel, '--batch'] + rc + [f'--output_user_root={root}/our', spec['command']]
                + lockfile + spec['flags'] + spec['targets'])
        result = subprocess.run(argv, cwd=workspace, env=env, capture_output=True, text=True)
        lines = [TIME.sub('(TIME) ', line.replace(workspace, '<WS>'))
                 for line in result.stderr.splitlines()]
        matched = []
        for pattern in spec['patterns']:
            found = [line for line in lines if re.search(pattern, line)]
            matched.append(dict(pattern=pattern, line=found[0] if found else None))
        files = {}
        out = os.path.join(workspace, 'bazel-out')
        for top, dirs, names in os.walk(out):
            dirs[:] = [d for d in dirs if not d.endswith('.runfiles')]
            for file in names:
                if file in spec['outputs']:
                    path = os.path.join(top, file)
                    files[os.path.relpath(path, out)] = open(path).read()
        extensions = None
        if spec['lockfile']:
            with open(os.path.join(workspace, 'MODULE.bazel.lock')) as lock:
                extensions = json.load(lock)['moduleExtensions']
        cases[name] = dict(spec, exit=result.returncode, stdout=result.stdout, stderr=matched, outputs=files,
                           module_extensions=extensions)
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, cases=cases), sys.stdout, indent=1, sort_keys=True)
    print()


if __name__ == '__main__':
    main()
