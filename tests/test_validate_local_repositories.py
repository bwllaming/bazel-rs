"""Harness-only tests; synthetic records are not CLI/Starlark parity evidence."""

import ast
import base64
import copy
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_local_repositories as suite


def record(text='', code=1, **changes):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **changes)


class PlanTests(unittest.TestCase):
    def test_complete_unique_plan_and_real_actions(self):
        self.assertEqual(len(suite.CASEPLAN), 6)
        self.assertEqual(len(set(suite.PLAN)), 6)
        for item in suite.CASEPLAN:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe, engine in suite.phases_for(item):
                value = suite.fixture(item['name'], probe)
                steps = suite.steps_for(value, engine)
                self.assertEqual(sum(cmd == 'build' for cmd, _, _ in steps), 1 if value['diagnostic'] else 2)
                for path, raw in value['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        ast.parse(raw.decode(), filename=path)

    def test_unknown_plan_never_falls_back(self):
        for name, probe in [('invented', 'true'), ('genuine', 'invented')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                suite.fixture(name, probe)

    def test_reference_only_and_frozen_native_gate(self):
        for item in suite.CASEPLAN:
            phases = suite.phases_for(item, True)
            self.assertTrue(all(engine == 'reference' for _, engine in phases))
            if item['kind'] == 'native_boundary':
                self.assertEqual(phases, [])
        with self.assertRaisesRegex(ValueError, 'frozen candidate'):
            suite.common.executable_evidence(suite.argument_parser().parse_args([]))

    def test_archive_original_hash_and_complete_members(self):
        files, directories = suite.archive_plan()
        self.assertEqual(len(files), 41)
        self.assertEqual(len(directories), 7)
        self.assertEqual(suite.digest(suite.ARCHIVE.read_bytes()), suite.ARCHIVE_SHA256)
        self.assertIn(b'local = True', files['features/private/version_repo.bzl'][0])
        self.assertIn(b'local = True', files['features/private/globals_repo.bzl'][0])
        self.assertIn(b'version = "0.0.0"', files['features/MODULE.bazel'][0])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            paths = suite.materialize(root, suite.fixture('genuine', 'full-four-files'))
            self.assertEqual(suite.manifest(root, True), dict(files=sorted(files), directories=directories))
            self.assertEqual(len(paths), len(files)+4)
            for path, (raw, mode) in files.items():
                self.assertEqual((root / path).read_bytes(), raw)
                self.assertEqual((root / path).stat().st_mode & 0o777, mode)

    def test_complete_helpers_not_minimal_globals_claim(self):
        full = suite.fixture('genuine', 'full-four-files')
        minimal = suite.fixture('genuine', 'minimal-globals-functional')
        self.assertEqual(len(full['generated']), 4)
        self.assertEqual(len(full['expected']), 4)
        self.assertIn(b'globals=GLOBALS', full['files']['ext.bzl'])
        self.assertEqual(full['files']['copy.bzl'].count(b'ctx.template('), 4)
        self.assertIn(b'@g//:BUILD.bazel', full['files']['ext.bzl'])
        self.assertIn(b'@v//:BUILD.bazel', full['files']['ext.bzl'])
        self.assertNotIn(b'load("@g//:globals.bzl"', full['files']['BUILD.bazel'])
        self.assertIn(b'globals.DefaultInfo != DefaultInfo', minimal['files']['consumer.bzl'])
        self.assertIn(b'ctx.actions.write', minimal['files']['consumer.bzl'])
        self.assertFalse(minimal['generated'])
        for value in (full, minimal):
            self.assertFalse(any(p.startswith('features/') for p in value['files']))

    def test_actual_version_value_consumed(self):
        value = suite.fixture('genuine', 'version-functional')
        self.assertIn(b'load("@v//:version.bzl","version")', value['files']['BUILD.bazel'])
        self.assertIn(b'"printf %s "+version', value['files']['BUILD.bazel'])
        self.assertEqual(value['expected'], {'result.txt': b'9.3.0'})

    def test_lifecycle_routes_default_and_explicit_flags(self):
        for route in ('direct', 'ordinary'):
            for local in ('default', 'false', 'true'):
                value = suite.fixture('lifecycle', route+'.'+local)
                self.assertEqual(value['callbacks'], 1)
                self.assertEqual(value['repos'], 1)
                self.assertIn(b'@made//:a', value['files']['BUILD.bazel'])
                self.assertIn(b'@made//:b', value['files']['BUILD.bazel'])
                self.assertIn(b'local='+str(local == 'true').encode(), value['files']['defs.bzl']) if local != 'default' else self.assertNotIn(b'local=', value['files']['defs.bzl'])

    def test_shared_transitive_phase_and_module_demand(self):
        value = suite.fixture('phase', 'shared-transitive')
        for p in ('defs.bzl', 'consumer.bzl'):
            self.assertIn(b'load(":transitive.bzl","observed")', value['files'][p])
        self.assertIn(b'INITIAL=native.bazel_version', value['files']['defs.bzl'])
        self.assertIn(b'getattr(native,"bazel_version",None)', value['files']['consumer.bzl'])
        self.assertEqual(value['expected']['result.txt'], b'9.3.0/9.3.0/9.3.0\nphase\n')
        module = suite.fixture('phase', 'module-native-absent')
        self.assertEqual(suite.steps_for(module, 'native')[0][1], '@missing//:result')

    def test_unused_and_demanded_failure_are_distinct(self):
        unused = suite.fixture('laziness', 'unused')
        failed = suite.fixture('laziness', 'failure-after-write')
        self.assertEqual(unused['files']['defs.bzl'], failed['files']['defs.bzl'])
        self.assertEqual(unused['callbacks'], 0)
        self.assertIsNone(unused['diagnostic'])
        self.assertEqual(failed['diagnostic'], 'failure')
        self.assertIn(b'ctx.file', failed['files']['defs.bzl'])

    def test_environment_boundary_is_invalid_shape_not_obsolete_support_denial(self):
        value = suite.fixture('native-boundary', 'environ-shape')
        self.assertIn(b'environ=None', value['files']['defs.bzl'])
        self.assertEqual(value['diagnostic'], 'environ-shape')
        suite.require_rejection(record('repository_rule environ must be a list or tuple of strings'), 'environ-shape')
        for text in ('repository_rule environ is unsupported', 'unknown global repository_rule',
                     'repository_rule configure is unsupported'):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(text), 'environ-shape')


class DiagnosticTests(unittest.TestCase):
    def test_controlled_categories(self):
        for diagnostic, text in dict(local="parameter 'local' got value of type 'int', want 'bool'",
                native="name 'native' is not defined", failure='Error in fail: LOCAL_AFTER_WRITE',
                configure='configure is unsupported', **{'environ-shape': 'environ must be a list or tuple'},
                escape='repository path traversal is forbidden').items():
            suite.require_rejection(record(text), diagnostic)
            for bad in ('panicked', 'network unavailable', 'permission denied', 'internal error', 'failed to download'):
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


class ReplayTests(unittest.TestCase):
    def positive(self, root):
        item, probe, engine = suite.CASEPLAN[0], 'direct.true', 'native'
        value = suite.fixture(item['name'], probe)
        workspace = root / 'workspace'
        workspace.mkdir()
        paths = suite.materialize(workspace, value)
        cache = root / 'cases/lifecycle/direct.true/native-cache'
        cache.mkdir(parents=True)
        output = cache / 'result.txt'
        output.write_bytes(value['expected']['result.txt'])
        output.chmod(0o555)
        args = suite.argument_parser().parse_args([])
        reference = suite.DirectReference(args.reference, workspace, root, None, 'local-repositories')
        records = []
        for i, (command, target, extra) in enumerate(suite.steps_for(value, engine)):
            rec = record('NATIVERUN\n'+('1 actions executed, 0 cache hits' if i == 0 else '0 actions executed, 1 cache hits'), 0)
            rec.update(id=i, name='lifecycle.direct.true.native.'+str(i), cwd=str(workspace), timeout_seconds=args.timeout,
                argv=suite.extensions.command_argv(args, reference, engine, cache, root / 'registry', command, target, extra),
                stdout='//:result\t'+str(output)+'\n')
            records.append(rec)
        inputs = suite.snapshot(paths)
        phase = dict(probe=probe, engine=engine, status='PASS', runs=[0, 1], cache=str(cache),
            inputs_before=inputs, inputs_after=copy.deepcopy(inputs), manifest_before=None, manifest_after=None,
            outputs=[[suite.evidence(output)], [suite.evidence(output)]], generated=[{}, {}],
            trees=[['a'*64], ['a'*64]], publication_entries=['a'*64])
        return phase, dict(runs=records, scratch_directory=str(root)), args, workspace, reference, item

    def test_replay_rejects_missing_callback_and_stale_cache_claims(self):
        with tempfile.TemporaryDirectory() as tmp:
            values = self.positive(Path(tmp))
            suite.validate_phase(*values)
            for mutate in (
                    lambda p, r: r['runs'][1].update(stderr='0 actions executed, 1 cache hits'),
                    lambda p, r: r['runs'][1].update(stderr='NATIVERUN\nNATIVERUN\n0 actions executed, 1 cache hits'),
                    lambda p, r: r['runs'][0].update(stderr='NATIVERUN\n0 actions executed, 1 cache hits'),
                    lambda p, r: p['outputs'][0][0].update(mode=0o755),
                    lambda p, r: p['outputs'][0][0].update(sha256='0'*64),
                    lambda p, r: p['outputs'][0][0].update(path='/invented/result.txt'),
                    lambda p, r: p['trees'][1].append('b'*64),
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

    def test_rehashed_source_is_not_immutable_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            phase, report, *rest = self.positive(Path(tmp))
            path = next(p for p in phase['inputs_before'] if p.endswith('defs.bzl'))
            raw = b'changed but honestly rehashed\n'
            for field in ('inputs_before', 'inputs_after'):
                phase[field][path].update(bytes_base64=base64.b64encode(raw).decode(), sha256=suite.digest(raw), size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError, 'immutable fixture'):
                suite.validate_phase(phase, report, *rest)

    def test_check_requires_all_ordered_phase_pairs(self):
        item = suite.CASEPLAN[0]
        args = suite.argument_parser().parse_args(['--reference-only'])
        check = dict(name=item['name'], kind=item['kind'], status='PASS', phases=[])
        with mock.patch.object(suite, 'validate_phase') as replay:
            for pairs in ([], list(reversed(suite.phases_for(item, True))), suite.phases_for(item, True)+[('unknown', 'reference')]):
                check['phases'] = [dict(probe=p, engine=e) for p, e in pairs]
                with self.assertRaisesRegex(ValueError, 'missing, reordered'):
                    suite.validate_check(check, {}, args, Path('/unused'), None)
            replay.assert_not_called()

    def test_pass_labels_without_evidence_do_not_pass(self):
        for report in ({}, {'status': 'PASS'}, {'checks': [dict(name=n, kind=k, status='PASS') for n, k in suite.PLAN]}):
            self.assertFalse(suite.report_passes(report))


if __name__ == '__main__':
    unittest.main()
