"""Harness guards only. Never launches native/Bazel, compiles or downloads."""

import ast
import base64
import copy
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_build_settings as settings


def record(stdout='', stderr='', returncode=0, **changes):
    result = dict(id=0, name='synthetic-not-CLI-evidence', stdout=stdout, stderr=stderr,
                  returncode=returncode, timed_out=False)
    result.update(changes)
    return result


MESSAGES = {
    'default': 'build_setting_default: expected bool, got int 2',
    'injected': 'build_setting_default is a reserved implicit attribute',
    'cfg': 'build_setting rules cannot use cfg transitions',
    'descriptor': 'build_setting expected a BuildSetting descriptor',
    'repeatable': 'repeatable requires flag = True',
    'allowed': "Error setting //:flag: invalid value 'green'. Allowed values are [\"red\", \"blue\"]",
    'make-variable': "Error setting //:flag: invalid make variable name 'bad-name'",
    'template-argument': 'TemplateVariableInfo vars: expected dict of strings',
    'duplicate-provider': 'duplicate provider TemplateVariableInfo',
    'return': 'rule returned a value that is not a provider',
    'ordinary': 'ctx has no attribute build_setting_value',
    'option': 'unsupported option --//:string_flag=blue',
    'unsupported': 'config.string_set is unsupported',
}


def synthetic_report(scratch):
    """Complete deliberately synthetic verifier input; never written as QA evidence."""
    report = dict(planned_checks=copy.deepcopy(settings.CASES), case_inputs_sha256=settings.json_digest(settings.CASES),
                  checks=[], runs=[], errors=[], cleanup=[], scratch_cleaned=True, scratch_directory=str(scratch),
                  cleanup_receipt=dict(path=str(scratch), removed=True), sources_unchanged=True,
                  sources_before={'synthetic': 'unit only'}, sources_after={'synthetic': 'unit only'},
                  completed_checks=9, completed_differential_cases=8, reference_release='bazel 9.3.0',
                  reference_shutdown_succeeded=True, executables_unchanged=True,
                  configuration={'native': '/synthetic/native', 'reference': '/synthetic/reference'},
                  fixture=settings.load_inputs(),
                  superseded={'/'.join(key): reason for key, reason in settings.SUPERSEDED.items()})
    report['executables'] = {name: dict(sha256=settings.REFERENCE_SHA256 if name == 'reference' else 'a' * 64)
                             for name in ('native', 'reference')}
    report['executables_after'] = {name: dict(value, path=report['configuration'][name]) for name, value in report['executables'].items()}
    ref = settings.reference_for(report)

    def run(name, argv, stdout='', stderr='', returncode=0):
        value = record(stdout, stderr, returncode, id=len(report['runs']), name=name, argv=argv, cwd=str(ref.workspace))
        for field in ('stdout', 'stderr'):
            raw = value[field].encode()
            value.update({field + '_base64': base64.b64encode(raw).decode(), field + '_sha256': settings.digest(raw),
                          field + '_full_sha256': settings.digest(raw), field + '_full_size_bytes': len(raw)})
        report['runs'].append(value)
        return value['id']

    run('reference.version', ref.startup + ['version', '--gnu_format'], 'bazel 9.3.0\n')
    for item in settings.CASES:
        check = dict(name=item['name'], kind=item['kind'], input=item, status='PASS', parity_claim=item['kind'] == 'differential', phases=[])
        report['checks'].append(check)
        for probe, engine in settings.phases_for(item):
            if (item['name'], probe) in settings.SUPERSEDED:
                check['phases'].append(dict(probe=probe, engine=engine, status='SUPERSEDED',
                                            reason=settings.SUPERSEDED_REASON, runs=[]))
                continue
            spec = settings.specification(item, probe)
            frozen = settings.snapshot(settings.prepare(ref.workspace, spec))
            cache = scratch / 'cases' / item['name'] / ('cache' if item['name'] in ('genuine.nine-defaults', 'cache.default-change') else probe + '-cache')
            phase = dict(probe=probe, engine=engine, status='PASS', inputs_before=frozen, inputs_after=frozen,
                         cache=str(cache), runs=[], artifacts=[])
            check['phases'].append(phase)
            relative = 'bazel-out/k8-fastbuild/bin/result.txt'
            output = cache / 'result.txt' if engine == 'native' else ref.output_base / 'execroot/_main' / relative
            for index, (command, extra) in enumerate(settings.steps_for(spec, engine)):
                argv = (settings.native_command(Path(report['configuration']['native']), cache, command, spec['target'], extra)
                        if engine == 'native' else ref.startup + [command] + list(settings.COMMON_FLAGS)
                        + (list(settings.BUILD_FLAGS) if command in ('build', 'cquery') else [])
                        + ['--repository_cache=' + str(scratch / 'repository-cache'), '--repo_contents_cache=' + str(scratch / 'repository-contents'),
                           '--registry=' + settings.REGISTRY.as_uri(), '--registry=https://bcr.bazel.build'] + extra + [spec['target']])
                stdout, stderr, code = '', '', 0
                if spec['diagnostic'] and index == len(settings.steps_for(spec, engine)) - 1:
                    code, stderr = 1, MESSAGES[spec['diagnostic']]
                    phase.update(diagnostic=spec['diagnostic'], outputs_before=[], outputs_after=[])
                elif command == 'query':
                    stdout = spec['target'] + '\n'
                elif command == 'build' and engine == 'native':
                    stdout = spec['target'] + '\t' + str(output) + '\n' if spec['expected'] is not None else ''
                    stderr = '%d actions executed, %d cache hits\n' % spec['counts']
                    phase['counts'] = dict(zip(('actions_executed', 'cache_hits'), spec['counts']))
                elif command == 'cquery':
                    stdout = relative + '\n' if spec['expected'] is not None else ''
                phase['runs'].append(run('.'.join([item['name'], probe, engine, str(index)]), argv, stdout, stderr, code))
            if not spec['diagnostic'] and spec['expected'] is not None:
                raw = spec['expected']
                phase['artifacts'].append(dict(path=str(output), logical_path=str(output), kind='file', mode=0o555,
                                               bytes_base64=base64.b64encode(raw).decode(), sha256=settings.digest(raw), size_bytes=len(raw)))
    run('build-settings.shutdown', ref.startup + ['shutdown'] + list(settings.COMMON_FLAGS))
    return report


class PlanTests(unittest.TestCase):
    def test_exact_nine_families_eight_genuine_one_boundary(self):
        self.assertEqual(len(settings.CASES), 9)
        self.assertEqual(len(set(name for name, _ in settings.PLAN)), 9)
        self.assertEqual([c['kind'] for c in settings.CASES], ['differential'] * 8 + ['native_boundary'])
        self.assertEqual(sum(len(c['probes']) for c in settings.CASES), 55)
        self.assertEqual(sum(len(settings.phases_for(c)) for c in settings.CASES), 104)
        for item in settings.CASES:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe, engine in settings.phases_for(item):
                spec = settings.specification(item, probe)
                self.assertTrue(settings.steps_for(spec, engine))
                self.assertFalse(any(command in ('test', 'run', 'mod') for command, _ in settings.steps_for(spec, engine)))
                if item['kind'] == 'native_boundary':
                    self.assertEqual(engine, 'native')

    def test_generated_sources_are_lexically_well_formed_not_fake_evaluation(self):
        for item in settings.CASES:
            for probe in item['probes']:
                for path, text in settings.specification(item, probe)['files'].items():
                    with self.subTest(family=item['name'], probe=probe, path=path):
                        ast.parse(text)
                        self.assertNotIn('providers = [', text)
                        self.assertNotIn('select(', text)
                        self.assertNotIn('toolchains =', text)

    def test_all_nine_rules_and_single_return_repeatable_are_genuine(self):
        spec = settings.specification(settings.CASES[0], 'cold')
        build = spec['files']['BUILD.bazel']
        for name in settings.NINE:
            self.assertIn(name + '(name = "' + name + '", build_setting_default = ', build)
        self.assertIn('load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")', spec['files']['consumer.bzl'])
        self.assertNotIn('BuildSettingInfo =', spec['files']['consumer.bzl'])
        self.assertEqual(spec['expected'], settings.NINE_BYTES)
        self.assertIn(b'string_list_flag=a||b\n', spec['expected'])
        for name in settings.NINE:
            pure = settings.specification(settings.CASES[0], name)
            self.assertIsNone(pure['expected'])
            self.assertEqual(pure['counts'], (0, 0))

    def test_root_uses_absolute_genuine_tree_without_replacing_upstream_metadata(self):
        for item in settings.CASES:
            for probe in item['probes']:
                module = settings.specification(item, probe)['files']['MODULE.bazel']
                self.assertIn('bazel_dep(name = "bazel_skylib", version = "1.8.2")', module)
                self.assertIn(repr(str(settings.FIXTURE / 'skylib')), module)
                # The checkout path itself may contain these words.
                declarations = module.replace(repr(str(settings.FIXTURE / 'skylib')), '')
                self.assertNotIn('platforms', declarations)
                self.assertNotIn('rules_license', declarations)
        upstream = (settings.FIXTURE / 'skylib/MODULE.bazel').read_text()
        self.assertIn('platforms', upstream)
        self.assertIn('rules_license', upstream)

    def test_invalid_allowed_values_query_succeeds_before_build_rejection(self):
        for item in settings.CASES[2:4]:
            spec = settings.specification(item, 'invalid')
            for engine in ('reference', 'native'):
                self.assertEqual(settings.steps_for(spec, engine), [('query', []), ('build', [])])
            self.assertIsNotNone(spec['diagnostic'])

    def test_cache_plan_is_red_blue_blue_red_not_counters_only(self):
        item = settings.CASES[7]
        specs = [settings.specification(item, p) for p in item['probes']]
        self.assertEqual([s['expected'] for s in specs], [b'color=red\n', b'color=blue\n', b'color=blue\n', b'color=red\n'])
        self.assertEqual([s['counts'] for s in specs], [(1, 0), (1, 0), (0, 1), (0, 1)])
        self.assertEqual(specs[0]['files'], specs[3]['files'])
        self.assertEqual(specs[1]['files'], specs[2]['files'])
        self.assertEqual([p for p in specs[0]['files'] if specs[0]['files'][p] != specs[1]['files'][p]], ['BUILD.bazel'])

    def test_template_order_copy_identity_and_nested_payload_are_explicit(self):
        spec = settings.specification(settings.CASES[4], 'identity-copy-order')
        self.assertEqual(spec['expected'], b'False;2;z|a;last;mutated\n')
        self.assertIn('keys = {one: "one", two: "two"}', spec['files']['defs.bzl'])
        self.assertIn('vars = {"z": "last", "a": "first"}', spec['files']['defs.bzl'])
        self.assertNotIn('sorted(', spec['files']['defs.bzl'])
        self.assertEqual(spec['files']['alias.bzl'], 'Template = platform_common.TemplateVariableInfo\n')
        nested = settings.specification(settings.CASES[5], 'nested')
        self.assertEqual(nested['expected'], b'red;False\n')
        self.assertIn('[Info(nested = info)]', nested['files']['defs.bzl'])

    def test_mutability_fixture_does_not_claim_frozen_getter_or_attr(self):
        spec = settings.specification(settings.CASES[6], 'getter-and-snapshot')
        text = spec['files']['defs.bzl']
        self.assertIn('first.append("mutated")', text)
        self.assertIn('second = ctx.build_setting_value', text)
        self.assertIn('values.append("late")', text)
        self.assertNotIn('ctx.attr.build_setting_default.append', text)
        self.assertEqual(spec['expected'], b'seed=seed|mutated;seed;seed\nother=other|mutated;other;other\nlate=seed;seed\n')

    def test_valid_upstream_legacy_and_extended_constructors_are_native_boundaries_only(self):
        item = settings.CASES[-1]
        self.assertEqual(item['kind'], 'native_boundary')
        for probe in ('legacy-true', 'allow-multiple', 'string-set'):
            self.assertNotIn(probe, [p for c in settings.CASES[:-1] for p in c['probes']])
            self.assertEqual(settings.specification(item, probe)['diagnostic'], 'unsupported')
        self.assertEqual([settings.specification(item, p)['extra'] for p in item['probes'][:3]], [
            ['--//:string_flag=blue'], ['--//:bool_flag'], ['--no//:bool_flag']])

    def test_defaults_output_sources_and_remote_policy(self):
        args = settings.argument_parser().parse_args([])
        self.assertEqual(args.output, settings.ROOT / 'artifacts/build-settings-validation.json')
        self.assertEqual(args.scratch_parent, Path('/tmp/opencode'))
        self.assertEqual(args.timeout, 180)
        argv = settings.native_command(Path('/native'), Path('/cache'), 'build', '//:result')
        self.assertIn('--experimental-builtin-modules', argv)
        self.assertIn('--registry=' + settings.REGISTRY.as_uri(), argv)
        # Explicit: no MODULE.bazel.lock I/O (docs/REGISTRY_LOCKFILE.md).
        self.assertIn('--lockfile_mode=off', argv)
        sources = settings.source_paths()
        for path in (settings.FIXTURE / 'bazel-skylib-1.8.2.tar.gz', settings.FIXTURE / 'LICENSE',
                     settings.FIXTURE / 'skylib/rules/common_settings.bzl', settings.ROOT / 'src/main.rs'):
            self.assertIn(path, sources)

    def test_direct_reference_reuses_genuine_closure_caches_and_no_native_selector(self):
        runner = mock.Mock()
        ref = settings.DirectReference(Path('/official'), Path('/workspace'), Path('/scratch'), runner, 'build-settings')
        ref.registry = settings.REGISTRY
        ref.run('q', 'query', '//:result')
        argv = runner.run.call_args.args[1]
        self.assertNotIn('--experimental-builtin-modules', argv)
        self.assertEqual([a for a in argv if a.startswith('--registry=')], [
            '--registry=' + settings.REGISTRY.as_uri(), '--registry=https://bcr.bazel.build'])
        self.assertIn('--repository_cache=/scratch/repository-cache', argv)


class FixtureTests(unittest.TestCase):
    def test_whole_archive_exact_tree_license_and_pins(self):
        receipt = settings.load_inputs()
        self.assertEqual(receipt['archive_entry_count'], 63)
        self.assertEqual(receipt['file_count'], 56)
        self.assertEqual(receipt['source_url'], settings.SOURCE_URL)
        self.assertEqual(receipt['archive_sha256'], settings.ARCHIVE_SHA256)
        self.assertEqual(receipt['files']['rules/common_settings.bzl']['sha256'], settings.COMMON_SHA256)
        for name in ('BUILD', 'MODULE.bazel', 'LICENSE', 'WORKSPACE'):
            self.assertIn(name, receipt['files'])
        self.assertTrue(all(f['mode'] == 0o644 for f in receipt['files'].values()))

    def test_fixture_corruption_and_extra_entries_cannot_hide_in_provenance(self):
        for mutation in ('archive', 'source', 'mode', 'extra', 'missing', 'symlink', 'license', 'receipt'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
                fixture = Path(directory) / 'fixture'
                shutil.copytree(settings.FIXTURE, fixture)
                path = fixture / 'skylib/rules/common_settings.bzl'
                if mutation == 'archive':
                    (fixture / 'bazel-skylib-1.8.2.tar.gz').write_bytes(b'not genuine')
                elif mutation == 'source':
                    path.write_text('BuildSettingInfo = provider()\n')
                elif mutation == 'mode':
                    path.chmod(0o755)
                elif mutation == 'extra':
                    (fixture / 'skylib/extra').mkdir()
                elif mutation == 'missing':
                    path.unlink()
                elif mutation == 'symlink':
                    path.unlink()
                    path.symlink_to(settings.FIXTURE / 'skylib/rules/common_settings.bzl')
                elif mutation == 'license':
                    (fixture / 'LICENSE').write_text('wrong')
                else:
                    receipt = json.loads((fixture / 'provenance.json').read_text())
                    receipt['source_url'] = 'https://example.invalid/fabricated.tar.gz'
                    (fixture / 'provenance.json').write_text(json.dumps(receipt))
                with self.assertRaises((ValueError, OSError)):
                    settings.load_inputs(fixture)

    def test_prepare_creates_only_inputs_never_golden_output(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            workspace = Path(directory) / 'workspace'
            spec = settings.specification(settings.CASES[0], 'cold')
            paths = settings.prepare(workspace, spec)
            self.assertEqual({p.relative_to(workspace).as_posix() for p in paths}, set(spec['files']))
            self.assertFalse((workspace / 'result.txt').exists())
            for relative, text in spec['files'].items():
                self.assertEqual((workspace / relative).read_bytes(), text.encode())


class ClassifierTests(unittest.TestCase):
    def test_actual_reference_and_native_return_and_unavailable_constructor_forms(self):
        for message in ('at index 0 of result of rule implementation function, got element of type Provider, want Info',
                        'bazel-rs: analysis of //:result: expected list or tuple, got struct'):
            settings.require_rejection(record(stderr=message, returncode=1), 'return')
        settings.require_rejection(record(stderr='Object of type `struct` has no attribute `string_set`', returncode=1), 'unsupported')
        for diagnostic, wrong in (('return', 'expected list or tuple, got struct'),
                                  ('unsupported', 'Object of type `struct` has no attribute `unrelated`')):
            with self.assertRaises(ValueError):
                settings.require_rejection(record(stderr=wrong, returncode=1), diagnostic)

    def test_semantic_categories_require_matching_diagnostics(self):
        for diagnostic, message in MESSAGES.items():
            with self.subTest(diagnostic=diagnostic):
                settings.require_rejection(record(stderr=message, returncode=1), diagnostic)
                with self.assertRaises(ValueError):
                    settings.require_rejection(record(stderr='generic failure', returncode=1), diagnostic)
        with self.assertRaises(ValueError):
            settings.require_rejection(record(stderr=MESSAGES['make-variable'], returncode=1), 'allowed')

    def test_infrastructure_and_noncontrolled_process_failures_never_count(self):
        for diagnostic, message in MESSAGES.items():
            for mutation in ({'returncode': 0}, {'returncode': None}, {'returncode': True}, {'returncode': 101},
                             {'returncode': -9}, {'returncode': 139}, {'timed_out': True}, {'interrupted': True},
                             {'spawn_error': 'missing ELF'}, {'output_truncated': True}, {'stdout': '//:result'},
                             *({'stderr': message + '\n' + text} for text in (
                                 'download failed', 'network unavailable', 'connection refused', 'SSLHandshakeException',
                                 'permission denied', 'access denied', 'panicked at x', 'panic', 'internal error',
                                 'FATAL:', 'timeout', 'timed out', 'syntax error', 'parse error', 'unexpected token',
                                 'unknown apparent repository @missing', 'no such package', 'cannot load defs.bzl',
                                 'module rules_license@1.0.0 not found', 'falling back to upstream'))):
                with self.subTest(diagnostic=diagnostic, mutation=mutation), self.assertRaises(ValueError):
                    settings.require_rejection(record(**dict(dict(stderr=message, returncode=1), **mutation)), diagnostic)

    def test_action_stats_require_exact_single_real_summary(self):
        self.assertEqual(settings.stats(record(stderr='1 actions executed, 0 cache hits\n'), (1, 0)),
                         {'actions_executed': 1, 'cache_hits': 0})
        for text in ('', '0 actions executed, 1 cache hits', '1 actions executed, 0 cache hits\n1 actions executed, 0 cache hits'):
            with self.assertRaises(ValueError):
                settings.stats(record(stderr=text), (1, 0))


class ArtifactTests(unittest.TestCase):
    def test_provider_only_empty_outputs_are_valid_but_missing_action_output_is_not(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            ref = mock.Mock()
            pure = settings.specification(settings.CASES[0], 'bool_flag')
            self.assertEqual(settings.output_paths(record(), pure, 'native', ref, scratch), [])
            with self.assertRaises(ValueError):
                settings.output_paths(record(), settings.specification(settings.CASES[0], 'cold'), 'native', ref, scratch)

    def test_native_output_labels_containment_and_duplicates_are_not_normalized(self):
        with tempfile.TemporaryDirectory(dir=Path(tempfile.gettempdir()).resolve()) as directory:
            scratch = Path(directory)
            path = scratch / 'result.txt'
            path.write_bytes(settings.NINE_BYTES)
            spec = settings.specification(settings.CASES[0], 'cold')
            good = '//:result\t' + str(path) + '\n'
            self.assertEqual(settings.output_paths(record(good), spec, 'native', None, scratch), [path])
            for text in ('@@//:result\t' + str(path), '//:result\t/outside', '//:result\trelative', good + good,
                         '//:result\t' + str(scratch / '../escape'), 'garbage'):
                with self.subTest(text=text), self.assertRaises(ValueError):
                    settings.output_paths(record(text), spec, 'native', None, scratch)

    def test_action_outputs_require_executable_bits_even_when_bytes_match(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            path = Path(directory) / 'output'
            path.write_bytes(b'red\n')
            path.chmod(0o555)
            self.assertTrue(settings.valid_evidence(settings.evidence(path), b'red\n', True))
            path.chmod(0o444)
            self.assertFalse(settings.valid_evidence(settings.evidence(path), b'red\n', True))


class ReportTests(unittest.TestCase):
    def test_cli_option_probes_are_retired_by_n16b_not_false_failures(self):
        self.assertEqual(sorted(settings.SUPERSEDED), [('native.unsupported', p) for p in
                                                       ('bool-option', 'negative-bool-option', 'string-option')])
        self.assertIn('superseded by N16b', settings.SUPERSEDED_REASON)
        with tempfile.TemporaryDirectory(dir=Path(tempfile.gettempdir()).resolve()) as directory:
            report = synthetic_report(Path(directory))
            self.assertTrue(settings.report_passes(report))
            boundary = report['checks'][-1]
            retired = boundary['phases'][0]
            self.assertEqual((retired['probe'], retired['status']), ('string-option', 'SUPERSEDED'))
            for mutation in ('executed', 'reason', 'status', 'evidence', 'report-reason', 'other-probe'):
                changed = copy.deepcopy(report)
                phases = changed['checks'][-1]['phases']
                if mutation == 'executed':
                    phases[0]['runs'] = [0]
                elif mutation == 'reason':
                    phases[0]['reason'] = 'skipped'
                elif mutation == 'status':
                    phases[0]['status'] = 'PASS'
                elif mutation == 'evidence':
                    phases[0]['diagnostic'] = 'option'
                elif mutation == 'report-reason':
                    del changed['superseded']
                else:
                    phases[3] = dict(probe=phases[3]['probe'], engine='native', status='SUPERSEDED',
                                     reason=settings.SUPERSEDED_REASON, runs=[])
                with self.subTest(mutation=mutation):
                    self.assertFalse(settings.report_passes(changed))

    def test_complete_synthetic_evidence_replays_and_tampering_fails(self):
        with tempfile.TemporaryDirectory(dir=Path(tempfile.gettempdir()).resolve()) as directory:
            report = synthetic_report(Path(directory))
            self.assertTrue(settings.report_passes(report))
            for mutation in ('missing-family', 'reordered-family', 'missing-phase', 'duplicate-run', 'argv', 'stdout',
                             'stream-hash', 'artifact-bytes', 'artifact-mode', 'artifact-path', 'counts', 'binary', 'cleanup'):
                changed = copy.deepcopy(report)
                first = changed['checks'][0]['phases'][0]
                native = changed['checks'][0]['phases'][1]
                if mutation == 'missing-family':
                    changed['checks'].pop()
                elif mutation == 'reordered-family':
                    changed['checks'].reverse()
                elif mutation == 'missing-phase':
                    changed['checks'][0]['phases'].pop()
                elif mutation == 'duplicate-run':
                    first['runs'][1] = first['runs'][0]
                elif mutation == 'argv':
                    changed['runs'][native['runs'][1]]['argv'].insert(-1, '--//:string_flag=blue')
                elif mutation == 'stdout':
                    changed['runs'][first['runs'][0]]['stdout'] = 'forged'
                elif mutation == 'stream-hash':
                    changed['runs'][first['runs'][0]]['stdout_full_sha256'] = '0' * 64
                elif mutation == 'artifact-bytes':
                    first['artifacts'][0]['bytes_base64'] = base64.b64encode(b'fake').decode()
                elif mutation == 'artifact-mode':
                    first['artifacts'][0]['mode'] = 0o444
                elif mutation == 'artifact-path':
                    first['artifacts'][0]['path'] = str(Path(directory) / 'unrelated-but-inside')
                elif mutation == 'counts':
                    native['counts']['actions_executed'] = 0
                elif mutation == 'binary':
                    changed['executables_after']['native']['sha256'] = '0' * 64
                else:
                    changed['cleanup_receipt']['removed'] = False
                with self.subTest(mutation=mutation):
                    self.assertFalse(settings.report_passes(changed))

    def test_pass_strings_counts_missing_pairs_are_not_engine_evidence(self):
        synthetic = dict(checks=[dict(name=c['name'], kind=c['kind'], status='PASS') for c in settings.CASES],
                         planned_checks=copy.deepcopy(settings.CASES), completed_checks=9,
                         completed_differential_cases=8, errors=[], cleanup=[], scratch_cleaned=True,
                         sources_unchanged=True, reference_shutdown_succeeded=True)
        self.assertFalse(settings.report_passes(synthetic))
        for value in (None, [], {}, {'checks': None}):
            self.assertFalse(settings.report_passes(value))

    def test_exact_phase_plan_is_required_before_trusting_pass(self):
        item = settings.CASES[2]
        for phases in ([], [{'probe': 'allowed', 'engine': 'native'}],
                       [{'probe': 'allowed', 'engine': 'reference'}],
                       [{'probe': p, 'engine': e} for p, e in reversed(settings.phases_for(item))]):
            check = dict(name=item['name'], kind=item['kind'], input=item, status='PASS', phases=phases)
            with self.assertRaisesRegex(ValueError, 'phases'):
                settings.validate_check(check, {})

    def test_complete_phase_names_without_real_commands_cannot_pass(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            item = settings.CASES[2]
            phases = []
            for probe, engine in settings.phases_for(item):
                frozen = settings.snapshot(settings.prepare(scratch / 'workspace', settings.specification(item, probe)))
                phases.append(dict(probe=probe, engine=engine, status='PASS', inputs_before=frozen, inputs_after=frozen,
                                   runs=[], cache=str(scratch / 'cases' / item['name'] / (probe + '-cache'))))
            check = dict(name=item['name'], kind=item['kind'], input=item, status='PASS', phases=phases)
            with self.assertRaisesRegex(ValueError, 'real invocations'):
                settings.validate_check(check, dict(scratch_directory=directory, configuration={'reference': '/official'}))

    def test_reference_failure_is_not_relabelled_as_boundary_or_pass(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            report = dict(checks=[], runs=[])
            runner = mock.Mock(report=report)
            runner.run.side_effect = ValueError('unit tests never execute native')
            reference = mock.Mock(workspace=scratch / 'workspace', output_base=scratch / 'reference-base')
            reference.run.side_effect = ValueError('unit tests never execute Bazel')
            settings.run_case(mock.Mock(native=Path('/native')), runner, scratch, reference, settings.CASES[2])
            check = report['checks'][0]
            self.assertEqual(check['status'], 'REFERENCE_FAILURE')
            self.assertFalse(check['parity_claim'])
            self.assertEqual([(p['probe'], p['engine']) for p in check['phases']], settings.phases_for(settings.CASES[2]))

    def test_shutdown_failure_still_removes_private_scratch(self):
        temporary = tempfile.TemporaryDirectory(dir='/tmp/opencode')
        scratch = Path(temporary.name)
        reference = mock.Mock()
        report = dict(runs=[record(stderr='shutdown failed', returncode=1, name='build-settings.shutdown')],
                      cleanup=[], errors=[], reference_shutdown_succeeded=False, scratch_cleaned=False)
        settings.finish(reference, temporary, report)
        reference.shutdown.assert_called_once()
        self.assertFalse(report['reference_shutdown_succeeded'])
        self.assertTrue(report['scratch_cleaned'])
        self.assertFalse(scratch.exists())
        self.assertEqual(report['cleanup_receipt'], dict(path=str(scratch), removed=True))
        self.assertTrue(report['errors'])

    def test_shutdown_exception_and_cleanup_failure_never_pass(self):
        reference = mock.Mock()
        reference.shutdown.side_effect = ValueError('shutdown exception')
        temporary = mock.Mock()
        temporary.name = '/tmp/opencode/synthetic-no-such-scratch'
        temporary.cleanup.side_effect = OSError('cleanup denied')
        report = dict(runs=[], cleanup=[], errors=[], reference_shutdown_succeeded=False, scratch_cleaned=False)
        settings.finish(reference, temporary, report)
        self.assertFalse(report['reference_shutdown_succeeded'])
        self.assertFalse(report['scratch_cleaned'])
        self.assertEqual(report['errors'], ['reference shutdown: shutdown exception', 'scratch cleanup: cleanup denied'])


if __name__ == '__main__':
    unittest.main()
