"""Harness-only tests; synthetic records do not establish Starlark parity."""

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
import validate_existing_rules as suite


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **changes)


class PlanTests(unittest.TestCase):
    def test_exact_unique_plan(self):
        self.assertEqual(len(suite.CASEPLAN), 6)
        self.assertEqual(len(set(suite.PLAN)), 6)
        self.assertEqual([len(c['probes']) for c in suite.CASEPLAN], [1, 14, 11, 4, 4, 2])
        self.assertEqual(sum(len(suite.phases_for(c)) for c in suite.CASEPLAN), 70)
        self.assertEqual(sum(len(suite.phases_for(c, True)) for c in suite.CASEPLAN), 34)
        for item in suite.CASEPLAN:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe, engine in suite.phases_for(item):
                value = suite.fixture(item['name'], probe)
                self.assertEqual(sum(cmd == 'build' for cmd, _, _ in suite.steps_for(value, engine)), 1 if value['diagnostic'] else 2)

    def test_lexical_structure(self):
        for item in suite.CASEPLAN:
            for probe in item['probes']:
                for path, raw in suite.fixture(item['name'], probe)['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(probe=probe, path=path):
                            ast.parse(raw.decode(), filename=path)

    def test_unknown_plan_never_falls_back(self):
        for name, probe in [('invented', 'x'), ('genuine', 'invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)

    def test_native_boundary_never_claims_differential_absence(self):
        item = suite.CASEPLAN[-1]
        self.assertEqual(item['kind'], 'native_boundary')
        self.assertEqual(suite.phases_for(item, True), [])
        self.assertTrue(all(e == 'native' for _, e in suite.phases_for(item)))
        for fn in suite.FUNCTIONS:
            value = suite.fixture('native-boundary', fn)
            self.assertIn(('hasattr(native,'+repr(fn)+')').encode(), value['files']['consumer.bzl'])

    def test_frozen_native_gate(self):
        with self.assertRaisesRegex(ValueError, 'frozen candidate'):
            suite.common.executable_evidence(suite.argument_parser().parse_args([]))

    def test_aliases_are_bound_at_init_not_invoked(self):
        tree = ast.parse(suite.ALIASES)
        self.assertTrue(all(isinstance(n.value, ast.Attribute) for n in tree.body[:2]))
        value = suite.fixture('extension', 'before-after-alias-copy')
        ext = value['files']['ext.bzl']
        self.assertLess(ext.index(b'r(name="made"'), ext.index(b'r(name="after",text=observe()'))
        self.assertIn(b'type(one(""))', ext)
        self.assertIn(b'one("../bad")', ext)
        self.assertIn(b'd=dict(all_rules())', ext)
        self.assertEqual((value['callbacks'], value['repos']), (2, 2))

    def test_dynamic_alias_repository_phase_routes(self):
        for route in ('direct', 'ordinary'):
            for stage in ('init', 'before-write', 'after-write'):
                for fn in suite.FUNCTIONS:
                    value = suite.fixture('phase', route+'.'+stage+'.'+fn)
                    self.assertEqual(value['diagnostic'], fn)
                    self.assertIn(suite.CALLS[fn].encode(), value['files']['defs.bzl'])
                    self.assertEqual(b'ctx.file("partial"' in value['files']['defs.bzl'], stage == 'after-write')
                    if route == 'ordinary' and stage != 'init':
                        self.assertIn(b'observe()\n    r(name="made")', value['files']['ext.bzl'])
            value = suite.fixture('phase', route+'.child-ctor')
            self.assertIn(b'A=ctor()', value['files']['defs.bzl'])
            self.assertIn(b'all_rules()', value['files']['child.bzl'])

    def test_argument_matrix_matches_reference_contract(self):
        contract = json.loads(suite.CONTRACT.read_text())
        observed = {r['name'].removeprefix('extension.signature.') for r in contract['runs'] if r['name'].startswith('extension.signature.')}
        self.assertEqual(set(suite.ARGUMENTS), observed)
        for key, expression in suite.ARGUMENTS.items():
            value = suite.fixture('arguments', key)
            self.assertIn(expression.encode(), value['files']['ext.bzl'])
            self.assertIsNotNone(value['diagnostic'])

    def test_frozen_all_four_operations(self):
        self.assertEqual(set(suite.MUTATIONS), {'setitem', 'update', 'setdefault', 'clear-empty'})
        for probe, expression in suite.MUTATIONS.items():
            value = suite.fixture('frozen', probe)
            self.assertEqual(value['diagnostic'], 'frozen')
            self.assertIn(('d=all_rules()\n    '+expression).encode(), value['files']['ext.bzl'])

    def test_genuine_maybe_not_copied_or_bypassed(self):
        for probe, count in [('maybe-single', 1), ('maybe-duplicate', 2)]:
            value = suite.fixture('genuine', probe)
            ext = value['files']['ext.bzl']
            self.assertIn(b'load("@bazel_tools//tools/build_defs/repo:utils.bzl","maybe")', ext)
            self.assertEqual(ext.count(b'maybe(r,name="made")'), count)
            self.assertNotIn(b'def maybe', ext)
            self.assertNotIn(b'getattr(native', ext)
            self.assertEqual(value['diagnostic'], 'duplicate' if count == 2 else None)

    def test_whole_original_extension_consumer(self):
        value = suite.fixture('genuine', 'full-extension-four-files')
        self.assertNotIn('ext.bzl', value['files'])
        self.assertNotIn('defs.bzl', value['files'])
        self.assertIn(b'use_extension("@bazel_features//private:extensions.bzl","version_extension")', value['files']['MODULE.bazel'])
        self.assertIn(b'use_repo_rule("//:consumer.bzl","copy")', value['files']['MODULE.bazel'])
        self.assertEqual(value['files']['consumer.bzl'].count(b'ctx.template('), 4)
        self.assertEqual(len(value['generated']), 4)
        self.assertEqual(len(value['expected']), 4)
        self.assertNotIn(b'features.bzl', value['files']['BUILD.bazel'])
        self.assertFalse(any(p.startswith('features/') for p in value['files']))
        original, _ = suite.archive_plan()
        self.assertIn(b'version_extension = modules.as_extension(bazel_features_repos)', original['features/private/extensions.bzl'][0])
        self.assertIn(b'@bazel_tools//tools/build_defs/repo:utils.bzl', original['features/private/repos.bzl'][0])

    def test_archive_all_bytes_modes_and_directory_modes(self):
        files, directories = suite.archive_plan()
        self.assertEqual((len(files), len(directories)), (41, 7))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            value = suite.fixture('genuine', 'full-extension-four-files')
            suite.materialize(root, value)
            self.assertEqual(suite.manifest(root, True), dict(files=sorted(files), directories=directories,
                directory_modes=suite.archive_directory_modes()))
            for path, (raw, mode) in files.items():
                self.assertEqual((root / path).read_bytes(), raw)
                self.assertEqual((root / path).stat().st_mode & 0o777, mode)

    def test_version_functional_is_real_load(self):
        value = suite.fixture('genuine', 'version-functional')
        self.assertIn(b'load("@bazel_features_version//:version.bzl","version")', value['files']['BUILD.bazel'])
        self.assertEqual(value['expected'], {'result.txt': b'9.3.0'})


class DiagnosticTests(unittest.TestCase):
    def test_native_argument_categories_without_function_name(self):
        messages = {
            'missing': 'Wrong number of positional arguments, expected 1, got 0',
            'too-many': 'Wrong number of positional arguments, expected 1, got 2',
            'rules-positional': 'Wrong number of positional arguments, expected 0, got 1',
            'named': 'Found `name` extra named parameter(s) for call to function',
            'unknown-kw': 'Found `unknown` extra named parameter(s) for call to function',
            'rules-keyword': 'Found `name` extra named parameter(s) for call to function',
            'type': "Type of parameter `name` doesn't match, expected `str`, actual `NoneType (repr: None)`",
        }
        for diagnostic, message in messages.items():
            suite.require_rejection(record('error: '+message+'\n'), diagnostic)
            with self.assertRaises(ValueError):
                suite.require_rejection(record('source example: '+message+'\n'), diagnostic)
        for wrong in ('Wrong number of positional arguments, expected 3, got 0',
                'Found `other` extra named parameter(s) for call to function',
                "Type of parameter `other` doesn't match, expected `str`, actual `int (repr: 1)`"):
            for diagnostic in messages:
                with self.assertRaises(ValueError):
                    suite.require_rejection(record('error: '+wrong+'\n'), diagnostic)

    def test_reference_errors_and_unrelated_failures(self):
        contract = json.loads(suite.CONTRACT.read_text())
        for r in contract['runs']:
            name = r['name']
            if name.startswith('extension.signature.'):
                diagnostic = suite.fixture('arguments', name.removeprefix('extension.signature.'))['diagnostic']
            elif name.startswith('extension.immutable.'):
                diagnostic = 'frozen'
            elif name == 'extension.genuine-maybe-duplicate-different-rules':
                diagnostic = 'duplicate'
            elif name.startswith(('module-init.', 'repository.')):
                diagnostic = name.split('.')[-1]
            else:
                continue
            with self.subTest(name=name):
                suite.require_rejection(r, diagnostic)
                for bad in ('panicked', 'network unavailable', 'permission denied', 'internal error', 'failed to download'):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(dict(r, stderr=r['stderr']+'\n'+bad), diagnostic)
                for code in (0, True, -9, 101, 124, 125, 126, 127, 137):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(dict(r, returncode=code), diagnostic)

    def test_infrastructure_flags_never_pass(self):
        for flag in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
            r = record('existing_rule'+suite.PHASE_TEXT)
            r[flag] = True
            with self.assertRaises(ValueError):
                suite.require_rejection(r, 'existing_rule')

    def test_marker_source_snippets_are_not_callbacks(self):
        self.assertEqual(suite.marker_count(record('    print("NATIVERUN")\n')), 0)
        self.assertEqual(suite.marker_count(record('DEBUG: /tmp/defs.bzl:2:5: NATIVERUN\n')), 1)


class ReplayTests(unittest.TestCase):
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

    def test_generated_manifest_is_publication_but_builtin_cache_is_not(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            builtin = cache / 'repositories/builtins/trees/source'
            builtin.parent.mkdir(parents=True)
            builtin.write_text('genuine source cache')
            self.assertEqual(suite.publication_evidence(cache), [])
            manifest = cache / 'repositories/direct/manifests/leak.json'
            manifest.parent.mkdir(parents=True)
            manifest.write_text('{}')
            self.assertEqual(suite.publication_evidence(cache), ['repositories/direct/manifests/leak.json'])

    def test_genuine_profile_command_is_bound_to_plan(self):
        args = suite.argument_parser().parse_args([])
        root = Path('/tmp/opencode/synthetic-not-executed')
        ref = suite.DirectReference(args.reference, root / 'workspace', root, None, 'existing-rules')
        for item in suite.CASEPLAN:
            for engine in ('native', 'reference'):
                argv = suite.command_argv(args, ref, engine, root / 'cache', root / 'registry', 'build', '//:result', [], item)
                self.assertEqual('--experimental-builtin-modules' in argv, engine == 'native' and item['name'] == 'genuine')

    def positive(self, root):
        item, probe, engine = suite.CASEPLAN[0], 'before-after-alias-copy', 'native'
        value = suite.fixture(item['name'], probe)
        workspace = root / 'workspace'
        workspace.mkdir()
        paths = suite.materialize(workspace, value)
        cache = root / 'cases/extension' / probe / 'native-cache'
        cache.mkdir(parents=True)
        output = cache / 'result.txt'
        output.write_bytes(value['expected']['result.txt'])
        output.chmod(0o555)
        args = suite.argument_parser().parse_args([])
        reference = suite.DirectReference(args.reference, workspace, root, None, 'existing-rules')
        records = []
        for i, (command, target, extra) in enumerate(suite.steps_for(value, engine)):
            rec = record('NATIVERUN\nNATIVERUN\n'+('1 actions executed, 0 cache hits' if i == 0 else '0 actions executed, 1 cache hits'), 0)
            rec.update(id=i, name='extension.'+probe+'.native.'+str(i), cwd=str(workspace), timeout_seconds=args.timeout,
                argv=suite.extensions.command_argv(args, reference, engine, cache, root / 'registry', command, target, extra),
                stdout='//:result\t'+str(output)+'\n')
            records.append(rec)
        inputs = suite.snapshot(paths)
        trees = ['a'*64, 'b'*64]
        phase = dict(probe=probe, engine=engine, status='PASS', runs=[0, 1], cache=str(cache),
            inputs_before=inputs, inputs_after=copy.deepcopy(inputs), manifest_before=None, manifest_after=None,
            outputs=[[suite.evidence(output)], [suite.evidence(output)]], generated=[{}, {}],
            trees=[trees, trees], publication_entries=trees, cache_files=['result.txt'])
        return phase, dict(runs=records, scratch_directory=str(root)), args, workspace, reference, item

    def test_replay_rejects_mutations(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.positive(Path(tmp))
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: r['runs'][1].update(stderr='0 actions executed, 1 cache hits'),
                    lambda p, r: r['runs'][0].update(stderr='NATIVERUN\nNATIVERUN\n0 actions executed, 1 cache hits'),
                    lambda p, r: p['outputs'][0][0].update(mode=0o755),
                    lambda p, r: p['outputs'][0][0].update(sha256='0'*64),
                    lambda p, r: p['outputs'][0][0].update(path='/invented/result.txt'),
                    lambda p, r: p['publication_entries'].append('.stage'),
                    lambda p, r: p['runs'].pop(),
                    lambda p, r: p['runs'].append(0),
                    lambda p, r: p['generated'][0].update(invented={}),
                    lambda p, r: p['inputs_before'].clear(),
                    lambda p, r: p.update(manifest_before={'files': []}),
                    lambda p, r: r['runs'][0].update(argv=['fake', 'build']),
                    lambda p, r: r['runs'][0].update(cwd='/wrong')):
                phase, report = copy.deepcopy(values[:2])
                mutate(phase, report)
                with self.assertRaises((ValueError, KeyError, IndexError)):
                    suite.validate_phase(phase, report, *values[2:])

    def test_rehashed_source_cannot_replace_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            phase, report, *rest = self.positive(Path(tmp))
            path = next(p for p in phase['inputs_before'] if p.endswith('defs.bzl'))
            raw = b'changed but rehashed\n'
            for field in ('inputs_before', 'inputs_after'):
                phase[field][path].update(bytes_base64=base64.b64encode(raw).decode(), sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable fixture'):
                suite.validate_phase(phase, report, *rest)

    def test_check_requires_every_ordered_phase(self):
        item = suite.CASEPLAN[1]
        args = suite.argument_parser().parse_args(['--reference-only'])
        check = dict(name=item['name'], kind=item['kind'], status='PASS', phases=[])
        with mock.patch.object(suite, 'validate_phase') as replay:
            for pairs in ([], list(reversed(suite.phases_for(item, True))), suite.phases_for(item, True)+[('unknown', 'reference')]):
                check['phases'] = [dict(probe=p, engine=e) for p, e in pairs]
                with self.assertRaisesRegex(ValueError, 'missing, reordered'):
                    suite.validate_check(check, {}, args, Path('/unused'), None)
            replay.assert_not_called()

    def test_pass_labels_are_not_evidence(self):
        for report in ({}, {'status': 'PASS'}, {'checks': [dict(name=n, kind=k, status='PASS') for n, k in suite.PLAN]}):
            self.assertFalse(suite.report_passes(report))


if __name__ == '__main__':
    unittest.main()
