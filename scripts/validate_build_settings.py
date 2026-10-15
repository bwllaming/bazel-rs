#!/usr/bin/env python3
"""Eight genuine default-only build-setting families and one native boundary.

No CLI setting override, select, transition, Make expansion or toolchain claim.
Unit tests validate this harness, not either engine. Real QA is opt-in.
"""

import base64
import datetime
import hashlib
import io
import json
from pathlib import Path
import re
import shutil
import signal
import stat
import sys
import tarfile
import tempfile

from validate import ROOT, COMMON_FLAGS, BUILD_FLAGS, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256, validate_binaries
from validate_repository_rules import RecordingRunner, evidence, valid_evidence, snapshot, argument_parser as common_parser
from validate_builtin_modules import fingerprint, fingerprints, success

FIXTURE = ROOT / 'tests/fixtures/build_settings'
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
ARCHIVE_SHA256 = '6e78f0e57de26801f6f564fa7c4a48dc8b36873e416257a92bbb0937eeac8446'
COMMON_SHA256 = 'f3bcedef4b2b2cbe9750d61852917954499c4ba5e83d79fb975ec5814eb76d20'
SOURCE_URL = 'https://github.com/bazelbuild/bazel-skylib/releases/download/1.8.2/bazel-skylib-1.8.2.tar.gz'
NINE = ('bool_flag', 'bool_setting', 'int_flag', 'int_setting', 'string_flag', 'string_setting',
        'string_list_flag', 'string_list_setting', 'repeatable_string_flag')
NINE_BYTES = (b'bool_flag=True\nbool_setting=False\nint_flag=7\nint_setting=-2\nstring_flag=red\n'
              b'string_setting=plain\nstring_list_flag=a||b\nstring_list_setting=x|y\nrepeatable_string_flag=seed|tail\n')


def case(name, probes, kind='differential'):
    return dict(name=name, kind=kind, probes=list(probes))


CASES = [
    case('genuine.nine-defaults', ('cold', 'warm', *NINE)),
    case('defaults.validation', ('controls', 'missing', 'none', 'bool-two', 'overflow', 'list-element',
                                'injected-default', 'injected-help', 'cfg', 'descriptor', 'repeatable-nonflag')),
    case('genuine.allowed-values', ('allowed', 'invalid')),
    case('genuine.template-values', ('string', 'integer', 'empty', 'invalid')),
    case('template.constructor', ('identity-copy-order', 'missing', 'none', 'list', 'key', 'value', 'unknown', 'duplicate')),
    case('template.returns', ('only', 'mixed', 'nested', 'predeclared', 'duplicate', 'constructor', 'struct')),
    case('context.isolation', ('getter-and-snapshot', 'ordinary')),
    case('cache.default-change', ('red', 'blue', 'blue-warm', 'red-restored')),
    case('native.unsupported', ('string-option', 'bool-option', 'negative-bool-option', 'allow-multiple', 'string-set', 'legacy-true'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASES]
# The pre-N16b boundary "Starlark build settings on the command line are
# unsupported" no longer exists: N16b implements them (docs/CONFIGURATION.md,
# tests/configured_build_settings.rs and the paired capture
# tests/fixtures/configured_build_settings/reference_receipt.json). These
# probes are retired: never executed, recorded with this reason, and only
# accepted in that recorded form. CASES keeps them for historical identity.
SUPERSEDED_REASON = ('superseded by N16b: Starlark build settings on the command line are implemented '
                     '(docs/CONFIGURATION.md); the pre-N16b unsupported-option boundary no longer exists')
SUPERSEDED = {('native.unsupported', probe): SUPERSEDED_REASON
              for probe in ('string-option', 'bool-option', 'negative-bool-option')}


def superseded_phase(phase):
    """A retired probe's phase: no runs or evidence, only the recorded reason."""
    return (phase.get('status') == 'SUPERSEDED' and phase.get('reason') == SUPERSEDED_REASON
            and phase.get('runs') == [] and set(phase) == {'probe', 'engine', 'status', 'reason', 'runs'})
DIAGNOSTICS = {
    'default': r'build_setting_default[^\n]*(?:mandatory|required|missing|None|bool|int|string|list|range|convert)|(?:mandatory|required|missing)[^\n]*build_setting_default|(?:out of range|32.bit|not.*boolean|expected.*(?:bool|int|string|list))',
    'injected': r'(?:build_setting_default|help)[^\n]*(?:built.in|implicit|reserved|already|override|redefin)|(?:built.in|implicit|reserved|already|override|redefin)[^\n]*(?:build_setting_default|help)',
    'cfg': r'(?:build.setting|build_setting)[^\n]*(?:cfg|transition)|(?:cfg|transition)[^\n]*(?:build.setting|build_setting)',
    'descriptor': r'build_setting[^\n]*(?:type|BuildSetting|config|descriptor|expected)|(?:expected|want)[^\n]*BuildSetting',
    'repeatable': r'(?:repeatable|flag)[^\n]*(?:True|true|flag|repeatable)',
    'allowed': r"invalid value 'green'\. Allowed values are",
    'make-variable': r"invalid make variable name 'bad-name'",
    'template-argument': r'(?:TemplateVariableInfo|variables|vars)[^\n]*(?:argument|parameter|dict|string|None|unexpected|missing|specified|value)|(?:argument|parameter|dict|string|None|unexpected|missing)[^\n]*(?:TemplateVariableInfo|variables|vars)',
    'duplicate-provider': r'(?:duplicate|multiple|more than once|twice)[^\n]*(?:TemplateVariableInfo|provider)|(?:TemplateVariableInfo|provider)[^\n]*(?:duplicate|multiple|more than once|twice)',
    'return': r'(?:return|returned)[^\n]*(?:provider|Info|struct|sequence|list)|(?:provider|Info)[^\n]*(?:return|returned)|result of rule implementation[^\n]*want Info|analysis of //[^\n]+: expected list or tuple, got struct\b',
    'ordinary': r'(?:build_setting_value)[^\n]*(?:attribute|field|build.setting|only|available)|(?:build.setting|no attribute|no field)[^\n]*build_setting_value',
    'option': r'unsupported option[^\n]*--(?:no)?//:',
    'unsupported': r'(?:allow_multiple|string_set|build_setting)[^\n]*(?:unsupported|not supported|BuildSetting|descriptor)|(?:unsupported|not supported)[^\n]*(?:allow_multiple|string_set|build_setting)|Object of type `struct` has no attribute `string_set`',
}
INFRASTRUCTURE = re.compile(r'panick?ed|\bpanic\b|internal error|FATAL:|crashed|segmentation fault|'
    r'falling back|fallback to|timed out|timeout|download failed|could not download|network|'
    r'connection (?:refused|reset)|SSLHandshake|UnknownHost|permission denied|access denied|'
    r'parse error|syntax error|unexpected token|unknown apparent repository|no such package|'
    r'cannot load|failed to load|module [^\n]*not found|no such file', re.I)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def json_digest(value):
    return digest(json.dumps(value, sort_keys=True).encode())


def load_inputs(root=None):
    """Verify the archive itself and every unmodified extracted upstream entry."""
    root = FIXTURE if root is None else root
    provenance = json.loads((root / 'provenance.json').read_bytes())
    raw = (root / 'bazel-skylib-1.8.2.tar.gz').read_bytes()
    if (digest(raw) != ARCHIVE_SHA256 or provenance['archive_sha256'] != ARCHIVE_SHA256
            or provenance['source_url'] != SOURCE_URL or provenance['version'] != '1.8.2'
            or provenance['common_settings_sha256'] != COMMON_SHA256
            or provenance['archive_entry_count'] != 63 or provenance['file_count'] != 56):
        raise ValueError('genuine pinned skylib provenance mismatch')
    files, directories = {}, set()
    with tarfile.open(fileobj=io.BytesIO(raw), mode='r:gz') as archive:
        members = archive.getmembers()
        if len(members) != 63 or len({m.name for m in members}) != 63:
            raise ValueError('archive entry count or uniqueness mismatch')
        for member in members:
            relative = Path(member.name)
            if relative.is_absolute() or '..' in relative.parts or not (member.isfile() or member.isdir()):
                raise ValueError('unsafe upstream archive member')
            path = root / 'skylib' / relative
            info = path.lstat()
            if member.isdir():
                if not stat.S_ISDIR(info.st_mode) or stat.S_IMODE(info.st_mode) != member.mode:
                    raise ValueError('upstream directory changed')
                directories.add(relative.as_posix())
            else:
                data = archive.extractfile(member).read()
                if not stat.S_ISREG(info.st_mode) or path.read_bytes() != data or stat.S_IMODE(info.st_mode) != member.mode:
                    raise ValueError('upstream file bytes or mode changed: ' + member.name)
                files[member.name] = dict(sha256=digest(data), size_bytes=len(data), mode=member.mode)
    actual = {p.relative_to(root / 'skylib').as_posix() for p in (root / 'skylib').rglob('*')}
    if actual != set(files) | directories or files != provenance['files']:
        raise ValueError('missing or additional upstream entry')
    if files['rules/common_settings.bzl']['sha256'] != COMMON_SHA256:
        raise ValueError('common_settings pin mismatch')
    if (root / 'LICENSE').read_bytes() != (root / 'skylib/LICENSE').read_bytes():
        raise ValueError('LICENSE mismatch')
    return provenance


def source_paths():
    return sorted(set([ROOT / p for p in ('Cargo.toml', 'Cargo.lock', 'scripts/validate.py',
        'scripts/validate_build_settings.py', 'scripts/validate_builtin_modules.py',
        'scripts/validate_direct_repositories.py', 'scripts/validate_repository_rules.py',
        'tests/test_validate_build_settings.py')] + list((ROOT / 'src').rglob('*.rs'))
        + [p for directory in (FIXTURE, REGISTRY, ROOT / 'resources/bazel_tools') for p in directory.rglob('*') if p.is_file()]))


def require_rejection(record, diagnostic):
    code = record['returncode']
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record['timed_out']
            or record.get('spawn_error') or record.get('interrupted') or record.get('output_truncated')
            or record['stdout'].strip() or INFRASTRUCTURE.search(record['stderr'])
            or not re.search(DIAGNOSTICS[diagnostic], record['stderr'], re.I)):
        raise ValueError('expected controlled build-setting rejection: ' + diagnostic)


def stats(record, expected):
    success(record)
    matches = re.findall(r'(\d+) actions executed, (\d+) cache hits', record['stderr'])
    if len(matches) != 1 or tuple(map(int, matches[0])) != tuple(expected):
        raise ValueError('native action/cache counts differ: expected ' + str(expected))
    return dict(zip(('actions_executed', 'cache_hits'), expected))


def rule(body, *, attrs='{}', descriptor=None, extra='', prefix=''):
    return (prefix + 'def _impl(ctx):\n' + ''.join('    ' + line + '\n' for line in body.splitlines())
            + 'subject = rule(implementation = _impl, attrs = ' + attrs
            + (', build_setting = ' + descriptor if descriptor else '') + extra + ')\n')


def output_body(expression):
    return ('output = ctx.actions.declare_file(ctx.label.name + ".txt")\n'
            'ctx.actions.write(output, ' + expression + ')\nreturn [DefaultInfo(files = depset([output]))]')


def specification(item, probe):
    name = item['name']
    files = {'MODULE.bazel': 'module(name = "build_settings_contract")\nbazel_dep(name = "bazel_skylib", version = "1.8.2")\n'
             + 'local_path_override(module_name = "bazel_skylib", path = ' + repr(str(FIXTURE / 'skylib')) + ')\n',
             'consumer.bzl': (FIXTURE / 'consumer.bzl').read_text(),
             'BUILD.bazel': (FIXTURE / 'nine.BUILD').read_text()}
    spec = dict(files=files, target='//:result', expected=NINE_BYTES, diagnostic=None,
                rejection_command='build', extra=[], counts=(1, 0), query_first=True)
    if name == 'genuine.nine-defaults':
        if probe == 'warm':
            spec['counts'] = (0, 1)
        elif probe in NINE:
            spec.update(target='//:' + probe, expected=None, counts=(0, 0))
    elif name == 'cache.default-change':
        color = 'blue' if probe.startswith('blue') else 'red'
        files['BUILD.bazel'] = ('load("@bazel_skylib//rules:common_settings.bzl", "string_flag")\n'
            'load(":consumer.bzl", "consume")\nstring_flag(name = "color", build_setting_default = "' + color + '")\n'
            'consume(name = "result", deps = [":color"])\n')
        spec.update(expected=('color=' + color + '\n').encode(), counts=(0, 1) if probe in ('blue-warm', 'red-restored') else (1, 0))
    elif name == 'defaults.validation':
        files['defs.bzl'] = rule('return []', descriptor='config.bool()')
        files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result", build_setting_default = True)\n'
        spec.update(expected=None, counts=(0, 0))
        if probe == 'controls':
            files['BUILD.bazel'] = ('load("@bazel_skylib//rules:common_settings.bzl", "bool_setting", "int_setting", "string_setting", "string_list_setting")\n'
                'load(":consumer.bzl", "consume")\n'
                'bool_setting(name = "zero", build_setting_default = 0)\nbool_setting(name = "one", build_setting_default = 1)\n'
                'int_setting(name = "min", build_setting_default = -2147483648)\nint_setting(name = "max", build_setting_default = 2147483647)\n'
                'string_setting(name = "empty_string", build_setting_default = "")\nstring_list_setting(name = "empty_list", build_setting_default = [])\n'
                'consume(name = "result", deps = [":zero", ":one", ":min", ":max", ":empty_string", ":empty_list"])\n')
            spec.update(expected=b'zero=False\none=True\nmin=-2147483648\nmax=2147483647\nempty_string=\nempty_list=\n', counts=(1, 0))
        else:
            spec.update(diagnostic='default', rejection_command='query', query_first=False)
            defaults = {'missing': '', 'none': ', build_setting_default = None', 'bool-two': ', build_setting_default = 2',
                        'overflow': ', build_setting_default = 2147483648', 'list-element': ', build_setting_default = ["ok", 1]'}
            if probe in defaults:
                descriptor = 'config.int()' if probe == 'overflow' else 'config.string_list()' if probe == 'list-element' else 'config.bool()'
                files['defs.bzl'] = rule('return []', descriptor=descriptor)
                files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result"' + defaults[probe] + ')\n'
            elif probe.startswith('injected-'):
                attr = 'build_setting_default' if probe == 'injected-default' else 'help'
                files['defs.bzl'] = rule('return []', descriptor='config.bool()', attrs='{"' + attr + '": attr.string()}')
                spec['diagnostic'] = 'injected'
            elif probe == 'cfg':
                files['defs.bzl'] = rule('return []', descriptor='config.bool()', extra=', cfg = "exec"')
                spec['diagnostic'] = 'cfg'
            elif probe == 'descriptor':
                files['defs.bzl'] = rule('return []', descriptor='"wrong"')
                spec['diagnostic'] = 'descriptor'
            else:
                files['defs.bzl'] = rule('return []', descriptor='config.string_list(repeatable = True, flag = False)')
                spec['diagnostic'] = 'repeatable'
    elif name in ('genuine.allowed-values', 'genuine.template-values'):
        template = name == 'genuine.template-values'
        constructor = 'int_flag' if probe == 'integer' else 'string_flag'
        default = '7' if probe == 'integer' else '"green"' if probe == 'invalid' and not template else '"red"'
        attrs = (', make_variable = ' + repr('bad-name' if probe == 'invalid' else '' if probe == 'empty' else 'FLAG') if template
                 else ', values = ["red", "blue"]')
        files['BUILD.bazel'] = ('load("@bazel_skylib//rules:common_settings.bzl", "' + constructor + '")\n'
            + 'load(":' + ('defs.bzl", "subject' if template else 'consumer.bzl", "consume') + '")\n'
            + constructor + '(name = "flag", build_setting_default = ' + default + attrs + ')\n'
            + ('subject' if template else 'consume') + '(name = "result", deps = [":flag"])\n')
        if template:
            expression = ('str(platform_common.TemplateVariableInfo in ctx.attr.deps[0]) + "\\n"' if probe == 'empty'
                          else 'ctx.attr.deps[0][platform_common.TemplateVariableInfo].variables["FLAG"] + "\\n"')
            files['defs.bzl'] = rule(output_body(expression), attrs='{"deps": attr.label_list()}')
        spec['expected'] = (b'False\n' if probe == 'empty' else b'7\n' if probe == 'integer' else b'red\n') if template else b'flag=red\n'
        if probe == 'invalid':
            spec.update(diagnostic='make-variable' if template else 'allowed', expected=None)
    elif name == 'template.constructor':
        files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result")\n'
        if probe == 'identity-copy-order':
            files['alias.bzl'] = 'Template = platform_common.TemplateVariableInfo\n'
            body = ('original = {"z": "last", "a": "first"}\n'
                    'one = Template(original)\noriginal["z"] = "changed"\n'
                    'first = one.variables\nfirst["z"] = "mutated"\n'
                    'two = platform_common.TemplateVariableInfo(vars = {"z": "last", "a": "first"})\n'
                    'keys = {one: "one", two: "two"}\n'
                    'text = str(one == two) + ";" + str(len(keys)) + ";" + "|".join(one.variables.keys()) + ";" + one.variables["z"] + ";" + first["z"] + "\\n"\n')
            files['defs.bzl'] = rule(body + output_body('text'), prefix='load(":alias.bzl", "Template")\n')
            spec['expected'] = b'False;2;z|a;last;mutated\n'
        else:
            args = {'missing': '', 'none': 'None', 'list': '[]', 'key': '{1: "a"}', 'value': '{"a": 1}',
                    'unknown': '{}, unknown = True', 'duplicate': '{}, vars = {}'}[probe]
            files['defs.bzl'] = rule('value = platform_common.TemplateVariableInfo(' + args + ')\nreturn [value]')
            spec.update(diagnostic='template-argument', expected=None)
    elif name == 'template.returns':
        prefix = 'Info = provider(fields = ["nested"])\n'
        body = 'info = platform_common.TemplateVariableInfo({"FLAG": "red"})\n'
        returns = {'only': '[info]', 'mixed': '[DefaultInfo(), Info(nested = info), info]',
                   'nested': '[Info(nested = info)]', 'duplicate': '[info, platform_common.TemplateVariableInfo({})]',
                   'constructor': '[platform_common.TemplateVariableInfo]', 'struct': 'struct(value = "old")'}
        if probe == 'predeclared':
            files['defs.bzl'] = rule(body + 'ctx.actions.write(ctx.outputs.out, "red\\n")\nreturn [info]',
                                     attrs='{"out": attr.output()}', prefix=prefix)
            files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result", out = "result.txt")\n'
            spec['expected'] = b'red\n'
        else:
            files['defs.bzl'] = rule(body + 'return ' + returns[probe], prefix=prefix)
            files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result")\n'
            spec.update(expected=None, counts=(0, 0))
            if probe in ('duplicate', 'constructor', 'struct'):
                spec['diagnostic'] = 'duplicate-provider' if probe == 'duplicate' else 'return'
            elif probe in ('mixed', 'nested'):
                files['defs.bzl'] += (rule(output_body('ctx.attr.deps[0][Info].nested.variables["FLAG"] + ";" + str(platform_common.TemplateVariableInfo in ctx.attr.deps[0]) + "\\n"'),
                                          attrs='{"deps": attr.label_list()}').replace('_impl', '_capture_impl').replace('subject = rule', 'capture = rule'))
                files['BUILD.bazel'] = 'load(":defs.bzl", "subject", "capture")\nsubject(name = "producer")\ncapture(name = "result", deps = [":producer"])\n'
                spec.update(expected=b'red;True\n' if probe == 'mixed' else b'red;False\n', counts=(1, 0))
    elif name == 'context.isolation':
        if probe == 'getter-and-snapshot':
            files['defs.bzl'] = (FIXTURE / 'mutability.bzl').read_text()
            files['BUILD.bazel'] = ('load(":defs.bzl", "mutating", "late_default", "capture")\n'
                'mutating(name = "seed", build_setting_default = ["seed"])\n'
                'mutating(name = "other", build_setting_default = ["other"])\nlate_default(name = "late")\n'
                'capture(name = "result", deps = [":seed", ":other", ":late"])\n')
            spec['expected'] = b'seed=seed|mutated;seed;seed\nother=other|mutated;other;other\nlate=seed;seed\n'
        else:
            files['defs.bzl'] = rule('value = ctx.build_setting_value\nreturn []')
            files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result")\n'
            spec.update(diagnostic='ordinary', expected=None)
    elif name == 'native.unsupported':
        spec.update(diagnostic='option' if probe.endswith('option') else 'unsupported', expected=None, query_first=False)
        if probe.endswith('option'):
            spec['extra'] = [{'string-option': '--//:string_flag=blue', 'bool-option': '--//:bool_flag',
                              'negative-bool-option': '--no//:bool_flag'}[probe]]
        else:
            descriptor = {'allow-multiple': 'config.string(flag = True, allow_multiple = True)',
                          'string-set': 'config.string_set()', 'legacy-true': 'True'}[probe]
            files['defs.bzl'] = rule('return []', descriptor=descriptor)
            files['BUILD.bazel'] = 'load(":defs.bzl", "subject")\nsubject(name = "result", build_setting_default = "red")\n'
            spec['rejection_command'] = 'query'
    else:
        raise ValueError('unplanned family')
    return spec


def phases_for(item):
    return [(probe, engine) for probe in item['probes'] for engine in
            (('reference', 'native') if item['kind'] == 'differential' else ('native',))]


def steps_for(spec, engine):
    result = [('query', [])] if spec['query_first'] else []
    if spec['diagnostic']:
        result.append((spec['rejection_command'], spec['extra']))
    else:
        result.append(('build', []))
        if engine == 'reference':
            result.append(('cquery', ['--output=files']))
    return result


def native_command(executable, cache, command, target, extra=()):
    return [str(executable), '--ignore_all_rc_files', '--cache-dir=' + str(cache), command,
            '--registry=' + REGISTRY.as_uri(), '--experimental-builtin-modules', *extra, target]


def prepare(workspace, spec):
    if workspace.exists():
        shutil.rmtree(workspace)
    for relative, text in spec['files'].items():
        path = workspace / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return [workspace / path for path in spec['files']]


def output_paths(record, spec, engine, reference, scratch):
    if engine == 'reference':
        paths = reference_output_paths(success(record), reference.workspace, reference.output_base)
    else:
        paths = []
        for line in success(record).splitlines():
            parts = line.split('\t')
            if len(parts) != 2 or parts[0] != spec['target']:
                raise ValueError('expected exact native LABEL<TAB>PATH')
            paths.append(Path(parts[1]))
    if len(paths) != (0 if spec['expected'] is None else 1) or len(set(paths)) != len(paths):
        raise ValueError('wrong artifact count, including provider-only zero-file contract')
    for path in paths:
        if not path.is_absolute() or '..' in path.parts or not path.resolve().is_relative_to(scratch):
            raise ValueError('unsafe output path')
    return paths


def run_case(args, runner, scratch, reference, item):
    check = dict(name=item['name'], kind=item['kind'], input=item, status='FAIL', parity_claim=False, phases=[])
    runner.report['checks'].append(check)
    for probe in item['probes']:
        if (item['name'], probe) in SUPERSEDED:
            for engine in ('reference', 'native') if item['kind'] == 'differential' else ('native',):
                check['phases'].append(dict(probe=probe, engine=engine, status='SUPERSEDED',
                                            reason=SUPERSEDED[(item['name'], probe)], runs=[]))
            continue
        spec = specification(item, probe)
        paths = prepare(reference.workspace, spec)
        shared = item['name'] in ('genuine.nine-defaults', 'cache.default-change')
        cache = scratch / 'cases' / item['name'] / ('cache' if shared else probe + '-cache')
        for engine in ('reference', 'native') if item['kind'] == 'differential' else ('native',):
            phase = dict(probe=probe, engine=engine, runs=[], status='FAIL', cache=str(cache), artifacts=[],
                         inputs_before=snapshot(paths))
            check['phases'].append(phase)
            try:
                if spec['diagnostic']:
                    # Delete only stale reference outputs from our private output base.
                    # A negative phase must leave no consumer output on either engine.
                    if engine == 'reference':
                        for stale in reference.output_base.glob('execroot/*/bazel-out/*/bin/result.txt'):
                            if not stale.resolve().is_relative_to(scratch):
                                raise ValueError('stale output escaped private scratch')
                            stale.unlink()
                    phase['outputs_before'] = rejection_outputs(cache, reference, engine)
                    if phase['outputs_before']:
                        raise ValueError('negative phase has stale consumer outputs')
                records = []
                for index, (command, extra) in enumerate(steps_for(spec, engine)):
                    label = '.'.join([item['name'], probe, engine, str(index)])
                    record = (runner.run(label, native_command(args.native, cache, command, spec['target'], extra), reference.workspace)
                              if engine == 'native' else reference.run(label, command, *extra, spec['target']))
                    phase['runs'].append(record['id'])
                    records.append(record)
                    if spec['diagnostic'] and index == len(steps_for(spec, engine)) - 1:
                        require_rejection(record, spec['diagnostic'])
                        phase['diagnostic'] = spec['diagnostic']
                    elif command == 'query':
                        if success(record).splitlines() != [spec['target']]:
                            raise ValueError('query target differs')
                    else:
                        success(record)
                        if command == 'build' and engine == 'native':
                            phase['counts'] = stats(record, spec['counts'])
                if not spec['diagnostic']:
                    for path in output_paths(records[-1], spec, engine, reference, scratch):
                        artifact = evidence(path.resolve(strict=True))
                        artifact['logical_path'] = str(path)
                        if artifact['kind'] != 'file' or not valid_evidence(artifact, spec['expected'], True):
                            raise ValueError('actual output bytes/executable mode differ')
                        phase['artifacts'].append(artifact)
                else:
                    phase['outputs_after'] = rejection_outputs(cache, reference, engine)
                    if phase['outputs_after']:
                        raise ValueError('rejected analysis executed the consumer write action')
                phase['status'] = 'PASS'
            except Exception as exc:
                phase['error'] = type(exc).__name__ + ': ' + str(exc)
            finally:
                phase['inputs_after'] = snapshot(paths)
                if phase['inputs_before'] != phase['inputs_after']:
                    phase.update(status='FAIL', error='CLI changed frozen fixture inputs')
    try:
        validate_check(check, runner.report)
        check.update(status='PASS', parity_claim=item['kind'] == 'differential')
    except (ValueError, KeyError, IndexError) as exc:
        check['error'] = str(exc)
        if any(p['engine'] == 'reference' and p['status'] != 'PASS' for p in check['phases']):
            check['status'] = 'REFERENCE_FAILURE'


def reference_for(report):
    scratch = Path(report['scratch_directory'])
    reference = DirectReference(Path(report['configuration']['reference']), scratch / 'workspace', scratch, None, 'build-settings')
    reference.registry = REGISTRY
    return reference


def rejection_outputs(cache, reference, engine):
    return sorted(map(str, cache.glob('**/result.txt') if engine == 'native'
                      else reference.output_base.glob('execroot/*/bazel-out/*/bin/result.txt')))


def validate_check(check, report):
    item = next(c for c in CASES if c['name'] == check['name'])
    if (check['input'] != item or check['kind'] != item['kind']
            or [(p['probe'], p['engine']) for p in check['phases']] != phases_for(item)):
        raise ValueError('missing, duplicate, reordered or unexpected phases')
    reference = reference_for(report)
    scratch = Path(report['scratch_directory'])
    for phase in check['phases']:
        if (item['name'], phase['probe']) in SUPERSEDED:
            if not superseded_phase(phase):
                raise ValueError('retired probe must be recorded as superseded, never executed')
            continue
        if phase.get('status') == 'SUPERSEDED':
            raise ValueError('only retired probes may be superseded')
        spec = specification(item, phase['probe'])
        shared = item['name'] in ('genuine.nine-defaults', 'cache.default-change')
        cache = scratch / 'cases' / item['name'] / ('cache' if shared else phase['probe'] + '-cache')
        if (phase['cache'] != str(cache) or phase['status'] != 'PASS' or phase.get('error')
                or not phase['inputs_before'] or phase['inputs_before'] != phase['inputs_after']):
            raise ValueError('failed phase, wrong cache or changed inputs')
        expected_inputs = {str(reference.workspace / p): raw.encode() for p, raw in spec['files'].items()}
        if set(phase['inputs_before']) != set(expected_inputs):
            raise ValueError('wrong planned input paths')
        for path, raw in expected_inputs.items():
            value = phase['inputs_before'][path]
            if value['path'] != path or value['kind'] != 'file' or not valid_evidence(value, raw):
                raise ValueError('wrong actual fixture input')
        steps = steps_for(spec, phase['engine'])
        if len(phase['runs']) != len(steps):
            raise ValueError('missing planned real invocations')
        records = []
        for index, (run_id, (command, extra)) in enumerate(zip(phase['runs'], steps)):
            record = report['runs'][run_id]
            records.append(record)
            expected = (native_command(Path(report['configuration']['native']), cache, command, spec['target'], extra)
                        if phase['engine'] == 'native' else reference.startup + [command] + list(COMMON_FLAGS)
                        + (list(BUILD_FLAGS) if command in ('build', 'cquery') else [])
                        + ['--repository_cache=' + str(scratch / 'repository-cache'), '--repo_contents_cache=' + str(scratch / 'repository-contents'),
                           '--registry=' + REGISTRY.as_uri(), '--registry=https://bcr.bazel.build'] + extra + [spec['target']])
            if (record['argv'] != expected or record['cwd'] != str(reference.workspace)
                    or record['name'] != '.'.join([item['name'], phase['probe'], phase['engine'], str(index)])):
                raise ValueError('invocation provenance mismatch')
            if spec['diagnostic'] and index == len(steps) - 1:
                require_rejection(record, spec['diagnostic'])
            elif command == 'query':
                if success(record).splitlines() != [spec['target']]:
                    raise ValueError('query mismatch')
            else:
                success(record)
                if command == 'build' and phase['engine'] == 'native' and phase['counts'] != stats(record, spec['counts']):
                    raise ValueError('missing exact native cache evidence')
        if spec['diagnostic']:
            if (phase['diagnostic'] != spec['diagnostic'] or phase['artifacts']
                    or phase['outputs_before'] != [] or phase['outputs_after'] != []):
                raise ValueError('missing controlled rejection evidence')
        else:
            paths = output_paths(records[-1], spec, phase['engine'], reference, scratch)
            if [a['logical_path'] for a in phase['artifacts']] != list(map(str, paths)):
                raise ValueError('artifacts not linked to actual CLI output')
            for artifact, path in zip(phase['artifacts'], paths):
                if (artifact['kind'] != 'file' or not valid_evidence(artifact, spec['expected'], True)
                        or artifact['path'] != str(path) or not Path(artifact['path']).is_relative_to(scratch)):
                    raise ValueError('artifact bytes/mode mismatch')
    if item['kind'] == 'differential':
        for one, two in zip(check['phases'][::2], check['phases'][1::2]):
            if one.get('status') == 'SUPERSEDED':
                continue
            if [(a['sha256'], bool(a['mode'] & 0o111)) for a in one['artifacts']] != [(a['sha256'], bool(a['mode'] & 0o111)) for a in two['artifacts']]:
                raise ValueError('genuine raw-byte/executable-mode comparison differs')


def report_passes(report):
    try:
        if (report['planned_checks'] != CASES or report['case_inputs_sha256'] != json_digest(CASES)
                or [(c['name'], c['kind']) for c in report['checks']] != PLAN
                or report['errors'] or report['cleanup'] or report['scratch_cleaned'] is not True
                or report['cleanup_receipt'] != dict(path=report['scratch_directory'], removed=True)
                or report['reference_shutdown_succeeded'] is not True or report['sources_unchanged'] is not True
                or not report['sources_before'] or report['sources_before'] != report['sources_after']
                or report['completed_checks'] != 9 or report['completed_differential_cases'] != 8
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['executables_unchanged'] is not True
                or any(report['executables'][name]['sha256'] != report['executables_after'][name]['sha256']
                       or report['executables_after'][name]['path'] != report['configuration'][name]
                       for name in ('native', 'reference'))
                or report['reference_release'] != 'bazel 9.3.0' or report['fixture'] != load_inputs()
                or report.get('superseded') != {'/'.join(key): reason for key, reason in SUPERSEDED.items()}):
            return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(record[field + '_base64'], validate=True)
                if (record[field + '_sha256'] != digest(raw) or raw.decode('utf-8', errors='replace') != record[field]
                        or record[field + '_full_sha256'] != digest(raw) or record[field + '_full_size_bytes'] != len(raw)):
                    return False
        reference = reference_for(report)
        if (runs[0]['name'] != 'reference.version' or runs[0]['argv'] != reference.startup + ['version', '--gnu_format']
                or success(runs[0]).strip() != 'bazel 9.3.0' or runs[-1]['name'] != 'build-settings.shutdown'
                or runs[-1]['argv'] != reference.startup + ['shutdown'] + list(COMMON_FLAGS)):
            return False
        success(runs[-1])
        used = [0, len(runs) - 1]
        for check in report['checks']:
            if check['status'] != 'PASS' or check.get('error') or check['parity_claim'] != (check['kind'] == 'differential'):
                return False
            validate_check(check, report)
            used.extend(i for phase in check['phases'] for i in phase['runs'])
        return sorted(used) == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def argument_parser():
    parser = common_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/build-settings-validation.json')
    return parser


def finish(reference, temporary, report):
    if reference is not None:
        try:
            report['reference_shutdown_attempted'] = True
            reference.shutdown()
            if report['runs'][-1]['name'] != 'build-settings.shutdown':
                raise ValueError('missing shutdown invocation')
            success(report['runs'][-1])
            report['reference_shutdown_succeeded'] = not report['cleanup']
        except Exception as exc:
            report['errors'].append('reference shutdown: ' + str(exc))
    if temporary is not None:
        try:
            scratch = Path(temporary.name)
            temporary.cleanup()
            report['scratch_cleaned'] = not scratch.exists()
            report['cleanup_receipt'] = dict(path=str(scratch), removed=report['scratch_cleaned'])
        except OSError as exc:
            report['errors'].append('scratch cleanup: ' + str(exc))


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for key in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, key, getattr(args, key).expanduser().resolve())
    if (args.output in (args.native, args.reference) or args.output in source_paths()
            or any(args.output.is_relative_to(ROOT / p) for p in ('src', 'scripts', 'tests', 'resources'))):
        raise ValueError('report must not overwrite sources, fixtures or executables')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[], planned_checks=CASES,
                  superseded={'/'.join(key): reason for key, reason in SUPERSEDED.items()},
                  case_inputs_sha256=json_digest(CASES), configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                  scratch_cleaned=False, sources_unchanged=False, reference_shutdown_attempted=False,
                  reference_shutdown_succeeded=False, started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  limitations=['Default-only; no select, transitions, CLI setting overrides, Make expansion or toolchain resolution.',
                    'Eight genuine differential families; one native-only unsupported boundary. Not full Bazel compatibility.',
                    'The three CLI-option probes of native.unsupported are retired (' + SUPERSEDED_REASON + ').',
                    'Real unmodified skylib 1.8.2 local override; genuine built-in file registry. Native builtin profile is explicitly enabled.',
                    'Reference may download genuine sources; native remote access remains disabled. No fake provider/namespace definitions.',
                    'Source/binary hashes identify supplied inputs, not independent proof of compilation provenance.',
                    'Synthetic unit-test records are harness guards and never genuine engine evidence.'])
    runner = RecordingRunner(report, args.timeout)
    temporary = reference = None
    interrupted = False
    try:
        report['sources_before'] = fingerprints(source_paths())
        report['fixture'] = load_inputs()
        report['executables'] = validate_binaries(args)
        temporary = tempfile.TemporaryDirectory(prefix='build-settings-', dir=args.scratch_parent)
        scratch = Path(temporary.name)
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        reference = DirectReference(args.reference, workspace, scratch, runner, 'build-settings')
        reference.registry = REGISTRY
        reference.started = True
        report['reference_release'] = success(runner.run('reference.version', reference.startup + ['version', '--gnu_format'], workspace)).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('reference version mismatch')
        for item in CASES:
            run_case(args, runner, scratch, reference, item)
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__ + ': ' + str(exc))
    finally:
        finish(reference, temporary, report)
        try:
            report['sources_after'] = fingerprints(source_paths())
            report['sources_unchanged'] = report.get('sources_before') == report['sources_after']
        except OSError as exc:
            report['errors'].append('source evidence: ' + str(exc))
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['completed_differential_cases'] = sum(c['status'] == 'PASS' and c['kind'] == 'differential' for c in report['checks'])
        try:
            report['executables_after'] = {name: fingerprint(getattr(args, name)) for name in ('native', 'reference')}
            report['executables_unchanged'] = all(report['executables'][name]['sha256'] == value['sha256']
                                                for name, value in report['executables_after'].items())
        except (OSError, KeyError, ValueError) as exc:
            report['errors'].append('executable evidence: ' + str(exc))
        if report_passes(report):
            report['status'] = 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        write_report(args.output, report)
    print('{} (default-only build settings): {}'.format(report['status'], args.output))
    return 130 if interrupted else 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
