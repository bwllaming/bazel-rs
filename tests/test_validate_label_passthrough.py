"""Portable N2 harness tests; synthetic receipts are NOT CLI parity evidence.

No native/reference invocation, ignored artifact, platform download or user HOME.
The real execution gate stays with the parent candidate owner.
"""

import ast
import base64
import copy
import io
import json
from pathlib import Path
import re
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_label_passthrough as suite


# OBSOLETE pre-N33 native binder wording, retained only as negative evidence
# (since N33 native reports Bazel's Label() text). Original:
# artifacts/label-passthrough-implementation/attempt-4-fixture-syntax/
# 03-all-cli-groups.stderr SHA256 4b1332ac954ff3b7d619a8f5be95493ab506bc43ebefe108501fc2518070e495.
OBSOLETE_NATIVE_ACTUAL = {
    'none': 'NoneType (repr: None)', 'bool': 'bool (repr: True)', 'int': 'int (repr: 1)',
    'list': 'list (repr: [])', 'dict': 'dict (repr: {})',
    'struct': 'struct (repr: struct(type="Label"))',
}
# Bare marker sequences extracted from immutable reference receipt SHA256
# 2d2ff3847ee92c214777be955088224d6bea288c4f43fee79ee13103353e047c,
# command IDs 21..28 respectively. DEBUG file/line prefixes are not retained here.
REFERENCE_CALLBACK_OBSERVATIONS = {
    ('custom', 'cold', 'build'): ('LP_ANALYSIS_CALLBACK',),
    ('custom', 'cold', 'cquery'): ('LP_ANALYSIS_CALLBACK',),
    ('custom', 'warm', 'build'): ('LP_ANALYSIS_CALLBACK',),
    ('custom', 'warm', 'cquery'): ('LP_ANALYSIS_CALLBACK',),
    ('extension', 'cold', 'build'): ('LP_EXTENSION_CALLBACK', 'LP_REPOSITORY_CALLBACK'),
    ('extension', 'cold', 'cquery'): ('LP_EXTENSION_CALLBACK',),
    ('extension', 'warm', 'build'): ('LP_EXTENSION_CALLBACK',),
    ('extension', 'warm', 'cquery'): ('LP_EXTENSION_CALLBACK',),
}
MUTATION_PROOFS = []  # In-memory unit evidence; never CLI/Starlark acceptance.
# Current (post-N33) actual native rows: all thirteen Label() binding cases from
# the integration-14 pre-gate BOUNDED_PAIRED_PASS report (provenance in the fixture).
BINDING_FIXTURE = json.loads((suite.FIXTURES / 'native_binding_observations_n33.json').read_bytes())
BINDING_OBSERVATIONS = BINDING_FIXTURE['observations']
# Pre-N33 retained rows (starlark-rust binder wording): obsolete negative evidence only.
OBSOLETE_BINDING_FIXTURE = json.loads((suite.FIXTURES / 'native_binding_observations.json').read_bytes())
OBSOLETE_BINDING_OBSERVATIONS = OBSOLETE_BINDING_FIXTURE['observations']
PUBLICATION_OBSERVATION = json.loads((suite.FIXTURES / 'native_publication_observation.json').read_bytes())
BINDING_STDERR_PINS = {
    'wrong-type-none': (620, 'cb9ab15fa90b93799db2f204236170dfa8fcdf9a91c03e34be0fe7e28aa7d402'),
    'wrong-type-bool': (616, 'b23f3fe56ae055156fa5f7ae8db09db273640da6fa0760a7bc899a95efcb210d'),
    'wrong-type-int': (605, 'a25f229053cc7c7bbbc3e56a1da15e65d56b29fcacbbf1855033c0b79e1ad757'),
    'wrong-type-list': (610, '8fa8d8e7fa300486c839711a276f28e7323c156555c2e1a000c4afb8f955a9dd'),
    'wrong-type-dict': (610, '87e42df95f9a45c49612608fa1cbbb6f0a4474fd3c3344a05a2210ee314d724d'),
    'wrong-type-struct': (641, '18d0d646c653851e2b6eaf9fbdf60a3cc82cfc6b824c54e3d8663e9efe2a2e6d'),
    'missing': (563, '0cf48b8de1c4df59060c443a4400757eedcf0f08a42f5c2a98276e91e05204db'),
    'extra': (622, 'da62a11ebbca3f7b3e3bf0c1765691aef8d0a0854e2dd562647d75e33799526d'),
    'keyword-string': (626, '29f5efd51cc5c95adf9ac156a3afcff21600531d3058fa149fdc2bea968b859b'),
    'keyword-label': (646, '08cdfed1e16bc2bd35617ee98cc9b294704c028f2d270004600f819b7b09a436'),
    'keyword-native-name': (642, 'fa4539bc09c0bd3502f171656500f62c6e9465be5c48be0b50f67311306ceeb8'),
    'keyword-unknown': (618, '0e1ef922991fa3e444534e1f35d9844f67b11d6c5ffd9eaa505ce1cad9ba5df8'),
    'positional-plus-keyword': (662, '3bf3d3199c7732ca3135c42851a95c06500a2e4d459b05286016b17dd172c09c'),
}
KEYWORD_CASES = ('keyword-string', 'keyword-label', 'keyword-native-name', 'keyword-unknown', 'positional-plus-keyword')
# OBSOLETE (pre-N33) exact 607-byte stderr from native preparation report SHA256
# 15919ac3c8a64cf45070131a8ae57afed86e2f01e5142158b104fc23542455e4,
# run 6 (wrong-type-none), exit 1. Since N33 it must be REJECTED; the current
# retained wrong-type-none row is BINDING_OBSERVATIONS['wrong-type-none'].
OBSOLETE_NATIVE_NONE_STDERR = (
    b'BEFORE_CALL\n'
    b'bazel-rs: Traceback (most recent call last):\n'
    b'  * /tmp/opencode/bazel-parity-label-passthrough/artifacts/label-passthrough-native-prep-k3jn39vp/cases/wrong-type-none/native/workspace/BUILD.bazel:2, in <module>\n'
    b'      run_probe()\n'
    b'  * //:probe.bzl:3, in run_probe\n'
    b'      result = Label(None)\n'
    b"error: Type of parameter `input` doesn't match, expected `Label | str`, actual `NoneType (repr: None)`\n"
    b' --> //:probe.bzl:3:14\n'
    b'  |\n'
    b'3 |     result = Label(None)\n'
    b'  |              ^^^^^^^^^^^\n'
    b'  |\n'
    b'\n'
    b'Supported rules: filegroup, genrule, and local custom rules. select and non-local repositories remain unsupported.\n'
)


def record(text='', code=0, stdout=b''):
    return dict(stderr=suite.blob(text.encode()), stdout=suite.blob(stdout), returncode=code,
        timeout=False, spawn_error=None, group_gone=True, cleanup_signals=[], outputs={}, publication={},
        action_files=[], action_files_before=[], execution_log=None)


def reference_error(case):
    if case.startswith('wrong-type-'):
        typename = suite.BAD_TYPES[case.removeprefix('wrong-type-')][1]
        return "in call to Label(), parameter 'input' got value of type '" + typename + "', want 'string or Label'"
    if case == 'missing':
        return 'Label() missing 1 required positional argument: input'
    if case == 'extra':
        return 'Label() accepts no more than 1 positional argument but got 2'
    if case in ('keyword-unknown', 'keyword-native-name'):
        return "Label() got unexpected keyword argument '" + ('unknown' if case == 'keyword-unknown' else 'label_string') + "'"
    return "Label() got named argument for positional-only parameter 'input'"


def native_error(case):
    # Since N33 the native binder's primary line is Bazel's own body.
    return reference_error(case)


def obsolete_native_error(case):
    # Pre-N33 starlark-rust binder wording; must now be rejected.
    if case.startswith('wrong-type-'):
        actual = OBSOLETE_NATIVE_ACTUAL[case.removeprefix('wrong-type-')]
        if case == 'wrong-type-struct':
            actual = 'struct (repr: struct(x=1))'
        return "Type of parameter `input` doesn't match, expected `Label | str`, actual `" + actual + '`'
    if case == 'missing':
        return 'Wrong number of positional arguments, expected 1, got 0'
    if case == 'extra':
        return 'Wrong number of positional arguments, expected 1, got 2'
    key = 'unknown' if case == 'keyword-unknown' else 'label_string' if case == 'keyword-native-name' else 'input'
    return 'Found `' + key + '` extra named parameter(s) for call to function'


def query_row(case, engine='reference'):
    if engine == 'native' and case in suite.CALLS:
        observed = BINDING_OBSERVATIONS[case]
        row = record(code=observed['returncode'])
        row.update(stdout=copy.deepcopy(observed['stdout']), stderr=copy.deepcopy(observed['stderr']))
        return row  # Actual retained streams in a synthetic unit envelope, not a new CLI run.
    if case == 'positive':
        text = '\n'.join('COPY_OK ' + n + ' @@' + repo + '//:same ' + repo + ' dict=1 string_hash_equal=True'
                         for n, repo in suite.COPY_ROWS)
        return record(text + '\nFROZEN_EXPORT_TWO_READERS_OK @@dep+//:same external-frozen-key\nPOSITIVE_COMPLETE\n', stdout=b'//:ok\n')
    if case in suite.CALLS:
        return record('BEFORE_CALL\nprobe.bzl:3:19\n    result = ' + suite.CALLS[case] + '\n'
                      'Error in Label: ' + reference_error(case) + '\n', 7)
    text = 'BEFORE_INVALID_CONSTRUCTION\nINVALID_CONSTRUCTED\nINVALID_PASSTHROUGH_OK\n'
    if case == suite.INVALID[0]:
        return record(text, stdout=b'//:ok\n')
    body = "'workspace_name' is not allowed on invalid Label @@[unknown repo 'not_visible' requested from @@]//:missing"
    if engine == 'native':
        # Integration-15 pre-gate native shape (deferred invalid Label getter).
        return record(text + 'BEFORE_GETTER\nbazel-rs: Traceback (most recent call last):\n'
                      '  * /w/BUILD.bazel:2, in <module>\n      run_probe()\n'
                      'error: ' + body + '\n  --> //:probe.bzl:10:13\n   |\n'
                      '10 |     value = copy.workspace_name\n   |             ^^^^^^^^^^^^^^^^^^^\n   |\n\n'
                      + suite.NATIVE_LOADING_FOOTER, 1)
    return record(text + 'BEFORE_GETTER\nprobe.bzl:10:17\n    value = copy.workspace_name\n'
        'Error: ' + body + '\n', 7)


def publication():
    identity = 'a' * 64
    manifest = dict(format=1, profile='direct-file-template-v1', identity=identity,
        files={'': dict(directory=True, mode=0o755, size=0, sha256=''),
               **{p: dict(directory=False, mode=0o644, size=len(raw), sha256=suite.sha(raw))
                  for p, raw in suite.REPOSITORY_FILES.items()}})
    return dict(identity=identity, roots=[identity],
        manifests={identity + '.json': dict(mode=0o644, raw=suite.blob(json.dumps(manifest).encode()))},
        inventory={p: dict(mode=0o644, raw=suite.blob(raw)) for p, raw in suite.REPOSITORY_FILES.items()})


def synthetic_report(reference_only=False, config_prefix='unit-cpu-fastbuild'):
    config = dict(reference='/frozen/reference', native=None if reference_only else '/frozen/native',
        native_sha256=None if reference_only else 'c' * 64, reference_only=reference_only,
        official_source='/pinned/official', timeout=180)
    binding = dict(sources={}, platform=suite.blob(suite.archive()))
    scratch = Path('/tmp/opencode/label-passthrough-portable-receipt-absent')
    report = dict(schema=1, config=config, binding_before=binding, binding_after=binding,
        scratch=str(scratch), cleanup=True, plan=suite.plan(reference_only), runs=[], observations=[],
        status='REFERENCE_PREPARATION_ONLY' if reference_only else 'BOUNDED_PAIRED_PASS')
    pins = {}
    for i, step in enumerate(report['plan']):
        case, engine, stage, verb = step
        row = query_row(case, engine) if case in suite.QUERIES else record()
        argv, cwd = suite.command(config, scratch, step)
        row.update(id=i, step=step, argv=argv, cwd=cwd, env=suite.environment(scratch), timeout_seconds=180,
            binding_sha256=suite.binding_sha(binding), inputs_before={}, inputs_after={}, pid=10000 + i,
            elapsed_seconds=0.1, private_before=suite.expected_private(binding), private_after=suite.expected_private(binding))
        if case == 'aux':
            row['stdout'] = suite.blob(((suite.OFFICIAL_COMMIT if engine == 'official' else 'b' * 40) + '\n').encode() if verb == 'head' else b'')
            row['result'] = suite.auxiliary_result(row, step, pins)
        elif case == 'version':
            row['stdout'] = suite.blob(b'Build label: 9.3.0\n' if engine == 'reference' else b'bazel-rs 0.1.0 (experimental; compatibility target Bazel 9.3.0)\n')
            row['result'] = suite.auxiliary_result(row, step, pins)
        else:
            row['inputs_before'] = row['inputs_after'] = suite.inputs(case)
            if case in suite.QUERIES:
                row['result'] = suite.query_result(row, case, engine)
            else:
                workspace, cache, output = suite.locations(scratch, step)
                bin_root = 'bazel-out/' + config_prefix + '/bin/'
                if verb == ('build' if engine == 'native' else 'cquery'):
                    producer = (cache / 'repositories/direct/trees' / ('a' * 64) / 'data.txt'
                        if case == 'extension' and engine == 'native' else
                        output / 'execroot/_main/external/+files+made/data.txt' if case == 'extension' else
                        (cache if engine == 'native' else output / 'execroot/_main') / bin_root / 'data.txt')
                    consumer = (cache if engine == 'native' else output / 'execroot/_main') / bin_root / 'result.txt'
                    paths = [producer, consumer]
                    row['stdout'] = suite.blob((''.join('//:result\t' + str(p) + '\n' for p in paths) if engine == 'native'
                        else ''.join(str(p.relative_to(output / 'execroot/_main')) + '\n' for p in paths)).encode())
                    row['outputs'] = {str(p): dict(mode=0o644 if case == 'extension' and p.name == 'data.txt' else 0o555,
                                                  raw=suite.blob(suite.DATA)) for p in paths}
                    if case == 'extension' and engine == 'native':
                        row['publication'] = publication()
                if engine == 'native' and verb == 'build':
                    count = 2 if case == 'custom' else 1
                    # Integration-18 (N61): a warm native build reuses the repository publication.
                    text = ('LP_ANALYSIS_CALLBACK\n' if case == 'custom' else
                            'LP_EXTENSION_CALLBACK\n' + ('LP_REPOSITORY_CALLBACK\n' if stage == 'cold' else ''))
                    text += f'bazel-rs: {count if stage == "cold" else 0} actions executed, {0 if stage == "cold" else count} cache hits\n'
                    row['stderr'] = suite.blob(text.encode())
                elif engine == 'native':
                    row['stdout'] = suite.blob(b'//:result\n')
                elif verb == 'build':
                    producer = bin_root + 'data.txt' if case == 'custom' else 'external/+files+made/data.txt'
                    consumer = bin_root + 'result.txt'
                    setup = 'external/bazel_tools/tools/genrule/genrule-setup.sh'
                    digest = dict(hash=suite.sha(suite.DATA), sizeBytes=str(len(suite.DATA)), hashFunctionName='SHA-256')
                    spawn = dict(exitCode=0, cacheHit=False, mnemonic='Genrule', targetLabel='//:consumer',
                        inputs=[dict(path=producer, digest=digest), dict(path=setup)],
                        actualOutputs=[dict(path=consumer, digest=digest)], listedOutputs=[consumer],
                        commandArgs=['/bin/bash', '-c', 'source ' + setup + '; cat ' + producer + ' > ' + consumer])
                    row['execution_log'] = suite.blob(json.dumps(spawn).encode() if stage == 'cold' else b'')
                if engine == 'reference':
                    row['stderr'] = suite.blob(('\n'.join(REFERENCE_CALLBACK_OBSERVATIONS[(case, stage, verb)]) + '\n').encode())
                row['result'] = suite.build_result(row, scratch, step)
        report['runs'].append(row)
        report['observations'].append(i)
        suite.complete_reference_pair(report['runs'], scratch)
    return report, config, binding


class FixtureTests(unittest.TestCase):
    def test_genuine_archive_is_portable_and_pinned(self):
        raw = suite.archive()
        self.assertEqual((len(raw), suite.sha(raw)), (7879, suite.PLATFORM_SHA))
        with tarfile.open(fileobj=io.BytesIO(raw), mode='r:gz') as archive:
            self.assertIn('MODULE.bazel', archive.getnames())
            self.assertIn(b'version = "1.0.0"', archive.extractfile('MODULE.bazel').read())

    def test_archive_corruption_and_bad_provenance_pins_rejected(self):
        for mutation in ('archive', 'size', 'sha256', 'reference', 'commit', 'url'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                meta = json.loads((suite.FIXTURES / 'provenance.json').read_bytes())
                encoded = (suite.FIXTURES / 'platforms-1.0.0.tar.gz.base64').read_bytes()
                if mutation == 'archive':
                    encoded = base64.b64encode(b'not the genuine archive')
                elif mutation in ('size', 'sha256', 'url'):
                    meta['platforms'][mutation] = 1 if mutation == 'size' else 'bad'
                else:
                    meta['reference_sha256' if mutation == 'reference' else 'official_commit'] = 'bad'
                (root / 'provenance.json').write_text(json.dumps(meta))
                (root / 'platforms-1.0.0.tar.gz.base64').write_bytes(encoded)
                with mock.patch.object(suite, 'FIXTURES', root), self.assertRaises(ValueError):
                    suite.archive()

    def test_all_fixtures_parse_and_build_has_no_passthrough_global(self):
        for case in (*suite.QUERIES, *suite.BUILDS):
            for path, raw in suite.fixture(case).items():
                with self.subTest(case=case, path=path):
                    ast.parse(raw.decode(), filename=path)
                    if path.endswith('BUILD.bazel'):
                        self.assertNotIn(b'Label(', raw)
                        self.assertNotIn(b'print(', raw)
                    self.assertNotIn(b'.repo_name', raw)

    def test_complete_case_and_type_coverage(self):
        self.assertEqual(set(suite.BAD_TYPES), {'none', 'bool', 'int', 'list', 'dict', 'struct'})
        self.assertEqual(len(suite.CALLS), 13)
        for name in ('keyword-string', 'keyword-label', 'keyword-native-name', 'keyword-unknown', 'positional-plus-keyword', 'extra', 'missing'):
            self.assertIn(name, suite.CALLS)
        with self.assertRaises(ValueError):
            suite.fixture('fallback')

    def test_frozen_two_reader_dictionary_and_logical_hash_equality(self):
        files = suite.fixture('positive')
        for p in ('root_export.bzl', 'dep/export.bzl', 'reader_a.bzl', 'reader_b.bzl'):
            self.assertIn(b'Label(', files[p])
        probe = files['probe.bzl']
        self.assertIn(b'hash(str(original)) != hash(str(copied))', probe)
        self.assertIn(b'len(values) != 1', probe)
        self.assertNotIn(b'hash(original)', probe)
        self.assertIn(b'copy_dep(ROOT)', probe)
        self.assertIn(b'copy_root(EXTERNAL)', probe)

    def test_producer_and_consumer_are_real_not_precreated(self):
        for case in suite.BUILDS:
            files = suite.fixture(case)
            self.assertNotIn('data.txt', files)
            self.assertNotIn('result.txt', files)
            self.assertIn(b'ctx.actions.write(output, render())', files['producer.bzl'])
        self.assertIn(b'ctx.file("data.txt", ctx.attr.text', suite.fixture('extension')['ext.bzl'])
        self.assertIn(b'cat $(SRCS) > $@', suite.fixture('extension')['BUILD.bazel'])

    def test_no_ignored_or_inflight_qa_dependencies(self):
        raw = Path(suite.__file__).read_text()
        self.assertNotIn('artifacts/', raw)
        self.assertNotIn('validate_label_repo_name', raw)
        self.assertNotIn('os.environ', raw)
        self.assertNotIn('GOROOT', raw)
        self.assertEqual(set(suite.environment(Path('/private'))), {'HOME', 'TMPDIR', 'PATH', 'LC_ALL', 'BAZEL_RS_RESULT_LINES'})

    def test_full_and_reference_modes_both_need_freeze(self):
        common = ['--reference', '/frozen/ref', '--official-source', '/official', '--output', '/receipt']
        for flags in ([], ['--reference-only']):
            with self.assertRaisesRegex(ValueError, 'freeze'):
                suite.configuration(suite.parser().parse_args(common + flags))
        with self.assertRaisesRegex(ValueError, 'explicit native'):
            suite.configuration(suite.parser().parse_args(common + ['--candidate-frozen']))
        config = suite.configuration(suite.parser().parse_args(common + ['--candidate-frozen', '--reference-only']))
        self.assertIsNone(config['native'])

    def test_subprocess_is_direct_private_env_and_process_group(self):
        config = dict(reference='/frozen/reference', official_source='/official', timeout=180)
        binding = dict(sources={}, platform=suite.blob(b'private archive'))
        private = suite.expected_private(binding)
        process = mock.Mock(pid=12345, returncode=0)
        process.communicate.return_value = (b'b' * 40 + b'\n', b'')
        process.poll.return_value = 0
        rows = []
        with mock.patch.object(suite, 'bindings', return_value=binding), \
             mock.patch.object(suite, 'private_inputs', return_value=private), \
             mock.patch.object(suite, 'group_exists', return_value=False), \
             mock.patch.object(suite.subprocess, 'Popen', return_value=process) as spawn:
            row = suite.run(config, Path('/private'), ['aux', 'checkout', 'before', 'head'], 0, binding, rows)
        self.assertEqual(len(rows), 1)
        self.assertIs(row, rows[0])
        self.assertEqual(spawn.call_args.kwargs['env'], suite.environment(Path('/private')))
        self.assertTrue(spawn.call_args.kwargs['start_new_session'])
        self.assertEqual(spawn.call_args.kwargs['stdin'], suite.subprocess.DEVNULL)
        process.communicate.assert_called_once_with(timeout=180)
        self.assertTrue(row['group_gone'])

    def test_executable_bad_pin_fails_before_any_spawn(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            exe = root / 'reference'
            exe.write_bytes(b'\x7fELFnot-genuine')
            exe.chmod(0o755)
            config = dict(reference=str(exe), official_source=str(root), reference_only=True)
            with mock.patch.object(suite, 'source_paths', return_value=[]), \
                 mock.patch.object(suite, 'OFFICIAL', {}), mock.patch.object(suite.subprocess, 'Popen') as spawn:
                with self.assertRaisesRegex(ValueError, 'executable pin'):
                    suite.bindings(config)
                spawn.assert_not_called()


class RejectionTests(unittest.TestCase):
    def test_observed_native_binder_primary_diagnostics(self):
        # Since N33: the native primary line is exactly Bazel's body.
        for case in suite.CALLS:
            text = suite.unblob(BINDING_OBSERVATIONS[case]['stderr']).decode()
            self.assertIn('\nerror: ' + reference_error(case) + '\n', text)
            with self.subTest(case=case):
                suite.diagnostic(case, 'native', text)

    def test_obsolete_pre_n33_native_wording_is_rejected(self):
        for case in suite.CALLS:
            text = suite.unblob(BINDING_OBSERVATIONS[case]['stderr']).decode()
            old = text.replace('error: ' + reference_error(case), 'error: ' + obsolete_native_error(case))
            self.assertNotEqual(old, text)
            with self.subTest(case=case), self.assertRaisesRegex(ValueError, 'wrong native'):
                suite.diagnostic(case, 'native', old)
        for case, observed in OBSOLETE_BINDING_OBSERVATIONS.items():
            with self.subTest(case=case, retained=True), self.assertRaisesRegex(ValueError, 'wrong native'):
                suite.query_result(dict(record(code=1), stderr=copy.deepcopy(observed['stderr'])), case, 'native')
        with self.assertRaisesRegex(ValueError, 'wrong native type/parameter category'):
            suite.query_result(record(OBSOLETE_NATIVE_NONE_STDERR.decode(), 1), 'wrong-type-none', 'native')

    def test_reference_and_synthetic_native_categories(self):
        for case in suite.CALLS:
            for engine in ('reference', 'native'):
                with self.subTest(case=case, engine=engine):
                    self.assertEqual(suite.query_result(query_row(case, engine), case, engine), 'PARITY_REJECTION_CATEGORY')

    def test_invalid_label_passthrough_and_getter_are_paired(self):
        # Integration-15: the former native early-construction divergence is gone.
        self.assertFalse(hasattr(suite, 'DIVERGENCES'))
        for case, result in zip(suite.INVALID, ('PARITY_INVALID_PASSTHROUGH', 'PARITY_INVALID_GETTER')):
            for engine in ('reference', 'native'):
                with self.subTest(case=case, engine=engine):
                    self.assertEqual(suite.query_result(query_row(case, engine), case, engine), result)
        getter = suite.INVALID[1]
        with self.assertRaises(ValueError):
            suite.query_result(query_row(getter, 'native'), getter, 'reference')
        with self.assertRaises(ValueError):
            suite.query_result(query_row(getter), getter, 'native')
        early = record('BEFORE_INVALID_CONSTRUCTION\nprobe.bzl:3:22\n    original = Label("@not_visible//:missing")\n'
                       'error: unknown apparent repository @not_visible from canonical repository ""\n', 1)
        for case in suite.INVALID:
            with self.subTest(case=case), self.assertRaises(ValueError):
                suite.query_result(copy.deepcopy(early), case, 'native')
        for old, new in ((b'BEFORE_GETTER\n', b''), (b"'workspace_name' is not", b"'repo_name' is not"),
                         (b'probe.bzl:10:13', b'probe.bzl:9:13'), (b'requested from @@]', b'requested from @@dep+]')):
            row = query_row(getter, 'native')
            row['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(old, new))
            with self.subTest(old=old), self.assertRaises(ValueError):
                suite.query_result(row, getter, 'native')
        row = query_row(getter, 'native')
        row['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(b'\n  --> ', b'\nerror: Other\n  --> '))
        with self.assertRaises(ValueError):
            suite.query_result(row, getter, 'native')

    def test_exact_bare_markers_not_source_echo(self):
        self.assertEqual(suite.markers('    print("BEFORE_CALL")\n3 | print("BEFORE_CALL")\n'), [])
        self.assertEqual(suite.markers('DEBUG: /w/probe.bzl:2:10: BEFORE_CALL\n'), ['BEFORE_CALL'])
        row = query_row('missing')
        row['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(b'BEFORE_CALL\n', b'    print("BEFORE_CALL")\n'))
        with self.assertRaises(ValueError):
            suite.query_result(row, 'missing', 'reference')

    def test_nonzero_is_not_a_pass(self):
        for code in (0, 1, 2, 8, 37, 101, 124, 127, -11):
            row = query_row('wrong-type-int')
            row['returncode'] = code
            with self.subTest(code=code), self.assertRaises(ValueError):
                suite.query_result(row, 'wrong-type-int', 'reference')

    def test_fallback_infrastructure_crash_missing_global_and_action_rejected(self):
        for text in ('fallback to Bazel', 'panicked at foo', 'failed to download archive', 'permission denied',
                     'name Label is not defined', 'Variable Label not found', '1 actions executed, 0 cache hits',
                     'SUBCOMMAND: action', 'LP_ANALYSIS_CALLBACK'):
            row = query_row('wrong-type-none')
            row['stderr'] = suite.blob(suite.unblob(row['stderr']) + text.encode() + b'\n')
            with self.subTest(text=text), self.assertRaises(ValueError):
                suite.query_result(row, 'wrong-type-none', 'reference')

    def test_wrong_type_parameter_callsite_or_late_failure_rejected(self):
        for old, new in ((b"'int'", b"'bool'"), (b"'input'", b"'label_string'"),
                         (b'probe.bzl:3', b'probe.bzl:4'), (b'Label(1)', b'Label(True)'),
                         (b'BEFORE_CALL', b'UNEXPECTED_AFTER_CALL')):
            row = query_row('wrong-type-int')
            row['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(old, new))
            with self.subTest(old=old), self.assertRaises(ValueError):
                suite.query_result(row, 'wrong-type-int', 'reference')

    def test_query_cannot_have_action_files_outputs_or_publication(self):
        for field, value in (('outputs', {'data': {}}), ('publication', {'id': 'fake'}), ('action_files', ['/cache/actions/a']),
                             ('action_files_before', ['/cache/actions/a']), ('execution_log', suite.blob(b'action'))):
            row = query_row('missing')
            row[field] = value
            with self.assertRaises(ValueError):
                suite.query_result(row, 'missing', 'reference')


class ReplayTests(unittest.TestCase):
    def setUp(self):
        self.report, self.config, self.binding = synthetic_report()

    def test_synthetic_complete_replay_only_harness_logic(self):
        self.assertEqual(suite.replay(self.report, self.config, self.binding), 'BOUNDED_PAIRED_PASS')
        report, config, binding = synthetic_report(True)
        self.assertEqual(suite.replay(report, config, binding), 'REFERENCE_PREPARATION_ONLY')
        self.assertFalse(any(row['step'][1] == 'native' for row in report['runs']))

    def test_complete_chronology_cannot_be_swapped_renumbered_referenced(self):
        cases = [(0, 2), (4, 6)]
        for a, b in (('cold', 'warm'), ('build', 'cquery')):
            rows = [r['id'] for r in self.report['runs'] if r['step'][0:2] == ['custom', 'reference']]
            cases.append((rows[0], rows[2] if a == 'cold' else rows[1]))
        for a, b in cases:
            report = copy.deepcopy(self.report)
            report['runs'][a], report['runs'][b] = report['runs'][b], report['runs'][a]
            for i, row in enumerate(report['runs']):
                row['id'] = i
            report['observations'] = list(range(len(report['runs'])))
            with self.subTest(a=a, b=b), self.assertRaisesRegex(ValueError, 'chronological'):
                suite.replay(report, self.config, self.binding)

    def test_missing_extra_duplicate_and_plan_rewrite_rejected(self):
        for mutation in ('missing', 'extra', 'duplicate', 'rewrite'):
            report = copy.deepcopy(self.report)
            if mutation == 'missing':
                report['runs'].pop()
            elif mutation == 'extra':
                report['runs'].append(report['runs'][-1])
            elif mutation == 'duplicate':
                report['runs'][6] = report['runs'][7]
            else:
                report['plan'][0], report['plan'][1] = report['plan'][1], report['plan'][0]
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                suite.replay(report, self.config, self.binding)

    def test_environment_source_argv_fixture_and_scope_tampering_rejected(self):
        row = next(r for r in self.report['runs'] if r['step'][0] == 'missing')
        for field, value in (('env', {'PATH': '/usr/bin:/bin', 'SECRET': 'not allowed'}),
                             ('argv', ['/ambient/wrapper', 'query']), ('binding_sha256', 'bad'),
                             ('inputs_after', {}), ('result', 'PASS'), ('timeout', True),
                             ('private_after', {}), ('cleanup_signals', ['SIGKILL'])):
            report = copy.deepcopy(self.report)
            report['runs'][row['id']][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                suite.replay(report, self.config, self.binding)
        changed = copy.deepcopy(self.binding)
        changed['sources']['src/starlark_types.rs'] = {'sha256': 'different'}
        with self.assertRaisesRegex(ValueError, 'current source/executable'):
            suite.replay(self.report, self.config, changed)

    def test_raw_byte_hash_and_status_tampering_rejected(self):
        for field, value in (('base64', base64.b64encode(b'tampered').decode()), ('sha256', '0' * 64), ('size', -1)):
            report = copy.deepcopy(self.report)
            report['runs'][0]['stdout'][field] = value
            with self.assertRaises(ValueError):
                suite.replay(report, self.config, self.binding)
        self.report['status'] = 'FULL_BAZEL_PARITY'
        with self.assertRaises(ValueError):
            suite.replay(self.report, self.config, self.binding)

    def test_version_and_official_pin_are_real_auxiliary_gates(self):
        for case, stdout in (('aux', b'0' * 40 + b'\n'), ('version', b'Build label: 9.2.0\n')):
            row = next(r for r in self.report['runs'] if r['step'][0] == case and
                       r['step'][1] == ('official' if case == 'aux' else 'reference'))
            row = copy.deepcopy(row)
            row['stdout'] = suite.blob(stdout)
            with self.assertRaises(ValueError):
                suite.auxiliary_result(row, row['step'], {})

    def test_completed_outputs_must_be_exact_0555_repository_sources_0644(self):
        for case, name, wrong_mode in (('custom', 'data.txt', 0o755), ('custom', 'result.txt', 0o644),
                                      ('extension', 'result.txt', 0o555 | 0o200), ('extension', 'data.txt', 0o555)):
            report = copy.deepcopy(self.report)
            row = next(r for r in report['runs'] if r['step'] == [case, 'native', 'cold', 'build'])
            path = next(p for p in row['outputs'] if Path(p).name == name)
            row['outputs'][path]['mode'] = wrong_mode
            with self.subTest(case=case, name=name), self.assertRaises(ValueError):
                suite.replay(report, self.config, self.binding)

    def test_native_warm_callbacks_reuse_but_identity_and_cache_are_independent(self):
        row = next(r for r in self.report['runs'] if r['step'] == ['extension', 'native', 'warm', 'build'])
        # Integration-18 (N61): the warm build reuses the publication; the repository callback stays silent.
        self.assertNotIn('LP_REPOSITORY_CALLBACK', suite.unblob(row['stderr']).decode())
        self.assertIn('LP_EXTENSION_CALLBACK', suite.unblob(row['stderr']).decode())
        repeated = copy.deepcopy(self.report)
        repeated['runs'][row['id']]['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(
            b'LP_EXTENSION_CALLBACK\n', b'LP_EXTENSION_CALLBACK\nLP_REPOSITORY_CALLBACK\n'))
        with self.assertRaises(ValueError):
            suite.replay(repeated, self.config, self.binding)
        for field, value in (('roots', ['a' * 64, '.stage-stray']), ('inventory', {}), ('manifests', {}), ('identity', 'b' * 64)):
            report = copy.deepcopy(self.report)
            report['runs'][row['id']]['publication'][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                suite.replay(report, self.config, self.binding)
        row['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(b'0 actions executed, 1 cache hits', b'1 actions executed, 0 cache hits'))
        with self.assertRaises(ValueError):
            suite.replay(self.report, self.config, self.binding)

    def test_reference_warm_action_and_wrong_consumer_input_rejected(self):
        for stage in ('cold', 'warm'):
            report = copy.deepcopy(self.report)
            row = next(r for r in report['runs'] if r['step'] == ['custom', 'reference', stage, 'build'])
            row['execution_log'] = suite.blob(json.dumps(dict(exitCode=0, mnemonic='Genrule', inputs=[], actualOutputs=[])).encode())
            with self.assertRaises(ValueError):
                suite.replay(report, self.config, self.binding)

    def test_command_flags_are_offline_private_and_query_has_no_platform_archive(self):
        for row in self.report['runs']:
            case, engine, _, verb = row['step']
            if engine == 'reference' and case not in ('aux', 'version'):
                argv = row['argv']
                self.assertIn('--repository_disable_download', argv)
                self.assertIn('--lockfile_mode=off', argv)
                self.assertIn('--incompatible_autoload_externally=', argv)
                self.assertEqual(len([a for a in argv if a.startswith('--registry=')]), 1)
                self.assertFalse(any('https://' in a for a in argv))
                self.assertEqual(any(a.startswith('--distdir=') for a in argv), case in suite.BUILDS)
                self.assertIn('--batch', argv)


class OracleMutationTests(unittest.TestCase):
    """Semantic mutations keep valid hashes, source/callsite snapshots and chronology."""

    def setUp(self):
        self.report, self.config, self.binding = synthetic_report()
        self.scratch = Path(self.report['scratch'])

    def row_id(self, step):
        return next(r['id'] for r in self.report['runs'] if r['step'] == step)

    def reject(self, report, index, category, mutation, criterion):
        row = report['runs'][index]
        case, engine, stage, verb = row['step']
        suite.check_order(report)
        self.assertEqual(row['inputs_before'], suite.inputs(case))
        self.assertEqual(row['inputs_after'], suite.inputs(case))
        rebuilt = []
        for field in ('stdout', 'stderr', 'execution_log'):
            if row[field] is not None:
                raw = suite.unblob(row[field])  # Must be a valid blob, not a hash-rejection test.
                self.assertEqual(row[field], suite.blob(raw))
                if row[field] != self.report['runs'][index][field]:
                    rebuilt.append(field)
        with self.assertRaisesRegex(ValueError, criterion) as live:
            if case in suite.QUERIES:
                suite.query_result(row, case, engine)
            else:
                witness = report['runs'][index + 1] if engine == 'reference' and verb == 'build' else None
                suite.build_result(row, self.scratch, row['step'], artifact_row=witness)
        with self.assertRaisesRegex(ValueError, criterion) as replay:
            suite.replay(report, self.config, self.binding)
        MUTATION_PROOFS.append(dict(category=category, mutation=mutation, step=row['step'],
            rebuilt_streams=rebuilt, stream_hashes_valid=True, complete_order_valid=True,
            source_callsite_snapshots_unchanged=True, live_error=str(live.exception), replay_error=str(replay.exception)))

    def test_negative_fallback_and_all_action_report_forms_rehashed_live_and_replay(self):
        variants = (
            'falling back to genuine Bazel', 'fall back to genuine Bazel', 'FALLING BACK to Bazel',
            'fallback to Bazel', '1 action cache hit', '2 action cache hits', '1 cache hit',
            '2 cache hits', '1 actions executed, 0 cache hits',
            'INFO: Build completed successfully, 1 total action',
            'INFO: Build completed successfully, 2 total actions',
            'INFO: 1 process: 1 internal.', 'Action cache hits: 1', 'Cache hit=1',
            'Total actions: 1', 'Executed 1 action', 'Executing action [1 / 1]', 'SUBCOMMAND: consumer',
            'actions executed: 1', 'Action cache hit for //:consumer', 'INFO: 1 cached action',
        )
        for engine in ('reference', 'native'):
            index = self.row_id(['wrong-type-int', engine, 'query', 'query'])
            for text in variants:
                with self.subTest(engine=engine, text=text):
                    report = copy.deepcopy(self.report)
                    row = report['runs'][index]
                    raw = suite.unblob(row['stderr'])
                    footer = suite.NATIVE_LOADING_FOOTER.encode()
                    if engine == 'native':
                        # Actual native rows end in the terminal footer: inject
                        # before it so the action/fallback guard (not the footer
                        # guard) is what must reject.
                        self.assertTrue(raw.endswith(footer))
                        raw = raw[:-len(footer)] + text.encode() + b'\n' + footer
                    else:
                        raw += text.encode() + b'\n'
                    row['stderr'] = suite.blob(raw)
                    self.reject(report, index, 'negative-query-guards', text,
                                'fallback/infrastructure/crash/global failure|query executed action/callback')

    def test_existing_failure_guards_remain_strict_with_recomputed_hashes(self):
        index = self.row_id(['missing', 'reference', 'query', 'query'])
        for text in ('unsupported feature', 'panicked at source', 'failed to download archive',
                     'name Label is not defined', 'Variable Label not found', 'syntax error'):
            with self.subTest(text=text):
                report = copy.deepcopy(self.report)
                row = report['runs'][index]
                row['stderr'] = suite.blob(suite.unblob(row['stderr']) + text.encode() + b'\n')
                self.reject(report, index, 'negative-query-guards', text, 'fallback/infrastructure/crash/global failure')

    def test_native_wrong_parameter_expected_union_actual_and_direction_rehashed(self):
        for kind in suite.BAD_TYPES:
            case = 'wrong-type-' + kind
            index = self.row_id([case, 'native', 'query', 'query'])
            good = native_error(case)
            typename = suite.BAD_TYPES[kind][1]
            wrong_actual = 'bool' if kind != 'bool' else 'int'
            variants = {
                'parameter-substring-prefix': good.replace("'input'", "'otherinput'"),
                'parameter-substring-suffix': good.replace("'input'", "'input_other'"),
                'expected-scalar-not-union': good.replace("'string or Label'", "'int'"),
                'expected-string-only': good.replace("'string or Label'", "'string'"),
                'expected-label-only': good.replace("'string or Label'", "'Label'"),
                'wrong-actual-type': good.replace("type '" + typename + "'", "type '" + wrong_actual + "'"),
                'reversed-expected-actual': "in call to Label(), parameter 'input' got value of type 'string or Label', want '" + typename + "'",
                'other-callee': good.replace('in call to Label()', 'in call to Other()'),
                'trailing-text': good + ' extra',
                'obsolete-pre-n33': obsolete_native_error(case),
                'oracle-token-reversal': 'Label input expected int, got string',
                'oracle-parameter-substring': 'Label otherinput expected string or Label, got int',
            }
            for mutation, diagnostic in variants.items():
                with self.subTest(kind=kind, mutation=mutation):
                    report = copy.deepcopy(self.report)
                    row = report['runs'][index]
                    row['stderr'] = suite.blob(suite.unblob(row['stderr']).replace(good.encode(), diagnostic.encode()))
                    self.reject(report, index, 'native-type-binding', mutation, 'wrong native type/parameter category')

    def test_missing_reversed_duplicated_extra_or_echo_cold_callbacks_rehashed(self):
        for case in suite.BUILDS:
            for engine in ('reference', 'native'):
                for stage in (('cold', 'warm') if engine == 'native' else ('cold',)):
                    index = self.row_id([case, engine, stage, 'build'])
                    original = suite.markers(suite.unblob(self.report['runs'][index]['stderr']).decode())
                    variants = {'missing': [], 'duplicate': original + original,
                                'extra': original + ['LP_UNRELATED_CALLBACK'],
                                'source-echo-only': ['    print("' + m + '")' for m in original]}
                    if len(original) == 2:
                        variants.update(reversed=list(reversed(original)), missing_repository=original[:1],
                                        missing_extension=original[1:])
                    for mutation, replacement in variants.items():
                        with self.subTest(case=case, engine=engine, stage=stage, mutation=mutation):
                            report = copy.deepcopy(self.report)
                            row = report['runs'][index]
                            rest = '\n'.join(line for line in suite.unblob(row['stderr']).decode().splitlines()
                                             if line not in original)
                            row['stderr'] = suite.blob(('\n'.join(replacement) + '\n' + rest + '\n').encode())
                            self.reject(report, index, 'producer-phases', mutation, 'wrong producer callback sequence/count/phase')

    def test_later_reference_callbacks_bounded_by_observation_or_total_suppression(self):
        for (case, stage, verb), expected in REFERENCE_CALLBACK_OBSERVATIONS.items():
            if (stage, verb) == ('cold', 'build'):
                self.assertEqual(suite.callback_sequences([case, 'reference', stage, verb]), (expected,))
                continue
            index = self.row_id([case, 'reference', stage, verb])
            self.assertEqual(suite.callback_sequences([case, 'reference', stage, verb]), ((), expected))
            suppressed = copy.deepcopy(self.report)
            suppressed['runs'][index]['stderr'] = suite.blob(b'')
            # Silence is permitted only here; actual output/cache/log assertions still run.
            self.assertEqual(suite.replay(suppressed, self.config, self.binding), 'BOUNDED_PAIRED_PASS')
            for observed in (expected + expected, ('LP_REPOSITORY_CALLBACK',), ('LP_OTHER_CALLBACK',),
                             ('LP_REPOSITORY_CALLBACK', 'LP_EXTENSION_CALLBACK')):
                report = copy.deepcopy(self.report)
                report['runs'][index]['stderr'] = suite.blob(('\n'.join(observed) + '\n').encode())
                with self.subTest(case=case, stage=stage, verb=verb, observed=observed):
                    self.reject(report, index, 'producer-phases', 'invalid-late-' + repr(observed),
                                'wrong producer callback sequence/count/phase')

    def test_same_basename_detached_execution_paths_rehashed_live_and_replay(self):
        for case in suite.BUILDS:
            index = self.row_id([case, 'reference', 'cold', 'build'])
            for field, basename in (('inputs', 'data.txt'), ('actualOutputs', 'result.txt')):
                for prefix in ('unrelated/', 'bazel-out/other-config/bin/', 'external/+files+unrelated/'):
                    with self.subTest(case=case, field=field, prefix=prefix):
                        report = copy.deepcopy(self.report)
                        row = report['runs'][index]
                        spawn = json.loads(suite.unblob(row['execution_log']))
                        entry = next(p for p in spawn[field] if Path(p['path']).name == basename)
                        old, new = entry['path'], prefix + basename
                        entry['path'] = new
                        # Keep the log internally coherent too; the independent cquery must catch it.
                        spawn['commandArgs'][-1] = spawn['commandArgs'][-1].replace(old, new)
                        if field == 'actualOutputs':
                            spawn['listedOutputs'] = [new]
                        row['execution_log'] = suite.blob(json.dumps(spawn).encode())
                        self.reject(report, index, 'execution-artifact-binding', field + ':' + new,
                                    'consumer execution paths detached from captured artifacts')

    def test_literal_consumer_relationship_and_digest_are_not_basename_claims(self):
        index = self.row_id(['custom', 'reference', 'cold', 'build'])
        for mutation in ('command', 'input-digest', 'output-digest', 'target', 'duplicate-input', 'listed-output'):
            report = copy.deepcopy(self.report)
            row = report['runs'][index]
            spawn = json.loads(suite.unblob(row['execution_log']))
            if mutation == 'command':
                spawn['commandArgs'][-1] = 'cat unrelated/data.txt > unrelated/result.txt'
            elif mutation.endswith('digest'):
                spawn['inputs' if mutation == 'input-digest' else 'actualOutputs'][0]['digest']['hash'] = '0' * 64
            elif mutation == 'target':
                spawn['targetLabel'] = '//:unrelated'
            elif mutation == 'duplicate-input':
                spawn['inputs'].append(copy.deepcopy(spawn['inputs'][0]))
            else:
                spawn['listedOutputs'] = ['unrelated/result.txt']
            row['execution_log'] = suite.blob(json.dumps(spawn).encode())
            with self.subTest(mutation=mutation):
                self.reject(report, index, 'execution-artifact-binding', mutation,
                            'literal producer-to-consumer command differs|consumer execution (?:paths|digest) detached')

    def test_artifact_and_log_both_renamed_still_require_fixture_root(self):
        for case in suite.BUILDS:
            report = copy.deepcopy(self.report)
            index = self.row_id([case, 'reference', 'cold', 'build'])
            row, query = report['runs'][index:index + 2]
            spawn = json.loads(suite.unblob(row['execution_log']))
            old = spawn['inputs'][0]['path']
            new = ('bazel-out/unit-cpu-fastbuild/bin/unrelated/data.txt' if case == 'custom'
                   else 'external/+files+unrelated/data.txt')
            spawn['inputs'][0]['path'] = new
            spawn['commandArgs'][-1] = spawn['commandArgs'][-1].replace(old, new)
            row['execution_log'] = suite.blob(json.dumps(spawn).encode())
            query['stdout'] = suite.blob(suite.unblob(query['stdout']).replace(old.encode(), new.encode()))
            key = next(p for p in query['outputs'] if p.endswith('/' + old))
            query['outputs'][key[:-len(old)] + new] = query['outputs'].pop(key)
            with self.subTest(case=case):
                self.reject(report, index, 'execution-artifact-binding', 'both-log-and-artifact-producer-renamed',
                            'producer outside fixture artifact root')

    def test_reference_configuration_prefix_comes_from_artifacts_not_platform_guess(self):
        for prefix in ('arm64-opt', 'portable-other-fastbuild'):
            report, config, binding = synthetic_report(True, config_prefix=prefix)
            self.assertEqual(suite.replay(report, config, binding), 'REFERENCE_PREPARATION_ONLY')

    def test_live_reference_build_cannot_pass_until_adjacent_artifacts_bound(self):
        index = self.row_id(['custom', 'reference', 'cold', 'build'])
        build, query = copy.deepcopy(self.report['runs'][index:index + 2])
        build['result'] = suite.build_result(build, self.scratch, build['step'])
        self.assertEqual(build['result'], 'REFERENCE_BUILD_PENDING_OUTPUT_BINDING')
        report = copy.deepcopy(self.report)
        report['runs'][index] = copy.deepcopy(build)
        with self.assertRaisesRegex(ValueError, 'observation result/scope mismatch'):
            suite.replay(report, self.config, self.binding)
        suite.complete_reference_pair([build, query], self.scratch)
        self.assertEqual(build['result'], 'PARITY_PRODUCER_CONSUMER')
        query['step'][2] = 'warm'
        with self.assertRaisesRegex(ValueError, 'detached live build/query pair'):
            suite.complete_reference_pair([build, query], self.scratch)


class NativeTerminalFooterTests(unittest.TestCase):
    reject = OracleMutationTests.reject

    def setUp(self):
        self.report, self.config, self.binding = synthetic_report()
        self.scratch = Path(self.report['scratch'])
        self.index = next(r['id'] for r in self.report['runs']
                          if r['step'] == ['wrong-type-none', 'native', 'query', 'query'])
        # Current retained row (post-N33 body, current footer). The legacy-footer
        # variant below is synthetic: the same actual body plus the legacy literal,
        # which the validator still accepts for older captures.
        self.retained = suite.unblob(BINDING_OBSERVATIONS['wrong-type-none']['stderr'])
        current = suite.NATIVE_LOADING_FOOTER.encode()
        assert self.retained.endswith(b'\n' + current)
        self.body = self.retained[:-len(current)]
        self.footer = suite.LEGACY_NATIVE_LOADING_FOOTER.encode()
        self.raw = self.body + self.footer
        self.report['runs'][self.index]['stderr'] = suite.blob(self.raw)

    def test_exact_retained_bytes_accept_without_modifying_raw_receipt(self):
        self.assertEqual((len(self.retained), suite.sha(self.retained)), BINDING_STDERR_PINS['wrong-type-none'])
        self.report['runs'][self.index]['stderr'] = suite.blob(self.retained)
        before = copy.deepcopy(self.report)
        self.assertEqual(suite.query_result(self.report['runs'][self.index], 'wrong-type-none', 'native'),
                         'PARITY_REJECTION_CATEGORY')
        # Only the envelope is synthetic; the stderr above is actual retained bytes.
        self.assertEqual(suite.replay(self.report, self.config, self.binding), 'BOUNDED_PAIRED_PASS')
        self.assertEqual(self.report, before)

    def test_obsolete_retained_607_bytes_now_rejected(self):
        self.assertEqual(len(OBSOLETE_NATIVE_NONE_STDERR), 607)
        self.assertEqual(suite.sha(OBSOLETE_NATIVE_NONE_STDERR),
                         '75289874d51a1fe479db7fe7ac01dde329d37715057df90f1fd84f0f73332e31')
        report = copy.deepcopy(self.report)
        report['runs'][self.index]['stderr'] = suite.blob(OBSOLETE_NATIVE_NONE_STDERR)
        self.reject(report, self.index, 'native-terminal-footer', 'obsolete-pre-n33-capture',
                    'wrong native type/parameter category')

    def test_footer_literal_is_bound_to_real_package_and_main_print_sites(self):
        self.assertEqual(self.footer, suite.LEGACY_NATIVE_LOADING_FOOTER.encode())
        package = (suite.ROOT / 'src/package.rs').read_text()
        main = (suite.ROOT / 'src/main.rs').read_text()
        self.assertIn('pub const NATIVE_LOADING_FOOTER: &str = "' + suite.NATIVE_LOADING_FOOTER[:-1] + '";', package)
        self.assertIn('"{e:#}\\n{NATIVE_LOADING_FOOTER}"', package)
        self.assertNotIn(suite.LEGACY_NATIVE_LOADING_FOOTER[:-1], package)
        self.assertIn('eprintln!("bazel-rs: {error}");', main)
        self.assertIn(suite.ROOT / 'src/package.rs', suite.source_paths())
        self.assertIn(suite.ROOT / 'src/main.rs', suite.source_paths())

    def test_current_footer_accepts_and_its_mutations_reject(self):
        footer = suite.NATIVE_LOADING_FOOTER.encode()
        row = copy.deepcopy(self.report['runs'][self.index])
        row['stderr'] = suite.blob(self.body + footer)
        self.assertEqual(suite.query_result(row, 'wrong-type-none', 'native'), 'PARITY_REJECTION_CATEGORY')
        variants = {
            'duplicate': self.body + footer + footer,
            'mixed-legacy-and-current': self.body + self.footer + footer,
            'missing-final-lf': self.body + footer[:-1],
            'changed-period': self.body + footer.replace(b'.md).', b'.md)!'),
            'changed-prefix': self.body + footer.replace(b'Supported rules:', b'Available rules:'),
            'case-changed': self.body + footer.upper(),
            'previous-footer-wording': self.body + suite.PREVIOUS_NATIVE_LOADING_FOOTER.encode()
            .replace(b'them.', b'them!'),
            'embedded-in-body': self.body + b'error: quoted ' + footer + footer,
        }
        for mutation, raw in variants.items():
            report = copy.deepcopy(self.report)
            report['runs'][self.index]['stderr'] = suite.blob(raw)
            with self.subTest(mutation=mutation):
                self.reject(report, self.index, 'native-terminal-footer', mutation,
                            '(?:modified or nonterminal|duplicate or embedded) native loading footer')

    def test_footer_is_optional_not_a_claim_about_all_native_diagnostic_counts(self):
        row = copy.deepcopy(self.report['runs'][self.index])
        row['stderr'] = suite.blob(self.body)
        self.assertEqual(suite.query_result(row, 'wrong-type-none', 'native'), 'PARITY_REJECTION_CATEGORY')
        self.assertEqual(suite.unblob(row['stderr']), self.body)

    def test_modified_duplicate_nonterminal_or_source_echo_footer_rehashed_rejected(self):
        variants = {
            'duplicate': self.body + self.footer + self.footer,
            'duplicate-separated': self.body + self.footer + b'\n' + self.footer,
            'nonterminal-text': self.raw + b'trailing ordinary text\n',
            'nonterminal-blank-line': self.raw + b'\n',
            'footer-before-body': self.footer + self.body,
            'missing-final-lf': self.body + self.footer[:-1],
            'crlf': self.body + self.footer[:-1] + b'\r\n',
            'indented': self.body + b' ' + self.footer,
            'trailing-space': self.body + self.footer[:-1] + b' \n',
            'changed-period': self.body + self.footer.replace(b'unsupported.', b'unsupported!'),
            'changed-prefix': self.body + self.footer.replace(b'Supported rules:', b'Available rules:'),
            'changed-to-supported': self.body + self.footer.replace(b'unsupported', b'supported'),
            'case-changed': self.body + self.footer.upper(),
            'source-echo-only': self.body + b'    print("' + self.footer[:-1] + b'")\n',
            'source-echo-plus-terminal': self.body + b'    print("' + self.footer[:-1] + b'")\n' + self.footer,
            'embedded-in-body': self.body + b'error: quoted ' + self.footer + self.footer,
        }
        for mutation, raw in variants.items():
            report = copy.deepcopy(self.report)
            report['runs'][self.index]['stderr'] = suite.blob(raw)
            with self.subTest(mutation=mutation):
                self.reject(report, self.index, 'native-terminal-footer', mutation,
                            '(?:modified or nonterminal|duplicate or embedded) native loading footer')

    def test_body_failure_or_action_before_or_after_footer_rehashed_rejected(self):
        for line in (b'error: unsupported additional API\n', b'falling back to genuine Bazel\n',
                     b'1 action cache hit\n', b'INFO: Build completed successfully, 1 total action\n',
                     b'panicked at parser\n', b'failed to download archive\n', b'name Label is not defined\n'):
            for position in ('before', 'after'):
                report = copy.deepcopy(self.report)
                raw = self.body + line + self.footer if position == 'before' else self.raw + line
                report['runs'][self.index]['stderr'] = suite.blob(raw)
                with self.subTest(line=line, position=position):
                    self.reject(report, self.index, 'native-terminal-footer', position + ':' + line.decode().rstrip('\n'),
                        'fallback/infrastructure/crash/global failure|query executed action/callback|modified or nonterminal native loading footer')

    def test_reference_positive_and_build_cannot_use_native_negative_exception(self):
        for step in (['wrong-type-none', 'reference', 'query', 'query'],
                     ['invalid-getter-after-passthrough', 'reference', 'query', 'query'],
                     ['positive', 'native', 'query', 'query'], ['custom', 'native', 'cold', 'build']):
            index = next(r['id'] for r in self.report['runs'] if r['step'] == step)
            report = copy.deepcopy(self.report)
            row = report['runs'][index]
            row['stderr'] = suite.blob(suite.unblob(row['stderr']) + self.footer)
            with self.subTest(step=step):
                self.reject(report, index, 'native-terminal-footer', 'not-native-negative:' + repr(step),
                            'fallback/infrastructure/crash/global failure')

    def test_exact_footer_does_not_relax_parameter_type_callsite_or_phase(self):
        variants = {
            'wrong-parameter': self.body.replace(b"parameter 'input'", b"parameter 'otherinput'"),
            'wrong-expected-union': self.body.replace(b"want 'string or Label'", b"want 'int'"),
            'wrong-actual-type': self.body.replace(b"type 'NoneType'", b"type 'bool'"),
            'reversed-types': self.body.replace(b"type 'NoneType', want 'string or Label'",
                                                b"type 'string or Label', want 'NoneType'"),
            'wrong-site': self.body.replace(b'//:probe.bzl:3', b'//:probe.bzl:4'),
            'wrong-expression': self.body.replace(b'result = Label(None)', b'result = Label(True)'),
            'source-echo-before-marker': self.body.replace(b'BEFORE_CALL\n', b'    print("BEFORE_CALL")\n'),
            'late-failure': self.body + b'UNEXPECTED_AFTER_CALL\n',
        }
        for mutation, body in variants.items():
            report = copy.deepcopy(self.report)
            report['runs'][self.index]['stderr'] = suite.blob(body + self.footer)
            with self.subTest(mutation=mutation):
                self.reject(report, self.index, 'native-terminal-footer', mutation,
                            'wrong native type/parameter category|wrong target operation/callsite|not a bound loading rejection')


class NativeBindingObservationTests(unittest.TestCase):
    reject = OracleMutationTests.reject

    def setUp(self):
        self.report, self.config, self.binding = synthetic_report()
        self.scratch = Path(self.report['scratch'])

    def row_id(self, case):
        return next(r['id'] for r in self.report['runs'] if r['step'] == [case, 'native', 'query', 'query'])

    def mutate(self, case, raw, name, criterion):
        report = copy.deepcopy(self.report)
        index = self.row_id(case)
        report['runs'][index]['stderr'] = suite.blob(raw)
        self.reject(report, index, 'native-binding-observations', name, criterion)

    def test_portable_exact_raw_fields_hashes_source_and_provenance(self):
        self.assertEqual(BINDING_FIXTURE['schema'], 1)
        origin = BINDING_FIXTURE['origin']
        self.assertEqual(origin['report_sha256'], '03a17107759f2eba4e8e427168f0333cec912c276df846d77f5948a0ca5feef1')
        self.assertEqual(origin['native_sha256'], 'eac59a1eca721b985b39e8397bf13f06bf944dfd2fa306ff4faea7fee6a91e73')
        self.assertEqual(origin['reference_sha256'], suite.REFERENCE_SHA)
        self.assertEqual(list(BINDING_OBSERVATIONS), list(suite.CALLS))
        self.assertEqual(list(BINDING_OBSERVATIONS), list(BINDING_STDERR_PINS))
        # Obsolete pre-N33 provenance stays pinned and unmodified.
        obsolete = OBSOLETE_BINDING_FIXTURE['origin']
        self.assertEqual(obsolete['report_sha256'], '630620c1d40bfe0b6877ff1894086b5c45c29c896bb7977ff554d1e4995c8aca')
        self.assertEqual(obsolete['native_sha256'], '6e6bdf3d13d9a2970a6a47c3eb5edd48d072fa6b282b8b382ec77e095a7d2ed2')
        plan = suite.plan(False)
        for case, observed in BINDING_OBSERVATIONS.items():
            with self.subTest(case=case):
                self.assertEqual(observed['id'], plan.index([case, 'native', 'query', 'query']))
                self.assertEqual(observed['step'], [case, 'native', 'query', 'query'])
                raw = suite.unblob(observed['stderr'])
                self.assertEqual((len(raw), suite.sha(raw)), BINDING_STDERR_PINS[case])
                self.assertEqual(suite.unblob(observed['stdout']), b'')
                self.assertEqual(observed['returncode'], 1)
                self.assertEqual(observed['probe'], suite.inputs(case)['probe.bzl'])
                self.assertIn(observed['cwd'].encode(), raw)
                row = query_row(case, 'native')
                before = copy.deepcopy(row)
                self.assertEqual(suite.query_result(row, case, 'native'), 'PARITY_REJECTION_CATEGORY')
                self.assertEqual(row, before)
        self.assertEqual(suite.replay(self.report, self.config, self.binding), 'BOUNDED_PAIRED_PASS')

    def test_reference_diagnostics_are_not_normalized_to_native_wording(self):
        # Same Bazel body, but the reference oracle still requires Java's
        # "Error in Label: " frame, never the native "error: " line.
        for case in BINDING_OBSERVATIONS:
            with self.subTest(case=case):
                suite.diagnostic(case, 'reference', 'Error in Label: ' + reference_error(case) + '\n')
                with self.assertRaisesRegex(ValueError, 'wrong reference type/binding category'):
                    suite.diagnostic(case, 'reference', 'error: ' + native_error(case) + '\n')

    def test_arity_wrong_expected_actual_and_reversed_counts_rehashed(self):
        bads = {'missing': ['Label() missing 0 required positional argument: input',
                            'Label() missing 2 required positional arguments: input',
                            'Label() missing 1 required positional argument: label_string',
                            'Label() missing 1 required positional argument: inputs',
                            'Other() missing 1 required positional argument: input',
                            'Label() accepts no more than 1 positional argument but got 0',
                            obsolete_native_error('missing')],
                'extra': ['Label() accepts no more than 1 positional argument but got 1',
                          'Label() accepts no more than 1 positional argument but got 3',
                          'Label() accepts no more than 2 positional arguments but got 2',
                          'Label() accepts no more than 0 positional arguments but got 2',
                          'Other() accepts no more than 1 positional argument but got 2',
                          'Label() accepts no more than 1 positional argument but got 20',
                          obsolete_native_error('extra')]}
        for case, variants in bads.items():
            raw = suite.unblob(BINDING_OBSERVATIONS[case]['stderr'])
            good = native_error(case).encode()
            self.assertIn(good, raw)
            for bad in variants:
                with self.subTest(case=case, bad=bad):
                    self.mutate(case, raw.replace(good, bad.encode()), 'counts-' + bad,
                                'wrong native arity|wrong native missing parameter')

    def test_wrong_substring_duplicate_keyword_and_other_callee_rehashed(self):
        for case in KEYWORD_CASES:
            raw = suite.unblob(BINDING_OBSERVATIONS[case]['stderr'])
            good = native_error(case)
            self.assertIn(good.encode(), raw)
            key = 'label_string' if case == 'keyword-native-name' else 'unknown' if case == 'keyword-unknown' else 'input'
            wrong = 'unknown' if key != 'unknown' else 'input'
            positional_only = 'positional-only' in good
            other_form = ("Label() got unexpected keyword argument '" + key + "'" if positional_only
                          else "Label() got named argument for positional-only parameter '" + key + "'")
            variants = {
                'wrong-keyword': good.replace("'" + key + "'", "'" + wrong + "'"),
                'keyword-prefix': good.replace("'" + key + "'", "'other" + key + "'"),
                'keyword-suffix': good.replace("'" + key + "'", "'" + key + "_other'"),
                'duplicate-keyword': good.replace("'" + key + "'", "'" + key + "' '" + key + "'"),
                'extra-keyword': good.replace("'" + key + "'", "'" + key + "', 'additional'"),
                'other-function': good.replace('Label()', 'Other()'),
                'other-binding-form': other_form,
                'extra-token-only': 'got unexpected keyword argument',
                'obsolete-pre-n33': obsolete_native_error(case),
            }
            for name, bad in variants.items():
                with self.subTest(case=case, mutation=name):
                    self.mutate(case, raw.replace(good.encode(), bad.encode()), name, 'wrong native positional-only binding')

    def test_real_Label_call_frame_span_and_phase_not_unrelated_source_echo(self):
        footer = suite.NATIVE_LOADING_FOOTER.encode()
        for case in BINDING_OBSERVATIONS:
            raw = suite.unblob(BINDING_OBSERVATIONS[case]['stderr'])
            body = raw[:-len(footer)]
            source = ('result = ' + suite.CALLS[case]).encode()
            other = source.replace(b'result = Label(', b'result = Other(', 1)
            frame = b'  * //:probe.bzl:3, in run_probe\n      '
            variants = {
                'other-frame-with-correct-span': body.replace(frame + source, frame + other),
                'other-span-with-correct-frame': body.replace(b'3 |     ' + source, b'3 |     ' + other),
                'other-function-with-Label-decoy': body.replace(source, other) + b'quoted source: ' + source + b'\n',
                'extra-other-frame': body + frame + other + b'\n',
                'wrong-callsite-line': body.replace(b'//:probe.bzl:3', b'//:probe.bzl:4'),
                'wrong-callsite-column': body.replace(b'//:probe.bzl:3:14', b'//:probe.bzl:3:15'),
                'wrong-caller': body.replace(b'in run_probe\n', b'in unrelated_function\n'),
                'before-marker-source-echo': body.replace(b'BEFORE_CALL\n', b'    print("BEFORE_CALL")\n'),
                'after-call-reached': body + b'UNEXPECTED_AFTER_CALL\n',
            }
            for name, changed in variants.items():
                with self.subTest(case=case, mutation=name):
                    self.mutate(case, changed + footer, name,
                                'wrong native binder operation/callsite|wrong target operation/callsite|not a bound loading rejection')

    def test_duplicate_primary_and_crash_fallback_action_footer_rehashed(self):
        footer = suite.NATIVE_LOADING_FOOTER.encode()
        for case in BINDING_OBSERVATIONS:
            raw = suite.unblob(BINDING_OBSERVATIONS[case]['stderr'])
            body = raw[:-len(footer)]
            variants = {
                'duplicate-primary': body + ('error: ' + native_error(case) + '\n').encode() + footer,
                'crash': body + b'panicked at evaluator\n' + footer,
                'fallback-before': body + b'falling back to genuine Bazel\n' + footer,
                'fallback-after': raw + b'falling back to genuine Bazel\n',
                'action-before': body + b'1 action cache hit\n' + footer,
                'action-after': raw + b'INFO: Build completed successfully, 1 total action\n',
                'duplicate-footer': raw + footer,
                'nonterminal-footer': raw + b'\n',
                'modified-footer': body + footer.replace(b'(docs/CONFIGURATION.md).', b'(docs/CONFIGURATION.md)!'),
            }
            for name, changed in variants.items():
                with self.subTest(case=case, mutation=name):
                    self.mutate(case, changed, name, 'one native primary diagnostic required|fallback/infrastructure/crash/global failure|'
                                'query executed action/callback|modified or nonterminal native loading footer|duplicate or embedded native loading footer')


class NativePublicationRootTests(unittest.TestCase):
    reject = OracleMutationTests.reject

    def setUp(self):
        self.report, self.config, self.binding = synthetic_report()
        self.scratch = Path(self.report['scratch'])
        self.observed = PUBLICATION_OBSERVATION['publication']
        self.identity = self.observed['identity']
        self.manifest_name = self.identity + '.json'
        # Unit envelopes are synthetic. Only the publication bytes/metadata are
        # actual retained cold evidence; this does not invent a warm CLI receipt.
        for row in self.report['runs']:
            if row['step'][0:2] == ['extension', 'native'] and row['step'][3] == 'build':
                row['publication'] = copy.deepcopy(self.observed)
                old, new = '/trees/' + 'a' * 64 + '/', '/trees/' + self.identity + '/'
                row['outputs'] = {p.replace(old, new): item for p, item in row['outputs'].items()}
                row['stdout'] = suite.blob(suite.unblob(row['stdout']).replace(old.encode(), new.encode()))
        self.index = next(r['id'] for r in self.report['runs'] if r['step'] == ['extension', 'native', 'cold', 'build'])

    def mutate_manifest(self, name, change):
        report = copy.deepcopy(self.report)
        item = report['runs'][self.index]['publication']['manifests'][self.manifest_name]
        manifest = json.loads(suite.unblob(item['raw']))
        change(manifest)
        item['raw'] = suite.blob(json.dumps(manifest).encode())
        self.assertEqual(item['raw'], suite.blob(suite.unblob(item['raw'])))
        self.reject(report, self.index, 'native-publication-root', name, 'publication manifest content')
        MUTATION_PROOFS[-1].update(manifest_hash_valid=True, rebuilt_manifest_sha256=item['raw']['sha256'])

    def test_actual_manifest_provenance_and_exact_root_are_preserved(self):
        origin = PUBLICATION_OBSERVATION['origin']
        self.assertEqual(origin['report_sha256'], '75c7eefc6a9d5247c30ebdf6651221256c24c43a83c66fd1c6e5f23758fe85fa')
        self.assertEqual((origin['run_id'], origin['step'], origin['returncode']),
                         (25, ['extension', 'native', 'cold', 'build'], 0))
        self.assertEqual(origin['native_sha256'], '6e6bdf3d13d9a2970a6a47c3eb5edd48d072fa6b282b8b382ec77e095a7d2ed2')
        # origin pins the store source of run 25 (provenance, never rewritten);
        # the current store is bound separately by the legacy-manifest
        # round-trip unit proof (repository_store.rs retained_legacy_manifest_*).
        self.assertEqual(origin['repository_store_sha256'], '1d0a0f1564f3eb757564e849f78614b72028206541ca208bb4d1f3194e24673a')
        compatibility = PUBLICATION_OBSERVATION['store_compatibility']
        self.assertEqual(compatibility['basis'], 'retained_legacy_manifest_bytes_round_trip_without_a_symlink_field')
        self.assertEqual(suite.sha((suite.ROOT / 'src/repository_store.rs').read_bytes()), compatibility['repository_store_sha256'])
        item = self.observed['manifests'][self.manifest_name]
        raw = suite.unblob(item['raw'])
        self.assertEqual((len(raw), suite.sha(raw)), (578, 'c6152455538a6170868d15b1ca9f983702efaa580d6436586bb3bb7dfb31dc62'))
        self.assertEqual(item['mode'], 0o600)  # Metadata-file observation, not a source/output mode rule.
        data = json.loads(raw)
        self.assertEqual(data['files'][''], dict(directory=True, mode=0o755, size=0, sha256=''))
        self.assertNotEqual(data['files']['']['sha256'], suite.sha(b''))
        before = copy.deepcopy(self.observed)
        suite.check_publication(self.observed)
        self.assertEqual(self.observed, before)
        self.assertEqual(suite.replay(self.report, self.config, self.binding), 'BOUNDED_PAIRED_PASS')

    def test_missing_regular_wrong_mode_size_digest_and_typed_root_rejected(self):
        self.mutate_manifest('root-missing', lambda m: m['files'].pop(''))
        self.mutate_manifest('root-regular-file', lambda m: m['files'].__setitem__('',
                             dict(directory=False, mode=0o644, size=0, sha256=suite.sha(b''))))
        for field, value in (('directory', False), ('directory', 1), ('mode', 0o700), ('mode', 0o644),
                             ('mode', 0o555), ('mode', 0o775), ('mode', 493.0), ('size', 1), ('size', -1),
                             ('size', False), ('size', 0.0), ('sha256', suite.sha(b'')), ('sha256', 'f' * 64), ('sha256', None)):
            with self.subTest(field=field, value=value):
                self.mutate_manifest('root-' + field + '-' + repr(value),
                                     lambda m, f=field, v=value: m['files'][''].__setitem__(f, v))
        self.mutate_manifest('root-extra-field', lambda m: m['files'][''].__setitem__('unrelated', 1))

    def test_unknown_and_traversal_entries_are_not_normalized_or_ignored(self):
        for path in ('unrelated.txt', 'nested/unplanned.txt', '../data.txt', '/data.txt',
                     'nested/../data.txt', './data.txt', 'nested//data.txt'):
            with self.subTest(path=path):
                self.mutate_manifest('unexpected-path-' + path,
                                     lambda m, p=path: m['files'].__setitem__(p, copy.deepcopy(m['files']['data.txt'])))
        for root in ('.', '/', '..', '../root'):
            with self.subTest(root=root):
                self.mutate_manifest('renamed-root-' + root,
                                     lambda m, p=root: m['files'].__setitem__(p, m['files'].pop('')))

    def test_planned_child_records_remain_exact_after_adding_root(self):
        for path in ('BUILD.bazel', 'REPO.bazel', 'data.txt'):
            for field, value in (('directory', True), ('directory', 0), ('mode', 0o555), ('size', 999),
                                 ('sha256', '0' * 64)):
                with self.subTest(path=path, field=field, value=value):
                    self.mutate_manifest('child-' + path + '-' + field + '-' + repr(value),
                                         lambda m, p=path, f=field, v=value: m['files'][p].__setitem__(f, v))
        self.mutate_manifest('empty-file-size-bool', lambda m: m['files']['REPO.bazel'].__setitem__('size', False))
        self.mutate_manifest('format-bool', lambda m: m.__setitem__('format', True))

    def test_source_0644_consumer_0555_and_actual_bytes_are_not_relaxed(self):
        for mutation in ('source-mode', 'source-bytes', 'source-extra-entry', 'consumer-mode'):
            report = copy.deepcopy(self.report)
            row = report['runs'][self.index]
            inventory = row['publication']['inventory']
            if mutation == 'source-mode':
                inventory['data.txt']['mode'] = 0o555
            elif mutation == 'source-bytes':
                inventory['data.txt']['raw'] = suite.blob(b'wrong canonical label data\n')
                suite.unblob(inventory['data.txt']['raw'])
            elif mutation == 'source-extra-entry':
                inventory['unrelated'] = copy.deepcopy(inventory['data.txt'])
            else:
                path = next(p for p in row['outputs'] if Path(p).name == 'result.txt')
                row['outputs'][path]['mode'] = 0o644
            with self.subTest(mutation=mutation):
                self.reject(report, self.index, 'native-publication-root', mutation,
                            'repository source inventory/0644/bytes|output bytes/hash/exact sealed mode')


if __name__ == '__main__':
    unittest.main()
