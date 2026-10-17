"""Portable harness units only; synthetic records are NOT CLI acceptance."""

import ast
import base64
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_host_os as suite


def record(text='', code=1):
    return dict(stdout='', stderr=text, returncode=code, client_reaped=True, timed_out=False)


def blob(path, raw, mode=0o644):
    return dict(path=str(path), kind='file', bytes_base64=base64.b64encode(raw).decode(),
                sha256=suite.digest(raw), size_bytes=len(raw), mode=mode)


def native_immutable_record(probe):
    """Portable diagnostic model; only direct.name was observed in first run."""
    value = suite.fixture('immutable', probe)
    route, field = probe.split('.')
    source = 'ext.bzl' if route == 'extension' else 'defs.bzl'
    assignment = '    host.'+field+'="controlled"'
    line = value['files'][source].decode().splitlines().index(assignment)+1
    callbacks = (['HOST_EXT_CALLBACK'] if value['ordinary'] else [])
    if route != 'extension':
        callbacks += ['HOST_REPO_CALLBACK']
    text = '\n'.join(callbacks)+'\n'
    text += ('bazel-rs: controlled fixture traceback\n'
             'error: cannot set .'+field+' field of repository_os value\n'
             ' --> //:'+source+':'+str(line)+':5\n'
             '  |\n'+str(line)+' | '+assignment+'\n  |     ^^^^^^^^^\n  |\n')
    return value, record(text), source, line


def native_proxy_record(workspace=None):
    """Exact stderr layout of the integration-14 pre-gate row (formerly run 112's
    pre-N33 wording), with only its private workspace relocated.

    No ignored report dependency: verification separately compares this literal
    transcript to the retained actual row before the parent rebinds QA sources.
    """
    workspace = str(workspace or (suite.ROOT / 'artifacts/host-os-s4jrw22c/workspace'))
    # Since N33 the body is Bazel's text (genuine 9.3.0 for the same line).
    text = ("bazel-rs: error: 'TagCallable' value has no field or method 'name'\n"
            ' --> '+workspace+'/MODULE.bazel:4:7\n'
            '  |\n'
            '4 | value=e.os.name\n'
            '  |       ^^^^^^^^^\n'
            '  |\n'
            '\n')
    return dict(record(text), cwd=workspace)


def native_build_os_record(workspace='/private/workspace'):
    # Compiler boundary, not an executed BUILD callback. Source echo is not a
    # runtime marker. Byte-identical (workspace relocated) to the
    # integration-14 pre-gate row; the actual row is checked separately.
    # Since N33 the body is Bazel's text ("name 'os' is not defined").
    text = ("bazel-rs: error: name 'os' is not defined\n"
            ' --> '+str(workspace)+'/BUILD.bazel:1:7\n'
            '  |\n1 | value=os.name\n  |       ^^\n  |\n\n'
            'Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and Starlark rule() '
            'definitions. select() resolves against modelled config_setting values (docs/CONFIGURATION.md).\n')
    return dict(record(text), cwd=str(workspace))


class PlanTests(unittest.TestCase):
    def test_eight_unique_families_and_valid_generated_sources(self):
        self.assertEqual(len(suite.CASEPLAN), 8)
        self.assertEqual(len({c['name'] for c in suite.CASEPLAN}), 8)
        for item in suite.CASEPLAN:
            self.assertEqual(len(set(item['probes'])), len(item['probes']))
            for probe in item['probes']:
                value = suite.fixture(item['name'], probe)
                for name, raw in value['files'].items():
                    if name.endswith(('.bzl', '.bazel')):
                        ast.parse(raw, filename=name)

    def test_no_unknown_fixture_fallback(self):
        for name, probe in [('bogus', 'direct'), ('metadata', 'synthetic-host')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)

    def test_root_and_sources_are_checkout_relative(self):
        self.assertEqual(suite.ROOT, Path(__file__).resolve().parents[1])
        paths = suite.source_paths()
        self.assertIn(Path(__file__).resolve(), paths)
        self.assertIn(suite.ROOT / 'src/host_os.rs', paths)
        self.assertTrue(all(p.is_relative_to(suite.ROOT) for p in paths))
        self.assertTrue(all('artifacts' not in p.parts for p in paths))
        self.assertNotIn('/home/coder/bazel', Path(suite.__file__).read_text())

    def test_exact_os_value_metadata_not_generic_struct(self):
        golden = suite.golden('module_ctx')
        self.assertEqual(golden['type'], 'repository_os')
        self.assertEqual(golden['dir'], ['arch', 'environ', 'name'])
        self.assertEqual(golden['name'], 'linux')
        self.assertEqual(golden['arch'], 'amd64')
        self.assertEqual(golden['repr'], suite.REPR)
        self.assertIn('keyed[ctx.os]', suite.METADATA)
        self.assertNotIn('getattr(host, "environ"', suite.METADATA)

    def test_all_flag_combinations_have_real_extension_and_repo_callbacks(self):
        for flag, options in suite.FLAGS.items():
            value = suite.fixture('metadata', flag)
            self.assertIn(('ext=module_extension(_ext'+options+')').encode(), value['files']['ext.bzl'])
            self.assertIn(b'text=json.encode(metadata(ctx))', value['files']['ext.bzl'])
            self.assertIn(b'"text":attr.string()', value['files']['defs.bzl'])
            self.assertIn(b'json.decode(ctx.attr.text)', value['files']['defs.bzl'])
            self.assertIn(b'ctx.file("data.txt"', value['files']['defs.bzl'])
            self.assertIn(b'cat $(SRCS)', value['files']['BUILD.bazel'])
            self.assertEqual(json.loads(suite.expected_bytes(value, {}, False))['extension'], suite.golden('module_ctx'))
        self.assertEqual(suite.FACTORS, dict(default='general', **{'00': 'general', '10': 'os:linux',
            '01': 'arch:amd64', '11': 'os:linux,arch:amd64'}))

    def test_reference_reads_repository_environ_only_in_its_single_state_family(self):
        for item in suite.CASEPLAN:
            pairs = suite.phases_for(item, True)
            if item['kind'] != 'differential':
                self.assertEqual(pairs, [])
            for probe, engine in pairs:
                self.assertEqual(engine, 'reference')
                value = suite.fixture(item['name'], probe)
                source = b'\n'.join(value['files'].values())
                self.assertNotIn(b'json.encode(ctx.os)', source)
                if item['name'] == 'environ':
                    # The repository (direct/ordinary) or the extension
                    # (extension/ctor) reads the client environment; one state.
                    reader = 'defs.bzl' if probe in suite.ROUTES else 'ext.bzl'
                    other = 'ext.bzl' if probe in suite.ROUTES else 'defs.bzl'
                    self.assertIn(b'ctx.os.environ' if probe != 'ctor' else b'ctx.getenv(', value['files'][reader])
                    self.assertNotIn(b'ctx.os.environ', value['files'][other])
                    self.assertEqual(len(suite.states('environ')), 1)
                else:
                    self.assertNotIn(b'ctx.os.environ', source)
                    self.assertNotIn(b'getattr(ctx.os,"environ"', source)

    def test_repository_environ_is_the_whole_controlled_client_environment(self):
        for route in suite.ROUTES:
            value = suite.fixture('environ', route)
            source = value['files']['defs.bzl']
            self.assertIn(b'getattr(ctx.os,"environ",{})==ctx.os.environ', source)
            self.assertIn(b'sorted(ctx.os.environ.keys())', source)
            self.assertIsNone(value['diagnostic'])
            self.assertEqual(value['ordinary'], route == 'ordinary')
            [(label, env, changed)] = suite.states('environ')
            self.assertEqual(json.loads(suite.expected_bytes(value, env, changed)),
                             ['alpha', sorted(['PATH', 'HOME', 'TMPDIR', 'LC_ALL', suite.A]), True, 'dict'])
            self.assertEqual(json.loads(suite.expected_bytes(value, {}, False))[:2],
                             ['DEFAULT', ['HOME', 'LC_ALL', 'PATH', 'TMPDIR']])
        item = next(c for c in suite.CASEPLAN if c['name'] == 'environ')
        self.assertEqual(item['kind'], 'differential')
        boundary = next(c for c in suite.CASEPLAN if c['name'] == 'native-boundary')['probes']
        for probe in ('direct.environ', 'direct.environ-default', 'ordinary.environ', 'ordinary.environ-default'):
            self.assertNotIn(probe, boundary)
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture('native-boundary', probe)

    def test_module_ctx_environ_getenv_and_declared_environ_are_differential(self):
        extension = suite.fixture('environ', 'extension')
        self.assertIn(b'getattr(ctx.os,"environ",{})==ctx.os.environ', extension['files']['ext.bzl'])
        self.assertIn(b"ctx.getenv('"+suite.A.encode()+b"',\"DEFAULT\")", extension['files']['ext.bzl'])
        self.assertEqual(json.loads(suite.expected_bytes(extension, {suite.A: 'alpha'}, False))[-1], 'alpha')
        ctor = suite.fixture('environ', 'ctor')
        self.assertIn(("ext=module_extension(_ext,environ=["+repr(suite.A)+"])").encode(), ctor['files']['ext.bzl'])
        self.assertEqual(suite.expected_bytes(ctor, {suite.A: 'alpha'}, False), b'alpha\n')
        self.assertEqual(suite.expected_bytes(ctor, {}, False), b'DEFAULT\n')
        boundary = next(c for c in suite.CASEPLAN if c['name'] == 'native-boundary')['probes']
        for probe in ('extension.environ', 'extension.environ-default', 'extension.getenv', 'ctor.environ'):
            self.assertNotIn(probe, boundary)
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture('native-boundary', probe)

    def test_version_migration_callbacks_never_read_host(self):
        item = next(c for c in suite.CASEPLAN if c['name'] == 'migration')
        self.assertEqual(item['kind'], 'native_version_migration')
        self.assertEqual(suite.phases_for(item, True), [])
        for route in suite.ROUTES:
            source = b'\n'.join(suite.fixture('migration', route)['files'].values())
            self.assertNotIn(b'ctx.os', source)
            self.assertNotIn(b'ctx.getenv', source)
            self.assertIn(b'CONST', source)

    def test_required_unit_gates_cannot_be_substituted_by_same_host_results(self):
        self.assertEqual(len(suite.UNIT_GATES), 4)
        self.assertIn('Same-host outputs do not prove cross-host flag keying', '\n'.join(suite.LIMITATIONS))
        self.assertIn('SEPARATE_RECEIPTS_REQUIRED_NOT_CLI_PROVEN', Path(suite.__file__).read_text())
        self.assertNotIn('--host-os', Path(suite.__file__).read_text())

    def test_cache_source_and_watched_environment_transitions(self):
        for route in suite.ROUTES:
            value = suite.fixture('cache', route)
            self.assertIn(b'ctx.template(', value['files']['defs.bzl'])
            self.assertNotEqual(suite.expected_bytes(value, {}, False), suite.expected_bytes(value, {}, True))
            env = suite.fixture('getenv-control', route)
            self.assertIn(('environ=['+repr(suite.A)+']').encode(), env['files']['defs.bzl'])
            self.assertIn(b'ctx.getenv(', env['files']['defs.bzl'])
            self.assertEqual(suite.expected_bytes(env, {}, False), b'DEFAULT:linux/amd64\n')
            self.assertEqual(suite.expected_bytes(env, {suite.A: ''}, False), b':linux/amd64\n')


class PrivacyAndPinsTests(unittest.TestCase):
    def test_no_ambient_access_in_runner(self):
        class ForbiddenEnvironment:
            def __getattribute__(self, key):
                raise AssertionError('ambient environment access')
            def __iter__(self):
                raise AssertionError('ambient enumeration')
        args = suite.argument_parser().parse_args(['--reference-only'])
        report = dict(runs=[], executables={'reference': dict(path='/bin/true', sha256='a'*64)})
        with mock.patch.object(suite.os, 'environ', ForbiddenEnvironment()):
            runner = suite.RecordingRunner(report, args, Path('/private'))
            process = mock.Mock(pid=321, returncode=0)
            process.communicate.return_value = (b'', b'')
            with mock.patch.object(runner, 'guard'), mock.patch.object(suite.subprocess, 'Popen', return_value=process) as spawn:
                row = runner.run('safe', ['/bin/true'], Path('/private'))
            self.assertEqual(set(spawn.call_args.kwargs['env']), {'PATH', 'HOME', 'TMPDIR', 'LC_ALL'})
            self.assertTrue(row['client_reaped'])
            self.assertTrue(row['precommand_pins_verified'])

    def test_timeout_reaps_only_owned_client_group(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        report = dict(runs=[], executables={'reference': dict(path='/bin/true', sha256='a'*64)})
        runner = suite.RecordingRunner(report, args, Path('/private'))
        process = mock.Mock(pid=321, returncode=-9)
        process.communicate.side_effect = [suite.subprocess.TimeoutExpired('safe', 1), (b'', b'')]
        with mock.patch.object(runner, 'guard'), mock.patch.object(suite.subprocess, 'Popen', return_value=process), \
                mock.patch.object(suite.os, 'killpg') as kill:
            row = runner.run('safe', ['/bin/true'], Path('/private'))
        kill.assert_called_once_with(321, suite.signal.SIGKILL)
        self.assertTrue(row['timed_out'])
        with self.assertRaises(ValueError):
            suite.controlled.healthy(row)

    def test_official_git_checks_use_explicit_safe_environment(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        with tempfile.TemporaryDirectory() as tmp:
            args.official_source = Path(tmp)
            raw = b'controlled source'
            (Path(tmp) / 'source').write_bytes(raw)
            with mock.patch.object(suite, 'OFFICIAL', {'source': suite.digest(raw)}), \
                    mock.patch.object(suite.subprocess, 'check_output', side_effect=[suite.OFFICIAL_COMMIT.encode(), raw]) as call:
                suite.official_pins(args, Path(tmp), verify_git=True)
            for invocation in call.call_args_list:
                self.assertEqual(set(invocation.kwargs['env']), {'PATH', 'HOME', 'TMPDIR', 'LC_ALL'})

    def test_frozen_native_and_separate_baseline_required(self):
        args = suite.argument_parser().parse_args([])
        with self.assertRaisesRegex(ValueError, 'frozen candidate'):
            suite.executables(args)
        with mock.patch.object(suite.common, 'executable_evidence', return_value={}):
            with self.assertRaisesRegex(ValueError, 'pinned K baseline'):
                suite.executables(args)
        args.reference_only = True
        with mock.patch.object(suite.common, 'executable_evidence', return_value={'reference': 'only'}):
            self.assertEqual(suite.executables(args), {'reference': 'only'})

    def test_guard_runs_before_spawn_and_failure_has_no_fallback(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        runner = suite.RecordingRunner(dict(runs=[]), args, Path('/private'))
        with mock.patch.object(runner, 'guard', side_effect=ValueError('drift')), \
                mock.patch.object(suite.subprocess, 'Popen') as spawn:
            with self.assertRaisesRegex(ValueError, 'drift'):
                runner.run('unsafe', ['/wrong'], Path('/private'))
        spawn.assert_not_called()

    def test_shutdown_not_blocked_by_source_drift(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        runner = suite.RecordingRunner(dict(runs=[], executables={'reference': dict(path='/bin/true', sha256='a'*64)}),
                                       args, Path('/private'))
        process = mock.Mock(pid=321, returncode=0)
        process.communicate.return_value = (b'', b'')
        with mock.patch.object(runner, 'guard', side_effect=ValueError('drift')) as guard, \
                mock.patch.object(suite.subprocess, 'Popen', return_value=process):
            row = runner.run('host-os.shutdown', ['/bin/true'], Path('/private'))
        guard.assert_not_called()
        self.assertFalse(row['precommand_pins_verified'])


class DiagnosticsTests(unittest.TestCase):
    def test_extension_whole_json_rejection_requires_extension_callback(self):
        value = suite.fixture('native-boundary', 'extension.whole-json')
        text = 'HOST_EXT_CALLBACK\nerror: cannot encode unsupported value as JSON\n'
        suite.require_rejection(record(text), value, 'native')
        for wrong in (text.replace('HOST_EXT_CALLBACK\n', ''),
                      text.replace('HOST_EXT_CALLBACK', 'HOST_REPO_CALLBACK'),
                      text.replace('cannot encode', 'can encode'), text+'panicked\n'):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(wrong), value, 'native')

    def test_whole_json_is_native_boundary_not_genuine_observation(self):
        value = suite.fixture('native-boundary', 'direct.whole-json')
        suite.require_rejection(record('HOST_REPO_CALLBACK\nerror: cannot encode unsupported value as JSON\n'), value, 'native')
        with self.assertRaises(ValueError):
            suite.require_rejection(record('HOST_REPO_CALLBACK\nerror: missing os\n'), value, 'native')

    def test_immutability_requires_error_body_and_assignment_site(self):
        value = suite.fixture('immutable', 'extension.name')
        text = 'HOST_EXT_CALLBACK\nhost.name="controlled"\nError: cannot set .name field of repository_os value\n'
        suite.require_rejection(record(text), value, 'reference')
        for wrong in (text.replace('host.name="controlled"\n', ''), text.replace('.name field', '.arch field')):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(wrong), value, 'reference')

    def test_native_immutable_exact_planned_field_and_site_accepted(self):
        for probe in ('direct.name', 'ordinary.arch', 'extension.name'):
            with self.subTest(probe=probe):
                value, row, _, _ = native_immutable_record(probe)
                suite.require_rejection(row, value, 'native')

    def test_native_immutable_rejects_nearby_field_type_and_error_bodies(self):
        for probe in ('direct.name', 'ordinary.arch', 'extension.name'):
            value, row, _, _ = native_immutable_record(probe)
            field = probe.split('.')[1]
            other = 'arch' if field == 'name' else 'name'
            text = row['stderr']
            error = 'error: cannot set .'+field+' field of repository_os value'
            wrongs = [text.replace('.'+field+' field', '.'+other+' field'),
                      text.replace('repository_os value', 'struct value'),
                      text.replace(error, 'error: Immutable'),
                      text.replace(error, 'error: repository_os is immutable'),
                      text.replace(error, 'error: cannot set '+field),
                      text.replace(error, 'error: Operation `.'+field+'=` not supported on type `repository_os`'),
                      text.replace('error: cannot set', 'note: cannot set'),
                      text.replace(error, error+' extra'),
                      text+'error: Other\n', text+error+'\n']
            for wrong in wrongs:
                with self.subTest(probe=probe, wrong=wrong):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record(wrong), value, 'native')

    def test_native_immutable_rejects_wrong_or_detached_assignment_site(self):
        for probe in ('direct.name', 'ordinary.arch', 'extension.name'):
            value, row, source, line = native_immutable_record(probe)
            text = row['stderr']
            assignment = '    host.'+probe.split('.')[1]+'="controlled"'
            wrongs = [text.replace('//:'+source, '//:consumer.bzl'),
                      text.replace(':'+str(line)+':5', ':'+str(line+1)+':5'),
                      text.replace(':'+str(line)+':5', ':'+str(line)+':6'),
                      text.replace(str(line)+' | ', str(line+1)+' | '),
                      text.replace(str(line)+' | '+assignment, str(line)+' |     fail("controlled")'),
                      text.replace(' --> //:'+source+':'+str(line)+':5\n', ''),
                      text.replace(str(line)+' | '+assignment+'\n', '')]
            for wrong in wrongs:
                with self.subTest(probe=probe, wrong=wrong):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record(wrong), value, 'native')
            detached = copy.deepcopy(value)
            detached['files'][source] += b'# unplanned source\n'
            with self.assertRaisesRegex(ValueError, 'unplanned'):
                suite.require_rejection(row, detached, 'native')

    def test_native_immutable_rejects_wrong_phase_and_after_markers(self):
        for probe in ('direct.name', 'ordinary.arch', 'extension.name'):
            value, row, _, _ = native_immutable_record(probe)
            text = row['stderr']
            first = suite.markers(row)[0]
            wrongs = [text.replace(first+'\n', '', 1),
                      text.replace(first, 'HOST_ANALYSIS_CALLBACK', 1),
                      text+'HOST_AFTER_MUTATION\n', text+'HOST_REPO_CALLBACK\n']
            for wrong in wrongs:
                with self.subTest(probe=probe, wrong=wrong):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record(wrong), value, 'native')

    def test_native_immutable_rejects_crashes_process_errors_and_stdout(self):
        value, row, _, _ = native_immutable_record('direct.name')
        for code in (0, -9, 101, 124, 125, 126, 127):
            with self.subTest(code=code), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, returncode=code), value, 'native')
        for suffix in ('panicked\n', 'FATAL: crash\n', 'internal error\n'):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=row['stderr']+suffix), value, 'native')
        for output in ('//:result\t/private/result.txt\n', ' ', '\n'):
            with self.subTest(output=output), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stdout=output), value, 'native')
        for flag in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
            with self.subTest(flag=flag), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, **{flag: True}), value, 'native')

    def test_native_immutable_does_not_relax_reference_or_other_boundaries(self):
        value, row, _, _ = native_immutable_record('direct.name')
        # Since N33 the native body is Bazel's own text, so the native record
        # also satisfies the (unchanged) reference pattern; that is convergence,
        # not relaxation. Unknown engines are still refused.
        suite.require_rejection(row, value, 'reference')
        for engine in ('baseline', 'unknown'):
            with self.subTest(engine=engine), self.assertRaises(ValueError):
                suite.require_rejection(row, value, engine)
        for probe in ('extension.whole-json', 'ordinary.whole-json', 'direct.whole-json'):
            with self.subTest(probe=probe), self.assertRaises(ValueError):
                suite.require_rejection(row, suite.fixture('native-boundary', probe), 'native')

    def test_native_proxy_os_exact_recorded_type_property_and_preflight_site(self):
        value = suite.fixture('native-boundary', 'proxy.os')
        self.assertEqual(value['files']['MODULE.bazel'].decode().splitlines()[3], 'value=e.os.name')
        row = native_proxy_record()
        self.assertEqual(suite.markers(row), [])
        self.assertEqual(row['stdout'], '')
        self.assertTrue(row['stderr'].endswith('  |       ^^^^^^^^^\n  |\n\n'))
        suite.require_rejection(row, value, 'native')
        suite.require_rejection(native_proxy_record('/another/private/workspace'), value, 'native')

    def test_native_proxy_os_rejects_wrong_type_property_and_error_class(self):
        value = suite.fixture('native-boundary', 'proxy.os')
        row = native_proxy_record()
        text = row['stderr']
        for wrong in (text.replace("'TagCallable'", "'module_ctx'"),
                      text.replace("'TagCallable'", "'repository_os'"),
                      text.replace("'TagCallable'", "'module_extension_tag'"),
                      text.replace("method 'name'", "method 'os'"), text.replace("method 'name'", "method 'arch'"),
                      text.replace("error: 'TagCallable'", "note: 'TagCallable'"),
                      text.replace("method 'name'", "method 'name' extra"),
                      text.replace(text.splitlines()[0],
                                   'bazel-rs: error: Object of type `TagCallable` has no attribute `name`'),
                      text+'error: Other\n', text+text.splitlines()[0]+'\n',
                      text.replace(text.splitlines()[0], "error: name 'os' is not defined")):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=wrong), value, 'native')

    def test_native_proxy_os_rejects_wrong_site_source_and_cwd(self):
        value = suite.fixture('native-boundary', 'proxy.os')
        row = native_proxy_record()
        text = row['stderr']
        for wrong in (text.replace('/MODULE.bazel:4:7', '/defs.bzl:4:7'),
                      text.replace('/MODULE.bazel:4:7', '/MODULE.bazel:5:7'),
                      text.replace('/MODULE.bazel:4:7', '/MODULE.bazel:4:8'),
                      text.replace('4 | value=e.os.name', '5 | value=e.os.name'),
                      text.replace('4 | value=e.os.name', '4 | value=ctx.os.name'),
                      text.replace('4 | value=e.os.name\n', ''),
                      text.replace(' --> '+row['cwd']+'/MODULE.bazel:4:7\n', '')):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=wrong), value, 'native')
        for cwd in ('', 'relative/workspace', '/another/private/workspace', row['cwd']+'/../workspace'):
            with self.subTest(cwd=cwd), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, cwd=cwd), value, 'native')
        detached = copy.deepcopy(value)
        detached['files']['MODULE.bazel'] += b'# unplanned source\n'
        with self.assertRaisesRegex(ValueError, 'unplanned'):
            suite.require_rejection(row, detached, 'native')

    def test_native_proxy_os_rejects_callbacks_after_markers_actions_and_stdout(self):
        value = suite.fixture('native-boundary', 'proxy.os')
        row = native_proxy_record()
        for suffix in ('HOST_EXT_CALLBACK\n', 'HOST_REPO_CALLBACK\n', 'HOST_ANALYSIS_CALLBACK\n',
                       'HOST_BUILD_PHASE\n', 'HOST_AFTER_PROXY\n',
                       '1 actions executed, 0 cache hits\n', '0 actions executed, 1 cache hits\n'):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=row['stderr']+suffix), value, 'native')
        for output in ('//:result\t/private/result.txt\n', ' ', '\n'):
            with self.subTest(output=output), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stdout=output), value, 'native')

    def test_native_proxy_os_rejects_infrastructure_crash_and_process_failures(self):
        value = suite.fixture('native-boundary', 'proxy.os')
        row = native_proxy_record()
        for suffix in ('panicked\n', 'FATAL: crash\n', 'internal error\n', 'permission denied\n',
                       'failed to download\n', 'network unavailable\n'):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=row['stderr']+suffix), value, 'native')
        for code in (0, -9, 101, 124, 125, 126, 127):
            with self.subTest(code=code), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, returncode=code), value, 'native')
        for flag in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
            with self.subTest(flag=flag), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, **{flag: True}), value, 'native')

    def test_native_proxy_os_does_not_relax_reference_or_metadata_getter_boundaries(self):
        value = suite.fixture('native-boundary', 'proxy.os')
        row = native_proxy_record()
        # Also from a workspace path containing "tag" and "os" (a worktree
        # such as .../bazel-parity-staging/artifacts/host-os-*/workspace): the
        # generic pattern matches message text, never paths.
        tagged = native_proxy_record('/tmp/opencode/bazel-parity-staging/artifacts/host-os-x/workspace')
        for engine in ('reference', 'baseline', 'unknown'):
            for record in (row, tagged):
                with self.subTest(engine=engine, cwd=record['cwd']), self.assertRaises(ValueError):
                    suite.require_rejection(record, value, engine)
        for probe in ('extension.whole-json', 'ordinary.whole-json', 'direct.whole-json'):
            with self.subTest(probe=probe), self.assertRaises(ValueError):
                suite.require_rejection(row, suite.fixture('native-boundary', probe), 'native')

    def test_native_proxy_os_phase_rejects_any_repository_publication(self):
        # Portable mocked runner, not a native process or a parity receipt.
        with tempfile.TemporaryDirectory() as tmp:
            scratch = Path(tmp)
            workspace = scratch / 'workspace'
            workspace.mkdir()
            args = suite.argument_parser().parse_args([])
            item = next(c for c in suite.CASEPLAN if c['name'] == 'native-boundary')
            cache = scratch / 'cases/native-boundary/proxy.os/cache'
            (suite.direct.trees_directory(cache) / ('a'*64)).mkdir(parents=True)
            runner = mock.Mock(report={'phases': []})
            runner.run.return_value = dict(native_proxy_record(workspace), id=0)
            with self.assertRaisesRegex(ValueError, 'rejection published'):
                suite.run_phase(args, runner, scratch, workspace, None, item, 'proxy.os', 'native')

    def test_native_build_os_undefined_global_at_bound_compile_site(self):
        value = suite.fixture('native-boundary', 'build.os')
        self.assertEqual(value['files']['BUILD.bazel'], b'value=os.name\nfilegroup(name="result",srcs=[])\n')
        row = native_build_os_record()
        suite.require_rejection(row, value, 'native')
        self.assertEqual(suite.markers(row), [])
        self.assertIn('not full BUILD namespace parity', '\n'.join(suite.LIMITATIONS))

    def test_native_build_os_rejects_original_print_and_other_missing_globals(self):
        value = suite.fixture('native-boundary', 'build.os')
        row = native_build_os_record()
        original = ('bazel-rs: error: Variable `print` not found, did you mean `int`?\n'
                    ' --> /private/workspace/BUILD.bazel:1:1\n'
                    '  |\n1 | print("HOST_BUILD_PHASE")\n  | ^^^^^\n  |\n')
        for text in (original, row['stderr'].replace("name 'os'", "name 'print'"),
                     row['stderr'].replace("name 'os'", "name 'ctx'"),
                     row['stderr'].replace("name 'os' is not defined", 'Variable `os` not found'),
                     row['stderr'].replace("name 'os' is not defined", 'no such target result'),
                     row['stderr'].replace("name 'os' is not defined", 'syntax error')):
            with self.subTest(text=text), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=text), value, 'native')

    def test_native_build_os_rejects_wrong_source_location_and_unbound_fixture(self):
        value = suite.fixture('native-boundary', 'build.os')
        row = native_build_os_record()
        for text in (row['stderr'].replace('/BUILD.bazel:1:7', '/MODULE.bazel:1:7'),
                     row['stderr'].replace('/BUILD.bazel:1:7', '/BUILD.bazel:2:7'),
                     row['stderr'].replace('/BUILD.bazel:1:7', '/BUILD.bazel:1:1'),
                     row['stderr'].replace('1 | value=os.name', '1 | value=ctx.os.name'),
                     row['stderr'].replace('1 | value=os.name\n', ''),
                     row['stderr'].replace(' --> /private/workspace/BUILD.bazel:1:7\n', '')):
            with self.subTest(text=text), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=text), value, 'native')
        with self.assertRaises(ValueError):
            suite.require_rejection(dict(row, cwd='/other/workspace'), value, 'native')
        detached = copy.deepcopy(value)
        detached['files']['BUILD.bazel'] = b'print("HOST_BUILD_PHASE")\nvalue=os\n'
        with self.assertRaisesRegex(ValueError, 'unplanned'):
            suite.require_rejection(row, detached, 'native')

    def test_native_build_os_rejects_infrastructure_crashes_syntax_and_missing_targets(self):
        value = suite.fixture('native-boundary', 'build.os')
        row = native_build_os_record()
        for suffix in ('panicked\n', 'FATAL: crash\n', 'internal error\n', 'network unavailable\n',
                       'permission denied\n', 'failed to download\n', 'syntax error\n',
                       'no such target result\n', 'missing target result\n', 'error: Other\n'):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=row['stderr']+suffix), value, 'native')
        for code in (0, -9, 101, 124, 125, 126, 127):
            with self.subTest(code=code), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, returncode=code), value, 'native')

    def test_native_build_os_requires_zero_callbacks_actions_and_outputs(self):
        value = suite.fixture('native-boundary', 'build.os')
        row = native_build_os_record()
        for suffix in ('HOST_BUILD_PHASE\n', 'HOST_REPO_CALLBACK\n', 'HOST_EXT_CALLBACK\n',
                       'HOST_AFTER_BUILD\n', '1 actions executed, 0 cache hits\n', '0 actions executed, 1 cache hits\n'):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stderr=row['stderr']+suffix), value, 'native')
        for output in ('//:result\t/private/result.txt\n', ' ', '\n'):
            with self.subTest(output=output), self.assertRaises(ValueError):
                suite.require_rejection(dict(row, stdout=output), value, 'native')

    def test_native_build_os_phase_rejects_any_repository_publication(self):
        with tempfile.TemporaryDirectory() as tmp:
            scratch = Path(tmp)
            workspace = scratch / 'workspace'
            workspace.mkdir()
            args = suite.argument_parser().parse_args([])
            item = next(c for c in suite.CASEPLAN if c['name'] == 'native-boundary')
            cache = scratch / 'cases/native-boundary/build.os/cache'
            (suite.direct.trees_directory(cache) / ('a'*64)).mkdir(parents=True)
            runner = mock.Mock(report={'phases': []})
            runner.run.return_value = dict(native_build_os_record(workspace), id=0)
            with self.assertRaisesRegex(ValueError, 'rejection published'):
                suite.run_phase(args, runner, scratch, workspace, None, item, 'build.os', 'native')

    def test_constructor_error_must_precede_callbacks(self):
        value = suite.fixture('binding', 'os-int')
        text = "Error in module_extension: parameter 'os_dependent' got int, want bool\n"
        suite.require_rejection(record(text), value, 'reference')
        with self.assertRaises(ValueError):
            suite.require_rejection(record('HOST_EXT_CALLBACK\n'+text), value, 'reference')

    def test_process_failure_never_passes_as_rejection(self):
        value = suite.fixture('native-boundary', 'extension.whole-json')
        text = 'HOST_EXT_CALLBACK\ncannot encode unsupported value as JSON\n'
        for code in (0, -9, 101, 124, 125, 126, 127):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(text, code), value, 'native')


class EvidenceTests(unittest.TestCase):
    def test_changed_identity_and_restore_are_not_inferred_from_bytes(self):
        observations = [dict(roots={'data.txt': p}) for p in ('a', 'a', 'b', 'b', 'a')]
        suite.check_cache(observations, 'cache')
        for index, wrong in ((1, 'b'), (2, 'a'), (3, 'a'), (4, 'b')):
            changed = copy.deepcopy(observations)
            changed[index]['roots']['data.txt'] = wrong
            with self.assertRaises(ValueError):
                suite.check_cache(changed, 'cache')

    def test_lockfile_factor_and_typed_attrs_are_observed(self):
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            data = {'moduleExtensions': {'//:ext.bzl%ext': {'os:linux': {
                'generatedRepoSpecs': {'made': {'attributes': {'text': json.dumps(suite.golden('module_ctx'))}}}}}}}
            path = workspace / 'MODULE.bazel.lock'
            path.write_text(json.dumps(data))
            evidence = suite.lockfile_factors(workspace, '10')
            self.assertEqual(evidence['file']['sha256'], suite.digest(path.read_bytes()))
            with self.assertRaisesRegex(ValueError, 'factor'):
                suite.lockfile_factors(workspace, '00')

    def test_incomplete_or_optimistic_report_fails_closed(self):
        for report in ({}, {'status': 'PASS'}, {'status': 'REFERENCE_ONLY_PASS', 'errors': []},
                       {'configuration': {'reference_only': True}, 'errors': []}):
            self.assertFalse(suite.report_passes(report))

    def test_reference_argv_has_no_repo_env_or_host_override(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        scratch = Path('/private')
        ref = suite.DirectReference(args.reference, scratch / 'workspace', scratch, None, 'host-os')
        for command in ('build', 'cquery'):
            argv = suite.command_argv(args, ref, 'reference', scratch / 'cache', scratch / 'registry', command)
            self.assertIn('--lockfile_mode=update', argv)
            self.assertFalse(any(p.startswith('--repo_env') for p in argv))
            self.assertFalse(any('-Dos.name' in p or '-Dos.arch' in p for p in argv))


if __name__ == '__main__':
    unittest.main()


class MigrationCountsTest(unittest.TestCase):
    def test_first_new_build_rebuilds_once_after_manifest_v8(self):
        import validate_host_os as h
        self.assertEqual(h.migration_counts('migration', 2, True), (1, 0))
        self.assertEqual(h.migration_counts('migration', 3, True), (0, 1))
        self.assertEqual(h.migration_counts('migration', 1, True), (0, 1))
        self.assertEqual(h.migration_counts('migration', 0, False), (1, 0))
        self.assertEqual(h.migration_counts('native', 2, True), (0, 1))
