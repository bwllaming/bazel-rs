#!/usr/bin/env python3
"""Bounded RunEnvironmentInfo metadata differential, NOT full Bazel parity.

Reference preparation never executes native. Full execution requires a new frozen
native SHA. Real Starlark fixtures, genuine ELF, local registry; no fallback.
PASS covers this bounded profile only. The preserved first matrix FAILED; its
historical mismatch is retained separately from the new local closure probe.
"""

import base64
import datetime
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile

import validate_symbolic_macros as utility

ROOT, COMMON_FLAGS = utility.ROOT, utility.COMMON_FLAGS
SUITE_NAME = 'run-environment-info'
common, direct = utility.common, utility.direct
digest, evidence, snapshot, valid_evidence = utility.digest, utility.evidence, utility.snapshot, utility.valid_evidence
success, DirectReference = utility.success, utility.DirectReference
REFERENCE_SHA256, SOURCE_PIN, REGISTRY = utility.REFERENCE_SHA256, utility.SOURCE_PIN, utility.REGISTRY
GAP_RECEIPT = ROOT / 'tests/fixtures/run_environment_info/module_gap.json'
BASELINE_COMMIT = '1f559e6d003284cd5e6abd758a9eace0c0cfec5f'
BASELINE_SOURCES = {
    'src/main.rs': 'd5b478ed48defa8be291fd5c889e41589bd336c17d244d3191f230be4c3d71ce',
    'src/repository.rs': 'c35a1267e5d4e164faa5b7553bdfc92ee40005afa198cd2a02d307e5812c0057',
    'src/module_graph.rs': '7d488212adf8845e1418c24ad873801d6de2975a1a285f72bf5074e964f190f2',
    'src/bzlmod.rs': 'fb4876c5600a4b155f54a2c5e96874983dc5df5978b922fc97a337ab0632d6ed',
}
BASELINE_TESTS = [
    dict(path='src/repository.rs', name='main_and_canonical_parsing_are_lazy_even_with_broken_module',
        baseline_line=1084, sha256='6d2f4399cbcec9199ad764d532afbe28139bf6d9ba2dae9584b1e96f6d9b740f'),
    dict(path='src/package.rs', name='local_bzl_loading_does_not_resolve_unused_invalid_module',
        baseline_line=1355, sha256='dbd3ddd880baf1a697440f7b978f9ec2f5420d0c2aea1c11bdf228ba2da80aa1'),
]
KNOWN_PARITY_GAPS = [dict(
    id='local-query-skips-invalid-module', status='KNOWN_DIFFERENTIAL_MISMATCH', original_matrix_status='FAIL',
    scope='Main-repository local queries may skip MODULE evaluation; undefined MODULE globals are then not diagnosed.',
    retained_receipt=dict(path='artifacts/run-environment-info-validation-first-attempt.json',
        sha256='1c8fff1a7b38b9f9fa62d7d1863fe3c69d23f6020480b4a82412e692cf9bda08'),
    evidence_excerpt=dict(path='tests/fixtures/run_environment_info/module_gap.json',
        sha256='164f1e618aab2411447cef8ebf936fa06c51dba2227b1d2ad1c3fbba42ab48aa'),
    original_phase=dict(family='module', probe='module-global', stage='loading', command='query', target='//app:check'),
    module=dict(path='MODULE.bazel', text='module(name="runenv_contract")\nx=RunEnvironmentInfo()\n',
        sha256='ac8da08eacf4c2033b611feda4f552d83bad0b3425f45923a67a5f01d3085304'),
    observations={
        'reference': dict(run_id=142, phase_status='PASS', returncode=48, stdout='',
            stdout_sha256='e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
            stderr_sha256='39fee5761771d3adcd1efed14288a27ad766b6b943e1aadb3c232d1b4b5bcc7d',
            executable_sha256=REFERENCE_SHA256,
            full_phase_sha256='853476bbe87f3678817d8193adc34ac630dd2ddf47d4a16ae5e4c1d4f0013f53'),
        'native': dict(run_id=143, phase_status='FAIL', returncode=0, stdout='//app:check\n',
            stdout_sha256='36163f3dca10ff251cba39ed8decf79486aa3c741abd88621c0f852c637f0127',
            stderr_sha256='b188546e8c87df2590bd3933f2a54fb11c50f9d7be707baed16976cb381fefa4',
            executable_sha256='b90ab24312fa4c41be2eda513888026f09fc1eabfc0bd123a8dfecc740d15947',
            full_phase_sha256='6cecde952e662888ac5c4af339453cb7a4cd4ed1643651e951ee6face3572dad'),
    },
    paired_replacement=dict(family='module', probe='module-global', target='@missing//:all',
        purpose='availability-on-demand: constructor is not predeclared when MODULE is evaluated; reject before repository lookup'),
    baseline=dict(commit=BASELINE_COMMIT, historical_sources=BASELINE_SOURCES, preexisting_tests=BASELINE_TESTS,
        evidence='Historical git blob hashes, not current source identity; unchanged low-level lazy regression bodies. No old baseline binary was executed.'),
    parity_claim=False,
)]
CLOSURE_PROBE = dict(family='module', probe='module-global-local', stage='loading', command='query',
    target='//app:check', module_sha256=KNOWN_PARITY_GAPS[0]['module']['sha256'],
    returncodes={'native': 1, 'reference': 48})
SOURCES = list(dict.fromkeys([Path(__file__).resolve(), ROOT / 'tests/test_validate_run_environment_info.py',
    GAP_RECEIPT, *(ROOT / p for p in BASELINE_SOURCES), ROOT / 'src/package.rs', *utility.SOURCES]))
CASEPLAN = [
    common.case('constructor', ('defaults', 'positional', 'mixed', 'named', 'unicode-nul',
        'none-env', 'none-inherited', 'list-env', 'key-int', 'value-label', 'inherited-string',
        'inherited-dict', 'inherited-int', 'unknown', 'hidden-flag', 'third-positional', 'duplicate')),
    common.case('ownership', ('introspection', 'input-snapshot', 'fresh-properties', 'frozen-owner',
        'saved-dict', 'saved-list', 'structural-hash', 'immutable')),
    common.case('transport', ('nested', 'multi-hop', 'predeclared', 'missing-key', 'required', 'instance-requirement',
        'string-requirement', 'string-key', 'instance-key')),
    common.case('phase', ('bare', 'list', 'default', 'duplicate', 'atomic')),
    common.case('module', ('build-global', 'module-global-local', 'module-global', 'alias', 'repository', 'extension')),
    common.case('action', ('implicit', 'explicit', 'all-entries')),
    common.case('native-boundary', ('executable', 'test'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASEPLAN]
LIMITATIONS = [
    'Bounded metadata API, nested ordinary-provider transport and explicit rule-authored action env only.',
    'No executable/test rule classification, DefaultInfo executable/runfiles, CLI run/test or testing.TestEnvironment support.',
    'Executable/test descriptors are native-only unsupported boundaries, never paired positive cases.',
    'Inherited names remain metadata: no host reads or automatic action environment folding.',
    'Unicode and NUL values are metadata only; NUL is never passed to an OS environment.',
    'Reference-only preparation cannot claim native PASS; synthetic unit receipts are not engine evidence.',
    'PASS is bounded metadata-profile PASS, not original-matrix or full Bazel parity.',
    'historical_parity_gaps preserves the original native0/reference48 FAIL unchanged; closure requires new paired local-query evidence.',
    'The external @missing//:all MODULE-availability regression remains separate from local root validation.',
]
MODULE = 'module(name="runenv_contract")\n'
READY, ENTERED = 'RUNENV_READY', 'RUNENV_CONSUMER'
HOST = {'RUNENV_CONTRACT_LITERAL': 'HOST_SYNTHETIC', 'RUNENV_CONTRACT_SECOND': 'HOST_SECOND'}
KEYS = 'P=provider()\nOther=provider()\nR=RunEnvironmentInfo\n'
INFO = 'R({"Z":"z","A":"a"},["N","N"])'


def write(expression):
    return ('out=ctx.actions.declare_file(ctx.label.name+".out")\n'
        'ctx.actions.write(out,'+expression+',is_executable=True)\n'
        'return [DefaultInfo(files=depset([out]))]\n')


def rule(body, prefix=KEYS, attrs='{}', extra=''):
    return (prefix+'def _impl(ctx):\n'+common.indent('print("'+ENTERED+'")\n'+body)
        +'check=rule(implementation=_impl,attrs='+attrs+extra+')\nprint("'+READY+'")\n')


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    files = {'MODULE.bazel': MODULE, 'BUILD.bazel': '', 'defs/BUILD.bazel': '',
        'app/BUILD.bazel': 'load("//defs:check.bzl","check")\ncheck(name="check")\n'}
    value = dict(target='//app:check', expected={'check.out': b'struct(environment = {}, inherited_environment = [])'},
        actions=1, diagnostic=None, stage='success', queries=[], genuine=False, markers=[READY], callback=True)
    body, prefix, attrs, extra = write('repr(R())'), KEYS, '{}', ''

    def reject(diagnostic, stage='loading'):
        value.update(diagnostic=diagnostic, stage=stage, expected={}, actions=0, callback=stage == 'callback')
        if stage == 'loading':
            value['markers'] = []

    if name == 'constructor':
        calls = {'defaults': 'R()', 'positional': 'R({"Z":"z"},("N","N"))',
            'mixed': 'R({"Z":"z"},inherited_environment=["N","N"])',
            'named': 'R(inherited_environment=["N","N"],environment={"Z":"z"})',
            'unicode-nul': 'R({"Z":"last","A":"first","":"","é":"☃","nul":"a\\000b"},("Z","Z","","☃"))',
            'none-env': 'R(environment=None)', 'none-inherited': 'R(inherited_environment=None)',
            'list-env': 'R(environment=[])', 'key-int': 'R({1:"v"})', 'value-label': 'R({"K":Label("//app:check")})',
            'inherited-string': 'R(inherited_environment="N")', 'inherited-dict': 'R(inherited_environment={})',
            'inherited-int': 'R(inherited_environment=[1])', 'unknown': 'R(unrecognized=True)',
            'hidden-flag': 'R(error_on_non_executable=False)', 'third-positional': 'R({},[],False)',
            'duplicate': 'R({},environment={})'}
        if probe in ('defaults', 'positional', 'mixed', 'named', 'unicode-nul'):
            body = write('repr('+calls[probe]+')')
            if probe in ('positional', 'mixed', 'named'):
                value['expected'] = {'check.out': b'struct(environment = {"Z": "z"}, inherited_environment = ["N", "N"])'}
            elif probe == 'unicode-nul':
                value['expected'] = {'check.out': 'struct(environment = {"Z": "last", "A": "first", "": "", "é": "☃", "nul": "a\\x00b"}, inherited_environment = ["Z", "Z", "", "☃"])'.encode()}
        else:
            prefix += 'bad='+calls[probe]+'\n'
            reject(probe)
    elif name == 'ownership':
        if probe == 'introspection':
            body = 'p=R()\n'+write('str([type(R),repr(R),dir(R),type(p),repr(p),dir(p),type(p.environment),type(p.inherited_environment),{R:"key"}[RunEnvironmentInfo]])')
            value['expected'] = {'check.out': b'["Provider", "<function RunEnvironmentInfo>", [], "RunEnvironmentInfo", "struct(environment = {}, inherited_environment = [])", ["environment", "inherited_environment"], "dict", "list", "key"]'}
        elif probe == 'input-snapshot':
            body = ('d={"Z":"z"}\nn=["N","N"]\np=R(d,n)\nd["Z"]="changed"\nn.append("A")\n'
                +write('str([p.environment,p.inherited_environment,d,n])'))
            value['expected'] = {'check.out': b'[{"Z": "z"}, ["N", "N"], {"Z": "changed"}, ["N", "N", "A"]]'}
        elif probe in ('fresh-properties', 'frozen-owner'):
            if probe == 'frozen-owner':
                prefix += 'D={"Z":"z","A":"a"}\nN=("N","N")\nF=R(D,N)\n'
                body = 'p=R(D,N)\nf=F\n'
            else:
                body = 'p='+INFO+'\nf=p\n'
            body += ('d=f.environment\nn=f.inherited_environment\nd["Z"]="changed"\nn.append("A")\n'
                +write('str([d,n,f.environment,f.inherited_environment,p==f])'))
            value['expected'] = {'check.out': b'[{"Z": "changed", "A": "a"}, ["N", "N", "A"], {"Z": "z", "A": "a"}, ["N", "N"], True]'}
        elif probe in ('saved-dict', 'saved-list'):
            prefix += 'F='+INFO+'\nS=F.'+('environment' if probe == 'saved-dict' else 'inherited_environment')+'\n'
            body = ('S["new"]="v"\n' if probe == 'saved-dict' else 'S.append("new")\n')+'return []\n'
            reject('frozen', 'callback')
        elif probe == 'structural-hash':
            prefix += 'F='+INFO+'\n'
            body = ('a=F\nb=R({"A":"a","Z":"z"},("N","N"))\nd={a:"before"}\nd[b]="after"\n'
                +write('str([a==b,d[a],len(d),a!=R({"Z":"changed","A":"a"},["N","N"]),'
                    'a!=R({"Z":"z","A":"a"},["N"]),R({},["A","B"])!=R({},["B","A"]),'
                    'a!=Other(environment=a.environment,inherited_environment=a.inherited_environment),a!={},a!=[]])'))
            value['expected'] = {'check.out': b'[True, "after", 1, True, True, True, True, True, True]'}
        else:
            body = 'p=R()\np.environment={}\nreturn []\n'
            reject('immutable', 'callback')
    elif name in ('transport', 'action'):
        expr = INFO if name == 'transport' else 'R({"RUNENV_CONTRACT_LITERAL":"LITERAL","RUNENV_CONTRACT_SECOND":"SECOND"},["RUNENV_CONTRACT_LITERAL","RUNENV_CONTRACT_LITERAL"])'
        prefix += ('def _produce(ctx):\n    return [P(info='+expr+',payload=['+expr+'],mapping={"x":'+expr+'},nested=struct(info='+expr+'))]\n'
            'produce=rule(implementation=_produce)\n')
        files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check","produce")\nproduce(name="producer")\ncheck(name="check",dep=":producer")\n'
        attrs = '{"dep":attr.label(providers=[P])}'
        body = 't=ctx.attr.dep\np=t[P].info\n'+write('str([R in t,p.environment,p.inherited_environment,p=='+INFO+',t[P].payload[0]==p,t[P].mapping["x"]==p,t[P].nested.info==p])')
        value['expected'] = {'check.out': b'[False, {"Z": "z", "A": "a"}, ["N", "N"], True, True, True, True]'}
        if name == 'action':
            body = ('p=ctx.attr.dep[P].info\nout=ctx.actions.declare_file("check.out")\n'
                'ctx.actions.run_shell(outputs=[out],command='+repr('printf "%s'+(':%s' if probe == 'all-entries' else '')+'" "${RUNENV_CONTRACT_LITERAL-unset}"'+(' "${RUNENV_CONTRACT_SECOND-unset}"' if probe == 'all-entries' else '')+' > "$1"; chmod +x "$1"')
                +',arguments=[out.path]'+(',env=p.environment' if probe != 'implicit' else '')+')\n'
                'return [DefaultInfo(files=depset([out]))]\n')
            value['expected'] = {'check.out': {'implicit': b'unset', 'explicit': b'LITERAL', 'all-entries': b'LITERAL:SECOND'}[probe]}
            if probe == 'all-entries':
                files['defs/relay.bzl'] = ('load("//defs:check.bzl","P")\ndef _relay(ctx):\n'
                    '    p=ctx.attr.dep[P].info\n    d=p.environment\n    d["RUNENV_CONTRACT_LITERAL"]="changed"\n'
                    '    n=p.inherited_environment\n    n.append("extra")\n    return [P(info=p)]\n'
                    'relay=rule(implementation=_relay,attrs={"dep":attr.label(providers=[P])})\n')
                files['app/BUILD.bazel'] = ('load("//defs:check.bzl","check","produce")\nload("//defs:relay.bzl","relay")\n'
                    'produce(name="producer")\nrelay(name="relay",dep=":producer")\ncheck(name="check",dep=":relay")\n')
                body = body.replace('return [DefaultInfo(files=depset([out]))]',
                    'meta=ctx.actions.declare_file("metadata.out")\n'
                    'ctx.actions.write(meta,repr(p),is_executable=True)\n'
                    'return [DefaultInfo(files=depset([out,meta]))]')
                value['expected']['metadata.out'] = (b'struct(environment = {"RUNENV_CONTRACT_LITERAL": "LITERAL", '
                    b'"RUNENV_CONTRACT_SECOND": "SECOND"}, inherited_environment = ["RUNENV_CONTRACT_LITERAL", "RUNENV_CONTRACT_LITERAL"])')
                value['actions'] = 2
        elif probe == 'multi-hop':
            files['defs/relay.bzl'] = ('load("//defs:check.bzl","P")\ndef _relay(ctx):\n'
                '    p=ctx.attr.dep[P].info\n    d=p.environment\n    d["Z"]="changed"\n'
                '    n=p.inherited_environment\n    n.append("extra")\n'
                '    return [P(info=p,payload=[p],mapping={"x":p},nested=struct(info=p))]\n'
                'relay=rule(implementation=_relay,attrs={"dep":attr.label(providers=[P])})\n')
            files['app/BUILD.bazel'] = ('load("//defs:check.bzl","check","produce")\nload("//defs:relay.bzl","relay")\n'
                'produce(name="producer")\nrelay(name="relay",dep=":producer")\ncheck(name="check",dep=":relay")\n')
        elif probe == 'predeclared':
            prefix = KEYS+('def _produce(ctx):\n    ctx.actions.write(ctx.outputs.out,"fallback",is_executable=True)\n'
                '    return [P(info='+INFO+'),Other()]\n'
                'produce=rule(implementation=_produce,attrs={"out":attr.output(mandatory=True)})\n')
            files['app/BUILD.bazel'] = files['app/BUILD.bazel'].replace(
                'produce(name="producer")', 'produce(name="producer",out="producer.out")')
            value.update(target='//app:producer', expected={'producer.out': b'fallback'}, callback=False)
        elif probe == 'required':
            attrs = '{"dep":attr.label(providers=[R])}'
            reject('required', 'preflight')
        elif probe in ('instance-requirement', 'string-requirement'):
            attrs = '{"dep":attr.label(providers=['+('R()' if probe.startswith('instance') else '"RunEnvironmentInfo"')+'])}'
            reject('constraint')
        elif probe in ('missing-key', 'string-key', 'instance-key'):
            key = {'missing-key': 'R', 'string-key': '"RunEnvironmentInfo"', 'instance-key': 'R()'}[probe]
            body = 'p=ctx.attr.dep['+key+']\nreturn []\n'
            reject('missing-provider' if probe == 'missing-key' else 'target-key', 'callback')
    elif name == 'phase':
        # Unique output names and a shell side effect make premature execution observable.
        body = ('out=ctx.actions.declare_file("rejected.out")\n'
            'ctx.actions.run_shell(outputs=[out],command="printf BAD > rejected.marker; printf BAD > $1",arguments=[out.path])\n')
        returns = {'bare': 'R()', 'list': '[R()]', 'default': '[DefaultInfo(files=depset([out])),R()]',
            'duplicate': '[R(),R()]', 'atomic': '[DefaultInfo(files=depset([out])),R()]'}
        body += 'return '+returns[probe]+'\n'
        reject('duplicate-provider' if probe == 'duplicate' else 'ordinary-return', 'callback')
        value['queries'] = [('//app:check', ['//app:check'])]
        if probe == 'atomic':
            prefix += ('def _good(ctx):\n    out=ctx.actions.declare_file("good.out")\n'
                '    ctx.actions.run_shell(outputs=[out],command="printf GOOD > good.marker; printf GOOD > $1",arguments=[out.path])\n'
                '    return [DefaultInfo(files=depset([out]))]\ngood=rule(implementation=_good)\n')
            files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check","good")\ngood(name="good")\ncheck(name="check")\n'
            value['target'] = ['//app:good', '//app:check']
    elif name == 'module':
        if probe in ('build-global', 'module-global', 'module-global-local'):
            files['app/BUILD.bazel' if probe == 'build-global' else 'MODULE.bazel'] += 'x=RunEnvironmentInfo()\n'
            reject('missing-global')
            if probe == 'module-global':
                value['target'] = '@missing//:all'
        elif probe == 'alias':
            files['defs/key.bzl'] = 'Alias=RunEnvironmentInfo\n'
            files['defs/reexport.bzl'] = 'load(":key.bzl","Alias")\nReexport=Alias\n'
            prefix = 'load(":reexport.bzl","Reexport")\n'+KEYS
            body = write('str([R==Reexport,{R:"key"}[Reexport],Reexport()==R()])')
            value['expected'] = {'check.out': b'[True, "key", True]'}
            files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check")\nload("//defs:reexport.bzl","Reexport")\nx=Reexport()\ncheck(name="check")\n'
        else:
            files['defs/context.bzl'] = ('Init=RunEnvironmentInfo({"K":"init"})\n'
                'def factory(k):\n    def impl(ctx):\n'
                '        p=RunEnvironmentInfo({"K":k})\n        d=Init.environment\n        d["K"]="fresh"\n'
                '        ctx.file("data",repr(p),executable=False)\n'
                '        ctx.file("BUILD.bazel",\'exports_files(["data"],visibility=["//visibility:public"])\')\n'
                '    return repository_rule(implementation=impl)\nr=factory("repo")\n'
                'def _extension(ctx):\n    p=RunEnvironmentInfo({"K":"ext"})\n'
                '    if p.environment["K"]!="ext":\n        fail("EXT_VALUE_MISMATCH")\n'
                '    r(name="made")\ne=module_extension(implementation=_extension)\n')
            files['MODULE.bazel'] += ('r=use_repo_rule("//defs:context.bzl","r")\nr(name="made")\n' if probe == 'repository'
                else 'e=use_extension("//defs:context.bzl","e")\nuse_repo(e,"made")\n')
            files['app/BUILD.bazel'] = 'genrule(name="check",srcs=["@made//:data"],outs=["check.out"],cmd="cat $(SRCS) > $@; chmod +x $@")\n'
            value.update(expected={'check.out': b'struct(environment = {"K": "repo"}, inherited_environment = [])'}, markers=[], callback=False)
    elif name == 'native-boundary':
        extra = ','+probe+'=True'
        reject('rule-'+probe)
    files['defs/check.bzl'] = rule(body, prefix, attrs, extra)
    value['failure_command'] = 'query' if value['stage'] == 'loading' else 'build'
    value['returncodes'] = {'native': 1, 'reference': 48 if name == 'module' and probe in ('module-global', 'module-global-local')
        else 7 if value['stage'] == 'loading' else 1} if value['diagnostic'] else {'native': 0, 'reference': 0}
    value['files'] = {p: text.encode() for p, text in files.items()}
    return value


DIAGNOSTICS = {
    'none-env': r"(?:parameter 'environment'.*NoneType.*dict|RunEnvironmentInfo.*environment.*(?:dict|None))",
    'none-inherited': r"(?:parameter 'inherited_environment'.*NoneType.*sequence|RunEnvironmentInfo.*inherited_environment.*(?:sequence|list|tuple))",
    'list-env': r"(?:parameter 'environment'.*list.*dict|RunEnvironmentInfo.*environment.*dict)",
    'key-int': r"(?:got dict<int, string> for 'environment'|RunEnvironmentInfo.*environment.*(?:key|string))",
    'value-label': r"(?:got dict<string, Label> for 'environment'|RunEnvironmentInfo.*environment.*(?:value|string))",
    'inherited-string': r"(?:parameter 'inherited_environment'.*string.*sequence|RunEnvironmentInfo.*inherited_environment.*(?:sequence|list|tuple))",
    'inherited-dict': r"(?:parameter 'inherited_environment'.*dict.*sequence|RunEnvironmentInfo.*inherited_environment.*(?:sequence|list|tuple))",
    'inherited-int': r"(?:index 0 of inherited_environment.*int.*string|RunEnvironmentInfo.*inherited_environment.*string)",
    'unknown': r"(?:RunEnvironmentInfo.*unexpected keyword argument 'unrecognized'|Found `unrecognized` extra named parameter)|^error: RunEnvironmentInfo: unknown argument unrecognized$",
    'hidden-flag': r"(?:RunEnvironmentInfo.*unexpected keyword argument 'error_on_non_executable'|Found `error_on_non_executable` extra named parameter)|^error: RunEnvironmentInfo: unknown argument error_on_non_executable$",
    'third-positional': r'RunEnvironmentInfo.*(?:no more than 2 positional arguments but got 3|(?:at most|maximum).*2.*positional)|^error: RunEnvironmentInfo: expected at most two positional arguments$',
    'duplicate': r"RunEnvironmentInfo.*(?:multiple values|duplicate).*environment",
    # Starlark.checkMutable (net/starlark/java/eval/Starlark.java:262-265); the
    # native interpreter prints the same text since N20 (docs/STARLARK_VENDOR.md),
    # so its former bare 'error: Immutable' is no longer accepted. The text ends
    # its error line ("trying to mutate a frozen %s value"), so suffixed
    # excerpts are not proof.
    'frozen': r'(?:trying to mutate a frozen (?:dict|list) value$|(?:dict|list).*immutable|cannot mutate.*frozen)',
    'immutable': r'(?:cannot set .environment field of RunEnvironmentInfo|Operation `\.environment=` not supported on type `RunEnvironmentInfo`)',
    'required': r'(?:does not have mandatory providers.*RunEnvironmentInfo|missing.*(?:required|mandatory).*RunEnvironmentInfo)',
    'constraint': r'(?:providers.*(?:sequence|Provider|constructor)|Provider.*providers)',
    'missing-provider': r"(?:(?:does not|doesn't) contain declared provider.*RunEnvironmentInfo|(?:missing|does not have|not found).*provider.*RunEnvironmentInfo)|^error: Target @@//app:producer does not provide builtin:RunEnvironmentInfo$",
    'target-key': r'(?:Target only supports indexing by object constructors|(?:Target|target).*index.*(?:Provider|constructor))',
    'ordinary-return': r'Returning RunEnvironmentInfo from a non-executable, non-test target has no effect',
    'duplicate-provider': r'(?:multiple conflicting returned providers.*RunEnvironmentInfo|(?:duplicate|multiple).*RunEnvironmentInfo.*provider|Returning RunEnvironmentInfo from a non-executable, non-test target has no effect)|^bazel-rs: analysis of //app:check: implementation returned provider builtin:RunEnvironmentInfo more than once$',
    'missing-global': r"(?:name ['`]RunEnvironmentInfo['`] is not defined|Variable `RunEnvironmentInfo` not found)",
    'rule-executable': r'(?:executable.*(?:unsupported|not supported|False)|(?:unsupported|not supported).*executable)',
    'rule-test': r'(?:test.*(?:unsupported|not supported|False)|(?:unsupported|not supported).*test)',
}


def require_rejection(record, diagnostic, expected_code=None):
    code, text = record['returncode'], record['stderr']
    errors = utility.error_lines(text)
    checked = text
    if diagnostic == 'missing-global':
        checked = re.sub(r'^ERROR: Error computing the main repository mapping: syntax error in MODULE\.bazel file for <root>\s*$', '', checked, flags=re.M)
    stale = re.search(r'(?:RunEnvironmentInfo[^\n]*(?:not found|not defined)|(?:no field|no attribute)[^\n]*RunEnvironmentInfo)', errors, re.I)
    repository_lookup_error = diagnostic == 'missing-global' and re.search(
        r'unknown repository|repository[^\n]*(?:not declared|not found|does not exist|could not be resolved)|'
        r'no repository visible as|no such package[^\n]*@@?missing', errors, re.I)
    if (diagnostic not in DIAGNOSTICS or type(code) is not int or not 0 < code < 128
            or code in (37, 101, 124, 125, 126, 127) or record['timed_out']
            or any(record.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or (expected_code is not None and code != expected_code)
            or record['stdout'].strip() or common.UNRELATED.search(checked)
            or (stale and diagnostic != 'missing-global') or repository_lookup_error
            or not re.search(DIAGNOSTICS[diagnostic], errors, re.I | re.M)):
        raise ValueError('expected controlled RunEnvironmentInfo rejection: '+diagnostic)


phases_for = common.phases_for
command_argv, action_store, no_actions = utility.command_argv, utility.action_store, utility.no_actions
materialize = utility.materialize


def steps_for(value, engine):
    steps = [('query', target, []) for target, _ in value['queries']]
    if value['diagnostic']:
        return steps+[(value['failure_command'], value['target'], [])]
    for _ in range(2):
        steps.append(('build', value['target'], []))
        if engine == 'reference':
            steps.append(('cquery', value['target'], ['--output=files']))
    return steps


def plan_receipt(reference_only=False):
    plan = []
    for item in CASEPLAN:
        for probe, engine in phases_for(item, reference_only):
            v = fixture(item['name'], probe)
            plan.append(dict(family=item['name'], probe=probe, engine=engine, stage=v['stage'],
                steps=steps_for(v, engine), diagnostic=v['diagnostic'], actions=v['actions'],
                returncode=v['returncodes'][engine],
                expected={p: base64.b64encode(raw).decode() for p, raw in v['expected'].items()},
                fixture_sha256=digest(json.dumps({p: base64.b64encode(raw).decode() for p, raw in v['files'].items()}, sort_keys=True).encode())))
    return dict(families=len(CASEPLAN), probes=sum(len(c['probes']) for c in CASEPLAN), phases=len(plan),
        commands=2+sum(len(p['steps']) for p in plan), sha256=digest(json.dumps(plan, sort_keys=True).encode()))


def require_markers(record, value, command, first, engine, rejection):
    if rejection and value['files'].get('MODULE.bazel') == KNOWN_PARITY_GAPS[0]['module']['text'].encode():
        if any(utility.marker_count(record, marker) for marker in (READY, ENTERED)):
            raise ValueError('BUILD/bzl/callback loaded before root MODULE validation')
    if first or engine == 'native':
        for marker in value['markers']:
            if not utility.marker_count(record, marker):
                raise ValueError('missing loading/phase marker: '+marker)
    entered = utility.marker_count(record, ENTERED)
    if (command == 'query' or (rejection and value['stage'] != 'callback')) and entered:
        raise ValueError('analysis callback executed before prerequisite validation')
    if value['callback'] and command == 'build' and (first or engine == 'native' or rejection) and not entered:
        raise ValueError('missing actual consumer callback')


def output_paths(record, value, engine, scratch, workspace, reference):
    return (direct.native_paths(record, value['target'], scratch) if engine == 'native'
        else utility.reference_output_paths(record['stdout'], workspace, reference.output_base))


def rejection_artifacts(workspace, reference, cache):
    names = ('rejected.out', 'good.out', 'rejected.marker', 'good.marker')
    return sorted(str(p) for root in (workspace, reference.output_base / 'execroot', cache)
        if root.exists() for p in root.rglob('*') if p.is_file() and p.name in names)


def run_phase(args, runner, scratch, workspace, reference, item, probe, engine):
    value = fixture(item['name'], probe)
    paths = materialize(workspace, value)
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    phase = dict(probe=probe, engine=engine, stage=value['stage'], cache=str(cache), runs=[], outputs=[], status='FAIL',
        inputs_before=snapshot(paths), action_store_before=action_store(cache) if engine == 'native' else None,
        rejection_artifacts_before=rejection_artifacts(workspace, reference, cache))
    try:
        if phase['rejection_artifacts_before']:
            raise ValueError('stale action markers')
        if engine == 'native':
            no_actions(phase['action_store_before'], cache)
            if any(v['exists'] for v in phase['action_store_before'].values()):
                raise ValueError('native cache was not fresh before execution')
        for i, (command, target, extra) in enumerate(steps_for(value, engine)):
            r = runner.run(item['name']+'.'+probe+'.'+engine+'.'+str(i),
                command_argv(args, reference, engine, cache, scratch / 'registry', command, target, extra), workspace)
            phase['runs'].append(r['id'])
            rejection = bool(value['diagnostic'] and i >= len(value['queries']))
            if rejection:
                require_rejection(r, value['diagnostic'], value['returncodes'][engine])
            else:
                success(r)
                if command == ('build' if engine == 'native' else 'cquery'):
                    phase['outputs'].append([evidence(p) for p in output_paths(r, value, engine, scratch, workspace, reference)])
            require_markers(r, value, command, i == 0, engine, rejection)
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    finally:
        phase['inputs_after'] = snapshot(paths)
        phase['action_store_after'] = action_store(cache) if engine == 'native' else None
        phase['rejection_artifacts_after'] = rejection_artifacts(workspace, reference, cache)
    return phase


def validate_phase(phase, report, args, workspace, reference, item):
    probe, engine = phase['probe'], phase['engine']
    if (probe, engine) not in phases_for(item, args.reference_only):
        raise ValueError('unknown phase/engine or reference claim for native-only boundary')
    value = fixture(item['name'], probe)
    if (phase['status'] != 'PASS' or phase.get('error') or phase['stage'] != value['stage']
            or phase['inputs_before'] != phase['inputs_after']):
        raise ValueError('failed phase, wrong semantic stage or changed source')
    planned = {str(workspace / p): raw for p, raw in value['files'].items()}
    if set(planned) != set(phase['inputs_before']):
        raise ValueError('missing or unplanned source inputs')
    for path, raw in planned.items():
        e = phase['inputs_before'][path]
        if e['path'] != path or e['kind'] != 'file' or e['mode'] != 0o644 or not valid_evidence(e, raw):
            raise ValueError('source differs from immutable fixture plan')
    scratch = Path(report['scratch_directory'])
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    if phase['cache'] != str(cache) or phase['rejection_artifacts_before'] or phase['rejection_artifacts_after']:
        raise ValueError('unplanned cache or prematurely executed action artifacts')
    if engine == 'native':
        no_actions(phase['action_store_before'], cache)
        if any(v['exists'] for v in phase['action_store_before'].values()):
            raise ValueError('native cache was not fresh')
        if value['diagnostic']:
            no_actions(phase['action_store_after'], cache)
    elif phase['action_store_before'] is not None or phase['action_store_after'] is not None:
        raise ValueError('reference contains native action claims')
    steps = steps_for(value, engine)
    if len(phase['runs']) != len(steps) or len(set(phase['runs'])) != len(steps):
        raise ValueError('missing, duplicate or extra commands')
    output_index = build_index = 0
    for i, (rid, (command, target, extra)) in enumerate(zip(phase['runs'], steps)):
        r = report['runs'][rid]
        if (type(rid) is not int or r['id'] != rid or r['cwd'] != str(workspace) or r['timeout_seconds'] != args.timeout
                or r['name'] != item['name']+'.'+probe+'.'+engine+'.'+str(i)
                or r['argv'] != command_argv(args, reference, engine, cache, scratch / 'registry', command, target, extra)):
            raise ValueError('command provenance mismatch')
        rejection = bool(value['diagnostic'] and i >= len(value['queries']))
        require_markers(r, value, command, i == 0, engine, rejection)
        if rejection:
            require_rejection(r, value['diagnostic'], value['returncodes'][engine])
            continue
        success(r)
        if i < len(value['queries']):
            if sorted(r['stdout'].splitlines()) != value['queries'][i][1]:
                raise ValueError('query did not return exact planned targets')
            continue
        if command == 'build':
            if engine == 'native':
                common.action_counts(r, (value['actions'], 0) if build_index == 0 else (0, value['actions']))
            build_index += 1
        if command != ('build' if engine == 'native' else 'cquery'):
            continue
        paths = output_paths(r, value, engine, scratch, workspace, reference)
        entries = phase['outputs'][output_index]
        if (list(map(str, paths)) != [e['path'] for e in entries] or len(entries) != len(value['expected'])
                or {Path(e['path']).name for e in entries} != set(value['expected'])):
            raise ValueError('output evidence not tied to exact CLI result')
        for e in entries:
            if e['kind'] != 'file' or e['mode'] != 0o555 or not valid_evidence(e, value['expected'][Path(e['path']).name], True):
                raise ValueError('consumer bytes or executable mode mismatch')
        output_index += 1
    if value['diagnostic']:
        if phase['outputs']:
            raise ValueError('rejected phase published outputs')
    elif len(phase['outputs']) != 2 or phase['outputs'][0] != phase['outputs'][1]:
        raise ValueError('missing or inconsistent cold/warm output evidence')


def validate_check(check, report, args, workspace, reference):
    item = next(c for c in CASEPLAN if c['name'] == check['name'])
    expected = phases_for(item, args.reference_only)
    if (check['kind'] != item['kind'] or [(p['probe'], p['engine']) for p in check['phases']] != expected
            or check.get('error') or check['status'] != ('PASS' if expected else 'SKIP')):
        raise ValueError('missing, reordered, failed or unknown phases')
    for phase in check['phases']:
        validate_phase(phase, report, args, workspace, reference, item)


def argument_parser():
    parser = common.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / ('artifacts/'+SUITE_NAME+'-validation.json'))
    parser.add_argument('--reference-source', type=Path, default=Path('/tmp/opencode/bazel-9.3-reference'))
    return parser


def regression_test_body(raw, name):
    text = raw.decode()
    start = text.index('    #[test]\n    fn '+name+'()')
    end = text.index('\n    }', start)+len('\n    }')
    return text[start:end].encode()


def validate_known_gap_sources(sources, check_baseline=False):
    """Replay retained observations from bytes, not a waived negative diagnostic.

    Offline replay needs no git or live files. Real runs also verify the old git
    blobs; current files and the tracked excerpt are snapshotted before/after.
    The original full receipt is identified by hash, never required as a live
    ignored artifact. This is not old-binary execution evidence or a full matrix.
    """
    gap = KNOWN_PARITY_GAPS[0]
    saved = sources[str(GAP_RECEIPT)]
    if saved['sha256'] != gap['evidence_excerpt']['sha256'] or not valid_evidence(saved):
        raise ValueError('missing or altered retained differential excerpt')
    original = json.loads(base64.b64decode(saved['bytes_base64'], validate=True))
    if (original['kind'] != 'retained_differential_excerpt'
            or original['source_receipt'] != gap['retained_receipt']
            or set(original['runs']) != {'142', '143'} or len(original['checks']) != 1):
        raise ValueError('excerpt origin or observation scope changed')
    if original['status'] != 'FAIL':
        raise ValueError('original differential mismatch must remain FAIL')
    check = next(c for c in original['checks'] if c['name'] == 'module')
    if check['status'] != 'FAIL' or len(check['phases']) != 2:
        raise ValueError('original module family must remain FAIL')
    for engine, expected in gap['observations'].items():
        phase = next(p for p in check['phases'] if (p['probe'], p['engine']) == ('module-global', engine))
        if (digest(json.dumps(phase, sort_keys=True).encode()) != expected['full_phase_sha256']
                or phase['status'] != expected['phase_status'] or phase['stage'] != 'loading'
                or phase['runs'] != [expected['run_id']]):
            raise ValueError('retained MODULE mismatch phase changed')
        r = original['runs'][str(expected['run_id'])]
        if (r['name'] != 'module.module-global.'+engine+'.0' or r['argv'][-1] != '//app:check'
                or 'query' not in r['argv'] or any(r[k] != expected[k] for k in
                    ('returncode', 'stdout', 'stdout_sha256', 'stderr_sha256', 'executable_sha256'))):
            raise ValueError('retained mismatch command or exact observation changed')
        for field in ('stdout', 'stderr'):
            raw = base64.b64decode(r[field+'_base64'], validate=True)
            if digest(raw) != expected[field+'_sha256'] or raw.decode() != r[field]:
                raise ValueError('retained mismatch stream bytes changed')
        path = str(Path(original['scratch_directory']) / 'workspace/MODULE.bazel')
        for field in ('inputs_before', 'inputs_after'):
            e = phase[field][path]
            if e['sha256'] != gap['module']['sha256'] or not valid_evidence(e, gap['module']['text'].encode()):
                raise ValueError('retained mismatch MODULE source changed')
    baseline = {}
    if check_baseline:
        for path in dict.fromkeys([*BASELINE_SOURCES, *(t['path'] for t in BASELINE_TESTS)]):
            baseline[path] = subprocess.run(['git', '-C', str(ROOT), 'show', BASELINE_COMMIT+':'+path],
                capture_output=True, timeout=10, check=True).stdout
    for path, sha in BASELINE_SOURCES.items():
        e = sources[str(ROOT / path)]
        if not valid_evidence(e) or (check_baseline and digest(baseline[path]) != sha):
            raise ValueError('invalid current source evidence or historical git blob mismatch: '+path)
    for test in BASELINE_TESTS:
        raw = base64.b64decode(sources[str(ROOT / test['path'])]['bytes_base64'], validate=True)
        if digest(regression_test_body(raw, test['name'])) != test['sha256']:
            raise ValueError('preexisting lazy MODULE regression body changed')
        if check_baseline and digest(regression_test_body(baseline[test['path']], test['name'])) != test['sha256']:
            raise ValueError('baseline lazy MODULE regression body differs')
    return dict(commit=BASELINE_COMMIT, historical_sources={p: digest(raw) for p, raw in baseline.items()
        if p in BASELINE_SOURCES}, verified_from_git=check_baseline)


def closure_receipt(report, reference_only):
    """Bind closure to actual successful phases, never to the requested run mode."""
    phases = [p for c in report['checks'] if c['name'] == CLOSURE_PROBE['family']
        for p in c['phases'] if p['probe'] == CLOSURE_PROBE['probe']]
    runs = {p['engine']: p['runs'] for p in phases}
    verified = (not reference_only and set(runs) == {'native', 'reference'}
        and all(p['status'] == 'PASS' and not p.get('error') for p in phases)
        and all(len(ids) == 1 and report['runs'][ids[0]]['returncode'] == CLOSURE_PROBE['returncodes'][engine]
            for engine, ids in runs.items()))
    return dict(probe=CLOSURE_PROBE, runs=runs, native_closure_verified=verified,
        status='RESOLVED_BY_ROOT_RECORDING_VALIDATION' if verified else
            'REFERENCE_ONLY_EXPECTED_CLOSURE' if reference_only else 'UNVERIFIED')


OFFICIAL_FILES = {
    'analysis/RunEnvironmentInfo.java': 'f75b6abb6fa2f17ad3c52fd6a763bc8457407b86465c58e449090827f2bd8d44',
    'starlarkbuildapi/RunEnvironmentInfoApi.java': 'ab0bc3d809e42d023aa186bbce38a8927f0d7c93425aaaa3a9f9dd970ce3dfe6',
    'packages/NativeInfo.java': '70623159df82d40dadf5a0fa040cb3b2511c71706da8a518864ccc312ff05db7',
    'packages/BuiltinProvider.java': '4551cf08d71dd305a14546f88f184dcef30f3c5882242899a83f865b531ecd93',
    'analysis/starlark/StarlarkRuleConfiguredTargetUtil.java': 'fbb2c4e8bf0b1fb49ba63f8a1b5f352c1c0ffbd71373c7d6dca3108c0785a1b6',
    'analysis/starlark/StarlarkGlobalsImpl.java': '00e6442ea93774598da9ae84fad534e7b9b0f7bdd5d61910e0b4576ba75ffc6b',
    'analysis/starlark/StarlarkAttrModule.java': '388421c44c623c1c6625fd9f2b059d2a7d1e13b8d45e7c96173f24866a917967',
    'analysis/configuredtargets/AbstractConfiguredTarget.java': '6ec77df09263c0e18d3443dd5911180156c43df4a5df2c2fc42aab01307fbbfc',
    '../../../../../net/starlark/java/eval/Starlark.java': 'cbc71970bef037ed0c8c56171d45582201fad2471b17f2944ffb131574070aa6',
}


def reference_source_evidence(path):
    result = subprocess.run(['git', '-C', str(path), 'rev-parse', 'HEAD'], capture_output=True, timeout=10, check=True)
    if result.stdout.decode().strip() != SOURCE_PIN:
        raise ValueError('wrong pinned official source revision')
    files = {name: digest((path / 'src/main/java/com/google/devtools/build/lib' / name).read_bytes()) for name in OFFICIAL_FILES}
    if files != OFFICIAL_FILES:
        raise ValueError('modified pinned official source')
    return dict(path=str(path), commit=SOURCE_PIN, files=files)


class RecordingRunner(utility.RecordingRunner):
    def __init__(self, report, args):
        super().__init__(report, args.timeout)
        self.args = args
        self.env.update(HOST)

    def run(self, name, argv, cwd, timeout=None):
        binaries = common.executable_evidence(self.args)
        source = reference_source_evidence(self.args.reference_source)
        if binaries != self.report['executables'] or source != self.report['official_source_before']:
            raise ValueError('pre-command executable/source binding changed')
        r = super().run(name, argv, cwd, timeout)
        r['preflight_binding_sha256'] = digest(json.dumps(dict(executables=binaries, source=source), sort_keys=True).encode())
        r['synthetic_environment'] = HOST.copy()
        return r


def shutdown(reference, report):
    path = reference.output_base / 'server/server.pid.txt'
    pid = int(path.read_text().strip()) if path.exists() else None
    if pid is not None and pid <= 1:
        raise ValueError('invalid private reference PID')
    r = reference.runner.run(SUITE_NAME+'.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS),
        reference.workspace, timeout=min(reference.runner.timeout, 15))
    success(r)
    remaining = sorted(str(p) for p in reference.output_base.rglob('server.pid.txt'))
    alive = pid is not None and Path('/proc', str(pid)).exists()
    report['server_receipt'] = dict(pidfile=str(path), pid=pid, proc_exists_after=alive, remaining_pidfiles=remaining)
    if alive or remaining:
        raise ValueError('private reference server remains after shutdown')
    report['reference_shutdown_succeeded'] = True


def report_passes(report):
    try:
        args = argument_parser().parse_args([])
        if set(report['configuration']) != set(vars(args)):
            return False
        for key, value in report['configuration'].items():
            setattr(args, key, Path(value) if key in ('native', 'reference', 'output', 'scratch_parent', 'reference_source') else value)
        expected_status = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS'
        engines = ('reference',) if args.reference_only else ('reference', 'native')
        started, finished = (datetime.datetime.fromisoformat(report[key]) for key in ('started_at', 'finished_at'))
        if started.tzinfo is None or finished.tzinfo is None or finished < started:
            return False
        if (report['schema_version'] != 1 or report['status'] not in ('FAIL', expected_status) or report['planned_checks'] != CASEPLAN
                or json.dumps(report['historical_parity_gaps'], sort_keys=True) != json.dumps(KNOWN_PARITY_GAPS, sort_keys=True)
                or json.dumps(report['gap_closure'], sort_keys=True) != json.dumps(closure_receipt(report, args.reference_only), sort_keys=True)
                or (not args.reference_only and report['gap_closure']['native_closure_verified'] is not True)
                or report['historical_baseline_verification'] != dict(commit=BASELINE_COMMIT,
                    historical_sources=BASELINE_SOURCES, verified_from_git=True)
                or report['plan'] != plan_receipt(args.reference_only) or report['limitations'] != LIMITATIONS
                or [(c['name'], c['kind']) for c in report['checks']] != PLAN
                or report['errors'] or report['cleanup'] or not report['scratch_cleaned'] or not report['reference_shutdown_succeeded']
                or report['sources_before'] != report['sources_after'] or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(p == e['path'] and e['kind'] == 'file' and valid_evidence(e) for p, e in report['sources_before'].items())
                or report['executables'] != report['executables_after'] or set(report['executables']) != set(engines)
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256 or report['reference_release'] != 'bazel 9.3.0'
                or report['official_source_before'] != report['official_source_after']
                or report['official_source_before'] != dict(path=str(args.reference_source), commit=SOURCE_PIN, files=OFFICIAL_FILES)):
            return False
        validate_known_gap_sources(report['sources_before'])
        if not args.reference_only and (not re.fullmatch('[0-9a-f]{64}', args.native_sha256 or '')
                or report['executables']['native']['sha256'] != args.native_sha256):
            return False
        if (report['completed_phases'] != report['plan']['phases']
                or report['completed_checks'] != sum(bool(phases_for(c, args.reference_only)) for c in CASEPLAN)):
            return False
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        reference = DirectReference(args.reference, workspace, scratch, None, SUITE_NAME)
        receipt = report['server_receipt']
        if (report['cleanup_receipt'] != dict(path=str(scratch), removed=True)
                or set(receipt) != {'pidfile', 'pid', 'proc_exists_after', 'remaining_pidfiles'}
                or receipt['pidfile'] != str(reference.output_base / 'server/server.pid.txt')
                or type(receipt['pid']) is not int or receipt['pid'] <= 1
                or receipt['proc_exists_after'] is not False or receipt['remaining_pidfiles'] != []):
            return False
        registry = {str(scratch / 'registry' / Path(p).relative_to(REGISTRY)): dict(e,
            path=str(scratch / 'registry' / Path(p).relative_to(REGISTRY)))
            for p, e in report['sources_before'].items() if Path(p).is_relative_to(REGISTRY)}
        if report['registry_before'] != registry or report['registry_after'] != registry:
            return False
        runs = report['runs']
        binding = digest(json.dumps(dict(executables=report['executables'], source=report['official_source_before']), sort_keys=True).encode())
        if len(runs) != report['plan']['commands'] or [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for r in runs:
            engine = next((e for e in engines if str(getattr(args, e)) == r['argv'][0]), None)
            if (engine is None or r['executable_sha256'] != report['executables'][engine]['sha256']
                    or r['preflight_binding_sha256'] != binding or 'environment' in r or 'environment_keys' in r
                    or r['synthetic_environment'] != HOST):
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(r[field+'_base64'], validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != r[field]):
                    return False
        for r, name, argv, timeout in (
                (runs[0], 'reference.version', reference.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], SUITE_NAME+'.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS), min(args.timeout, 15))):
            if r['name'] != name or r['argv'] != argv or r['cwd'] != str(workspace) or r['timeout_seconds'] != timeout:
                return False
            success(r)
        if runs[0]['stdout'].strip() != 'bazel 9.3.0':
            return False
        used = [0]
        for check in report['checks']:
            validate_check(check, report, args, workspace, reference)
            used.extend(rid for phase in check['phases'] for rid in phase['runs'])
        return used+[len(runs)-1] == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def main(argv=None):
    args = argument_parser().parse_args(argv)
    if args.reference_only and args.output == ROOT / ('artifacts/'+SUITE_NAME+'-validation.json'):
        args.output = ROOT / ('artifacts/'+SUITE_NAME+'-reference.json')
    for field in ('native', 'reference', 'output', 'scratch_parent', 'reference_source'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if (args.output in [args.native, args.reference, ROOT / KNOWN_PARITY_GAPS[0]['retained_receipt']['path'], *SOURCES]
            or args.output.is_relative_to(args.reference_source)
            or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests', 'src', 'resources', '.direnv'))):
        raise ValueError('report cannot overwrite source or executable')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
        planned_checks=CASEPLAN, plan=plan_receipt(args.reference_only), limitations=LIMITATIONS,
        historical_parity_gaps=KNOWN_PARITY_GAPS,
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        scratch_cleaned=False, reference_shutdown_succeeded=False,
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
    runner = RecordingRunner(report, args)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['historical_baseline_verification'] = validate_known_gap_sources(report['sources_before'], check_baseline=True)
        report['executables'] = common.executable_evidence(args)
        report['official_source_before'] = reference_source_evidence(args.reference_source)
        scratch = Path(tempfile.mkdtemp(prefix=SUITE_NAME+'-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(MODULE)
        shutil.copytree(REGISTRY, scratch / 'registry')
        report['registry_before'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        reference = DirectReference(args.reference, workspace, scratch, runner, SUITE_NAME)
        report['reference_release'] = success(runner.run('reference.version', reference.startup+['version', '--gnu_format'], workspace)).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('wrong genuine reference release')
        for item in CASEPLAN:
            check = dict(name=item['name'], kind=item['kind'], phases=[], status='FAIL')
            report['checks'].append(check)
            for probe, engine in phases_for(item, args.reference_only):
                check['phases'].append(run_phase(args, runner, scratch, workspace, reference, item, probe, engine))
            check['status'] = 'PASS' if check['phases'] else 'SKIP'
            try:
                validate_check(check, report, args, workspace, reference)
            except Exception as exc:
                check.update(status='FAIL', error=str(exc))
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        if reference is not None:
            try:
                shutdown(reference, report)
            except Exception as exc:
                report['errors'].append('shutdown: '+str(exc))
        if scratch is not None:
            try:
                report['registry_after'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
                if reference is None or report['reference_shutdown_succeeded']:
                    shutil.rmtree(scratch)
                report['scratch_cleaned'] = not scratch.exists()
                report['cleanup_receipt'] = dict(path=str(scratch), removed=report['scratch_cleaned'])
            except OSError as exc:
                report['errors'].append('cleanup: '+str(exc))
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['executables_after'] = common.executable_evidence(args)
            report['official_source_after'] = reference_source_evidence(args.reference_source)
        except Exception as exc:
            report['errors'].append('final evidence: '+str(exc))
        report['completed_phases'] = sum(p['status'] == 'PASS' for c in report['checks'] for p in c['phases'])
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['gap_closure'] = closure_receipt(report, args.reference_only)
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS'
        utility.write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return 130 if interrupted else 0 if report['status'] in ('PASS', 'REFERENCE_ONLY_PASS') else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
