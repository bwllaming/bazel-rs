"""Harness mutations only. Synthetic records never constitute executed parity."""

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
import validate_package_specification as suite


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **changes)


class PlanTests(unittest.TestCase):
    def test_exact_plan_and_lexical_fixtures(self):
        self.assertEqual(len(suite.CASEPLAN), 9)
        self.assertEqual(len(set(suite.PLAN)), 9)
        self.assertEqual(sum(len(c['probes']) for c in suite.CASEPLAN), 80)
        self.assertEqual(suite.plan_receipt()['phases'], 154)
        self.assertEqual(suite.plan_receipt(True)['phases'], 74)
        for item in suite.CASEPLAN:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe in item['probes']:
                value = suite.fixture(item['name'], probe)
                for path, raw in value['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(family=item['name'], probe=probe, path=path):
                            ast.parse(raw.decode(), filename=path)
                if value['diagnostic']:
                    self.assertIn(value['diagnostic'], suite.DIAGNOSTICS)
                    self.assertEqual(value['failure_command'], 'query' if value['stage'] == 'loading' else 'build')
                else:
                    self.assertEqual(sum(c == 'build' for c, _, _ in suite.steps_for(value, 'native')), 2)

    def test_unknown_plan_and_boundary_reference_rejected(self):
        for name, probe in [('invented', 'x'), ('producer', 'invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)
        boundary = suite.CASEPLAN[-1]
        self.assertEqual(suite.phases_for(boundary, True), [])
        self.assertTrue(all(e == 'native' for _, e in suite.phases_for(boundary)))
        for c in suite.CASEPLAN[:-1]:
            for probe in c['probes']:
                self.assertNotIn('return-info', probe)

    def test_real_producer_and_memberships_not_equality_deduplicated(self):
        value = suite.fixture('producer', 'equal-distinct')
        self.assertIn(b'package_group(', value['files']['app/BUILD.bazel'])
        self.assertIn(b'{a:"same"}[b]', value['files']['defs/check.bzl'])
        self.assertIn(b'ctx.attr.gs[1]', value['files']['defs/check.bzl'])
        self.assertEqual(value['expected'], {'equal.out': b'[True, "same", True, False, True]'})
        self.assertIn(b'providers=[PackageSpecificationInfo]', value['files']['defs/check.bzl'])

    def test_mapping_strings_and_label_includes(self):
        value = suite.fixture('producer', 'external-canonical')
        self.assertIn(b'p.contains("@alias//foo:x")', value['files']['defs/check.bzl'])
        self.assertIn(b'p.contains(Label("@alias//foo:x"))', value['files']['defs/check.bzl'])
        self.assertEqual(value['expected'], {'check.out': b'[False, False, True, True, True]'})
        self.assertIn(b'Label("//app:leaf")', suite.fixture('producer', 'label-includes')['files']['defs/check.bzl'])

    def test_source_output_and_empty_rules_are_distinct(self):
        for probe in ('source', 'output'):
            value = suite.fixture('constraints', probe)
            self.assertIsNone(value['diagnostic'])
            self.assertIn(b'allow_files=True', value['files']['defs/check.bzl'])
        self.assertEqual(suite.fixture('constraints', 'empty-rule')['diagnostic'], 'required')
        self.assertEqual(suite.fixture('constraints', 'list-missing')['stage'], 'preflight')

    def test_placement_not_provider_satisfaction_and_empty_erases_gate(self):
        for probe in ('and-missing', 'or-missing', 'substring'):
            self.assertIsNone(suite.fixture('placement', probe)['diagnostic'])
        for probe in ('plain', 'default', 'empty-erases', 'srcs'):
            self.assertEqual(suite.fixture('placement', probe)['diagnostic'], 'misplaced')
        self.assertIn(b'[[],[provider()]]', suite.fixture('constraints', 'unexported-after-empty')['files']['defs/check.bzl'])

    def test_queries_do_not_analyze_visibility_or_expand_query_language(self):
        for probe in ('root-missing', 'same-package-missing', 'source-missing', 'implicit-source-missing', 'wrong-kind'):
            value = suite.fixture('visibility', probe)
            self.assertEqual(value['queries'], [(value['target'], [value['target']])])
            self.assertEqual(value['stage'], 'preflight')
        value = suite.fixture('graph', 'standalone-query')
        self.assertEqual(value['expected'], {})
        self.assertEqual(value['actions'], 0)
        self.assertEqual(value['queries'][1:], [('//app:all', ['//app:check']), ('//app/...', ['//app:check'])])
        for c in suite.CASEPLAN:
            for p in c['probes']:
                self.assertNotIn(':*', repr(suite.fixture(c['name'], p)['queries']))

    def test_pin_required_and_reference_only_cannot_execute_native(self):
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


class DiagnosticsTests(unittest.TestCase):
    def test_actual_native_diagnostic_spellings_without_generic_fallback(self):
        messages = {
            'keyword': 'Found `target` extra named parameter(s) for call to function',
            'missing': 'Wrong number of positional arguments, expected 1, got 0',
            'immutable': 'Operation `.contains=` not supported on type `PackageSpecificationInfo`',
            'grammar': 'invalid package path "foo/": empty, \'.\' and \'..\' path components are not allowed',
            'collision': 'output //app:demo_group collides with a rule name',
            'declaration-phase': 'rule declaration outside package evaluation',
        }
        for diagnostic, message in messages.items():
            suite.require_rejection(record('error: '+message), diagnostic)
            for text in ('    error: '+message, 'error: unrelated failure',
                    'error: '+message+'\nerror: Variable `PackageSpecificationInfo` not found'):
                with self.subTest(diagnostic=diagnostic, text=text), self.assertRaises(ValueError):
                    suite.require_rejection(record(text), diagnostic)
        for diagnostic, text in (
                ('keyword', 'Found `other` extra named parameter(s) for call to function'),
                ('missing', 'Wrong number of positional arguments, expected 2, got 0'),
                ('immutable', 'Operation `.contains=` not supported on type `OtherInfo`'),
                ('declaration-phase', 'rule declaration outside unrelated evaluation')):
            with self.subTest(text=text), self.assertRaises(ValueError):
                suite.require_rejection(record('error: '+text), diagnostic)

    def test_native_uncallable_provider_operation_is_narrow(self):
        for text in ('Operation call() not supported on type Provider',
                'Operation `call()` not supported on type `Provider`'):
            suite.require_rejection(record('error: '+text), 'constructor')
        for text in ('Operation `call()` not supported on type `OtherProvider`',
                'Operation `call()` not supported on type `ProviderInstance`',
                'Operation `call()` not supported on type `rule`',
                'Operation `getattr()` not supported on type `Provider`',
                'Object has no attribute PackageSpecificationInfo',
                'Variable `PackageSpecificationInfo` not found',
                'Operation `call()` not supported on type `Provider`\n'
                'error: Variable `PackageSpecificationInfo` not found'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                suite.require_rejection(record('error: '+text), 'constructor')

    def test_narrow_error_line_and_missing_globals(self):
        examples = {'constructor': "'Provider' object is not callable",
            'keyword': "contains() got named argument for positional-only parameter 'target'",
            'type': "contains() parameter 'target' got int, want Label or string",
            'missing': 'contains() missing 1 required positional argument: target',
            'extra': 'contains() accepts no more than 1 positional argument but got 2',
            'relative': "invalid label ':x': absolute label must begin with '@' or '//'",
            'immutable': 'cannot set .contains field of PackageSpecificationInfo value',
            'negate': 'Cannot negate public package specification',
            'grammar': 'invalid package name //foo/',
            'required': 'target does not have mandatory providers: Custom',
            'constraint-shape': 'providers got string, want sequence',
            'export': 'Providers should be top-level values in extension files that define them.',
            'misplaced': 'package group //app:g is misplaced here',
            'group-kind': 'Label //app:wrong does not refer to a package group.',
            'cycle': 'in package group: cycle in dependency graph',
            'declaration-phase': 'package_group() can only be used while evaluating a BUILD file',
            'literal-boundary': 'package specification contains unsupported ASCII character',
            'depth': 'package group depth budget exceeded',
            'return-info': 'returning PackageSpecificationInfo is unsupported',
            'missing-target': 'no such target //app:absent',
            'namespace': 'target violates macro naming rules',
            'collision': 'target demo_group conflicts with an existing target',
            'visibility': 'target is not visible from target'}
        self.assertEqual(set(examples), set(suite.DIAGNOSTICS))
        for diagnostic, message in examples.items():
            with self.subTest(diagnostic=diagnostic):
                suite.require_rejection(record('Error: '+message), diagnostic)
                for text in ('    Error: '+message, 'source example: '+message,
                        "Error: name 'PackageSpecificationInfo' is not defined\nError: "+message,
                        'error: Variable `package_group` not found\nError: '+message,
                        'Error: '+message+'\nnetwork unavailable', 'Error: '+message+'\npanicked'):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record(text), diagnostic)
                for code in (0, True, -9, 37, 101, 124, 125, 126, 127, 137):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record('Error: '+message, code), diagnostic)

    def test_stage_markers_prevent_false_positive_and_early_callback(self):
        value = suite.fixture('contains', 'keyword')
        r = record(suite.READY+'\nError: contains() got named argument for positional-only target')
        with self.assertRaisesRegex(ValueError, 'consumer callback'):
            suite.require_markers(r, value, 'build', True, 'native', True)
        value = suite.fixture('placement', 'plain')
        r['stderr'] = suite.READY+'\n'+suite.ENTERED+'\n'
        with self.assertRaisesRegex(ValueError, 'before prerequisite'):
            suite.require_markers(r, value, 'build', True, 'native', True)
        r['stderr'] = suite.WRONG
        with self.assertRaisesRegex(ValueError, 'wrong-kind'):
            suite.require_markers(r, value, 'build', True, 'native', True)


class ReplayTests(unittest.TestCase):
    def test_zero_artifact_group_allows_only_empty_success_stdout(self):
        value = suite.fixture('graph', 'standalone-query')
        r = record('', 0)
        self.assertEqual(suite.output_paths(r, value, 'native', Path('/unused'), Path('/unused'), None), [])
        for output in ('\n', '//app:g\t/invented/output\n'):
            r['stdout'] = output
            with self.assertRaisesRegex(ValueError, 'zero-artifact'):
                suite.output_paths(r, value, 'native', Path('/unused'), Path('/unused'), None)

    def test_provider_only_edges_do_not_claim_prerequisite_action_execution(self):
        for family, probe in [('constraints', 'output'), ('visibility', 'rule-allow'),
                ('visibility', 'output-allow'), ('visibility', 'macro-definition-allow'),
                ('visibility', 'macro-producer-allow')]:
            value = suite.fixture(family, probe)
            self.assertEqual(value['actions'], 1)
            self.assertIn(b'ctx.actions.write', value['files']['defs/check.bzl'])
            self.assertNotIn(b'ctx.actions.run', value['files']['defs/check.bzl'])

    def synthetic(self, root, engine='reference'):
        item = suite.common.case('producer', ('core',))
        args = suite.argument_parser().parse_args(['--reference-only'] if engine == 'reference' else ['--native-sha256', 'a'*64])
        value = suite.fixture('producer', 'core')
        workspace = root / 'workspace'
        workspace.mkdir()
        paths = suite.materialize(workspace, value)
        reference = suite.DirectReference(args.reference, workspace, root, None, 'package-specification')
        cache = root / 'cases/producer/core/native-cache'
        store_before = suite.action_store(cache)
        output = (reference.output_base / 'execroot/_main/bazel-out/synthetic/bin/app/check.out' if engine == 'reference' else
            cache / 'actions' / ('a'*64) / 'outputs/check.out')
        output.parent.mkdir(parents=True)
        output.write_bytes(value['expected']['check.out'])
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
                preflight_binding_sha256=binding)
            r['stdout'] = stdout
            for field in ('stdout', 'stderr'):
                raw = r[field].encode()
                r.update({field+'_base64': base64.b64encode(raw).decode(), field+'_sha256': suite.digest(raw),
                    field+'_full_sha256': suite.digest(raw), field+'_full_size_bytes': len(raw)})
            runs.append(r)
        append('reference.version', reference.startup+['version', '--gnu_format'], 'bazel 9.3.0\n')
        for i, (command, target, extra) in enumerate(suite.steps_for(value, engine)):
            stdout = 'bazel-out/synthetic/bin/app/check.out\n' if command == 'cquery' else ''
            stderr = suite.READY+'\n'+suite.ENTERED+'\n' if i == 0 or engine == 'native' else ''
            if engine == 'native':
                stdout = target+'\t'+str(output)+'\n'
                stderr += '1 actions executed, 0 cache hits' if i == 0 else '0 actions executed, 1 cache hits'
            append('producer.core.'+engine+'.'+str(i), suite.command_argv(args, reference, engine, cache, root / 'registry', command, target, extra), stdout, stderr)
        ids = list(range(1, len(runs)))
        append('package-specification.shutdown', reference.startup+['shutdown']+list(suite.COMMON_FLAGS), timeout=min(args.timeout, 15))
        phase = dict(probe='core', engine=engine, stage='success', cache=str(cache), runs=ids,
            outputs=[entries, copy.deepcopy(entries)], status='PASS', inputs_before=suite.snapshot(paths), inputs_after=suite.snapshot(paths),
            action_store_before=store_before if engine == 'native' else None,
            action_store_after=suite.action_store(cache) if engine == 'native' else None)
        registry = {str(root / 'registry' / Path(p).relative_to(suite.REGISTRY)): dict(e,
            path=str(root / 'registry' / Path(p).relative_to(suite.REGISTRY)))
            for p, e in sources.items() if Path(p).is_relative_to(suite.REGISTRY)}
        report = dict(schema_version=1, status='REFERENCE_ONLY_PASS' if args.reference_only else 'PASS',
            configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
            planned_checks=[item], plan=suite.plan_receipt(args.reference_only), limitations=suite.LIMITATIONS,
            errors=[], cleanup=[], scratch_cleaned=True, reference_shutdown_succeeded=True,
            sources_before=sources, sources_after=copy.deepcopy(sources), executables=executables,
            executables_after=copy.deepcopy(executables), reference_release='bazel 9.3.0',
            official_source_before=official, official_source_after=copy.deepcopy(official),
            completed_phases=1, completed_checks=1, scratch_directory=str(root), cleanup_receipt=dict(path=str(root), removed=True),
            server_receipt=dict(pidfile=str(reference.output_base / 'server/server.pid.txt'), pid=4321,
                proc_exists_after=False, remaining_pidfiles=[]), registry_before=registry, registry_after=copy.deepcopy(registry),
            runs=runs, checks=[dict(name='producer', kind='differential', status='PASS', phases=[phase])])
        return phase, report, args, workspace, reference, item

    def test_phase_mutations_engine_stage_source_outputs_actions_and_commands(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            values = self.synthetic(Path(tmp), 'native')
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: p.update(engine='invented'), lambda p, r: p.update(stage='loading'),
                    lambda p, r: p.update(status='SKIP'), lambda p, r: p['inputs_before'].clear(),
                    lambda p, r: p['runs'].pop(), lambda p, r: p['runs'].append(p['runs'][0]),
                    lambda p, r: p['outputs'][0][0].update(sha256='0'*64),
                    lambda p, r: p['outputs'][0][0].update(mode=0o755),
                    lambda p, r: p['outputs'][0][0].update(path='/invented/check.out'),
                    lambda p, r: p['action_store_before']['actions'].update(exists=True),
                    lambda p, r: p['action_store_before']['actions'].update(path='/invented/actions'),
                    lambda p, r: r['runs'][1].update(argv=['fake', 'build']),
                    lambda p, r: r['runs'][1].update(stderr=suite.READY+'\n'+suite.ENTERED+'\n0 actions executed, 1 cache hits')):
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises((ValueError, KeyError, IndexError)):
                    suite.validate_phase(phase, report, *values[2:])

    def test_complete_report_mutations_cannot_forge_parity(self):
        item = suite.common.case('producer', ('core',))
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp, mock.patch.object(suite, 'CASEPLAN', [item]), mock.patch.object(
                suite, 'PLAN', [('producer', 'differential')]):
            _, report, *_ = self.synthetic(Path(tmp))
            self.assertTrue(suite.report_passes(report))
            for mutate in (
                    lambda r: r.update(status='PASS'), lambda r: r['plan'].update(sha256='0'*64),
                    lambda r: r['plan'].update(phases=0), lambda r: r['plan'].update(commands=1),
                    lambda r: r['checks'][0]['phases'].clear(),
                    lambda r: r['checks'][0]['phases'][0].update(engine='native'),
                    lambda r: r['checks'][0]['phases'][0].update(stage='loading'),
                    lambda r: r['checks'][0]['phases'][0].update(status='FAIL'),
                    lambda r: r['checks'][0].update(status='SKIP'),
                    lambda r: r.update(scratch_cleaned=False), lambda r: r['cleanup_receipt'].update(removed=False),
                    lambda r: r['server_receipt'].update(proc_exists_after=True),
                    lambda r: r['server_receipt'].update(pid=1),
                    lambda r: r['server_receipt'].update(pidfile='/invented/server.pid.txt'),
                    lambda r: r['server_receipt']['remaining_pidfiles'].append('/leftover'),
                    lambda r: r.update(reference_shutdown_succeeded=False),
                    lambda r: r['cleanup'].append({'forced': True}), lambda r: r['sources_after'].clear(),
                    lambda r: r['registry_after'].clear(), lambda r: r['official_source_before'].update(commit='0'*40),
                    lambda r: r['executables']['reference'].update(sha256='0'*64),
                    lambda r: r['executables_after']['reference'].update(sha256='0'*64),
                    lambda r: r['runs'][1].update(preflight_binding_sha256='0'*64),
                    lambda r: r['runs'][1].update(stderr='changed'),
                    lambda r: r['runs'][1].update(environment={'forbidden': 'not evidence'}),
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

    def test_exact_ordered_plan_and_native_boundary_reject_reference(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        for item in (suite.CASEPLAN[0], suite.CASEPLAN[-1]):
            check = dict(name=item['name'], kind=item['kind'], status='PASS',
                phases=[dict(probe=p, engine='reference') for p in reversed(item['probes'])])
            with mock.patch.object(suite, 'validate_phase') as replay:
                with self.assertRaisesRegex(ValueError, 'missing, reordered'):
                    suite.validate_check(check, {}, args, Path('/unused'), None)
                replay.assert_not_called()

    def test_before_each_command_binding_failure_prevents_execution(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        report = dict(runs=[], executables={'reference': {'sha256': suite.REFERENCE_SHA256}}, official_source_before={})
        runner = suite.RecordingRunner(report, args)
        with mock.patch.object(suite.common, 'executable_evidence', return_value={}), mock.patch.object(
                suite, 'reference_source_evidence', return_value={}), mock.patch.object(suite.macros.RecordingRunner, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'binding changed'):
                runner.run('not-executed', ['fake'], Path('/unused'))
            run.assert_not_called()

    def test_cleanup_failure_retains_server_state_without_argv_or_environment(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            root = Path(tmp)
            reference = suite.DirectReference(Path('/not-executed'), root, root, mock.Mock(timeout=10), 'package-specification')
            pidfile = reference.output_base / 'server/server.pid.txt'
            pidfile.parent.mkdir(parents=True)
            pidfile.write_text('4321')
            reference.runner.run.return_value = record('forced shutdown failure', 1, name='shutdown', id=0)
            report = dict(reference_shutdown_succeeded=False)
            with self.assertRaises(ValueError):
                suite.shutdown(reference, report)
            self.assertFalse(report['reference_shutdown_succeeded'])
            self.assertTrue(pidfile.exists())
            self.assertNotIn('environment', report)


if __name__ == '__main__':
    unittest.main()
