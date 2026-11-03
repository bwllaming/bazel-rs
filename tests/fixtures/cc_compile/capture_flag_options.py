#!/usr/bin/env python3
"""Capture tests/fixtures/cc_compile/flag_options_receipt.json with genuine Bazel.

Usage: capture_flag_options.py --bazel /path/to/bazel-9.3.1 --scratch DIR > flag_options_receipt.json

Sets up the workspace exactly like tests/cc_compile.rs (Workspace::new: the
fixture, the shared vendored modules, the stubs, the generated prebuilt
libraries), builds //bin:tool with the C++ configuration flag options and
`-s` (--batch, no rc files, private HOME and output user root) and records
the compile command lines and the link parameter file, sorted.
"""

import argparse
import glob
import hashlib
import json
import os
import shutil
import subprocess
import sys

MANIFEST = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))))
FIXTURE = os.path.join(MANIFEST, 'tests/fixtures/cc_compile')
STUBBED = ['buildozer', 'rules_java', 'rules_python', 'rules_shell', 'zlib']
VENDORED = ['rules_cc', 'bazel_skylib', 'platforms', 'bazel_features', 'package_metadata',
            'apple_support', 'hermetic_cc_toolchain']

FLAGS = ['--copt=-DCOPT', '--conlyopt=-DCONLY', '--cxxopt=-DCXX', '--linkopt=-lm',
         '--per_file_copt=bin/lib@-DPERFILE,-DPF\\,2', '--features=-default_compile_flags',
         '--dynamic_mode=off']


def setup(root):
    shutil.copytree(os.path.join(FIXTURE, 'workspace'), root, symlinks=True)
    toolchain = os.path.join(MANIFEST, 'tests/fixtures/cc_toolchain')
    shared = os.path.join(MANIFEST, 'tests/fixtures/cc_features/vendor')
    for relative in open(os.path.join(toolchain, 'vendor_shared.txt')).read().splitlines():
        target = os.path.join(root, 'vendor', relative)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copy(os.path.join(shared, relative), target)
    shutil.copytree(os.path.join(toolchain, 'vendor'), os.path.join(root, 'vendor'),
                    dirs_exist_ok=True)
    shutil.copytree(os.path.join(FIXTURE, 'workspace/vendor'), os.path.join(root, 'vendor'),
                    dirs_exist_ok=True)
    module = ('module(name = "cc_compile_fixture")\n\n'
              'bazel_dep(name = "rules_cc", version = "0.2.17")\n'
              'bazel_dep(name = "platforms", version = "1.1.0")\n'
              'bazel_dep(name = "bazel_skylib", version = "1.9.0")\n'
              'bazel_dep(name = "hermetic_cc_toolchain", version = "4.1.0")\n')
    for name in VENDORED:
        module += f'local_path_override(module_name = "{name}", path = "vendor/{name}")\n'
    stubs = os.path.join(MANIFEST, 'tests/fixtures/test_flags/stubs')
    for name in STUBBED:
        module += (f'local_path_override(module_name = "{name}", '
                   f'path = "{os.path.join(stubs, name)}")\n')
    module += ('local_path_override(module_name = "protobuf", '
               f'path = "{os.path.join(toolchain, "stubs/protobuf")}")\n')
    open(os.path.join(root, 'MODULE.bazel'), 'w').write(module)
    env = dict(PATH='/usr/bin:/bin', LC_ALL='C', LANG='C')
    cdep = os.path.join(root, 'cdep')
    for argv in (['gcc', '-c', '-O0', '-g0', 'prebuilt.c', '-o', 'prebuilt.o'],
                 ['ar', 'rcD', 'libprebuilt.a', 'prebuilt.o'],
                 ['rm', 'prebuilt.o'],
                 ['gcc', '-shared', '-O0', '-g0', '-Wl,-build-id=none',
                  '-Wl,-soname=libprebuilt.so', 'prebuilt.c', '-o', 'libprebuilt.so']):
        subprocess.run(argv, cwd=cdep, env=env, check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel', required=True)
    parser.add_argument('--scratch', required=True)
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    shutil.rmtree(scratch, ignore_errors=True)
    root = os.path.join(scratch, 'ws')
    setup(root)
    home = os.path.join(scratch, 'home')
    os.makedirs(home)
    registry = 'file://' + os.path.join(MANIFEST, 'tests/fixtures/builtin_modules/registry')
    argv = [args.bazel, '--batch', '--ignore_all_rc_files', f'--output_user_root={scratch}/our',
            'build', f'--registry={registry}', '-s'] + FLAGS + ['//bin:tool']
    result = subprocess.run(argv, cwd=root, capture_output=True, text=True,
                            env=dict(PATH='/usr/bin:/bin', HOME=home, TMPDIR=home, LC_ALL='C'))
    compiles = sorted(line.strip() for line in result.stderr.splitlines()
                      if line.startswith('  /usr/bin/gcc') and ' -c ' in line)
    params = glob.glob(f'{scratch}/our/*/execroot/_main/bazel-out/k8-fastbuild/bin/bin/tool-0.params')
    digest = hashlib.sha256(open(args.bazel, 'rb').read()).hexdigest()
    json.dump(dict(bazel='9.3.1', elf_sha256=digest, flags=FLAGS, exit=result.returncode,
                   compiles=compiles,
                   link_params=open(params[0]).read().splitlines() if params else None),
              sys.stdout, indent=1)
    print()


if __name__ == '__main__':
    main()
