#!/usr/bin/env python3
"""Probe every genuine Bazel 9.3 option of a command through bazel-rs.

Usage:
  option_sweep.py --bazel-rs PATH --scratch DIR [--commands build,test,run,query,cquery]
                  [--only NAME,...] [--markdown]

The option names, boolean syntax, converters and command membership come
from src/option_metadata.rs (genuine `help flags-as-proto`); the defaults
from src/help_topics.rs (genuine `help <command> --long`) and, for
undocumented options, src/option_source_defaults.rs. Each option runs
once with its default value (booleans: `--name` and `--noname`; list
options without a default: a plausible converter value) on a one-target
workspace (`build --nobuild //:x`, `query //:x`, ...), and the probes that
bazel-rs rejects as not modelled (`options are never silently ignored`,
`unsupported option`, `not modelled`) are listed. A value error (exit 2
from the option's converter) is reported separately: the option is
recognized. Startup-only options are skipped. Each row says whether the
probe spelled the genuine default (`default`) or another value (`other`).
Exit 0 always; the report goes to stdout (a tab-separated table, or
Markdown with --markdown).
"""

import argparse
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

COMMANDS = ['aquery', 'build', 'canonicalize-flags', 'clean', 'config', 'coverage', 'cquery',
            'dump', 'fetch', 'help', 'info', 'license', 'mobile-install', 'mod', 'print_action',
            'query', 'run', 'shutdown', 'test', 'vendor', 'version', 'startup']

# A plausible value per converter (the option's documented syntax).
CONVERTER_VALUES = {
    'String': 'x',
    'Integer': '1',
    'NonNegativeInteger': '1',
    'Long': '1',
    'Double': '1.0',
    'Duration': '1s',
    'RemoteDuration': '1s',
    'PathFragment': '/tmp/x',
    'EmptyToNullPathFragment': '/tmp/x',
    'AbsolutePathFragment': '/tmp/x',
    'Label': '//:x',
    'EmptyToNullLabel': '//:x',
    'LabelList': '//:x',
    'LabelOrderedSet': '//:x',
    'CommaSeparatedOptionList': 'x',
    'CommaSeparatedNonEmptyOptionList': 'x',
    'CommaSeparatedOptionSet': 'x',
    'Assignment': 'x=y',
    'StringToStringList': 'x=y',
    'EnvVars': 'X=y',
    'RegexPattern': 'x',
    'RegexFilter': '-x',
    'PerLabelOptions': '//x@-y',
    'Percentage': '50',
    'OptionalPercentage': '50',
    'RamResource': 'HOST_RAM*.5',
    'CpuResource': 'HOST_CPUS',
    'MultiResource': 'cpu=2',
    'DynamicMode': 'off',
    'TriState': 'auto',
    'DottedVersion': '1.0',
    'ProgressReportInterval': '10',
    'MaxStdoutErrBytes': '1048576',
    'ConvenienceSymlinks': 'normal',
    'LocalTestJobs': '2',
    'StrictDeps': 'default',
}

# Per-option values where the converter value is not plausible.
NAME_VALUES = {
    'modify_execution_info': 'Genrule=+no-remote',
    'symlink_prefix': '/',
    'output_filter': '^//',
    'auto_output_filter': 'packages',
    'toolchain_resolution_debug': '.*',
    'per_file_copt': '//x@-O2',
    'host_per_file_copt': '//x@-O2',
    'features': 'race',
    'host_features': 'race',
    'local_resources': 'cpu=2',
    'output': 'label',
    'color': 'no',
    'curses': 'no',
}

REJECTED = re.compile(r'options are never silently ignored|unsupported option|not modelled|'
                      r'is not implemented|only the Bazel 9\.3 default')


def parse_metadata():
    text = open(os.path.join(ROOT, 'src/option_metadata.rs'), encoding='utf-8').read()
    options = []
    for match in re.finditer(r'O \{ name: "([^"]+)", abbrev: \'[^\']*\', boolean: (true|false), '
                             r'multiple: (true|false), converter: "([^"]*)", expansion: '
                             r'&\[([^\]]*)\], commands: (0x[0-9a-f]+) \}', text):
        name, boolean, multiple, converter, expansion, commands = match.groups()
        options.append(dict(name=name, boolean=boolean == 'true', multiple=multiple == 'true',
                            converter=converter, expansion=bool(expansion.strip()),
                            commands=int(commands, 16)))
    return options


def parse_defaults():
    text = open(os.path.join(ROOT, 'src/help_topics.rs'), encoding='utf-8').read()
    defaults = {}
    for match in re.finditer(r'\("(?:\[no\])?([a-z0-9_]+)(?: \[-.\])? \((.*?)\)", "[A-Z_]+", 0x', text):
        name, description = match.groups()
        default = re.search(r'default: \\"(.*?)\\"\s*$', description)
        if default:
            defaults[name] = default.group(1).replace('\\\\', '\\')
    return defaults


def source_defaults():
    text = open(os.path.join(ROOT, 'src/option_source_defaults.rs'), encoding='utf-8').read()
    return {name: value.replace('\\\\', '\\')
            for name, value in re.findall(r'\("([a-z0-9_:]+)", "((?:[^"\\]|\\.)*)"\)', text)}


def probes(option, defaults):
    """(argument, whether it spells the genuine default) pairs."""
    name = option['name']
    default = defaults.get(name)
    if option['boolean']:
        return [(f'--{name}', default == 'true'), (f'--no{name}', default == 'false')]
    if option['expansion'] or option['converter'] == 'Void':
        return [(f'--{name}', False)]
    value = NAME_VALUES.get(name)
    if value is None:
        value = default or CONVERTER_VALUES.get(option['converter'], 'x')
    return [(f'--{name}={value}', value == default and not option['multiple'])]


def setup(scratch):
    workspace = os.path.join(scratch, 'ws')
    for directory in (workspace, os.path.join(scratch, 'home'), os.path.join(scratch, 'tmp')):
        os.makedirs(directory, exist_ok=True)
    open(os.path.join(workspace, 'MODULE.bazel'), 'w').close()
    with open(os.path.join(workspace, 'defs.bzl'), 'w') as defs:
        defs.write('def _impl(ctx):\n'
                   '    out = ctx.actions.declare_file(ctx.label.name + ".sh")\n'
                   '    ctx.actions.write(out, "#!/bin/sh\\nexit 0\\n", is_executable = True)\n'
                   '    return [DefaultInfo(executable = out)]\n\n'
                   'probe_test = rule(implementation = _impl, test = True)\n')
    with open(os.path.join(workspace, 'BUILD'), 'w') as build:
        build.write('load(":defs.bzl", "probe_test")\n\n'
                    'probe_test(name = "x")\n\n'
                    'platform(name = "p")\n')
    return workspace


def run(binary, scratch, workspace, command, arg):
    target = ['--nobuild', '//:x'] if command == 'build' else ['//:x']
    platforms = [] if command == 'query' else ['--platforms=//:p', '--host_platform=//:p']
    argv = [binary, '--ignore_all_rc_files', f'--cache-dir={scratch}/cache', command,
            '--experimental-builtin-modules=false', '--lockfile_mode=off', '--memory-limit=1024'
            ] + platforms + [arg] + target
    env = dict(PATH='/usr/bin:/bin', HOME=f'{scratch}/home', TMPDIR=f'{scratch}/tmp',
               BAZEL_RS_CACHE_GC='off')
    try:
        result = subprocess.run(argv, cwd=workspace, env=env, capture_output=True, text=True,
                                timeout=60)
    except subprocess.TimeoutExpired:
        return 'timeout', ''
    if result.returncode == 0:
        return 'ok', ''
    lines = [line for line in result.stderr.splitlines()
             if line.strip() and not line.startswith('bazel-rs: memory limit')]
    message = next((line for line in lines if REJECTED.search(line)), None)
    if message is not None:
        return 'rejected', message
    return 'error', lines[-1] if lines else f'exit {result.returncode}'


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bazel-rs', required=True)
    parser.add_argument('--scratch', required=True)
    parser.add_argument('--commands', default='build,test,run,query,cquery')
    parser.add_argument('--only', default='')
    parser.add_argument('--markdown', action='store_true')
    args = parser.parse_args()
    scratch = os.path.abspath(args.scratch)
    workspace = setup(scratch)
    defaults = {**source_defaults(), **parse_defaults()}
    only = set(filter(None, args.only.split(',')))
    rows = {}
    for option in parse_metadata():
        if only and option['name'] not in only:
            continue
        for command in args.commands.split(','):
            if not option['commands'] & (1 << COMMANDS.index(command)):
                continue
            for arg, at_default in probes(option, defaults):
                verdict, message = run(args.bazel_rs, scratch, workspace, command, arg)
                if verdict in ('rejected', 'error', 'timeout'):
                    key = (arg, 'default' if at_default else 'other', verdict, message)
                    rows.setdefault(key, []).append(command)
    if args.markdown:
        print('| probe | value | verdict | commands | message |')
        print('| --- | --- | --- | --- | --- |')
    for (arg, value, verdict, message), commands in sorted(rows.items()):
        message = message.replace('|', '\\|')
        if args.markdown:
            print(f'| `{arg}` | {value} | {verdict} | {",".join(commands)} | {message[:160]} |')
        else:
            print(f'{arg}\t{value}\t{verdict}\t{",".join(commands)}\t{message[:200]}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
