#!/usr/bin/env python3
"""Regenerate src/command_options.rs from genuine Bazel 9.3.0 `help flags-as-proto`.

Usage:
  extract_command_options.py --bazel /path/to/bazel-9.3.0 --scratch DIR > src/command_options.rs
  extract_command_options.py --from-base64 FLAGS.b64 > src/command_options.rs

The binary's SHA-256 must be the pinned genuine Bazel 9.3.0 one, and the
captured (or supplied) base64 output must have the pinned SHA-256; the script
refuses to generate otherwise.

`bazel help flags-as-proto` (runtime/commands/HelpCommand.java:295-322) emits a
base64 FlagCollection (src/main/protobuf/bazel_flags.proto) with every
non-INTERNAL option of every command and the commands that register it; it is
the per-command option membership BlazeOptionHandler consults for `common`
rc options (OptionsParser.getFallbackOptionsData over the options classes of
every command, runtime/BlazeOptionHandler.java:128-137, 219-245). The genuine
binary is run with `--batch`, `--ignore_all_rc_files`, a private
`--output_user_root` and HOME under the given scratch directory, which is
removed afterwards. Only the parsing shape is kept: name, old name,
abbreviation, boolean syntax (OptionDefinition.usesBooleanValueSyntax),
requiresValue, and membership in build/query/mod/any other command.
"""

import argparse
import base64
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile

REFERENCE_SHA256 = 'd302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8'
# sha256 of the stripped base64 `help flags-as-proto` output of that binary.
FLAGS_SHA256 = '42691ca192f8dbc093ee2dd746adf3550a19c305e8caa32034ca3b6c0fdfe5c0'
MODELLED = ('build', 'query', 'mod')


def varint(data, index):
    result = shift = 0
    while True:
        byte = data[index]
        index += 1
        result |= (byte & 0x7f) << shift
        shift += 7
        if byte < 0x80:
            return result, index


def fields(data):
    index = 0
    while index < len(data):
        key, index = varint(data, index)
        number, wire = key >> 3, key & 7
        if wire == 0:
            value, index = varint(data, index)
        elif wire == 2:
            length, index = varint(data, index)
            value = data[index:index + length]
            index += length
        else:
            raise ValueError(f'unexpected wire type {wire}')
        yield number, value


def decode(encoded):
    flags = []
    for number, value in fields(base64.b64decode(encoded.strip(), validate=True)):
        if number != 1:
            raise ValueError('not a FlagCollection')
        flag = dict(commands=[], negative=False, requires_value=False, abbrev='', old_name='')
        for field, item in fields(value):
            if field == 1:
                flag['name'] = item.decode()
            elif field == 2:
                flag['negative'] = bool(item)
            elif field == 4:
                flag['commands'].append(item.decode())
            elif field == 5:
                flag['abbrev'] = item.decode()
            elif field == 10:
                flag['requires_value'] = bool(item)
            elif field == 11:
                flag['old_name'] = item.decode()
        flags.append(flag)
    return flags


def verify_binary(bazel):
    with open(bazel, 'rb') as stream:
        digest = hashlib.sha256(stream.read()).hexdigest()
    if digest != REFERENCE_SHA256:
        raise SystemExit(f'refusing to generate: {bazel} has sha256 {digest}, '
                         f'not the pinned genuine Bazel 9.3.0 binary {REFERENCE_SHA256}')


def capture(bazel, scratch):
    verify_binary(bazel)
    work = tempfile.mkdtemp(prefix='flags-', dir=scratch)
    try:
        home = os.path.join(work, 'home')
        os.mkdir(home)
        env = dict(HOME=home, PATH='/usr/bin:/bin', TMPDIR=work, LANG='C.UTF-8')
        result = subprocess.run(
            [bazel, '--batch', '--ignore_all_rc_files', '--output_user_root=' + os.path.join(work, 'root'),
             'help', 'flags-as-proto'],
            cwd=home, env=env, check=True, capture_output=True, timeout=600)
        return result.stdout.decode()
    finally:
        shutil.rmtree(work, ignore_errors=True)


def rust_string(text):
    return '"' + text.replace('\\', '\\\\').replace('"', '\\"') + '"'


def render(flags, digest):
    commands = sorted({command for flag in flags for command in flag['commands']} - {'startup'})
    rows = []
    for flag in sorted(flags, key=lambda f: f['name']):
        member = set(flag['commands']) - {'startup'}
        if not member:
            continue  # startup-only: not in any command's options data
        mask = 0
        for bit, command in enumerate(MODELLED):
            if command in member:
                mask |= 1 << bit
        if member - set(MODELLED):
            mask |= 1 << len(MODELLED)
        abbrev = repr(flag['abbrev']) if flag['abbrev'] else "'\\0'"
        rows.append('    ({}, {}, {}, {}, {}, {}),'.format(
            rust_string(flag['name']), rust_string(flag['old_name']), abbrev,
            'true' if flag['negative'] else 'false',
            'true' if flag['requires_value'] else 'false', mask))
    out = [
        '//! Generated by scripts/extract_command_options.py from genuine Bazel 9.3.0',
        f'//! (sha256 {REFERENCE_SHA256}) `help flags-as-proto`',
        f'//! (base64 sha256 {digest}); do not edit.',
        '//! Every non-INTERNAL command option (runtime/commands/HelpCommand.java:295-322):',
        '//! (name, old name or "", abbreviation or NUL, boolean syntax, requires a value,',
        '//! command mask: 1 build, 2 query, 4 mod, 8 any other command).',
        '#[rustfmt::skip]',
        'pub(crate) const COMMAND_OPTIONS: &[(&str, &str, char, bool, bool, u8)] = &[',
        *rows,
        '];',
        '',
        '/// The Bazel 9.3.0 commands (BlazeRuntime.getCommandMap keys).',
        '#[rustfmt::skip]',
        'pub(crate) const COMMANDS: &[&str] = &[',
        *('    {},'.format(rust_string(command)) for command in commands),
        '];',
        '',
    ]
    return '\n'.join(out)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bazel')
    parser.add_argument('--scratch')
    parser.add_argument('--from-base64')
    args = parser.parse_args()
    if args.from_base64:
        encoded = open(args.from_base64).read()
    elif args.bazel and args.scratch:
        encoded = capture(args.bazel, args.scratch)
    else:
        parser.error('--from-base64 or --bazel with --scratch is required')
    digest = hashlib.sha256(encoded.strip().encode()).hexdigest()
    if digest != FLAGS_SHA256:
        raise SystemExit(f'refusing to generate: flags-as-proto output sha256 {digest} is not the pinned '
                         f'output {FLAGS_SHA256} of genuine Bazel 9.3.0 {REFERENCE_SHA256}')
    sys.stdout.write(render(decode(encoded), digest))


if __name__ == '__main__':
    main()
