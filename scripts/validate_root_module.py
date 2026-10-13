#!/usr/bin/env python3
"""G: bounded root MODULE recording validation; not eager graph/Bazel parity.

Reuse F's strict receipt runner in an isolated module namespace. This is ordinary
code reuse, not a mocked engine: every phase invokes the SHA-bound real CLI.
Neither importing this module nor configuring its runner mutates the F suite.
Reference-only mode cannot establish native closure. Low-level snapshot lifetime,
ignore-dev/builtin isolation and optional-read races belong to the Rust unit gate.
"""

import importlib.util
from pathlib import Path
import re
import signal
import sys

_spec = importlib.util.spec_from_file_location('_root_module_receipts',
    Path(__file__).with_name('validate_run_environment_info.py'))
harness = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(harness)
ROOT = harness.ROOT
ROOT_ONCE, BUILD_LOAD, BZL_LOAD, CALLBACK = 'G_ROOT_ONCE', 'G_BUILD_LOAD', 'G_BZL_LOAD', 'G_CALLBACK'
CASEPLAN = [
    harness.common.case('module', ('module-global-local', 'runenv-build', 'unknown-query', 'unknown-build',
        'syntax-query', 'syntax-build', 'division-query', 'division-build')),
    harness.common.case('valid', ('local', 'root-print', 'external-print')),
    harness.common.case('lazy-native', ('unused-dependency', 'unused-extension', 'unused-repository'), 'native_boundary'),
    harness.common.case('filesystem-native', ('workspace', 'repo', 'invalid-utf8', 'directory'), 'native_boundary'),
]
LIMITATIONS = [
    'Root recording evaluation before local query/build only; no eager graph, registry/network or full Bazel parity claim.',
    'The exact original MODULE bytes and local query are paired; original native0/reference48 FAIL is immutable history.',
    'Unused graph declarations are native-only narrower-profile checks; genuine Bazel may resolve even unused dependencies.',
    'Missing MODULE with WORKSPACE/REPO and malformed filesystem inputs are explicit native-only policy boundaries.',
    'Native CLI processes each record root once, including later external graph demand; reference server warm print count is not equated.',
    'Snapshot lifetime, option isolation and low-level API laziness require the independent Rust unit/integration gate.',
    'Reference-only preparation never runs native or claims native closure. Active source edits invalidate the before/after gate.',
]


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    files = {'MODULE.bazel': b'module(name="root_contract")\n', 'BUILD.bazel': b'', 'defs/BUILD.bazel': b'',
        'app/BUILD.bazel': b'load("//defs:check.bzl","check","build_marker")\nbuild_marker()\ncheck(name="check")\n'}
    value = dict(target='//app:check', expected={'check.out': b'g-ok\n'}, actions=1, diagnostic=None,
        stage='success', queries=[('//app:check', ['//app:check'])], genuine=False,
        markers=[BUILD_LOAD, BZL_LOAD], callback=True, root_print=False)
    if name == 'module':
        value.update(expected={}, actions=0, stage='loading', queries=[], markers=[], callback=False)
        if probe in ('module-global-local', 'runenv-build'):
            files['MODULE.bazel'] = harness.KNOWN_PARITY_GAPS[0]['module']['text'].encode()
            value['diagnostic'] = 'runenv'
        else:
            diagnostic = probe.split('-')[0]
            files['MODULE.bazel'] += {'unknown': b'x=UnknownRootSymbol\n', 'syntax': b'x=(\n',
                'division': b'x=1//0\n'}[diagnostic]
            value['diagnostic'] = diagnostic
    elif name == 'valid':
        if probe != 'local':
            value['root_print'] = True
            files['MODULE.bazel'] += ('print("'+ROOT_ONCE+'")\n').encode()
        if probe == 'external-print':
            files['MODULE.bazel'] += (b'bazel_dep(name="dep",version="1.0")\n'
                b'local_path_override(module_name="dep",path="dep")\n')
            files['dep/MODULE.bazel'] = b'module(name="dep",version="1.0")\n'
            files['dep/BUILD.bazel'] = b'exports_files(["defs.bzl"],visibility=["//visibility:public"])\n'
            files['dep/defs.bzl'] = b'TEXT="g-ok\\n"\n'
    elif name == 'lazy-native':
        files['defs/poison.bzl'] = b'fail("G_POISON_DEFINITION_LOADED")\n'
        files['MODULE.bazel'] += {
            'unused-dependency': b'bazel_dep(name="unavailable_g_dependency",version="99.0")\n',
            'unused-extension': b'e=use_extension("//defs:poison.bzl","e")\nuse_repo(e,"unused")\n',
            'unused-repository': b'r=use_repo_rule("//defs:poison.bzl","r")\nr(name="unused")\n',
        }[probe]
    elif name == 'filesystem-native':
        if probe in ('workspace', 'repo'):
            del files['MODULE.bazel']
            files['WORKSPACE' if probe == 'workspace' else 'REPO.bazel'] = b''
        else:
            value.update(expected={}, actions=0, diagnostic=probe, stage='loading', queries=[], markers=[], callback=False)
            if probe == 'invalid-utf8':
                files['MODULE.bazel'] = b'\xff\xfe\n'
            else:
                del files['MODULE.bazel']
                files['WORKSPACE'] = b''
                # The planned child proves that MODULE.bazel is a directory, not absence.
                files['MODULE.bazel/.qa-directory'] = b''
    prefix = b'load("@dep//:defs.bzl","TEXT")\n' if (name, probe) == ('valid', 'external-print') else b'TEXT="g-ok\\n"\n'
    action = ('    ctx.actions.run_shell(outputs=[out],command="printf BAD > rejected.marker; '
        'printf BAD > $1",arguments=[out.path])\n' if value['diagnostic'] else
        '    ctx.actions.write(out,TEXT,is_executable=True)\n')
    files['defs/check.bzl'] = prefix+(
        'print("'+BZL_LOAD+'")\ndef build_marker():\n    print("'+BUILD_LOAD+'")\n'
        'def _impl(ctx):\n    print("'+CALLBACK+'")\n'
        '    out=ctx.actions.declare_file("check.out")\n'+action+
        '    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_impl)\n').encode()
    value.update(files=files, failure_command='build' if probe.endswith('-build') else 'query',
        returncodes={'native': 1, 'reference': 48} if value['diagnostic'] else {'native': 0, 'reference': 0})
    return value


DIAGNOSTICS = {
    'runenv': r"(?:name ['`]RunEnvironmentInfo['`] is not defined|Variable `RunEnvironmentInfo` not found)",
    'unknown': r"(?:name ['`]UnknownRootSymbol['`] is not defined|Variable `UnknownRootSymbol` not found)",
    'syntax': r"(?:syntax error at 'newline': expected expression|Parse error: unexpected new line, expected expression)",
    'division': r'(?:integer|Floor) division by zero',
    'invalid-utf8': r'MODULE\.bazel.*(?:UTF-8|utf-8|stream did not contain valid UTF)',
    'directory': r'MODULE\.bazel.*(?:Is a directory|is a directory)',
}


def require_rejection(record, diagnostic, expected_code=None):
    text = record['stderr']
    errors = harness.utility.error_lines(text)
    checked = re.sub(r'^ERROR: Error computing the main repository mapping: (?:syntax error|error executing) in MODULE\.bazel file for <root>\s*$', '', text, flags=re.M)
    unrelated = harness.common.UNRELATED.pattern
    if diagnostic == 'syntax':
        unrelated = unrelated.replace('|syntax error', '').replace('|parse error', '')
    if (diagnostic not in DIAGNOSTICS or type(record['returncode']) is not int
            or record['returncode'] != expected_code or expected_code not in (1, 48)
            or record['timed_out'] or any(record.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or record['stdout'] != '' or re.search(unrelated, checked, re.I)
            or re.search(r'unknown repository|no repository visible|no such package|repository[^\n]*(?:not declared|not found)', errors, re.I)
            or not re.search(DIAGNOSTICS[diagnostic], errors, re.I | re.M)):
        raise ValueError('expected controlled root MODULE rejection: '+diagnostic)
    for marker in (BUILD_LOAD, BZL_LOAD, CALLBACK, 'G_POISON_DEFINITION_LOADED'):
        if harness.utility.marker_count(record, marker):
            raise ValueError('BUILD/bzl/callback ran before root recording validation')


def require_markers(record, value, command, first, engine, rejection):
    count = harness.utility.marker_count
    if rejection:
        if any(count(record, marker) for marker in (BUILD_LOAD, BZL_LOAD, CALLBACK)):
            raise ValueError('BUILD/bzl/callback ran before root recording validation')
        return
    if 'G_POISON_DEFINITION_LOADED' in record['stderr']:
        raise ValueError('unused declaration was evaluated')
    if first or engine == 'native':
        for marker in value['markers']:
            if count(record, marker) != 1:
                raise ValueError('missing or repeated loading marker: '+marker)
    if command == 'query' and count(record, CALLBACK):
        raise ValueError('query ran analysis callback')
    if command == 'build' and engine == 'native' and count(record, CALLBACK) != 1:
        raise ValueError('missing analysis callback')
    if value['root_print'] and (engine == 'native' or first) and count(record, ROOT_ONCE) != 1:
        raise ValueError('root MODULE was not evaluated exactly once')


def rejection_artifacts(workspace, reference, cache):
    return sorted(str(p) for root in (workspace, reference.output_base / 'execroot', cache)
        if root.exists() for p in root.rglob('rejected.marker') if p.is_file())


# All inherited functions resolve these globals in this private runner namespace.
harness.SUITE_NAME = 'root-module-validation-g'
harness.CASEPLAN = CASEPLAN
harness.PLAN = [(c['name'], c['kind']) for c in CASEPLAN]
harness.LIMITATIONS = LIMITATIONS
harness.SOURCES = list(dict.fromkeys([Path(__file__).resolve(), ROOT / 'tests/test_validate_root_module.py', *harness.SOURCES]))
harness.fixture = fixture
harness.require_rejection = require_rejection
harness.require_markers = require_markers
harness.rejection_artifacts = rejection_artifacts
argument_parser, plan_receipt, report_passes, main = harness.argument_parser, harness.plan_receipt, harness.report_passes, harness.main


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
