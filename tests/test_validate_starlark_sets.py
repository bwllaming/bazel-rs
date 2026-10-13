"""Portable harness-only mutation tests. No binary or ignored report is executed.

Synthetic receipts below deliberately are NOT engine/parity evidence. Fixtures
and expectations come only from tracked test/harness source and registry files.
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
import validate_starlark_sets as s


def record(text='', code=1, stdout=''):
    return dict(stdout=stdout, stderr=text, returncode=code, timed_out=False)


def streams(r):
    for field in ('stdout', 'stderr'):
        raw = r[field].encode()
        r.update({field+'_base64': base64.b64encode(raw).decode(), field+'_sha256': s.digest(raw),
            field+'_full_sha256': s.digest(raw), field+'_full_size_bytes': len(raw)})
    return r


def entry(path, raw, mode=0o644):
    return dict(path=str(path), bytes_base64=base64.b64encode(raw).decode(), size_bytes=len(raw),
        sha256=s.digest(raw), kind='file', mode=mode)


MESSAGES = {
    'frozen': 'trying to mutate a frozen set value',
    'constructor-hash': "unhashable type: 'builtin_function_or_method'",
    'index': "type 'builtin_function_or_method' has no operator [](builtin_function_or_method)",
    'not-provider': "providers must be a list of providers, got builtin_function_or_method",
    'target-key': 'Target indexing requires an exported provider constructor',
    'unhashable-element': "unhashable type: 'list'", 'set-unhashable': "unhashable type: 'set'",
    'depset': 'depset expected list or tuple, got set', 'args': 'expected list or tuple, got set',
    'disabled-flag': 'unsupported option --incompatible_enable_starlark_set=false',
    'native-field': "Object has no attribute 'set'",
}
KEY_MESSAGES = {
    'membership-builtin': 'Type Target only supports querying by object constructors, got builtin_function_or_method instead',
    'membership-string': 'Type Target only supports querying by object constructors, got string instead',
    'membership-none': 'Type Target only supports querying by object constructors, got NoneType instead',
    'membership-int': 'Type Target only supports querying by object constructors, got int instead',
    'unexported-membership': "<target //:child> only supports querying by exported providers. Assign the provider a name in a top-level assignment statement.",
    'unexported-index': "<target //:child> only supports indexing by exported providers. Assign the provider a name in a top-level assignment statement.",
}
CRASH = ('DEBUG: /synthetic/defs.bzl:3:5: SET_BEFORE_SELF_XOR [1, 2]\n'
    'FATAL: bazel crashed due to an internal error.\n'
    'Caused by: java.util.ConcurrentModificationException\n'
    '\tat net.starlark.java.eval.StarlarkSet.symmetricDifferenceUpdate(StarlarkSet.java:788)\n')


def synthetic(reference_only=False):
    """Build non-executed receipts to test the independent replay validator."""
    args = s.argument_parser().parse_args(['--reference-only'] if reference_only else ['--native-sha256', 'a'*64])
    root = Path('/tmp/opencode/SYNTHETIC-NOT-EXECUTED-set-test')
    ws = root / 'workspace'
    ref = s.DirectReference(args.reference, ws, root, None, s.SUITE)
    sources = {str(p): dict(path=str(p), sha256='b'*64, size=1, mode=0o644) for p in s.SOURCES}
    registry = {}
    for p in s.REGISTRY.rglob('*'):
        if p.is_file():
            raw = p.read_bytes()
            sources[str(p)].update(sha256=s.digest(raw), size=len(raw))
            path = root / 'registry' / p.relative_to(s.REGISTRY)
            registry[str(path)] = entry(path, raw)
    executables = {'reference': dict(path=str(args.reference), sha256=s.REFERENCE_SHA256)}
    if not reference_only:
        executables['native'] = dict(path=str(args.native), sha256=args.native_sha256)
    official = dict(path=str(args.reference_source), commit=s.SOURCE_PIN, files=s.OFFICIAL_FILES)
    report = dict(schema_version=1, status='REFERENCE_ONLY_PASS' if reference_only else 'PASS_ONLY_BOUNDED_PROFILE',
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        plan=s.plan_receipt(reference_only), planned_checks=s.CASEPLAN, limitations=s.LIMITATIONS,
        known_reference_defects=s.KNOWN_REFERENCE_DEFECTS, integration_origin=s.INTEGRATION_ORIGIN,
        known_parity_gaps=s.KNOWN_PARITY_GAPS,
        native_claim=dict(status='NOT_RUN' if reference_only else 'EXECUTED_BOUNDED_PROFILE',
            commands=sum(len(s.steps_for(c['name'],p,e)) for c in s.CASEPLAN for p,e in s.phases_for(c,reference_only) if e=='native'),
            pass_claim=not reference_only),
        started_at='2026-10-13T00:00:00+00:00',finished_at='2026-10-13T00:01:00+00:00',
        errors=[], cleanup=[], sources_before=sources, sources_after=copy.deepcopy(sources),
        executables=executables, executables_after=copy.deepcopy(executables),
        official_source_before=official, official_source_after=copy.deepcopy(official),
        scratch_directory=str(root), owned_pids=[987654321], registry_before=registry,
        registry_after=copy.deepcopy(registry), runs=[], phases=[],
        cleanup_receipt=dict(path=str(root), removed=True, pids={'987654321': False}, remaining_pidfiles=[]))
    # No ignored genuine distfile needed: patch only the expected hash at call time.
    report['platforms_distfile'] = entry('/synthetic/platforms', b'SYNTHETIC DISTFILE NOT AN ARCHIVE')
    binding = s.digest(json.dumps(dict(executables=executables, source=official, production=sources), sort_keys=True).encode())

    def add(name, argv, r, timeout=args.timeout):
        r.update(id=len(report['runs']), name=name, argv=argv, cwd=str(ws), timeout_seconds=timeout,
            executable_sha256=next(v['sha256'] for v in executables.values() if v['path'] == argv[0]),
            preflight_binding_sha256=binding)
        report['runs'].append(streams(r))
        return r['id']

    add('reference.version', ref.startup+['version', '--gnu_format'], record('', 0, 'bazel 9.3.0\n'))
    for c in s.CASEPLAN:
        for probe, engine in s.phases_for(c, reference_only):
            family = c['name']
            v = s.fixture(family, probe)
            cache = root / 'cases' / family / probe / engine / 'cache'
            empty = {n: dict(path=str(cache / n), exists=False, entries=[]) for n in ('actions', 'work')}
            p = dict(family=family, probe=probe, engine=engine, cache=str(cache), runs=[], outputs={}, logs={},
                input_snapshots={}, extra_outputs={}, actions_before=empty, actions_after=copy.deepcopy(empty), forbidden_outputs=[],
                status='KNOWN_REFERENCE_DEFECT' if v['defect'] and engine == 'reference' else 'PASS')
            files = v['files'].copy()
            for i, step in enumerate(s.steps_for(family, probe, engine)):
                stage, cmd, target, _ = step
                if stage == 'changed' and cmd == 'build':
                    files[v['changed'][0]] = v['changed'][1]
                p['input_snapshots'][str(i)] = {str(ws / name): entry(ws / name, raw) for name, raw in files.items()}
                r = record('', 0)
                if stage == 'defect':
                    r = record(CRASH, 37)
                elif stage == 'reject':
                    diagnostic=v['diagnostic']
                    message=(KEY_MESSAGES[diagnostic] if engine=='reference' else
                        'Target '+('indexing' if diagnostic=='unexported-index' else 'membership')+' requires an exported provider constructor') if diagnostic in KEY_MESSAGES else MESSAGES[diagnostic]
                    r = record('ERROR: '+message+'\n', 1)
                    if engine == 'reference':
                        p['logs']['reject'] = dict(path=str(cache.parent / 'reject.execution.json'), absent=True)
                if stage != 'defect' and cmd == 'build':
                    r['stderr'] += ''.join(m+'\n' for m in v['markers'])
                    if not v['diagnostic'] and engine == 'native':
                        r['stderr'] += ('0 actions executed, '+str(v['actions'])+' cache hits\n' if stage == 'warm' else str(v['actions'])+' actions executed, 0 cache hits\n')
                    elif not v['diagnostic']:
                        spawns = []
                        if stage in ('cold', 'changed'):
                            names = ['H2SetProducer', 'H2SetConsumer'] if family == 'provider' else ['H2ExtensionConsumer'] if family == 'extension' or probe == 'module-bzl' else []
                            for n in names:
                                expected = v['changed'][2] if stage=='changed' else v['expected']
                                produced = b'B2\n' if stage=='changed' else b'B\n'
                                ins = {'b.txt':produced} if n=='H2SetProducer' else {'bazel-out/bin/producer.generated':produced,'a.txt':b'A\n'} if n=='H2SetConsumer' else {'external/+ext+generated/data':expected}
                                out = produced if n=='H2SetProducer' else expected
                                spawns.append(dict(mnemonic=n,exitCode=0,cacheHit=False,
                                    inputs=[dict(path=k,digest=dict(hash=s.digest(b))) for k,b in ins.items()],
                                    actualOutputs=[dict(path='bazel-out/bin/'+('producer.generated' if n=='H2SetProducer' else 'consume.out'),digest=dict(hash=s.digest(out)))]))
                        raw = '\n'.join(json.dumps(x) for x in spawns).encode()
                        p['logs'][stage] = entry(cache.parent / (stage+'.execution.json'), raw)
                        if stage == 'restart':
                            r['stderr'] += 'INFO: 1 process: 2 action cache hit, 1 internal.\n'
                if stage in ('cold', 'warm', 'restart', 'changed', 'repository') and cmd == ('build' if engine == 'native' else 'cquery'):
                    raw = v['changed'][2] if stage in ('changed', 'repository') else v['expected']
                    if stage == 'repository':
                        path = (cache / 'external/+ext+generated/data' if engine == 'native' else ref.output_base / 'execroot/_main/external/+ext+generated/data')
                    else:
                        path = (cache / 'actions' / ('d'*64 if stage != 'changed' else 'e'*64) / 'outputs/consume.out' if engine == 'native' else ref.output_base / 'execroot/_main/bazel-out/k8-fastbuild/bin/consume.out')
                    label = '@@+ext+generated//:data' if stage == 'repository' else target
                    r['stdout'] = label+'\t'+str(path)+'\n' if engine == 'native' else str(path.relative_to(ref.output_base / 'execroot/_main'))+'\n'
                    p['outputs'][stage] = [entry(path, raw, 0o644 if stage == 'repository' else 0o555)]
                    if engine=='native' and stage!='repository':
                        p['actions_after']['actions']['exists']=True
                        p['actions_after']['actions']['entries'].append(str(path.relative_to(cache/'actions')))
                    if stage == 'repository':
                        p['extra_outputs']['BUILD.bazel'] = entry(path.parent / 'BUILD.bazel', s.REPO_BUILD)
                    if family == 'provider':
                        raws = [b'B\n', b'B2\n'] if stage == 'changed' and engine == 'native' else [b'B2\n' if stage == 'changed' else b'B\n']
                        p['extra_outputs'][stage] = [entry((cache / 'actions' / str(j) / 'producer.generated' if engine == 'native' else path.parent / 'producer.generated'), b, 0o555) for j,b in enumerate(raws)]
                if stage == 'default-empty':
                    r['stdout'] = ''
                if stage == 'canonical':
                    r['stdout'] = '@@+ext+generated//:data\n'
                if stage == 'graph':
                    r['stdout'] = json.dumps(dict(
                        actions=[dict(mnemonic='H2SetProducer',inputDepSetIds=[1],outputIds=[1]),dict(mnemonic='H2SetConsumer',inputDepSetIds=[2])],
                        pathFragments=[dict(id=1,label='bazel-out/bin/producer.generated'),dict(id=2,label='b.txt'),dict(id=3,label='a.txt')],
                        artifacts=[dict(id=i,pathFragmentId=i) for i in (1,2,3)],
                        depSetOfFiles=[dict(id=1,directArtifactIds=[2]),dict(id=2,directArtifactIds=[1,3])]))
                rid = add(family+'.'+probe+'.'+engine+'.'+str(i), s.command_argv(args, ref, engine, cache, root, step), r,
                    min(args.timeout, 15) if cmd == 'shutdown' else args.timeout)
                p['runs'].append(rid)
            p['inputs_after'] = {str(ws / name): entry(ws / name, raw) for name, raw in files.items()}
            report['phases'].append(p)
    add(s.SUITE+'.shutdown', ref.startup+['shutdown']+list(s.COMMON_FLAGS), record('', 0), min(args.timeout, 15))
    report['completed_phases'] = len(report['phases'])
    report['membership_gap_closure'] = s.membership_closure(report,reference_only)
    return report


class PlanTests(unittest.TestCase):
    def test_exact_plan_counts_and_starlark_syntax(self):
        self.assertEqual((len(s.CASEPLAN), s.plan_receipt(True)['phases'], s.plan_receipt(False)['phases']), (7,29,62))
        self.assertEqual((s.plan_receipt(True)['commands'],s.plan_receipt(False)['commands']), (81,136))
        for c in s.CASEPLAN:
            self.assertEqual(len(c['probes']), len(set(c['probes'])))
            for p in c['probes']:
                for name, raw in s.fixture(c['name'],p)['files'].items():
                    if name.endswith(('.bzl','.bazel')):
                        tree = ast.parse(raw)
                        self.assertFalse(any(isinstance(n,ast.AugAssign) for n in tree.body))

    def test_unknown_probe_fails_closed(self):
        for family, probe in [('invented','metadata'),('constructor','invented')]:
            with self.assertRaises(ValueError):s.fixture(family,probe)

    def test_native_boundaries_are_not_differential(self):
        item = next(c for c in s.CASEPLAN if c['name']=='boundaries')
        for p,e in s.phases_for(item,True):self.assertNotIn(p,s.NATIVE_ONLY)
        self.assertTrue(all(e=='native' for p,e in s.phases_for(item) if p in s.NATIVE_ONLY))

    def test_genuine_provider_only_returns_p_and_explicit_file_list(self):
        self.assertIn('return [P(files = set([generated, ctx.file.a, generated, ctx.file.a]))]',s.PROVIDER)
        self.assertNotIn('DefaultInfo',s.PROVIDER.split('def _consumer')[0])
        self.assertIn('files = list(ctx.attr.dep[P].files)',s.PROVIDER)
        self.assertIn('inputs = files',s.PROVIDER)
        v=s.fixture('provider','generated-closure')
        self.assertEqual(v['expected'],b'B\nA\n')
        self.assertEqual(v['changed'],('b.txt',b'B2\n',b'B2\nA\n'))

    def test_extension_and_frozen_are_real_not_sdk_mocks(self):
        for token in ('repository_rule(', 'module_extension(', 'ctx.file(', 'items = list(items)', 'ctx.actions.run_shell('):
            self.assertIn(token,s.EXTENSION)
        for token in ('alias = s', 's |=', 's -=', 's &=', 's ^=', 'base_after='):
            self.assertIn(token,s.FROZEN)

    def test_original_functional_bytes_are_pinned_without_ignored_artifacts(self):
        # Mechanically read from the genuine contract identified by INTEGRATION_ORIGIN.
        pins = {
            ('provider','generated-closure'): {'MODULE.bazel':'9f927de287fea15bbb2dcb9151767814c04df52d2e4424fc3e3b8711ac79647d','BUILD.bazel':'26e5ae516a406f2f7c3c8897bb4f3ad3c0749aa51725204820c06a9a99b56890','defs.bzl':'44404d9e83e955dd94ee66c8bedb0c4693a9d597ade79e4c218d7be82b24c85d','a.txt':'06f961b802bc46ee168555f066d28f4f0e9afdf3f88174c1ee6f9de004fc30a0','b.txt':'c0cde77fa8fef97d476c10aad3d2d54fcc2f336140d073651c2dcccf1e379fd6'},
            ('frozen','factory'): {'MODULE.bazel':'a40408e174d09058de7ac71e93f5e884ff809fba96895207d09b769173d9b52e','BUILD.bazel':'37fe3e49669d18f8df53a884dadcd6e8ad0f111165281720627d084612c7f799','base.bzl':'88c1e7d804609f03ff7785eda002bd6ab5b5a8181b93057187f73463033d5137','defs.bzl':'47c6210b1877b8563c9d3677575d660d6049e285dc01e484b6d812ab3cf7cc9e'},
            ('extension','repository-action'): {'MODULE.bazel':'322f87e964072ec707e373fc7eaff451a1eec2078d65e6a574027d4173c80de4','BUILD.bazel':'c3c92518748ad173d0adb19e9396fb011e866d7d263aa42e0cb7f080f9cbb95a','base.bzl':'ab11403c68cebcc8da9b993acd98aa4d70e45e19e452444728cb6026ccdb5bee','defs.bzl':'9bfe72a63f931e7d8ea43ea5227d085f5169c8c9dd75d5d9bb4c825c9a53317b'},
        }
        for pair, expected in pins.items():
            self.assertEqual({k:s.digest(v) for k,v in s.fixture(*pair)['files'].items()},expected)

    def test_explicit_old_binary_blocked_reference_only_never_native(self):
        args=s.argument_parser().parse_args(['--native-sha256',s.OLD_NATIVE])
        with self.assertRaisesRegex(ValueError,'old H1'):s.executable_evidence(args)
        args.reference_only=True
        with mock.patch.object(s.common,'executable_evidence',return_value={'reference':{}}) as helper:
            self.assertEqual(s.executable_evidence(args),{'reference':{}})
            helper.assert_called_once_with(args)

    def test_pre_fix_debug_sha_cannot_be_new_frozen_candidate(self):
        args=s.argument_parser().parse_args(['--native-sha256',s.PRE_FIX_DEBUG_SHA])
        with self.assertRaisesRegex(ValueError,'pre-fix debug'):s.executable_evidence(args)

    def test_unexported_constructor_is_created_during_loading_not_analysis(self):
        for probe in ('unexported-membership','unexported-index','exported-keys'):
            v=s.fixture('constructor',probe);raw=v['files']['defs.bzl']
            self.assertIn(b'def make():\n    return provider()\nBOX=[make()]\nP=provider()\n',raw)
            self.assertNotIn(b'provider()',raw.split(b'def _impl(ctx):')[1])
            self.assertEqual(v['stage'],'callback')
        v=s.fixture('constructor','exported-keys')
        self.assertEqual(v['expected'],b'(False, True, 0)\n')
        self.assertIsNone(v['diagnostic'])

    def test_membership_keys_are_paired_actual_callback_probes(self):
        exprs={'membership-set':'set','membership-len':'len','membership-string':'"x"',
            'membership-none':'None','membership-int':'7'}
        for probe,key in exprs.items():
            v=s.fixture('constructor',probe)
            self.assertIn(('x='+key+' in ctx.attr.dep\n').encode(),v['files']['defs.bzl'])
            self.assertEqual(v['markers'],['SET_CALLBACK'])
            self.assertEqual(v['actions'],0)
            self.assertIsNone(v['expected'])
            self.assertFalse(v['native_only'])

    def test_historical_false_and_old_reference_prep_cannot_be_overwritten(self):
        gap=s.KNOWN_PARITY_GAPS[0]
        self.assertEqual(gap['classification'],'HISTORICAL_DIFFERENTIAL_MISMATCH')
        self.assertEqual(gap['original_classification'],'KNOWN_DIFFERENTIAL_MISMATCH')
        self.assertEqual(gap['native']['returncode'],0)
        self.assertIs(gap['native']['result'],False)
        self.assertEqual(gap['reference']['returncode'],1)
        for path in s.PRESERVED_REPORTS:
            with self.subTest(path=path),self.assertRaisesRegex(ValueError,'overwrite'):
                s.main(['--reference-only','--output',str(path)])

    def test_source_binding_includes_real_production_vendor_provenance(self):
        names={str(p.relative_to(s.ROOT)) for p in s.SOURCES}
        for name in ('src/package.rs','src/bzlmod.rs','src/providers.rs','src/starlark_types.rs','Cargo.lock','vendor/starlark-0.14.2.provenance.json'):
            self.assertIn(name,names)
        self.assertTrue(any(n.startswith('vendor/') and n.endswith('.rs') for n in names))


class DiagnosticTests(unittest.TestCase):
    def test_anchored_messages_only_and_no_old_missing_global(self):
        for diagnostic,text in MESSAGES.items():
            s.require_rejection(record('ERROR: '+text),diagnostic,'native')
            for bad in ('    ERROR: '+text,'source example: '+text,"ERROR: Variable `set` not found"):
                with self.subTest(diagnostic=diagnostic,bad=bad),self.assertRaises(ValueError):
                    s.require_rejection(record(bad),diagnostic,'native')

    def test_nonzero_is_not_enough(self):
        for code in (0,True,-9,37,101,124,127,137):
            with self.assertRaises(ValueError):s.require_rejection(record('ERROR: '+MESSAGES['frozen'],code),'frozen','native')
        for flag in ('timed_out','spawn_error','interrupted','output_truncated'):
            r=record('ERROR: '+MESSAGES['frozen']);r[flag]=True
            with self.assertRaises(ValueError):s.require_rejection(r,'frozen','native')

    def test_reference_defect_exact_crash_not_success_or_ordinary_error(self):
        s.require_defect(record(CRASH,37))
        for r in [record(CRASH,1),record(CRASH,0),record('ERROR: immutable',37),record(CRASH.replace('StarlarkSet.symmetricDifferenceUpdate','other'),37),record(CRASH+'SET_ACTION_READY\n',37)]:
            with self.assertRaises(ValueError):s.require_defect(r)

    def test_reference_crash_batch_isolated_no_live_server_damage(self):
        args=s.argument_parser().parse_args([]);root=Path('/tmp/opencode/synthetic')
        ref=s.DirectReference(args.reference,root/'workspace',root,None,s.SUITE)
        cache=root/'cases/algebra/self-xor-method/reference/cache'
        argv=s.command_argv(args,ref,'reference',cache,root,s.steps_for('algebra','self-xor-method','reference')[0])
        self.assertIn('--batch',argv)
        self.assertNotIn('--output_base='+str(ref.output_base),argv)

    def test_membership_export_gate_has_engine_specific_anchored_errors(self):
        for diagnostic,message in KEY_MESSAGES.items():
            native='Target '+('indexing' if diagnostic=='unexported-index' else 'membership')+' requires an exported provider constructor'
            for engine,text in [('reference',message),('native',native)]:
                s.require_rejection(record('Error: '+text),diagnostic,engine)
                for bad in ['    Error: '+text,'example '+text,'Error: missing provider',
                        'Error: Target indexing requires an exported provider constructor' if diagnostic!='unexported-index' else 'Error: Target membership requires an exported provider constructor',
                        'Error: '+text+'\nSET_ACTION_READY\n']:
                    with self.subTest(diagnostic=diagnostic,engine=engine,text=bad),self.assertRaises(ValueError):
                        s.require_rejection(record(bad),diagnostic,engine)
                for code in (0,7,37,101):
                    with self.assertRaises(ValueError):s.require_rejection(record('Error: '+text,code),diagnostic,engine)

    def test_membership_rejects_wrong_reference_type_or_export_category(self):
        for diagnostic in KEY_MESSAGES:
            for other in KEY_MESSAGES:
                if other!=diagnostic:
                    with self.subTest(diagnostic=diagnostic,other=other),self.assertRaises(ValueError):
                        s.require_rejection(record('Error: '+KEY_MESSAGES[other]),diagnostic,'reference')


class ReplayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.full=synthetic(False);cls.reference=synthetic(True)

    def passes(self,r):
        with mock.patch.object(s,'PLATFORM_SHA',s.digest(b'SYNTHETIC DISTFILE NOT AN ARCHIVE')):
            return s.report_passes(r)

    def test_complete_synthetic_reports_replay_but_are_not_engine_evidence(self):
        self.assertTrue(self.passes(self.full));self.assertTrue(self.passes(self.reference))

    def test_membership_closure_requires_paired_execution_not_reference_preparation(self):
        full=self.full['membership_gap_closure'];reference=self.reference['membership_gap_closure']
        self.assertEqual(full['status'],'RESOLVED_FOR_FROZEN_CANDIDATE')
        self.assertIs(full['native_closure_verified'],True)
        self.assertEqual(full['candidate_sha256'],'a'*64)
        self.assertEqual(reference['status'],'REFERENCE_PREPARED_NATIVE_NOT_RUN')
        self.assertIs(reference['native_closure_verified'],False)
        self.assertIsNone(reference['candidate_sha256'])
        self.assertEqual(len(full['observations']),2*len(s.KEY_PROBES))
        self.assertEqual(len(reference['observations']),len(s.KEY_PROBES))
        self.assertTrue(all(o['engine']=='reference' for o in reference['observations']))
        for o in full['observations']:
            self.assertTrue(o['verified'])
            self.assertTrue(o['runs'])
            self.assertTrue(all(q['returncode']==(0 if o['probe']=='exported-keys' else 1) for q in o['runs']))

    def test_old_native_false_never_counts_as_membership_gap_closure(self):
        for probe in s.KEY_PROBES[:-1]:
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['probe']==probe and p['engine']=='native')
            run=r['runs'][p['runs'][0]]
            run.update(returncode=0,stderr='SET_CALLBACK\nMEMBERSHIP_RESULT False\nSET_ACTION_READY\n')
            streams(run)
            r['membership_gap_closure']=s.membership_closure(r,False)
            self.assertEqual(r['membership_gap_closure']['status'],'INCOMPLETE')
            self.assertIs(r['membership_gap_closure']['native_closure_verified'],False)
            self.assertFalse(self.passes(r))

    def test_membership_false_error_missing_callback_or_wrong_stage_cannot_close_gap(self):
        for kind in ('missing-global','missing-provider','missing-callback','reference-type','native-index-error','unexecuted'):
            r=copy.deepcopy(self.full)
            engine='reference' if kind=='reference-type' else 'native'
            p=next(p for p in r['phases'] if p['probe']=='membership-set' and p['engine']==engine)
            run=r['runs'][p['runs'][0]]
            if kind=='missing-global':run['stderr']='Error: Variable `set` not found\n'
            elif kind=='missing-provider':run['stderr']='SET_CALLBACK\nError: Target does not provide P\n'
            elif kind=='missing-callback':run['stderr']=run['stderr'].replace('SET_CALLBACK\n','')
            elif kind=='reference-type':run['stderr']='SET_CALLBACK\nError: '+KEY_MESSAGES['membership-string']+'\n'
            elif kind=='native-index-error':run['stderr']='SET_CALLBACK\nError: Target indexing requires an exported provider constructor\n'
            else:p['runs']=[]
            streams(run)
            r['membership_gap_closure']=s.membership_closure(r,False)
            self.assertEqual(r['membership_gap_closure']['status'],'INCOMPLETE')
            self.assertFalse(self.passes(r))

    def test_membership_valid_exported_key_outputs_are_mandatory(self):
        for wrong in (b'(True, True, 0)\n',b'(False, False, 0)\n',b'(False, True, 1)\n'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['probe']=='exported-keys' and p['engine']=='native')
            for stage in ('cold','warm'):
                p['outputs'][stage][0]=entry(p['outputs'][stage][0]['path'],wrong,0o555)
            r['membership_gap_closure']=s.membership_closure(r,False)
            self.assertEqual(r['membership_gap_closure']['status'],'INCOMPLETE')
            self.assertFalse(self.passes(r))

    def test_closure_metadata_cannot_be_forged_or_historical_false_retconned(self):
        for kind in ('native-claim','status','candidate','observations','edge-pin','historical-rc','historical-result','historical-receipt'):
            r=copy.deepcopy(self.reference if kind in ('native-claim','status') else self.full)
            closure=r['membership_gap_closure']
            if kind=='native-claim':closure['native_closure_verified']=True
            elif kind=='status':closure['status']='RESOLVED_FOR_FROZEN_CANDIDATE'
            elif kind=='candidate':closure['candidate_sha256']='b'*64
            elif kind=='observations':closure['observations'].pop()
            elif kind=='edge-pin':closure['edge_contract_origin']['sha256']='0'*64
            elif kind=='historical-rc':r['known_parity_gaps'][0]['native']['returncode']=1
            elif kind=='historical-result':r['known_parity_gaps'][0]['native']['result']=True
            else:r['known_parity_gaps'][0]['native']['sha256']='0'*64
            self.assertFalse(self.passes(r))

    def test_closure_candidate_and_command_binding_must_be_new_and_consistent(self):
        for kind in ('old-release','old-debug','different-executable','changed-source'):
            r=copy.deepcopy(self.full)
            if kind in ('old-release','old-debug'):
                sha=s.OLD_NATIVE if kind=='old-release' else s.PRE_FIX_DEBUG_SHA
                r['configuration']['native_sha256']=sha
                for field in ('executables','executables_after'):r[field]['native']['sha256']=sha
            elif kind=='different-executable':
                p=next(p for p in r['phases'] if p['probe']=='membership-set' and p['engine']=='native')
                r['runs'][p['runs'][0]]['executable_sha256']='b'*64
            else:r['sources_after'][str(s.ROOT/'src/starlark_types.rs')]['sha256']='0'*64
            r['membership_gap_closure']=s.membership_closure(r,False)
            self.assertEqual(r['membership_gap_closure']['status'],'INCOMPLETE')
            self.assertFalse(self.passes(r))

    def test_reject_top_level_mutations(self):
        for change in (
            lambda r:r.update(status='PASS'), lambda r:r.update(known_reference_defects=[]),
            lambda r:r.update(known_parity_gaps=[]), lambda r:r.update(unplanned_native_PASS=True),
            lambda r:r.update(completed_phases=0), lambda r:r['phases'].pop(),
            lambda r:r['plan'].update(commands=1), lambda r:r['limitations'].pop(),
            lambda r:r['sources_after'].pop(''+str(s.ROOT/'src/providers.rs')),
            lambda r:r['executables_after']['reference'].update(sha256='0'*64),
            lambda r:r['cleanup_receipt'].update(removed=False),
            lambda r:r['cleanup_receipt']['pids'].update({'987654321':True}),
            lambda r:r['cleanup_receipt'].update(remaining_pidfiles=['server.pid.txt']),
            lambda r:r['registry_after'].clear(), lambda r:r['official_source_after'].update(commit='wrong'),
            lambda r:r['runs'][1].update(preflight_binding_sha256='0'*64),
            lambda r:r['runs'][1].update(executable_sha256='0'*64),
        ):
            r=copy.deepcopy(self.full);change(r)
            with self.subTest(change=change):self.assertFalse(self.passes(r))

    def test_output_mutations_fail_even_when_rehashed(self):
        for mutation in ('missing','bytes','mode','path','warm','source'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['family']=='provider' and p['engine']=='native')
            if mutation=='missing':p['outputs'].pop('changed')
            elif mutation=='bytes':p['outputs']['changed'][0]=entry(p['outputs']['changed'][0]['path'],b'B\nA\n',0o555)
            elif mutation=='mode':p['outputs']['cold'][0]['mode']=0o755
            elif mutation=='path':p['outputs']['cold'][0]['path']='/ghost/output'
            elif mutation=='warm':p['outputs']['warm'][0]['sha256']='0'*64
            else:p['input_snapshots']['2'][str(Path(r['scratch_directory'])/'workspace/b.txt')]=entry(Path(r['scratch_directory'])/'workspace/b.txt',b'B\n')
            with self.subTest(mutation=mutation):self.assertFalse(self.passes(r))

    def test_native_wrong_self_alias_contents_never_pass(self):
        for probe in s.DEFECTS:
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['probe']==probe and p['engine']=='native')
            for stage in ('cold','warm'):
                p['outputs'][stage][0]=entry(p['outputs'][stage][0]['path'],b'([], [1, 2])\n',0o555)
            self.assertFalse(self.passes(r))

    def test_defect_misclassification_missing_signature_wrong_rc(self):
        for kind in ('parity','ordinary','rc','omitted'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['probe']==s.DEFECTS[0] and p['engine']=='reference')
            run=r['runs'][p['runs'][0]]
            if kind=='parity':p['status']='PASS'
            elif kind=='ordinary':run['stderr']='ERROR: immutable\n';streams(run)
            elif kind=='rc':run['returncode']=1
            else:r['known_reference_defects'].pop()
            self.assertFalse(self.passes(r))

    def test_stream_truncation_rehashed_wrong_marker_and_cache_claim(self):
        for kind in ('truncated','rehash','marker','cache','command'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['family']=='globals' and p['engine']=='native')
            run=r['runs'][p['runs'][0]]
            if kind=='truncated':run['output_truncated']=True
            elif kind=='rehash':run['stderr_base64']=base64.b64encode(b'wrong').decode()
            elif kind=='marker':run['stderr']=run['stderr'].replace('SET_CALLBACK','OTHER');streams(run)
            elif kind=='cache':run['stderr']=run['stderr'].replace('1 actions executed, 0 cache hits','0 actions executed, 1 cache hits');streams(run)
            else:run['argv'][-1]='//:other'
            self.assertFalse(self.passes(r))

    def test_reference_only_cannot_claim_native_pass_or_unexecuted_commands(self):
        r=copy.deepcopy(self.reference);r['status']='PASS_ONLY_BOUNDED_PROFILE'
        self.assertFalse(self.passes(r))
        r=copy.deepcopy(self.reference);r['phases'][0]['engine']='native'
        self.assertFalse(self.passes(r))
        r=copy.deepcopy(self.full);r['phases'][0]['runs']=[]
        self.assertFalse(self.passes(r))
        r=copy.deepcopy(self.reference);r['native_claim']['pass_claim']=True
        self.assertFalse(self.passes(r))

    def test_rehashed_execution_log_must_prove_input_dependency_hashes(self):
        for kind in ('input-missing','input-hash','output-hash','producer-missing','cache-hit','wrong-mnemonic'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['family']=='provider' and p['engine']=='reference')
            e=p['logs']['changed'];spawns=s.json_objects(base64.b64decode(e['bytes_base64']))
            if kind=='input-missing':spawns[0]['inputs']=[]
            elif kind=='input-hash':spawns[0]['inputs'][0]['digest']['hash']='0'*64
            elif kind=='output-hash':spawns[0]['actualOutputs'][0]['digest']['hash']='0'*64
            elif kind=='producer-missing':spawns.pop(0)
            elif kind=='cache-hit':spawns[0]['cacheHit']=True
            else:spawns[0]['mnemonic']='Other'
            p['logs']['changed']=entry(e['path'],'\n'.join(json.dumps(x) for x in spawns).encode())
            with self.subTest(kind=kind):self.assertFalse(self.passes(r))

    def test_action_graph_must_connect_provider_generated_file(self):
        for kind in ('producer','input','artifact','cycle'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['family']=='provider' and p['engine']=='reference')
            run=next(r['runs'][rid] for rid in p['runs'] if 'aquery' in r['runs'][rid]['argv'])
            graph=json.loads(run['stdout'])
            if kind=='producer':graph['actions'].pop(0)
            elif kind=='input':graph['depSetOfFiles'][1]['directArtifactIds']=[3]
            elif kind=='artifact':graph['pathFragments'][0]['label']='not-produced'
            else:graph['pathFragments'][0]['parentId']=1
            run['stdout']=json.dumps(graph);streams(run)
            with self.subTest(kind=kind):self.assertFalse(self.passes(r))

    def test_reference_warm_incremental_not_fabricated_two_hits(self):
        r=copy.deepcopy(self.full)
        p=next(p for p in r['phases'] if p['family']=='provider' and p['engine']=='reference')
        warm=r['runs'][p['runs'][2]]
        warm['stderr']='INFO: 1 process: 1 internal.\n';streams(warm)
        self.assertTrue(self.passes(r))
        restart=next(r['runs'][rid] for rid in p['runs'] if '2 action cache hit' in r['runs'][rid]['stderr'])
        restart['stderr']='INFO: 1 process: 1 internal.\n';streams(restart)
        self.assertFalse(self.passes(r))

    def test_native_action_store_cannot_be_missing_or_inspected_elsewhere(self):
        for kind in ('empty','path','missing-output','stale'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['family']=='provider' and p['engine']=='native')
            if kind=='empty':p['actions_after']['actions']['exists']=False
            elif kind=='path':p['actions_after']['actions']['path']='/ghost/actions'
            elif kind=='missing-output':p['actions_after']['actions']['entries']=[]
            else:p['actions_before']['actions']['exists']=True
            self.assertFalse(self.passes(r))

    def test_error_phases_reject_outputs_and_callback_execution(self):
        for kind in ('output','action','marker','log'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['probe']=='hash-key' and p['engine']=='reference')
            run=r['runs'][p['runs'][0]]
            if kind=='output':p['outputs']['cold']=[]
            elif kind=='action':p['actions_after']['work']['entries']=['work.out']
            elif kind=='marker':run['stderr']+='SET_CALLBACK\n';streams(run)
            else:p['logs']['reject']=entry(p['logs']['reject']['path'],b'{"mnemonic":"BAD"}')
            self.assertFalse(self.passes(r))

    def test_repository_bytes_modes_and_canonical_identity_are_not_discarded(self):
        for kind in ('bytes','mode','build','canonical','alias','missing'):
            r=copy.deepcopy(self.full)
            p=next(p for p in r['phases'] if p['family']=='extension' and p['engine']=='native')
            if kind=='bytes':p['outputs']['repository'][0]=entry(p['outputs']['repository'][0]['path'],b'wrong')
            elif kind=='mode':p['outputs']['repository'][0]['mode']=0o555
            elif kind=='build':p['extra_outputs']['BUILD.bazel']['mode']=0o555
            elif kind=='canonical':r['runs'][p['runs'][-1]]['stdout']='@generated//:data\n';streams(r['runs'][p['runs'][-1]])
            elif kind=='alias':r['runs'][p['runs'][-2]]['stdout']=r['runs'][p['runs'][-2]]['stdout'].replace('@@+ext+generated','@generated');streams(r['runs'][p['runs'][-2]])
            else:p['extra_outputs'].clear()
            self.assertFalse(self.passes(r))

    def test_same_before_after_source_tamper_still_breaks_command_binding(self):
        r=copy.deepcopy(self.full)
        for field in ('sources_before','sources_after'):
            r[field][str(s.ROOT/'src/providers.rs')]['sha256']='0'*64
        self.assertFalse(self.passes(r))

    def test_cleanup_and_phase_receipts_require_complete_plan_not_just_counts(self):
        for kind in ('order','duplicate','extra','defectmissing','pidmissing'):
            r=copy.deepcopy(self.full)
            if kind=='order':r['phases'][0],r['phases'][1]=r['phases'][1],r['phases'][0]
            elif kind=='duplicate':r['phases'][1]=copy.deepcopy(r['phases'][0])
            elif kind=='extra':r['phases'].append(copy.deepcopy(r['phases'][0]))
            elif kind=='defectmissing':r['phases']=[p for p in r['phases'] if p['probe'] not in s.DEFECTS]
            else:r['owned_pids']=[];r['cleanup_receipt']['pids']={}
            self.assertFalse(self.passes(r))


if __name__=='__main__':
    unittest.main()
