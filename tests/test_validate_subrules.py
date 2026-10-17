"""Portable harness-only mutations: no ignored research report/binary dependency.

Synthetic receipts are NOT engine or compatibility evidence. Current reference
executions and frozen native acceptance are separate gates.
"""
import ast
import base64
import copy
import json
from pathlib import Path
import sys
import unittest
from unittest import mock

sys.path.insert(0,str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_subrules as s


def record(text='',code=0,stdout=''):
    return dict(stdout=stdout,stderr=text,returncode=code,timed_out=False)


def streams(r):
    for field in ('stdout','stderr'):
        raw=r[field].encode()
        r.update({field+'_base64':base64.b64encode(raw).decode(),field+'_sha256':s.digest(raw),
            field+'_full_sha256':s.digest(raw),field+'_full_size_bytes':len(raw)})
    return r


def entry(path,raw,mode=0o644):
    return dict(path=str(path),bytes_base64=base64.b64encode(raw).decode(),size_bytes=len(raw),sha256=s.digest(raw),kind='file',mode=mode)


MESSAGES = {
    'unhashable': "unhashable type: 'Subrule'", 'positional': 'subrule() got unexpected positional argument',
    'factory-unhashable': "unhashable type: 'builtin_function_or_method'",
    'doc': "subrule() got unexpected keyword argument 'doc'", 'unknown': "subrule() got unexpected keyword argument 'unknown'",
    'missing': "missing required parameter 'implementation'",
    **{p:'subrule implementation must be a Starlark function' for p in ('len-implementation','native-implementation','callable-implementation')},
    'bad-subrules': 'subrules must contain only real Subrule values', 'bad-rule-subrules': 'subrules must contain only real Subrule values',
    'loading-call': 'ctx.actions and subrules are only available during rule analysis',
    'unexported': "invalid subrule hasn't been exported by a bzl file",
    'build-global': "name 'subrule' is not defined", 'module-global': "name 'subrule' is not defined",
    'native-field': "Object has no attribute 'subrule'",
    'transitive-direct': "rule 'r' must declare 'Leaf' in 'subrules'",
    'nested-undeclared': "subrule Outer must declare Leaf in 'subrules'",
    'read-toolchains': 'subrules using toolchains must enable automatic exec-groups. For more info, see https://bazel.build/extending/auto-exec-groups#migration-aegs',
    'undeclared-fragment': "S has to declare 'cpp' as a required fragment in order to access it. Please update the 'fragments' argument of the subrule definition (for example: fragments = [\"cpp\"])",
    'private-cpp-api': "file '//:defs.bzl' cannot use private API",
    'override': "got invalid named argument: '_dep' is an implicit dependency and cannot be overridden",
    'public-attr': "illegal attribute name 'dep': subrules may only define private attributes (whose names begin with '_').",
    'nonlabel-attr': "bad type for attribute '_dep': subrule attributes may only be label or lists of labels.",
    'no-default': "for attribute '_dep': no default value specified",
    'computed-default': "illegal default value for attribute '_dep': subrules cannot define computed defaults.",
    'bad-cfg': "bad cfg for attribute '_dep': subrules may only have target/exec attributes.",
    'two-toolchains': 'subrules may require at most 1 toolchain, got: [ToolchainTypeRequirement[toolchainType=//:a, mandatory=true, ignoreIfInvalid=false], ToolchainTypeRequirement[toolchainType=//:b, mandatory=true, ignoreIfInvalid=false]]',
    'mandatory-lifted': "//:consume: missing value for mandatory attribute '$//:defs.bzl%S%_dep' in 'r' rule",
    'lifted-label-list': "subrule S attribute '_deps' (attr.label_list) is not supported by the native profile: Bazel 9.3 crashes passing a lifted label_list to the implementation (InvalidStarlarkValueException: Lists$TransformingRandomAccessList)",
    'explicit-flag': 'unsupported option --experimental_rule_extension_api=false',
    'write-list': 'write content must be a string or Args', 'write-tuple': 'write content must be a string or Args',
}


def crash(probe):
    # Compact synthetic crash used only to exercise replay's exact class/phase.
    return ('FATAL: bazel crashed due to an internal error.\n'
        'Caused by: net.starlark.java.eval.Starlark$UncheckedEvalException: NullPointerException thrown during Starlark evaluation (//:'+probe+')\n'
        'at <starlark>.args(<builtin>:0)\nat <starlark>._rule(/synthetic/defs.bzl:7)\n'
        'Caused by: java.lang.NullPointerException: Cannot invoke "com.google.devtools.build.lib.analysis.starlark.StarlarkRuleContext.getStarlarkSemantics()" because "this.starlarkRuleContext" is null\n'
        'at com.google.devtools.build.lib.analysis.starlark.StarlarkSubrule$SubruleContext.getStarlarkSemantics(StarlarkSubrule.java:461)\n'
        'at com.google.devtools.build.lib.analysis.starlark.StarlarkActionFactory.args(StarlarkActionFactory.java:1259)\n'
        'at com.google.devtools.build.lib.analysis.starlark.StarlarkRuleConfiguredTargetUtil.evalRule(StarlarkRuleConfiguredTargetUtil.java:86)\n')


def graph():
    return dict(targets=[dict(id=1,label='//consumer:consume')],
        actions=[dict(mnemonic='I0SubruleProducer',inputDepSetIds=[1],outputIds=[1],targetId=1),
            dict(mnemonic='I0SubruleConsumer',inputDepSetIds=[2],outputIds=[3],targetId=1)],
        pathFragments=[dict(id=1,label='bazel-out/bin/consumer/consume.generated'),dict(id=2,label='consumer/seed.txt'),dict(id=3,label='bazel-out/bin/consumer/consume.out')],
        artifacts=[dict(id=i,pathFragmentId=i) for i in (1,2,3)],
        depSetOfFiles=[dict(id=1,directArtifactIds=[2]),dict(id=2,directArtifactIds=[1])])


def synthetic(reference_only=False):
    args=s.argument_parser().parse_args(['--reference-only'] if reference_only else ['--native-sha256','a'*64])
    root=Path('/tmp/opencode/SYNTHETIC-NOT-EXECUTED-subrule-test');ws=root/'workspace'
    ref=s.DirectReference(args.reference,ws,root,None,s.SUITE)
    sources={str(p):dict(path=str(p),sha256='b'*64,size=1,mode=0o644) for p in s.source_paths()}
    registry={}
    for p in s.REGISTRY.rglob('*'):
        if p.is_file():
            raw=p.read_bytes();sources[str(p)].update(sha256=s.digest(raw),size=len(raw))
            path=root/'registry'/p.relative_to(s.REGISTRY);registry[str(path)]=entry(path,raw)
    executables={'reference':dict(path=str(args.reference),sha256=s.REFERENCE_SHA256)}
    if not reference_only:executables['native']=dict(path=str(args.native),sha256=args.native_sha256)
    official=dict(path=str(args.reference_source),commit=s.SOURCE_PIN,files=s.OFFICIAL_FILES)
    report=dict(schema_version=1,status='REFERENCE_ONLY_PASS' if reference_only else 'PASS_ONLY_BOUNDED_PROFILE',
        configuration={k:str(v) if isinstance(v,Path) else v for k,v in vars(args).items()},
        plan=s.plan_receipt(reference_only),planned_checks=s.CASEPLAN,limitations=s.LIMITATIONS,
        known_reference_defects=s.KNOWN_REFERENCE_DEFECTS,origins=s.ORIGINS,
        native_claim=dict(status='NOT_RUN' if reference_only else 'EXECUTED_BOUNDED_PROFILE',
            commands=sum(len(s.steps_for(c['name'],p,e)) for c in s.CASEPLAN for p,e in s.phases_for(c,reference_only) if e=='native'),pass_claim=not reference_only),
        started_at='2026-10-13T00:00:00+00:00',finished_at='2026-10-13T00:01:00+00:00',errors=[],cleanup=[],
        sources_before=sources,sources_after=copy.deepcopy(sources),executables=executables,executables_after=copy.deepcopy(executables),
        official_source_before=official,official_source_after=copy.deepcopy(official),scratch_directory=str(root),
        owned_pids=[987654321],owned_batch_pids=[987654322,987654323],registry_before=registry,registry_after=copy.deepcopy(registry),runs=[],phases=[],
        cleanup_receipt=dict(path=str(root),removed=True,pids={str(p):False for p in (987654321,987654322,987654323)},remaining_pidfiles=[]),
        platforms_distfile=entry('/synthetic/platforms',b'SYNTHETIC DISTFILE NOT AN ARCHIVE'))
    binding=s.digest(json.dumps(dict(executables=executables,source=official,production=sources),sort_keys=True).encode())
    def add(name,argv,r,timeout=args.timeout):
        r.update(id=len(report['runs']),name=name,argv=argv,cwd=str(ws),timeout_seconds=timeout,
            executable_sha256=next(v['sha256'] for v in executables.values() if v['path']==argv[0]),preflight_binding_sha256=binding)
        report['runs'].append(streams(r));return r['id']
    add('reference.version',ref.startup+['version','--gnu_format'],record(stdout='bazel 9.3.0\n'))
    for c in s.CASEPLAN:
        for probe,engine in s.phases_for(c,reference_only):
            family=c['name'];v=s.fixture(family,probe);cache=root/'cases'/family/probe/engine/'cache'
            empty={n:dict(path=str(cache/n),exists=False,entries=[]) for n in ('actions','work')}
            p=dict(family=family,probe=probe,engine=engine,cache=str(cache),runs=[],outputs={},logs={},input_snapshots={},generated={},
                actions_before=empty,actions_after=copy.deepcopy(empty),forbidden_outputs=[],status=s.phase_status(v,engine))
            files=v['files'].copy()
            for i,step in enumerate(s.steps_for(family,probe,engine)):
                stage,cmd,target,_=step
                if stage=='changed' and cmd=='build':files[v['changed'][0]]=v['changed'][1]
                p['input_snapshots'][str(i)]={str(ws/n):entry(ws/n,b) for n,b in files.items()}
                r=record()
                if stage=='defect':
                    r=record(crash(probe),37)
                    r['batch_process']=dict(pid=report['owned_batch_pids'][s.DEFECTS.index(probe)],reaped=True,alive_after=False)
                elif stage=='reject':
                    d=v['diagnostic']
                    if d in s.GUARDS:
                        field,kind=s.GUARDS[d]
                        message=('analysis scope receiver is locked or no longer active' if engine=='native' else
                            "cannot access field or method '"+field+"' of "+kind+' context')
                    else:message=MESSAGES[d]
                    r=record('Error: '+message+'\n',48 if d=='module-global' and engine=='reference' else 1)
                if cmd=='build':
                    if stage!='defect':r['stderr']+=''.join(m+'\n' for m in v['markers'])
                    if engine=='reference':
                        if v['diagnostic']:p['logs'][stage]=dict(path=str(cache.parent/(stage+'.execution.json')),absent=True)
                        else:
                            spawns=[]
                            if stage!='warm':
                                raw=next(iter((v['changed'][2] if stage=='changed' else v['expected']).values()))
                                for name in v['spawns']:
                                    ins={'consumer/seed.txt':raw} if name=='I0SubruleProducer' else {'bazel-out/bin/consumer/consume.generated':raw} if name=='I0SubruleConsumer' else {'external/+ext+made/data':raw} if name=='I0ModuleConsumer' else {}
                                    out='consumer/consume.generated' if name=='I0SubruleProducer' else 'consumer/consume.out' if name=='I0SubruleConsumer' else next(iter(v['expected']))
                                    spawns.append(dict(mnemonic=name,exitCode=0,cacheHit=False,inputs=[dict(path=k,digest=dict(hash=s.digest(b))) for k,b in ins.items()],actualOutputs=[dict(path='bazel-out/bin/'+out,digest=dict(hash=s.digest(raw)))]))
                            p['logs'][stage]=entry(cache.parent/(stage+'.execution.json'),'\n'.join(json.dumps(q) for q in spawns).encode())
                    elif not v['diagnostic']:
                        r['stderr']+=('0 actions executed, '+str(v['actions'])+' cache hits\n' if stage=='warm' else str(v['actions'])+' actions executed, 0 cache hits\n')
                if stage in ('cold','warm','changed') and cmd==('build' if engine=='native' else 'cquery'):
                    wanted=v['changed'][2] if stage=='changed' else v['expected'];p['outputs'][stage]=[]
                    for name,raw in wanted.items():
                        path=(cache/'actions'/('d'*64 if stage!='changed' else 'e'*64)/'outputs'/name if engine=='native' else ref.output_base/'execroot/_main/bazel-out/k8-fastbuild/bin'/('consumer' if family=='functional' and probe=='factory-two-action' else '')/name)
                        r['stdout']+=(target+'\t'+str(path) if engine=='native' else str(path.relative_to(ref.output_base/'execroot/_main')))+'\n'
                        p['outputs'][stage].append(entry(path,raw,0o555))
                        if engine=='native':
                            p['actions_after']['actions']['exists']=True
                            p['actions_after']['actions']['entries'].append(str(path.relative_to(cache/'actions')))
                    if family=='functional' and probe=='factory-two-action':
                        raws=[b'seed-A!\n',b'seed-B!\n'] if stage=='changed' and engine=='native' else [b'seed-B!\n' if stage=='changed' else b'seed-A!\n']
                        p['generated'][stage]=[entry(cache/'actions'/str(j)/'consume.generated' if engine=='native' else path.parent/'consume.generated',b,0o555) for j,b in enumerate(raws)]
                if stage=='graph':r['stdout']=json.dumps(graph())
                p['runs'].append(add(family+'.'+probe+'.'+engine+'.'+str(i),s.command_argv(args,ref,engine,cache,root,step),r))
            p['inputs_after']={str(ws/n):entry(ws/n,b) for n,b in files.items()};report['phases'].append(p)
    add(s.SUITE+'.shutdown',ref.startup+['shutdown']+list(s.COMMON_FLAGS),record(),min(args.timeout,15))
    report['completed_phases']=len(report['phases'])
    return report


COMMANDS_REFERENCE,COMMANDS_FULL=115,196


class PlanTests(unittest.TestCase):
    def test_complete_bounded_plan_and_syntax(self):
        self.assertEqual((len(s.CASEPLAN),s.plan_receipt(True)['phases'],s.plan_receipt(False)['phases']),(9,59,121))
        self.assertEqual((s.plan_receipt(True)['commands'],s.plan_receipt(False)['commands']),(COMMANDS_REFERENCE,COMMANDS_FULL))
        for c in s.CASEPLAN:
            self.assertEqual(len(c['probes']),len(set(c['probes'])))
            for p in c['probes']:
                for n,b in s.fixture(c['name'],p)['files'].items():
                    if n.endswith(('.bzl','.bazel')):ast.parse(b)

    def test_unknown_plan_entries_fail_closed(self):
        for pair in [('bogus','metadata'),('constructor','bogus')]:
            with self.assertRaises(ValueError):s.fixture(*pair)

    def test_original_args_callbacks_pinned_without_ignored_reports(self):
        pins={'parent_object':'2fdcb484f0ff687338a4aecb383df4330faf546700ade92da3205ac8d638209c',
            'parent_bound':'2d8b70ae87acbfc6768c3e6515dfa260b4e7841468bf4e6a4bfb3fb6415e4fb2',
            'child_object':'c287683ecef3287dd1a8f7a39ec4995ce62ae64b7a59958f11f48f14cdf2fa0d',
            'child_bound':'8af8285b811ba3fcdd9ca08bff9e72c9ba91719f996e437be5f9e41f9382cbf2'}
        for p,h in pins.items():self.assertEqual(s.digest(s.fixture('args',p)['files']['defs.bzl']),h)

    def test_named_seed_fixture_not_silently_rewritten_case01(self):
        v=s.fixture('functional','zeroattrs-no-config-views')
        self.assertEqual(v['expected'],{'zeroattrs.seed':b'seed-data\n'})
        self.assertEqual(s.digest(v['files']['defs.bzl']),'2c16a46c4824f411e73b31e2e03303ef95a80ebe62a71951607e6cceda2d2236')
        self.assertNotIn(b'fragments)',v['files']['defs.bzl'])
        self.assertIn('SCOPE_CONTRACT',v['origin'])

    def test_write_args_callbacks_and_raw_outputs_match_immutable_contract(self):
        pins={'mixed':'d28d6ce9525a8ce8bcaf0e1af7b617205067bc155111ad38e952b4a4c6480d9f',
            'empty':'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
            'formatted':'3572e0c284736a08246a820c50f661eed7d964f9c2faecd5a12037e235dc3194',
            'mapped':'2b28846e4fa0a6d66d32cbeca78cc67c436b2e868f7d83be3e0b3040fb5885b5'}
        for name,sha in pins.items():
            v=s.fixture('args','write-'+name)
            self.assertEqual(s.digest(v['files']['defs.bzl']),'e1ecee9e2e68e882b705798391d9e0a2c42d214b378eabe7b9aa825a130090a2')
            self.assertEqual(s.digest(v['expected'][name+'.out']),sha)
            self.assertEqual(v['actions'],2 if name=='mixed' else 1)
            self.assertNotIn(b'run_shell',v['files']['defs.bzl'])
        self.assertEqual(s.WRITE_OUTPUTS['empty'],b'')
        self.assertIn(b"\n''\n",s.WRITE_OUTPUTS['mixed'])
        self.assertIn(b"'line1\nline2'\n",s.WRITE_OUTPUTS['mixed'])
        self.assertIn("'雪é🙂'\n".encode(),s.WRITE_OUTPUTS['mixed'])

    def test_returned_builder_still_consumed_by_parent_write_not_shell(self):
        v=s.fixture('args','returned-builder');defs=v['files']['defs.bzl']
        self.assertIn(b'ctx.actions.write(out,result.args)',defs)
        self.assertIn(b'result.args.add("from-parent")',defs)
        self.assertNotIn(b'run_shell',defs)
        self.assertEqual(v['expected']['args_escape.out'],b'from-subrule\nfrom-parent\n')

    def test_mapper_timing_not_claimed_and_paramfile_setters_are_not_boundaries(self):
        probes=[p for c in s.CASEPLAN for p in c['probes']]
        self.assertNotIn('mapper_failure',probes)
        self.assertIn('Bazel defers it to write execution',str(s.LIMITATIONS))
        for probe in ('param-file-format','use-param-file'):
            self.assertNotIn(probe,probes)
        self.assertIn('supported since N23d',str(s.LIMITATIONS))

    def test_constructor_hash_is_separate_from_instance_metadata(self):
        v=s.fixture('constructor','factory-unhashable')
        self.assertIn(b'X={subrule:1}',v['files']['defs.bzl'])
        self.assertEqual(v['stage'],'loading')
        self.assertEqual(v['diagnostic'],'factory-unhashable')

    def test_factory_owns_actual_closure_provider_files_and_consumer_target(self):
        v=s.fixture('functional','factory-two-action')
        for token in ('def implementation(ctx, source, suffix="!")','return subrule(','Alias=S','subrules=(S,)','out.owner != ctx.label','I0SubruleProducer'):
            self.assertIn(token,s.FACTORY)
        for token in ('p=Outer(ctx.file.src,suffix="!")','inputs=[p.file]','DefaultInfo(files=depset([out]))','p.file.owner != ctx.label','subrules=[Outer]'):
            self.assertIn(token,s.CONSUMER)
        self.assertEqual(v['changed'],('consumer/seed.txt',b'seed-B!\n',{'consume.out':b'seed-B!\n'}))
        self.assertEqual(v['target'],'//consumer:consume')

    def test_configuration_family_is_differential_with_bazel_texts_and_outputs(self):
        item=next(c for c in s.CASEPLAN if c['name']=='configuration')
        self.assertEqual({e for _,e in s.phases_for(item)},{'reference','native'})
        self.assertEqual(len(s.phases_for(item,True)),len(item['probes']))
        boundaries=next(c for c in s.CASEPLAN if c['name']=='boundaries')['probes']
        for gone in ('attrs','toolchains','fragments','read-toolchains','read-fragments'):
            self.assertNotIn(gone,boundaries)
        for probe in item['probes']:
            v=s.fixture('configuration',probe)
            self.assertFalse(v['native_only'])
            if probe in s.CONFIGURATION_OUTPUTS:
                self.assertIsNone(v['diagnostic'])
                self.assertEqual(v['expected'],{'consume.out':s.CONFIGURATION_OUTPUTS[probe]})
            else:
                self.assertEqual(v['diagnostic'],probe)
                self.assertTrue(s.re.search(s.DIAGNOSTICS[probe],'Error: '+MESSAGES[probe]))
        lifted=s.fixture('configuration','lifted-attrs')['files']['defs.bzl']
        for token in (b'configuration_field(fragment="cpp",name="fdo_optimize")',b'allow_single_file=True',b'subrules=[Leaf]',b'subrules=[Outer]',b'cfg=config.none()'):
            self.assertIn(token,lifted)
        # Bazel 9.3 crashes on lifted label_list: never in the differential family.
        self.assertNotIn(b'label_list',lifted)
        self.assertIn(b'cfg=T)',s.fixture('configuration','bad-cfg')['files']['defs.bzl'])
        boundary=s.fixture('boundaries','lifted-label-list')
        self.assertTrue(boundary['native_only']);self.assertEqual(boundary['diagnostic'],'lifted-label-list')
        self.assertIn(b'_dep=None',s.fixture('configuration','override')['files']['defs.bzl'])
        self.assertIn('NEW',str(s.LIMITATIONS))

    def test_native_boundaries_never_become_differential(self):
        item=next(c for c in s.CASEPLAN if c['name']=='boundaries')
        self.assertEqual(s.phases_for(item,True),[])
        self.assertTrue(all(e=='native' for _,e in s.phases_for(item)))

    def test_ordinary_setting_visibility_is_explicit_native_profile_not_parity(self):
        v=s.fixture('boundaries','ordinary-setting-visibility')
        self.assertTrue(v['native_only']);self.assertIsNone(v['diagnostic'])
        self.assertEqual(v['expected'],{'consume.out':b'(False, False)\n'})
        self.assertIn('Bazel ordinary ctx has build_setting_value',str(s.LIMITATIONS))
        self.assertNotIn(b'build_setting_value',s.fixture('context','surface')['files']['defs.bzl'])

    def test_unexported_declarations_fail_loading_before_callback(self):
        v=s.fixture('constructor','unexported')
        self.assertEqual(v['stage'],'loading');self.assertEqual(v['markers'],[])
        self.assertIn(b'BOX=[subrule(implementation=_sub)]',v['files']['defs.bzl'])
        self.assertIn(b'subrules=BOX',v['files']['defs.bzl'])

    def test_defect_native_is_controlled_divergence_not_success(self):
        for p in s.DEFECTS:
            v=s.fixture('args',p)
            self.assertEqual(s.phase_status(v,'native'),'INTENTIONAL_DIVERGENCE')
            self.assertEqual(s.steps_for('args',p,'native')[0][0],'reject')
            self.assertEqual(s.steps_for('args',p,'reference')[0][:2],('defect','build'))
            self.assertEqual(v['expected'],{})

    def test_source_binding_covers_all_production_vendor_and_provenance(self):
        names={str(p.relative_to(s.ROOT)) for p in s.source_paths()}
        for n in ('src/package.rs','src/providers.rs','Cargo.lock','vendor/starlark-0.14.2.provenance.json','tests/test_validate_subrules.py'):
            self.assertIn(n,names)
        self.assertTrue(any(n.startswith('vendor/') and n.endswith('.rs') for n in names))

    def test_old_binary_blocked_but_reference_only_does_not_touch_native(self):
        args=s.argument_parser().parse_args(['--native-sha256',s.OLD_NATIVE])
        with self.assertRaisesRegex(ValueError,'old pre-subrule'):s.executable_evidence(args)
        args.reference_only=True
        with mock.patch.object(s.common,'executable_evidence',return_value={'reference':{}}) as f:
            self.assertEqual(s.executable_evidence(args),{'reference':{}});f.assert_called_once_with(args)

    def test_original_contract_outputs_protected(self):
        for path in s.ORIGINS:
            with self.assertRaisesRegex(ValueError,'overwrite'):s.main(['--reference-only','--output',str(s.ROOT/path)])


class DiagnosticTests(unittest.TestCase):
    def test_errors_require_anchored_actual_diagnostics(self):
        for d,text in MESSAGES.items():
            s.require_rejection(record('Error: '+text,1),d,'native')
            for bad in ('    Error: '+text,'source example: '+text,'Error: network unavailable'):
                with self.subTest(d=d,bad=bad),self.assertRaises(ValueError):s.require_rejection(record(bad,1),d,'native')

    def test_guard_fields_and_kinds_must_match_reference(self):
        for d,(field,kind) in s.GUARDS.items():
            text="cannot access field or method '"+field+"' of "+kind+' context'
            s.require_rejection(record('Error: '+text,1),d,'reference')
            for bad in (text.replace(field,'wrong'),text.replace(kind+' context','unrelated context')):
                with self.assertRaises(ValueError):s.require_rejection(record('Error: '+bad,1),d,'reference')

    def test_wrong_rc_crash_missing_global_are_not_guard_errors(self):
        for code in (0,True,-9,37,101,124,127,137):
            with self.assertRaises(ValueError):s.require_rejection(record('Error: analysis scope receiver is locked or no longer active',code),'child-args','native')
        for flag in ('timed_out','spawn_error','interrupted','output_truncated'):
            r=record('Error: '+MESSAGES['unhashable'],1);r[flag]=True
            with self.assertRaises(ValueError):s.require_rejection(r,'unhashable','native')
        with self.assertRaises(ValueError):s.require_rejection(record('Error: Variable `subrule` not found',1),'child-label','native')

    def test_exact_args_npe_without_invented_buffered_marker(self):
        for p in s.DEFECTS:s.require_defect(record(crash(p),37),p)
        for token in ('getStarlarkSemantics','StarlarkActionFactory.args','defs.bzl:7','evalRule','//:child_object'):
            with self.subTest(token=token),self.assertRaises(ValueError):s.require_defect(record(crash('child_object').replace(token,'WRONG'),37),'child_object')
        for code in (0,1,7,True,-9,101):
            with self.assertRaises(ValueError):s.require_defect(record(crash('child_object'),code),'child_object')

    def test_module_undefined_global_allows_only_exact_reference_syntax_trailer(self):
        text="ERROR: MODULE.bazel:2:3: name 'subrule' is not defined\nERROR: Error computing the main repository mapping: syntax error in MODULE.bazel file for <root>\n"
        s.require_rejection(record(text,48),'module-global','reference')
        for wrong in (text+'parse error\n',text.replace("name 'subrule' is not defined","unknown problem")):
            with self.assertRaises(ValueError):s.require_rejection(record(wrong,48),'module-global','reference')

    def test_actual_native_named_parameter_diagnostics(self):
        for d,text in [('positional','Missing named-only parameter `implementation` for call to `subrule`'),
                ('doc','Found `doc` extra named parameter(s) for call to subrule'),
                ('unknown','Found `unknown` extra named parameter(s) for call to subrule')]:
            s.require_rejection(record('error: '+text,1),d,'native')
            with self.assertRaises(ValueError):s.require_rejection(record('    '+text,1),d,'native')


class ReplayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):cls.full=synthetic();cls.reference=synthetic(True)

    def passes(self,r):
        with mock.patch.object(s,'PLATFORM_SHA',s.digest(b'SYNTHETIC DISTFILE NOT AN ARCHIVE')):return s.report_passes(r)

    def phase(self,r,family,probe,engine='native'):
        return next(p for p in r['phases'] if (p['family'],p['probe'],p['engine'])==(family,probe,engine))

    def test_complete_synthetic_receipts_replay_not_engine_evidence(self):
        self.assertTrue(self.passes(self.full));self.assertTrue(self.passes(self.reference))

    def test_top_level_fail_closed(self):
        changes=[lambda r:r.update(status='PASS'),lambda r:r.update(known_reference_defects=[]),lambda r:r.update(extra=True),
            lambda r:r.update(completed_phases=0),lambda r:r['phases'].pop(),lambda r:r['plan'].update(commands=1),
            lambda r:r['limitations'].pop(),lambda r:r['origins'].clear(),lambda r:r['sources_after'].pop(str(s.ROOT/'src/providers.rs')),
            lambda r:r['executables_after']['reference'].update(sha256='0'*64),lambda r:r['registry_after'].clear(),
            lambda r:r['official_source_after'].update(commit='wrong'),lambda r:r['runs'][1].update(preflight_binding_sha256='0'*64),
            lambda r:r['runs'][1].update(executable_sha256='0'*64),lambda r:r['runs'][1].update(environment={}),
            lambda r:r['cleanup_receipt'].update(removed=False),lambda r:r['cleanup_receipt']['pids'].update({'987654321':True}),
            lambda r:r['cleanup_receipt'].update(remaining_pidfiles=['server.pid.txt']),lambda r:r['owned_batch_pids'].pop()]
        for change in changes:
            r=copy.deepcopy(self.full);change(r)
            with self.subTest(change=change):self.assertFalse(self.passes(r))

    def test_boolean_success_code_and_truncated_stream_are_not_execution(self):
        for key,value in [('returncode',False),('id',True),('timed_out',True),('output_truncated',True)]:
            r=copy.deepcopy(self.full);r['runs'][1][key]=value;self.assertFalse(self.passes(r))

    def test_defect_rehash_cannot_hide_wrong_crash_or_wrong_phase(self):
        for kind in ('parity','missing','rc','wrong-crash','wrong-phase','after-factory','pid','batch','native-acceptance'):
            r=copy.deepcopy(self.full);engine='native' if kind=='native-acceptance' else 'reference'
            p=self.phase(r,'args','child_object',engine);run=r['runs'][p['runs'][0]]
            if kind=='parity':p['status']='PASS'
            elif kind=='missing':r['known_reference_defects'].pop()
            elif kind=='rc':run['returncode']=1
            elif kind=='wrong-crash':run['stderr']=run['stderr'].replace('NullPointerException','ConcurrentModificationException')
            elif kind=='wrong-phase':run['stderr']=run['stderr'].replace('defs.bzl:7','defs.bzl:3')
            elif kind=='after-factory':run['stderr']+='AFTER_FACTORY child_object\n'
            elif kind=='pid':run['batch_process']['reaped']=False
            elif kind=='batch':run['argv'].remove('--batch')
            else:run.update(returncode=0,stderr='BEFORE_FACTORY child_object\nAFTER_FACTORY child_object\n')
            streams(run)
            with self.subTest(kind=kind):self.assertFalse(self.passes(r))

    def test_parent_args_exception_requires_successful_real_output(self):
        for probe in ('parent_object','parent_bound'):
            for kind in ('guard','no-output','wrong-content'):
                r=copy.deepcopy(self.full);p=self.phase(r,'args',probe);run=r['runs'][p['runs'][0]]
                if kind=='guard':run.update(returncode=1,stderr='Error: analysis scope receiver is locked or no longer active\n');streams(run)
                elif kind=='no-output':p['outputs'].clear()
                else:p['outputs']['cold'][0]=entry(p['outputs']['cold'][0]['path'],b'wrong\n',0o555)
                self.assertFalse(self.passes(r))

    def test_output_bytes_modes_paths_and_defaultinfo_are_required(self):
        for kind in ('missing','bytes','mode','path','warm','target','source','generated'):
            r=copy.deepcopy(self.full);p=self.phase(r,'functional','factory-two-action')
            if kind=='missing':p['outputs'].pop('changed')
            elif kind=='bytes':p['outputs']['changed'][0]=entry(p['outputs']['changed'][0]['path'],b'seed-A!\n',0o555)
            elif kind=='mode':p['outputs']['cold'][0]['mode']=0o755
            elif kind=='path':p['outputs']['cold'][0]['path']='/ghost/output'
            elif kind=='warm':p['outputs']['warm'][0]['sha256']='0'*64
            elif kind=='target':
                run=r['runs'][p['runs'][0]];run['stdout']=run['stdout'].replace('//consumer:consume','//owner:consume');streams(run)
            elif kind=='source':
                ws=Path(r['scratch_directory'])/'workspace';p['input_snapshots']['2'][str(ws/'consumer/seed.txt')]=entry(ws/'consumer/seed.txt',b'seed-A!\n')
            else:p['generated'].clear()
            with self.subTest(kind=kind):self.assertFalse(self.passes(r))

    def test_rehashed_spawns_prove_two_action_file_dependency(self):
        for kind in ('input-missing','input-hash','output-hash','producer-missing','cache-hit','mnemonic'):
            r=copy.deepcopy(self.full);p=self.phase(r,'functional','factory-two-action','reference');e=p['logs']['changed']
            spawns=s.shared.json_objects(base64.b64decode(e['bytes_base64']))
            if kind=='input-missing':spawns[0]['inputs']=[]
            elif kind=='input-hash':spawns[0]['inputs'][0]['digest']['hash']='0'*64
            elif kind=='output-hash':spawns[0]['actualOutputs'][0]['digest']['hash']='0'*64
            elif kind=='producer-missing':spawns.pop(0)
            elif kind=='cache-hit':spawns[0]['cacheHit']=True
            else:spawns[0]['mnemonic']='Other'
            p['logs']['changed']=entry(e['path'],'\n'.join(json.dumps(x) for x in spawns).encode())
            with self.subTest(kind=kind):self.assertFalse(self.passes(r))

    def test_action_graph_owner_and_dependency_cannot_be_faked(self):
        for kind in ('owner','producer','input','cycle'):
            r=copy.deepcopy(self.full);p=self.phase(r,'functional','factory-two-action','reference');run=r['runs'][p['runs'][-1]];g=json.loads(run['stdout'])
            if kind=='owner':g['targets'][0]['label']='//owner:consume'
            elif kind=='producer':g['actions'].pop(0)
            elif kind=='input':g['depSetOfFiles'][1]['directArtifactIds']=[2]
            else:g['pathFragments'][0]['parentId']=1
            run['stdout']=json.dumps(g);streams(run)
            self.assertFalse(self.passes(r))

    def test_registration_then_guard_failure_publishes_nothing(self):
        for kind in ('file','cache','work','execution-log','bypass','loading-error','missing-marker'):
            r=copy.deepcopy(self.full);p=self.phase(r,'context','locked_direct','reference');run=r['runs'][p['runs'][0]]
            if kind=='file':p['forbidden_outputs']=['locked_direct.seed']
            elif kind in ('cache','work'):p['actions_after']['actions' if kind=='cache' else 'work']['entries']=['forbidden']
            elif kind=='execution-log':p['logs']['reject']=entry(p['logs']['reject']['path'],b'{"mnemonic":"Action"}')
            elif kind=='bypass':run['stderr']+='GUARD_BYPASSED\n'
            elif kind=='loading-error':run['stderr']="Error: name 'subrule' is not defined\n"
            else:run['stderr']=run['stderr'].replace('SUBRULE_ENTER @@//:locked_direct\n','')
            streams(run)
            with self.subTest(kind=kind):self.assertFalse(self.passes(r))

    def test_structural_results_and_args_builder_survival_are_actual_bytes(self):
        for family,probe in [('context','structural'),('context','surface'),('args','returned-builder'),('constructor','metadata')]:
            r=copy.deepcopy(self.full);p=self.phase(r,family,probe)
            p['outputs']['cold'][0]=entry(p['outputs']['cold'][0]['path'],b'normalization is not parity\n',0o555)
            self.assertFalse(self.passes(r))

    def test_write_args_raw_quote_utf8_empty_and_modes_not_normalized(self):
        changes=[('write-mixed',b'plain\nspace value\n'),('write-mixed',s.WRITE_OUTPUTS['mixed'].replace(b"\n''\n",b'\n\n')),
            ('write-mixed',s.WRITE_OUTPUTS['mixed'].replace(b"'line1\nline2'",b"'line1\\nline2'")),
            ('write-mixed',s.WRITE_OUTPUTS['mixed'].replace('雪é🙂'.encode(),b'?')),
            ('write-empty',b'\n'),('write-formatted',s.WRITE_OUTPUTS['formatted'].replace(b"'<a>'",b'<a>')),
            ('write-mapped',b'x y\nq\'z\nz\n')]
        for probe,raw in changes:
            r=copy.deepcopy(self.full);p=self.phase(r,'args',probe)
            for stage in ('cold','warm'):
                for i,e in enumerate(p['outputs'][stage]):p['outputs'][stage][i]=entry(e['path'],raw,0o555)
            with self.subTest(probe=probe,raw=raw):self.assertFalse(self.passes(r))
        r=copy.deepcopy(self.full);p=self.phase(r,'args','write-mixed')
        p['outputs']['cold'][0]['mode']=0o644;self.assertFalse(self.passes(r))

    def test_previous_native_global_metadata_mismatch_cannot_be_normalized(self):
        r=copy.deepcopy(self.full);p=self.phase(r,'constructor','metadata')
        wrong=b'("function", "subrule", "subrule", [], True, "Subrule", "<subrule S>", "<subrule S>", True, False, [])\n'
        for stage in ('cold','warm'):p['outputs'][stage][0]=entry(p['outputs'][stage][0]['path'],wrong,0o555)
        self.assertFalse(self.passes(r))

    def test_loading_rejections_never_accept_callback_execution(self):
        r=copy.deepcopy(self.full);p=self.phase(r,'constructor','positional');run=r['runs'][p['runs'][0]]
        run['stderr']+='I0_RULE\n';streams(run);self.assertFalse(self.passes(r))

    def test_phase_order_duplicate_missing_and_reference_only_claims(self):
        for kind in ('order','duplicate','omitted','native-claim','unexecuted'):
            r=copy.deepcopy(self.reference if kind=='native-claim' else self.full)
            if kind=='order':r['phases'][0],r['phases'][1]=r['phases'][1],r['phases'][0]
            elif kind=='duplicate':r['phases'][1]=copy.deepcopy(r['phases'][0])
            elif kind=='omitted':r['phases']=[p for p in r['phases'] if not (p['family']=='args' and p['probe'] in s.DEFECTS)]
            elif kind=='native-claim':r['native_claim']['pass_claim']=True
            else:r['phases'][0]['runs']=[]
            self.assertFalse(self.passes(r))

    def test_source_and_binary_mutation_even_both_endpoints(self):
        for kind in ('source','binary','stream','cache'):
            r=copy.deepcopy(self.full)
            if kind=='source':
                for field in ('sources_before','sources_after'):r[field][str(s.ROOT/'src/providers.rs')]['sha256']='0'*64
            elif kind=='binary':
                r['configuration']['native_sha256']=s.OLD_NATIVE
                for field in ('executables','executables_after'):r[field]['native']['sha256']=s.OLD_NATIVE
            elif kind=='stream':r['runs'][1]['stderr_base64']=base64.b64encode(b'wrong').decode()
            else:self.phase(r,'functional','factory-two-action')['actions_after']['actions']['entries']=[]
            self.assertFalse(self.passes(r))


if __name__=='__main__':unittest.main()
