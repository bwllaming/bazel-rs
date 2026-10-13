"""Harness-only tests; synthetic records are not CLI/Starlark parity evidence."""

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
import validate_symbolic_macros as suite


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **changes)


class PlanTests(unittest.TestCase):
    def test_exact_unique_plan_and_lexical_fixtures(self):
        self.assertEqual(len(suite.CASEPLAN), 8)
        self.assertEqual(len(set(suite.PLAN)), 8)
        self.assertEqual(sum(len(suite.phases_for(c, True)) for c in suite.CASEPLAN), 50)
        self.assertEqual(sum(len(suite.phases_for(c)) for c in suite.CASEPLAN), 108)
        for item in suite.CASEPLAN:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe in item['probes']:
                value = suite.fixture(item['name'], probe)
                for path, raw in value['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(probe=probe, path=path):
                            ast.parse(raw.decode(), filename=path)
                for engine in ('native', 'reference'):
                    steps = suite.steps_for(value, engine)
                    if not value['diagnostic']:
                        self.assertEqual(sum(cmd == 'build' for cmd, _, _ in steps), 2)

    def test_unknown_plan_never_falls_back(self):
        for name, probe in [('invented', 'x'), ('functional', 'invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)

    def test_native_boundaries_never_claim_reference_rejection(self):
        item = suite.CASEPLAN[-1]
        self.assertEqual(item['kind'], 'native_boundary')
        self.assertEqual(suite.phases_for(item, True), [])
        self.assertTrue(all(engine == 'native' for _, engine in suite.phases_for(item)))
        value = suite.fixture('native-boundary', 'name-none')
        self.assertIn(b'm(name=None)', value['files']['app/BUILD.bazel'])
        for c in suite.CASEPLAN[:-1]:
            for probe in c['probes']:
                self.assertNotIn(b'm(name=None)', b''.join(suite.fixture(c['name'], probe)['files'].values()))

    def test_frozen_candidate_required_and_hash_mismatch(self):
        args = suite.argument_parser().parse_args([])
        for sha in (None, '', 'baseline', '0'*63):
            args.native_sha256 = sha
            with self.assertRaisesRegex(ValueError, 'frozen candidate'):
                suite.common.executable_evidence(args)
        args.native_sha256 = 'a'*64
        with mock.patch.object(suite.common, 'validate_binaries', return_value={'native': {'sha256': 'b'*64}}):
            with self.assertRaisesRegex(ValueError, 'differs from frozen'):
                suite.common.executable_evidence(args)

    def test_reference_elf_pin_and_reference_only_never_checks_native(self):
        args = suite.argument_parser().parse_args(['--reference-only'])
        with mock.patch.object(suite.common, 'validate_executable') as validate, mock.patch.object(
                suite.common, 'file_evidence', return_value={'sha256': '0'*64}), mock.patch.object(
                suite.common, 'validate_binaries') as native:
            with self.assertRaisesRegex(ValueError, 'not pinned'):
                suite.common.executable_evidence(args)
            validate.assert_called_once_with(args.reference, official=True)
            native.assert_not_called()

    def test_private_closure_is_real_custom_rule_and_original_macro(self):
        value = suite.fixture('functional', 'private-closure')
        defs = value['files']['defs/m.bzl']
        self.assertIn(b'def impl(name,visibility):', defs)
        self.assertIn(b'ctx.actions.write', defs)
        self.assertIn(b'_private=factory(', defs)
        self.assertIn(b'm=_private', defs)
        self.assertIn(b'native.package_name()', defs)
        self.assertNotIn(b'getattr(', defs)
        self.assertEqual(value['expected'], {'demo.custom': b'closure:app', 'demo.native': b'native-data'})

    def test_factory_owner_not_implementation_lexical_or_reexport(self):
        value = suite.fixture('functional', 'factory-owner-alias')
        self.assertIn(b'def factory():', value['files']['defs/m.bzl'])
        self.assertIn(b'_private=factory()', value['files']['b/m.bzl'])
        self.assertIn(b'load("//b:m.bzl",original="m")', value['files']['alias/m.bzl'])
        self.assertIn(b'//b:secret', value['files']['defs/m.bzl'])
        external = suite.fixture('visibility', 'external-same-path')
        self.assertIn(b'local_path_override', external['files']['MODULE.bazel'])
        self.assertIn('dep/defs/m.bzl', external['files'])
        self.assertIn('defs/BUILD.bazel', external['files'])

    def test_source_eligibility_custom_rule_never_selected(self):
        value = suite.fixture('sources', 'custom-default-unselected')
        self.assertEqual(value['target'], '//app:demo')
        self.assertIn(b'fail("UNSELECTED_IMPL_EXECUTED")', value['files']['app/m.bzl'])
        self.assertIn(b'attr.label(default=":physical"', value['files']['app/m.bzl'])
        self.assertLess(value['files']['app/BUILD.bazel'].index(b'm(name="demo")'),
            value['files']['app/BUILD.bazel'].index(b'establish(name="unselected")'))
        reserved = suite.fixture('sources', 'reserved-name')
        self.assertEqual(reserved['queries'], [('//app:all', ['//app:consumer'])])
        self.assertEqual(reserved['diagnostic'], 'missing-target')

    def test_namespace_targets_not_artifacts_and_root_order_preserved(self):
        args = suite.argument_parser().parse_args([])
        root = Path('/tmp/opencode/synthetic-not-executed')
        ref = suite.DirectReference(args.reference, root / 'workspace', root, None, 'symbolic-macros')
        for probe, roots in [('outside-output-first', ['//app:outside', '//app:demo']),
                ('outside-output-second', ['//app:demo', '//app:outside'])]:
            value = suite.fixture('namespace', probe)
            self.assertEqual(value['target'], roots)
            for engine in ('native', 'reference'):
                argv = suite.command_argv(args, ref, engine, root / 'cache', root / 'registry', 'build', roots, [])
                self.assertEqual(argv[-2:], roots)
        dynamic = suite.fixture('namespace', 'dynamic-artifact')
        self.assertFalse(dynamic['queries'])
        self.assertEqual(dynamic['expected'], {'outside_dynamic': b'dynamic-data'})


class DiagnosticTests(unittest.TestCase):
    def test_specific_error_lines_not_source_snippets(self):
        messages = {'visibility': 'Visibility error: target is not visible from target',
            'namespace': "Target //app:outside declared in symbolic macro 'demo' violates macro naming rules",
            'collision': "macro 'demo' conflicts with an existing target",
            'missing-target': "no such target '//app:physical': target 'physical' not declared",
            'missing-name': "missing value for mandatory attribute 'name' in 'm' macro",
            'integer-name': "expected value of type 'string' for attribute 'name' of 'm', but got 1",
            'positional': 'unexpected positional arguments',
            'unknown-none': "no such attribute 'unknown' in 'm' macro",
            'return': "macro 'demo' may not return a non-None value (got 7)",
            'unexported': 'Cannot instantiate a macro that has not been exported',
            'implementation': "parameter 'implementation' got value of type 'builtin_function_or_method', want 'Starlark function'",
            'frozen': 'trying to mutate a frozen list value',
            'constructor-phase': 'macro() can only be used during .bzl initialization',
            'invocation-phase': 'a symbolic macro can only be instantiated while evaluating a BUILD file',
            'glob-phase': 'glob() can only be used while evaluating a BUILD file or a legacy macro',
            'package-phase': 'package() can only be used while evaluating a BUILD file',
            'user-attrs': 'macro user attrs are unsupported', 'inheritance': 'inherit_attrs is unsupported',
            'finalizer': 'finalizer=True is unsupported', 'nested': 'nested symbolic macros are unsupported',
            'exports-files': 'exports_files inside a symbolic macro is unsupported',
            'package-group': 'package_group visibility is unsupported'}
        self.assertEqual(set(messages), set(suite.DIAGNOSTICS))
        for diagnostic, message in messages.items():
            with self.subTest(diagnostic=diagnostic):
                suite.require_rejection(record('Error: '+message), diagnostic)
                with self.assertRaises(ValueError):
                    suite.require_rejection(record('source example: '+message), diagnostic)
                for bad in ('panicked', 'network unavailable', 'permission denied', 'internal error', 'failed to download'):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record('Error: '+message+'\n'+bad), diagnostic)
                for code in (0, True, -9, 37, 101, 124, 125, 126, 127, 137):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(record('Error: '+message, code), diagnostic)

    def test_old_native_missing_macro_is_never_semantic_rejection(self):
        for text in ("error: Variable `macro` not found", "ERROR: name 'macro' is not defined",
                "error: Object has no attribute 'macro'"):
            for diagnostic in suite.DIAGNOSTICS:
                with self.assertRaises(ValueError):
                    suite.require_rejection(record(text), diagnostic)

    def test_actual_native_diagnostic_shapes(self):
        for diagnostic, text in {
                'visibility': 'bazel-rs: //app:demo is not visible to //app:consumer',
                'missing-target': 'bazel-rs: source target //app:physical is not declared by exports_files or a rule in its package',
                'namespace': 'bazel-rs: target //app:outside is outside symbolic macro namespace demo: expected the macro name or its prefix',
                'positional': 'error: Wrong number of positional arguments, expected 0, got 1',
                'unexported': 'error: symbolic macro must be exported before invocation',
                'glob-phase': 'error: glob() is not allowed inside a symbolic macro',
                'package-group': 'error: package_group visibility is not implemented: //app:friends',
                }.items():
            suite.require_rejection(record(text), diagnostic)
            with self.assertRaises(ValueError):
                suite.require_rejection(record('    '+text), diagnostic)

    def test_infrastructure_flags_never_pass(self):
        for flag in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
            r = record('Error: unexpected positional arguments')
            r[flag] = True
            with self.assertRaises(ValueError):
                suite.require_rejection(r, 'positional')

    def test_callback_marker_not_source_excerpt(self):
        self.assertEqual(suite.marker_count(record('    print("MACRO_CALLBACK")\n'), 'MACRO_CALLBACK'), 0)
        self.assertEqual(suite.marker_count(record('DEBUG: /tmp/defs.bzl:2:5: MACRO_CALLBACK\n'), 'MACRO_CALLBACK'), 1)


class ReplayTests(unittest.TestCase):
    def positive(self, root):
        item, probe, engine = suite.CASEPLAN[0], 'private-closure', 'native'
        value = suite.fixture(item['name'], probe)
        workspace = root / 'workspace'
        workspace.mkdir()
        paths = suite.materialize(workspace, value)
        cache = root / 'cases' / item['name'] / probe / 'native-cache'
        before = suite.action_store(cache)
        outputs = []
        for name, raw in value['expected'].items():
            output = cache / 'actions' / ('a'*64) / 'outputs' / name
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(raw)
            output.chmod(0o555)
            outputs.append(output)
        args = suite.argument_parser().parse_args([])
        reference = suite.DirectReference(args.reference, workspace, root, None, 'symbolic-macros')
        records = []
        for i, (command, target, extra) in enumerate(suite.steps_for(value, engine)):
            rec = record('MACRO_READY\nMACRO_CALLBACK\n'+('2 actions executed, 0 cache hits' if i == 0 else '0 actions executed, 2 cache hits'), 0)
            rec.update(id=i, name='functional.'+probe+'.native.'+str(i), cwd=str(workspace), timeout_seconds=args.timeout,
                argv=suite.command_argv(args, reference, engine, cache, root / 'registry', command, target, extra),
                stdout=''.join(target+'\t'+str(p)+'\n' for p in outputs))
            records.append(rec)
        inputs = suite.snapshot(paths)
        entries = [suite.evidence(p) for p in outputs]
        phase = dict(probe=probe, engine=engine, status='PASS', runs=[0, 1], cache=str(cache),
            inputs_before=inputs, inputs_after=copy.deepcopy(inputs), outputs=[entries, copy.deepcopy(entries)],
            action_store_before=before, action_store_after=suite.action_store(cache))
        return phase, dict(runs=records, scratch_directory=str(root)), args, workspace, reference, item

    def test_replay_rejects_mutated_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.positive(Path(tmp))
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: r['runs'][1].update(stderr='0 actions executed, 2 cache hits'),
                    lambda p, r: r['runs'][0].update(stderr='MACRO_READY\nMACRO_CALLBACK\n0 actions executed, 2 cache hits'),
                    lambda p, r: p['outputs'][0][0].update(mode=0o755),
                    lambda p, r: p['outputs'][0][0].update(sha256='0'*64),
                    lambda p, r: p['outputs'][0][0].update(path='/invented/demo.custom'),
                    lambda p, r: p['runs'].pop(),
                    lambda p, r: p['runs'].append(0),
                    lambda p, r: p['inputs_before'].clear(),
                    lambda p, r: p.update(engine='invented'),
                    lambda p, r: p.update(status='SKIP'),
                    lambda p, r: p['action_store_before']['actions'].update(exists=True),
                    lambda p, r: r['runs'][0].update(argv=['fake', 'build']),
                    lambda p, r: r['runs'][0].update(cwd='/wrong')):
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises((ValueError, KeyError, IndexError)):
                    suite.validate_phase(phase, report, *values[2:])

    def test_rehashed_source_cannot_replace_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            phase, report, *rest = self.positive(Path(tmp))
            path = next(p for p in phase['inputs_before'] if p.endswith('m.bzl'))
            raw = b'changed but rehashed\n'
            for field in ('inputs_before', 'inputs_after'):
                phase[field][path].update(bytes_base64=base64.b64encode(raw).decode(), sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable fixture'):
                suite.validate_phase(phase, report, *rest)

    def test_real_action_roots_and_no_wrong_nonexistent_path_claim(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            suite.no_actions(suite.action_store(cache), cache)
            (cache / 'actions' / 'leak').mkdir(parents=True)
            (cache / 'actions/leak/manifest.json').write_text('{}')
            with self.assertRaises(ValueError):
                suite.no_actions(suite.action_store(cache), cache)
            store = suite.action_store(Path('/invented'))
            with self.assertRaises(ValueError):
                suite.no_actions(store, cache)

    def test_check_requires_every_ordered_phase_engine_and_status(self):
        item = suite.CASEPLAN[0]
        args = suite.argument_parser().parse_args(['--reference-only'])
        check = dict(name=item['name'], kind=item['kind'], status='PASS', phases=[])
        with mock.patch.object(suite, 'validate_phase') as replay:
            for pairs in ([], list(reversed(suite.phases_for(item, True))),
                    suite.phases_for(item, True)+[('unknown', 'reference')],
                    [(p, 'native') for p, _ in suite.phases_for(item, True)]):
                check['phases'] = [dict(probe=p, engine=e) for p, e in pairs]
                with self.assertRaisesRegex(ValueError, 'missing, reordered'):
                    suite.validate_check(check, {}, args, Path('/unused'), None)
            replay.assert_not_called()

    def test_boundary_reference_claim_rejected(self):
        item = suite.CASEPLAN[-1]
        args = suite.argument_parser().parse_args(['--reference-only'])
        check = dict(name=item['name'], kind=item['kind'], status='PASS',
            phases=[dict(probe='name-none', engine='reference')])
        with self.assertRaises(ValueError):
            suite.validate_check(check, {}, args, Path('/unused'), None)

    def test_pass_labels_are_not_evidence(self):
        for report in ({}, {'status': 'PASS'}, {'checks': [dict(name=n, kind=k, status='PASS') for n, k in suite.PLAN]}):
            self.assertFalse(suite.report_passes(report))

    def reference_report(self, root, item):
        """Synthetic reduced plan; exact real replay code, no executed CLI claim."""
        args = suite.argument_parser().parse_args(['--reference-only'])
        value = suite.fixture('functional', 'private-closure')
        workspace = root / 'workspace'
        workspace.mkdir()
        paths = suite.materialize(workspace, value)
        reference = suite.DirectReference(args.reference, workspace, root, None, 'symbolic-macros')
        cache = root / 'cases/functional/private-closure/native-cache'
        output_names = ['bazel-out/synthetic/bin/app/'+name for name in value['expected']]
        entries = []
        for name, output_name in zip(value['expected'], output_names):
            path = reference.output_base / 'execroot/_main' / output_name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(value['expected'][name])
            path.chmod(0o555)
            entries.append(suite.evidence(path))
        records = []
        def append(name, argv, stdout='', stderr='', timeout=args.timeout):
            r = record(stderr, 0, id=len(records), name=name, argv=argv, cwd=str(workspace), timeout_seconds=timeout,
                executable_sha256=suite.REFERENCE_SHA256)
            r['stdout'] = stdout
            for field in ('stdout', 'stderr'):
                raw = r[field].encode()
                r.update({field+'_base64': base64.b64encode(raw).decode(), field+'_sha256': suite.digest(raw),
                    field+'_full_sha256': suite.digest(raw), field+'_full_size_bytes': len(raw)})
            records.append(r)
        append('reference.version', reference.startup+['version', '--gnu_format'], 'bazel 9.3.0\n')
        for index, (command, target, extra) in enumerate(suite.steps_for(value, 'reference')):
            append('functional.private-closure.reference.'+str(index),
                suite.command_argv(args, reference, 'reference', cache, root / 'registry', command, target, extra),
                '\n'.join(output_names)+'\n' if command == 'cquery' else '',
                'MACRO_READY\nMACRO_CALLBACK\n' if index == 0 else '')
        append('symbolic-macros.shutdown', reference.startup+['shutdown']+list(suite.COMMON_FLAGS), timeout=min(args.timeout, 15))
        sources = suite.snapshot(suite.SOURCES)
        registry = {str(root / 'registry' / Path(p).relative_to(suite.REGISTRY)): dict(e,
            path=str(root / 'registry' / Path(p).relative_to(suite.REGISTRY)))
            for p, e in sources.items() if Path(p).is_relative_to(suite.REGISTRY)}
        phase = dict(probe='private-closure', engine='reference', cache=str(cache), runs=[1, 2, 3, 4],
            outputs=[entries, copy.deepcopy(entries)], status='PASS', inputs_before=suite.snapshot(paths),
            inputs_after=suite.snapshot(paths), action_store_before=None, action_store_after=None)
        return dict(schema_version=1, status='REFERENCE_ONLY_PASS',
            configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
            reference_source_pin=suite.SOURCE_PIN, planned_checks=[item],
            plan_sha256=suite.digest(json.dumps([item], sort_keys=True).encode()), limitations=suite.LIMITATIONS,
            errors=[], cleanup=[], scratch_cleaned=True, reference_shutdown_succeeded=True,
            sources_before=sources, sources_after=copy.deepcopy(sources),
            executables={'reference': {'sha256': suite.REFERENCE_SHA256}},
            executables_after={'reference': {'sha256': suite.REFERENCE_SHA256}}, reference_release='bazel 9.3.0',
            completed_phases=1, completed_checks=1, scratch_directory=str(root),
            cleanup_receipt=dict(path=str(root), removed=True), registry_before=registry,
            registry_after=copy.deepcopy(registry), runs=records,
            checks=[dict(name='functional', kind='differential', status='PASS', phases=[phase])])

    def test_complete_report_replay_and_plan_sha_cleanup_mutations(self):
        item = suite.common.case('functional', ('private-closure',))
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(suite, 'CASEPLAN', [item]), mock.patch.object(
                suite, 'PLAN', [('functional', 'differential')]):
            report = self.reference_report(Path(tmp), item)
            self.assertTrue(suite.report_passes(report))
            for mutate in (
                    lambda r: r.update(plan_sha256='0'*64),
                    lambda r: r.update(status='PASS'),
                    lambda r: r.update(reference_source_pin='0'*40),
                    lambda r: r['planned_checks'][0]['probes'].append('invented'),
                    lambda r: r['checks'][0]['phases'].clear(),
                    lambda r: r['checks'][0]['phases'][0].update(engine='native'),
                    lambda r: r['checks'][0]['phases'][0].update(status='FAIL'),
                    lambda r: r['checks'][0].update(status='SKIP'),
                    lambda r: r.update(scratch_cleaned=False),
                    lambda r: r['cleanup_receipt'].update(removed=False),
                    lambda r: r.update(reference_shutdown_succeeded=False),
                    lambda r: r['cleanup'].append({'forced': True}),
                    lambda r: r['sources_after'].clear(),
                    lambda r: r['registry_after'].clear(),
                    lambda r: r['executables']['reference'].update(sha256='0'*64),
                    lambda r: r['executables_after']['reference'].update(sha256='0'*64),
                    lambda r: r['runs'][1].update(stderr='changed stream'),
                    lambda r: r['runs'][1].update(environment={'forbidden': 'serialization'}),
                    lambda r: r['runs'][1].update(executable_sha256='0'*64),
                    lambda r: r['runs'].pop(),
                    lambda r: r.update(completed_phases=0)):
                changed = copy.deepcopy(report)
                mutate(changed)
                self.assertFalse(suite.report_passes(changed))

    def test_runner_never_serializes_environment(self):
        report = {'runs': []}
        runner = suite.RecordingRunner(report, 10)
        def parent_run(self, *args, **kwargs):
            rec = {'environment': {'TEST_ONLY': 'not-for-report'}, 'environment_keys': ['TEST_ONLY']}
            self.report['runs'].append(rec)
            return rec
        with mock.patch.object(suite.extensions.RecordingRunner, 'run', parent_run):
            rec = runner.run('synthetic', ['not-executed'], Path('/unused'))
        self.assertNotIn('environment', rec)
        self.assertNotIn('environment_keys', rec)

    def test_cleanup_on_reference_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            def run(self, name, argv, cwd, timeout=None):
                r = record('forced version failure', 1, name=name)
                self.report['runs'].append(r)
                return r
            def shutdown(self):
                self.runner.report['runs'].append(record('', 0))
            with mock.patch.object(suite, 'snapshot', return_value={}), mock.patch.object(
                    suite.common, 'executable_evidence', return_value={}), mock.patch.object(
                    suite.RecordingRunner, 'run', run), mock.patch.object(suite.DirectReference, 'shutdown', shutdown):
                code = suite.main(['--reference-only', '--scratch-parent', str(root), '--output', str(root / 'report.json')])
            self.assertEqual(code, 1)
            report = json.loads((root / 'report.json').read_text())
            self.assertTrue(report['reference_shutdown_succeeded'])
            self.assertTrue(report['scratch_cleaned'])
            self.assertFalse(Path(report['scratch_directory']).exists())
            self.assertEqual(report['cleanup_receipt'], dict(path=report['scratch_directory'], removed=True))


if __name__ == '__main__':
    unittest.main()
