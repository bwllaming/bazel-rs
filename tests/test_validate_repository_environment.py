"""Portable harness tests. Synthetic records are NOT native/reference acceptance."""

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
import validate_repository_environment as suite


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, client_reaped=True, timed_out=False, **changes)


FROZEN_NATIVE_ERROR = '''bazel-rs: cannot evaluate //:consumer.bzl: Traceback (most recent call last):
  * //:consumer.bzl:2, in <module>
      NAMES.append('BAZEL_RS_ENV_B')
error: trying to mutate a frozen list value
 --> //:consumer.bzl:2:1
  |
2 | NAMES.append('BAZEL_RS_ENV_B')
  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  |
'''


class PlanTests(unittest.TestCase):
    def test_ten_families_complete_unique_plan(self):
        self.assertEqual(len(suite.CASEPLAN), 10)
        self.assertEqual(len(set(suite.PLAN)), 10)
        for item in suite.CASEPLAN:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe in item['probes']:
                value = suite.fixture(item['name'], probe)
                for path, raw in value['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        ast.parse(raw, filename=path)
                for engine in ('native', 'reference'):
                    steps = suite.phase_steps(item['name'], probe, engine)
                    self.assertEqual(sum(command == 'build' for _, command in steps), len(suite.states(item['name'], probe)))

    def test_unknown_case_has_no_fallback(self):
        for name, probe in [('invented', 'direct'), ('watched', 'direct.invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)

    def test_native_boundaries_never_reference_parity(self):
        for item in suite.CASEPLAN:
            pairs = suite.phases_for(item, True)
            self.assertTrue(all(engine == 'reference' for _, engine in pairs))
            if item['kind'] == 'native_boundary':
                self.assertEqual(pairs, [])
        with self.assertRaisesRegex(ValueError, 'frozen candidate'):
            suite.common.executable_evidence(suite.argument_parser().parse_args([]))

    def test_real_context_getter_template_and_consumer_both_routes(self):
        for route in suite.ROUTES:
            for variant in ('list', 'tuple', 'snapshot'):
                value = suite.fixture('watched', route+'.'+variant)
                defs = value['files']['defs.bzl']
                self.assertIn(b'ctx.getenv(', defs)
                self.assertIn(b'ctx.template(', defs)
                self.assertIn(b'CAPTURED=', defs)
                self.assertIn(b'local=False', defs)
                self.assertIn(b'attrs=', defs)
                self.assertIn(b'cat $(SRCS)', value['files']['BUILD.bazel'])
                if variant == 'snapshot':
                    self.assertGreater(defs.index(b'NAMES.append'), defs.index(b'r=repository_rule'))
                if route == 'ordinary':
                    self.assertIn(b'r(name="made")', value['files']['ext.bzl'])

    def test_module_getenv_is_a_differential_extension_input_not_a_boundary(self):
        # N21 implements module_ctx.getenv like Bazel; it is no longer a native
        # rejection probe and has no rejection classifier.
        item = next(c for c in suite.CASEPLAN if c['name'] == 'module-getenv')
        self.assertEqual((item['kind'], item['probes']), ('differential', ['ordinary']))
        self.assertEqual(suite.phases_for(item), [('ordinary', 'reference'), ('ordinary', 'native')])
        boundary = next(c for c in suite.CASEPLAN if c['name'] == 'native-boundary')
        self.assertNotIn('module-getenv', boundary['probes'])
        self.assertNotIn('module-getenv', suite.DIAGNOSTICS)
        value = suite.fixture('module-getenv', 'ordinary')
        self.assertIsNone(value['diagnostic'])
        self.assertFalse(value['template'])
        files = value['files']
        self.assertIn(b'r(name="made",text=ctx.getenv('+repr(suite.A).encode()+b',"MISSING"))', files['ext.bzl'])
        self.assertIn(b'use_extension("//:ext.bzl","e")', files['MODULE.bazel'])
        # The repository callback itself never observes the environment.
        self.assertNotIn(b'getenv', files['defs.bzl'])
        self.assertNotIn(b'os.environ', files['defs.bzl'])
        self.assertIn(b'environ=[]', files['defs.bzl'])
        self.assertIn(b'value=CAPTURED+ctx.attr.text+', files['defs.bzl'])
        self.assertEqual([suite.expected_callbacks('module-getenv', 'ordinary', 'reference', i) for i in range(5)],
                         [['A'], [], ['A'], [], ['A']])
        # N61: warm and restored states reuse the attr-keyed publication.
        self.assertEqual([suite.expected_callbacks('module-getenv', 'ordinary', 'native', i) for i in range(5)],
                         [['A'], [], ['A'], ['A'], []])
        outputs = [suite.expected_outputs('module-getenv', 'ordinary', env, source)['a.txt']
                   for _, env, source in suite.states('module-getenv', 'ordinary')]
        self.assertEqual(outputs, [b'prefix:alpha\n']*2+[b'prefix:beta\n']*2+[b'prefix:alpha\n'])

    def test_declared_unused_and_discarded_dynamic_really_differ(self):
        for route in suite.ROUTES:
            declared = suite.fixture('identity', route+'.declared')['files']['defs.bzl']
            dynamic = suite.fixture('identity', route+'.dynamic')['files']['defs.bzl']
            self.assertNotIn(b'ctx.getenv(', declared)
            self.assertIn(b'ctx.getenv(', dynamic)
            self.assertIn(b'environ=[]', dynamic)
            self.assertIn(b'"CONST"', declared)
            self.assertIn(b'"CONST"', dynamic)

    def test_engine_specific_callback_counts(self):
        # N61: warm, restored and source-warm states reuse the pointer-addressed
        # publication; changed client environments and changed sources execute.
        self.assertEqual([suite.expected_callbacks('watched', 'direct.snapshot', 'native', i) for i in range(7)],
                         [['A'], [], ['A'], ['A'], [], ['A'], []])
        self.assertEqual([suite.expected_callbacks('watched', 'direct.snapshot', 'reference', i) for i in range(7)],
                         [['A'], [], ['A'], [], ['A'], ['A'], []])
        self.assertEqual(suite.expected_callbacks('isolation', 'ordinary', 'reference', 2), ['A'])
        self.assertEqual(suite.expected_callbacks('isolation', 'ordinary', 'native', 2), ['A', 'B'])
        # Empty declared sets share one specification: the pointer follows the
        # latest world, so restoring re-executes like Bazel's external/<name>.
        self.assertEqual([suite.expected_callbacks('identity', 'direct.dynamic', 'native', i) for i in range(5)],
                         [['A'], [], ['A'], ['A'], ['A']])
        self.assertEqual([suite.expected_callbacks('defaults', 'direct', 'native', i) for i in range(5)],
                         [['A'], [], ['A'], ['A'], ['A']])

    def test_missing_empty_present_and_defaults(self):
        values = suite.states('defaults', 'direct')
        outputs = [suite.expected_outputs('defaults', 'direct', env, source)['a.txt'] for _, env, source in values]
        self.assertEqual(outputs, [b'prefix:None/None/DEFAULT\n']*2+[b'prefix://\n', b'prefix:alpha/alpha/alpha\n', b'prefix:None/None/DEFAULT\n'])

    def test_odd_names_are_real_getter_inputs_not_environment_enumeration(self):
        for variant in ('declared', 'dynamic'):
            defs = suite.fixture('odd', 'direct.'+variant)['files']['defs.bzl']
            self.assertIn(b'"", "=", "A=B"', defs)
            self.assertIn(b'\\000TAIL', defs)
            self.assertIn(b'ctx.getenv(n)', defs)
            self.assertNotIn(b'os.environ', defs)
        env = suite.states('odd', 'direct.declared')[2][1]
        self.assertEqual(suite.expected_outputs('odd', 'direct.declared', env, False)['a.txt'],
                         b'prefix:None/DEFAULT|None/DEFAULT|None/DEFAULT|None/DEFAULT|question/question\n')

    def test_binding_errors_use_real_getter_not_undefined_global(self):
        for probe in suite.CASEPLAN[5]['probes']:
            value = suite.fixture('binding', probe)
            self.assertIsNotNone(value['diagnostic'])
            if probe not in ('environ-none', 'environ-element', 'frozen'):
                self.assertIn(b'ctx.getenv(', value['files']['defs.bzl'])
                self.assertEqual(value['callbacks'], 1)
            else:
                self.assertEqual(value['callbacks'], 0)

    def test_repo_env_is_a_differential_override_not_a_native_rejection(self):
        # N40 implements --repo_env like Bazel; the former native repo-env-inline/
        # repo-env-split rejection probes and their classifier are gone.
        args = suite.argument_parser().parse_args([])
        scratch = Path('/private')
        ref = suite.DirectReference(args.reference, scratch / 'workspace', scratch, None, 'repository-environment')
        for engine in ('native', 'reference'):
            argv = suite.command_argv(args, ref, engine, scratch / 'cache', scratch / 'registry', 'build')
            self.assertFalse(any(x.startswith('--repo_env') for x in argv))
        boundary = next(c for c in suite.CASEPLAN if c['name'] == 'native-boundary')
        self.assertFalse(any(p.startswith('repo-env') for p in boundary['probes']))
        self.assertNotIn('repo-env', suite.DIAGNOSTICS)
        self.assertNotIn('repo-env', suite.NATIVE_ERRORS)
        item = next(c for c in suite.CASEPLAN if c['name'] == 'repo-env')
        self.assertEqual((item['kind'], item['probes']), ('differential', ['direct.inline', 'ordinary.split']))
        self.assertEqual(suite.phases_for(item), [('direct.inline', 'reference'), ('direct.inline', 'native'),
                                                  ('ordinary.split', 'reference'), ('ordinary.split', 'native')])
        for probe in item['probes']:
            value = suite.fixture('repo-env', probe)
            self.assertIsNone(value['diagnostic'])
            self.assertIn(b'environ=['+repr(suite.A).encode()+b','+repr(suite.A).encode()+b']', value['files']['defs.bzl'])
            self.assertIn(b'ctx.getenv('+repr(suite.A).encode()+b',"MISSING")', value['files']['defs.bzl'])
        self.assertEqual([suite.extras('repo-env', 'direct.inline', 'build', i) for i in range(6)],
                         [['--repo_env='+suite.A+'=override-1']]*2+[['--repo_env='+suite.A+'=override-2']]*2
                         + [['--repo_env='+suite.A+'=override-1'], []])
        self.assertEqual(suite.extras('repo-env', 'ordinary.split', 'cquery', 2),
                         ['--repo_env', suite.A+'=override-2', '--output=files'])
        self.assertEqual(suite.extras('repo-env', 'ordinary.split', 'cquery', 5), ['--output=files'])
        self.assertEqual(suite.extras('watched', 'direct.list', 'build', 0), [])
        self.assertFalse(any(suite.SECRET in x for i in range(6) for x in suite.extras('repo-env', 'direct.inline', 'build', i)))
        # The flag wins over the client value; an override-masked client change is
        # unrelated; dropping the flag falls back to the client value.
        outputs = [suite.expected_outputs('repo-env', 'direct.inline', env, source,
                                          suite.repo_env_override('repo-env', 'direct.inline', i))['a.txt']
                   for i, (_, env, source) in enumerate(suite.states('repo-env', 'direct.inline'))]
        self.assertEqual(outputs, [b'prefix:override-1\n']*2+[b'prefix:override-2\n']*2
                         + [b'prefix:override-1\n', b'prefix:alpha\n'])
        self.assertEqual(suite.states('repo-env', 'direct.inline')[3][1][suite.A], 'beta')
        self.assertEqual([suite.expected_callbacks('repo-env', 'ordinary.split', 'reference', i) for i in range(6)],
                         [['A'], [], ['A'], [], ['A'], ['A']])
        # N61: warm and restored overrides reuse; a changed override value or a
        # dropped flag executes.
        self.assertEqual([suite.expected_callbacks('repo-env', 'ordinary.split', 'native', i) for i in range(6)],
                         [['A'], [], ['A'], ['A'], [], ['A']])


class PrivacyTests(unittest.TestCase):
    def test_no_ambient_capture_even_during_construction_or_run(self):
        class ForbiddenEnvironment:
            def __getattribute__(self, key):
                raise AssertionError('ambient environment accessed')
            def __iter__(self):
                raise AssertionError('ambient environment enumerated')
        report = dict(runs=[], executables={'reference': {'path': '/bin/true', 'sha256': 'a'*64}})
        with mock.patch.object(suite.os, 'environ', ForbiddenEnvironment()):
            runner = suite.RecordingRunner(report, 2, Path('/private'))
            runner.values = {suite.A: 'alpha'}
            proc = mock.Mock(pid=123, returncode=0)
            proc.communicate.return_value = (b'', b'')
            with mock.patch.object(suite.subprocess, 'Popen', return_value=proc) as spawn:
                row = runner.run('safe', ['/bin/true'], Path('/private'))
            self.assertEqual(set(spawn.call_args.kwargs['env']), {'PATH', 'HOME', 'TMPDIR', 'LC_ALL', suite.A})
            self.assertEqual(set(row['controlled_environment']), {'PATH', 'HOME', 'TMPDIR', 'LC_ALL', suite.A})
            self.assertNotIn('environment_keys', row)
            self.assertTrue(row['client_reaped'])

    def test_explicit_child_mapping_rejects_unplanned_key(self):
        with self.assertRaisesRegex(ValueError, 'unplanned'):
            suite.controlled_environment(Path('/private'), {'UNPLANNED': 'not-used'})
        env = suite.controlled_environment(Path('/private'), {suite.A: b'synthetic-\xff'})
        self.assertEqual(base64.b64decode(suite.encoded_environment(env)[suite.A]), b'synthetic-\xff')

    def test_runner_reaps_timeout_owned_group_only(self):
        report = dict(runs=[], executables={'reference': {'path': '/bin/true', 'sha256': 'a'*64}})
        runner = suite.RecordingRunner(report, 2, Path('/private'))
        proc = mock.Mock(pid=123, returncode=-9)
        proc.communicate.side_effect = [suite.subprocess.TimeoutExpired('controlled', 2), (b'', b'')]
        with mock.patch.object(suite.subprocess, 'Popen', return_value=proc), mock.patch.object(suite.os, 'killpg') as kill:
            row = runner.run('safe', ['/bin/true'], Path('/private'))
        kill.assert_called_once_with(123, suite.signal.SIGKILL)
        self.assertTrue(row['timed_out'])
        self.assertTrue(row['client_reaped'])

    def test_metadata_schema_and_secret_rejection(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            p = cache / 'repositories/direct/manifests' / ('a'*64+'.json')
            p.parent.mkdir(parents=True)
            data = dict(format=1, profile='direct-file-template-v1', identity='a'*64, files={})
            p.write_text(json.dumps(data))
            suite.native_metadata(cache)
            for bad in (dict(data, environ={suite.A: suite.SECRET}), dict(data, profile=suite.SECRET), dict(data, profile=suite.A)):
                p.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    suite.native_metadata(cache)

    def test_metadata_accepts_n61_spec_and_recorded_probes_but_no_values(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            p = cache / 'repositories/direct/manifests' / ('a'*64+'.json')
            p.parent.mkdir(parents=True)
            probes = dict(version=1, watched_env=dict(sha256='c'*64, names=[suite.A, '?']))
            data = dict(format=1, profile='direct-file-template-v1', identity='a'*64, files={},
                        spec='b'*64, recorded=list(json.dumps(probes).encode()))
            p.write_text(json.dumps(data))
            suite.native_metadata(cache)
            # Values never persist, wherever they hide; unknown probe names and
            # schema growth beyond the N61 digest/probe fields stay rejected.
            for bad in (
                dict(data, recorded=list(json.dumps(dict(version=1, watched_env=dict(sha256='c'*64, names=['alpha']))).encode())),
                dict(data, recorded=list(json.dumps(dict(version=1, templates=[dict(source='input.tpl', sha256=suite.SECRET)])).encode())),
                dict(data, recorded=[999]),
                dict(data, recorded=[123, 34]),
                dict(data, recorded=dict(version=1)),
                dict(data, spec='not-a-digest'),
                dict(data, environ={suite.A: 'alpha'}),
            ):
                p.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    suite.native_metadata(cache)

    def test_no_ignored_research_report_dependency(self):
        self.assertTrue(all('artifacts' not in p.parts for p in suite.SOURCES))
        self.assertEqual(len(suite.OFFICIAL), 3)
        self.assertTrue(all(len(v) == 64 for v in suite.OFFICIAL.values()))


class DiagnosticTests(unittest.TestCase):
    def test_native_frozen_error_requires_exact_error_and_bound_mutation_site(self):
        suite.require_rejection(record(FROZEN_NATIVE_ERROR), 'frozen', 'native', 'frozen')
        error = 'error: trying to mutate a frozen list value'
        self.assertIn(error, FROZEN_NATIVE_ERROR.splitlines())
        for wrong in (
                error,
                # The pre-N20 native text is no longer the faithful one.
                FROZEN_NATIVE_ERROR.replace(error, 'error: Immutable'),
                FROZEN_NATIVE_ERROR.replace(error, 'error: Trying to mutate a frozen list value'),
                FROZEN_NATIVE_ERROR.replace(error, 'error: trying to mutate a frozen dict value'),
                FROZEN_NATIVE_ERROR.replace(error, error+' (list)'),
                FROZEN_NATIVE_ERROR.replace(error, 'note: trying to mutate a frozen list value'),
                FROZEN_NATIVE_ERROR.replace(error, 'error: Other'),
                FROZEN_NATIVE_ERROR.replace('//:consumer.bzl', '//:defs.bzl'),
                FROZEN_NATIVE_ERROR.replace('//:consumer.bzl:2:1', '//:consumer.bzl:3:1'),
                FROZEN_NATIVE_ERROR.replace("2 | NAMES.append('BAZEL_RS_ENV_B')", '2 | fail("Immutable")'),
                FROZEN_NATIVE_ERROR+'error: Other\n',
                FROZEN_NATIVE_ERROR+'panicked\n',
                FROZEN_NATIVE_ERROR+'name NAMES is not defined\n'):
            with self.subTest(text=wrong), self.assertRaises(ValueError):
                suite.require_rejection(record(wrong), 'frozen', 'native', 'frozen')
        for probe in (None, 'default-present', 'environ-element'):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(FROZEN_NATIVE_ERROR), 'frozen', 'native', probe)
        # The genuine reference classifier is intentionally unchanged; it accepts
        # Bazel's form and never the obsolete native 'Immutable' text.
        suite.require_rejection(record('cannot mutate frozen list'), 'frozen', 'reference', 'frozen')
        suite.require_rejection(record('Error in append: trying to mutate a frozen list value'), 'frozen', 'reference', 'frozen')
        with self.assertRaises(ValueError):
            suite.require_rejection(record(FROZEN_NATIVE_ERROR.replace(
                'error: trying to mutate a frozen list value', 'error: Immutable')), 'frozen', 'reference', 'frozen')

    def test_precise_categories_reject_infrastructure_and_missing_globals(self):
        messages = {'keyword': 'getenv got unexpected keyword argument name',
                    'name-type': 'getenv name must be a string', 'default-type': 'getenv default must be string or None',
                    'arity': 'getenv missing argument name', 'environ-shape': 'environ must be a list or tuple',
                    'environ-element': 'environ entries must be strings', 'frozen': 'cannot mutate frozen list',
                    'encoding': 'repository environment must contain ASCII text',
                    'limit': 'repository environment name limit exceeded'}
        for diagnostic, text in messages.items():
            suite.require_rejection(record(text), diagnostic)
            for extra in ('panicked', 'network unavailable', 'permission denied', 'internal error', 'name ctx is not defined'):
                with self.subTest(diagnostic=diagnostic, extra=extra), self.assertRaises(ValueError):
                    suite.require_rejection(record(text+'\n'+extra), diagnostic)
            for code in (0, True, -9, 101, 124, 125, 126, 127, 137):
                with self.assertRaises(ValueError):
                    suite.require_rejection(record(text, code), diagnostic)
            for flag in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
                row = record(text)
                row[flag] = True
                with self.assertRaises(ValueError):
                    suite.require_rejection(row, diagnostic)
        for key, message in suite.NATIVE_ERRORS.items():
            diagnostic = 'limit' if key.endswith('-limit') else key
            suite.require_rejection(record('error: '+message), diagnostic, 'native', key)
            for wrong in ('repository environment read failed', 'getenv is unsupported',
                          message+'; '+suite.SECRET, message+' changed',
                          'source snippet mentions '+message):
                with self.subTest(native_class=key, wrong=wrong), self.assertRaises(ValueError):
                    suite.require_rejection(record(wrong), diagnostic, 'native', key)
        # Native fixed-message matching cannot mistake one typed failure for a
        # different one or use a reference binder message as native evidence.
        for diagnostic, wrong in (
                ('default-type', suite.NATIVE_ERRORS['name-type']),
                ('name-type', suite.NATIVE_ERRORS['default-type']),
                ('arity', suite.NATIVE_ERRORS['encoding']),
                ('default-type', 'getenv default has type int, want string or None')):
            with self.assertRaises(ValueError):
                suite.require_rejection(record('error: '+wrong), diagnostic, 'native')
        for probe, other in (('name-limit', 'getter-limit'), ('getter-limit', 'name-limit')):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(suite.NATIVE_ERRORS[other]), 'limit', 'native', probe)
        with self.assertRaises(ValueError):
            suite.require_rejection(record(suite.NATIVE_ERRORS['name-limit']), 'limit', 'native')


class ReplayTests(unittest.TestCase):
    def test_native_frozen_replay_keeps_callback_action_and_source_barriers(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workspace = root / 'workspace'
            workspace.mkdir()
            value = suite.fixture('binding', 'frozen')
            suite.materialize(workspace, value)
            args = suite.argument_parser().parse_args([])
            ref = suite.DirectReference(args.reference, workspace, root, None, 'repository-environment')
            cache = root / 'cases/binding/frozen/native-cache'
            env = suite.states('binding', 'frozen')[0][1]
            row = record(FROZEN_NATIVE_ERROR)
            row.update(id=0, name='binding.frozen.native.0.build', cwd=str(workspace), timeout_seconds=args.timeout,
                       argv=suite.command_argv(args, ref, 'native', cache, root / 'registry', 'build'),
                       controlled_environment=suite.encoded_environment(suite.controlled_environment(root, env)))
            inputs = suite.snapshot(workspace / p for p in value['files'])
            obs = dict(index=0, command='build', inputs_before=inputs, inputs_after=copy.deepcopy(inputs),
                       outputs={}, roots={}, metadata={}, no_publication=True)
            phase = dict(probe='frozen', engine='native', cache=str(cache), runs=[0], observations=[obs], status='PASS')
            report = dict(runs=[row], scratch_directory=str(root))
            item = next(c for c in suite.CASEPLAN if c['name'] == 'binding')
            suite.validate_phase(phase, report, args, workspace, ref, item)
            for extra, expected in (('ENV_CALLBACK_A\n', 'callback count'),
                                    ('0 actions executed, 1 cache hits\n', 'reached actions'),
                                    ('1 actions executed, 0 cache hits\n', 'reached actions')):
                altered = copy.deepcopy(report)
                altered['runs'][0]['stderr'] += extra
                with self.assertRaisesRegex(ValueError, expected):
                    suite.validate_phase(phase, altered, args, workspace, ref, item)
            changed = copy.deepcopy(phase)
            changed['observations'][0]['no_publication'] = False
            with self.assertRaisesRegex(ValueError, 'published outputs'):
                suite.validate_phase(changed, report, args, workspace, ref, item)
            changed = copy.deepcopy(phase)
            raw = b'fail("Immutable")\n'
            source_path = str(workspace / 'consumer.bzl')
            for field in ('inputs_before', 'inputs_after'):
                changed['observations'][0][field][source_path].update(
                    bytes_base64=base64.b64encode(raw).decode(), sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable plan'):
                suite.validate_phase(changed, report, args, workspace, ref, item)

    def positive(self, root, name='identity', probe='direct.declared'):
        """Synthetic harness-only records with real temporary files, never CLI evidence."""
        item, engine = next(c for c in suite.CASEPLAN if c['name'] == name), 'native'
        value = suite.fixture(item['name'], probe)
        workspace = root / 'workspace'
        workspace.mkdir()
        suite.materialize(workspace, value)
        cache = root / 'cases' / name / probe / 'native-cache'
        args = suite.argument_parser().parse_args([])
        ref = suite.DirectReference(args.reference, workspace, root, None, 'repository-environment')
        phase = dict(probe=probe, engine=engine, cache=str(cache), runs=[], observations=[], status='PASS')
        report = dict(runs=[], scratch_directory=str(root))
        known, seen_payloads = set(), set()
        for i, (_, env, source) in enumerate(suite.states(name, probe)):
            override = suite.repo_env_override(name, probe, i)
            effective = env[suite.A] if override is None else override
            identity = ('c' if source else 'b' if effective in ('beta', 'override-2')
                        else 'd' if name == 'repo-env' and override is None else 'a')*64
            tree = suite.direct.trees_directory(cache) / identity
            tree.mkdir(parents=True, exist_ok=True)
            data = suite.expected_outputs(name, probe, env, source, override)
            # This fixture helper has one non-executable logical input. Derive
            # its synthetic action identity independently from repository identity.
            payload = data['a.txt']
            out = cache / 'actions' / suite.digest(payload) / 'result.txt'
            out.parent.mkdir(parents=True, exist_ok=True)
            if not out.exists():
                out.write_bytes(data['result.txt'])
            out.chmod(0o555)
            src = tree / 'a.txt'
            src.write_bytes(data['a.txt'])
            src.chmod(0o644)
            mp = cache / 'repositories/direct/manifests' / (identity+'.json')
            mp.parent.mkdir(parents=True, exist_ok=True)
            mp.write_text(json.dumps(dict(format=1, profile='direct-file-template-v1', identity=identity, files={})))
            hit = payload in seen_payloads
            seen_payloads.add(payload)
            known.add(identity)
            # N61: only states whose world differs from the pointer-addressed
            # publication execute the callback (expected_callbacks models it).
            markers = ''.join('ENV_CALLBACK_'+name+'\n'
                              for name in suite.expected_callbacks(item['name'], probe, engine, i))
            row = record(markers+('0 actions executed, 1 cache hits' if hit else '1 actions executed, 0 cache hits'), 0)
            row.update(id=i, name=name+'.'+probe+'.native.'+str(i)+'.build', cwd=str(workspace), timeout_seconds=args.timeout,
                       argv=suite.command_argv(args, ref, engine, cache, root / 'registry', 'build',
                                               suite.extras(name, probe, 'build', i)),
                       controlled_environment=suite.encoded_environment(suite.controlled_environment(root, env)),
                       stdout='//:result\t'+str(src)+'\n//:result\t'+str(out)+'\n')
            report['runs'].append(row)
            outputs = {p.name: dict(suite.evidence(p), logical_path=str(p)) for p in (src, out)}
            if value['template']:
                (workspace / 'input.tpl').write_bytes(b'@VALUE@source-v2\n' if source else value['files']['input.tpl'])
            inputs = suite.snapshot(workspace / p for p in value['files'])
            obs = dict(index=i, command='build', inputs_before=inputs, inputs_after=copy.deepcopy(inputs),
                       outputs=outputs, roots={'a.txt': str(tree)}, metadata=suite.native_metadata(cache), publications=sorted(known))
            phase['runs'].append(i)
            phase['observations'].append(obs)
        return phase, report, args, workspace, ref, item

    def test_positive_replay_and_mutation_guards(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.positive(Path(tmp))
            suite.validate_phase(*values)
            mutations = (
                lambda p, r: r['runs'][2].update(stderr='0 actions executed, 1 cache hits'),
                lambda p, r: r['runs'][1].update(stderr='ENV_CALLBACK_A\nENV_CALLBACK_A\n0 actions executed, 1 cache hits'),
                lambda p, r: r['runs'][2].update(stderr='ENV_CALLBACK_A\n1 actions executed, 0 cache hits'),
                lambda p, r: r['runs'][0]['controlled_environment'].update(UNPLANNED=''),
                lambda p, r: r['runs'][0].update(argv=['fake', 'build']),
                lambda p, r: p['observations'][0]['outputs']['a.txt'].update(sha256='0'*64),
                lambda p, r: p['observations'][0]['outputs']['a.txt'].update(logical_path='/invented'),
                lambda p, r: p['observations'][0]['outputs']['result.txt'].update(mode=0o755),
                lambda p, r: p['observations'][0]['publications'].append('.stage'),
                lambda p, r: p['observations'][0]['metadata'].clear(),
                lambda p, r: p['observations'][0]['inputs_before'].clear(),
                lambda p, r: p['runs'].pop(),
                lambda p, r: p['observations'].pop(),
                lambda p, r: p.update(cache='/wrong'),
            )
            for mutate in mutations:
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises((ValueError, KeyError, IndexError)):
                    suite.validate_phase(phase, report, *values[2:])

    def test_repo_env_native_replay_uses_the_generic_identity_rules(self):
        for probe in ('direct.inline', 'ordinary.split'):
            with self.subTest(probe=probe), tempfile.TemporaryDirectory() as tmp:
                values = self.positive(Path(tmp), 'repo-env', probe)
                suite.validate_phase(*values)
                roots = [o['roots']['a.txt'] for o in values[0]['observations']]
                self.assertEqual((roots[0], roots[2], roots[4]), (roots[1], roots[3], roots[0]))
                self.assertNotIn(roots[5], roots[:5])
                for mutate in (lambda r: r['runs'][2].update(argv=r['runs'][0]['argv']),
                               lambda r: r['runs'][5].update(argv=r['runs'][4]['argv'])):
                    phase, report = copy.deepcopy(values[:2])
                    mutate(report)
                    with self.assertRaisesRegex(ValueError, 'provenance'):
                        suite.validate_phase(phase, report, *values[2:])

    def test_module_getenv_native_replay_uses_the_generic_identity_rules(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.positive(Path(tmp), 'module-getenv', 'ordinary')
            suite.validate_phase(*values)
            roots = [o['roots']['a.txt'] for o in values[0]['observations']]
            self.assertEqual((roots[0], roots[2]), (roots[1], roots[3]))
            self.assertNotEqual(roots[0], roots[2])
            self.assertEqual(roots[0], roots[4])
            # A warm invocation reuses the publication: its callback must not
            # run, and an executing state must run it exactly once.
            phase, report = copy.deepcopy(values[:2])
            report['runs'][2]['stderr'] = '0 actions executed, 1 cache hits'
            with self.assertRaisesRegex(ValueError, 'callback count'):
                suite.validate_phase(phase, report, *values[2:])
            phase, report = copy.deepcopy(values[:2])
            report['runs'][1]['stderr'] = 'ENV_CALLBACK_A\n' + report['runs'][1]['stderr']
            with self.assertRaisesRegex(ValueError, 'callback count'):
                suite.validate_phase(phase, report, *values[2:])

    def test_constant_bytes_hit_cache_but_must_change_repository_identity(self):
        for route in suite.ROUTES:
            for variant in ('declared', 'dynamic'):
                with self.subTest(route=route, variant=variant), tempfile.TemporaryDirectory() as tmp:
                    values = self.positive(Path(tmp), probe=route+'.'+variant)
                    phase, report = values[:2]
                    suite.validate_phase(*values)
                    roots = [o['roots']['a.txt'] for o in phase['observations']]
                    self.assertNotEqual(roots[0], roots[2])
                    self.assertEqual(roots[0], roots[4])
                    self.assertEqual(len({o['outputs']['result.txt']['path'] for o in phase['observations']}), 1)
                    for i, row in enumerate(report['runs']):
                        suite.common.action_counts(row, (1, 0) if i == 0 else (0, 1))
                    # Fabricate coherent publication/path evidence claiming the
                    # watched change was discarded. Correct cache hits cannot
                    # excuse the independent repository identity violation.
                    original_root = roots[0]
                    original_manifest = next(iter(phase['observations'][0]['metadata']))
                    for obs, row in zip(phase['observations'], report['runs']):
                        entry = obs['outputs']['a.txt']
                        replacement = str(Path(original_root) / 'a.txt')
                        row['stdout'] = row['stdout'].replace(entry['logical_path'], replacement)
                        entry.update(path=replacement, logical_path=replacement)
                        obs['roots']['a.txt'] = original_root
                        obs['publications'] = [Path(original_root).name]
                        obs['metadata'] = {original_manifest: obs['metadata'][original_manifest]}
                    with self.assertRaisesRegex(ValueError, 'watched change did not affect input identity'):
                        suite.validate_phase(*values)

    def test_changed_bytes_execute_and_restored_or_warm_bytes_hit_both_routes(self):
        expected = [(1, 0), (0, 1), (1, 0), (0, 1), (0, 1), (1, 0), (0, 1)]
        for route in suite.ROUTES:
            with self.subTest(route=route), tempfile.TemporaryDirectory() as tmp:
                values = self.positive(Path(tmp), 'watched', route+'.snapshot')
                suite.validate_phase(*values)
                for row, counts in zip(values[1]['runs'], expected):
                    suite.common.action_counts(row, counts)
                for i, counts in enumerate(expected):
                    phase, report = copy.deepcopy(values[:2])
                    executed, hits = counts[::-1]
                    markers = ''.join('ENV_CALLBACK_'+name+'\n'
                                      for name in suite.expected_callbacks('watched', route+'.snapshot', 'native', i))
                    report['runs'][i]['stderr'] = markers+'%d actions executed, %d cache hits' % (executed, hits)
                    with self.subTest(index=i), self.assertRaisesRegex(ValueError, 'action counts'):
                        suite.validate_phase(phase, report, *values[2:])

    def test_action_expectations_never_adopt_corrupted_observed_bytes_or_modes(self):
        for route in suite.ROUTES:
            with self.subTest(route=route), tempfile.TemporaryDirectory() as tmp:
                values = self.positive(Path(tmp), 'watched', route+'.list')
                for replace_bytes in (True, False):
                    phase, report = copy.deepcopy(values[:2])
                    # Pretend beta still produced alpha and hit the consumer cache.
                    # Even internally consistent/rehashed output records must fail
                    # expected bytes/modes before the cache model can use them.
                    current = phase['observations'][2]
                    for name in ('a.txt', 'result.txt'):
                        entry = current['outputs'][name]
                        if replace_bytes:
                            original = phase['observations'][0]['outputs'][name]
                            for field in ('bytes_base64', 'sha256', 'size_bytes'):
                                entry[field] = original[field]
                        else:
                            entry['mode'] = 0o755 if name == 'a.txt' else 0o644
                    report['runs'][2]['stderr'] = 'ENV_CALLBACK_A\n0 actions executed, 1 cache hits'
                    with self.assertRaisesRegex(ValueError, 'artifact bytes/mode/provenance mismatch'):
                        suite.validate_phase(phase, report, *values[2:])

    def test_action_signature_preserves_individual_logical_inputs_not_just_output(self):
        first = {'a.txt': b'a', 'b.txt': b'bc', 'result.txt': b'abc'}
        second = {'a.txt': b'ab', 'b.txt': b'c', 'result.txt': b'abc'}
        self.assertNotEqual(suite.expected_action_inputs(first), suite.expected_action_inputs(second))
        self.assertEqual(suite.expected_action_inputs(first),
                         (('@made//:a.txt', b'a', 0), ('@second//:b.txt', b'bc', 0)))

    def test_honestly_rehashed_source_is_not_fixture_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            phase, report, *rest = self.positive(Path(tmp))
            p = next(p for p in phase['observations'][0]['inputs_before'] if p.endswith('defs.bzl'))
            raw = b'changed\n'
            for field in ('inputs_before', 'inputs_after'):
                phase['observations'][0][field][p].update(bytes_base64=base64.b64encode(raw).decode(), sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable plan'):
                suite.validate_phase(phase, report, *rest)

    def test_pass_labels_without_complete_evidence_never_pass(self):
        for report in ({}, {'status': 'PASS'}, {'status': 'REFERENCE_ONLY_PASS'}, {'checks': []}):
            self.assertFalse(suite.report_passes(report))


if __name__ == '__main__':
    unittest.main()
