"""Harness unit tests. Synthetic records are NOT CLI or Starlark parity evidence."""

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
import validate_string_list_dict as suite


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **changes)


MESSAGES = {
    'key': "expected value of type 'string' for dict key element, but got 1 (int)",
    'value': "expected value of type 'list(string)' for dict value element, but got 'x' (string)",
    'element': "expected value of type 'string' for element 0 of dict value element, but got 1 (int)",
    'dict': "expected value of type 'dict(string, list(string))' for attribute 'd', but got [] (list)",
    'mandatory': "mandatory attribute 'd' isn't being specified",
    'empty': "attribute 'd' must be non empty",
    'freeze-dict': 'Error: trying to mutate a frozen dict value',
    'freeze-list': 'Error in append: trying to mutate a frozen list value',
    'default-none': "parameter 'default' got value of type 'NoneType', want 'dict'",
    'default-callable': "parameter 'default' got value of type 'function', want 'dict'",
    'doc-int': "parameter 'doc' got value of type 'int', want 'string or NoneType'",
    'allow-int': "parameter 'allow_empty' got value of type 'int', want 'bool'",
    'mandatory-int': "parameter 'mandatory' got value of type 'int', want 'bool'",
    'two-positional': 'string_list_dict() accepts no more than 1 positional argument but got 2',
    'configurable': 'attr.string_list_dict configurable is unsupported',
    'ascii': 'only ASCII strings are supported',
}


class PlanTests(unittest.TestCase):
    def test_unique_complete_plan(self):
        self.assertEqual(len(suite.CASES), 8)
        self.assertEqual(len(set(suite.PLAN)), 8)
        self.assertEqual([c['kind'] for c in suite.CASES], ['differential']*7+['native_boundary'])
        for item in suite.CASES:
            self.assertEqual(len(set(item['probes'])), len(item['probes']))
            for probe, engine in suite.phases_for(item):
                value = suite.fixture(item['name'], probe)
                steps = suite.steps_for(item, probe, engine)
                self.assertEqual(steps[0], ('query', value['query'], []))
                self.assertTrue(all(cmd in ('query', 'build', 'cquery') for cmd, _, _ in steps))
                if not value['diagnostic']:
                    self.assertEqual(sum(cmd == 'build' and target == '//:result' for cmd, target, _ in steps), 2)

    def test_unknown_cases_and_probes_do_not_fall_back(self):
        for name, probe in [('invented', 'snapshots'), ('custom', 'invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)

    def test_lexical_fixture_structure_not_starlark_emulation(self):
        for item in suite.CASES:
            for probe in item['probes']:
                for path, raw in suite.fixture(item['name'], probe)['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(name=item['name'], probe=probe, path=path):
                            ast.parse(raw.decode(), filename=path)
                            if 'ascii-' not in probe:
                                self.assertTrue(raw.isascii())

    def test_all_positive_routes_use_real_consumer_action(self):
        for name, probe in [('custom', 'snapshots'), ('direct', 'module-late'),
                            ('ordinary', 'per-call'), ('tags', 'module-late')]:
            value = suite.fixture(name, probe)
            self.assertIn(b'cat $(SRCS) > $@', value['files']['BUILD.bazel'])
            self.assertIn(b'ctx.actions.write' if name == 'custom' else b'ctx.file', value['files']['defs.bzl'])
            self.assertIn(b'd.keys()', value['files']['defs.bzl'])
            self.assertNotIn(b'json.encode', value['files']['defs.bzl'])
        self.assertEqual(suite.fixture('ordinary', 'per-call')['repos'], 2)

    def test_snapshot_boundaries_and_frozen_diagnostics_are_separate(self):
        self.assertEqual(suite.fixture('custom', 'snapshots')['expected'], suite.BEFORE*2)
        self.assertEqual(suite.fixture('ordinary', 'per-call')['expected'], suite.BEFORE+suite.AFTER)
        for name in ('direct', 'tags'):
            value = suite.fixture(name, 'module-late')
            self.assertEqual(value['expected'], suite.AFTER)
            self.assertIn(suite.MUTATE.encode(), value['files']['MODULE.bazel'])
        for route in ('custom', 'direct', 'tags'):
            for shape in ('dict', 'list'):
                value = suite.fixture('controls', route+'.freeze-'+shape)
                self.assertEqual(value['diagnostic'], 'freeze-'+shape)
                self.assertEqual(value['failure_command'], 'build' if route == 'custom' else 'query')

    def test_allow_empty_analysis_is_not_loading_failure(self):
        for probe in ('empty-explicit', 'empty-default'):
            value = suite.fixture('controls', 'custom.'+probe)
            self.assertEqual(value['failure_command'], 'build')
            item = suite.CASES[6]
            self.assertEqual(suite.steps_for(item, 'custom.'+probe, 'native'),
                             [('query', '//:one', []), ('build', '//:result', [])])
            for route in ('direct', 'tags'):
                self.assertIsNone(suite.fixture('controls', route+'.'+probe)['diagnostic'])
        for route in ('custom', 'direct', 'tags'):
            self.assertEqual(suite.fixture('controls', route+'.nested-empty')['expected'], b'dict\nz:list:\n')

    def test_provenance_changes_only_call_input_order(self):
        for route in ('direct', 'ordinary'):
            first = suite.fixture('provenance', route+'-first')
            changed = suite.fixture('provenance', route+'-reorder')
            self.assertEqual(first, suite.fixture('provenance', route+'-restore'))
            different = [p for p in first['files'] if first['files'][p] != changed['files'][p]]
            self.assertEqual(different, ['MODULE.bazel' if route == 'direct' else 'ext.bzl'])
            self.assertNotEqual(first['expected'], changed['expected'])

    def test_reference_only_has_no_native_boundary_or_native_commands(self):
        for item in suite.CASES:
            phases = suite.phases_for(item, True)
            self.assertTrue(all(engine == 'reference' for _, engine in phases))
            if item['kind'] == 'native_boundary':
                self.assertEqual(phases, [])
        args = suite.argument_parser().parse_args([])
        with self.assertRaisesRegex(ValueError, 'frozen candidate'):
            suite.executable_evidence(args)

    def test_adjacent_sequence_and_build_setting_defaults_are_real_tuples(self):
        value = suite.fixture('custom', 'sequence-tuples')
        self.assertIn(b'config.string_list()', value['files']['defs.bzl'])
        self.assertIn(b'build_setting_default=("x","y")', value['files']['BUILD.bazel'])
        self.assertIn(b'ctx.build_setting_value', value['files']['defs.bzl'])
        self.assertIn(b'attr.string_list(default=("x","y"))', value['files']['defs.bzl'])
        self.assertIn(b'attr.label_list(allow_files=True,default=(":input",))', value['files']['defs.bzl'])
        self.assertIn(b'outs=("extra.txt",)', value['files']['BUILD.bazel'])
        self.assertEqual(value['actions'], 3)
        self.assertIn(b'list:x,y\nlist:1\nlist:1\nlist:x,y\nextra\n', value['expected'])


class DiagnosticTests(unittest.TestCase):
    def test_controlled_classes(self):
        self.assertEqual(set(MESSAGES), set(suite.DIAGNOSTICS))
        for diagnostic, text in MESSAGES.items():
            suite.require_rejection(record(text), diagnostic)

    def test_infrastructure_and_wrong_phase_never_count_as_expected_errors(self):
        for diagnostic, text in MESSAGES.items():
            for bad in ('panicked', 'network unavailable', 'connection refused', 'syntax error',
                        'permission denied', 'falling back', 'failed to download', 'internal error'):
                with self.subTest(diagnostic=diagnostic, bad=bad), self.assertRaises(ValueError):
                    suite.require_rejection(record(text+'\n'+bad), diagnostic)
            for code in (0, True, -9, 101, 124, 125, 126, 127, 137):
                with self.assertRaises(ValueError):
                    suite.require_rejection(record(text, code), diagnostic)
            for flag in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
                value = record(text)
                value[flag] = True
                with self.assertRaises(ValueError):
                    suite.require_rejection(value, diagnostic)
            with self.assertRaises(ValueError):
                suite.require_rejection(record('generic build failed'), diagnostic)
        # Since N20 the native text is Bazel's Starlark.checkMutable text; the
        # former native 'error: Immutable' is rejected for both engines.
        suite.require_rejection(record('error: trying to mutate a frozen dict value'), 'freeze-dict', 'native')
        suite.require_rejection(record('error: trying to mutate a frozen list value'), 'freeze-list', 'native')
        for diagnostic in ('freeze-dict', 'freeze-list'):
            for engine in ('native', 'reference'):
                with self.assertRaises(ValueError):
                    suite.require_rejection(record('error: Immutable'), diagnostic, engine)
        with self.assertRaises(ValueError):
            suite.require_rejection(record('error: trying to mutate a frozen list value'), 'freeze-dict', 'native')
        for text in ('Immutable download failed', 'something Immutable elsewhere', 'Immutable'):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(text), 'freeze-list')
        with self.assertRaises(ValueError):
            suite.require_rejection(record(MESSAGES['freeze-list']), 'freeze-dict')

    def test_exact_action_counts(self):
        suite.action_counts(record('2 actions executed, 0 cache hits', 0), (2, 0))
        for text in ('', '1 actions executed, 0 cache hits',
                     '2 actions executed, 0 cache hits\n2 actions executed, 0 cache hits'):
            with self.assertRaises(ValueError):
                suite.action_counts(record(text, 0), (2, 0))

    def test_native_generic_shape_does_not_weaken_reference_categories(self):
        text = 'A=attr.string_list_dict(default=None)\nerror: expected a dict of strings to a list of strings (or tuple of strings)\n'
        suite.require_rejection(record(text), 'default-none', 'native')
        with self.assertRaises(ValueError):
            suite.require_rejection(record(text), 'default-none', 'reference')
        with self.assertRaises(ValueError):
            suite.require_rejection(record(text), 'default-callable', 'native')
        with self.assertRaises(ValueError):
            suite.require_rejection(record(text.splitlines()[1]), 'key', 'native')

    def test_exact_query_identity(self):
        value = suite.fixture('direct', 'tuples')
        good = record('', 0)
        good['stdout'] = '@@+r+one//:data\n'
        suite.require_query(good, value, 'native')
        for text in ('', '@one//:data\n', '@@+r+one//:data\n//:extra\n'):
            good['stdout'] = text
            with self.assertRaises(ValueError):
                suite.require_query(good, value, 'native')


class ReplayTests(unittest.TestCase):
    def prepare_positive(self, root):
        item, probe, engine = suite.CASES[0], 'none-default', 'native'
        value = suite.fixture(item['name'], probe)
        workspace, cache = root / 'workspace', root / 'case/native-cache'
        registry, paths = suite.materialize(root / 'case', workspace, value)
        args = suite.argument_parser().parse_args([])
        reference = suite.DirectReference(args.reference, workspace, root, None, 'string-list-dict')
        output = cache / 'result.txt'
        output.parent.mkdir()
        output.write_bytes(value['expected'])
        output.chmod(0o555)
        records = []
        for index, (command, target, extra) in enumerate(suite.steps_for(item, probe, engine)):
            rec = record('' if index == 0 else '2 actions executed, 0 cache hits' if index == 1 else '0 actions executed, 2 cache hits', 0)
            rec.update(id=index, name='custom.none-default.native.'+str(index), cwd=str(workspace),
                timeout_seconds=args.timeout, stdout='//:one\n' if index == 0 else '//:result\t'+str(output)+'\n',
                argv=suite.extensions.command_argv(args, reference, engine, cache, registry, command, target, extra))
            records.append(rec)
        inputs = suite.snapshot(paths)
        phase = dict(probe=probe, engine=engine, status='PASS', runs=[0, 1, 2],
            outputs=[suite.evidence(output), suite.evidence(output)], sources=[], publication_entries=[],
            cache=str(cache), registry=str(registry), inputs_before=inputs, inputs_after=copy.deepcopy(inputs))
        return phase, dict(runs=records, scratch_directory=str(root)), args, workspace, reference, item

    def prepare_negative(self, root):
        item = suite.CASES[5]
        probe, engine = 'direct.key-int', 'reference'
        value = suite.fixture(item['name'], probe)
        workspace = root / 'workspace'
        registry, paths = suite.materialize(root / 'case', workspace, value)
        args = suite.argument_parser().parse_args(['--reference-only'])
        reference = suite.DirectReference(args.reference, workspace, root, None, 'string-list-dict')
        cache = root / 'case/native-cache'
        command, target, extra = suite.steps_for(item, probe, engine)[0]
        rec = record(MESSAGES['key'])
        rec.update(id=0, name='typing.direct.key-int.reference.0', cwd=str(workspace), timeout_seconds=args.timeout,
                   argv=suite.extensions.command_argv(args, reference, engine, cache, registry, command, target, extra))
        inputs = suite.snapshot(paths)
        phase = dict(probe=probe, engine=engine, status='PASS', runs=[0], outputs=[], sources=[],
                     cache=str(cache), registry=str(registry), inputs_before=inputs, inputs_after=copy.deepcopy(inputs))
        return phase, dict(runs=[rec]), args, workspace, reference, item

    def test_replay_rederives_fixture_commands_diagnostics_and_inputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.prepare_negative(Path(tmp))
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: p['runs'].clear(),
                    lambda p, r: p['runs'].append(0),
                    lambda p, r: p.update(status='FAIL'),
                    lambda p, r: p['inputs_before'].clear(),
                    lambda p, r: r['runs'][0].update(stderr='generic error'),
                    lambda p, r: r['runs'][0].update(argv=['fake', 'query']),
                    lambda p, r: r['runs'][0].update(name='different.phase'),
                    lambda p, r: r['runs'][0].update(cwd='/other'),
                    lambda p, r: p['outputs'].append({'invented': True})):
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises((ValueError, KeyError)):
                    suite.validate_phase(phase, report, *values[2:])

    def test_rehashed_altered_fixture_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            phase, report, *rest = self.prepare_negative(Path(tmp))
            path = next(p for p in phase['inputs_before'] if p.endswith('defs.bzl'))
            raw = b'not the planned fixture\n'
            for key in ('inputs_before', 'inputs_after'):
                phase[key][path].update(bytes_base64=base64.b64encode(raw).decode(),
                                       sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable fixture'):
                suite.validate_phase(phase, report, *rest)

    def test_positive_replay_rejects_bytes_modes_paths_counts_and_query_tampering(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.prepare_positive(Path(tmp))
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: p['outputs'].pop(),
                    lambda p, r: p['outputs'][0].update(mode=0o755),
                    lambda p, r: p['outputs'][0].update(path='/unrelated/result.txt'),
                    lambda p, r: p['outputs'][0].update(sha256='0'*64),
                    lambda p, r: r['runs'][0].update(stdout='//:wrong\n'),
                    lambda p, r: r['runs'][1].update(stderr='0 actions executed, 2 cache hits'),
                    lambda p, r: p['publication_entries'].append('a'*64)):
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises(ValueError):
                    suite.validate_phase(phase, report, *values[2:])

    def test_reference_provenance_is_batch_and_isolated_native_cache_is_unchanged(self):
        root = Path('/private/provenance/direct')
        reference = suite.DirectReference(Path('/official/bazel'), Path('/private/workspace'), Path('/private'), None, 'string-list-dict')
        first = suite.phase_reference(reference, suite.CASES[4], 'direct-first', root)
        changed = suite.phase_reference(reference, suite.CASES[4], 'direct-reorder', root)
        self.assertIn('--batch', first.startup)
        self.assertNotEqual(first.output_base.parent, changed.output_base.parent)
        self.assertNotEqual(first.output_base.parent, root / 'native-cache')
        self.assertIs(suite.phase_reference(reference, suite.CASES[0], 'snapshots', root), reference)

    def test_pass_labels_and_missing_reports_never_mint_pass(self):
        for report in ({}, {'status': 'PASS'}, {'checks': [dict(name=n, kind=k, status='PASS') for n, k in suite.PLAN]}):
            self.assertFalse(suite.report_passes(report))

    def test_plan_order_is_required_before_replaying_any_phase(self):
        item = suite.CASES[0]
        args = suite.argument_parser().parse_args(['--reference-only'])
        check = dict(name=item['name'], kind=item['kind'], status='PASS', phases=[])
        with self.assertRaisesRegex(ValueError, 'missing, reordered'):
            suite.validate_check(check, {}, args, Path('/unused'), None)
        phases = [dict(probe=p, engine=e) for p, e in suite.phases_for(item, True)]
        check['phases'] = list(reversed(phases))
        with mock.patch.object(suite, 'validate_phase') as replay:
            with self.assertRaisesRegex(ValueError, 'missing, reordered'):
                suite.validate_check(check, {}, args, Path('/unused'), None)
            replay.assert_not_called()


if __name__ == '__main__':
    unittest.main()
