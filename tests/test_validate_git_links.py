"""Harness-only tests: private real Git, synthetic Bazel, NO native/Bazel execution."""

import base64
import copy
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_git_links as links
from validate import Runner


def item(name):
    return next(value for value in links.CASES if value['name'] == name)


def record(stdout='', stderr='', returncode=0, **extra):
    return dict(id=0, name='synthetic', stdout=stdout, stderr=stderr,
                returncode=returncode, timed_out=False, **extra)


# Contract examples only. These are intentionally NOT claimed as captured stderr.
MESSAGES = {
    'absolute': 'Git symlink e2e/smoke/.bazelversion has an absolute target',
    'escape': 'Git symlink e2e/smoke/.bazelversion escapes repository root',
    'dangling': 'Git symlink e2e/smoke/.bazelversion has a dangling target',
    'cycle': 'Git symlink e2e/smoke/.bazelversion has a cycle',
    'directory': 'Git symlink e2e/smoke/.bazelversion has a directory target',
    'module_link': 'MODULE.bazel symlink is unsupported',
    'malformed_target': 'Git committed symlink malformedlink has an unsafe target containing a NUL control byte',
}


# Verbatim retained first-run stderr, artifacts/git-links-validation-initial-failure.json.
# Native SHA256 eb9b5f2cf26d281ea7fd3cef43aba468cbd2c1b2aeb54082011ebd6002488992.
# No runtime dependency on that report or its removed private scratch tree.
CAPTURED = {
    'module_link': 'bazel-rs: Git file is not regular (symlinks/special files forbidden): /tmp/opencode/git-links-validation-hbpmlh83/native/native.root-module-link/repositories/git/trees/.stage-SkXhqh/MODULE.bazel\n',
}
# Retired with N19 (docs/ARCHIVE_SYMLINKS.md): native no longer rejects a patched
# link leaf ('unsafe patch symlink', the former 'patch_link' class) or a source
# artifact through an internal link (the former uncaptured 'source_link'
# contract); genuine Bazel builds both, so both phases now require success.
# Retired with N26 (docs/REPOSITORY_RULES.md): a .bzl loaded through an
# internal link (the former captured 'bzl_link' class); Bazel loads it too.
RETIRED_REJECTIONS = (
    'bazel-rs: symlink load sources are unsupported: @@opencode_gitcheck_dep+//bzl:native.bzl (/tmp/opencode/git-links-validation-hbpmlh83/native/native.selected-consumer-boundaries/repositories/git/trees/34daeec2a87e5feababbec9d39617960b7f65f9f7ffdaafd222b8e136ca08049/bzl/native.bzl)\n',
    'bazel-rs: cannot apply patch //:git-local.patch: unsafe patch symlink: "/tmp/opencode/git-links-validation-hbpmlh83/native/native.patch-symlink-leaf/repositories/git/trees/.stage-ts1Jlm/e2e/smoke/.bazelversion"\n',
    'bazel-rs: source symlinks are not supported: /private/git/tree/artifact/link\n',
)


class CapturedDiagnosticTests(unittest.TestCase):
    def test_captured_committed_nul_target_forbidden_character(self):
        message = 'bazel-rs: invalid committed Git symlink "malformedlink": symlink target contains a forbidden character: "data\\0../outside"\n'
        links.require_rejection(record(stderr=message, returncode=1), 'malformed_target')
        for wrong in [message.replace('malformedlink', 'another-link'),
                      message.replace('symlink target contains a forbidden character', 'file not found'),
                      message + 'permission denied\n']:
            with self.assertRaises(ValueError):
                links.require_rejection(record(stderr=wrong, returncode=1), 'malformed_target')

    def test_captured_diagnostics_match_only_their_own_classes(self):
        for diagnostic, text in CAPTURED.items():
            links.require_rejection(record(stderr=text, returncode=1), diagnostic)
            for other in MESSAGES:
                if other != diagnostic:
                    with self.subTest(actual=diagnostic, expected=other), self.assertRaises(ValueError):
                        links.require_rejection(record(stderr=text, returncode=1), other)

    def test_captured_diagnostics_reject_nearby_paths_modules_and_split_lines(self):
        wrong = {
            'module_link': [CAPTURED['module_link'].replace('/MODULE.bazel', '/' + other)
                            for other in ('BUILD.bazel', 'OTHER_MODULE.bazel', 'MODULE.bazel.backup', 'native.bzl')]
                           + [CAPTURED['module_link'].replace('forbidden): ', 'forbidden):\n'),
                              CAPTURED['module_link'].replace('Git file', 'Archive file')],
        }
        for diagnostic, examples in wrong.items():
            for text in examples:
                with self.subTest(diagnostic=diagnostic, text=text), self.assertRaises(ValueError):
                    links.require_rejection(record(stderr=text, returncode=1), diagnostic)

    def test_captured_diagnostics_still_reject_unrelated_failures_and_bad_exits(self):
        for diagnostic, text in CAPTURED.items():
            for prefix in ('permission denied', 'could not resolve host', 'connection refused',
                           'syntax error', 'parse error', 'unexpected token', 'fatal: failed', 'panicked'):
                with self.subTest(diagnostic=diagnostic, prefix=prefix), self.assertRaises(ValueError):
                    links.require_rejection(record(stderr=prefix + '\n' + text, returncode=1), diagnostic)
            for change in ({'returncode': 0}, {'returncode': 101}, {'timed_out': True}, {'stdout': 'partial graph'}):
                with self.assertRaises(ValueError):
                    links.require_rejection(dict(record(stderr=text, returncode=1), **change), diagnostic)

    def test_retired_link_rejections_no_longer_satisfy_any_rejection_class(self):
        self.assertNotIn('patch_link', links.DIAGNOSTICS)
        self.assertNotIn('source_link', links.DIAGNOSTICS)
        self.assertNotIn('bzl_link', links.DIAGNOSTICS)
        native = '@gitdep//bzl:selected\n'
        canonical = '@@' + links.shared.DEP + '+//bzl:selected\n'
        links.bzl_result(record(native), native=True)
        links.bzl_result(record(canonical))
        for wrong, is_native in ((record(canonical), True), (record(''), False),
                                 (record(native + native), True),
                                 (record(stderr=RETIRED_REJECTIONS[0], returncode=1), True)):
            with self.assertRaises(ValueError):
                links.bzl_result(wrong, native=is_native)
        self.assertIsNone(item('native.patch-symlink-leaf')['diagnostic'])
        for text in RETIRED_REJECTIONS:
            for diagnostic in links.DIAGNOSTICS:
                with self.subTest(text=text, diagnostic=diagnostic), self.assertRaises(ValueError):
                    links.require_rejection(record(stderr=text, returncode=1), diagnostic)

    def test_patched_link_leaf_layout_is_a_regular_file_and_referent_is_unchanged(self):
        files = links.layout(item('native.patch-symlink-leaf'), Path('/unused'), patched=True)
        self.assertEqual(files[links.LINK], ('file', links.PATCHED_VERSION))
        self.assertEqual(files['.bazelversion'], ('file', links.VERSION))
        self.assertEqual(links.layout(item('native.patch-symlink-leaf'), Path('/unused'))[links.LINK],
                         ('symlink', links.RAW.encode()))

    def test_linked_source_artifact_identity_and_bytes(self):
        value = item('native.selected-consumer-boundaries')
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            scratch = Path(temporary)
            output_base = scratch / 'git-links-output-base'
            tree = links.shared.trees_directory(scratch / 'native' / value['name']) / 'opaque'
            write_tree(tree, links.layout(value, scratch, patched=True))
            native = links.artifact_manifest('native', [tree / 'data'], scratch, value, output_base)
            self.assertTrue(links.evidence_matches(native['link'], links.shared.data()))
            reference = output_base / 'execroot/_main/external' / (links.shared.DEP + '+')
            write_tree(reference, links.layout(value, scratch, patched=True))
            evidence = links.artifact_manifest('reference', [reference / 'artifact/link'], scratch, value, output_base)
            self.assertTrue(links.evidence_matches(evidence['link'], links.shared.data()))
            for engine, paths in (('native', [tree / 'artifact/link']), ('native', [tree / 'data', tree / 'MODULE.bazel']),
                                  ('native', [reference / 'data']), ('reference', [reference / 'data']),
                                  ('reference', [tree / 'data']), ('native', [])):
                with self.subTest(engine=engine, paths=paths), self.assertRaises(ValueError):
                    links.artifact_manifest(engine, paths, scratch, value, output_base)
            (tree / 'data').write_bytes(b'other bytes\n')
            with self.assertRaises(ValueError):
                links.artifact_manifest('native', [tree / 'data'], scratch, value, output_base)


def write_tree(root, files):
    root.mkdir(parents=True, exist_ok=True)
    for name, (kind, raw) in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink() or path.exists():
            path.unlink()
        if kind == 'symlink':
            os.symlink(os.fsdecode(raw), path)
        else:
            path.write_bytes(raw)


class FixtureTests(unittest.TestCase):
    def test_plan_defaults_and_scoped_cases(self):
        self.assertEqual(len(links.CASES), 19)
        self.assertEqual(len(set(links.PLAN)), 19)
        self.assertEqual(sum(kind == 'differential' for _, kind in links.PLAN), 4)
        args = links.argument_parser().parse_args([])
        self.assertEqual(args.timeout, 180)
        self.assertEqual(args.output, links.ROOT / 'artifacts/git-links-validation.json')
        self.assertEqual(len(links.RAW.encode()), 19)
        self.assertEqual(links.REFERENCE_SHA256, 'd302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8')
        for imported in ('validate.py', 'validate_modules.py', 'validate_patches.py', 'validate_git.py'):
            self.assertIn(links.ROOT / 'scripts' / imported, links.SOURCES)
        self.assertIn('native.nonexistent-intermediate-dotdot', dict(links.PLAN))
        self.assertEqual(links.phases(item('native.selected-consumer-boundaries'))[-2:], ['bzl', 'artifact'])
        self.assertEqual(links.phases(item('native.reject-committed-nul-target')), ['result'])

    def test_real_git_commits_all_four_positive_layouts_with_120000_links(self):
        for value in links.CASES[:4]:
            with self.subTest(case=value['name']), tempfile.TemporaryDirectory(prefix='links ; ', dir='/tmp/opencode') as temporary:
                root = Path(temporary)
                runner = links.RecordingRunner({'runs': []}, 5)
                previous = runner.env
                check = {}
                fixture = links.materialize(root / 'case', root / 'workspace', value, runner, check)
                self.assertIs(runner.env, previous)
                self.assertEqual(len(runner.report['runs']), 6)
                self.assertTrue(all(r['argv'][0] == '/usr/bin/git' and r['returncode'] == 0 for r in runner.report['runs']))
                self.assertRegex(check['commit'], r'^[0-9a-f]{40}$')
                expected = links.expected_tracked(links.full_layout(value, root / 'case'))
                index = links.tracked_entries(runner.report['runs'][-2]['stdout'])
                tree = links.tracked_entries(runner.report['runs'][-1]['stdout'], True)
                self.assertEqual(index, expected)
                self.assertEqual(tree, expected)
                prefix = value.get('prefix', '')
                selected = fixture['origin'] / prefix
                entry = links.entry_evidence(selected / links.LINK)
                self.assertEqual(entry['kind'], 'symlink')
                self.assertTrue(stat.S_ISLNK((selected / links.LINK).lstat().st_mode))
                self.assertEqual(index[(prefix + '/' if prefix else '') + links.LINK][0], '120000')
                if value.get('shape') != 'chain':
                    self.assertEqual(entry['target_size_bytes'], 19)
                    self.assertEqual(os.readlink(selected / links.LINK), links.RAW)
                self.assertEqual((selected / links.LINK).read_bytes(), links.VERSION)
                if value.get('shape') == 'build':
                    self.assertTrue((selected / 'BUILD.bazel').is_symlink())
                    self.assertEqual((selected / 'BUILD.bazel').read_bytes(), links.shared.BUILD.encode())
                self.assertIn(check['commit'], (root / 'workspace/MODULE.bazel').read_text())
                self.assertEqual(check['override']['remote'], fixture['origin'].as_uri())
                self.assertFalse((fixture['registry'] / 'modules').exists())
                self.assertNotIn('SSH_AUTH_SOCK', check['git_environment'])
                self.assertEqual(check['git_environment']['PATH'], '/usr/bin:/bin')

    def test_malicious_snapshot_no_follow_and_cleanup_cannot_touch_outside_sibling(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            sentinel = outside / 'sentinel'
            sentinel.write_bytes(links.OUTSIDE)
            origin = root / 'owned-origin'
            origin.mkdir()
            os.symlink(sentinel, origin / 'absolute')
            os.symlink(outside, origin / 'directory')
            os.symlink('not-there/../regular', origin / 'trap')
            os.symlink('cycle', origin / 'cycle')
            (origin / 'regular').write_bytes(b'known regular\n')
            real_open = os.open
            opened = []

            def guarded_open(path, flags, *args, **kwargs):
                self.assertFalse(Path(path).is_symlink(), 'inventory attempted to open a link')
                self.assertNotEqual(Path(path), sentinel, 'inventory followed an outside target')
                self.assertTrue(flags & os.O_NOFOLLOW)
                opened.append(Path(path))
                return real_open(path, flags, *args, **kwargs)

            with mock.patch.object(os, 'open', side_effect=guarded_open), \
                    mock.patch.object(Path, 'read_bytes', side_effect=AssertionError('following read forbidden')):
                manifest = links.inventory(origin)
            self.assertEqual(opened, [origin / 'regular'])
            self.assertEqual(set(manifest), {'absolute', 'directory', 'trap', 'cycle', 'regular'})
            self.assertEqual(manifest['absolute']['raw_target'], str(sentinel))
            self.assertNotIn('bytes_base64', manifest['absolute'])
            self.assertTrue(all(links.valid_entry(e) for e in manifest.values()))
            shutil.rmtree(origin)
            self.assertEqual(sentinel.read_bytes(), links.OUTSIDE)

    def test_unsafe_git_fixtures_still_track_real_links_without_following(self):
        for name in ('native.reject-absolute', 'native.reject-escape', 'native.reject-cycle',
                     'native.nonexistent-intermediate-dotdot', 'native.prefix-containment'):
            with self.subTest(case=name), tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
                root = Path(temporary)
                value = item(name)
                runner = links.RecordingRunner({'runs': []}, 5)
                fixture = links.materialize(root / 'case', root / 'workspace', value, runner, {})
                state = links.inputs_snapshot(fixture)
                self.assertTrue(links.manifest_matches(state['origin'], links.full_layout(value, root / 'case'), fixture['origin']))
                path = fixture['origin'] / value.get('prefix', '') / links.LINK
                self.assertTrue(path.is_symlink())
                if value.get('shape') == 'prefix_escape':
                    self.assertEqual(path.read_bytes(), b'outside selected prefix\n')
                    selected = root / 'selected'
                    write_tree(selected, links.layout(value, root / 'case'))
                    self.assertFalse((selected / links.LINK).exists())
                if value.get('shape') == 'trap':
                    self.assertTrue((fixture['origin'] / '.bazelversion').is_file())
                    with self.assertRaises(FileNotFoundError):
                        path.read_bytes()  # OS semantics, not a lexical normalization.
                self.assertEqual(fixture['marker'].read_bytes(), links.OUTSIDE)

    def test_patch_inputs_and_selected_boundaries_are_not_fake_builtins(self):
        self.assertIn('--- a/.bazelversion\n+++ b/.bazelversion', links.patch_contents(item('native.patch-regular-referent')))
        self.assertIn('+++ /dev/null', links.patch_contents(item('native.patch-delete-referent')))
        self.assertIn('--- a/' + links.LINK, links.patch_contents(item('native.patch-symlink-leaf')))
        files = links.layout(item('native.selected-consumer-boundaries'), Path('/private'))
        self.assertEqual(files['bzl/native.bzl'], ('symlink', b'../defs.real.bzl'))
        self.assertEqual(files['artifact/link'], ('symlink', b'../data'))
        self.assertIn(b'load(":native.bzl"', files['bzl/BUILD.bazel'][1])
        self.assertIn(b'srcs = ["link"]', files['artifact/BUILD.bazel'][1])
        self.assertEqual(files['MODULE.bazel'][0], 'file')


class MalformedBlobTests(unittest.TestCase):
    def test_real_git_nul_blob_is_index_only_exact_and_pinned_after_base(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            value = item('native.reject-committed-nul-target')
            runner = links.RecordingRunner({'runs': []}, 5)
            check = {}
            real_symlink = os.symlink

            def no_nul_os_link(target, path, *args, **kwargs):
                self.assertNotIn(b'\0', os.fsencode(target))
                self.assertNotEqual(Path(path).name, links.MALFORMED_LINK)
                return real_symlink(target, path, *args, **kwargs)

            with mock.patch.object(os, 'symlink', side_effect=no_nul_os_link):
                fixture = links.materialize(root / 'case', root / 'workspace', value, runner, check)
            self.assertEqual(len(runner.report['runs']), 15)
            self.assertNotEqual(check['base_commit'], check['commit'])
            self.assertEqual(check['override']['commit'], check['commit'])
            self.assertIn(check['commit'], (root / 'workspace/MODULE.bazel').read_text())
            self.assertNotIn(check['base_commit'], (root / 'workspace/MODULE.bazel').read_text())
            self.assertFalse(os.path.lexists(fixture['origin'] / links.MALFORMED_LINK))
            self.assertFalse((fixture['origin'] / links.MALFORMED_PAYLOAD).exists())
            self.assertTrue(check['malformed_payload_removed'])
            self.assertTrue(check['malformed_link_worktree_absent'])
            self.assertEqual((fixture['origin'] / 'data').read_bytes(), links.shared.data())
            self.assertEqual(check['malformed_blob'], links.malformed_blob_evidence(b'data\0../outside'))
            self.assertEqual(base64.b64decode(check['malformed_blob']['bytes_base64']), b'data\0../outside')
            self.assertEqual(check['malformed_blob']['git_mode'], '120000')
            self.assertTrue(links.valid_entry(check['malformed_payload'], 'file', links.MALFORMED_TARGET))
            records = {phase: runner.report['runs'][index] for phase, index in check['git_setup_run_ids'].items()}
            self.assertEqual(records['parent-malformed']['stdout'].strip(), check['base_commit'])
            self.assertEqual(records['blob-bytes']['stdout'].encode(), links.MALFORMED_TARGET)
            self.assertEqual(records['blob-size']['stdout'].strip(), str(len(links.MALFORMED_TARGET)))
            for phase in ('index-malformed', 'tree-malformed'):
                tracked = links.tracked_entries(records[phase]['stdout'], phase == 'tree-malformed')
                self.assertEqual(tracked, links.expected_tracked(links.committed_layout(value, root / 'case')))
                self.assertEqual(tracked[links.MALFORMED_LINK], ('120000', links.blob_oid(links.MALFORMED_TARGET)))
                self.assertNotEqual(tracked[links.MALFORMED_LINK][1], links.blob_oid(b'data'))
            for phase in ('index', 'tree'):
                self.assertNotIn(links.MALFORMED_LINK, links.tracked_entries(records[phase]['stdout'], phase == 'tree'))
            before = links.inputs_snapshot(fixture)
            self.assertEqual(len(before['git_inputs']), 6)
            self.assertTrue(all(links.valid_entry(e, 'file') for e in before['git_inputs'].values()))
            self.assertEqual(before, links.inputs_snapshot(fixture))
            self.assertEqual(fixture['marker'].read_bytes(), links.OUTSIDE)

    def test_malformed_setup_failure_removes_payload_and_restores_private_environment(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            runner = links.RecordingRunner({'runs': []}, 5)
            original_env = runner.env
            original_run = Runner.run

            def fail_after_hash(self, name, argv, cwd, timeout=None):
                if name.endswith('.git.index-blob'):
                    self_test.assertFalse((Path(cwd) / links.MALFORMED_PAYLOAD).exists())
                    raise RuntimeError('injected index failure')
                return original_run(self, name, argv, cwd, timeout)

            self_test = self
            with mock.patch.object(Runner, 'run', new=fail_after_hash), self.assertRaisesRegex(RuntimeError, 'index failure'):
                links.materialize(root / 'case', root / 'workspace', item('native.reject-committed-nul-target'), runner, {})
            self.assertIs(runner.env, original_env)
            self.assertFalse((root / 'case/origin' / links.MALFORMED_PAYLOAD).exists())
            self.assertFalse(os.path.lexists(root / 'case/origin' / links.MALFORMED_LINK))
            self.assertEqual((root / 'case/outside-marker').read_bytes(), links.OUTSIDE)

    def test_malformed_target_diagnostic_requires_entry_identity_and_reason(self):
        message = MESSAGES['malformed_target']
        links.require_rejection(record(stderr=message, returncode=1), 'malformed_target')
        for wrong in (message.replace('malformedlink', 'differentlink'),
                      'Git symlink malformedlink target unsupported',
                      'unsafe Git file malformedlink',
                      'unsafe symlink target\nmalformedlink',
                      'symlink malformedlink target missing',
                      'permission denied\n' + message, 'syntax error\n' + message,
                      'connection refused\n' + message):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                links.require_rejection(record(stderr=wrong, returncode=1), 'malformed_target')
        with self.assertRaises(ValueError):
            links.require_rejection(record(stderr=message, stdout='partial graph', returncode=1), 'malformed_target')

    def test_sanitized_blob_output_is_not_accepted_as_fixture_provenance(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            runner = links.RecordingRunner({'runs': []}, 5)
            original_run = Runner.run

            def truncate(self, name, argv, cwd, timeout=None):
                value = original_run(self, name, argv, cwd, timeout)
                if name.endswith('.git.blob-bytes'):
                    value['stdout'] = 'data'  # Simulate the checkout truncation, not real Git cat-file.
                return value

            with mock.patch.object(Runner, 'run', new=truncate), self.assertRaisesRegex(ValueError, 'truncated or changed'):
                links.materialize(root / 'case', root / 'workspace', item('native.reject-committed-nul-target'), runner, {})
            self.assertFalse((root / 'case/origin' / links.MALFORMED_PAYLOAD).exists())
            self.assertEqual((root / 'case/outside-marker').read_bytes(), links.OUTSIDE)


class EvidenceTests(unittest.TestCase):
    def test_inventory_exact_kind_raw_spelling_complete_tree_and_link_reads(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary) / 'tree'
            value = item('materialize.two-link-chain')
            write_tree(root, links.layout(value, root.parent))
            evidence = links.materialized_evidence(root, value, root.parent)
            self.assertTrue(links.tree_evidence_matches(evidence, value, root.parent))
            self.assertEqual(evidence['entries'][links.LINK]['raw_target'], './version-link')
            self.assertEqual(evidence['entries']['e2e/smoke/version-link']['raw_target'], '../.././.bazelversion')
            self.assertEqual(evidence['entries']['e2e']['kind'], 'directory')
            self.assertEqual(set(evidence['link_reads']), {links.LINK, 'e2e/smoke/version-link'})
            for read in evidence['link_reads'].values():
                self.assertTrue(links.evidence_matches(read, links.VERSION))
            (root / 'e2e/smoke/version-link').unlink()
            os.symlink('../../.bazelversion', root / 'e2e/smoke/version-link')
            with self.assertRaisesRegex(ValueError, 'raw target'):
                links.materialized_evidence(root, value, root.parent)

    def test_wrong_kind_same_bytes_and_same_content_retarget_are_rejected_and_unlinked(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary) / 'tree'
            value = links.CASES[0]
            for probe in ('retarget', 'regular', 'referent'):
                with self.subTest(probe=probe):
                    write_tree(root, links.layout(value, root.parent))
                    evidence = links.tamper_cache(root, probe)
                    self.assertTrue(links.valid_entry(evidence))
                    self.assertEqual((root / '.bazelversion.same').read_bytes(), links.VERSION)
                    if probe != 'referent':
                        self.assertEqual((root / '.bazelversion').read_bytes(), links.VERSION)
                        self.assertEqual((root / links.LINK).read_bytes(), links.VERSION)
                    with self.assertRaisesRegex(ValueError, 'entry kind, raw target, bytes'):
                        links.materialized_evidence(root, value, root.parent)

    def test_reference_tree_is_from_reference_cquery_not_native_and_leaf_not_resolved_away(self):
        for prefix in ('', 'nested/pkg'):
            with self.subTest(prefix=prefix), tempfile.TemporaryDirectory(dir=Path(tempfile.gettempdir()).resolve()) as temporary:
                scratch = Path(temporary)
                base = scratch / 'git-links-output-base'
                logical = base / 'execroot/_main/external' / (links.shared.DEP + '+')
                publication = scratch / 'repository-contents/opaque'
                selected = publication / '.tmp_git_root' / prefix if prefix else publication
                value = links.CASES[0]
                write_tree(selected, links.layout(value, scratch))
                if prefix:
                    for name in ('MODULE.bazel', 'data'):
                        os.symlink(Path('.tmp_git_root') / prefix / name, publication / name)
                logical.parent.mkdir(parents=True)
                os.symlink(publication, logical)
                stdout = ''.join('external/' + links.shared.DEP + '+/' + name + '\n' for name in ('data', 'MODULE.bazel'))
                paths = links.reference_output_paths(stdout, scratch / 'workspace', base)
                discovered = links.shared.reference_source_root(paths, base)
                self.assertEqual(discovered, selected)
                evidence = links.materialized_evidence(discovered, value, scratch)
                self.assertEqual(evidence['entries'][links.LINK]['kind'], 'symlink')
                self.assertEqual(evidence['entries'][links.LINK]['target_size_bytes'], 19)
                self.assertNotEqual(Path(evidence['entries'][links.LINK]['path']), (selected / links.LINK).resolve())
                self.assertFalse((scratch / 'native').exists())

    def test_metadata_and_manifest_tampering_is_rejected(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            write_tree(root, links.layout(links.CASES[0], root))
            baseline = links.materialized_evidence(root, links.CASES[0], root)
            for field, replacement in (('kind', 'file'), ('raw_target', '../../.bazelversion.same'),
                                       ('target_base64', ''), ('target_size_bytes', 18),
                                       ('target_sha256', '0' * 64), ('mode', 0o644), ('path', '/outside')):
                altered = copy.deepcopy(baseline)
                altered['entries'][links.LINK][field] = replacement
                self.assertFalse(links.tree_evidence_matches(altered, links.CASES[0], root), field)
            for mutation in ('missing-link', 'extra-link', 'directory', 'read-bytes', 'referent'):
                altered = copy.deepcopy(baseline)
                if mutation == 'missing-link':
                    del altered['entries'][links.LINK]
                elif mutation == 'extra-link':
                    altered['entries']['extra'] = altered['entries'][links.LINK]
                elif mutation == 'directory':
                    del altered['entries']['e2e']
                elif mutation == 'read-bytes':
                    altered['link_reads'][links.LINK]['bytes_base64'] = ''
                else:
                    altered['link_reads'][links.LINK]['referent'] = '.bazelversion.same'
                self.assertFalse(links.tree_evidence_matches(altered, links.CASES[0], root), mutation)
            os.symlink('.bazelversion', root / 'unexpected')
            with self.assertRaises(ValueError):
                links.materialized_evidence(root, links.CASES[0], root)

    def test_positive_patch_reads_new_referent_with_identical_link_metadata(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            root = Path(temporary)
            value = item('native.patch-regular-referent')
            write_tree(root, links.layout(value, root))
            before = links.entry_evidence(root / links.LINK)
            (root / '.bazelversion').write_bytes(links.PATCHED_VERSION)
            evidence = links.materialized_evidence(root, value, root)
            self.assertEqual(before, evidence['entries'][links.LINK])
            self.assertTrue(links.evidence_matches(evidence['link_reads'][links.LINK], links.PATCHED_VERSION))

    def test_narrow_diagnostics_reject_network_parse_permission_and_neighboring_classes(self):
        for diagnostic, message in MESSAGES.items():
            good = record(stderr=message, returncode=1)
            links.require_rejection(good, diagnostic)
            for other in MESSAGES:
                if other != diagnostic:
                    with self.subTest(expected=other, actual=diagnostic), self.assertRaises(ValueError):
                        links.require_rejection(good, other)
            for prefix in ('connection refused', 'permission denied', 'failed to download',
                           'syntax error in BUILD.bazel', 'parse error', 'SSLHandshake', 'panicked', 'fatal: failure'):
                with self.subTest(diagnostic=diagnostic, prefix=prefix), self.assertRaises(ValueError):
                    links.require_rejection(record(stderr=prefix + '\n' + message, returncode=1), diagnostic)
            for change in ({'returncode': 0}, {'returncode': True}, {'returncode': 101}, {'returncode': -9},
                           {'timed_out': True}, {'spawn_error': 'missing'}, {'interrupted': True}, {'stdout': 'partial'}):
                with self.assertRaises(ValueError):
                    links.require_rejection(dict(good, **change), diagnostic)
        for message in ('symlink unsupported', 'Git fetch failed', 'unsupported Git tree mode', 'symlink target invalid'):
            for diagnostic in MESSAGES:
                with self.assertRaises(ValueError):
                    links.require_rejection(record(stderr=message, returncode=1), diagnostic)


class MainTests(unittest.TestCase):
    def run_main(self, directory, fault=None):
        """Real Git is the ONLY executed CLI. All Bazel results below are synthetic."""
        output = Path(directory) / 'synthetic-unit-report.json'
        actual_run = Runner.run
        observations = []

        def run(runner, name, argv, cwd, timeout=None):
            if argv[0] == '/usr/bin/git':
                return actual_run(runner, name, argv, cwd, timeout)
            cwd = Path(cwd)
            if name == 'reference.version':
                result = record('bazel 9.3.0\n')
            elif name == 'git-links.shutdown':
                result = record(returncode=1 if fault == 'shutdown' else 0)
            else:
                check = runner.report['checks'][-1]
                value = check['input']
                engine, phase = name[len(value['name']) + 1:].split('.', 1)
                scratch = Path(runner.report['scratch_directory'])
                case_root = scratch / 'cases' / value['name']
                origin = case_root / 'origin'
                observations.append((value['name'], engine, phase))
                self.assertIn(check['commit'], (cwd / 'MODULE.bazel').read_text())
                self.assertEqual((cwd / 'git-local.patch').read_text(), links.patch_contents(value))
                if phase.startswith('warm.'):
                    self.assertFalse(origin.exists())
                if fault == 'interrupt':
                    raise KeyboardInterrupt
                if fault == 'unexpected':
                    raise RuntimeError('injected unexpected failure')
                if fault == 'warm-retry' and phase.startswith('warm.'):
                    raise RuntimeError('injected while origin is unavailable')
                if fault == 'input':
                    (cwd / 'git-local.patch').write_bytes(b'changed')
                if phase == 'bzl':
                    prefix = '@gitdep'  # both engines print Bazel's display form (N39)
                    result = record(prefix + '//bzl:selected\n')
                elif value['diagnostic']:
                    diagnostic = value['diagnostic']
                    # Captured word-order regressions are exercised end-to-end in
                    # the harness; new cases remain explicitly synthetic contracts.
                    message = CAPTURED.get(diagnostic, MESSAGES[diagnostic])
                    result = record(stderr=message, returncode=1)
                    if fault == 'publication' and value['diagnostic']:
                        (links.shared.trees_directory(scratch / 'native' / value['name']) / '.leaked').mkdir(parents=True)
                elif phase.startswith('artifact'):
                    if engine == 'native':
                        tree = links.shared.trees_directory(scratch / 'native' / value['name']) / 'opaque'
                        write_tree(tree, links.layout(value, case_root, patched=True))
                        if fault == 'artifact':
                            (tree / 'data').write_bytes(b'wrong artifact')
                        result = record(links.shared.canonical(links.ARTIFACT) + '\t' + str(tree / 'data') + '\n')
                    else:
                        base = scratch / 'git-links-output-base/execroot/_main'
                        tree = base / 'external' / (links.shared.DEP + '+')
                        if tree.is_symlink():
                            tree.unlink()
                        write_tree(tree, links.layout(value, case_root, patched=True))
                        result = record('external/' + links.shared.DEP + '+/artifact/link\n' if phase.endswith('.files') else '')
                elif phase == 'query':
                    prefix = '@gitdep'  # both engines print Bazel's display form (N39)
                    result = record(prefix + '//:copy\n' + prefix + '//:sources\n')
                else:
                    source = 'sources' in phase
                    if engine == 'native':
                        base = scratch / 'native' / value['name']
                        tree = links.shared.trees_directory(base) / 'opaque'
                        if value['mode'] == 'repair' and phase.endswith('.sources'):
                            probe = phase.split('.')[0]
                            tampered = links.entry_evidence(tree / ('.bazelversion' if probe == 'referent' else links.LINK))
                            self.assertEqual(tampered, check['tampering'][probe])
                            if fault == 'repair':
                                # Leave the mutation in place; output bytes alone cannot pass.
                                pass
                            else:
                                write_tree(tree, links.layout(value, case_root, patched=True))
                        else:
                            write_tree(tree, links.layout(value, case_root, patched=True))
                    else:
                        base = scratch / 'git-links-output-base/execroot/_main'
                        tree = base / 'external' / (links.shared.DEP + '+')
                        publication = scratch / 'repository-contents' / value['name']
                        selected = publication / '.tmp_git_root/nested/pkg' if value.get('prefix') else publication
                        write_tree(selected, links.layout(value, case_root, patched=True))
                        if selected != publication:
                            for filename in ('MODULE.bazel', 'data'):
                                if not (publication / filename).is_symlink():
                                    os.symlink(Path('.tmp_git_root/nested/pkg') / filename, publication / filename)
                        tree.parent.mkdir(parents=True, exist_ok=True)
                        if tree.is_symlink():
                            tree.unlink()
                        os.symlink(publication, tree)
                    paths = [tree / filename for filename in ('data', 'MODULE.bazel')] if source else [
                        base / 'bazel-out/k8-fastbuild/bin/external' / (links.shared.DEP + '+') / 'copy.txt']
                    if not source:
                        paths[0].parent.mkdir(parents=True, exist_ok=True)
                        paths[0].write_bytes(links.shared.data())
                    if fault == 'bytes':
                        paths[0].write_bytes(b'wrong artifact')
                    if engine == 'native':
                        target = '@gitdep//:' + ('sources' if source else 'copy')
                        result = record(''.join(links.shared.canonical(target) + '\t' + str(path) + '\n' for path in paths))
                    else:
                        result = record(''.join(path.relative_to(base).as_posix() + '\n' for path in paths) if phase.endswith('.files') else '')
                if fault == 'reference' and engine == 'reference':
                    result = record(stderr='failed to download genuine built-in', returncode=1)
            result.update(id=len(runner.report['runs']), name=name, argv=list(argv), cwd=str(cwd),
                          timeout_seconds=runner.timeout if timeout is None else timeout)
            runner.report['runs'].append(result)
            return result

        def binaries(args):
            return {name: {'path': str(getattr(args, name)), 'size_bytes': 123,
                           'sha256': links.REFERENCE_SHA256 if name == 'reference' else 'a' * 64}
                    for name in ('native', 'reference')}

        with mock.patch.object(links, 'validate_binaries', side_effect=binaries), \
                mock.patch.object(Runner, 'run', new=run), mock.patch('builtins.print'):
            code = links.main(['--scratch-parent', directory, '--output', str(output)])
        report = json.loads(output.read_text())
        self.assertTrue(report['scratch_cleaned'])
        self.assertTrue(report['reference_shutdown_attempted'])
        self.assertTrue(report['sources_unchanged'])
        self.assertFalse(Path(report['scratch_directory']).exists())
        return code, report, observations

    def test_full_synthetic_wiring_and_strict_report_tamper_guards(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            code, report, observations = self.run_main(temporary)
        self.assertEqual(code, 0, [(c['name'], c.get('error')) for c in report['checks'] if c['status'] != 'PASS'] + report['errors'])
        self.assertTrue(links.report_passes(report))
        self.assertEqual(report['completed_checks'], 19)
        self.assertEqual(report['completed_differential_cases'], 4)
        self.assertEqual(sum(r['argv'][0] == '/usr/bin/git' for r in report['runs']), 123)
        self.assertTrue(report['build_commands_executed'])
        self.assertFalse(report['test_commands_executed'])
        # Native-only cases invoke only native, except the reference comparison
        # of the consumer case's linked source artifact and linked .bzl load.
        self.assertTrue(all(engine == 'native' or (name == 'native.selected-consumer-boundaries'
                                                   and (phase.startswith('artifact') or phase == 'bzl'))
                            for name, engine, phase in observations if name.startswith('native.')))
        self.assertIn(('native.selected-consumer-boundaries', 'reference', 'artifact.files'), observations)
        self.assertIn(('native.selected-consumer-boundaries', 'reference', 'bzl'), observations)
        self.assertIn(('native.selected-consumer-boundaries', 'native', 'bzl'), observations)
        for mutation in ('case', 'plan', 'parity', 'kind', 'pin', 'native-hash', 'sources', 'shutdown', 'shutdown-code',
                         'scratch', 'run-id', 'extra-run', 'stdout-hash', 'stdout-raw', 'argv', 'cwd', 'git-mode',
                         'commit', 'input', 'marker', 'link-kind', 'link-target', 'link-bytes', 'read-bytes',
                         'reference-root', 'artifact', 'publication', 'tamper', 'warm', 'counts',
                         'linked-artifact-bytes', 'linked-artifact-path', 'linked-artifact-missing',
                         'blob-evidence', 'blob-run', 'blob-index', 'blob-oid', 'blob-size', 'blob-parent',
                         'blob-pin', 'blob-payload', 'blob-worktree', 'blob-inputs'):
            changed = copy.deepcopy(report)
            first = changed['checks'][0]
            if mutation == 'case':
                changed['checks'].pop()
            elif mutation == 'plan':
                changed['planned_checks'] = []
            elif mutation == 'parity':
                first['parity_claim'] = False
            elif mutation == 'kind':
                first['kind'] = 'native_only'
            elif mutation in ('pin', 'native-hash'):
                changed['executables']['reference' if mutation == 'pin' else 'native']['sha256'] = 'wrong'
            elif mutation == 'sources':
                changed['sources_before'].pop(str(links.ROOT / 'scripts/validate_git.py'))
            elif mutation in ('scratch', 'shutdown'):
                changed['scratch_cleaned' if mutation == 'scratch' else 'reference_shutdown_attempted'] = False
            elif mutation == 'shutdown-code':
                changed['runs'][-1]['returncode'] = 1
            elif mutation == 'run-id':
                first['run_ids']['native.sources'] = -1
            elif mutation == 'extra-run':
                changed['runs'].append(dict(changed['runs'][-1], id=len(changed['runs'])))
            elif mutation == 'stdout-hash':
                changed['runs'][0]['stdout_sha256'] = 'wrong'
            elif mutation == 'stdout-raw':
                changed['runs'][0]['stdout_base64'] = base64.b64encode(b'wrong bytes').decode()
            elif mutation == 'argv':
                changed['runs'][first['run_ids']['native.sources']]['argv'][3] = 'query'
            elif mutation == 'cwd':
                changed['runs'][first['run_ids']['native.sources']]['cwd'] = '/not-owned'
            elif mutation == 'git-mode':
                r = changed['runs'][first['git_setup_run_ids']['tree']]
                r['stdout'] = r['stdout'].replace('120000', '100644')
                r['stdout_sha256'] = links.digest(r['stdout'].encode())
            elif mutation == 'commit':
                first['override']['commit'] = 'f' * 40
            elif mutation in ('input', 'marker'):
                first['inputs_unchanged' if mutation == 'input' else 'outside_marker_unchanged'] = False
            elif mutation.startswith('link-'):
                e = first['trees']['native.sources']['entries'][links.LINK]
                e[{'link-kind': 'kind', 'link-target': 'raw_target', 'link-bytes': 'target_base64'}[mutation]] = 'file' if mutation == 'link-kind' else ''
            elif mutation == 'read-bytes':
                first['trees']['native.sources']['link_reads'][links.LINK]['bytes_base64'] = ''
            elif mutation == 'reference-root':
                first['trees']['reference.sources']['root'] = str(Path(changed['scratch_directory']) / 'native')
            elif mutation == 'artifact':
                first['artifacts']['native.copy']['copy.txt']['bytes_base64'] = ''
            elif mutation.startswith('linked-artifact'):
                consumers = next(c for c in changed['checks'] if c['input']['mode'] == 'consumers')
                if mutation == 'linked-artifact-bytes':
                    consumers['artifacts']['native.artifact']['link']['bytes_base64'] = base64.b64encode(b'x').decode()
                elif mutation == 'linked-artifact-path':
                    consumers['artifacts']['reference.artifact']['link']['path'] = '/elsewhere/link'
                else:
                    del consumers['artifacts']['reference.artifact']
            elif mutation == 'publication':
                next(c for c in changed['checks'] if c['input']['diagnostic'])['no_publication'] = False
            elif mutation == 'tamper':
                next(c for c in changed['checks'] if c['input']['mode'] == 'repair')['tampering']['retarget']['raw_target'] = links.RAW
            elif mutation == 'warm':
                next(c for c in changed['checks'] if c['input']['mode'] == 'warm')['origin_unavailable'] = False
            elif mutation == 'counts':
                changed['completed_checks'] = 18
            elif mutation.startswith('blob-'):
                malformed = next(c for c in changed['checks'] if c['input'].get('shape') == 'nul_blob')
                if mutation == 'blob-evidence':
                    malformed['malformed_blob'] = links.malformed_blob_evidence(b'data')
                elif mutation == 'blob-pin':
                    malformed['override']['commit'] = malformed['base_commit']
                elif mutation == 'blob-payload':
                    malformed['malformed_payload_removed'] = False
                elif mutation == 'blob-worktree':
                    malformed['malformed_link_worktree_absent'] = False
                elif mutation == 'blob-inputs':
                    malformed['inputs_before']['git_inputs'] = {}
                    malformed['inputs_after']['git_inputs'] = {}
                else:
                    phase = {'blob-run': 'blob-bytes', 'blob-index': 'tree-malformed',
                             'blob-oid': 'hash-blob', 'blob-size': 'blob-size', 'blob-parent': 'parent-malformed'}[mutation]
                    r = changed['runs'][malformed['git_setup_run_ids'][phase]]
                    r['stdout'] = (r['stdout'].replace('120000', '100644') if mutation == 'blob-index'
                                   else 'data' if mutation == 'blob-run' else '4\n' if mutation == 'blob-size'
                                   else 'f' * 40 + '\n')
                    r['stdout_sha256'] = links.digest(r['stdout'].encode())
            with self.subTest(mutation=mutation):
                self.assertFalse(links.report_passes(changed))
        for malformed in ({}, None, [], {'checks': []}):
            self.assertFalse(links.report_passes(malformed))

    def test_synthetic_failures_restore_origins_and_cleanup(self):
        for fault in ('reference', 'shutdown', 'interrupt', 'unexpected', 'input', 'bytes', 'publication', 'repair', 'warm-retry', 'artifact'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
                code, report, _ = self.run_main(temporary, fault)
                self.assertEqual(code, 130 if fault == 'interrupt' else 1)
                self.assertEqual(report['status'], 'FAIL')
                self.assertFalse(links.report_passes(report))
                self.assertTrue(all(c.get('outside_marker_unchanged', True) for c in report['checks']))
                if fault == 'warm-retry':
                    warm = next(c for c in report['checks'] if c['input']['mode'] == 'warm')
                    self.assertTrue(warm['inputs_unchanged'])

    def test_missing_binary_and_protected_outputs_do_not_execute(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as temporary:
            output = Path(temporary) / 'report.json'
            with mock.patch.object(Runner, 'run', side_effect=AssertionError('must not spawn')), mock.patch('builtins.print'):
                self.assertEqual(links.main(['--native', temporary + '/absent', '--output', str(output)]), 1)
            self.assertEqual(json.loads(output.read_text())['runs'], [])
        for path in (links.shared.GIT, *links.SOURCES):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, 'must not overwrite'):
                links.main(['--output', str(path)])


if __name__ == '__main__':
    unittest.main()
