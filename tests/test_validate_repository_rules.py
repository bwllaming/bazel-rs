"""Harness-only tests. No native/reference Bazel process or fake Starlark runtime.

Synthetic diagnostics test classifier guards, not engine behavior. Syntax tests
check the Python-compatible fixture subset lexically, not Bazel acceptance.
"""

import ast
import base64
import copy
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_repository_rules as rules


def record(stdout='', stderr='', returncode=0, **changes):
    return dict(id=0, name='synthetic-not-CLI-evidence', stdout=stdout, stderr=stderr,
                returncode=returncode, timed_out=False, **changes)


MESSAGES = {
    'missing': "mandatory attribute 'required' missing in 'maker' call",
    'unknown': "no such attribute 'unknown_attr' in 'maker' rule",
    'rollback': 'Error in fail: RULE_ROLLBACK_SENTINEL',
    'path': 'repository output path must be relative and clean',
    'symlink': 'template source symlinks are unsupported',
    'execute': "repository_ctx has no method 'execute'",
    'return': 'repository rule return value is unsupported; expected None',
    'extension': "name 'module_extension' is not defined",
    'ascii': 'non-ASCII string literals are unsupported',
    'cycle': 'repository template dependency cycle: made -> other -> made',
    'ticks': 'Maximum number of ticks exceeded',
}


class FixtureTests(unittest.TestCase):
    def test_fixed_plan_has_seven_differentials_and_nine_boundaries(self):
        self.assertEqual(len(rules.CASES), 16)
        self.assertEqual(len(set(name for name, _ in rules.PLAN)), 16)
        self.assertEqual(sum(c['kind'] == 'differential' for c in rules.CASES), 7)
        self.assertEqual([c['name'] for c in rules.CASES[:7]], [
            'file.transitive-attrs-overwrite', 'template.binary-label-and-stage',
            'identity.root-and-dependency', 'generated.definition-and-template',
            'lazy.unused-callback', 'group.unused-missing-attribute',
            'group.unused-unknown-attribute'])
        self.assertTrue(all(c['kind'] == 'native_boundary' for c in rules.CASES[7:]))
        for item in rules.CASES:
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe, engine in rules.phases_for(item):
                steps = rules.steps_for(item, probe, engine)
                self.assertTrue(steps)
                self.assertFalse(any(step[0] in ('test', 'run') for step in steps))

    def test_generated_definition_is_exact_data_with_nested_quotes_and_newlines(self):
        syntax = ast.parse(rules.GENERATED_SEED)
        file_calls = [node for node in ast.walk(syntax) if isinstance(node, ast.Call)
                      and isinstance(node.func, ast.Attribute) and node.func.attr == 'file']
        definitions = [ast.literal_eval(node.args[1]) for node in file_calls
                       if ast.literal_eval(node.args[0]) == 'defs.bzl']
        self.assertEqual(definitions, [rules.GENERATED_CHILD])
        self.assertIn('Label("//:seed")', definitions[0])
        self.assertIn('ctx.template("data", ctx.attr._source', definitions[0])
        compile(definitions[0], 'generated-defs.bzl', 'exec')
        self.assertEqual(ast.literal_eval(rules.quote('a"b\\c\n\r\t')), 'a"b\\c\n\r\t')
        with self.assertRaises(ValueError):
            rules.quote('é')

    def test_ascii_controls_use_starlark_octal_not_hex_or_unicode_escapes(self):
        quoted = rules.quote('\x00\x017\x7f')
        self.assertEqual(quoted, '"\\000\\0017\\177"')
        self.assertEqual(ast.literal_eval(quoted), '\x00\x017\x7f')
        self.assertNotIn('\\x', quoted)
        self.assertNotIn('\\u', quoted)
        value = rules.fixture('file.transitive-attrs-overwrite')
        self.assertIn(b'ctx.file("octal", "\\000", executable = False)', value['files']['defs.bzl'])
        self.assertEqual(value['expected']['octal'], (b'\x00', False))

    def test_initialization_budget_is_native_only_and_before_callback_execution(self):
        item = rules.CASES[-1]
        self.assertEqual(item['name'], 'native.initialization-budget')
        self.assertEqual(item['kind'], 'native_boundary')
        self.assertEqual(rules.phases_for(item), [('result', 'native')])
        self.assertEqual(rules.steps_for(item, 'result', 'native'), [('query', '@made//:data', [])])
        value = rules.fixture(item['name'])
        self.assertEqual(value['diagnostic'], 'ticks')
        self.assertTrue(value['files']['defs.bzl'].startswith(b'INITIALIZATION = [i + 1 for i in range(1000000)]\n'))
        self.assertIn(b'def _impl(ctx):', value['files']['defs.bzl'])
        for message in ('timeout', 'evaluation limit exceeded', 'memory limit exceeded', 'ticks: 1000000'):
            with self.assertRaises(ValueError):
                rules.require_rejection(record(stderr=message, returncode=1), 'ticks')

    def test_all_trusted_positive_fixture_bodies_are_lexically_well_formed(self):
        for item in rules.CASES:
            for probe in item['probes']:
                files = rules.fixture(item['name'], probe, Path('/private/outside-marker'))['files']
                for path, raw in files.items():
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(case=item['name'], probe=probe, path=path):
                            compile(raw.decode(), path, 'exec')
        # This is a quoting guard, never an assertion that Python implements Starlark.

    def test_binary_template_is_literal_ordered_and_not_utf8_decoded(self):
        value = rules.fixture('template.binary-label-and-stage')
        raw = value['files']['private/source']
        self.assertEqual(raw, b'A B\r\nUTF8:\xc3\xa9 raw:\xff\x00\r\n')
        self.assertEqual(value['expected']['data'], (b'C C\r\nUTF8:\xc3\xa9 raw:\xff\x00\r\n', True))
        self.assertEqual(value['expected']['nonexec'][1], False)
        self.assertIn('{"A": "B", "B": "C", "": "NO"}', rules.TEMPLATE_DEFS)
        self.assertIn('ctx.template("same", "same"', rules.TEMPLATE_DEFS)
        self.assertEqual(value['files']['private/BUILD.bazel'], b'')
        self.assertNotIn(b'exports_files', value['files']['private/BUILD.bazel'])

    def test_transitive_frozen_helpers_attrs_none_and_overwrites_are_real_calls(self):
        value = rules.fixture('file.transitive-attrs-overwrite')
        self.assertIn('load(":helper.bzl", "write_data")', rules.FILE_DEFS)
        self.assertIn('load(":leaf.bzl", "decorate")', rules.HELPER)
        self.assertIn('def _private(ctx)', rules.HELPER)
        self.assertIn('PREFIX = ["hello "]', rules.LEAF)
        for text in ('attr.string(', 'attr.int(', 'attr.bool(', 'attr.string_list(',
                     'attr.label(', 'attr.label_list(', '"_private"', 'mandatory = True'):
            self.assertIn(text, rules.ATTRS)
        self.assertNotIn('attr.int_list', rules.ATTRS)
        self.assertNotIn('ctx.attr.numbers', rules.FILE_DEFS)
        self.assertIn('ctx.attr.optional != None', rules.FILE_DEFS)
        self.assertIn('type(ctx.attr.source) != "Label"', rules.FILE_DEFS)
        self.assertEqual(value['expected']['data'], (b'hello Ada\n', False))
        self.assertEqual(value['expected']['nested/overwritten'], (b'new\n', False))
        self.assertEqual(value['expected']['empty'], (b'', True))
        self.assertEqual(value['expected']['toggle'], (b'on\n', True))
        self.assertIn(b'required = "supplied", person = "Ada"', value['files']['MODULE.bazel'])
        self.assertIn('default = "default-person"', rules.ATTRS)

    def test_same_group_laziness_and_validation_have_distinct_proofs(self):
        lazy = rules.fixture('lazy.unused-callback')
        self.assertIn(b'maker(name = "unused", poison = True)', lazy['files']['MODULE.bazel'])
        self.assertIn(rules.LAZY_MARKER.encode(), lazy['files']['defs.bzl'])
        for name, diagnostic in (('group.unused-missing-attribute', 'missing'),
                                 ('group.unused-unknown-attribute', 'unknown')):
            value = rules.fixture(name)
            self.assertEqual(value['diagnostic'], diagnostic)
            self.assertIn(b'maker(name = "made", required = "ok")', value['files']['MODULE.bazel'])
            self.assertIn(rules.LAZY_MARKER.encode(), value['files']['defs.bzl'])

    def test_owner_registry_and_generated_chain_use_genuine_definitions(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            root = Path(directory)
            value = rules.materialize(root / 'case', root / 'workspace', 'identity.root-and-dependency', 'result')
            entry = value['registry'] / 'modules/contractdep/1.0'
            source = Path(json.loads((entry / 'source.json').read_text())['path'])
            self.assertTrue(source.is_relative_to(root))
            self.assertEqual((source / 'MODULE.bazel').read_bytes(), (entry / 'MODULE.bazel').read_bytes())
            self.assertEqual((source / 'defs.bzl').read_text(), rules.OWNER_DEFS)
            self.assertFalse((value['registry'] / 'modules/platforms').exists())
            self.assertFalse((value['registry'] / 'modules/bazel_tools').exists())
            self.assertNotIn('local_path_override', (root / 'workspace/MODULE.bazel').read_text())
            self.assertEqual((root / 'workspace/private/BUILD.bazel').read_bytes(), b'')
            self.assertNotIn(root / 'workspace/data', value['paths'])
        chain = rules.fixture('generated.definition-and-template')
        self.assertIn(b'use_repo_rule("@seed//:defs.bzl", "maker")', chain['files']['MODULE.bazel'])
        self.assertEqual(chain['expected'], {'data': (b'C C\n', False)})
        self.assertNotIn('defs.bzl', [p for p in chain['files'] if p.startswith('seed/')])

    def test_ascii_policy_checks_source_even_when_result_is_ascii_or_dead(self):
        module = rules.fixture('native.ascii-profile', 'module-unicode')
        closure = rules.fixture('native.ascii-profile', 'closure-unicode')
        self.assertIn('str(len("é"))'.encode(), module['files']['MODULE.bazel'])
        self.assertIn('str(len("é"))'.encode(), closure['files']['leaf.bzl'])
        self.assertIn(b'load(":leaf.bzl", "value")', closure['files']['helper.bzl'])
        self.assertIn(b'def unused():\n    return str(chr)', rules.fixture('native.ascii-profile', 'chr')['files']['leaf.bzl'])
        lazy = rules.fixture('native.main-query-lazy')
        self.assertIn('é'.encode(), lazy['files']['MODULE.bazel'])
        self.assertIn(b'INVALID_UNUSED_DEFINITION_EVALUATED', lazy['files']['defs.bzl'])
        self.assertEqual(lazy['target'], '//:data')

    def test_policy_return_and_extension_are_not_differential_claims(self):
        for probe in ('metadata', 'integer', 'extension'):
            value = rules.fixture('native.unsupported', probe)
            self.assertEqual(value['diagnostic'], 'extension' if probe == 'extension' else 'return')
        self.assertIn(b'return 42', rules.fixture('native.unsupported', 'integer')['files']['defs.bzl'])
        extension = rules.fixture('native.unsupported', 'extension')['files']['defs.bzl']
        self.assertIn(b'ordinary = module_extension(implementation = _impl)', extension)
        self.assertNotIn(b'use_extension', extension)


class DiagnosticTests(unittest.TestCase):
    def test_synthetic_classifiers_accept_only_expected_controlled_markers(self):
        for diagnostic, text in MESSAGES.items():
            with self.subTest(diagnostic=diagnostic):
                rules.require_rejection(record(stderr=text, returncode=1), diagnostic)
                for wrong in MESSAGES:
                    if wrong != diagnostic:
                        with self.assertRaises(ValueError):
                            rules.require_rejection(record(stderr=MESSAGES[wrong], returncode=1), diagnostic)

    def test_failure_never_passes_from_exit_status_network_crash_or_timeout(self):
        for diagnostic, text in MESSAGES.items():
            good = record(stderr=text, returncode=1)
            for change in ({'returncode': 0}, {'returncode': None}, {'returncode': True}, {'returncode': 101},
                           {'returncode': -9}, {'returncode': 139}, {'timed_out': True},
                           {'spawn_error': 'missing'}, {'interrupted': True}, {'output_truncated': True},
                           {'stdout': '//:data\n'}, {'stderr': 'unknown repository'},
                           *({'stderr': prefix + '\n' + text} for prefix in (
                               'network unavailable', 'download failed', 'SSLHandshakeException', 'panicked at',
                               'FATAL:', 'internal error', 'timeout', 'parse error', 'unexpected token', 'falling back'))):
                with self.subTest(diagnostic=diagnostic, change=change), self.assertRaises(ValueError):
                    rules.require_rejection(dict(good, **change), diagnostic)

    def test_sibling_attribute_failure_cannot_be_callback_failure(self):
        for diagnostic in ('missing', 'unknown'):
            with self.assertRaises(ValueError):
                rules.require_rejection(record(stderr=MESSAGES[diagnostic] + '\nError in fail: ' + rules.LAZY_MARKER, returncode=1), diagnostic)

    def test_source_regular_file_guard_requires_explicit_symlink_reason(self):
        # repository_ctx.rs regular_file() diagnostic, not captured CLI evidence.
        rules.require_rejection(record(stderr='/private/source must be a regular file, not a symlink', returncode=1), 'symlink')
        with self.assertRaises(ValueError):
            rules.require_rejection(record(stderr='/private/source must be a regular file', returncode=1), 'symlink')

    def test_canonical_is_observed_not_apparent_label_normalization(self):
        rules.require_canonical(record('@@+maker+made//:data\n'), rules.ROOT_CANONICAL)
        rules.require_canonical(record('@@contractdep++maker+made//:data\n'), rules.DEP_CANONICAL)
        for text in ('@made//:data\n', '@@made//:data\n', '@@+other+made//:data\n',
                     '@@+maker+made//:data\n@@+maker+made//:data\n', ''):
            with self.assertRaises(ValueError):
                rules.require_canonical(record(text), rules.ROOT_CANONICAL)


class EvidenceTests(unittest.TestCase):
    def test_binary_and_mode_fingerprints_and_symlink_no_follow(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            path = Path(directory) / 'source'
            path.write_bytes(rules.BINARY)
            path.chmod(0o644)
            first = rules.evidence(path)
            self.assertTrue(rules.valid_evidence(first, rules.BINARY, False))
            self.assertEqual(first['sha256'], hashlib.sha256(rules.BINARY).hexdigest())
            path.chmod(0o755)
            self.assertNotEqual(first, rules.evidence(path))
            self.assertTrue(rules.valid_evidence(rules.evidence(path), rules.BINARY, True))
            link = Path(directory) / 'link'
            link.symlink_to(path)
            value = rules.evidence(link)
            self.assertEqual(value['kind'], 'symlink')
            self.assertEqual(base64.b64decode(value['bytes_base64']), os.fsencode(path))
            self.assertNotEqual(value['sha256'], first['sha256'])
            for change in ({'sha256': 'wrong'}, {'size_bytes': 0}, {'bytes_base64': '!invalid!'}, {'mode': '0755'}):
                self.assertFalse(rules.valid_evidence(dict(first, **change)))

    def test_publication_selection_traces_actual_output_among_multiple_trees(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            cache = Path(directory) / 'cache'
            roots = [rules.trees_directory(cache) / (letter * 64) for letter in ('a', 'b')]
            for root in roots:
                root.mkdir(parents=True)
                (root / 'data').write_bytes(b'hello Ada\n')
                (root / 'data').chmod(0o644)
                (root / 'REPO.bazel').write_bytes(b'')
            self.assertEqual(rules.publication_root(roots[1] / 'data', cache), roots[1])
            value = rules.source_artifacts([roots[1] / 'data'], {'data': (b'hello Ada\n', False)}, cache=cache)
            self.assertEqual(value['roots'], {'data': str(roots[1])})
            self.assertTrue(value['files']['data']['module_absent'])
            (roots[1] / 'MODULE.bazel').write_bytes(b'')
            with self.assertRaisesRegex(ValueError, 'fabricated MODULE'):
                rules.source_artifacts([roots[1] / 'data'], {'data': (b'hello Ada\n', False)}, cache=cache)
            with self.assertRaises(ValueError):
                rules.no_publication(cache)

    def test_native_paths_require_root_build_label_and_owned_absolute_path(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            root = Path(directory)
            path = root / 'cache/data'
            self.assertEqual(rules.native_paths(record('//:data\t' + str(path) + '\n'), '//:data', root), [path])
            for text in ('@made//:data\t' + str(path), '//:data\trelative', '//:data\t/etc/passwd',
                         '//:data\t' + str(path) + '\n//:data\t' + str(path), '', '//:data'):
                with self.assertRaises(ValueError):
                    rules.native_paths(record(text), '//:data', root)

    def test_inputs_intentionally_mutate_only_between_frozen_phases(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            root = Path(directory)
            one = rules.materialize(root / 'case', root / 'workspace', 'native.refresh-and-repair', 'cold')
            before = rules.snapshot(one['paths'])
            two = rules.materialize(root / 'case', root / 'workspace', 'native.refresh-and-repair', 'helper-change', update=True)
            after = rules.snapshot(two['paths'])
            changed = [p for p in before if before[p] != after[p]]
            self.assertEqual(changed, [str(root / 'workspace/leaf.bzl')])
            self.assertEqual(before[str(one['outside'])], after[str(two['outside'])])
            self.assertEqual(one['expected']['data'][0], b'X\n')
            self.assertEqual(two['expected']['data'][0], b'Y\n')

    def test_refresh_explicitly_reexports_the_mutated_leaf_in_all_four_phases(self):
        item = next(c for c in rules.CASES if c['name'] == 'native.refresh-and-repair')
        phases = ['cold', 'helper-change', 'input-change', 'repair']
        self.assertEqual(item['probes'], phases)
        self.assertEqual(rules.phases_for(item), [(phase, 'native') for phase in phases])
        expected_outputs = [b'X\n', b'Y\n', b'Y Y\r\n\xff', b'Y Y\r\n\xff']
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            root = Path(directory)
            previous = None
            for index, phase in enumerate(phases):
                with self.subTest(phase=phase):
                    value = rules.materialize(root / 'case', root / 'workspace', item['name'], phase,
                                              update=index != 0)
                    files = value['files']
                    self.assertEqual(files['helper.bzl'],
                                     b'load(":leaf.bzl", _replacement = "replacement")\nreplacement = _replacement\n')
                    # Assert the actual rule consumes the explicit public export,
                    # not an unrelated constant or an unused mutated helper.
                    self.assertIn(b'load(":helper.bzl", "replacement")', files['defs.bzl'])
                    self.assertIn(b'substitutions = {"A": replacement}', files['defs.bzl'])
                    self.assertEqual(files['leaf.bzl'], b'replacement = "X"\n' if index == 0 else b'replacement = "Y"\n')
                    self.assertEqual(value['expected']['data'], (expected_outputs[index], False))
                    self.assertEqual(rules.steps_for(item, phase, 'native'),
                                     [('build', '//:data', []), ('build', '//:copy', [])])
                    current = rules.snapshot(value['paths'])
                    if previous is not None:
                        changed_hashes = [path for path in previous
                                          if previous[path]['sha256'] != current[path]['sha256']]
                        wanted = ([] if phase == 'repair' else
                                  [str(root / 'workspace' / ('leaf.bzl' if phase == 'helper-change' else 'private/source'))])
                        self.assertEqual(changed_hashes, wanted)
                        self.assertEqual(previous[str(value['outside'])], current[str(value['outside'])])
                    previous = current

    def test_hashes_and_bounded_streams_survive_interruption(self):
        report = {'runs': []}
        runner = rules.RecordingRunner(report, 1)

        def interrupted(*args, **kwargs):
            raw = b'\xff' * (rules.MAX_STREAM + 1)
            report['runs'].append(record(stdout='unused', stdout_base64=base64.b64encode(raw).decode()))
            raise KeyboardInterrupt

        with mock.patch.object(rules.Runner, 'run', side_effect=interrupted), self.assertRaises(KeyboardInterrupt):
            runner.run('interrupt', ['/not/executed'], Path('/unused'))
        entry = report['runs'][0]
        self.assertTrue(entry['output_truncated'])
        self.assertEqual(entry['stdout_full_size_bytes'], rules.MAX_STREAM + 1)
        self.assertEqual(len(base64.b64decode(entry['stdout_base64'])), rules.MAX_STREAM)
        self.assertEqual(entry['stdout_sha256'], rules.digest(b'\xff' * rules.MAX_STREAM))
        self.assertNotIn('TOKEN', entry['environment'])


class InvocationAndReportTests(unittest.TestCase):
    def test_reference_uses_real_builtin_closure_and_existing_api(self):
        runner = mock.Mock()
        reference = rules.DirectReference(Path('/official'), Path('/workspace'), Path('/scratch'), runner, 'repository-rules')
        reference.registry = Path('/private/registry space')
        reference.run('files', 'cquery', '--output=files', '//:data')
        argv = runner.run.call_args.args[1]
        for flag in ('--ignore_all_rc_files', '--repository_cache=/scratch/repository-cache',
                     '--repo_contents_cache=/scratch/repository-contents', '--remote_cache=',
                     '--remote_executor=', '--bes_backend=', '--output=files'):
            self.assertIn(flag, argv)
        self.assertEqual([v for v in argv if v.startswith('--registry=')], [
            '--registry=file:///private/registry%20space', '--registry=https://bcr.bazel.build'])
        self.assertFalse(any('override' in arg for arg in argv))
        native = rules.native_command(Path('/native'), Path('/private/cache'), reference.registry, 'build', '//:data')
        self.assertEqual(native, ['/native', '--ignore_all_rc_files', '--cache-dir=/private/cache', 'build',
                                  '--lockfile_mode=off', '--registry=file:///private/registry%20space', '//:data'])
        defaults = rules.argument_parser().parse_args([])
        self.assertEqual(defaults.output.name, 'repository-rules-validation.json')
        self.assertEqual(defaults.scratch_parent, Path('/tmp/opencode'))
        self.assertEqual(rules.REFERENCE_SHA256, 'd302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8')

    def test_counts_and_pass_strings_are_not_cli_evidence(self):
        synthetic = dict(checks=[dict(name=c['name'], kind=c['kind'], status='PASS') for c in rules.CASES],
                         planned_checks=copy.deepcopy(rules.CASES), completed_checks=16,
                         completed_differential_cases=7, errors=[], cleanup=[], scratch_cleaned=True,
                         sources_unchanged=True, reference_shutdown_succeeded=True)
        self.assertFalse(rules.report_passes(synthetic))
        for mutation in ('missing', 'duplicate', 'unexpected', 'reordered', 'wrong-kind'):
            altered = copy.deepcopy(synthetic)
            if mutation == 'missing':
                altered['checks'].pop()
            elif mutation == 'duplicate':
                altered['checks'][-1] = altered['checks'][0]
            elif mutation == 'unexpected':
                altered['checks'].append({'name': 'extra', 'kind': 'differential', 'status': 'PASS'})
            elif mutation == 'reordered':
                altered['checks'].reverse()
            else:
                altered['checks'][0]['kind'] = 'native_boundary'
            self.assertFalse(rules.report_passes(altered))
        for value in (None, [], {}, {'checks': None}, {'checks': [{'status': 'PASS'}]}):
            self.assertFalse(rules.report_passes(value))

    def test_reference_error_cannot_be_relabeled_native_boundary_pass(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            report = {'checks': [], 'runs': []}
            runner = mock.Mock(report=report)
            runner.run.side_effect = AssertionError('native must not hide reference failure')
            reference = mock.Mock()
            reference.run.return_value = record(stderr='download failed', returncode=1)
            item = next(c for c in rules.CASES if c['name'] == 'group.unused-missing-attribute')
            rules.run_case(rules.argument_parser().parse_args([]), runner, scratch, scratch / 'workspace', reference, item)
            self.assertEqual(report['checks'][0]['status'], 'REFERENCE_FAILURE')
            self.assertFalse(report['checks'][0]['parity_claim'])
            runner.run.assert_not_called()

    def test_shutdown_failure_is_strict_and_still_removes_workspace_cache(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory) / 'owned'
            (scratch / 'workspace').mkdir(parents=True)
            (scratch / 'cache').mkdir()
            report = dict(runs=[record(stderr='shutdown rejected', returncode=1)], cleanup=[], errors=[],
                          reference_shutdown_succeeded=False, scratch_cleaned=False)
            report['runs'][0]['name'] = 'repository-rules.shutdown'
            reference = mock.Mock()
            rules.finish(reference, scratch, report)
            reference.shutdown.assert_called_once()
            self.assertFalse(report['reference_shutdown_succeeded'])
            self.assertTrue(report['scratch_cleaned'])
            self.assertFalse(scratch.exists())
            self.assertTrue(report['errors'])

    def test_interrupted_main_preserves_failure_report_and_strict_cleanup(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            output = Path(directory) / 'receipt.json'

            def seam(runner, name, argv, cwd, timeout=None):
                # Only lifecycle commands are simulated; no simulated case passes.
                if name not in ('reference.version', 'repository-rules.shutdown'):
                    self.fail('unexpected subprocess seam')
                entry = record('bazel 9.3.0\n' if name == 'reference.version' else '')
                entry.update(id=len(runner.report['runs']), name=name, argv=list(argv), cwd=str(cwd), timeout_seconds=timeout)
                runner.report['runs'].append(entry)
                return entry

            with mock.patch.object(rules, 'validate_binaries', return_value={}), \
                    mock.patch.object(rules.Runner, 'run', new=seam), \
                    mock.patch.object(rules, 'run_case', side_effect=KeyboardInterrupt), mock.patch('builtins.print'):
                code = rules.main(['--output', str(output), '--scratch-parent', directory])
            report = json.loads(output.read_text())
            self.assertEqual(code, 130)
            self.assertEqual(report['status'], 'FAIL')
            self.assertEqual(report['completed_checks'], 0)
            self.assertIn('interrupted', report['errors'])
            self.assertTrue(report['reference_shutdown_succeeded'])
            self.assertTrue(report['scratch_cleaned'])
            self.assertFalse(Path(report['scratch_directory']).exists())
            self.assertTrue(report['sources_unchanged'])
            self.assertFalse(rules.report_passes(report))


if __name__ == '__main__':
    unittest.main()
