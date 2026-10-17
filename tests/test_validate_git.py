"""Harness-only tests: real /usr/bin/git setup; synthetic Bazel, never CLI evidence."""

import base64
import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_git as gitcheck
from validate import Runner


def item(name):
    return next(value for value in gitcheck.CASES if value['name'] == name)


def record(stdout='', stderr='', returncode=0, **extra):
    return dict(id=0, name='synthetic', stdout=stdout, stderr=stderr,
                returncode=returncode, timed_out=False, **extra)


# Semantic contract examples only; intentionally NOT represented as captured CLI.
MESSAGES = {
    'module_name': 'error: Git MODULE.bazel module name mismatch: expected opencode_gitcheck_dep',
    'module_syntax': 'error: Git MODULE.bazel syntax error: unexpected token EOF',
    'unknown_commit': "git fetch: fatal: remote error: upload-pack: not our ref " + 'f' * 40,
    'tag': 'error: git_override tag is unsupported',
    'branch': 'error: git_override branch is unsupported',
    'commit': 'error: git_override commit requires a full 40 hex SHA',
    'submodules': 'error: git_override submodules are not supported',
    'commands': 'error: git_override patch_cmds are unsupported',
    'external_patches': 'error: git_override external patches are unsupported',
    'prefix_path': 'error: strip_prefix contains unsafe parent traversal ..',
    'prefix_symlink': 'error: strip_prefix must not select a symlink',
}


# Verbatim stderr from the first retained artifacts/git-validation.json (native
# SHA256 9f31a98b0e19547c20a0b401f2e5fc90abf9adbdfebb5989313b74823b3a4535).
# No runtime dependency on the report or its already-cleaned scratch directory.
RECORDED_SELECTOR = ('bazel-rs: git_override for module "opencode_gitcheck_dep" supports only full commit pins; '
                     'tags, branches and init_submodules are not implemented\n')
RECORDED_DIAGNOSTICS = {
    'module_name': ('bazel-rs: module name mismatch at /tmp/opencode/git-validation-y8p760la/native/'
                    'native.module-name-before-publication/repositories/git/trees/.stage-Pat85l/MODULE.bazel: '
                    'requested "opencode_gitcheck_dep", declared "opencode_gitcheck_wrong"\n'),
    'module_syntax': ('bazel-rs: error: Parse error: unexpected new line, expected expression\n'
                      '--> /tmp/opencode/git-validation-y8p760la/native/native.malformed-module-before-publication/'
                      'repositories/git/trees/.stage-frEqCd/MODULE.bazel:1:1\n |\n |\n\n'),
    'tag': RECORDED_SELECTOR,
    'branch': RECORDED_SELECTOR,
    'submodules': RECORDED_SELECTOR,
    'commands': 'bazel-rs: patch_cmds are unsupported for materialization; no commands are executed\n',
    'prefix_symlink': 'bazel-rs: unsupported Git tree mode: symlinks and special entries are forbidden\n',
}

# Exact parsed official/native JSON from graph.pinned-first-commit. The underscore
# denotes an empty selected version, NOT the declared MODULE version or root version.
RECORDED_GRAPH = {
    'key': '<root>', 'name': 'opencode_gitcheck_root', 'version': '1.0',
    'apparentName': 'opencode_gitcheck_root', 'dependencies': [{
        'key': 'opencode_gitcheck_dep@_', 'name': 'opencode_gitcheck_dep', 'version': '7.0',
        'apparentName': 'gitdep', 'dependencies': [], 'indirectDependencies': [], 'cycles': [],
    }], 'indirectDependencies': [], 'cycles': [], 'root': True,
}
RECORDED_LEAF = {
    'key': 'opencode_gitcheck_leaf@2.0', 'name': 'opencode_gitcheck_leaf', 'version': '2.0',
    'apparentName': 'opencode_gitcheck_leaf', 'dependencies': [], 'indirectDependencies': [], 'cycles': [],
}
RECORDED_SOURCE_FILES = 'external/opencode_gitcheck_dep+/data\nexternal/opencode_gitcheck_dep+/MODULE.bazel\n'


class RecordedRegressionTests(unittest.TestCase):
    def test_committed_directory_link_prefix_rejection(self):
        # Captured after file-link support: the fixture's linked -> . prefix is
        # rejected from its committed blob before checkout can follow it.
        text = 'bazel-rs: invalid committed Git symlink "linked": directory target is unsupported: repository root\n'
        gitcheck.require_rejection(record(stderr=text, returncode=1), 'prefix_symlink')
        for wrong in [text.replace('directory target', 'file target'),
                      text.replace('invalid committed Git symlink', 'invalid cache'),
                      text + 'permission denied\n']:
            with self.assertRaises(ValueError):
                gitcheck.require_rejection(record(stderr=wrong, returncode=1), 'prefix_symlink')

    def test_captured_native_diagnostics_and_unrelated_failure_guards(self):
        selectors = {'tag', 'branch', 'submodules'}
        for diagnostic, message in RECORDED_DIAGNOSTICS.items():
            with self.subTest(diagnostic=diagnostic):
                gitcheck.require_rejection(record(stderr=message, returncode=1), diagnostic)
            # One captured selector message explicitly names all three options.
            for other in MESSAGES:
                if other == diagnostic or {diagnostic, other} <= selectors:
                    continue
                with self.subTest(diagnostic=diagnostic, other=other), self.assertRaises(ValueError):
                    gitcheck.require_rejection(record(stderr=message, returncode=1), other)
            for prefix in ('panicked at', 'SSLHandshake', 'failed to download MODULE.bazel',
                           'connection refused', 'authentication failed', 'permission denied',
                           'falling back', 'FATAL: crash', 'syntax error in BUILD.bazel',
                           'Parse error in unrelated/file.bzl'):
                with self.subTest(diagnostic=diagnostic, prefix=prefix), self.assertRaises(ValueError):
                    gitcheck.require_rejection(record(stderr=prefix + '\n' + message, returncode=1), diagnostic)
            for fields in ({'returncode': 0}, {'returncode': 101}, {'returncode': -9},
                           {'timed_out': True}, {'stdout': 'unexpected output'}, {'spawn_error': 'missing'}):
                with self.subTest(diagnostic=diagnostic, fields=fields), self.assertRaises(ValueError):
                    gitcheck.require_rejection(dict(record(stderr=message, returncode=1), **fields), diagnostic)

    def test_multiline_parse_requires_adjacent_module_location_and_specific_error(self):
        message = RECORDED_DIAGNOSTICS['module_syntax']
        for wrong in (message.replace('MODULE.bazel', 'BUILD.bazel'),
                      message.replace('MODULE.bazel', 'other.bzl') + '\nreading MODULE.bazel\n',
                      message.replace('Parse error: unexpected new line, expected expression', 'repository evaluation failed'),
                      message.replace('\n--> ', '\nunrelated context\n--> '),
                      'Parse error: unexpected new line, expected expression\nMODULE.bazel not found',
                      'MODULE.bazel\nerror: permission denied'):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                gitcheck.require_rejection(record(stderr=wrong, returncode=1), 'module_syntax')

    def test_actual_wording_does_not_accept_neighboring_failures(self):
        for diagnostic, wrong in (
                ('module_name', RECORDED_DIAGNOSTICS['module_name'].replace('MODULE.bazel', 'BUILD.bazel')),
                ('module_name', RECORDED_DIAGNOSTICS['module_name'].replace('opencode_gitcheck_wrong', 'unrelated')),
                ('tag', RECORDED_SELECTOR.replace('tags, ', '')),
                ('branch', RECORDED_SELECTOR.replace('branches and ', '')),
                ('submodules', RECORDED_SELECTOR.replace('init_submodules', 'extensions')),
                ('commands', RECORDED_DIAGNOSTICS['commands'].replace('patch_cmds', 'build_commands')),
                ('prefix_symlink', RECORDED_DIAGNOSTICS['prefix_symlink'].replace('Git tree mode', 'file permission'))):
            with self.subTest(diagnostic=diagnostic), self.assertRaises(ValueError):
                gitcheck.require_rejection(record(stderr=wrong, returncode=1), diagnostic)

    def test_recorded_graphs_still_require_explicit_full_tree_expectations(self):
        for name in ('graph.pinned-first-commit', 'graph.patched-module-dependency'):
            actual = copy.deepcopy(RECORDED_GRAPH)
            if name.endswith('patched-module-dependency'):
                actual['dependencies'][0]['dependencies'] = [copy.deepcopy(RECORDED_LEAF)]
            expected = gitcheck.graph_expected(item(name))
            self.assertEqual(actual, expected)
            for field, wrong in (('key', 'opencode_gitcheck_dep@'), ('key', 'opencode_gitcheck_dep@99.0'),
                                 ('version', ''), ('version', '99.0'), ('apparentName', 'wrong')):
                changed = copy.deepcopy(actual)
                changed['dependencies'][0][field] = wrong
                with self.subTest(name=name, field=field, wrong=wrong), self.assertRaises(ValueError):
                    gitcheck.compare_graphs(changed, changed, expected)
            changed = copy.deepcopy(actual)
            changed['version'] = ''
            with self.assertRaises(ValueError):
                gitcheck.compare_graphs(changed, changed, expected)

    def test_cquery_source_links_resolve_only_the_selected_reference_tree(self):
        for prefix in ('', 'nested/pkg'):
            with self.subTest(prefix=prefix), tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
                scratch = Path(temporary)
                output_base = scratch / 'git-output-base'
                logical = output_base / 'execroot/_main/external/opencode_gitcheck_dep+'
                publication = scratch / 'repository-contents/opaque-reference-key'
                selected = publication / '.tmp_git_root' / prefix if prefix else publication
                selected.mkdir(parents=True)
                expected = gitcheck.expected_sources(item('materialize.strip-prefix'))
                for name, contents in expected.items():
                    (selected / name).write_bytes(contents)
                    if prefix:
                        (publication / name).symlink_to(Path('.tmp_git_root') / prefix / name)
                if prefix:
                    (publication / '.tmp_git_root/.git').mkdir()
                    (publication / '.tmp_git_root/outside.txt').write_bytes(b'not selected\n')
                logical.parent.mkdir(parents=True)
                logical.symlink_to(publication, target_is_directory=True)
                paths = gitcheck.reference_output_paths(RECORDED_SOURCE_FILES, scratch / 'workspace', output_base)
                self.assertEqual({path.parent for path in paths}, {logical})
                # The first harness incorrectly applied this native-only rule to
                # the official execroot link, despite having read correct bytes.
                with self.assertRaisesRegex(ValueError, 'missing real materialized tree'):
                    gitcheck.tree_manifest(logical, expected)
                resolved = gitcheck.reference_source_root(paths, output_base)
                self.assertEqual(resolved, selected)
                self.assertTrue(gitcheck.manifest_matches(gitcheck.tree_manifest(resolved, expected), expected))
                self.assertTrue(gitcheck.manifest_matches(gitcheck.output_manifest(paths, expected, True), expected))
                (selected / 'data').write_bytes(b'wrong commit or patch\n')
                with self.assertRaises(ValueError):
                    gitcheck.tree_manifest(resolved, expected)
                (selected / 'data').write_bytes(expected['data'])
                (selected / '.git').mkdir()
                with self.assertRaises(ValueError):
                    gitcheck.tree_manifest(resolved, expected)

    def test_reference_links_reject_escape_wrong_identity_and_mixed_roots(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            scratch = Path(temporary)
            output_base = scratch / 'git-output-base'
            logical = output_base / 'execroot/_main/external/opencode_gitcheck_dep+'
            logical.mkdir(parents=True)
            paths = gitcheck.reference_output_paths(RECORDED_SOURCE_FILES, scratch / 'workspace', output_base)
            for path in paths:
                path.write_bytes(b'fixture\n')
            self.assertEqual(gitcheck.reference_source_root(paths, output_base), logical)
            for wrong in ([], paths + paths, [logical / 'wrong', paths[1]],
                          [logical.parent / 'wrong-repository/data', paths[1]]):
                with self.assertRaises(ValueError):
                    gitcheck.reference_source_root(wrong, output_base)
            for destination in (scratch / 'native/opaque-key', output_base / 'other-selected-root'):
                destination.mkdir(parents=True)
                (destination / 'data').write_bytes(b'fixture\n')
                paths[0].unlink()
                paths[0].symlink_to(destination / 'data')
                with self.assertRaisesRegex(ValueError, 'escape.*or selected directory'):
                    gitcheck.reference_source_root(paths, output_base)
                paths[0].unlink()
                paths[0].write_bytes(b'fixture\n')


class FixtureTests(unittest.TestCase):
    def test_inventory_graph_versions_and_defaults(self):
        self.assertEqual(len(gitcheck.CASES), 19)
        self.assertEqual(len(set(gitcheck.PLAN)), 19)
        self.assertEqual(sum(kind == 'differential' for _, kind in gitcheck.PLAN), 4)
        args = gitcheck.argument_parser().parse_args([])
        self.assertEqual(args.timeout, 180)
        self.assertEqual(args.output.name, 'git-validation.json')
        graph = gitcheck.graph_expected(item('graph.patched-module-dependency'))
        dep = graph['dependencies'][0]
        self.assertEqual((dep['key'], dep['version'], dep['apparentName']),
                         ('opencode_gitcheck_dep@_', '7.0', 'gitdep'))
        self.assertEqual(dep['dependencies'][0]['key'], 'opencode_gitcheck_leaf@2.0')

    def test_real_git_two_commits_literal_arguments_and_private_environment(self):
        with tempfile.TemporaryDirectory(prefix='git fixture ; ', dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            runner = gitcheck.RecordingRunner({'runs': []}, 5)
            previous = runner.env
            check = {}
            value = item('materialize.strip-prefix')
            fixture = gitcheck.materialize(root / 'case', root / 'workspace', value, runner, check)
            self.assertIs(runner.env, previous)
            self.assertNotEqual(check['first_commit'], check['second_commit'])
            for sha in (check['first_commit'], check['second_commit']):
                self.assertRegex(sha, r'^[0-9a-f]{40}$')
            runs = runner.report['runs']
            self.assertEqual(len(runs), 9)
            self.assertTrue(all(run['returncode'] == 0 and run['argv'][0] == '/usr/bin/git' for run in runs))
            self.assertTrue(all(run['timeout_seconds'] == 120 and not run['timed_out'] for run in runs))
            self.assertEqual(runs[7]['stdout'].encode(), gitcheck.data())
            self.assertEqual(runs[8]['stdout'].encode(), gitcheck.data('second'))
            self.assertEqual((fixture['source'] / 'data').read_bytes(), gitcheck.data('second'))
            module = (root / 'workspace/MODULE.bazel').read_text()
            self.assertIn('version = "99.0"', module)
            self.assertIn('commit = "' + check['first_commit'] + '"', module)
            self.assertNotIn(check['second_commit'], module)
            self.assertIn('strip_prefix = "nested/pkg"', module)
            self.assertFalse((fixture['registry'] / 'modules' / gitcheck.DEP).exists())
            self.assertFalse(fixture['absent_archive'].exists())
            metadata = json.loads((fixture['registry'] / 'modules/opencode_gitcheck_leaf/metadata.json').read_text())
            self.assertEqual(metadata['versions'], ['2.0'])
            self.assertTrue((fixture['origin'] / 'outside.txt').exists())
            self.assertEqual(check['git_environment']['PATH'], '/usr/bin:/bin')
            self.assertTrue(Path(check['git_environment']['HOME']).is_relative_to(root))
            self.assertNotIn('SSH_AUTH_SOCK', check['git_environment'])
            self.assertNotIn('GIT_CONFIG_PARAMETERS', check['git_environment'])
            self.assertTrue(all(gitcheck.evidence_matches(e) for e in gitcheck.snapshot(fixture['files']).values()))

    def test_patch_sections_are_explicit_exact_context_and_final_module(self):
        patch = gitcheck.source_patch()
        self.assertEqual([line for line in patch.splitlines() if line.startswith('diff --git')],
                         ['diff --git a/MODULE.bazel b/MODULE.bazel', 'diff --git a/data b/data'])
        self.assertIn('--- a/MODULE.bazel\n+++ b/MODULE.bazel', patch)
        self.assertIn('--- a/data\n+++ b/data', patch)
        self.assertIn('@@ -8,3 +8,3 @@\n header\n-first café\n+patched café\n footer\n', patch)
        self.assertIn('-# first café\n+# patched café\n', patch)
        self.assertEqual(gitcheck.expected_sources(item('materialize.module-source-once'))['MODULE.bazel'],
                         gitcheck.PATCHED_MODULE.encode())

    def test_environment_restored_on_exception_and_user_environment_untouched(self):
        original = dict(os.environ)
        runner = mock.Mock(env={'HOME': 'original', 'GIT_CONFIG_GLOBAL': 'original-poison'})
        before = runner.env
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            safe = gitcheck.private_environment(Path(temporary))
            with self.assertRaisesRegex(RuntimeError, 'injected'):
                with gitcheck.environment(runner, safe):
                    self.assertNotEqual(runner.env, before)
                    raise RuntimeError('injected')
        self.assertIs(runner.env, before)
        self.assertEqual(dict(os.environ), original)

    def test_setup_failure_restores_runner_environment(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            runner = mock.Mock(env={'HOME': 'untouched'})
            before = runner.env
            runner.run.side_effect = KeyboardInterrupt
            with self.assertRaises(KeyboardInterrupt):
                gitcheck.materialize(root / 'case', root / 'workspace', gitcheck.CASES[0], runner, {})
            self.assertIs(runner.env, before)

    def test_poison_is_private_inert_and_preservable(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            env, markers, payloads = gitcheck.poison_configuration(root / 'poison', root / 'origin')
            self.assertEqual(set(env), {'GIT_CONFIG_GLOBAL', 'GIT_SSH_COMMAND'})
            self.assertEqual(len(payloads), 5)
            config = Path(env['GIT_CONFIG_GLOBAL']).read_text()
            for text in ('insteadOf = ' + (root / 'origin').as_uri(), 'ssh://127.0.0.1:1/',
                         '[credential]', 'hooksPath', 'templateDir'):
                self.assertIn(text, config)
            self.assertTrue(all(not marker.exists() for marker in markers))
            evidence = gitcheck.snapshot(payloads)
            self.assertTrue(all(gitcheck.evidence_matches(value) for value in evidence.values()))


class ContractTests(unittest.TestCase):
    def test_rejections_specific_to_expected_class(self):
        for diagnostic, message in MESSAGES.items():
            good = record(stderr=message, returncode=1)
            gitcheck.require_rejection(good, diagnostic)
            for other in MESSAGES:
                if diagnostic != other:
                    with self.subTest(expected=other, actual=diagnostic), self.assertRaises(ValueError):
                        gitcheck.require_rejection(good, other)
            for change in ({'returncode': 0}, {'returncode': True}, {'returncode': 101},
                           {'returncode': -9}, {'returncode': 139}, {'returncode': None},
                           {'stdout': 'partial success'}, {'timed_out': True}, {'spawn_error': 'absent'},
                           {'interrupted': True}, {'stderr': 'unrelated registry missing'},
                           *({'stderr': prefix + '\n' + message} for prefix in (
                               'panicked at', 'SSLHandshake', 'failed to download MODULE.bazel',
                               'connection refused', 'authentication failed', 'falling back'))):
                with self.subTest(diagnostic=diagnostic, change=change), self.assertRaises(ValueError):
                    gitcheck.require_rejection(dict(good, **change), diagnostic)

    def test_generic_neighborhood_failures_are_not_semantic_rejections(self):
        for message in ('git fetch failed', 'fatal: bad repository', 'unsupported feature',
                        'MODULE.bazel evaluation failed', 'module name mismatch',
                        'patch failed', 'strip_prefix does not exist', 'syntax error in BUILD.bazel'):
            for diagnostic in MESSAGES:
                with self.subTest(message=message, diagnostic=diagnostic), self.assertRaises(ValueError):
                    gitcheck.require_rejection(record(stderr=message, returncode=1), diagnostic)

    def test_no_publication_counts_hidden_stages_only_in_owned_tree_cache(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            (root / 'unrelated').mkdir()
            gitcheck.no_publication(root)
            stage = gitcheck.trees_directory(root) / '.stage-checkout'
            stage.mkdir(parents=True)
            with self.assertRaises(ValueError):
                gitcheck.no_publication(root)

    def test_manifest_output_identity_and_git_metadata_rejected(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            for name, contents in gitcheck.expected_sources(gitcheck.CASES[0]).items():
                (root / name).write_bytes(contents)
            expected = gitcheck.expected_sources(gitcheck.CASES[0])
            paths = [root / name for name in expected]
            value = gitcheck.output_manifest(paths, expected, True)
            self.assertTrue(gitcheck.manifest_matches(value, expected))
            for wrong in ([], paths + paths, [paths[0], paths[0]]):
                with self.assertRaises(ValueError):
                    gitcheck.output_manifest(wrong, expected, True)
            text = ''.join(gitcheck.canonical('@gitdep//:sources') + '\t' + str(path) + '\n' for path in paths)
            self.assertEqual(gitcheck.native_paths(record(text), '@gitdep//:sources', root), paths)
            with self.assertRaises(ValueError):
                gitcheck.native_paths(record(text.replace('@@opencode_gitcheck_dep+', '@gitdep')), '@gitdep//:sources', root)
            (root / '.git').mkdir()
            with self.assertRaises(ValueError):
                gitcheck.tree_manifest(root, expected)

    def test_cli_flags_and_genuine_reference_cache_configuration(self):
        runner = mock.Mock()
        ref = gitcheck.PatchReference(Path('/official'), Path('/workspace'), Path('/scratch'), runner, 'git')
        ref.registry = Path('/private registry')
        ref.run('graph', 'mod')
        argv = runner.run.call_args.args[1]
        self.assertEqual(argv[argv.index('mod'):argv.index('mod') + 2], ['mod', 'graph'])
        for flag in ('--ignore_all_rc_files', '--lockfile_mode=off', '--repository_cache=/scratch/repository-cache',
                     '--repo_contents_cache=/scratch/repository-contents', '--output_user_root=/scratch/git-output-user-root',
                     '--output_base=/scratch/git-output-base', '--registry=https://bcr.bazel.build', *gitcheck.PROFILE):
            self.assertIn(flag, argv)
        for command in ('mod', 'query', 'build'):
            native = gitcheck.native_command(Path('/native'), Path('/cache'), ref.registry, command)
            self.assertEqual(native[:4], ['/native', '--ignore_all_rc_files', '--cache-dir=/cache', command])
            self.assertIn('--lockfile_mode=off', native)


class MainTests(unittest.TestCase):
    def run_main(self, directory, fault=None):
        """Only intercept non-Git subprocesses; real fixture commits prove provenance."""
        output = Path(directory) / 'report.json'
        original_run = Runner.run
        observations = []

        def run(runner, name, argv, cwd, timeout=None):
            if argv[0] == '/usr/bin/git':
                return original_run(runner, name, argv, cwd, timeout)
            cwd = Path(cwd)
            if name == 'reference.version':
                result = record('bazel 9.3.0\n' if fault != 'version' else 'bazel 9.2.0\n')
            elif name == 'git.shutdown':
                result = record(returncode=1 if fault == 'shutdown' else 0)
            else:
                check = runner.report['checks'][-1]
                value = check['input']
                engine, phase = name[len(value['name']) + 1:].split('.', 1)
                # Root fixture and provenance are inspected at the invocation boundary.
                module = (cwd / 'MODULE.bazel').read_text()
                cache = next((Path(arg.split('=', 1)[1]) for arg in argv if arg.startswith('--cache-dir=')), None)
                case_root = Path(runner.report['scratch_directory']) / 'cases' / value['name']
                origin = case_root / 'origin'
                observations.append(dict(name=value['name'], engine=engine, phase=phase,
                                         argv=list(argv), env=dict(runner.env), module=module,
                                         origin_exists=origin.exists(), cwd=str(cwd)))
                self.assertIn('version = "99.0"', module)
                self.assertIn('remote = "' + origin.as_uri() + '"', module)
                if value.get('leaf'):
                    self.assertIn('opencode_gitcheck_leaf', (cwd / 'git-local.patch').read_text())
                if not value.get('selector') and not value.get('unknown_commit'):
                    self.assertIn('commit = "' + check['first_commit'] + '"', module)
                self.assertFalse((case_root / 'registry/modules' / gitcheck.DEP).exists())
                if value.get('symlink'):
                    self.assertTrue((origin / 'linked').is_symlink())
                if value.get('bad_module') == 'name':
                    self.assertIn('opencode_gitcheck_wrong', (origin / 'MODULE.bazel').read_text())
                if value.get('bad_module') == 'syntax':
                    self.assertEqual((origin / 'MODULE.bazel').read_text(), 'module(name =\n')
                if value.get('selector') in ('tag', 'branch'):
                    self.assertNotIn('commit', check['override'])
                    self.assertEqual(check['override'][value['selector']], 'fixture-main')
                if value.get('selector') == 'abbreviated':
                    self.assertEqual(check['override']['commit'], check['first_commit'][:12])
                if value.get('submodules'):
                    self.assertIn('init_submodules = True', module)
                if value.get('commands'):
                    self.assertIn('PATCH_COMMAND_MUST_NOT_RUN', module)
                if value.get('external_patches'):
                    self.assertIn('@@opencode_gitcheck_dep+//:external.patch', module)
                if value['diagnostic'] in gitcheck.BEFORE_FETCH:
                    self.assertFalse(origin.exists())
                if fault == 'interrupt':
                    raise KeyboardInterrupt
                if fault == 'unexpected':
                    raise RuntimeError('injected exception')
                if fault == 'inputs':
                    (cwd / 'git-local.patch').write_bytes(b'mutated')
                if value['diagnostic']:
                    result = record(stderr=RECORDED_DIAGNOSTICS.get(value['diagnostic'], MESSAGES[value['diagnostic']]), returncode=1)
                    if fault == 'publication':
                        (gitcheck.trees_directory(cache) / '.leaked').mkdir(parents=True)
                elif value['mode'] == 'graph':
                    graph = copy.deepcopy(RECORDED_GRAPH)
                    if value.get('leaf'):
                        graph['dependencies'][0]['dependencies'] = [copy.deepcopy(RECORDED_LEAF)]
                    result = record(json.dumps(graph))
                    if engine == 'native':
                        tree = gitcheck.trees_directory(cache) / 'opaque-key'
                        tree.mkdir(parents=True, exist_ok=True)
                        for filename, contents in gitcheck.expected_sources(value).items():
                            (tree / filename).write_bytes(contents)
                elif phase == 'query':
                    prefix = '@@' + gitcheck.DEP + '+' if engine == 'native' else '@gitdep'
                    result = record(prefix + '//:copy\n' + prefix + '//:sources\n')
                else:
                    edited = value['mode'] == 'edit' and phase.startswith('again.')
                    if edited:
                        self.assertEqual((cwd / 'git-local.patch').read_text(), gitcheck.source_patch('edited'))
                    if value['mode'] == 'warm' and phase.startswith('again.'):
                        self.assertFalse(origin.exists())
                    expected = gitcheck.expected_sources(value, edited)
                    source = 'sources' in phase
                    if engine == 'native':
                        tree = gitcheck.trees_directory(cache) / ('edited-opaque-key' if edited else 'opaque-key')
                        if value['mode'] == 'repair' and phase == 'again.sources':
                            self.assertEqual((tree / 'data').read_bytes(), b'corrupt cache probe\n')
                        base = cache
                    else:
                        base = Path(next(a.split('=', 1)[1] for a in argv if a.startswith('--output_base='))) / 'execroot/_main'
                        tree = base / 'external' / (gitcheck.DEP + '+')
                        # Reproduce official execroot + strip_prefix source links,
                        # rather than pretending cquery's logical root is a real dir.
                        publication = Path(runner.report['scratch_directory']) / 'repository-contents' / value['name']
                        selected = publication / '.tmp_git_root/nested/pkg' if value.get('prefix') else publication
                        selected.mkdir(parents=True, exist_ok=True)
                        for filename, contents in expected.items():
                            (selected / filename).write_bytes(contents)
                            if selected != publication and not (publication / filename).is_symlink():
                                (publication / filename).symlink_to(Path('.tmp_git_root/nested/pkg') / filename)
                        if selected != publication:
                            (publication / '.tmp_git_root/outside.txt').write_bytes(b'unselected checkout file\n')
                            (publication / '.tmp_git_root/.git').mkdir(exist_ok=True)
                        tree.parent.mkdir(parents=True, exist_ok=True)
                        if tree.is_symlink():
                            tree.unlink()
                        tree.symlink_to(publication, target_is_directory=True)
                    paths = ([tree / filename for filename in expected] if source else
                             [base / 'bazel-out/k8-fastbuild/bin/external' / (gitcheck.DEP + '+') / 'copy.txt'])
                    for path in paths:
                        path.parent.mkdir(parents=True, exist_ok=True)
                        path.write_bytes(expected[path.name] if source else expected['data'])
                        if fault == 'bytes':
                            path.write_bytes(b'wrong\n')
                    if engine == 'native':
                        result = record(''.join(gitcheck.canonical('@gitdep//:' + ('sources' if source else 'copy'))
                                                + '\t' + str(path) + '\n' for path in paths))
                    else:
                        result = record(''.join(path.relative_to(base).as_posix() + '\n' for path in paths)
                                        if phase.endswith('.files') else '')
                if value['mode'] == 'poison':
                    self.assertTrue(runner.env['GIT_CONFIG_GLOBAL'].endswith('poison.gitconfig'))
                    if fault == 'poison':
                        Path(runner.env['GIT_CONFIG_GLOBAL']).with_name('HOOK_RAN').write_bytes(b'bad')
                elif engine == 'reference':
                    self.assertNotIn('GIT_SSH_COMMAND', runner.env)
                    self.assertEqual(Path(runner.env['GIT_CONFIG_GLOBAL']).read_bytes(), b'')
                if fault == 'reference' and engine == 'reference':
                    result = record(stderr='failed to download genuine built-in', returncode=1)
                if fault == 'retry' and phase.startswith('again.'):
                    raise RuntimeError('injected during cache probe')
            result.update(id=len(runner.report['runs']), name=name, argv=list(argv), cwd=str(cwd),
                          timeout_seconds=runner.timeout if timeout is None else timeout)
            runner.report['runs'].append(result)
            return result

        evidence = {'native': {'sha256': 'synthetic-native'}, 'reference': {'sha256': gitcheck.REFERENCE_SHA256}}
        with mock.patch.object(gitcheck, 'validate_binaries', return_value=evidence), \
                mock.patch.object(Runner, 'run', new=run), mock.patch('builtins.print'):
            code = gitcheck.main(['--scratch-parent', directory, '--output', str(output)])
        report = json.loads(output.read_text())
        self.assertTrue(report['scratch_cleaned'])
        self.assertTrue(report['reference_shutdown_attempted'])
        self.assertTrue(report['sources_unchanged'])
        self.assertFalse(Path(report['scratch_directory']).exists())
        return code, report, observations

    def test_main_exact_wiring_report_and_tamper_revalidation(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            code, report, observations = self.run_main(temporary)
        self.assertEqual(code, 0, [(c['name'], c.get('error')) for c in report['checks'] if c['status'] != 'PASS'])
        self.assertTrue(gitcheck.report_passes(report))
        self.assertEqual(report['completed_checks'], 19)
        self.assertEqual(report['completed_differential_cases'], 4)
        self.assertEqual(sum(r['argv'][0] == '/usr/bin/git' for r in report['runs']), 171)
        self.assertEqual(len({o['cwd'] for o in observations}), 1)
        self.assertTrue(report['build_commands_executed'])
        self.assertFalse(report['test_commands_executed'])
        for observation in observations:
            if observation['name'].startswith('native.'):
                self.assertEqual(observation['engine'], 'native')
        for malformed in ({}, None, [], {'checks': []}):
            self.assertFalse(gitcheck.report_passes(malformed))
        for mutation in ('case-count', 'plan', 'kind', 'parity', 'status', 'input', 'files', 'run-id', 'run-count',
                         'stdout-hash', 'graph', 'scratch', 'shutdown', 'shutdown-exit', 'version-run', 'pin',
                         'first-commit', 'head-commit', 'git-show', 'git-executable', 'counts', 'artifact',
                         'publication', 'cache-key', 'edited-patch', 'corrupt', 'poison', 'lockfile', 'cache-dir',
                         'argv-command', 'argv-cwd', 'artifact-path', 'git-show-argv', 'patch-bytes', 'root-bytes',
                         'reference-root-escape', 'reference-tree-bytes', 'reference-tree-path'):
            changed = copy.deepcopy(report)
            first = changed['checks'][0]
            find = lambda mode: next(c for c in changed['checks'] if c['input']['mode'] == mode)
            if mutation == 'case-count':
                changed['checks'].pop()
            elif mutation == 'plan':
                changed['planned_checks'] = []
            elif mutation == 'kind':
                first['kind'] = 'native_only'
            elif mutation == 'parity':
                first['parity_claim'] = False
            elif mutation == 'status':
                first['status'] = 'FAIL'
            elif mutation == 'input':
                first['input']['leaf'] = True
            elif mutation == 'files':
                first['files_after'] = {}
            elif mutation == 'run-id':
                first['run_ids']['native.graph'] = -1
            elif mutation == 'run-count':
                first['run_ids'].pop('reference.graph')
            elif mutation == 'stdout-hash':
                changed['runs'][0]['stdout_sha256'] = 'bad'
            elif mutation == 'graph':
                r = changed['runs'][first['run_ids']['native.graph']]
                r['stdout'] = '{}'
                r['stdout_sha256'] = gitcheck.digest(b'{}')
            elif mutation in ('scratch', 'shutdown'):
                changed['scratch_cleaned' if mutation == 'scratch' else 'reference_shutdown_attempted'] = False
            elif mutation == 'shutdown-exit':
                changed['runs'][-1]['returncode'] = 1
            elif mutation == 'version-run':
                changed['runs'][0]['returncode'] = 1
            elif mutation == 'pin':
                changed['executables']['reference']['sha256'] = 'wrong'
            elif mutation == 'first-commit':
                first['first_commit'] = 'a' * 40
            elif mutation == 'head-commit':
                first['override']['commit'] = first['second_commit']
            elif mutation == 'git-show':
                r = changed['runs'][first['git_setup_run_ids']['show-first']]
                r['stdout'] = gitcheck.data('second').decode()
                r['stdout_sha256'] = gitcheck.digest(r['stdout'].encode())
            elif mutation == 'git-executable':
                changed['runs'][first['git_setup_run_ids']['init']]['argv'][0] = '/fake/git'
            elif mutation == 'counts':
                changed['completed_checks'] = 18
            elif mutation == 'artifact':
                find('source')['artifacts']['native.copy']['copy.txt']['bytes_base64'] = ''
            elif mutation == 'publication':
                next(c for c in changed['checks'] if c['input']['diagnostic'])['no_publication'] = False
            elif mutation == 'cache-key':
                find('edit')['cache_root_after'] = find('edit')['cache_root_before']
            elif mutation == 'edited-patch':
                find('edit')['edited_patch']['bytes_base64'] = ''
            elif mutation == 'corrupt':
                find('repair')['corrupt_before']['bytes_base64'] = ''
            elif mutation == 'poison':
                find('poison')['poison_markers_absent'] = False
            elif mutation in ('lockfile', 'cache-dir'):
                r = changed['runs'][first['run_ids']['native.graph']]
                prefix = '--lockfile_mode=' if mutation == 'lockfile' else '--cache-dir='
                r['argv'] = [arg for arg in r['argv'] if not arg.startswith(prefix)]
            elif mutation == 'argv-command':
                r = changed['runs'][first['run_ids']['native.graph']]
                r['argv'][3] = 'query'
            elif mutation == 'argv-cwd':
                changed['runs'][first['run_ids']['native.graph']]['cwd'] = '/not-owned'
            elif mutation == 'artifact-path':
                find('source')['artifacts']['native.copy']['copy.txt']['path'] = '/outside/copy.txt'
            elif mutation == 'git-show-argv':
                changed['runs'][first['git_setup_run_ids']['show-first']]['argv'][-1] = 'HEAD:data'
            elif mutation in ('patch-bytes', 'root-bytes'):
                check = find('source')
                filename = 'git-local.patch' if mutation == 'patch-bytes' else 'MODULE.bazel'
                path = str(Path(changed['scratch_directory']) / 'workspace' / filename)
                for field in ('files_before', 'files_after'):
                    check[field][path].update(bytes_base64='', size_bytes=0, sha256=gitcheck.digest(b''))
            elif mutation == 'reference-root-escape':
                find('source')['reference_source_root'] = str(Path(changed['scratch_directory']) / 'native')
            elif mutation == 'reference-tree-bytes':
                find('source')['reference_source_manifest']['data']['bytes_base64'] = ''
            elif mutation == 'reference-tree-path':
                find('source')['reference_source_manifest']['data']['path'] = '/outside/data'
            with self.subTest(mutation=mutation):
                self.assertFalse(gitcheck.report_passes(changed))

    def test_failures_and_exceptions_restore_inputs_and_always_cleanup(self):
        for fault in ('reference', 'interrupt', 'unexpected', 'version', 'shutdown', 'inputs',
                      'bytes', 'publication', 'poison', 'retry'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
                code, report, _ = self.run_main(temporary, fault)
                self.assertEqual(code, 130 if fault == 'interrupt' else 1)
                self.assertEqual(report['status'], 'FAIL')
                self.assertFalse(gitcheck.report_passes(report))
                for check in report['checks']:
                    if fault == 'retry' and check['input']['mode'] == 'warm':
                        self.assertTrue(check['inputs_unchanged'])
                    if fault == 'poison' and check['input']['mode'] == 'poison':
                        self.assertTrue(check['environment_restored'])
                        self.assertFalse(check['poison_markers_absent'])
                        self.assertEqual(len(check['poison_payloads']), 5)

    def test_cache_edit_exception_restores_exact_patch(self):
        # A direct case call lets us inspect the private input before cleanup.
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            report = {'checks': [], 'runs': []}
            runner = gitcheck.RecordingRunner(report, 5)
            runner.env = gitcheck.private_environment(root / 'environment')
            original = Runner.run

            def run(self, name, argv, cwd, timeout=None):
                if argv[0] == '/usr/bin/git':
                    return original(self, name, argv, cwd, timeout)
                if name.endswith('.query'):
                    return record('@@opencode_gitcheck_dep+//:copy\n@@opencode_gitcheck_dep+//:sources\n')
                if '.again.' in name:
                    raise RuntimeError('during edit')
                tree = gitcheck.trees_directory(root / 'native/native.patch-edit-immutable') / 'old-key'
                tree.mkdir(parents=True, exist_ok=True)
                expected = gitcheck.expected_sources(item('native.patch-edit-immutable'))
                for filename, contents in expected.items():
                    (tree / filename).write_bytes(contents)
                if name.endswith('.sources'):
                    return record(''.join('@@opencode_gitcheck_dep+//:sources\t' + str(tree / filename) + '\n' for filename in expected))
                out = root / 'bazel-out/k8-fastbuild/bin/external/opencode_gitcheck_dep+/copy.txt'
                out.parent.mkdir(parents=True)
                out.write_bytes(expected['data'])
                return record('@@opencode_gitcheck_dep+//:copy\t' + str(out) + '\n')

            with mock.patch.object(Runner, 'run', new=run), self.assertRaisesRegex(RuntimeError, 'during edit'):
                gitcheck.run_case(mock.Mock(native=Path('/native')), runner, root, root / 'workspace',
                                  mock.Mock(), item('native.patch-edit-immutable'))
            self.assertEqual((root / 'workspace/git-local.patch').read_text(), gitcheck.source_patch())
            self.assertTrue(report['checks'][0]['inputs_unchanged'])

    def test_missing_binary_and_protected_outputs_never_spawn(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            output = Path(temporary) / 'report.json'
            with mock.patch.object(Runner, 'run', side_effect=AssertionError('must not spawn')), mock.patch('builtins.print'):
                code = gitcheck.main(['--native', temporary + '/absent', '--output', str(output)])
            self.assertEqual(code, 1)
            self.assertEqual(json.loads(output.read_text())['runs'], [])
        for path in (gitcheck.GIT, Path(gitcheck.__file__), gitcheck.ROOT / 'tests/test_validate_git.py'):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, 'must not overwrite'):
                gitcheck.main(['--output', str(path)])

    def test_cleanup_failure_is_retained_as_failure(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            output = Path(temporary) / 'report.json'
            original = gitcheck.shutil.rmtree

            def fail_cleanup(path):
                original(path)
                raise OSError('injected cleanup failure')

            with mock.patch.object(gitcheck, 'validate_binaries', return_value={}), \
                    mock.patch.object(Runner, 'run', side_effect=RuntimeError('no Bazel subprocess')), \
                    mock.patch.object(gitcheck.shutil, 'rmtree', side_effect=fail_cleanup), mock.patch('builtins.print'):
                self.assertEqual(gitcheck.main(['--scratch-parent', temporary, '--output', str(output)]), 1)
            report = json.loads(output.read_text())
            self.assertFalse(report['scratch_cleaned'])
            self.assertTrue(any('scratch cleanup' in error for error in report['errors']))


if __name__ == '__main__':
    unittest.main()
