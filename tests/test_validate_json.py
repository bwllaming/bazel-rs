"""Portable harness-only mutations. Synthetic receipts are NOT engine evidence.

Only tracked source/constants are read. No ignored report, frozen ELF, JVM or
research prototype is a unit-test dependency.
"""
import ast
import base64
import copy
import json
from pathlib import Path
import sys
import unittest
from unittest import mock

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'scripts'))
import validate_json as s


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
    'module-global':"name 'json' is not defined",'native-field':"Object has no attribute 'json'",
    'unhashable':"unhashable type: 'json'",'immutable':'cannot set .encode field of json value',
    **{p:"encode() got named argument for positional-only parameter 'x'" for p in ('encode-named','decode-named','indent-named','encode-indent-named')},
    **{p:'accepts no more than 1 positional argument but got 2' for p in ('indent-positional','encode-indent-positional','arity-default')},
    'type-default':"parameter 'x' got value of type 'int', want 'string'",
    'option-type':"parameter 'indent' got value of type 'NoneType', want 'string'",
    'frozen':'trying to mutate a frozen list value',
    **{'offset'+str(n):'at offset '+str(n)+', unexpected character "]"' for n in (6,8,10)},
    'int-key':'dict has int key, want string','bool-key':'dict has bool key, want string',
    **{p:'cannot encode non-finite float +inf' for p in ('nan','inf','overflow')},
    **{p:'cannot encode '+t+' as JSON' for p,t in [('label','Label'),('module','json'),('callable','builtin_function_or_method')]},
    'nested':'in dict key "a": at list index 0: cannot encode Label as JSON',
    **{p:'input is not valid JSON' for p in ('numeric-eof','encode-numeric-eof')},
    **{p:'nesting depth limit exceeded' for p in ('depth65','cycle')},
    **{p:'JSON resource budget exceeded' for p in ('range-quota','default-quota')},
    'native-info':'NativeInfo unsupported in JSON','external-file':'external File unsupported in JSON',
}


def synthetic(reference_only=False):
    args=s.argument_parser().parse_args(['--reference-only'] if reference_only else ['--native-sha256','a'*64])
    root=Path('/tmp/opencode/SYNTHETIC-NOT-EXECUTED-json-test');ws=root/'workspace'
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
        owned_pids=[987654321],owned_batch_pids=[],registry_before=registry,registry_after=copy.deepcopy(registry),runs=[],phases=[],
        cleanup_receipt=dict(path=str(root),removed=True,pids={'987654321':False},remaining_pidfiles=[]),
        platforms_distfile=entry('/synthetic/platforms',b'SYNTHETIC NOT AN ARCHIVE'))
    binding=s.digest(json.dumps(dict(executables=executables,source=official,production=sources),sort_keys=True).encode())
    def add(name,argv,r,timeout=args.timeout):
        r.update(id=len(report['runs']),name=name,argv=argv,cwd=str(ws),timeout_seconds=timeout,
            executable_sha256=next(v['sha256'] for v in executables.values() if v['path']==argv[0]),preflight_binding_sha256=binding)
        report['runs'].append(streams(r));return r['id']
    add('reference.version',ref.startup+['version','--gnu_format'],record(stdout='bazel 9.3.0\n'))
    for c in s.CASEPLAN:
        for probe,which in s.phases_for(c,reference_only):
            family=c['name'];v=s.fixture(family,probe);cache=root/'cases'/family/probe/which/'cache'
            empty={n:dict(path=str(cache/n),exists=False,entries=[]) for n in ('actions','work')}
            p=dict(family=family,probe=probe,engine=which,cache=str(cache),runs=[],outputs={},logs={},input_snapshots={},generated={},
                actions_before=empty,actions_after=copy.deepcopy(empty),forbidden_outputs=[],status='PASS')
            files=v['files'].copy()
            for i,step in enumerate(s.steps_for(family,probe,which)):
                stage,cmd,target,_=step
                if stage=='changed' and cmd=='build':files[v['changed'][0]]=v['changed'][1]
                p['input_snapshots'][str(i)]={str(ws/n):entry(ws/n,b) for n,b in files.items()}
                r=record()
                if stage=='reject':
                    message=s.NATIVE_MESSAGES.get(v['diagnostic'],MESSAGES[v['diagnostic']]) if which=='native' else MESSAGES[v['diagnostic']]
                    r=record('Error: '+message+'\n',48 if v['diagnostic']=='module-global' and which=='reference' else 1)
                if cmd=='build':
                    if stage!='repository':r['stderr']+=''.join(m+'\n' for m in v['markers'])
                    if family=='module-bzl' and i==0:r['stderr']+='JSON_MODULE_BZL\nJSON_REPOSITORY_CALLBACK\n'
                    if which=='reference':
                        if v['diagnostic']:p['logs'][stage]=dict(path=str(cache.parent/(stage+'.execution.json')),absent=True)
                        else:
                            spawns=[]
                            if family=='module-bzl' and stage=='cold':
                                spawns=[dict(mnemonic='JJsonConsumer',exitCode=0,cacheHit=False,
                                    inputs=[dict(path='external/+ext+made/data',digest=dict(hash=s.digest(s.REPO_DATA)))],
                                    actualOutputs=[dict(path='bazel-out/k8-fastbuild/bin/consume.out',digest=dict(hash=s.digest(s.REPO_DATA)))])]
                            p['logs'][stage]=entry(cache.parent/(stage+'.execution.json'),'\n'.join(json.dumps(x) for x in spawns).encode())
                    elif not v['diagnostic']:
                        counts=(0,0) if stage=='repository' else (0,v['actions']) if stage=='warm' else (1,18) if family=='functional' and stage=='changed' else (v['actions'],0)
                        r['stderr']+=str(counts[0])+' actions executed, '+str(counts[1])+' cache hits\n'
                if stage in ('cold','warm','changed') and cmd==('build' if which=='native' else 'cquery'):
                    p['outputs'][stage]=[]
                    for name,raw in s.expected_outputs(v,stage,which).items():
                        h='e'*64 if stage=='changed' and name=='dependency.json' else 'd'*64
                        path=cache/'actions'/h/'outputs'/name if which=='native' else ref.output_base/'execroot/_main/bazel-out/k8-fastbuild/bin'/name
                        r['stdout']+=(target+'\t'+str(path) if which=='native' else str(path.relative_to(ref.output_base/'execroot/_main')))+'\n'
                        p['outputs'][stage].append(entry(path,raw,0o555))
                        if which=='native':
                            p['actions_after']['actions']['exists']=True
                            p['actions_after']['actions']['entries'].append(str(path.relative_to(cache/'actions')))
                if stage=='repository':
                    path=cache/'repositories/direct/trees'/('f'*64)/'data' if which=='native' else ref.output_base/'execroot/_main/external/+ext+made/data'
                    r['stdout']=target+'\t'+str(path)+'\n' if which=='native' else str(path.relative_to(ref.output_base/'execroot/_main'))+'\n'
                    p['generated']={'repository':[entry(path,s.REPO_DATA)],'build':[entry(path.parent/'BUILD.bazel',s.REPO_BUILD)]}
                p['runs'].append(add(family+'.'+probe+'.'+which+'.'+str(i),s.command_argv(args,ref,which,cache,root,step),r))
            p['inputs_after']={str(ws/n):entry(ws/n,b) for n,b in files.items()};report['phases'].append(p)
    add(s.SUITE+'.shutdown',ref.startup+['shutdown']+list(s.COMMON_FLAGS),record(),min(args.timeout,15))
    report['completed_phases']=len(report['phases'])
    return report


class PlanTests(unittest.TestCase):
    def test_eight_families_complete_and_syntax(self):
        self.assertEqual((len(s.CASEPLAN),s.plan_receipt(True)['phases'],s.plan_receipt(True)['commands']),(8,37,69))
        self.assertEqual((s.plan_receipt(False)['probes'],s.plan_receipt(False)['phases'],s.plan_receipt(False)['commands']),(45,82,127))
        for c in s.CASEPLAN:
            self.assertEqual(len(c['probes']),len(set(c['probes'])))
            for p in c['probes']:
                for name,raw in s.fixture(c['name'],p)['files'].items():
                    if name.endswith(('.bazel','.bzl')):ast.parse(raw)

    def test_unknown_probe_fails_closed(self):
        for pair in [('invented','metadata'),('api','invented')]:
            with self.assertRaises(ValueError):s.fixture(*pair)

    def test_isolated_shared_engine_does_not_mutate_subrule_suite(self):
        self.assertEqual(s.infrastructure.SUITE,'subrules')
        self.assertNotEqual(s.engine.CASEPLAN,s.infrastructure.CASEPLAN)
        self.assertNotIn('native_capacity',s.infrastructure.plan_receipt(True))

    def test_method_specific_capacity_is_bound_in_plan(self):
        legacy=dict(weighted_bytes=64*1024*1024,nodes=100000,max_open_containers=64,
            budget_scope='per_invocation_shared_cumulative')
        expected={m:legacy.copy() for m in ('encode','indent','encode_indent')}
        expected['decode']=dict(legacy,weighted_bytes=256*1024*1024,nodes=250000,effective_utf8_input_bytes=8*1024*1024)
        self.assertEqual(s.NATIVE_CAPACITY,expected)
        for reference_only in (False,True):
            plan=s.plan_receipt(reference_only)
            self.assertEqual(plan['native_capacity'],expected)
            self.assertEqual(plan['fixture_plan_sha256'],s._base_plan_receipt(reference_only)['sha256'])
            sha=plan.pop('sha256')
            self.assertEqual(sha,s.digest(json.dumps(plan,sort_keys=True).encode()))
            plan['native_capacity']['decode']['nodes']=1
            self.assertEqual(s.NATIVE_CAPACITY,expected)

    def test_only_decode_quota_uses_new_node_boundary_and_never_default_success(self):
        v=s.fixture('profile','default-quota')
        self.assertIn(b'json.decode("["+"null,"*249999+"null]",default=[])',v['files']['defs.bzl'])
        self.assertEqual((v['diagnostic'],v['expected'],v['actions'],v['markers'],v['native_only']),
            ('default-quota',{},0,['JSON_CALLBACK'],True))
        self.assertEqual(s.steps_for('profile','default-quota','native'),[('reject','build','//:consume',[])])
        self.assertIn(b'json.encode(range(100001))',s.fixture('profile','range-quota')['files']['defs.bzl'])

    def test_current_float_api_exact_golden_and_bit_binding(self):
        rows=s.float_rows();self.assertEqual(len(rows),1024)
        v=s.fixture('floats','golden1024');self.assertEqual(len(v['expected']['consume.out'].splitlines()),1024)
        self.assertIn(b'a=float(decimal)',v['files']['defs.bzl'])
        self.assertIn(b'b=json.decode(lexeme)',v['files']['defs.bzl'])
        self.assertIn(b'type(a)',v['files']['defs.bzl']);self.assertIn(b'type(b)',v['files']['defs.bzl'])
        self.assertIn(b'1000000000000000.2',v['expected']['consume.out'])

    def test_historical_floats_are_not_relabelled_prototype_reference_passes(self):
        text=' '.join(s.LIMITATIONS)
        for token in ('BOUND_HISTORICAL_GOLDEN','CURRENT_EXECUTION','2/1024','23440','NATIVE_ONLY'):
            self.assertIn(token,text)
        for name in s.ORIGINS:
            with self.assertRaisesRegex(ValueError,'overwrite'):s.main(['--reference-only','--output',str(s.ROOT/name)])

    def test_portable_golden_content_pins_not_just_origin_metadata(self):
        with mock.patch.object(Path,'read_bytes',return_value=b'{}'):
            with self.assertRaisesRegex(ValueError,'extraction pin'):s.portable('float')

    def test_wrong_float_bits_and_lost_lexical_type_fail(self):
        for key,value in [('bits_hex','0000000000000001'),('expected_json','0')]:
            corpus=copy.deepcopy(s.portable('float'));corpus['rows'][0][key]=value
            with mock.patch.object(s,'portable',return_value=corpus),self.assertRaises(ValueError):s.float_rows()

    def test_provider_is_real_json_attr_and_nineteen_action_outputs(self):
        v=s.fixture('functional','provider19');raw=v['files']['defs.bzl']
        self.assertIn(b'return [P(payload=json.decode(ctx.attr.data))]',raw)
        self.assertIn(b'json.encode(ctx.attr.dep[P].payload)',raw)
        self.assertEqual(v['actions'],19);self.assertEqual(len(v['expected']),19)
        self.assertEqual([n for n in v['expected'] if v['expected'][n]!=v['changed'][2][n]],['dependency.json'])
        self.assertEqual(v['changed'][0],'BUILD.bazel')
        self.assertNotIn(b'print(',v['files']['BUILD.bazel'])
        self.assertIn(b'BUILD_META=(type(json),dir(json),json.encode(json.decode("[true,null]")))',v['files']['BUILD.bazel'])
        self.assertIn('print("BUILD_META"',s.portable('functional')['files']['BUILD.bazel'])

    def test_build_json_value_is_actually_evaluated_and_consumed_without_print(self):
        v=s.fixture('api','build')
        self.assertIn(b'VALUE=json.encode([type(json),json.encode([True,None])])',v['files']['BUILD.bazel'])
        self.assertIn(b'build_value=VALUE',v['files']['BUILD.bazel'])
        self.assertIn(b'if ctx.attr.build_value !=',v['files']['defs.bzl'])
        self.assertNotIn(b'print(',v['files']['BUILD.bazel'])

    def test_first_failed_frozen_receipt_is_preserved_not_relabelled(self):
        self.assertEqual(s.FIRST_ATTEMPT_SHA256,'9a76d8166e5ab39129ee3ffa63a44da1238e2c9fc1510c827d021a1f5a0bce6b')
        with self.assertRaisesRegex(ValueError,'preserved first failed'):
            s.main(['--reference-only','--output',str(s.FIRST_ATTEMPT)])
        self.assertIn('NO rich diagnostic-text/context parity claim',' '.join(s.LIMITATIONS))

    def test_metadata_exact_identity_and_types_not_namespace(self):
        for token in ('saved==saved','saved==json.encode','json.encode==json.encode','METHOD==ALIAS'):
            self.assertIn(token,s.METADATA)
        value=json.loads(s.META_EXPECTED)
        self.assertEqual(value[0],'json');self.assertEqual(value[5],'builtin_function_or_method')
        self.assertEqual(value[-4:],[True,False,False,True])

    def test_default_identity_mutability_null_bigint_and_float_types(self):
        v=s.fixture('decode','identity-mutable-types')
        for token in ('got.append("identity")','JSON_DEFAULT_IDENTITY','json.decode("null",42)','json.encode_indent(d)','fresh.append(3)'):
            self.assertIn(token,s.IDENTITY)
        self.assertIn(b'"int","int","float","float","float","int"',v['expected']['consume.out'])
        self.assertIn(b'123456789123456789123456789',v['expected']['consume.out'])

    def test_repository_calls_real_module_and_actual_file_producer(self):
        v=s.fixture('module-bzl','repository-callback')
        for token in ('use_extension','use_repo'):self.assertIn(token.encode(),v['files']['MODULE.bazel'])
        for token in ('repository_rule(','json.decode(','data["a"].append(1)','ctx.file('):self.assertIn(token,s.REPOSITORY)
        self.assertIn(b'inputs=[ctx.file.src]',v['files']['defs.bzl'])
        self.assertEqual(v['expected']['consume.out'],b'{"a":[1],"callback":true,"z":0}')

    def test_file_triples_are_explicit_not_config_normalization(self):
        v=s.fixture('profile','main-files')
        self.assertNotEqual(s.expected_outputs(v,'cold','native'),s.expected_outputs(v,'cold','reference'))
        for which,config in [('native','rust-fastbuild'),('reference','k8-fastbuild')]:
            value=json.loads(s.expected_outputs(v,'cold',which)['file_encoding.json'])
            self.assertEqual(value[0],dict(path='input.txt',root='',short_path='input.txt'))
            self.assertEqual(value[1]['path'],value[1]['root']+'/'+value[1]['short_path'])
            self.assertEqual(value[1]['root'],'bazel-out/'+config+'/bin')

    def test_native_boundaries_never_reference_errors(self):
        item=next(c for c in s.CASEPLAN if c['name']=='profile')
        self.assertEqual(s.phases_for(item,True),[('main-files','reference')])
        self.assertTrue(all(e=='native' for p,e in s.phases_for(item) if p in s.NATIVE_ONLY))
        self.assertIn('EXPLICIT_OUT_OF_PROFILE',' '.join(s.LIMITATIONS))

    def test_old_binary_blocked_and_reference_only_not_touch_native(self):
        args=s.argument_parser().parse_args(['--native-sha256',s.OLD_NATIVE])
        with self.assertRaisesRegex(ValueError,'pre-JSON'):s.executable_evidence(args)
        args.reference_only=True
        with mock.patch.object(s.common,'executable_evidence',return_value={'reference':{}}) as f:
            self.assertEqual(s.executable_evidence(args),{'reference':{}});f.assert_called_once_with(args)

    def test_complete_source_bindings(self):
        names={str(p.relative_to(s.ROOT)) for p in s.source_paths()}
        for n in ('src/package.rs','src/providers.rs','src/starlark_types.rs','src/json.rs','src/repository_budget.rs','tests/json.rs','Cargo.lock','scripts/validate_json.py','tests/test_validate_json.py','tests/fixtures/json/float_reference.json'):
            self.assertIn(n,names)
        self.assertTrue(any(n.startswith('vendor/') and n.endswith('.rs') for n in names))


class DiagnosticTests(unittest.TestCase):
    def test_anchored_real_errors_only(self):
        for d,message in MESSAGES.items():
            message=s.NATIVE_MESSAGES.get(d,message)
            s.require_rejection(record('Error: '+message,1),d,'native')
            for bad in ('    Error: '+message,'example '+message,'Error: network unavailable',"Error: Variable `json` not found"):
                if d=='module-global' and 'Variable' in bad:continue
                with self.subTest(d=d,bad=bad),self.assertRaises(ValueError):s.require_rejection(record(bad,1),d,'native')

    def test_nonzero_and_crash_are_not_valid_rejections(self):
        for code in (0,True,-9,37,101,124,127,137):
            with self.assertRaises(ValueError):s.require_rejection(record('Error: '+MESSAGES['inf'],code),'inf','native')
        for flag in ('spawn_error','interrupted','timed_out','output_truncated'):
            r=record('Error: '+MESSAGES['inf'],1);r[flag]=True
            with self.assertRaises(ValueError):s.require_rejection(r,'inf','native')

    def test_offsets_require_exact_original_byte_positions(self):
        for n in (6,8,10):
            for other in (6,8,10):
                if n!=other:
                    with self.assertRaises(ValueError):s.require_rejection(record('Error: '+s.NATIVE_MESSAGES['offset'+str(other)],1),'offset'+str(n),'native')

    def test_native_projection_never_weakens_reference_diagnostics(self):
        for d,message in s.NATIVE_MESSAGES.items():
            s.require_rejection(record('error: '+message,1),d,'native')
            if d not in ('native-info','external-file','depth65','range-quota','default-quota'):
                with self.assertRaises(ValueError):s.require_rejection(record('Error: '+message,1),d,'reference')
            for wrong in (message+' unrelated', 'unrelated '+message, '    error: '+message):
                with self.assertRaises(ValueError):s.require_rejection(record('error: '+wrong if not wrong.startswith('    ') else wrong,1),d,'native')

    def test_actual_module_negative_requires_reference_rc48(self):
        text="ERROR: MODULE.bazel:2:3: name 'json' is not defined\nERROR: Error computing the main repository mapping: syntax error in MODULE.bazel file for <root>\n"
        s.require_rejection(record(text,48),'module-global','reference')
        with self.assertRaises(ValueError):s.require_rejection(record(text,1),'module-global','reference')

    def test_resource_message_is_exact_neutral_and_never_fallback(self):
        for probe in ('range-quota','default-quota'):
            message='JSON host preparation resource limit exceeded'
            self.assertEqual(s.NATIVE_MESSAGES[probe],message)
            s.require_rejection(record('Error: '+message,1),probe,'native')
            for wrong in ('JSON host preparation resource limit exceeded (64 MiB)',
                    'JSON resource budget exceeded','JSON maximum open container depth exceeded (64)'):
                with self.assertRaises(ValueError):s.require_rejection(record('Error: '+wrong,1),probe,'native')
            with self.assertRaises(ValueError):s.require_rejection(record('JSON_ACTION_READY',0),probe,'native')


class ReplayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):cls.full=synthetic();cls.reference=synthetic(True)

    def passes(self,r):
        with mock.patch.object(s.engine,'PLATFORM_SHA',s.digest(b'SYNTHETIC NOT AN ARCHIVE')):return s.report_passes(r)

    def phase(self,r,family,probe,which='native'):
        return next(p for p in r['phases'] if (p['family'],p['probe'],p['engine'])==(family,probe,which))

    def mutate_output(self,r,family,probe,transform,which='native'):
        p=self.phase(r,family,probe,which)
        for stage,entries in p['outputs'].items():
            for i,e in enumerate(entries):
                entries[i]=entry(e['path'],transform(base64.b64decode(e['bytes_base64'])),e['mode'])

    def test_synthetic_replay_is_complete_but_not_engine_evidence(self):
        self.assertTrue(self.passes(self.full));self.assertTrue(self.passes(self.reference))

    def test_valid_plan_hash_cannot_authorize_wrong_method_profile(self):
        mutations=[('decode','weighted_bytes',64*1024*1024),('decode','nodes',100000),
            ('decode','effective_utf8_input_bytes',16*1024*1024),('encode','weighted_bytes',256*1024*1024),
            ('indent','nodes',250000),('encode_indent','budget_scope','per_helper')]
        for original in (self.full,self.reference):
            for method,key,value in mutations:
                r=copy.deepcopy(original);plan=r['plan']
                plan['native_capacity'][method][key]=value
                plan.pop('sha256');plan['sha256']=s.digest(json.dumps(plan,sort_keys=True).encode())
                self.assertFalse(self.passes(r),(method,key,value))

    def test_rehashed_old_decode_quota_input_cannot_replay_new_plan(self):
        r=copy.deepcopy(self.full);p=self.phase(r,'profile','default-quota')
        for files in [*p['input_snapshots'].values(),p['inputs_after']]:
            for path,e in list(files.items()):
                if path.endswith('/defs.bzl'):
                    raw=base64.b64decode(e['bytes_base64']).replace(b'"null,"*249999+"null]"',b'"0,"*100001+"0]"')
                    self.assertNotEqual(raw,base64.b64decode(e['bytes_base64']))
                    files[path]=entry(path,raw,e['mode'])
        self.assertFalse(self.passes(r))

    def test_missing_extra_duplicate_reordered_phases_and_reference_waiver(self):
        for kind in ('missing','extra','duplicate','reorder','waiver','unexecuted'):
            r=copy.deepcopy(self.full)
            if kind=='missing':r['phases'].pop()
            elif kind=='extra':r['phases'].append(copy.deepcopy(r['phases'][0]))
            elif kind=='duplicate':r['phases'][1]=copy.deepcopy(r['phases'][0])
            elif kind=='reorder':r['phases'][0],r['phases'][1]=r['phases'][1],r['phases'][0]
            elif kind=='waiver':r['phases'][0]['status']='KNOWN_REFERENCE_DEFECT'
            else:r['phases'][0]['runs']=[]
            self.assertFalse(self.passes(r),kind)

    def test_lost_float_lexeme_and_integer_type_even_rehashed(self):
        for family,probe,old,new in [('floats','golden1024',b'1000000000000000.2',b'1000000000000000.3'),
                ('floats','golden1024',b'\tfloat\tfloat\t',b'\tint\tfloat\t'),
                ('decode','identity-mutable-types',b'"int","int"',b'"float","float"'),
                ('decode','identity-mutable-types',b'123456789123456789123456789',b'123456789123456789123456780')]:
            r=copy.deepcopy(self.full);self.mutate_output(r,family,probe,lambda b:b.replace(old,new))
            self.assertFalse(self.passes(r))

    def test_default_wrong_identity_marker_and_null_not_fallback(self):
        r=copy.deepcopy(self.full);p=self.phase(r,'decode','identity-mutable-types');run=r['runs'][p['runs'][0]]
        run['stderr']=run['stderr'].replace('JSON_DEFAULT_IDENTITY','DEFAULT_WRONG_IDENTITY');streams(run)
        self.assertFalse(self.passes(r))
        r=copy.deepcopy(self.full);self.mutate_output(r,'decode','identity-mutable-types',lambda b:b.replace(b'null,null',b'42,null'))
        self.assertFalse(self.passes(r))

    def test_formatter_must_preserve_spelling_and_harmless_quirks(self):
        for old,new in [(b'1e+02',b'100.0'),(b'\\\\u0061',b'a'),(b'[}',b'[]')]:
            r=copy.deepcopy(self.full);self.mutate_output(r,'indent','lexemes',lambda b:b.replace(old,new));self.assertFalse(self.passes(r))

    def test_no_filepath_rewrite_or_fabricated_root(self):
        for old,new in [(b'rust-fastbuild',b'k8-fastbuild'),(b'"root":""',b'"root":"."'),(b'"short_path":"input.txt"',b'"short_path":"wrong"')]:
            r=copy.deepcopy(self.full);self.mutate_output(r,'profile','main-files',lambda b:b.replace(old,new));self.assertFalse(self.passes(r))

    def test_source_binary_registry_and_cleanup_mutations(self):
        changes=[lambda r:r['sources_after'][str(s.ROOT/'src/providers.rs')].update(sha256='0'*64),
            lambda r:r['executables_after']['reference'].update(sha256='0'*64),lambda r:r['registry_after'].clear(),
            lambda r:r['cleanup_receipt'].update(removed=False),lambda r:r['cleanup_receipt']['pids'].update({'987654321':True}),
            lambda r:r['cleanup_receipt'].update(remaining_pidfiles=['server.pid.txt']),lambda r:r['origins'].clear(),
            lambda r:r['runs'][1].update(preflight_binding_sha256='0'*64),lambda r:r['runs'][1].update(executable_sha256='0'*64),
            lambda r:r['runs'][1].update(returncode=False),lambda r:r['runs'][1].update(environment={})]
        for change in changes:
            r=copy.deepcopy(self.full);change(r);self.assertFalse(self.passes(r))

    def test_reference_only_cannot_close_native_gate_or_use_old_release(self):
        r=copy.deepcopy(self.reference);r['native_claim']['pass_claim']=True;self.assertFalse(self.passes(r))
        r=copy.deepcopy(self.full);r['configuration']['native_sha256']=s.OLD_NATIVE
        for k in ('executables','executables_after'):r[k]['native']['sha256']=s.OLD_NATIVE
        self.assertFalse(self.passes(r))

    def test_missing_real_callback_markers(self):
        for family,probe,marker in [('functional','provider19','JSON_PRODUCER_DECODE'),('functional','provider19','JSON_CONSUMER_ENCODE'),
                ('module-bzl','repository-callback','JSON_MODULE_BZL'),('module-bzl','repository-callback','JSON_REPOSITORY_CALLBACK')]:
            r=copy.deepcopy(self.full);p=self.phase(r,family,probe);run=r['runs'][p['runs'][0]]
            run['stderr']=run['stderr'].replace(marker,'WRONG');streams(run);self.assertFalse(self.passes(r))

    def test_provider_source_edit_output_counts_and_transition(self):
        for kind in ('missing','stale','cache','source','mode','path'):
            r=copy.deepcopy(self.full);p=self.phase(r,'functional','provider19')
            if kind=='missing':p['outputs'].pop('changed')
            elif kind=='stale':p['outputs']['changed']=copy.deepcopy(p['outputs']['cold'])
            elif kind=='cache':
                run=r['runs'][p['runs'][2]];run['stderr']=run['stderr'].replace('1 actions executed, 18 cache hits','19 actions executed, 0 cache hits');streams(run)
            elif kind=='source':p['input_snapshots']['2']=copy.deepcopy(p['input_snapshots']['0'])
            elif kind=='mode':p['outputs']['cold'][0]['mode']=0o755
            else:p['outputs']['cold'][0]['path']='/ghost/output'
            self.assertFalse(self.passes(r),kind)

    def test_repository_actual_bytes_mode_path_and_action_dependency(self):
        for kind in ('bytes','mode','path','input','hash','build','absent'):
            r=copy.deepcopy(self.full);p=self.phase(r,'module-bzl','repository-callback','reference')
            if kind=='bytes':p['generated']['repository'][0]=entry(p['generated']['repository'][0]['path'],b'{}')
            elif kind=='mode':p['generated']['repository'][0]['mode']=0o555
            elif kind=='path':p['generated']['repository'][0]['path']='/ghost/data'
            elif kind=='build':p['generated']['build'][0]['mode']=0o555
            elif kind=='absent':p['generated'].clear()
            else:
                log=p['logs']['cold'];spawns=s.shared.json_objects(base64.b64decode(log['bytes_base64']))
                if kind=='input':spawns[0]['inputs']=[]
                else:spawns[0]['inputs'][0]['digest']['hash']='0'*64
                p['logs']['cold']=entry(log['path'],'\n'.join(json.dumps(x) for x in spawns).encode())
            self.assertFalse(self.passes(r),kind)

    def test_native_repository_path_requires_actual_bound_tree_not_external_rewrite(self):
        for path in ('/ghost/data','/tmp/opencode/SYNTHETIC-NOT-EXECUTED-json-test/cases/module-bzl/repository-callback/native/cache/external/+ext+made/data',
                '/tmp/opencode/SYNTHETIC-NOT-EXECUTED-json-test/cases/module-bzl/repository-callback/native/cache/repositories/direct/trees/not-a-digest/data'):
            r=copy.deepcopy(self.full);p=self.phase(r,'module-bzl','repository-callback');run=r['runs'][p['runs'][-1]]
            p['generated']['repository'][0]['path']=path
            p['generated']['build'][0]['path']=str(Path(path).parent/'BUILD.bazel')
            run['stdout']='@@+ext+made//:data\t'+path+'\n';streams(run)
            self.assertFalse(self.passes(r))

    def test_errors_never_publish_actions_outputs_or_default_success(self):
        for kind in ('action','output','success','log'):
            r=copy.deepcopy(self.full);p=self.phase(r,'decode','type-default','reference');run=r['runs'][p['runs'][0]]
            if kind=='action':p['actions_after']['actions']['entries']=['bad']
            elif kind=='output':p['outputs']['cold']=[]
            elif kind=='success':run.update(returncode=0,stderr='JSON_CALLBACK\nJSON_ACTION_READY\n');streams(run)
            else:p['logs']['reject']=entry(p['logs']['reject']['path'],b'{"mnemonic":"BAD"}')
            self.assertFalse(self.passes(r))

    def test_stream_bytes_truncation_and_command_binding(self):
        for kind in ('stream','truncated','command','cache'):
            r=copy.deepcopy(self.full)
            if kind=='stream':r['runs'][1]['stderr_base64']=base64.b64encode(b'wrong').decode()
            elif kind=='truncated':r['runs'][1]['output_truncated']=True
            elif kind=='command':r['runs'][1]['argv'][-1]='//:other'
            else:self.phase(r,'api','metadata')['actions_after']['actions']['entries']=[]
            self.assertFalse(self.passes(r))


if __name__=='__main__':unittest.main()
