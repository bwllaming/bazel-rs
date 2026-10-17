"""Portable unit/mutation tests: never execute native or genuine Bazel."""
import base64
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import validate_label_repo_name as qa


def data_for(case):
    """Synthetic oracle inputs, NOT retained genuine/native execution receipts."""
    main, dep, other = '@@//:root.txt', '@@dep+//:dep.txt', '@@other+//:other.txt'
    if case == 'analysis':
        return dict(root=list(map(qa.surface, [main]*4+[dep, other,
            '@@unfetched+segments+name//no/package:no_target'])), equal=[True]*6,
            analysis=list(map(qa.surface, ['@@//:result']+['@@dep++observe+made//:data.txt']*3)))
    return dict(external=list(map(qa.surface, [dep, dep, other, main])),
        extension=qa.surface('@@dep+//:extension.bzl'),
        participants=[['root_contract', False, 'ABSENT'], ['dep', False, 'ABSENT']],
        tags=[[owner, key, qa.surface(label)] for owner, key, label in [
            ('root_contract', 'root-local', main), ('root_contract', 'root-shared', dep),
            ('root_contract', 'root-self', main), ('dep', 'dep-local', dep),
            ('dep', 'dep-shared', other), ('dep', 'dep-self', dep), ('dep', 'dep-default', dep)]])


def streams(row, stdout='', stderr=''):
    for key, text in [('stdout', stdout), ('stderr', stderr)]:
        raw = text.encode()
        row.update({key: text, key+'_base64': base64.b64encode(raw).decode(), key+'_sha256': qa.digest(raw),
                    key+'_full_sha256': qa.digest(raw), key+'_full_size_bytes': len(raw)})
    return row


def negative(case, engine='reference'):
    """Source-backed synthetic diagnostics test the checker, never count as probes."""
    external = case.startswith('external')
    path = 'dep/probe.bzl' if external else 'probe.bzl'
    line = 3 if engine == 'native' and case in qa.DIVERGENCE else 5
    source = qa.fixture(case)[path].decode().splitlines()[line-1].strip()
    phase = 'N1_BEGIN '+case+'\n'
    if line == 5:
        phase += 'N1_CONSTRUCTED '+case+' Label\n'
    site = ('File "/fixture/'+path+'", line '+str(line)+', column 5, in probe\n' if engine == 'reference' else
            ' --> '+('@@dep+//' if external else '//')+':probe.bzl:'+str(line)+':5\n')
    if engine == 'reference':
        message = ('cannot set .repo_name field of Label value' if case == 'assignment' else
            "'Label' value has no field or method 'missing_field'" if case == 'unknown-field' else
            "'repo_name' is not allowed on invalid Label")
    else:
        message = ('error: Operation `.repo_name=` not supported on type `Label`' if case == 'assignment' else
            'error: Object of type `Label` has no attribute `missing_field`' if case == 'unknown-field' else
            'unknown apparent repository @'+('' if case == 'external-empty' else 'not_visible')+
            ' from canonical repository '+('"dep+"' if external else '""')+
            '; extension repository not materialized or dependency not declared')
    return streams(dict(returncode=7 if engine == 'reference' else 1, client_reaped=True),
                   stderr=phase+site+'    '+source+'\n'+message+'\n')


def synthetic_report():
    """Complete synthetic transcript for mutation tests, not reference evidence."""
    scratch = Path('/private/synthetic-label-test')
    args = qa.parser().parse_args(['--reference', '/pinned/ref', '--official-source', '/pinned/source',
        '--scratch-parent', '/private', '--output', '/synthetic', '--reference-only'])
    pins = dict(sources=qa.fingerprints(qa.source_paths()), official={qa.JAVA: qa.JAVA_SHA},
        platforms_distfile=qa.platform_evidence(scratch, staged=False),
        registry={str(scratch / 'registry' / Path(p).relative_to(qa.REGISTRY)): item
            for p, item in qa.fingerprints(p for p in qa.REGISTRY.rglob('*') if p.is_file()).items()},
        executables={'reference': dict(path=str(args.reference), sha256=qa.REFERENCE_SHA256, size_bytes=1)})
    report = dict(status='SYNTHETIC_UNIT_TEST_ONLY', errors=[], pins_before=pins, pins_after=copy.deepcopy(pins),
        executables=copy.deepcopy(pins['executables']), scratch=str(scratch), limitations=qa.LIMITATIONS,
        contract_sha256=qa.CONTRACT_SHA, cleanup=dict(path=str(scratch), removed=True), runs=[], observations=[],
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()})

    def run(name, argv, cwd, stdout='', stderr='', code=0):
        i = len(report['runs'])
        row = streams(dict(id=i, name=name, argv=argv, cwd=str(cwd), timeout_seconds=180, stdin_bytes=0,
            controlled_environment=qa.controlled.encoded_environment(qa.controlled.controlled_environment(scratch, {})),
            owned_client_pid=10000+i, group_cleanup=dict(pgid=10000+i, checked=True, killed_remaining=False),
            client_reaped=True, returncode=code, platforms_distfile_sha256=qa.PLATFORM_SHA,
            executable_sha256=qa.REFERENCE_SHA256 if argv[0] == '/pinned/ref' else None), stdout, stderr)
        report['runs'].append(row)
        return row

    def git(phase):
        for suffix, words in [('head', ['rev-parse', 'HEAD']), ('status', ['status', '--porcelain=v1', '--untracked-files=no'])]:
            run('official.'+phase+'.'+suffix, ['/usr/bin/git', '--no-optional-locks', '-C', str(args.official_source)]+words,
                scratch, qa.COMMIT+'\n' if suffix == 'head' else '')
    git('before')
    run('reference.version', qa.argv_for(args, scratch, 'version', 'reference', 'version'), scratch, 'bazel 9.3.0\n')
    for case, state, command in qa.sequence('reference'):
        workspace = scratch / 'reference' / case / 'workspace'
        inputs = {p: qa.encoded(raw) for p, raw in qa.fixture(case).items()}
        obs = dict(case=case, state=state, command=command, engine='reference', inputs=inputs,
            inputs_after=copy.deepcopy(inputs), run=len(report['runs']),
            classification='INTENTIONAL_DIVERGENCE' if case in qa.DIVERGENCE else 'PAIRED_GETTER')
        report['observations'].append(obs)
        stdout, stderr, code = '', '', 0
        if case not in qa.POSITIVE:
            n = negative(case)
            stderr, code = n['stderr'], 7
            obs['no_outputs'] = True
        else:
            if state == 'cold' and command == 'build':
                stderr = 'N1_EXTENSION\nN1_REPO\n'+('N1_ANALYSIS\n' if case == 'analysis' else '')
            if command == 'cquery':
                raw = (json.dumps(data_for(case), sort_keys=True)+'\n').encode()
                names = ['data.txt', 'result.txt'] if case == 'pipeline' else ['result.txt']
                lines = [('external/dep++observe+made/' if name == 'data.txt' else 'bazel-out/k8-fastbuild/bin/')+name for name in names]
                stdout = '\n'.join(lines)+'\n'
                paths = qa.reference_output_paths(stdout, workspace, workspace.parent / 'output')
                outputs = {}
                for name, path in zip(names, paths):
                    mode = 0o644 if name == 'data.txt' else 0o555
                    outputs[name] = dict(qa.encoded(raw), path=str(path), logical_path=str(path), mode=mode, kind='file')
                obs['outputs'] = outputs
                obs['roots'] = {'data.txt': str(Path(outputs['data.txt']['path']).parent)} if case == 'pipeline' else {}
        run('.'.join(('reference', case, state, command)), qa.argv_for(args, scratch, case, 'reference', command),
            workspace, stdout, stderr, code)
    git('after')
    return report


def reorder_rows(report, order):
    """Adversarial chronology edit with consistent IDs, links and stream hashes."""
    old = report['runs']
    remap = {old_index: new_index for new_index, old_index in enumerate(order)}
    report['runs'] = [old[i] for i in order]
    for i, row in enumerate(report['runs']):
        row['id'] = i
        if 'stdout' in row:
            streams(row, row['stdout'], row['stderr'])
    for obs in report['observations']:
        obs['run'] = remap[obs['run']]


class LabelRepoNameTests(unittest.TestCase):
    def test_plan_is_small_and_complete(self):
        self.assertEqual(len(qa.sequence('reference')), 13)
        self.assertEqual(len(qa.sequence('native')), 9)
        self.assertEqual(len(qa.CASES), 7)
        self.assertEqual(len(qa.DIVERGENCE), 3)

    def test_fixtures_are_derived_and_portable(self):
        for case in qa.CASES:
            files = qa.fixture(case)
            self.assertEqual(files, qa.fixture(case))
            for path, raw in files.items():
                self.assertNotIn(b'/tmp/', raw)
                self.assertNotIn(b'Label(dep)', raw)
                self.assertNotIn(b'dir(VALUE)', raw)
                self.assertFalse(Path(path).is_absolute())
            self.assertNotIn(b'Label(', files['BUILD.bazel'])
            self.assertNotIn(b'print(', files['BUILD.bazel'])
        with self.assertRaises(ValueError):
            qa.fixture('unknown')

    def test_selected_surface_mutations(self):
        generated = '@@actual+discovered+canonical//:data.txt'
        data = dict(root=list(map(qa.surface, ['@@//:root.txt']*4+['@@dep+//:dep.txt', '@@other+//:other.txt',
            '@@unfetched+segments+name//no/package:no_target'])), equal=[True]*6,
            analysis=[qa.surface('@@//:result')]+[qa.surface(generated)]*3)
        qa.check_data('analysis', data)
        for field, index, value in [('analysis', 0, 'wrong'), ('root', 1, 'shared'),
                                    ('root', 3, 'string'), ('root', 4, 'Label'),
                                    ('root', 6, False), ('root', 7, False), ('root', 8, 'OTHER')]:
            bad = copy.deepcopy(data)
            bad[field][0][index] = value
            with self.subTest(field=field, index=index), self.assertRaises(ValueError):
                qa.check_data('analysis', bad)
        bad = copy.deepcopy(data)
        bad['equal'][0] = False
        with self.assertRaises(ValueError):
            qa.check_data('analysis', bad)

    def test_exact_byte_evidence(self):
        raw = b'actual\x00bytes\xff\n'
        self.assertEqual(qa.decoded(qa.encoded(raw)), raw)
        for field, value in [('sha256', '0'*64), ('size_bytes', 1), ('bytes_base64', 'invalid')]:
            bad = qa.encoded(raw)
            bad[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                qa.decoded(bad)

    def test_negative_probe_barriers(self):
        for case in qa.CASES[2:]:
            files = qa.fixture(case)
            path = 'dep/probe.bzl' if case.startswith('external') else 'probe.bzl'
            lines = files[path].decode().splitlines()
            self.assertIn('N1_BEGIN '+case, lines[1])
            self.assertIn('Label(', lines[2])
            self.assertIn('N1_CONSTRUCTED '+case, lines[3])
            self.assertIn('value.', lines[4])
            self.assertIn('N1_AFTER '+case, lines[5])
            bad = dict(files, **{path: files[path]+b'#changed\n'})
            with self.assertRaises(ValueError):
                qa.rejection({}, case, 'native', bad)

    def test_private_environment_and_no_fallback(self):
        env = qa.controlled.controlled_environment(Path('/private'), {})
        self.assertEqual(set(env), {'PATH', 'HOME', 'TMPDIR', 'LC_ALL'})
        self.assertEqual(env['PATH'], '/usr/bin:/bin')
        args = qa.parser().parse_args(['--reference', '/pinned/ref', '--official-source', '/source',
            '--scratch-parent', '/private', '--output', '/out', '--reference-only'])
        command = qa.argv_for(args, Path('/private'), 'pipeline', 'reference', 'build')
        self.assertIn('--batch', command)
        self.assertIn('--ignore_all_rc_files', command)
        self.assertIn('--repository_disable_download', command)
        self.assertFalse(any('https://' in x for x in command))
        self.assertEqual(args.timeout, 180)

    def test_full_mode_requires_pin(self):
        with self.assertRaisesRegex(ValueError, 'explicit --native'):
            qa.main(['--reference', '/missing/ref', '--official-source', '/missing/source',
                '--scratch-parent', '/missing/private', '--output', '/missing/output'])

    def test_retained_first_failure_is_exact_not_pass(self):
        item = json.loads((ROOT / 'tests/fixtures/label_repo_name/preparation_blocker.json').read_bytes())
        self.assertEqual(qa.digest(item['stderr'].encode()), item['stderr_sha256'])
        self.assertEqual(item['input_sha256'], {p: qa.digest(raw) for p, raw in qa.fixture('pipeline').items()})
        self.assertEqual(item['returncode'], 1)
        self.assertEqual(item['command_count'], 6)
        self.assertTrue(item['before_after_pins_equal'])
        self.assertTrue(item['scratch_removed'])
        self.assertEqual(item['reference_sha256'], qa.REFERENCE_SHA256)
        row = dict(returncode=1, client_reaped=True, stderr=item['stderr'])
        with self.assertRaisesRegex(ValueError, 'uncontrolled process failure'):
            qa.controlled.healthy(row)

    def test_synthetic_negative_categories_and_mutations(self):
        for engine in ('reference', 'native'):
            for case in qa.CASES[2:]:
                row = negative(case, engine)
                qa.rejection(row, case, engine, qa.fixture(case))
                changes = [row['stderr'].replace('probe.bzl', 'other.bzl'),
                    row['stderr'].replace('N1_BEGIN', 'WRONG_PHASE'),
                    row['stderr']+'N1_REPO\n', row['stderr']+'N1_AFTER '+case+'\n',
                    row['stderr']+'panicked\n', row['stderr'].replace('Label', 'struct'),
                    row['stderr'].replace('repo_name', 'workspace_name') if case != 'unknown-field' else
                    row['stderr'].replace('missing_field', 'other_field')]
                # Eager native errors have no Label type/getter text by design.
                for text in changes:
                    if text == row['stderr']:
                        continue
                    with self.subTest(case=case, engine=engine, text=text), self.assertRaises(ValueError):
                        qa.rejection(streams(dict(row), stderr=text), case, engine, qa.fixture(case))
                for update in [dict(returncode=0), dict(stdout='//:result\n'), dict(timed_out=True), dict(client_reaped=False)]:
                    with self.subTest(case=case, engine=engine, update=update), self.assertRaises(ValueError):
                        qa.rejection(dict(row, **update), case, engine, qa.fixture(case))

    def test_full_synthetic_replay(self):
        self.assertTrue(qa.validate_report(synthetic_report()))

    def test_replay_source_pin_and_registry_mutations(self):
        for category in ('sources', 'registry', 'official', 'executables'):
            report = synthetic_report()
            report['pins_before'][category] = {}
            report['pins_after'][category] = {}
            with self.subTest(category=category), self.assertRaises(ValueError):
                qa.validate_report(report)
        report = synthetic_report()
        path = next(iter(report['pins_before']['sources']))
        for field in ('pins_before', 'pins_after'):
            report[field]['sources'][path]['sha256'] = '0'*64
        with self.assertRaises(ValueError):
            qa.validate_report(report)

    def test_replay_command_binding_and_coverage_mutations(self):
        for field, value in [('argv', ['/fake']), ('cwd', '/outside'), ('name', 'other-case'),
                             ('executable_sha256', '0'*64), ('timeout_seconds', 181), ('stdin_bytes', 1)]:
            report = synthetic_report()
            report['runs'][3][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                qa.validate_report(report)
        for operation in ('remove', 'duplicate', 'reorder', 'fixture', 'classification'):
            report = synthetic_report()
            obs = report['observations']
            if operation == 'remove':
                obs.pop()
            elif operation == 'duplicate':
                obs.append(copy.deepcopy(obs[-1]))
            elif operation == 'reorder':
                obs.reverse()
            elif operation == 'fixture':
                obs[0]['inputs']['BUILD.bazel'] = qa.encoded(b'filegroup(name="result")\n')
                obs[0]['inputs_after'] = copy.deepcopy(obs[0]['inputs'])
            else:
                obs[-1]['classification'] = 'PAIRED_GETTER'
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                qa.validate_report(report)

    def test_replay_environment_stream_crash_cleanup_mutations(self):
        for operation in ('ambient', 'stream', 'hash', 'crash', 'group', 'scratch'):
            report = synthetic_report()
            row = report['runs'][3]
            if operation == 'ambient':
                row['controlled_environment']['GOROOT'] = 'forbidden'
            elif operation == 'stream':
                row['stderr'] = 'invented'
            elif operation == 'hash':
                row['stderr_full_sha256'] = '0'*64
            elif operation == 'crash':
                streams(row, stderr='panicked\n')
            elif operation == 'group':
                row['group_cleanup']['checked'] = False
            else:
                report['cleanup']['removed'] = False
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                qa.validate_report(report)

    def test_replay_output_and_cache_mutations(self):
        for operation in ('bytes', 'mode', 'type', 'path', 'warm-path', 'listing', 'root', 'negative-output'):
            report = synthetic_report()
            obs = next(o for o in report['observations'] if 'outputs' in o)
            output = obs['outputs']['result.txt']
            if operation == 'bytes':
                output.update(qa.encoded(b'{}\n'))
            elif operation == 'mode':
                output['mode'] = 0o644
            elif operation == 'type':
                output['kind'] = 'symlink'
            elif operation == 'path':
                output['path'] = '/outside/result.txt'
            elif operation == 'warm-path':
                warm = next(o for o in report['observations'] if o['state'] == 'warm' and 'outputs' in o)
                warm['outputs']['result.txt']['path'] += '-changed'
            elif operation == 'listing':
                streams(report['runs'][obs['run']], stdout='unrelated.txt\n')
            elif operation == 'root':
                obs['roots']['data.txt'] += '-changed'
            else:
                report['observations'][-1]['no_outputs'] = False
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                qa.validate_report(report)

    def test_positive_warm_callback_reemission_allowed(self):
        report = synthetic_report()
        for obs in report['observations']:
            if obs['case'] in qa.POSITIVE and obs['state'] == 'warm':
                row = report['runs'][obs['run']]
                streams(row, row['stdout'], 'N1_EXTENSION\nN1_REPO\n'+('N1_ANALYSIS\n' if obs['case'] == 'analysis' else ''))
        self.assertTrue(qa.validate_report(report))

    def test_positive_cold_phase_is_required(self):
        report = synthetic_report()
        streams(report['runs'][3])
        with self.assertRaises(ValueError):
            qa.validate_report(report)

    def test_publication_inventory_is_byte_and_manifest_bound(self):
        cache = Path('/private/cache')
        identity = 'a'*64
        root = qa.artifacts.trees_directory(cache) / identity
        manifest_path = str(cache / 'repositories/direct/manifests' / (identity+'.json'))
        files = {'REPO.bazel': b'', 'generated.bzl': b'VALUE = Label("//:data.txt")\n',
            'BUILD.bazel': b'exports_files(["data.txt", "generated.bzl"], visibility=["//visibility:public"])\n',
            'data.txt': (json.dumps(data_for('pipeline'))+'\n').encode()}
        inventory = {name: dict(qa.encoded(raw), path=str(root / name), kind='file', mode=0o644)
                     for name, raw in files.items()}
        entries = {'': dict(directory=True, mode=0o700, size=0, sha256='')}
        entries.update({name: dict(directory=False, mode=0o644, size=len(raw), sha256=qa.digest(raw))
                        for name, raw in files.items()})
        manifest = dict(format=1, profile='direct-file-template-v1', identity=identity, files=entries)
        obs = dict(publications=[identity], metadata={manifest_path: dict(qa.encoded(json.dumps(manifest).encode()),
            path=manifest_path, kind='file', mode=0o644)}, inventory={str(root): dict(mode=0o700, files=inventory)},
            outputs={'data.txt': inventory['data.txt']})
        qa.check_inventory(obs, cache)
        for operation in ('extra-publication', 'manifest', 'inventory-bytes', 'inventory-mode', 'source-root'):
            bad = copy.deepcopy(obs)
            if operation == 'extra-publication':
                bad['publications'].append('b'*64)
            elif operation == 'manifest':
                changed = copy.deepcopy(manifest)
                changed['files']['data.txt']['sha256'] = '0'*64
                bad['metadata'][manifest_path].update(qa.encoded(json.dumps(changed).encode()))
            elif operation == 'inventory-bytes':
                bad['inventory'][str(root)]['files']['generated.bzl'].update(qa.encoded(b'VALUE=None\n'))
            elif operation == 'inventory-mode':
                bad['inventory'][str(root)]['files']['data.txt']['mode'] = 0o755
            else:
                bad['outputs']['data.txt']['path'] = '/private/other/data.txt'
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                qa.check_inventory(bad, cache)

    def test_portable_platforms_release_and_exact_archive(self):
        item = json.loads(qa.PLATFORM_FIXTURE.read_bytes())
        raw = qa.platform_bytes(item)
        self.assertEqual(len(raw), 7879)
        self.assertEqual(qa.digest(raw), qa.PLATFORM_SHA)
        self.assertEqual(raw[:2], b'\x1f\x8b')
        self.assertEqual(item['provenance']['reference_source_commit'], qa.COMMIT)

    def test_platforms_bad_hash_base64_release_and_provenance(self):
        item = json.loads(qa.PLATFORM_FIXTURE.read_bytes())
        for field, value in [('sha256', '0'*64), ('size_bytes', 1), ('version', '1.0.1'),
                             ('basename', '../platforms.tar.gz'), ('release_url', 'https://untrusted.invalid'),
                             ('bytes_base64', '!invalid'), ('bytes_base64', base64.b64encode(b'fake').decode())]:
            bad = copy.deepcopy(item)
            bad[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                qa.platform_bytes(bad)
        for field in qa.PLATFORM_PROVENANCE:
            bad = copy.deepcopy(item)
            bad['provenance'][field] = 'untrusted'
            with self.subTest(provenance=field), self.assertRaises(ValueError):
                qa.platform_bytes(bad)

    def test_platforms_private_staging_required_and_verified(self):
        with tempfile.TemporaryDirectory(prefix='label-platforms-') as directory:
            scratch = Path(directory)
            with self.assertRaises(FileNotFoundError):
                qa.platform_evidence(scratch)
            evidence = qa.stage_platforms(scratch)
            self.assertEqual(evidence, qa.platform_evidence(scratch, staged=False))
            path = scratch / 'distdir' / qa.PLATFORM_NAME
            path.write_bytes(b'not the genuine archive')
            with self.assertRaises(ValueError):
                qa.platform_evidence(scratch)
            path.unlink()
            path.symlink_to(qa.PLATFORM_FIXTURE)
            with self.assertRaises(ValueError):
                qa.platform_evidence(scratch)

    def test_platforms_flags_are_offline_and_replay_bound(self):
        report = synthetic_report()
        for row in report['runs']:
            if row['name'].startswith('reference.') and row['name'] != 'reference.version':
                self.assertIn('--distdir='+report['scratch']+'/distdir', row['argv'])
                self.assertIn('--repository_disable_download', row['argv'])
                self.assertIn('--lockfile_mode=off', row['argv'])
                self.assertIn('--incompatible_autoload_externally=', row['argv'])
                self.assertEqual([a for a in row['argv'] if a.startswith('--registry=')],
                                 ['--registry='+Path(report['scratch'], 'registry').as_uri()])
        for operation in ('missing-distdir', 'other-distdir', 'downloads', 'registry', 'precommand'):
            bad = copy.deepcopy(report)
            row = bad['runs'][3]
            if operation == 'missing-distdir':
                row['argv'] = [a for a in row['argv'] if not a.startswith('--distdir=')]
            elif operation == 'other-distdir':
                row['argv'] = ['--distdir=/ambient' if a.startswith('--distdir=') else a for a in row['argv']]
            elif operation == 'downloads':
                row['argv'].remove('--repository_disable_download')
            elif operation == 'registry':
                row['argv'].append('--registry=https://bcr.bazel.build')
            else:
                row['platforms_distfile_sha256'] = '0'*64
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                qa.validate_report(bad)

    def test_platforms_report_mutations(self):
        for operation in ('absent', 'bytes', 'path', 'mode', 'provenance', 'provenance-hash', 'fixture-hash'):
            report = synthetic_report()
            item = report['pins_before']['platforms_distfile']
            if operation == 'absent':
                report['pins_before'].pop('platforms_distfile')
            elif operation == 'bytes':
                item['archive'].update(qa.encoded(b'fake'))
            elif operation == 'path':
                item['archive']['path'] = '/ambient/platforms-1.0.0.tar.gz'
            elif operation == 'mode':
                item['archive']['mode'] = 0o755
            elif operation == 'provenance':
                item['provenance'] = dict(item['provenance'], source_sha256='0'*64)
            elif operation == 'provenance-hash':
                item['provenance_sha256'] = '0'*64
            else:
                item['fixture_sha256'] = '0'*64
            report['pins_after'] = copy.deepcopy(report['pins_before'])
            with self.subTest(operation=operation), self.assertRaises(ValueError):
                qa.validate_report(report)

    def test_retained_genuine_analysis_completed_output_with_bindings(self):
        item = json.loads((ROOT / 'tests/fixtures/label_repo_name/analysis_completed_output.json').read_bytes())
        self.assertEqual(item['report_sha256'], 'cd18124b58bcfedfa0e6b262faa6436e8ede9ac2b021d63e9b3153a9783cf076')
        self.assertEqual(item['original_status'], 'FAIL')
        self.assertEqual(item['original_errors'], ['ValueError: wrong output file/mode', 'replay: failed run or pin drift'])
        self.assertTrue(item['pins_before_after_equal'])
        self.assertTrue(item['inputs_before_after_equal'])
        self.assertEqual(item['input_sha256'], {p: qa.digest(raw) for p, raw in qa.fixture('analysis').items()})
        self.assertEqual(item['official'], {qa.JAVA: qa.JAVA_SHA})
        # Deliberately re-pinned for N23d (docs/ACTIONS_PARAMS.md): the retained
        # output was produced with the pre-N23d action.rs (native-action-v5).
        # src/action.rs changed since (tree artifacts, parameter files,
        # transforms; later N41's Bazel spawn argv and remote cache, native-action-v8)
        # but keeps OUTPUT_MODE 0555 for
        # completed regular outputs, the property this retained output pins.
        self.assertEqual(item['source_pins']['src/action.rs']['sha256'],
                         '18084fd1f3b2ff8405e6697899c24388ded2f2e501078350242868caf570008d')
        self.assertIn(b'const OUTPUT_MODE: u32 = 0o555;', (ROOT / 'src/action.rs').read_bytes())
        self.assertEqual(item['source_pins']['scripts/validate_label_repo_name.py']['sha256'],
                         '1e16cd72777aa6011ab89c9e7e36d482f480eb3082db9ca2a63589d33aeac79e')
        row, scratch = item['command'], Path(item['scratch'])
        args = SimpleNamespace(reference=Path(row['argv'][0]))
        self.assertEqual((row['id'], row['name'], row['returncode']), (8, 'reference.analysis.cold.cquery', 0))
        self.assertEqual(row['executable_sha256'], qa.REFERENCE_SHA256)
        self.assertEqual(row['argv'], qa.argv_for(args, scratch, 'analysis', 'reference', 'cquery'))
        self.assertEqual(row['cwd'], str(scratch / 'reference/analysis/workspace'))
        build = item['preceding_build']
        self.assertEqual((build['id'], build['name'], build['returncode']), (7, 'reference.analysis.cold.build', 0))
        self.assertEqual(build['argv_sha256'], qa.digest(json.dumps(
            qa.argv_for(args, scratch, 'analysis', 'reference', 'build'), sort_keys=True).encode()))
        output = item['output']
        listed = qa.reference_output_paths(row['stdout'], Path(row['cwd']), scratch / 'reference/analysis/output')
        self.assertEqual(listed, [Path(output['logical_path'])])
        self.assertEqual(output['logical_path'], output['path'])
        self.assertTrue(Path(output['path']).is_relative_to(scratch))
        self.assertEqual(output['sha256'], '36e52e79f5ec4302188f29ca6921ce75dd27d4f82a40816a888f90ed25afa5d5')
        self.assertEqual(output['size_bytes'], 1179)
        qa.check_outputs('analysis', {'result.txt': output})
        self.assertNotIn(b'is_executable', qa.fixture('analysis')['consumer.bzl'])

    def test_analysis_completed_mode_rejects_incorrect_permissions(self):
        item = json.loads((ROOT / 'tests/fixtures/label_repo_name/analysis_completed_output.json').read_bytes())
        self.assertEqual(item['output']['mode'], 0o555)
        for mode in (0o444, 0o700, 0o644, 0o755, 0o4555, 0):
            bad = dict(item['output'], mode=mode)
            with self.subTest(mode=oct(mode)), self.assertRaisesRegex(ValueError, 'wrong output file/mode'):
                qa.check_outputs('analysis', {'result.txt': bad})
        bad = dict(item['output'], kind='symlink')
        with self.assertRaises(ValueError):
            qa.check_outputs('analysis', {'result.txt': bad})

    def test_replay_reports_criterion_failure_separately_from_pin_drift(self):
        report = synthetic_report()
        report['errors'] = ['ValueError: wrong output file/mode']
        with self.assertRaisesRegex(ValueError, '^recorded execution or criterion failure: ValueError: wrong output file/mode$'):
            qa.validate_report(report)
        report['pins_after']['official'] = {}
        with self.assertRaisesRegex(ValueError, '^before/after pin drift$'):
            qa.validate_report(report)

    def test_chronology_rejects_reordered_workloads_with_rebound_ids(self):
        for operation in ('cquery-before-build', 'warm-before-cold'):
            report = synthetic_report()
            order = list(range(len(report['runs'])))
            if operation == 'cquery-before-build':
                order[3:5] = [4, 3]
            else:
                order[3:7] = [5, 6, 3, 4]
            reorder_rows(report, order)
            for obs in report['observations']:
                self.assertEqual(report['runs'][obs['run']]['name'],
                                 '.'.join((obs['engine'], obs['case'], obs['state'], obs['command'])))
            with self.subTest(operation=operation), self.assertRaisesRegex(ValueError, 'chronological run plan mismatch'):
                qa.validate_report(report)

    def test_chronology_rejects_auxiliary_relocation_with_filtered_order_preserved(self):
        auxiliary = ['official.before.head', 'official.before.status', 'reference.version',
                     'official.after.head', 'official.after.status']
        for operation in ('early-after-pins', 'late-version'):
            report = synthetic_report()
            order = list(range(len(report['runs'])))
            if operation == 'early-after-pins':
                order = order[:3]+order[-2:]+order[3:-2]
            else:
                order[2:4] = [3, 2]
            reorder_rows(report, order)
            self.assertEqual([r['name'] for r in report['runs'] if r['name'] in auxiliary], auxiliary)
            with self.subTest(operation=operation), self.assertRaisesRegex(ValueError, 'chronological run plan mismatch'):
                qa.validate_report(report)

    def test_complete_chronological_plan_all_modes(self):
        scratch = Path('/private/plan-only')
        for mode, count, engines in [('reference-only', 18, ['reference']),
                                     ('native-preparation', 14, ['native']),
                                     ('paired', 27, ['reference', 'native'])]:
            args = SimpleNamespace(reference_only=mode == 'reference-only', native_preparation=mode == 'native-preparation',
                reference=Path('/pinned/reference'), native=Path('/pinned/native'), native_sha256='a'*64,
                official_source=Path('/pinned/source'))
            plan = qa.ordered_run_plan(args, scratch)
            self.assertEqual(len(plan), count)
            report = dict(runs=[{k: v for k, v in p.items() if k != 'observation'} for p in plan],
                observations=[dict(zip(('run', 'engine', 'case', 'state', 'command'), (p['id'], *p['observation'])))
                              for p in plan if p['observation'] is not None])
            expected = [(e, *step) for e in engines for step in qa.sequence(e)]
            self.assertEqual([p['observation'] for p in plan if p['observation'] is not None], expected)
            qa.check_run_order(report, args, scratch)
            order = list(range(len(plan)))
            # Full mode crosses engines; other modes invert a cold/warm or build/cquery pair.
            right = 3+len(qa.sequence('reference')) if mode == 'paired' else 4
            order[3], order[right] = order[right], order[3]
            reorder_rows(report, order)
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, 'chronological run plan mismatch'):
                qa.check_run_order(report, args, scratch)

    def test_negative_query_live_guard_rejects_fallback_infrastructure_and_actions(self):
        additions = [('falling back to genuine Bazel\n', 'fallback or unrelated infrastructure'),
            ('fallback to genuine Bazel\n', 'fallback or unrelated infrastructure'),
            ('connection reset\n', 'fallback or unrelated infrastructure'),
            ('SSLHandshake failed\n', 'fallback or unrelated infrastructure'),
            ('1 actions executed, 0 cache hits\n', 'negative query reported action'),
            ('0 actions executed, 1 cache hits\n', 'negative query reported action'),
            ('1 action cache hit\n', 'negative query reported action'),
            ('INFO: Build completed successfully, 1 total action\n', 'negative query reported action')]
        for engine in ('reference', 'native'):
            for case in qa.CASES[2:]:
                original = negative(case, engine)
                qa.rejection(original, case, engine, qa.fixture(case))
                for text, error in additions:
                    row = streams(dict(original), original['stdout'], original['stderr']+text)
                    self.assertEqual(row['stderr_sha256'], qa.digest(row['stderr'].encode()))
                    with self.subTest(engine=engine, case=case, text=text), self.assertRaisesRegex(ValueError, error):
                        qa.rejection(row, case, engine, qa.fixture(case))

    def test_negative_query_replay_guards_use_recomputed_stream_evidence(self):
        additions = [('falling back to genuine Bazel\n', 'fallback or unrelated infrastructure'),
                     ('connection reset\n', 'fallback or unrelated infrastructure'),
                     ('1 actions executed, 0 cache hits\n', 'negative query reported action')]
        for case in qa.CASES[2:]:
            for text, error in additions:
                report = synthetic_report()
                obs = next(o for o in report['observations'] if o['case'] == case)
                row = report['runs'][obs['run']]
                streams(row, row['stdout'], row['stderr']+text)
                self.assertEqual(row['stderr_full_sha256'], qa.digest(row['stderr'].encode()))
                self.assertEqual(base64.b64decode(row['stderr_base64']), row['stderr'].encode())
                with self.subTest(case=case, text=text), self.assertRaisesRegex(ValueError, error):
                    qa.validate_report(report)

    def test_negative_replay_still_rejects_missing_getter_wrong_site_and_crash(self):
        for operation, error in [('missing-getter', 'wrong Label field/type'),
                                 ('wrong-source', 'rejection missing bound source/site'),
                                 ('crash', 'uncontrolled process failure')]:
            report = synthetic_report()
            obs = next(o for o in report['observations'] if o['case'] == 'root-unmapped')
            row = report['runs'][obs['run']]
            text = row['stderr']
            if operation == 'missing-getter':
                text = text.replace("'repo_name' is not allowed on invalid Label",
                                    "'Label' value has no field or method 'repo_name'")
            elif operation == 'wrong-source':
                text = text.replace('probe.bzl', 'unrelated.bzl')
            else:
                text += 'panicked\n'
            streams(row, row['stdout'], text)
            with self.subTest(operation=operation), self.assertRaisesRegex(ValueError, error):
                qa.validate_report(report)

    def test_positive_action_counters_are_not_negative_query_evidence(self):
        report = synthetic_report()
        for obs in report['observations']:
            if obs['case'] in qa.POSITIVE and obs['command'] == 'build':
                row = report['runs'][obs['run']]
                counters = '1 actions executed, 0 cache hits\n' if obs['state'] == 'cold' else '0 actions executed, 1 cache hits\n'
                streams(row, row['stdout'], row['stderr']+counters)
        self.assertTrue(qa.validate_report(report))


if __name__ == '__main__':
    unittest.main()
