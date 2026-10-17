"""Synthetic mutation guards only: never evidence of native/reference execution."""

import ast
import base64
import copy
import json
import shutil
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_run_environment_info as suite


def setUpModule():
    """Every test runs with access to the ignored original receipt forbidden."""
    forbidden = suite.ROOT / suite.KNOWN_PARITY_GAPS[0]['retained_receipt']['path']
    actual_evidence, actual_open = suite.direct.evidence, Path.open

    def guarded_evidence(path):
        if Path(path).resolve() == forbidden:
            raise AssertionError('unit attempted to read ignored original receipt')
        return actual_evidence(path)

    def guarded_open(path, *args, **kwargs):
        if path.resolve() == forbidden:
            raise AssertionError('unit attempted to read ignored original receipt')
        return actual_open(path, *args, **kwargs)

    for patch in (mock.patch.object(suite.direct, 'evidence', side_effect=guarded_evidence),
            mock.patch.object(Path, 'open', guarded_open)):
        patch.start()
        unittest.addModuleCleanup(patch.stop)


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **changes)


class PlanTests(unittest.TestCase):
    def test_exact_plan_and_lexical_fixtures(self):
        self.assertEqual(len(suite.CASEPLAN), 9)
        # 57 probes and 113 phases since N23f removed the run-command probe.
        self.assertEqual(sum(len(c['probes']) for c in suite.CASEPLAN), 57)
        self.assertEqual(suite.plan_receipt()['phases'], 113)
        self.assertEqual(suite.plan_receipt(True)['phases'], 56)
        for item in suite.CASEPLAN:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe in item['probes']:
                v = suite.fixture(item['name'], probe)
                for path, raw in v['files'].items():
                    self.assertNotIn(b'\0', raw)
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(family=item['name'], probe=probe, path=path):
                            ast.parse(raw.decode(), filename=path)
                if v['diagnostic']:
                    self.assertIn(v['diagnostic'], suite.DIAGNOSTICS)
                    self.assertEqual(v['failure_command'], v.get('command') or ('query' if v['stage'] == 'loading' else 'build'))
                elif v.get('query_only'):
                    self.assertEqual(suite.steps_for(v, 'native'), [('query', '//app:check', [])])
                    self.assertEqual(suite.steps_for(v, 'reference'), [('query', '//app:check', [])])
                else:
                    self.assertEqual(sum(c == 'build' for c, _, _ in suite.steps_for(v, 'native')), 2)

    def test_unknown_and_boundary_engines(self):
        for family, probe in [('invented', 'x'), ('constructor', 'invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(family, probe)
        self.assertEqual(suite.phases_for(suite.CASEPLAN[-1], True), [])
        self.assertTrue(all(e == 'native' for _, e in suite.phases_for(suite.CASEPLAN[-1])))

    def test_executable_and_test_rule_declarations_are_differential_until_analysis(self):
        # Native loads rule(executable=True)/rule(test=True) like Bazel, so
        # query and the _test class-name check are paired with genuine Bazel.
        item = next(c for c in suite.CASEPLAN if c['name'] == 'rule-kind')
        self.assertEqual(item['kind'], 'differential')
        self.assertEqual(suite.phases_for(item, True), [(p, 'reference') for p in item['probes']])
        for probe, symbol, flag in (('executable-query', b'check', b',executable=True)'),
                                    ('test-query', b'check_test', b',test=True)')):
            v = suite.fixture('rule-kind', probe)
            self.assertIn(symbol+b'=rule(implementation=_impl,attrs={}'+flag, v['files']['defs/check.bzl'])
            self.assertIn(symbol+b'(name="check")', v['files']['app/BUILD.bazel'])
            self.assertEqual((v['diagnostic'], v['stage'], v['queries'], v['expected'], v['callback']),
                             (None, 'query', [('//app:check', ['//app:check'])], {}, False))
            self.assertEqual(v['returncodes'], {'native': 0, 'reference': 0})
        for probe, symbol, flag in (('test-misnamed', 'check', b',test=True)'), ('plain-misnamed', 'check_test', b')')):
            v = suite.fixture('rule-kind', probe)
            self.assertIn(symbol.encode()+b'=rule(implementation=_impl,attrs={}'+flag, v['files']['defs/check.bzl'])
            self.assertEqual((v['diagnostic'], v['stage'], v['failure_command']), ('misnamed-'+symbol, 'loading', 'query'))
            self.assertEqual(v['returncodes'], {'native': 1, 'reference': 7})
            message = ("Invalid rule class name '"+symbol+"', test rule class names must end with '_test'"
                       " and other rule classes must not")
            observed = {'native': 'bazel-rs: cannot evaluate //defs:check.bzl: error: '+message,
                        'reference': 'ERROR: /w/defs/check.bzl:9:7: '+message}
            for engine, text in observed.items():
                suite.require_rejection(record(text, v['returncodes'][engine]), v['diagnostic'], v['returncodes'][engine])
            other = 'check_test' if symbol == 'check' else 'check'
            for text in (message.replace("'"+symbol+"'", "'"+other+"'"), message+' (suffix)',
                         'ERROR: source excerpt: '+message[:-4], 'bazel-rs: test rules are unsupported'):
                with self.subTest(probe=probe, text=text), self.assertRaises(ValueError):
                    suite.require_rejection(record(text, 7), v['diagnostic'], 7)

    def test_executable_and_test_builds_are_differential(self):
        # N23a: executable/test rule targets are analyzed and built natively.
        item = next(c for c in suite.CASEPLAN if c['name'] == 'rule-build')
        self.assertEqual(item['kind'], 'differential')
        self.assertEqual(suite.phases_for(item, True), [(p, 'reference') for p in item['probes']])
        for probe, symbol in (('executable', 'check'), ('test', 'check_test')):
            v = suite.fixture('rule-build', probe)
            kind = probe.encode()
            self.assertIn(symbol.encode()+b'=rule(implementation=_impl,attrs={},'+kind+b'=True)', v['files']['defs/check.bzl'])
            self.assertIn(b'return [DefaultInfo(executable=out),'+suite.INFO.encode()+b']', v['files']['defs/check.bzl'])
            self.assertIn(symbol.encode()+b'(name="check")', v['files']['app/BUILD.bazel'])
            self.assertEqual((v['diagnostic'], v['stage'], v['callback'], v['actions']), (None, 'success', True, 1))
            self.assertEqual(v['expected'], {'check.out': b'struct(environment = {}, inherited_environment = [])'})
            self.assertEqual(v['returncodes'], {'native': 0, 'reference': 0})
            self.assertEqual(sum(c == 'build' for c, _, _ in suite.steps_for(v, 'native')), 2)
            self.assertEqual(sum(c == 'cquery' for c, _, _ in suite.steps_for(v, 'reference')), 2)
        for probe, symbol in (('executable-missing', 'check'), ('test-missing', 'check_test')):
            v = suite.fixture('rule-build', probe)
            self.assertIn(b'return [DefaultInfo(files=depset([out]))]', v['files']['defs/check.bzl'])
            self.assertEqual((v['diagnostic'], v['stage'], v['failure_command'], v['callback']),
                             ('missing-executable-'+symbol, 'callback', 'build', True))
            self.assertEqual(v['returncodes'], {'native': 1, 'reference': 1})
            message = ("The rule '"+symbol+"' is executable. It needs to create an executable File and pass it as"
                       " the 'executable' parameter to the DefaultInfo it returns.")
            observed = {'native': 'bazel-rs: analysis of //app:check: '+message,
                        'reference': 'ERROR: /w/app/BUILD.bazel:2:11: in '+symbol+' rule //app:check: \n/w/defs/check.bzl:7:19: '+message}
            for engine, text in observed.items():
                suite.require_rejection(record(text), v['diagnostic'], 1)
            other = 'check_test' if symbol == 'check' else 'check'
            for text in (message.replace("'"+symbol+"'", "'"+other+"'"), message+' x', message[:-1]):
                with self.subTest(probe=probe, text=text), self.assertRaises(ValueError):
                    suite.require_rejection(record(text), v['diagnostic'], 1)

    def test_reference_test_rule_registry_patches_are_preflighted_and_sri_pinned(self):
        # Genuine Bazel fetches these modules to resolve the test toolchain type for a
        # test rule; a file registry without their patches fails the reference (i11 run).
        pins = suite.require_registry_patches(suite.REGISTRY)
        self.assertEqual(sorted(pins), sorted(m+'/module_dot_bazel_version.patch' for m in suite.TEST_TOOLCHAIN_PATCHED_MODULES))
        self.assertEqual(pins['rules_shell/0.6.1/module_dot_bazel_version.patch']['sha256'],
                         '5f0700eaa9a33770aae4ae8b06bec8e433f518eb50711378c8cd3a5d7854ff2d')
        self.assertTrue(any('test toolchain' in line or 'test probes' in line for line in suite.LIMITATIONS))
        target = 'modules/rules_shell/0.6.1/patches/module_dot_bazel_version.patch'
        for fault in ('missing', 'changed', 'symlink'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
                registry = Path(tmp) / 'registry'
                shutil.copytree(suite.REGISTRY, registry)
                path = registry / target
                if fault == 'missing':
                    path.unlink()
                elif fault == 'changed':
                    path.write_bytes(path.read_bytes() + b'\n')
                else:
                    raw = path.read_bytes()
                    path.unlink()
                    (Path(tmp) / 'outside.patch').write_bytes(raw)
                    path.symlink_to(Path(tmp) / 'outside.patch')
                with self.assertRaisesRegex(ValueError, 'registry patch'):
                    suite.require_registry_patches(registry)

    def test_test_command_is_the_exact_native_boundary(self):
        item = suite.CASEPLAN[-1]
        self.assertEqual((item['name'], item['kind']), ('native-boundary', 'native_boundary'))
        self.assertEqual(suite.phases_for(item, True), [])
        # `run` is implemented since N23f (docs/RUNFILES_TREES.md).
        self.assertNotIn('run-command', item['probes'])
        for probe, command, symbol in (('test-command', 'test', 'check_test'),):
            v = suite.fixture('native-boundary', probe)
            self.assertIn(b'return [DefaultInfo(executable=out)]', v['files']['defs/check.bzl'])
            self.assertIn(symbol.encode()+b'(name="check")', v['files']['app/BUILD.bazel'])
            self.assertEqual((v['stage'], v['failure_command'], v['diagnostic'], v['callback'], v['markers']),
                             ('command', command, probe, False, []))
            self.assertEqual(suite.steps_for(v, 'native'), [('query', '//app:check', []), (command, '//app:check', [])])
            message = 'bazel-rs: unsupported command or startup option "'+command+'"; run --help for the native subset'
            suite.require_rejection(record(message), v['diagnostic'], 1)
            suite.require_markers(record(message), v, command, False, 'native', True)
            with self.assertRaises(ValueError):
                suite.require_markers(record(suite.ENTERED+'\n'+message), v, command, False, 'native', True)
            other = 'run' if command == 'test' else 'test'
            for text in (message.replace('"'+command+'"', '"'+other+'"'), message+' x', '  '+message,
                         'error: '+command+' is not supported'):
                with self.subTest(probe=probe, text=text), self.assertRaises(ValueError):
                    suite.require_rejection(record(text), v['diagnostic'], 1)

    def test_unicode_nul_not_normalized_or_os_environment(self):
        v = suite.fixture('constructor', 'unicode-nul')
        raw = v['files']['defs/check.bzl']
        self.assertIn(b'\\000', raw)
        self.assertNotIn(b'\\x00', raw)
        self.assertIn('é'.encode(), v['expected']['check.out'])
        self.assertIn(b'\\x00', v['expected']['check.out'])
        self.assertNotIn(b'run_shell', raw)

    def test_structural_hash_uses_assignment_not_duplicate_literal(self):
        raw = suite.fixture('ownership', 'structural-hash')['files']['defs/check.bzl']
        self.assertIn(b'd={a:"before"}\n    d[b]="after"', raw)
        self.assertNotIn(b'{a:', raw.split(b'd={a:', 1)[-1])
        self.assertIn(b'Other(environment=', raw)
        self.assertIn(b'["B","A"]', raw)

    def test_nested_transport_and_multihop_real_providers(self):
        v = suite.fixture('transport', 'nested')
        raw = v['files']['defs/check.bzl']
        self.assertIn(b'return [P(info=R(', raw)
        self.assertIn(b'providers=[P]', raw)
        self.assertIn(b'R in t', raw)
        self.assertTrue(v['expected']['check.out'].startswith(b'[False,'))
        relay = suite.fixture('transport', 'multi-hop')['files']['defs/relay.bzl']
        self.assertIn(b'p=ctx.attr.dep[P].info', relay)
        self.assertIn(b'd["Z"]="changed"', relay)
        self.assertIn(b'n.append("extra")', relay)

    def test_explicit_actions_do_not_apply_inherited_metadata(self):
        for probe, expected in [('implicit', b'unset'), ('explicit', b'LITERAL'), ('all-entries', b'LITERAL:SECOND')]:
            v = suite.fixture('action', probe)
            raw = v['files']['defs/check.bzl']
            self.assertEqual(v['expected']['check.out'], expected)
            self.assertEqual(b'env=p.environment' in raw, probe != 'implicit')
            self.assertNotIn(b'use_default_shell_env=True', raw)
            self.assertNotIn(b'printenv', raw)
            self.assertNotIn(b'os.environ', raw)
        self.assertTrue(all(k.startswith('RUNENV_CONTRACT_') for k in suite.HOST))

    def test_ordinary_return_and_atomic_order(self):
        for probe in suite.CASEPLAN[3]['probes']:
            v = suite.fixture('phase', probe)
            self.assertEqual(v['stage'], 'callback')
            self.assertEqual(v['queries'], [('//app:check', ['//app:check'])])
            self.assertIn(b'rejected.marker', v['files']['defs/check.bzl'])
            self.assertEqual(v['expected'], {})
        self.assertEqual(suite.fixture('phase', 'atomic')['target'], ['//app:good', '//app:check'])

    def test_saved_properties_freeze_separately_from_fresh(self):
        for probe in ('saved-dict', 'saved-list'):
            self.assertEqual(suite.fixture('ownership', probe)['diagnostic'], 'frozen')
        self.assertIsNone(suite.fixture('ownership', 'frozen-owner')['diagnostic'])

    def test_constructor_errors_and_phase_specific_requirements(self):
        self.assertEqual(suite.fixture('transport', 'required')['stage'], 'preflight')
        self.assertEqual(suite.fixture('transport', 'instance-requirement')['stage'], 'loading')
        self.assertEqual(suite.fixture('transport', 'missing-key')['stage'], 'callback')
        self.assertIn(b'error_on_non_executable=False', suite.fixture('constructor', 'hidden-flag')['files']['defs/check.bzl'])
        self.assertEqual(suite.fixture('constructor', 'hidden-flag')['diagnostic'], 'hidden-flag')
        self.assertEqual(suite.fixture('constructor', 'hidden-flag')['returncodes'], {'native': 1, 'reference': 7})
        self.assertEqual(suite.fixture('module', 'module-global')['returncodes']['reference'], 48)

    def test_frozen_sha_required_and_reference_never_executes_native(self):
        args = suite.argument_parser().parse_args([])
        for sha in (None, '', 'old', '0'*63):
            args.native_sha256 = sha
            with self.assertRaisesRegex(ValueError, 'frozen candidate'):
                suite.common.executable_evidence(args)
        args.native_sha256 = 'a'*64
        with mock.patch.object(suite.common, 'validate_binaries', return_value={'native': {'sha256': 'b'*64}}):
            with self.assertRaisesRegex(ValueError, 'differs from frozen'):
                suite.common.executable_evidence(args)
        args.reference_only = True
        with mock.patch.object(suite.common, 'validate_executable'), mock.patch.object(
                suite.common, 'file_evidence', return_value={'sha256': suite.REFERENCE_SHA256}), mock.patch.object(
                suite.common, 'validate_binaries') as native:
            suite.common.executable_evidence(args)
            native.assert_not_called()

    def test_predeclared_output_uses_real_attribute_and_default_files_fallback(self):
        v = suite.fixture('transport', 'predeclared')
        raw = v['files']['defs/check.bzl']
        self.assertIn(b'attrs={"out":attr.output(mandatory=True)}', raw)
        self.assertNotIn(b'outputs={', raw)
        self.assertIn(b'produce(name="producer",out="producer.out")', v['files']['app/BUILD.bazel'])
        self.assertIn(b'ctx.actions.write(ctx.outputs.out,"fallback",is_executable=True)', raw)
        producer = raw.split(b'def _produce(ctx):', 1)[1].split(b'produce=rule(', 1)[0]
        self.assertIn(b'return [P(info=R(', producer)
        self.assertIn(b',Other()]', producer)
        self.assertNotIn(b'DefaultInfo', producer)
        self.assertEqual(v['target'], '//app:producer')
        self.assertEqual(v['expected'], {'producer.out': b'fallback'})
        self.assertEqual(v['actions'], 1)

    def test_module_global_materialized_at_actual_invocation_cwd(self):
        # A runner spy, NOT native execution: verifies the bytes passed to the CLI.
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            root = Path(tmp)
            workspace = root / 'workspace'
            workspace.mkdir()
            args = suite.argument_parser().parse_args(['--native-sha256', 'a'*64])
            reference = suite.DirectReference(args.reference, workspace, root, None, 'run-environment-info')
            item = next(c for c in suite.CASEPLAN if c['name'] == 'module')
            runner = mock.Mock()

            def inspect(name, argv, cwd):
                self.assertEqual(cwd, workspace)
                self.assertEqual(argv[0], str(args.native))
                self.assertIn('query', argv)
                self.assertEqual(argv[-1], '@missing//:all')
                self.assertEqual((cwd / 'MODULE.bazel').read_bytes(),
                    b'module(name="runenv_contract")\nx=RunEnvironmentInfo()\n')
                return record('error: Variable `RunEnvironmentInfo` not found', 1, id=0)
            runner.run.side_effect = inspect
            phase = suite.run_phase(args, runner, root, workspace, reference, item, 'module-global', 'native')
            runner.run.assert_called_once()
            self.assertEqual(phase['status'], 'PASS')
            self.assertEqual(phase['inputs_before'], phase['inputs_after'])
            v = suite.fixture('module', 'module-global')
            self.assertEqual(v['returncodes'], {'native': 1, 'reference': 48})
            with self.assertRaises(ValueError):
                suite.require_rejection(record('', 0), v['diagnostic'], v['returncodes']['native'])


class DiagnosticTests(unittest.TestCase):
    def test_local_root_probe_precedes_external_regression_and_rejects_loading(self):
        item = next(c for c in suite.CASEPLAN if c['name'] == 'module')
        self.assertLess(item['probes'].index('module-global-local'), item['probes'].index('module-global'))
        value = suite.fixture('module', 'module-global-local')
        self.assertEqual(suite.steps_for(value, 'native'), [('query', '//app:check', [])])
        self.assertEqual(suite.digest(value['files']['MODULE.bazel']), suite.KNOWN_PARITY_GAPS[0]['module']['sha256'])
        self.assertEqual(value['returncodes'], {'native': 1, 'reference': 48})
        for marker in (suite.READY, suite.ENTERED):
            with self.assertRaisesRegex(ValueError, 'before root MODULE'):
                suite.require_markers(record(marker), value, 'query', True, 'native', True)

    def test_closure_receipt_requires_actual_new_success_and_mode(self):
        phases = [dict(probe='module-global-local', engine=engine, runs=[i], status='PASS')
            for i, engine in enumerate(('reference', 'native'))]
        report = dict(checks=[dict(name='module', phases=phases)], runs=[dict(returncode=48), dict(returncode=1)])
        self.assertTrue(suite.closure_receipt(report, False)['native_closure_verified'])
        self.assertFalse(suite.closure_receipt(report, True)['native_closure_verified'])
        for mutate in (lambda r: r['runs'][1].update(returncode=0),
                lambda r: r['checks'][0]['phases'][1].update(status='FAIL'),
                lambda r: r['checks'][0]['phases'].pop()):
            changed = copy.deepcopy(report)
            mutate(changed)
            self.assertFalse(suite.closure_receipt(changed, False)['native_closure_verified'])

    def test_observed_native_diagnostic_spellings_are_exact_and_not_excerpts(self):
        # Seven failed native probes, six classifiers (two saved projections).
        examples = [
            ('constructor', 'unknown', 'error: RunEnvironmentInfo: unknown argument unrecognized'),
            ('constructor', 'hidden-flag', 'error: RunEnvironmentInfo: unknown argument error_on_non_executable'),
            ('constructor', 'third-positional', 'error: RunEnvironmentInfo: expected at most two positional arguments'),
            # Bazel's Starlark.checkMutable text, printed natively since N20.
            ('ownership', 'saved-dict', 'error: trying to mutate a frozen dict value'),
            ('ownership', 'saved-list', 'error: trying to mutate a frozen list value'),
            ('transport', 'missing-key', 'error: Target @@//app:producer does not provide builtin:RunEnvironmentInfo'),
            ('phase', 'duplicate', 'bazel-rs: analysis of //app:check: implementation returned provider builtin:RunEnvironmentInfo more than once'),
        ]
        for family, probe, message in examples:
            v = suite.fixture(family, probe)
            with self.subTest(family=family, probe=probe):
                suite.require_rejection(record(message), v['diagnostic'], 1)
                for text in ('    '+message, 'source: '+message, message+' unrelated suffix',
                        message+'\nerror: Variable `RunEnvironmentInfo` not found',
                        message+'\nERROR: network unavailable', 'error: unrelated failure'):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record(text), v['diagnostic'], 1)
                for code in (0, 7, 48, True, -9, 101):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record(message, code), v['diagnostic'], 1)
                entered = v['stage'] == 'callback'
                text = (suite.READY+'\n'+suite.ENTERED+'\n' if entered else '')+message
                suite.require_markers(record(text), v, v['failure_command'], True, 'native', True)
                wrong = (suite.READY+'\n' if entered else suite.ENTERED+'\n')+message
                with self.assertRaises(ValueError):
                    suite.require_markers(record(wrong), v, v['failure_command'], True, 'native', True)

    def test_native_classifier_near_misses_and_other_provider_rejected(self):
        examples = [
            ('unknown', 'error: RunEnvironmentInfo: unknown argument other'),
            ('hidden-flag', 'error: RunEnvironmentInfo: unknown argument unrecognized'),
            ('third-positional', 'error: OtherInfo: expected at most two positional arguments'),
            ('third-positional', 'error: RunEnvironmentInfo: expected at most three positional arguments'),
            ('frozen', 'error: ImmutableSomething'),
            ('frozen', 'error: Immutable'),
            ('missing-provider', 'error: Target @@//app:producer does not provide builtin:OtherInfo'),
            ('missing-provider', 'error: Target @@//app:other does not provide builtin:RunEnvironmentInfo'),
            ('duplicate-provider', 'bazel-rs: analysis of //app:check: implementation returned provider builtin:OtherInfo more than once'),
        ]
        for diagnostic, message in examples:
            with self.subTest(diagnostic=diagnostic, message=message), self.assertRaises(ValueError):
                suite.require_rejection(record(message), diagnostic, 1)

    def test_module_availability_rejects_repository_lookup_error_not_just_global(self):
        v = suite.fixture('module', 'module-global')
        self.assertEqual(v['target'], '@missing//:all')
        self.assertEqual(v['stage'], 'loading')
        self.assertEqual(v['files']['MODULE.bazel'], b'module(name="runenv_contract")\nx=RunEnvironmentInfo()\n')
        missing = 'error: Variable `RunEnvironmentInfo` not found'
        suite.require_rejection(record(missing), 'missing-global', 1)
        for text in ('error: repository @missing is not declared', 'error: unknown repository @missing',
                "ERROR: no repository visible as '@missing'", "ERROR: no such package '@@missing//': not found"):
            for message in (text, missing+'\n'+text):
                with self.subTest(message=message), self.assertRaises(ValueError):
                    suite.require_rejection(record(message), 'missing-global', 1)

    def test_controlled_error_rejects_unrelated_excerpt_missing_global(self):
        text = 'Returning RunEnvironmentInfo from a non-executable, non-test target has no effect'
        suite.require_rejection(record('ERROR: '+text), 'ordinary-return')
        for message in ('    ERROR: '+text, 'source: '+text, 'ERROR: unrelated error',
                'ERROR: '+text+'\nerror: Variable `RunEnvironmentInfo` not found',
                'ERROR: '+text+'\nnetwork unavailable', 'ERROR: '+text+'\npanicked',
                'Error: Operation `call()` not supported on type `Provider`'):
            with self.subTest(message=message), self.assertRaises(ValueError):
                suite.require_rejection(record(message), 'ordinary-return')
        for code in (0, True, -9, 37, 101, 124, 125, 126, 127, 137):
            with self.assertRaises(ValueError):
                suite.require_rejection(record('Error: '+text, code), 'ordinary-return')

    def test_constructor_is_callable_wrong_argument_is_not_uncallable(self):
        suite.require_rejection(record("Error in RunEnvironmentInfo: RunEnvironmentInfo() got unexpected keyword argument 'error_on_non_executable'"), 'hidden-flag')
        for text in ('Error: Operation `call()` not supported on type `Provider`',
                'Error: RunEnvironmentInfo is not callable', "Error: name 'RunEnvironmentInfo' is not defined"):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(text), 'hidden-flag')
        text = "Error in RunEnvironmentInfo: RunEnvironmentInfo() got unexpected keyword argument 'error_on_non_executable'"
        suite.require_rejection(record(text, 7), 'hidden-flag', 7)
        with self.assertRaises(ValueError):
            suite.require_rejection(record(text, 1), 'hidden-flag', 7)
        text = "ERROR: /workspace/MODULE.bazel:2:3: name 'RunEnvironmentInfo' is not defined\nERROR: Error computing the main repository mapping: syntax error in MODULE.bazel file for <root>"
        suite.require_rejection(record(text, 48), 'missing-global', 48)
        with self.assertRaises(ValueError):
            suite.require_rejection(record(text+'\nERROR: syntax error in unrelated file', 48), 'missing-global', 48)

    def test_phase_markers_reject_wrong_error_phase(self):
        v = suite.fixture('transport', 'required')
        with self.assertRaisesRegex(ValueError, 'before prerequisite'):
            suite.require_markers(record(suite.READY+'\n'+suite.ENTERED), v, 'build', True, 'native', True)
        v = suite.fixture('phase', 'bare')
        with self.assertRaisesRegex(ValueError, 'consumer callback'):
            suite.require_markers(record(suite.READY), v, 'build', True, 'native', True)
        with self.assertRaisesRegex(ValueError, 'before prerequisite'):
            suite.require_markers(record(suite.READY+'\n'+suite.ENTERED), v, 'query', True, 'reference', False)


class ReplayTests(unittest.TestCase):
    def synthetic(self, root, engine='reference'):
        item = suite.common.case('constructor', ('defaults',))
        args = suite.argument_parser().parse_args(['--reference-only'] if engine == 'reference' else ['--native-sha256', 'a'*64])
        v = suite.fixture('constructor', 'defaults')
        workspace = root / 'workspace'
        workspace.mkdir()
        paths = suite.materialize(workspace, v)
        reference = suite.DirectReference(args.reference, workspace, root, None, 'run-environment-info')
        cache = root / 'cases/constructor/defaults/native-cache'
        before = suite.action_store(cache)
        output = (reference.output_base / 'execroot/_main/bazel-out/synthetic/bin/app/check.out' if engine == 'reference' else
            cache / 'actions' / ('a'*64) / 'outputs/check.out')
        output.parent.mkdir(parents=True)
        output.write_bytes(v['expected']['check.out'])
        output.chmod(0o555)
        entries = [suite.evidence(output)]
        sources = suite.snapshot(suite.SOURCES)
        executables = {'reference': {'sha256': suite.REFERENCE_SHA256}}
        if engine == 'native':
            executables['native'] = {'sha256': 'a'*64}
        official = dict(path=str(args.reference_source), commit=suite.SOURCE_PIN, files=suite.OFFICIAL_FILES)
        binding = suite.digest(json.dumps(dict(executables=executables, source=official), sort_keys=True).encode())
        runs = []

        def append(name, argv, stdout='', stderr='', timeout=args.timeout):
            r = record(stderr, 0, id=len(runs), name=name, argv=argv, cwd=str(workspace), timeout_seconds=timeout,
                executable_sha256=suite.REFERENCE_SHA256 if argv[0] == str(args.reference) else 'a'*64,
                preflight_binding_sha256=binding, synthetic_environment=suite.HOST.copy())
            r['stdout'] = stdout
            for field in ('stdout', 'stderr'):
                raw = r[field].encode()
                r.update({field+'_base64': base64.b64encode(raw).decode(), field+'_sha256': suite.digest(raw),
                    field+'_full_sha256': suite.digest(raw), field+'_full_size_bytes': len(raw)})
            runs.append(r)
        append('reference.version', reference.startup+['version', '--gnu_format'], 'bazel 9.3.0\n')
        for i, (command, target, extra) in enumerate(suite.steps_for(v, engine)):
            stdout = 'bazel-out/synthetic/bin/app/check.out\n' if command == 'cquery' else ''
            stderr = suite.READY+'\n'+suite.ENTERED+'\n' if i == 0 or engine == 'native' else ''
            if engine == 'native':
                stdout = target+'\t'+str(output)+'\n'
                stderr += '1 actions executed, 0 cache hits' if i == 0 else '0 actions executed, 1 cache hits'
            append('constructor.defaults.'+engine+'.'+str(i), suite.command_argv(args, reference, engine, cache, root / 'registry', command, target, extra), stdout, stderr)
        ids = list(range(1, len(runs)))
        append('run-environment-info.shutdown', reference.startup+['shutdown']+list(suite.COMMON_FLAGS), timeout=min(args.timeout, 15))
        phase = dict(probe='defaults', engine=engine, stage='success', cache=str(cache), runs=ids,
            outputs=[entries, copy.deepcopy(entries)], status='PASS', inputs_before=suite.snapshot(paths), inputs_after=suite.snapshot(paths),
            action_store_before=before if engine == 'native' else None,
            action_store_after=suite.action_store(cache) if engine == 'native' else None,
            rejection_artifacts_before=[], rejection_artifacts_after=[])
        registry = {str(root / 'registry' / Path(p).relative_to(suite.REGISTRY)): dict(e,
            path=str(root / 'registry' / Path(p).relative_to(suite.REGISTRY)))
            for p, e in sources.items() if Path(p).is_relative_to(suite.REGISTRY)}
        report = dict(schema_version=1, status='REFERENCE_ONLY_PASS' if args.reference_only else 'PASS',
            started_at='2026-10-13T00:00:00+00:00', finished_at='2026-10-13T00:01:00+00:00',
            configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
            planned_checks=[item], plan=suite.plan_receipt(args.reference_only), limitations=suite.LIMITATIONS,
            historical_parity_gaps=copy.deepcopy(suite.KNOWN_PARITY_GAPS),
            historical_baseline_verification=dict(commit=suite.BASELINE_COMMIT,
                historical_sources=suite.BASELINE_SOURCES.copy(), verified_from_git=True),
            errors=[], cleanup=[], scratch_cleaned=True, reference_shutdown_succeeded=True,
            sources_before=sources, sources_after=copy.deepcopy(sources), executables=executables,
            executables_after=copy.deepcopy(executables), reference_release='bazel 9.3.0',
            official_source_before=official, official_source_after=copy.deepcopy(official),
            completed_phases=1, completed_checks=1, scratch_directory=str(root), cleanup_receipt=dict(path=str(root), removed=True),
            server_receipt=dict(pidfile=str(reference.output_base / 'server/server.pid.txt'), pid=4321,
                proc_exists_after=False, remaining_pidfiles=[]), registry_before=registry, registry_after=copy.deepcopy(registry),
            registry_patches=suite.require_registry_patches(suite.REGISTRY), runs=runs, checks=[dict(name='constructor', kind='differential', status='PASS', phases=[phase])])
        report['gap_closure'] = suite.closure_receipt(report, args.reference_only)
        return phase, report, args, workspace, reference, item

    def test_phase_mutations(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            values = self.synthetic(Path(tmp), 'native')
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: p.update(engine='invented'), lambda p, r: p.update(stage='loading'),
                    lambda p, r: p.update(status='SKIP'), lambda p, r: p.pop('status'),
                    lambda p, r: p['inputs_before'].clear(), lambda p, r: p['runs'].pop(),
                    lambda p, r: p['runs'].append(p['runs'][0]),
                    lambda p, r: p['outputs'][0][0].update(sha256='0'*64),
                    lambda p, r: p['outputs'][0][0].update(mode=0o755),
                    lambda p, r: p['outputs'][0][0].update(path='/invented/check.out'),
                    lambda p, r: p['action_store_before']['actions'].update(exists=True),
                    lambda p, r: p['rejection_artifacts_after'].append('/invented/good.marker'),
                    lambda p, r: r['runs'][1].update(argv=['fake', 'build']),
                    lambda p, r: r['runs'][1].update(stderr=suite.READY+'\n'+suite.ENTERED+'\n0 actions executed, 1 cache hits')):
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises((ValueError, KeyError, IndexError)):
                    suite.validate_phase(phase, report, *values[2:])

    def test_complete_report_mutations(self):
        item = suite.common.case('constructor', ('defaults',))
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp, mock.patch.object(suite, 'CASEPLAN', [item]), mock.patch.object(
                suite, 'PLAN', [('constructor', 'differential')]):
            _, report, *_ = self.synthetic(Path(tmp))
            self.assertTrue(suite.report_passes(report))
            for mutate in (
                    lambda r: r.update(status='PASS'), lambda r: r.pop('status'),
                    lambda r: r.pop('historical_parity_gaps'), lambda r: r.update(historical_parity_gaps=[]),
                    lambda r: r['historical_parity_gaps'][0].update(status='PASS'),
                    lambda r: r['historical_parity_gaps'][0].update(parity_claim=True),
                    lambda r: r['historical_parity_gaps'][0].update(parity_claim=0),
                    lambda r: r['historical_parity_gaps'][0].update(original_matrix_status='PASS'),
                    lambda r: r['historical_parity_gaps'][0]['observations']['native'].update(returncode=1),
                    lambda r: r['historical_parity_gaps'][0]['retained_receipt'].update(sha256='0'*64),
                    lambda r: r['historical_parity_gaps'][0].pop('evidence_excerpt'),
                    lambda r: r['historical_parity_gaps'][0]['evidence_excerpt'].update(sha256='0'*64),
                    lambda r: r['historical_parity_gaps'][0]['paired_replacement'].update(target='//app:check'),
                    lambda r: r['gap_closure'].update(native_closure_verified=True),
                    lambda r: r['gap_closure'].update(status='RESOLVED_BY_ROOT_RECORDING_VALIDATION'),
                    lambda r: r['historical_baseline_verification'].update(verified_from_git=False),
                    lambda r: r['plan'].update(sha256='0'*64), lambda r: r['plan'].update(commands=1),
                    lambda r: r.pop('registry_patches'), lambda r: r.update(registry_patches={}),
                    lambda r: r['registry_patches'].popitem(),
                    lambda r: r['checks'][0]['phases'].clear(),
                    lambda r: r['checks'][0]['phases'].append(copy.deepcopy(r['checks'][0]['phases'][0])),
                    lambda r: r['checks'][0]['phases'][0].update(engine='native'),
                    lambda r: r['checks'][0]['phases'][0].update(stage='loading'),
                    lambda r: r['checks'][0]['phases'][0].update(status='FAIL'),
                    lambda r: r['checks'][0].update(status='SKIP'), lambda r: r.update(scratch_cleaned=False),
                    lambda r: r['cleanup_receipt'].update(removed=False),
                    lambda r: r['server_receipt'].update(proc_exists_after=True),
                    lambda r: r['server_receipt'].update(pid=1),
                    lambda r: r['server_receipt']['remaining_pidfiles'].append('/leftover'),
                    lambda r: r.update(reference_shutdown_succeeded=False), lambda r: r['sources_after'].clear(),
                    lambda r: r['registry_after'].clear(), lambda r: r['official_source_before'].update(commit='0'*40),
                    lambda r: r['executables']['reference'].update(sha256='0'*64),
                    lambda r: r['executables_after']['reference'].update(sha256='0'*64),
                    lambda r: r['runs'][1].update(preflight_binding_sha256='0'*64),
                    lambda r: r['runs'][1].update(stderr='changed'),
                    lambda r: r['runs'][1].update(environment={'forbidden': 'not evidence'}),
                    lambda r: r['runs'][1].update(synthetic_environment={}),
                    lambda r: r['runs'].pop(), lambda r: r.update(completed_phases=0)):
                changed = copy.deepcopy(report)
                mutate(changed)
                self.assertFalse(suite.report_passes(changed))

    def test_rehashed_fixture_cannot_replace_plan(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            phase, report, *rest = self.synthetic(Path(tmp))
            path = next(p for p in phase['inputs_before'] if p.endswith('check.bzl'))
            raw = b'fabricated but rehashed\n'
            for field in ('inputs_before', 'inputs_after'):
                phase[field][path].update(bytes_base64=base64.b64encode(raw).decode(), sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable fixture'):
                suite.validate_phase(phase, report, *rest)

    def test_wrong_phase_order_and_boundary_reference_rejected(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        for item in (suite.CASEPLAN[0], suite.CASEPLAN[-1]):
            check = dict(name=item['name'], kind=item['kind'], status='PASS',
                phases=[dict(probe=p, engine='reference') for p in reversed(item['probes'])])
            with mock.patch.object(suite, 'validate_phase') as replay:
                with self.assertRaisesRegex(ValueError, 'missing, reordered'):
                    suite.validate_check(check, {}, args, Path('/unused'), None)
                replay.assert_not_called()

    def test_precommand_hash_failure_prevents_execution(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        report = dict(runs=[], executables={'reference': {'sha256': suite.REFERENCE_SHA256}}, official_source_before={})
        runner = suite.RecordingRunner(report, args)
        with mock.patch.object(suite.common, 'executable_evidence', return_value={}), mock.patch.object(
                suite, 'reference_source_evidence', return_value={}), mock.patch.object(suite.utility.RecordingRunner, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'binding changed'):
                runner.run('not-executed', ['fake'], Path('/unused'))
            run.assert_not_called()

    def test_cleanup_failure_retains_owned_pid_without_process_dump(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            root = Path(tmp)
            reference = suite.DirectReference(Path('/not-executed'), root, root, mock.Mock(timeout=10), 'run-environment-info')
            pidfile = reference.output_base / 'server/server.pid.txt'
            pidfile.parent.mkdir(parents=True)
            pidfile.write_text('4321')
            reference.runner.run.return_value = record('shutdown failure', 1, name='shutdown', id=0)
            report = dict(reference_shutdown_succeeded=False)
            with self.assertRaises(ValueError):
                suite.shutdown(reference, report)
            self.assertFalse(report['reference_shutdown_succeeded'])
            self.assertTrue(pidfile.exists())

    def test_plan_binds_expected_output_not_just_command_names(self):
        original = suite.fixture
        before = suite.plan_receipt(True)

        def changed(*args):
            v = original(*args)
            v['expected'] = {'forged.out': b'forged'}
            return v
        with mock.patch.object(suite, 'fixture', side_effect=changed):
            self.assertNotEqual(before['sha256'], suite.plan_receipt(True)['sha256'])

    def test_known_gap_retains_actual_failed_phase_and_baseline_source_evidence(self):
        gap = suite.KNOWN_PARITY_GAPS[0]
        self.assertEqual(gap['status'], 'KNOWN_DIFFERENTIAL_MISMATCH')
        self.assertEqual(gap['original_matrix_status'], 'FAIL')
        self.assertIs(gap['parity_claim'], False)
        self.assertEqual(gap['original_phase']['target'], '//app:check')
        self.assertEqual(gap['paired_replacement']['target'], '@missing//:all')
        self.assertEqual(gap['observations']['native']['returncode'], 0)
        self.assertEqual(gap['observations']['native']['phase_status'], 'FAIL')
        self.assertEqual(gap['observations']['reference']['returncode'], 48)
        self.assertEqual(gap['baseline']['commit'], suite.BASELINE_COMMIT)
        sources = suite.snapshot(suite.SOURCES)
        with mock.patch.object(suite.subprocess, 'run') as run:
            suite.validate_known_gap_sources(sources)
            run.assert_not_called()  # Offline replay requires neither git nor live engines.

    def test_tracked_excerpt_never_requires_ignored_original_receipt(self):
        gap = suite.KNOWN_PARITY_GAPS[0]
        original = suite.ROOT / gap['retained_receipt']['path']
        self.assertNotIn(original, suite.SOURCES)
        self.assertFalse(any(path.is_relative_to(suite.ROOT / 'artifacts') for path in suite.SOURCES))
        self.assertEqual(suite.GAP_RECEIPT, suite.ROOT / gap['evidence_excerpt']['path'])
        raw = suite.GAP_RECEIPT.read_bytes()
        self.assertLess(len(raw), 100_000)
        self.assertEqual(suite.digest(raw), gap['evidence_excerpt']['sha256'])
        excerpt = json.loads(raw)
        self.assertEqual(excerpt['kind'], 'retained_differential_excerpt')
        self.assertEqual(excerpt['source_receipt'], gap['retained_receipt'])
        self.assertEqual(excerpt['source_receipt']['sha256'],
            '1c8fff1a7b38b9f9fa62d7d1863fe3c69d23f6020480b4a82412e692cf9bda08')
        self.assertEqual(set(excerpt['runs']), {'142', '143'})
        self.assertEqual(len(excerpt['checks'][0]['phases']), 2)
        # Verify the module-wide portability guards really are active.
        with self.assertRaisesRegex(AssertionError, 'ignored original'):
            original.read_bytes()
        with self.assertRaisesRegex(AssertionError, 'ignored original'):
            suite.snapshot([original])

    def test_excerpt_origin_scope_stream_and_phase_guards_beyond_outer_hash(self):
        sources = suite.snapshot(suite.SOURCES)
        original = json.loads(base64.b64decode(sources[str(suite.GAP_RECEIPT)]['bytes_base64']))
        for mutate in (
                lambda e: e.update(kind='full_matrix'),
                lambda e: e['source_receipt'].update(sha256='0'*64),
                lambda e: e['source_receipt'].update(path='invented.json'),
                lambda e: e['runs'].update({'0': e['runs']['142']}),
                lambda e: e['checks'][0]['phases'].pop(),
                lambda e: e['checks'][0]['phases'][1].update(status='PASS'),
                lambda e: e['runs']['143'].update(returncode=1),
                lambda e: e['runs']['142'].update(stderr_base64=base64.b64encode(b'changed').decode()),
                lambda e: e['runs']['143'].update(executable_sha256='0'*64)):
            excerpt = copy.deepcopy(original)
            mutate(excerpt)
            raw = json.dumps(excerpt, sort_keys=True).encode()
            changed = copy.deepcopy(sources)
            changed[str(suite.GAP_RECEIPT)].update(bytes_base64=base64.b64encode(raw).decode(),
                sha256=suite.digest(raw), size_bytes=len(raw))
            # Isolate the inner guard from the independent exact excerpt-file pin.
            gaps = copy.deepcopy(suite.KNOWN_PARITY_GAPS)
            gaps[0]['evidence_excerpt']['sha256'] = suite.digest(raw)
            with mock.patch.object(suite, 'KNOWN_PARITY_GAPS', gaps), self.assertRaises((ValueError, KeyError)):
                suite.validate_known_gap_sources(changed)

    def test_rehashed_gap_receipt_and_baseline_test_mutations_rejected(self):
        sources = suite.snapshot(suite.SOURCES)
        for path, replacement in (
                (str(suite.GAP_RECEIPT), b'{"status":"PASS"}\n'),
                (str(suite.ROOT / 'src/repository.rs'), b'changed lazy repository source\n'),
                (str(suite.ROOT / 'src/package.rs'),
                    base64.b64decode(sources[str(suite.ROOT / 'src/package.rs')]['bytes_base64']).replace(
                        b'"invalid MODULE !!!"', b'"valid module instead"'))):
            changed = copy.deepcopy(sources)
            changed[path].update(bytes_base64=base64.b64encode(replacement).decode(),
                sha256=suite.digest(replacement), size_bytes=len(replacement))
            with self.subTest(path=path), self.assertRaises(ValueError):
                suite.validate_known_gap_sources(changed)


if __name__ == '__main__':
    unittest.main()
