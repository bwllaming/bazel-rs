"""Synthetic guard tests, not evidence that either engine executed."""

import base64
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_root_module as suite
import validate_run_environment_info as metadata

h = suite.harness


def record(text='', code=1, **kwargs):
    return dict(stdout='', stderr=text, returncode=code, timed_out=False, **kwargs)


def streams(r):
    for field in ('stdout', 'stderr'):
        raw = r[field].encode()
        r.update({field+'_base64': base64.b64encode(raw).decode(), field+'_sha256': h.digest(raw),
            field+'_full_sha256': h.digest(raw), field+'_full_size_bytes': len(raw)})


def synthetic_report(root, reference_only=False):
    """Create real fixture/output evidence around an explicitly synthetic runner."""
    args = suite.argument_parser().parse_args(['--reference-only'] if reference_only else ['--native-sha256', 'a'*64])
    workspace = root / 'workspace'
    workspace.mkdir()
    reference = h.DirectReference(args.reference, workspace, root, None, h.SUITE_NAME)
    sources = h.snapshot(h.SOURCES)
    executables = {'reference': {'sha256': h.REFERENCE_SHA256}}
    if not reference_only:
        executables['native'] = {'sha256': 'a'*64}
    official = dict(path=str(args.reference_source), commit=h.SOURCE_PIN, files=h.OFFICIAL_FILES)
    binding = h.digest(json.dumps(dict(executables=executables, source=official), sort_keys=True).encode())
    report = dict(schema_version=1, status='REFERENCE_ONLY_PASS' if reference_only else 'PASS',
        started_at='2026-10-13T00:00:00+00:00', finished_at='2026-10-13T00:01:00+00:00',
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        planned_checks=suite.CASEPLAN, plan=suite.plan_receipt(reference_only), limitations=suite.LIMITATIONS,
        historical_parity_gaps=copy.deepcopy(h.KNOWN_PARITY_GAPS),
        historical_baseline_verification=dict(commit=h.BASELINE_COMMIT, historical_sources=h.BASELINE_SOURCES.copy(), verified_from_git=True),
        errors=[], cleanup=[], scratch_cleaned=True, reference_shutdown_succeeded=True,
        sources_before=sources, sources_after=copy.deepcopy(sources), executables=executables, executables_after=copy.deepcopy(executables),
        official_source_before=official, official_source_after=copy.deepcopy(official), reference_release='bazel 9.3.0',
        scratch_directory=str(root), cleanup_receipt=dict(path=str(root), removed=True),
        server_receipt=dict(pidfile=str(reference.output_base / 'server/server.pid.txt'), pid=4321,
            proc_exists_after=False, remaining_pidfiles=[]), runs=[], checks=[])
    registry = {str(root / 'registry' / Path(p).relative_to(h.REGISTRY)): dict(e,
        path=str(root / 'registry' / Path(p).relative_to(h.REGISTRY)))
        for p, e in sources.items() if Path(p).is_relative_to(h.REGISTRY)}
    report.update(registry_before=registry, registry_after=copy.deepcopy(registry))

    def append(name, argv, cwd, stdout='', stderr='', code=0, timeout=args.timeout):
        engine = 'reference' if argv[0] == str(args.reference) else 'native'
        r = record(stderr, code, id=len(report['runs']), name=name, argv=argv, cwd=str(cwd), timeout_seconds=timeout,
            executable_sha256=executables[engine]['sha256'], preflight_binding_sha256=binding, synthetic_environment=h.HOST.copy())
        r['stdout'] = stdout
        streams(r)
        report['runs'].append(r)
        return r

    append('reference.version', reference.startup+['version', '--gnu_format'], workspace, stdout='bazel 9.3.0\n')
    diagnostics = {
        'runenv': 'error: Variable `RunEnvironmentInfo` not found',
        'unknown': 'error: Variable `UnknownRootSymbol` not found',
        'syntax': 'error: Parse error: unexpected new line, expected expression',
        'division': 'error: integer division by zero',
        'invalid-utf8': 'error: MODULE.bazel: stream did not contain valid UTF-8',
        'directory': 'error: MODULE.bazel: Is a directory',
    }
    for item in suite.CASEPLAN:
        check = dict(name=item['name'], kind=item['kind'], phases=[], status='PASS')
        report['checks'].append(check)
        for probe, engine in h.phases_for(item, reference_only):
            value = suite.fixture(item['name'], probe)
            steps = h.steps_for(value, engine)
            cache = root / 'cases' / item['name'] / probe / 'native-cache'

            def run(name, argv, cwd):
                i = int(name.rsplit('.', 1)[1])
                command = steps[i][0]
                if value['diagnostic']:
                    return append(name, argv, cwd, stderr=diagnostics[value['diagnostic']], code=value['returncodes'][engine])
                stderr = '\n'.join(value['markers'])+'\n'
                if value['root_print']:
                    stderr += suite.ROOT_ONCE+'\n'
                stdout = '//app:check\n' if command == 'query' else ''
                if command == 'build':
                    stderr += suite.CALLBACK+'\n'
                    if engine == 'native':
                        build_index = sum(step[0] == 'build' for step in steps[:i])
                        stderr += '1 actions executed, 0 cache hits' if build_index == 0 else '0 actions executed, 1 cache hits'
                if command == ('build' if engine == 'native' else 'cquery'):
                    output = (cache / 'actions' / ('a'*64) / 'outputs/check.out' if engine == 'native' else
                        reference.output_base / 'execroot/_main/bazel-out/synthetic/bin/app/check.out')
                    output.parent.mkdir(parents=True, exist_ok=True)
                    if output.exists():
                        output.chmod(0o644)
                    output.write_bytes(b'g-ok\n')
                    output.chmod(0o555)
                    stdout = '//app:check\t'+str(output)+'\n' if engine == 'native' else 'bazel-out/synthetic/bin/app/check.out\n'
                return append(name, argv, cwd, stdout, stderr)

            check['phases'].append(h.run_phase(args, mock.Mock(run=run), root, workspace, reference, item, probe, engine))
        if not check['phases']:
            check['status'] = 'SKIP'
    append(h.SUITE_NAME+'.shutdown', reference.startup+['shutdown']+list(h.COMMON_FLAGS), workspace, timeout=min(args.timeout, 15))
    report.update(completed_checks=sum(c['status'] == 'PASS' for c in report['checks']),
        completed_phases=sum(p['status'] == 'PASS' for c in report['checks'] for p in c['phases']))
    report['gap_closure'] = h.closure_receipt(report, reference_only)
    return report


class PlanTests(unittest.TestCase):
    def test_exact_plan_and_runner_isolation(self):
        self.assertEqual(suite.plan_receipt()['families'], 4)
        self.assertEqual(suite.plan_receipt()['probes'], 18)
        self.assertEqual(suite.plan_receipt()['phases'], 29)
        self.assertEqual(suite.plan_receipt(True)['phases'], 11)
        self.assertEqual(metadata.SUITE_NAME, 'run-environment-info')
        # F's own plan (rule-kind differential added); G replaces it privately.
        self.assertEqual(len(metadata.CASEPLAN), 8)
        self.assertNotIn('rule-kind', [c['name'] for c in h.CASEPLAN])
        self.assertIsNot(h, metadata)
        # Native BUILD does not predeclare print. A loaded ordinary function
        # records BUILD entry without adding an unsupported fixture global.
        for item in suite.CASEPLAN:
            for probe in item['probes']:
                value = suite.fixture(item['name'], probe)
                self.assertNotIn(b'print(', value['files']['app/BUILD.bazel'])
                self.assertIn(b'build_marker()\n', value['files']['app/BUILD.bazel'])
                self.assertIn(b'def build_marker():\n    print("G_BUILD_LOAD")', value['files']['defs/check.bzl'])

    def test_original_module_bytes_target_and_both_commands(self):
        gap = h.KNOWN_PARITY_GAPS[0]
        for probe, command in [('module-global-local', 'query'), ('runenv-build', 'build')]:
            value = suite.fixture('module', probe)
            self.assertEqual(h.digest(value['files']['MODULE.bazel']), gap['module']['sha256'])
            self.assertEqual(h.steps_for(value, 'native'), [(command, '//app:check', [])])
            self.assertEqual(value['returncodes'], {'native': 1, 'reference': 48})
            self.assertIn(b'rejected.marker', value['files']['defs/check.bzl'])

    def test_invalid_cases_are_full_evaluator_not_symbol_scan(self):
        for diagnostic in ('unknown', 'syntax', 'division'):
            for command in ('query', 'build'):
                value = suite.fixture('module', diagnostic+'-'+command)
                self.assertEqual(value['diagnostic'], diagnostic)
                self.assertNotIn(b'RunEnvironmentInfo', value['files']['MODULE.bazel'])
                self.assertEqual(value['failure_command'], command)
        self.assertIn(b'1//0', suite.fixture('module', 'division-query')['files']['MODULE.bazel'])

    def test_native_boundaries_never_reference(self):
        for item in suite.CASEPLAN[2:]:
            self.assertEqual(h.phases_for(item, True), [])
            self.assertTrue(all(engine == 'native' for _, engine in h.phases_for(item)))
        self.assertNotIn('MODULE.bazel', suite.fixture('filesystem-native', 'workspace')['files'])
        self.assertIn('MODULE.bazel/.qa-directory', suite.fixture('filesystem-native', 'directory')['files'])
        self.assertEqual(suite.fixture('filesystem-native', 'invalid-utf8')['files']['MODULE.bazel'], b'\xff\xfe\n')

    def test_external_graph_demand_uses_local_override_and_root_print(self):
        value = suite.fixture('valid', 'external-print')
        self.assertTrue(value['root_print'])
        self.assertIn(b'local_path_override', value['files']['MODULE.bazel'])
        self.assertIn(b'load("@dep//:defs.bzl"', value['files']['defs/check.bzl'])
        self.assertEqual(value['expected'], {'check.out': b'g-ok\n'})
        self.assertEqual(value['queries'], [('//app:check', ['//app:check'])])

    def test_unused_definition_poison_is_not_root_poison(self):
        for probe in suite.CASEPLAN[2]['probes']:
            value = suite.fixture('lazy-native', probe)
            self.assertIn(b'G_POISON_DEFINITION_LOADED', value['files']['defs/poison.bzl'])
            self.assertIsNone(value['diagnostic'])
            self.assertNotIn(b'G_POISON', value['files']['MODULE.bazel'])


class DiagnosticTests(unittest.TestCase):
    def test_exact_error_class_and_no_unrelated_failure(self):
        message = 'error: Variable `RunEnvironmentInfo` not found'
        suite.require_rejection(record(message), 'runenv', 1)
        for text in ('source: '+message, '    '+message, 'error: Variable `OtherGlobal` not found',
                message+'\nERROR: network unavailable', message+'\nerror: unknown repository @missing',
                message+'\n'+suite.BUILD_LOAD, message+'\n'+suite.BZL_LOAD, message+'\n'+suite.CALLBACK):
            with self.subTest(text=text), self.assertRaises(ValueError):
                suite.require_rejection(record(text), 'runenv', 1)
        for code in (0, True, -9, 7, 48, 101, 124):
            with self.subTest(code=code), self.assertRaises(ValueError):
                suite.require_rejection(record(message, code), 'runenv', 1)
        for key in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'):
            bad = record(message)
            bad[key] = True
            with self.assertRaises(ValueError):
                suite.require_rejection(bad, 'runenv', 1)

    def test_root_print_once_native_but_reference_warm_cache_permitted(self):
        value = suite.fixture('valid', 'root-print')
        text = suite.BUILD_LOAD+'\n'+suite.BZL_LOAD+'\n'+suite.ROOT_ONCE+'\n'
        suite.require_markers(record(text, 0), value, 'query', True, 'native', False)
        suite.require_markers(record('', 0), value, 'cquery', False, 'reference', False)
        for changed in (text+suite.ROOT_ONCE+'\n', text.replace(suite.ROOT_ONCE, ''), text+suite.CALLBACK+'\n'):
            with self.assertRaises(ValueError):
                suite.require_markers(record(changed, 0), value, 'query', True, 'native', False)


class ReplayTests(unittest.TestCase):
    def test_full_synthetic_report_and_closure_mutations(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            report = synthetic_report(Path(tmp))
            self.assertTrue(suite.report_passes(report), [(c['name'], p.get('error')) for c in report['checks'] for p in c['phases'] if p['status'] != 'PASS'])
            closure = report['gap_closure']
            self.assertTrue(closure['native_closure_verified'])
            self.assertEqual(closure['status'], 'RESOLVED_BY_ROOT_RECORDING_VALIDATION')
            native_id = closure['runs']['native'][0]
            mutations = [
                lambda r: r['gap_closure'].update(native_closure_verified=False),
                lambda r: r['gap_closure'].update(native_closure_verified=1),
                lambda r: r['gap_closure']['runs'].update(native=[0]),
                lambda r: r['checks'][0]['phases'].pop(1),
                lambda r: r['checks'][0]['phases'][1].update(status='FAIL'),
                lambda r: r['checks'][0]['phases'][1]['inputs_before'].clear(),
                lambda r: r['checks'][0]['phases'][1]['action_store_after']['actions']['entries'].append('premature'),
                lambda r: r['checks'][0]['phases'][1]['rejection_artifacts_after'].append('rejected.marker'),
                lambda r: r['checks'][0]['phases'][1].update(error='hidden'),
                lambda r: r['runs'][native_id].update(returncode=0),
                lambda r: r['runs'][native_id].update(returncode=True),
                lambda r: r['runs'][native_id]['argv'].__setitem__(-1, '@missing//:all'),
                lambda r: r['runs'][native_id].update(stderr='error: Variable `WrongGlobal` not found'),
                lambda r: r['runs'][native_id].update(stderr=r['runs'][native_id]['stderr']+'\n'+suite.CALLBACK),
                lambda r: r['historical_parity_gaps'][0].update(original_matrix_status='PASS'),
                lambda r: r['historical_parity_gaps'][0]['observations']['native'].update(returncode=1),
                lambda r: r['historical_baseline_verification']['historical_sources'].clear(),
                lambda r: r['sources_after'].clear(), lambda r: r['registry_after'].clear(),
                lambda r: r['executables_after']['native'].update(sha256='b'*64),
                lambda r: r['configuration'].update(native_sha256='b'*64),
                lambda r: r['runs'][native_id].update(preflight_binding_sha256='0'*64),
                lambda r: r['server_receipt'].update(proc_exists_after=True),
                lambda r: r['cleanup_receipt'].update(removed=False),
                lambda r: r.update(finished_at='2026-10-12T00:00:00+00:00'),
                lambda r: r['plan'].update(commands=0), lambda r: r.update(completed_phases=0),
            ]
            for i, mutate in enumerate(mutations):
                changed = copy.deepcopy(report)
                mutate(changed)
                # Rehash altered streams: semantic checks must reject them independently.
                streams(changed['runs'][native_id])
                with self.subTest(mutation=i):
                    self.assertFalse(suite.report_passes(changed))

    def test_reference_only_never_claims_native_closure(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as tmp:
            report = synthetic_report(Path(tmp), True)
            self.assertTrue(suite.report_passes(report))
            self.assertFalse(report['gap_closure']['native_closure_verified'])
            self.assertEqual(report['gap_closure']['status'], 'REFERENCE_ONLY_EXPECTED_CLOSURE')
            self.assertEqual(set(report['executables']), {'reference'})
            self.assertTrue(all(p['engine'] == 'reference' for c in report['checks'] for p in c['phases']))
            report['gap_closure']['native_closure_verified'] = True
            self.assertFalse(suite.report_passes(report))

    def test_portable_history_accepts_intentional_current_source_change(self):
        sources = h.snapshot(h.SOURCES)
        path = str(h.ROOT / 'src/main.rs')
        raw = base64.b64decode(sources[path]['bytes_base64'])+b'\n// synthetic current G change\n'
        sources[path].update(bytes_base64=base64.b64encode(raw).decode(), size_bytes=len(raw), sha256=h.digest(raw))
        forbidden = h.ROOT / h.KNOWN_PARITY_GAPS[0]['retained_receipt']['path']
        actual = Path.open

        def guarded(path, *args, **kwargs):
            if path.resolve() == forbidden:
                raise AssertionError('ignored original receipt read')
            return actual(path, *args, **kwargs)

        with mock.patch.object(Path, 'open', guarded), mock.patch.object(h.subprocess, 'run') as git:
            h.validate_known_gap_sources(sources)
            git.assert_not_called()

    def test_historical_git_verification_is_not_current_source_identity(self):
        sources = h.snapshot(h.SOURCES)
        # Unit test only: no live git or ignored artifact dependency.
        with mock.patch.object(h.subprocess, 'run', return_value=mock.Mock(stdout=b'wrong historical source')):
            with self.assertRaisesRegex(ValueError, 'historical git blob mismatch'):
                h.validate_known_gap_sources(sources, check_baseline=True)


if __name__ == '__main__':
    unittest.main()
