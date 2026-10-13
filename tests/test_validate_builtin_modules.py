"""Harness guards only: synthetic records never stand in for engine QA.

No native, Bazel, strace, downloads, or compilation is launched by these tests.
Tiny synthetic trees exercise the verifier, not the genuine materializer.
"""

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
import validate_builtin_modules as builtin


def record(stdout='', stderr='', returncode=0, **changes):
    value = dict(id=0, name='synthetic-not-CLI-evidence', stdout=stdout, stderr=stderr,
                 returncode=returncode, timed_out=False)
    value.update(changes)
    return value


MESSAGES = {
    'explicit': 'unknown apparent repository @bazel_tools from canonical repository ""; extension repository not materialized or dependency not declared',
    'malformed': '--experimental-builtin-modules expects true or false',
    'filesystem-override': 'unsupported option --override_repository; MODULE overrides do not replace the built-in repository filesystem',
    'collision': 'repository name "bazel_tools" is already used',
    'empty-registry': 'module rules_license@1.0.0 was not found in registries [/private/empty-registry]',
    'metadata-consumed': 'module metadata_only_probe@8.7 was not found in registries [/private/registry]',
}
ROOTS = {'empty-root': builtin.ROOT_MODULE,
         'older-platforms': builtin.ROOT_MODULE + 'bazel_dep(name = "platforms", version = "0.0.10")\n'}


class PlanAndFixtureTests(unittest.TestCase):
    def test_exact_eight_cases_two_genuine_pairs_six_boundaries(self):
        self.assertEqual(len(builtin.CASES), 8)
        self.assertEqual(len(set(name for name, _ in builtin.PLAN)), 8)
        self.assertEqual([c['kind'] for c in builtin.CASES], ['differential'] * 2 + ['native_boundary'] * 6)
        self.assertEqual(sum(len(builtin.phases_for(c)) for c in builtin.CASES), 23)
        self.assertEqual(sum(len(builtin.steps_for(c, e)) for c in builtin.CASES
                             for _, e in builtin.phases_for(c)), 27)
        for item in builtin.CASES:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe, engine in builtin.phases_for(item):
                self.assertTrue(builtin.steps_for(item, engine))
                self.assertFalse(any(step[0] in ('test', 'run', 'mod') for step in builtin.steps_for(item, engine)))
                if item['kind'] == 'native_boundary':
                    self.assertEqual(engine, 'native')

    def test_real_helper_is_loaded_in_macro_not_emulated_in_build(self):
        build, macro = ast.parse(builtin.BUILD), ast.parse(builtin.MACRO)
        self.assertNotIn('**', builtin.BUILD)
        self.assertNotIn('struct(', builtin.BUILD)
        self.assertIn("load(':copy.bzl', 'copy')", builtin.BUILD)
        self.assertIn("load('@bazel_tools//tools/build_defs/repo:utils.bzl', 'update_attrs')", builtin.MACRO)
        self.assertIn("update_attrs(struct(name = 'copy')", builtin.MACRO)
        self.assertIn('native.genrule(**values)', builtin.MACRO)
        self.assertIn('cat $(SRCS) > $@', builtin.MACRO)
        self.assertEqual(len(build.body), 2)
        self.assertEqual(len(macro.body), 2)
        # Python AST is only a lexical guard, not a fake Starlark evaluator.

    def test_all_generated_module_inputs_are_lexically_well_formed(self):
        for item in builtin.CASES:
            for probe in item['probes']:
                spec = builtin.specification(item, probe, ROOTS)
                with self.subTest(case=item['name'], probe=probe):
                    ast.parse(spec['module'])
                    ast.parse(spec['build'])
                    if spec['metadata']:
                        ast.parse(spec['metadata'])

    def test_captured_real_closure_and_distributed_helper_integrity(self):
        manifest, roots, helper = builtin.load_inputs()
        self.assertEqual(roots, ROOTS)
        self.assertEqual(len(manifest['files']), 421)
        self.assertTrue(all(v['mode'] == 0o755 for v in manifest['files']))
        entry = next(v for v in manifest['files'] if v['path'] == builtin.HELPER)
        self.assertEqual(builtin.digest(helper), entry['sha256'])
        self.assertEqual(len(helper), entry['size'])
        self.assertIn(b'def update_attrs(', helper)
        self.assertIn(b'print(', helper)
        self.assertNotEqual(builtin.digest(helper), builtin.digest(builtin.MACRO.encode()))

    def test_native_command_keeps_remote_disabled_and_reference_uses_shared_api(self):
        command = builtin.native_command(Path('/native'), Path('/private/cache'), Path('/private/mirror'), 'query', builtin.TARGET)
        self.assertEqual(command, ['/native', '--ignore_all_rc_files', '--cache-dir=/private/cache', 'query',
                                   '--registry=file:///private/mirror', builtin.SELECTOR, builtin.TARGET])
        self.assertNotIn('--lockfile_mode=off', command)
        runner = mock.Mock()
        reference = builtin.DirectReference(Path('/official'), Path('/workspace'), Path('/scratch'), runner, 'builtin-modules')
        reference.registry = Path('/private/mirror')
        reference.run('q', 'query', builtin.TARGET)
        argv = runner.run.call_args.args[1]
        self.assertNotIn(builtin.SELECTOR, argv)
        self.assertEqual([v for v in argv if v.startswith('--registry=')], [
            '--registry=file:///private/mirror', '--registry=https://bcr.bazel.build'])
        self.assertIn('--repository_cache=/scratch/repository-cache', argv)
        self.assertIn('--repo_contents_cache=/scratch/repository-contents', argv)
        self.assertIn('--remote_executor=', argv)

    def test_profile_and_collisions_are_native_only_precise_subprobes(self):
        item = builtin.CASES[2]
        self.assertIsNone(builtin.specification(item, 'default', ROOTS)['selector'])
        self.assertEqual(builtin.specification(item, 'false', ROOTS)['selector'], builtin.SELECTOR + '=false')
        self.assertEqual(builtin.specification(item, 'malformed', ROOTS)['selector'], builtin.SELECTOR + '=maybe')
        self.assertEqual(builtin.specification(item, 'filesystem-override', ROOTS)['extra'], ['--override_repository=bazel_tools=metadata'])
        item = builtin.CASES[3]
        ignored = builtin.specification(item, 'ignored-dev-alias', ROOTS)
        self.assertIn('dev_dependency = True', ignored['module'])
        self.assertIn('repo_name = "bazel_tools"', ignored['module'])
        self.assertIn('--ignore_dev_dependency=true', ignored['extra'])
        self.assertEqual(ignored['diagnostic'], 'collision')

    def test_metadata_override_is_explicit_test_only_and_canonical(self):
        item = builtin.CASES[5]
        negative = builtin.specification(item, 'metadata-consumed', ROOTS)
        positive = builtin.specification(item, 'filesystem-retained', ROOTS)
        self.assertIn('local_path_override(module_name = "bazel_tools", path = "metadata")', positive['module'])
        self.assertEqual(positive['target'], builtin.CANONICAL)
        self.assertEqual(positive['metadata'], 'module(name = "bazel_tools", version = "8.7")\n')
        self.assertIn('metadata_only_probe', negative['metadata'])
        self.assertEqual(negative['diagnostic'], 'metadata-consumed')
        self.assertIsNone(positive['diagnostic'])
        for item in builtin.CASES[:2]:
            self.assertNotIn('override', builtin.specification(item, item['probes'][0], ROOTS)['module'])

    def test_prepare_owns_only_private_inputs_and_records_metadata_change(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            workspace = Path(directory) / 'workspace'
            item = builtin.CASES[1]
            paths = builtin.prepare(workspace, builtin.specification(item, 'older-platforms', ROOTS))
            self.assertEqual({p.name for p in paths}, {'MODULE.bazel', 'BUILD.bazel', 'copy.bzl'})
            self.assertEqual((workspace / 'copy.bzl').read_text(), builtin.MACRO)
            self.assertFalse((workspace / 'copy.txt').exists())
            item = builtin.CASES[5]
            first = builtin.prepare(workspace, builtin.specification(item, 'metadata-consumed', ROOTS))
            before = builtin.snapshot(first)
            second = builtin.prepare(workspace, builtin.specification(item, 'filesystem-retained', ROOTS))
            after = builtin.snapshot(second)
            self.assertEqual([p for p in before if before[p] != after[p]], [str(workspace / 'metadata/MODULE.bazel')])
            self.assertFalse((workspace / 'copy.bzl').exists())

    def test_defaults_and_source_capture_include_manifest_not_archive_bytes(self):
        args = builtin.argument_parser().parse_args([])
        self.assertEqual(args.output, builtin.ROOT / 'artifacts/builtin-modules-validation.json')
        self.assertEqual(args.scratch_parent, Path('/tmp/opencode'))
        self.assertEqual(args.timeout, 180)
        sources = builtin.source_paths()
        for path in (builtin.RESOURCES / 'manifest.json', builtin.RESOURCES / 'embedded_tools.tar.gz',
                     builtin.FIXTURE / 'expectations.json', builtin.ROOT / 'src/main.rs',
                     builtin.ROOT / 'scripts/validate_builtin_modules.py'):
            self.assertIn(path, sources)
        value = builtin.fingerprint(builtin.RESOURCES / 'MODULE.bazel')
        self.assertNotIn('bytes_base64', value)
        self.assertEqual(value['sha256'], builtin.digest((builtin.RESOURCES / 'MODULE.bazel').read_bytes()))


class ClassifierTests(unittest.TestCase):
    def test_each_diagnostic_requires_its_exact_class(self):
        for diagnostic, message in MESSAGES.items():
            builtin.require_rejection(record(stderr=message, returncode=1), diagnostic)
            for wrong, other in MESSAGES.items():
                if wrong != diagnostic:
                    with self.subTest(expected=diagnostic, actual=wrong), self.assertRaises(ValueError):
                        builtin.require_rejection(record(stderr=other, returncode=1), diagnostic)

    def test_infrastructure_timeout_panic_and_missing_workspace_never_pass(self):
        for diagnostic, message in MESSAGES.items():
            good = record(stderr=message, returncode=1)
            for mutation in ({'returncode': 0}, {'returncode': None}, {'returncode': True}, {'returncode': 101},
                             {'returncode': -9}, {'returncode': 139}, {'timed_out': True}, {'interrupted': True},
                             {'spawn_error': 'missing ELF'}, {'output_truncated': True}, {'stdout': builtin.CANONICAL},
                             *({'stderr': message + '\n' + text} for text in (
                                 'download failed', 'network unavailable', 'connection refused', 'SSLHandshakeException',
                                 'permission denied', 'access denied', 'panicked at x', 'panic', 'internal error',
                                 'FATAL:', 'timeout', 'timed out', 'syntax error', 'parse error', 'unexpected token',
                                 'no workspace', 'MODULE.bazel not found', 'falling back to upstream'))):
                with self.subTest(diagnostic=diagnostic, mutation=mutation), self.assertRaises(ValueError):
                    builtin.require_rejection(dict(good, **mutation), diagnostic)

    def test_missing_closure_is_not_arbitrary_missing_module(self):
        for message in ('module rules_license@2.0 was not found in registries [/private/mirror]',
                        'module platforms@1.0.0 was not found in registries [/private/mirror]',
                        'module rules_license@1.0.0 was not found in registries [https://bcr.bazel.build]',
                        'module rules_license@1.0.0 not found', 'missing registry'):
            with self.assertRaises(ValueError):
                builtin.require_rejection(record(stderr=message, returncode=1), 'empty-registry')

    def test_only_genuine_builtin_prefix_spelling_is_normalized(self):
        native = builtin.require_query(record(builtin.CANONICAL + '\n'), 'native')
        for label in (builtin.TARGET, builtin.CANONICAL):
            reference = builtin.require_query(record(label + '\n'), 'reference')
            self.assertEqual(native['canonical_labels'], reference['canonical_labels'])
            self.assertEqual(reference['raw_labels'], [label])
        with self.assertRaises(ValueError):
            builtin.require_query(record(builtin.TARGET + '\n'), 'native')
        for label in ('', '@@bazel_tools+//tools/build_defs/repo:utils.bzl', builtin.CANONICAL + '\n' + builtin.CANONICAL,
                      builtin.CANONICAL + ' ', '//tools/build_defs/repo:utils.bzl', '@other//tools/build_defs/repo:utils.bzl'):
            with self.assertRaises(ValueError):
                builtin.require_query(record(label), 'reference')

    def test_success_cannot_hide_false_exit_status_or_process_failure(self):
        for mutation in ({'returncode': False}, {'returncode': 1}, {'timed_out': True}, {'interrupted': True},
                         {'spawn_error': 'absent'}, {'stderr': 'falling back to upstream'}, {'output_truncated': True}):
            with self.assertRaises(ValueError):
                builtin.require_query(record(builtin.CANONICAL, **mutation), 'native')


class TraceTests(unittest.TestCase):
    def trace(self, text, directory):
        path = Path(directory) / 'trace'
        path.write_text(text)
        return builtin.evidence(path)

    def test_single_actual_initial_exec_required_with_successful_exit(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            raw = '123 execve("/private/native", ["/private/native", "query"], 0x123 /* 4 vars */) = 0\n123 +++ exited with 0 +++\n'
            result = builtin.require_native_only_trace(self.trace(raw, directory), Path('/private/native'))
            self.assertEqual(result, dict(exec_attempts=1, initial_executable='/private/native', no_other_exec_attempts=True))
            for wrong in ('', '123 +++ exited with 0 +++\n', raw.replace(' = 0', ' = -1 ENOENT'),
                          raw.replace('/private/native', '/official/bazel'), raw.replace('exited with 0', 'exited with 1')):
                with self.assertRaises(ValueError):
                    builtin.require_native_only_trace(self.trace(wrong, directory), Path('/private/native'))

    def test_all_secondary_exec_attempts_fail_even_failed_absolute_path_execs(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            initial = '1 execve("/private/native", ["/private/native"], 0x123) = 0\n1 +++ exited with 0 +++\n'
            for extra in ('2 execve("/bin/sh", ["sh"], 0x456) = 0\n',
                          '2 execve("/absolute/reference/bazel", [], 0x456) = -1 ENOENT\n',
                          '2 execve("/java", [], 0x456) = 0\n',
                          '2 execveat(AT_FDCWD, "/upstream", [], 0x456, 0) = 0\n',
                          '2 <... execve resumed>) = 0\n'):
                with self.subTest(extra=extra), self.assertRaises(ValueError):
                    builtin.require_native_only_trace(self.trace(initial + extra, directory), Path('/private/native'))

    def test_strace_command_uses_real_execve_and_execveat_trace_file(self):
        self.assertEqual(builtin.traced_command(['/copied/native', 'query'], Path('/private/exec.log')),
                         ['/usr/bin/strace', '-f', '-e', 'trace=execve,execveat', '-o', '/private/exec.log', '/copied/native', 'query'])


class TreeEvidenceTests(unittest.TestCase):
    def create_tree(self, directory):
        # Deliberately tiny synthetic verifier input, NEVER a replacement built-in fixture.
        cache = Path(directory) / 'cache'
        root = builtin.trees_directory(cache) / ('a' * 64)
        (root / 'nested').mkdir(parents=True)
        for path in (root, root / 'nested'):
            path.chmod(0o755)
        files = {'WORKSPACE': b'workspace bytes\n', 'nested/data': b'raw\x00\xff\n'}
        entries = []
        for relative, raw in files.items():
            path = root / relative
            path.write_bytes(raw)
            path.chmod(0o755)
            entries.append(dict(path=relative, mode=0o755, size=len(raw), sha256=builtin.digest(raw)))
        manifest = {'files': entries}
        resources = Path(directory) / 'resources'
        resources.mkdir()
        raw = json.dumps(manifest).encode()
        (resources / 'manifest.json').write_bytes(raw)
        (root.parent / (root.name + '.json')).write_bytes(raw)
        return cache, root, manifest, resources

    def test_manifest_checker_reads_every_byte_mode_directory_and_sidecar(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            cache, root, manifest, resources = self.create_tree(directory)
            with mock.patch.object(builtin, 'RESOURCES', resources):
                tree = builtin.inspect_tree(cache, manifest)
                self.assertEqual(set(tree['files']), {'WORKSPACE', 'nested/data'})
                self.assertEqual(tree['directories'], {'.': 0o755, 'nested': 0o755})
                self.assertEqual(tree['root'], str(root))
                self.assertEqual(tree['files']['nested/data']['sha256'], builtin.digest(b'raw\x00\xff\n'))
                self.assertEqual(tree['entries'], ['a' * 64, 'a' * 64 + '.json'])

    def test_corruptions_cannot_be_accepted_using_mutable_sidecar(self):
        for change in ('bytes', 'mode', 'extra', 'missing', 'link', 'directory-mode', 'extra-directory', 'sidecar', 'stage'):
            with self.subTest(change=change), tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
                cache, root, manifest, resources = self.create_tree(directory)
                path = root / 'nested/data'
                if change == 'bytes':
                    path.write_bytes(b'wrong\n')
                elif change == 'mode':
                    path.chmod(0o644)
                elif change == 'extra':
                    (root / 'extra').write_bytes(b'x')
                elif change == 'missing':
                    path.unlink()
                elif change == 'link':
                    path.unlink()
                    path.symlink_to(root / 'WORKSPACE')
                elif change == 'directory-mode':
                    (root / 'nested').chmod(0o700)
                elif change == 'extra-directory':
                    (root / 'extra').mkdir()
                elif change == 'sidecar':
                    (root.parent / (root.name + '.json')).write_text('{}')
                else:
                    (root.parent / '.stage-leaked').mkdir()
                with mock.patch.object(builtin, 'RESOURCES', resources), self.assertRaises(ValueError):
                    builtin.inspect_tree(cache, manifest)

    def test_cache_mutations_have_exact_before_after_receipts(self):
        for probe in ('bytes', 'mode', 'extra', 'missing-manifest'):
            with self.subTest(probe=probe), tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
                cache, root, _, _ = self.create_tree(directory)
                original = builtin.evidence(root / 'WORKSPACE')
                result = builtin.mutate_cache(cache, probe)
                self.assertNotEqual(result['before'], result['after'])
                self.assertEqual(result['probe'], probe)
                self.assertTrue(Path(result['path']).is_relative_to(builtin.trees_directory(cache)))
                if probe == 'bytes':
                    self.assertEqual(result['before'], original)
                    self.assertTrue(builtin.valid_evidence(result['after'], b'intentional cache corruption\n'))
                elif probe == 'mode':
                    self.assertEqual(result['before']['sha256'], result['after']['sha256'])
                    self.assertEqual(result['after']['mode'], 0o644)
                elif probe == 'extra':
                    self.assertIsNone(result['before'])
                else:
                    self.assertIsNone(result['after'])

    def test_rejection_requires_no_tree_or_staging_publication(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            cache = Path(directory)
            self.assertEqual(builtin.no_publication(cache), [])
            (builtin.trees_directory(cache) / '.stage-leaked').mkdir(parents=True)
            with self.assertRaises(ValueError):
                builtin.no_publication(cache)


class ReportAndCleanupTests(unittest.TestCase):
    def test_pass_strings_counts_and_missing_pairs_are_not_cli_evidence(self):
        synthetic = dict(checks=[dict(name=c['name'], kind=c['kind'], status='PASS') for c in builtin.CASES],
                         planned_checks=copy.deepcopy(builtin.CASES), completed_checks=8,
                         completed_differential_cases=2, errors=[], cleanup=[], scratch_cleaned=True,
                         sources_unchanged=True, reference_shutdown_succeeded=True)
        self.assertFalse(builtin.report_passes(synthetic))
        for mutation in ('missing', 'duplicate', 'reordered', 'unexpected', 'wrong-kind'):
            report = copy.deepcopy(synthetic)
            if mutation == 'missing':
                report['checks'].pop()
            elif mutation == 'duplicate':
                report['checks'][-1] = report['checks'][0]
            elif mutation == 'reordered':
                report['checks'].reverse()
            elif mutation == 'unexpected':
                report['checks'].append({'name': 'unplanned', 'kind': 'native_boundary'})
            else:
                report['checks'][0]['kind'] = 'native_boundary'
            self.assertFalse(builtin.report_passes(report))
        for value in (None, [], {}, {'checks': None}):
            self.assertFalse(builtin.report_passes(value))

    def test_validate_check_demands_exact_phase_plan_before_trusting_pass(self):
        item = builtin.CASES[0]
        for phases in ([], [{'probe': 'empty-root', 'engine': 'native'}],
                       [{'probe': 'empty-root', 'engine': 'reference'}],
                       [{'probe': 'empty-root', 'engine': 'native'}, {'probe': 'empty-root', 'engine': 'reference'}]):
            check = dict(name=item['name'], kind=item['kind'], input=item, status='PASS', phases=phases)
            with self.assertRaisesRegex(ValueError, 'phases'):
                builtin.validate_check(check, {}, {}, ROOTS, b'')

    def test_missing_real_invocations_cannot_pass_with_correct_phase_names(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            item = builtin.CASES[0]
            paths = builtin.prepare(scratch / 'workspace', builtin.specification(item, 'empty-root', ROOTS))
            frozen = builtin.snapshot(paths)
            phases = [dict(probe=p, engine=e, status='PASS', inputs_before=frozen, inputs_after=frozen,
                           registry_before='frozen', registry_after='frozen', runs=[],
                           cache=str(scratch / 'cases' / item['name'] / 'empty-root-cache'),
                           registry=str(builtin.FIXTURE / 'registry')) for p, e in builtin.phases_for(item)]
            check = dict(name=item['name'], kind=item['kind'], input=item, status='PASS', phases=phases)
            report = dict(scratch_directory=str(scratch), configuration={'reference': '/official'})
            with self.assertRaisesRegex(ValueError, 'real invocations'):
                builtin.validate_check(check, report, {'files': []}, ROOTS, b'')

    def test_reference_failure_never_relabels_as_native_boundary_or_shortcuts_pair(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            report = {'checks': [], 'runs': []}
            runner = mock.Mock(report=report)
            reference = mock.Mock(workspace=scratch / 'workspace')
            args = builtin.argument_parser().parse_args([])
            args.distributed_native = scratch / 'native'
            attempted = []

            def fail(local, ref, workspace, phase, *args):
                attempted.append(phase['engine'])
                raise ValueError('download failed' if phase['engine'] == 'reference' else 'native not launched by unit test')

            with mock.patch.object(builtin, 'invoke_phase', side_effect=fail):
                builtin.run_case(args, runner, runner, scratch, reference, builtin.CASES[0], {'files': []}, ROOTS, b'')
            self.assertEqual(attempted, ['reference', 'native'])
            self.assertEqual(report['checks'][0]['status'], 'REFERENCE_FAILURE')
            self.assertFalse(report['checks'][0]['parity_claim'])

    def test_shutdown_failure_still_cleans_private_temporary_directory(self):
        temporary = tempfile.TemporaryDirectory(dir='/tmp/opencode')
        scratch = Path(temporary.name)
        (scratch / 'workspace').mkdir()
        reference = mock.Mock()
        report = dict(runs=[record(stderr='shutdown failed', returncode=1, name='builtin-modules.shutdown')],
                      cleanup=[], errors=[], reference_shutdown_succeeded=False, scratch_cleaned=False)
        builtin.finish(reference, temporary, report)
        reference.shutdown.assert_called_once()
        self.assertFalse(report['reference_shutdown_succeeded'])
        self.assertTrue(report['scratch_cleaned'])
        self.assertEqual(report['cleanup_receipt'], {'path': str(scratch), 'removed': True})
        self.assertFalse(scratch.exists())
        self.assertTrue(report['errors'])
        self.assertFalse(builtin.report_passes(report))

    def test_cleanup_failure_and_trap_invocation_are_not_success_receipts(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            temporary = mock.Mock(name=directory)
            temporary.name = directory
            temporary.cleanup.side_effect = OSError('cleanup denied')
            (Path(directory) / 'path-trap.log').write_text('attempted')
            report = dict(runs=[], cleanup=[], errors=[], scratch_cleaned=False)
            builtin.finish(None, temporary, report)
            self.assertFalse(report['scratch_cleaned'])
            self.assertFalse(report['path_trap_not_invoked'])
            self.assertIn('scratch cleanup: cleanup denied', report['errors'])
            self.assertFalse(builtin.report_passes(report))

    def test_shutdown_exception_also_runs_cleanup(self):
        temporary = tempfile.TemporaryDirectory(dir='/tmp/opencode')
        reference = mock.Mock()
        reference.shutdown.side_effect = ValueError('shutdown exception')
        report = dict(runs=[], errors=[], cleanup=[], scratch_cleaned=False, reference_shutdown_succeeded=False)
        builtin.finish(reference, temporary, report)
        self.assertTrue(report['scratch_cleaned'])
        self.assertFalse(report['reference_shutdown_succeeded'])
        self.assertEqual(report['errors'], ['reference shutdown: shutdown exception'])


if __name__ == '__main__':
    unittest.main()
