#!/usr/bin/env python3
"""Bounded JSON execution matrix, not general Bazel compatibility.

Portable goldens are tracked data, never ignored research-report dependencies.
The subrule harness supplies the established receipt/runner/cleanup machinery in
an isolated module instance: overriding hooks cannot change its own test suite.
JSON-specific phases, diagnostics, action counts and outputs are checked here.
"""
import base64
import importlib.util
import json
from pathlib import Path
import re
import signal
import struct
import sys

import validate_subrules as infrastructure

_spec = importlib.util.spec_from_file_location('_json_receipt_engine', infrastructure.__file__)
engine = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(engine)
shared, common, utility, direct = engine.shared, engine.common, engine.utility, engine.direct
ROOT, REGISTRY, SOURCE_PIN = engine.ROOT, engine.REGISTRY, engine.SOURCE_PIN
REFERENCE_SHA256, PLATFORM_SHA = engine.REFERENCE_SHA256, engine.PLATFORM_SHA
digest, evidence, snapshot = engine.digest, engine.evidence, engine.snapshot
DirectReference, COMMON_FLAGS = engine.DirectReference, engine.COMMON_FLAGS
valid_evidence, success = engine.valid_evidence, engine.success
SUITE = 'json'
OLD_NATIVE = 'a9879eebe5703cdc534dbc9eced9fac3f79a75426b38e7c50140efa555ea773a'
FIRST_ATTEMPT = ROOT / 'artifacts/json-validation-first-attempt.json'
# Immutable failed frozen execution: 37 reference PASS, 24 native PASS, 21 native
# FAIL, 124/127 commands; receipt remains evidence, never a runtime dependency.
FIRST_ATTEMPT_SHA256 = '9a76d8166e5ab39129ee3ffa63a44da1238e2c9fc1510c827d021a1f5a0bce6b'
FIXTURES = ROOT / 'tests/fixtures/json'
FIXTURE_PINS = {
    'float': '52eac2a98f5578d55fe2c6fd5c0e7a2eaf3a0be31d38710a7693220a77bf99dc',
    # N41 changed only native_profile (generated_root k8-fastbuild after D1;
    # the mechanical extraction's pin was e48d82e1...269c15e).
    'file': '3ad5ed1b9006ecf9af6d2201c178db642570bab957eae5c4b7a38ebc0d83e394',
    'functional': '0f943c2a6dc3d3286ad35dedbcf54e0da9f1c769dbe91bac8189a7d2c0cab5e0',
}
ORIGINS = {
    'artifacts/json-contract.json': 'cc8d00ea849464637d3b9a4e8048d33801f7a2da5ea72496923f4eef090b1421',
    'artifacts/json-edge-contract.json': '935869268974885578816a5bd5a449d5fd6317b4d772f5e5428bd5649383cf13',
    'artifacts/json-float-algorithm-contract.json': '082b9ed1c79f040f681041102f5d2c7f031f5def0e11c68cf9d5f6111c9a7f89',
    'artifacts/json-closest-even-prototype.json': 'bb85b1b2dac11805a2d289d0a85b907085a5a0c48e3324d607da7f5e9690da7f',
}
OFFICIAL_FILES = {
    'src/main/java/net/starlark/java/lib/json/Json.java': 'c130342af69ef9b80ee2d4c65fdbd7a985ffacaa5876ae02090c97d651d7b17b',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkGlobalsImpl.java': '00e6442ea93774598da9ae84fad534e7b9b0f7bdd5d61910e0b4576ba75ffc6b',
    'src/main/java/com/google/devtools/build/lib/actions/Artifact.java': '03a69adfee1b171bcdf1ded4458d265386130264fc6b15e27db2fa170dd1bb52',
}
LIMITATIONS = [
    'PASS_ONLY_BOUNDED_PROFILE; reference-only never executes native or closes native acceptance.',
    'All paired matrix probes are CURRENT_EXECUTION, including 1024 float rows. Expected float strings/bits are BOUND_HISTORICAL_GOLDEN, not new measurements invented from stale output inventories.',
    'Historical vendor JSON misbehavior and heuristic failure 2/1024 remain failures. Closest-even prototype 23440 properties are NATIVE_ONLY/math evidence, NOT reference passes.',
    'Only BUILD and shared ordinary/MODULE-loaded bzl have json. Direct MODULE.json is a real negative preflight.',
    'Corrected run38 nineteen action outputs are bound exactly. Named derivative replaces unsupported BUILD print with a tuple assignment evaluating the same JSON expressions; original tracked fixture remains unchanged. Source edit changes only producer string attr; only dependency.json output/action changes (native 1 execution, 18 hits).',
    'File main source/generated profile only. Exact native and reference k8-fastbuild triples (native wrote rust-fastbuild before N41 closed D1) are checked independently, NOT byte parity via path normalization.',
    'Builtin NativeInfo and external File are EXPLICIT_OUT_OF_PROFILE native boundaries, not reference error claims and not empty-object encoding.',
    'Native depth 64 remains shared. Decode alone uses 256 MiB weighted/250000 nodes and an 8 MiB effective UTF-8 input ceiling. Encode/Indent/EncodeIndent retain 64 MiB weighted/100000 nodes, one cumulative budget per invocation including all helper stages. HostBudget Default and non-JSON profiles are unchanged; decoded values need not fit encoding. These are safety bounds, not JVM thresholds or RSS guarantees. Rust tests independently cover capacity/adversarial/producer-consumer behavior, preexisting BigInt, preclone checks, fake NativeSlot types, DAG/quota/internal errors; this CLI matrix does not replace them.',
    'Byte-string default UTF8 key order only; nondefault UTF16 semantics excluded. Indentation preserves harmless observed quirks, not strict JSON validation.',
    'Diagnostic categories/offsets use separate native and reference messages. Native resource text is bound to the current candidate source, pending frozen CLI execution; other exact projections retain observed messages. Native unsupported values omit reference type names and nested key/index context; NO rich diagnostic-text/context parity claim. Bound fixture inputs and callback markers establish the actual rejected types.',
    'All execution receipts require unchanged start/end and pre-command production/binary/source/registry pins; concurrent edits invalidate preparation acceptance.',
    'Earlier JSON acceptance receipts retain their historical capacity profile and are not execution evidence for this candidate.',
]
KNOWN_REFERENCE_DEFECTS = []  # Harmless formatter quirks are ordinary tested results, not crash waivers.
DEFECTS = ()
CASEPLAN = [
    common.case('api', ('metadata', 'build', 'module', 'native-field', 'unhashable', 'immutable',
        'encode-named', 'decode-named', 'indent-named', 'encode-indent-named', 'indent-positional', 'encode-indent-positional')),
    common.case('decode', ('identity-mutable-types', 'frozen', 'type-default', 'arity-default', 'offset6', 'offset8', 'offset10')),
    common.case('encode', ('utf8-order-aliases', 'int-key', 'bool-key', 'nan', 'inf', 'overflow', 'label', 'module', 'callable', 'nested')),
    common.case('indent', ('lexemes', 'numeric-eof', 'encode-numeric-eof', 'option-type')),
    common.case('floats', ('golden1024',)),
    common.case('functional', ('provider19',)),
    common.case('module-bzl', ('repository-callback',)),
    common.case('profile', ('main-files', 'depth64', 'depth65-default', 'depth65', 'cycle', 'range-quota', 'default-quota', 'native-info', 'external-file')),
]
NATIVE_ONLY = {'depth64', 'depth65-default', 'depth65', 'cycle', 'range-quota', 'default-quota', 'native-info', 'external-file'}
# Candidate policy metadata, not an execution/approval claim or a Bazel contract.
# No effective-input ceiling is imposed separately on encoding/formatting.
NATIVE_CAPACITY = {
    method: dict(weighted_bytes=(256 if method == 'decode' else 64) * 1024 * 1024,
        nodes=250000 if method == 'decode' else 100000, max_open_containers=64,
        budget_scope='per_invocation_shared_cumulative',
        **({'effective_utf8_input_bytes': 8 * 1024 * 1024} if method == 'decode' else {}))
    for method in ('decode', 'encode', 'indent', 'encode_indent')
}


def source_paths():
    return sorted(set([Path(__file__).resolve(), ROOT / 'tests/test_validate_json.py',
        ROOT / 'Cargo.toml', ROOT / 'Cargo.lock', ROOT / 'vendor/starlark-0.14.2.provenance.json',
        ROOT / 'tests/fixtures/builtin_modules/provenance.json',
        *ROOT.glob('scripts/*.py'), *ROOT.glob('src/**/*.rs'), *ROOT.glob('tests/*.rs'),
        *(p for p in FIXTURES.rglob('*') if p.is_file()),
        *ROOT.glob('vendor/starlark-0.14.2/**/*.rs'), *ROOT.glob('vendor/starlark-0.14.2/**/Cargo.toml'),
        *(p for p in REGISTRY.rglob('*') if p.is_file()),
        *(p for p in (ROOT / 'resources').rglob('*') if p.is_file())]))


def portable(name):
    raw = (FIXTURES / (name+'_reference.json')).read_bytes()
    if digest(raw) != FIXTURE_PINS[name]:
        raise ValueError('portable golden differs from mechanical extraction pin')
    return json.loads(raw)


def float_rows():
    corpus = portable('float')
    rows = corpus['rows']
    if corpus['count'] != 1024 or len(rows) != 1024 or len({r['bits_hex'] for r in rows}) != 1024:
        raise ValueError('incomplete float corpus')
    if corpus['provenance']['source_sha256']['artifacts/json-float-algorithm-contract.json'] != ORIGINS['artifacts/json-float-algorithm-contract.json']:
        raise ValueError('unbound historical float origin')
    for r in rows:
        if set(r) != {'bits_hex', 'input_decimal', 'expected_json'} or not re.fullmatch('[0-9a-f]{16}', r['bits_hex']):
            raise ValueError('invalid float row')
        for token in (r['input_decimal'], r['expected_json']):
            if struct.pack('>d', float(token)).hex() != r['bits_hex']:
                raise ValueError('decimal does not recover exact binary64 bits')
        if not any(c in r['expected_json'] for c in '.e'):
            raise ValueError('lost float lexical type')
    return rows


def float_fixture():
    rows = float_rows()
    values = [(r['bits_hex'], r['input_decimal'], r['input_decimal'] if any(c in r['input_decimal'].lower() for c in '.e') else r['input_decimal']+'.0') for r in rows]
    body = 'lines=[]\nfor bits,decimal,lexeme in '+repr(values)+':\n'
    body += '    a=float(decimal)\n    b=json.decode(lexeme)\n    lines.append(bits+"\\t"+type(a)+"\\t"+type(b)+"\\t"+json.encode(a)+"\\t"+json.encode(b))\n'
    return body, ''.join(r['bits_hex']+'\tfloat\tfloat\t'+r['expected_json']+'\t'+r['expected_json']+'\n' for r in rows).encode()


def result(expression='"ok\\n"'):
    return 'out=ctx.actions.declare_file("consume.out")\nctx.actions.write(out,'+expression+')\nprint("JSON_ACTION_READY")\nreturn [DefaultInfo(files=depset([out]))]\n'


METADATA = '''load(":base.bzl", "J", "METHOD", "ALIAS")
def metadata():
    saved=json.encode
    return [type(json),repr(json),str(json),dir(json),J==json,type(saved),repr(saved),repr(json.decode),repr(json.indent),repr(json.encode_indent),saved==saved,saved==json.encode,json.encode==json.encode,METHOD==ALIAS]
'''
META_EXPECTED = json.dumps(['json', '<unknown object net.starlark.java.lib.json.Json>', '<unknown object net.starlark.java.lib.json.Json>',
    ['decode','encode','encode_indent','indent'], True, 'builtin_function_or_method',
    '<built-in method encode of json value>', '<built-in method decode of json value>',
    '<built-in method indent of json value>', '<built-in method encode_indent of json value>', True, False, False, True], separators=(',', ':')).encode()
IDENTITY = '''fallback=[]
got=json.decode("!",fallback)
got.append("identity")
if fallback != ["identity"]:
    fail("DEFAULT_WRONG_IDENTITY")
print("JSON_DEFAULT_IDENTITY")
d=json.decode('{"z":1,"a":2,"z":3,"n":[[]]}')
d["n"][0].append(1)
json.encode(d)
json.encode_indent(d)
d["n"].append(2)
fresh=json.decode("[]")
other=json.decode("[]")
fresh.append(3)
values=[json.decode(s) for s in ["-0","0","1.0","1e0","-0.0","123456789123456789123456789"]]
payload=[fallback,json.decode("null",42),json.decode("!",None),d.keys(),d,fresh,other,[type(x) for x in values],values,type(json.decode("1e999")),str(json.decode("1e999")),str(json.decode("-1e999")),json.decode("5.E1"),json.decode("5.e1","INVALID")]
'''
IDENTITY_EXPECTED = b'[["identity"],null,null,["z","a","n"],{"a":2,"n":[[1],2],"z":3},[3],[],["int","int","float","float","float","int"],[0,0,1.0,1.0,-0.0,123456789123456789123456789],"float","+inf","-inf",50.0,"INVALID"]'
REPO_BUILD = b'exports_files(["data"],visibility=["//visibility:public"])\n'
REPO_DATA = b'{"a":[1],"callback":true,"z":0}'
REPOSITORY = '''def _repo(ctx):
    print("JSON_REPOSITORY_CALLBACK")
    data=json.decode('{"z":0,"a":[]}')
    data["a"].append(1)
    data["callback"]=True
    ctx.file("data",json.encode(data),executable=False)
    ctx.file("BUILD.bazel",'exports_files(["data"],visibility=["//visibility:public"])\\n',executable=False)
repo=repository_rule(implementation=_repo)
def _ext(ctx):
    print("JSON_MODULE_BZL")
    repo(name="made")
ext=module_extension(implementation=_ext)
'''


def fixture(family, probe):
    if not any(c['name'] == family and probe in c['probes'] for c in CASEPLAN):
        raise ValueError('unknown planned JSON probe')
    files = {'MODULE.bazel': 'module(name="json_j")\n', 'BUILD.bazel': 'load(":defs.bzl","r")\nr(name="consume")\n'}
    v = dict(target='//:consume', expected={'consume.out': b'ok\n'}, actions=1, diagnostic=None,
        changed=None, markers=['JSON_CALLBACK','JSON_ACTION_READY'], stage='analysis', native_only=probe in NATIVE_ONLY and family=='profile',
        defect=False, spawns=[], origin='CURRENT_JSON_NAMED_FIXTURE')
    prefix, body, attrs = '', result(), ''
    expressions = {
        'encode-named':'json.encode(x=1)', 'decode-named':'json.decode(x="null")',
        'indent-named':'json.indent(s="[]")', 'encode-indent-named':'json.encode_indent(x=[])',
        'indent-positional':'json.indent("[]"," ")', 'encode-indent-positional':'json.encode_indent([]," ")',
        'type-default':'json.decode(1,default=[])', 'arity-default':'json.decode("{}",None,0)',
        'int-key':'json.encode({1:"x"})', 'bool-key':'json.encode({True:1})',
        'nan':'json.encode(float("nan"))', 'inf':'json.encode(float("inf"))', 'overflow':'json.encode(json.decode("1e999"))',
        'label':'json.encode(Label("//:consume"))', 'module':'json.encode(json)', 'callable':'json.encode(json.encode)',
        'nested':'json.encode({"a":[Label("//:consume")]})', 'numeric-eof':'json.indent("1")',
        'encode-numeric-eof':'json.encode_indent(1)', 'option-type':'json.indent("[]",indent=None)',
        'native-info':'json.encode(DefaultInfo())', 'range-quota':'json.encode(range(100001))',
        # Root + 250000 nulls exceeds Decode's node ceiling, below its byte/input
        # ceilings (Rust boundary proof). Numeric tokens can hit bytes first.
        'default-quota':'json.decode("["+"null,"*249999+"null]",default=[])',
        'depth65':'json.decode("["*65+"0"+"]"*65)',
    }
    if family == 'api' and probe in ('metadata','build'):
        files['base.bzl'] = 'J=json\nMETHOD=json.encode\nALIAS=METHOD\n'
        prefix = METADATA
        body = result('json.encode(metadata())')
        v['expected'] = {'consume.out': META_EXPECTED}
        if probe == 'build':
            files['BUILD.bazel'] = ('load(":defs.bzl","r")\n'
                'VALUE=json.encode([type(json),json.encode([True,None])])\n'
                'r(name="consume",build_value=VALUE)\n')
            attrs = ',attrs={"build_value":attr.string()}'
            body = ('if ctx.attr.build_value != \'["json","[true,null]"]\':\n'
                '    fail("BAD_BUILD_JSON_VALUE")\n')+body
    elif family == 'api' and probe == 'module':
        files['MODULE.bazel'] += 'X=json\n'
        v.update(diagnostic='module-global',stage='loading')
    elif family == 'api' and probe in ('native-field','unhashable','immutable'):
        prefix = {'native-field':'X=native.json\n','unhashable':'X={json:1}\n','immutable':'json.encode=1\n'}[probe]
        v.update(diagnostic=probe,stage='loading')
    elif family == 'decode' and probe == 'identity-mutable-types':
        body = IDENTITY+result('json.encode(payload)')
        v['expected'] = {'consume.out': IDENTITY_EXPECTED}
        v['markers'].append('JSON_DEFAULT_IDENTITY')
    elif family == 'decode' and probe == 'frozen':
        files['base.bzl'] = 'D=json.decode(\'{"x":[[]]}\')\n'
        prefix = 'load(":base.bzl","D")\n'
        body = 'D["x"][0].append(1)\n'+result()
        v['diagnostic'] = 'frozen'
    elif family == 'decode' and probe.startswith('offset'):
        token = {'offset6':'["é",]', 'offset8':'["😀",]', 'offset10':'["\\u00e9",]'}[probe]
        body = 'json.decode('+repr(token)+')\n'+result()
        v['diagnostic'] = probe
    elif family == 'encode' and probe == 'utf8-order-aliases':
        # Raw Unicode, not Starlark-unsupported Python \u escapes.
        body = 'a=[1]\nd={"😀":5,"\ue000":4,"é":3,"A":2,"\\001":1}\n'+result('json.encode([d,[a,a],set([3,1,2]),range(3),struct(z=(1,2),a=[])])')
        v['expected'] = {'consume.out': '[{"\\u0001":1,"A":2,"é":3,"\ue000":4,"😀":5},[[1],[1]],[3,1,2],[0,1,2],{"a":[],"z":[1,2]}]'.encode()}
    elif family == 'indent' and probe == 'lexemes':
        body = 's=\'{"z":1e+02,"z":2,"a":"\\\\u0061"}\'\n'
        body += result('json.encode([json.indent(s,indent=" "),json.indent("[}"),json.indent("null trailing"),json.indent("1 "),json.indent("[0]",prefix="P!",indent="i?")])')
        v['expected'] = {'consume.out': json.dumps(['{\n "z": 1e+02,\n "z": 2,\n "a": "\\u0061"\n}', '[}', 'null', '1', '[\nP!i?0\nP!]'],separators=(',',':')).encode()}
    elif family == 'floats':
        body, raw = float_fixture()
        body += result('"\\n".join(lines)+"\\n"')
        v.update(expected={'consume.out':raw},origin='BOUND_HISTORICAL_1024_BITS_STRINGS_PLUS_CURRENT_EXECUTION')
    elif family == 'functional':
        saved = portable('functional')
        if saved['provenance']['run_index'] != 38 or len(saved['outputs']) != 19:
            raise ValueError('only corrected run38 nineteen outputs are authoritative')
        files = saved['files'].copy()
        original_print = 'print("BUILD_META",type(json),dir(json),json.encode(json.decode("[true,null]")))'
        if files['BUILD.bazel'].count(original_print) != 1:
            raise ValueError('run38 BUILD print derivative source differs')
        files['BUILD.bazel'] = files['BUILD.bazel'].replace(original_print,
            'BUILD_META=(type(json),dir(json),json.encode(json.decode("[true,null]")))')
        files['defs.bzl'] = files['defs.bzl'].replace('def _producer(ctx):\n','def _producer(ctx):\n    print("JSON_PRODUCER_DECODE")\n').replace('def _consumer(ctx):\n','def _consumer(ctx):\n    print("JSON_CONSUMER_ENCODE")\n')
        expected = {n:b.encode() for n,b in saved['outputs'].items()}
        changed = expected.copy()
        changed['dependency.json'] = b'{"a":{"source":"dependency-edited"},"z":[1,2]}'
        v.update(target='//:check',expected=expected,actions=19,markers=['JSON_PRODUCER_DECODE','JSON_CONSUMER_ENCODE'],
            changed=('BUILD.bazel',files['BUILD.bazel'].replace('dependency','dependency-edited').encode(),changed),origin='CORRECTED_RUN38_MARKERS_AND_BUILD_TUPLE_DERIVATIVE')
    elif family == 'module-bzl' or family == 'profile' and probe == 'external-file':
        files['MODULE.bazel'] += 'e=use_extension("//:ext.bzl","ext")\nuse_repo(e,"made")\n'
        files['ext.bzl'] = REPOSITORY
        files['BUILD.bazel'] = 'load(":defs.bzl","r")\nr(name="consume",src="@made//:data")\n'
        attrs = ',attrs={"src":attr.label(allow_single_file=True)}'
        if probe == 'external-file':
            body = 'json.encode(ctx.file.src)\n'+result()
            v['diagnostic'] = probe
        else:
            body = 'out=ctx.actions.declare_file("consume.out")\nctx.actions.run_shell(inputs=[ctx.file.src],outputs=[out],arguments=[ctx.file.src.path,out.path],command=\'cat "$1" > "$2"\',mnemonic="JJsonConsumer")\nprint("JSON_ACTION_READY")\nreturn [DefaultInfo(files=depset([out]))]\n'
            v.update(expected={'consume.out':REPO_DATA},spawns=['JJsonConsumer'])
    elif family == 'profile' and probe == 'main-files':
        saved = portable('file')
        files = saved['files'].copy()
        files['defs.bzl'] = files['defs.bzl'].replace('def _impl(ctx):\n','def _impl(ctx):\n    print("JSON_CALLBACK")\n')
        v.update(target='//:check',expected={'file_encoding.json':saved['reference_output'].encode()},markers=['JSON_CALLBACK'],origin='MAIN_FILES_ENGINE_SPECIFIC_TRIPLES')
    elif family == 'profile' and probe in ('depth64','depth65-default'):
        if probe == 'depth64':
            body = 'text="["*64+"0"+"]"*64\nif json.encode(json.decode(text)) != text:\n    fail("DEPTH64_BAD")\n'+result()
        else:
            body = 'fallback=[]\ngot=json.decode("["*65+"0"+"]"*65,default=fallback)\ngot.append(1)\nif fallback != [1]:\n    fail("DEFAULT_WRONG_IDENTITY")\nprint("JSON_DEFAULT_IDENTITY")\n'+result()
            v['markers'].append('JSON_DEFAULT_IDENTITY')
    elif family == 'profile' and probe == 'cycle':
        body = 'a=[]\na.append(a)\njson.encode(a)\n'+result()
        v['diagnostic'] = 'cycle'
    elif probe in expressions:
        body = expressions[probe]+'\n'+result()
        v['diagnostic'] = probe
    else:
        raise ValueError('unimplemented JSON probe')
    if 'defs.bzl' not in files:
        files['defs.bzl'] = prefix+'def _impl(ctx):\n'+common.indent('print("JSON_CALLBACK")\n'+body)+'r=rule(implementation=_impl'+attrs+')\n'
    if v['diagnostic']:
        v.update(expected={},actions=0,spawns=[],markers=['JSON_CALLBACK'] if v['stage']=='analysis' else [])
    v['files'] = {n:b.encode() if isinstance(b,str) else b for n,b in files.items()}
    return v


DIAGNOSTICS = {
    'module-global': r"(?:name 'json' is not defined|Variable `json` not found)",
    'native-field': r"(?:no (?:attribute|field|method|native function or rule)|has no attribute)[^\n]*['\"`]json",
    'unhashable': r"unhashable type[^\n]*json|json[^\n]*not hashable",
    'immutable': r"cannot set .encode field of json|json[^\n]*(?:immutable|frozen|read.only)",
    **{p:r'(?:positional.only|extra named|unexpected named|named argument)[^\n]*|Missing positional-only parameter' for p in ('encode-named','decode-named','indent-named','encode-indent-named')},
    **{p:r'(?:positional argument|positional parameter|too many positional)' for p in ('indent-positional','encode-indent-positional','arity-default')},
    'type-default': r"(?:parameter[^\n]*x[^\n]*int[^\n]*(?:string|str)|Expected[^\n]*(?:str|string)[^\n]*int)",
    'option-type': r"(?:parameter[^\n]*indent[^\n]*NoneType[^\n]*(?:string|str)|Expected[^\n]*(?:str|string)[^\n]*NoneType)",
    'frozen': r'(?:mutate[^\n]*frozen list|immutable[^\n]*list|list[^\n]*immutable)',
    **{'offset'+str(n):r'offset '+str(n)+r', unexpected character "\]"' for n in (6,8,10)},
    'int-key': r'dict has int key, want string', 'bool-key':r'dict has bool key, want string',
    **{p:r'cannot encode non-finite float' for p in ('nan','inf','overflow')},
    **{p:r'cannot encode '+t+r' as JSON' for p,t in [('label','Label'),('module','json'),('callable','builtin_function_or_method')]},
    'nested':r'in dict key "a": at list index 0: cannot encode Label as JSON',
    **{p:r'input is not valid JSON' for p in ('numeric-eof','encode-numeric-eof')},
    **{p:r'(?:nesting depth limit exceeded|cyclic[^\n]*JSON|JSON[^\n]*cycl)' for p in ('depth65','cycle')},
    **{p:r'(?:resource|budget|quota|node limit|size limit)[^\n]*(?:exceed|limit)|(?:exceed)[^\n]*(?:budget|limit)' for p in ('range-quota','default-quota')},
    'native-info':r'(?:NativeInfo|DefaultInfo|builtin provider)[^\n]*(?:unsupported|not supported|outside)|cannot encode DefaultInfo',
    'external-file':r'(?:external|main.repository|MAIN)[^\n]*(?:File|file|JSON|unsupported)|(?:File|file)[^\n]*(?:external|main.repository|MAIN)',
}

# Exact messages from the preserved first frozen run, except the ceiling-neutral
# resource message bound to the current candidate source (not yet CLI evidence)
# and 'frozen': since N20 the vendored interpreter reports Bazel's
# Starlark.checkMutable text for the frozen decoded list
# (net/starlark/java/eval/Starlark.java:262-265; docs/STARLARK_VENDOR.md),
# replacing the first run's native "Immutable".
# No catch-all "nonzero" waiver. Reference patterns above remain unchanged.
# Fixture/phase/argument and source bindings are mandatory for this projection.
NATIVE_MESSAGES = {
    'immutable': 'Operation `.encode=` not supported on type `json`',
    'frozen': 'trying to mutate a frozen list value',
    'type-default': 'JSON argument must be a string',
    'option-type': 'JSON argument must be a string',
    **{'offset'+str(n): 'at offset '+str(n)+', unexpected character' for n in (6,8,10)},
    **{p: 'dict has non-string key, want string' for p in ('int-key','bool-key')},
    **{p: 'cannot encode unsupported value as JSON' for p in ('label','module','callable','nested')},
    **{p: 'at offset 1, unexpected end of number' for p in ('numeric-eof','encode-numeric-eof')},
    'depth65': 'JSON maximum open container depth exceeded (64)',
    **{p: 'JSON host preparation resource limit exceeded' for p in ('range-quota','default-quota')},
    'native-info': 'cannot encode native Info as JSON',
    'external-file': 'cannot encode unsupported File profile as JSON',
}


def require_rejection(r, diagnostic, which):
    codes = (48,) if diagnostic=='module-global' and which=='reference' else (1,7) if which=='reference' else (1,)
    checked = r['stderr'].replace('syntax error in MODULE.bazel file for <root>','') if diagnostic=='module-global' else r['stderr']
    pattern = (r'(?m)(?:^|: )'+re.escape(NATIVE_MESSAGES[diagnostic])+r'$'
        if which=='native' and diagnostic in NATIVE_MESSAGES else DIAGNOSTICS[diagnostic])
    if (type(r['returncode']) is not int or r['returncode'] not in codes or r['timed_out'] or r['stdout'].strip()
            or any(r.get(k) for k in ('spawn_error','interrupted','output_truncated')) or common.UNRELATED.search(checked)
            or not re.search(pattern,utility.error_lines(r['stderr']),re.I)
            or utility.marker_count(r,'JSON_ACTION_READY') or 'SUBCOMMAND:' in r['stderr']):
        raise ValueError('expected anchored JSON rejection: '+diagnostic)
    if diagnostic != 'module-global' and re.search(r"name 'json' is not defined|Variable `json` not found", r['stderr']):
        raise ValueError('old missing JSON global is not semantic rejection')


def phases_for(item, reference_only=False):
    return [(p,e) for p in item['probes'] for e in ('reference','native')
        if not (reference_only and e=='native') and not (item['name']=='profile' and p in NATIVE_ONLY and e=='reference')]


def steps_for(family, probe, which):
    v = fixture(family,probe)
    if v['diagnostic']:
        return [('reject','build',v['target'],[])]
    steps = []
    for stage in ['cold','warm']+(['changed'] if v['changed'] else []):
        steps.append((stage,'build',v['target'],[]))
        if which=='reference':
            steps.append((stage,'cquery',v['target'],['--output=files']))
    if family=='module-bzl':
        steps.append(('repository','build' if which=='native' else 'cquery','@@+ext+made//:data',[] if which=='native' else ['--output=files']))
    return steps


def expected_outputs(v, stage, which):
    if v['origin']=='MAIN_FILES_ENGINE_SPECIFIC_TRIPLES':
        # Both engines write k8-fastbuild since N41 closed D1 (docs/REMOTE_CACHE.md).
        root = 'bazel-out/k8-fastbuild/bin'
        value = [{'path':'input.txt','root':'','short_path':'input.txt'},
            {'path':root+'/file_encoding.json','root':root,'short_path':'file_encoding.json'}, [3,1,2], [0,1,2]]
        return {'file_encoding.json':json.dumps(value,separators=(',',':')).encode()}
    return v['changed'][2] if stage=='changed' else v['expected']


def check_spawns(spawns, v, stage):
    expected = [] if stage=='warm' else v['spawns']
    if [s['mnemonic'] for s in spawns] != expected:
        raise ValueError('missing/extra repository consumer execution')
    for s in spawns:
        if s['exitCode'] != 0 or s.get('cacheHit') or len(s['inputs']) != 1 or len(s['actualOutputs']) != 1:
            raise ValueError('invalid JSON consumer spawn')
        inp,out = s['inputs'][0],s['actualOutputs'][0]
        if (inp['path'] != 'external/+ext+made/data' or inp['digest']['hash'] != digest(REPO_DATA)
                or out['path'] != 'bazel-out/k8-fastbuild/bin/consume.out' or out['digest']['hash'] != digest(REPO_DATA)):
            raise ValueError('repository File/action dependency not proven')


def run_phase(args, runner, root, workspace, ref, family, probe, which):
    # Base runner already records every command/input/log and owned PID. Repository
    # files are additional actual outputs, not invented metadata about a callback.
    p = _base_run_phase(args,runner,root,workspace,ref,family,probe,which)
    if family=='module-bzl' and p['status']=='PASS':
        try:
            r = runner.report['runs'][p['runs'][-1]]
            paths = engine.output_paths(r,'@@+ext+made//:data',which,root,workspace,ref)
            p['generated'] = {'repository':[evidence(o) for o in paths], 'build':[evidence(o.parent/'BUILD.bazel') for o in paths]}
        except Exception as exc:
            p.update(status='FAIL',error=str(exc))
    return p


def validate_phase(p, report, args, workspace, ref):
    family,probe,which = p['family'],p['probe'],p['engine']
    item = next(c for c in CASEPLAN if c['name']==family)
    if (probe,which) not in phases_for(item,args.reference_only) or p['status']!='PASS' or p.get('error') or p['forbidden_outputs']:
        raise ValueError('unplanned/failed JSON phase; no reference-kind waiver')
    v = fixture(family,probe)
    root = Path(report['scratch_directory'])
    cache = root/'cases'/family/probe/which/'cache'
    if p['cache']!=str(cache):
        raise ValueError('wrong cache')
    utility.no_actions(p['actions_before'],cache)
    if any(x['exists'] for x in p['actions_before'].values()):
        raise ValueError('cold action store already exists')
    if v['diagnostic']:
        utility.no_actions(p['actions_after'],cache)
    steps = steps_for(family,probe,which)
    if len(p['runs'])!=len(steps) or len(set(p['runs']))!=len(steps) or set(p['input_snapshots'])!={str(i) for i in range(len(steps))}:
        raise ValueError('missing/duplicate phase command')
    files,outputs,logs = v['files'].copy(),set(),set()
    for i,(rid,step) in enumerate(zip(p['runs'],steps)):
        stage,cmd,target,_ = step
        if stage=='changed' and cmd=='build':
            files[v['changed'][0]]=v['changed'][1]
        shared.check_input(p['input_snapshots'][str(i)],files,workspace)
        r = report['runs'][rid]
        if (type(rid) is not int or r['id']!=rid or r['name']!=family+'.'+probe+'.'+which+'.'+str(i)
                or r['cwd']!=str(workspace) or r['argv']!=engine.command_argv(args,ref,which,cache,root,step) or r['timeout_seconds']!=args.timeout):
            raise ValueError('unbound command')
        require_rejection(r,v['diagnostic'],which) if v['diagnostic'] else success(r)
        if cmd=='build' and stage!='repository' and (i==0 or which=='native' or stage=='changed'):
            for marker in v['markers']:
                if not utility.marker_count(r,marker):
                    raise ValueError('missing actual phase marker: '+marker)
        if v['stage']=='loading' and utility.marker_count(r,'JSON_CALLBACK'):
            raise ValueError('loading error reached callback')
        if family=='module-bzl' and i==0:
            for marker in ('JSON_MODULE_BZL','JSON_REPOSITORY_CALLBACK'):
                if not utility.marker_count(r,marker):
                    raise ValueError('missing real module/repository callback')
        if cmd=='build' and which=='reference':
            logs.add(stage)
            log = p['logs'][stage]
            path = str(cache.parent/(stage+'.execution.json'))
            if not (v['diagnostic'] and log==dict(path=path,absent=True)):
                if log['path']!=path or not valid_evidence(log):
                    raise ValueError('unbound execution log')
                spawns = shared.json_objects(base64.b64decode(log['bytes_base64']))
                if v['diagnostic']:
                    if spawns: raise ValueError('actions executed on error')
                else:
                    check_spawns(spawns,v,stage)
        if v['diagnostic']:
            continue
        if cmd=='build' and which=='native':
            counts = (0,0) if stage=='repository' else (0,v['actions']) if stage=='warm' else (1,18) if family=='functional' and stage=='changed' else (v['actions'],0)
            common.action_counts(r,counts)
        if stage=='repository':
            paths = engine.output_paths(r,target,which,root,workspace,ref)
            entries = p['generated']['repository']
            if len(entries)!=1 or [str(o) for o in paths]!=[e['path'] for e in entries] or len(p['generated']['build'])!=1:
                raise ValueError('missing actual repository file')
            path = Path(entries[0]['path'])
            if which=='native':
                # Native returns its actual content-addressed repository tree,
                # not the execroot external symlink used in action arguments.
                base = cache/'repositories/direct/trees'
                parts = path.relative_to(base).parts
                valid_path = len(parts)==2 and re.fullmatch('[0-9a-f]{64}',parts[0]) and parts[1]=='data'
            else:
                valid_path = path==ref.output_base/'execroot/_main/external/+ext+made/data'
            if not valid_path or p['generated']['build'][0]['path']!=str(path.parent/'BUILD.bazel'):
                raise ValueError('repository path/canonical identity not bound')
            shared.check_file(entries[0],REPO_DATA,0o644)
            shared.check_file(p['generated']['build'][0],REPO_BUILD,0o644)
            continue
        if cmd!=('build' if which=='native' else 'cquery'):
            continue
        outputs.add(stage)
        paths = engine.output_paths(r,target,which,root,workspace,ref)
        entries = p['outputs'][stage]
        wanted = expected_outputs(v,stage,which)
        if [str(o) for o in paths]!=[e['path'] for e in entries] or len(entries)!=len(wanted) or {Path(e['path']).name for e in entries}!=set(wanted):
            raise ValueError('missing/unbound output')
        for e in entries:
            shared.check_file(e,wanted[Path(e['path']).name],0o555)
    shared.check_input(p['inputs_after'],files,workspace)
    if set(p['outputs'])!=outputs or set(p['logs'])!=logs or set(p['generated'])!=({'repository','build'} if family=='module-bzl' else set()):
        raise ValueError('missing/extra evidence')
    if 'cold' in outputs and p['outputs']['cold']!=p['outputs']['warm']:
        raise ValueError('warm output drift')
    if which=='native' and not v['diagnostic']:
        store=p['actions_after']['actions']
        if store['path']!=str(cache/'actions') or store['exists'] is not True or not store['entries']:
            raise ValueError('native action cache absent')
        for entries in p['outputs'].values():
            for e in entries:
                if str(Path(e['path']).relative_to(cache/'actions')) not in store['entries']:
                    raise ValueError('output absent from actual action cache')


def argument_parser():
    parser=infrastructure.argument_parser()
    parser.description=__doc__
    parser.set_defaults(output=ROOT/'artifacts/json-validation.json')
    return parser


def executable_evidence(args):
    if not args.reference_only and args.native_sha256 in (OLD_NATIVE,infrastructure.OLD_NATIVE,shared.OLD_NATIVE,shared.PRE_FIX_DEBUG_SHA):
        raise ValueError('old pre-JSON binary is not a new frozen candidate')
    return common.executable_evidence(args)


_base_plan_receipt=engine.plan_receipt


def plan_receipt(reference_only):
    plan = _base_plan_receipt(reference_only)
    plan['fixture_plan_sha256'] = plan.pop('sha256')
    plan['native_capacity'] = {method: profile.copy() for method,profile in NATIVE_CAPACITY.items()}
    plan['sha256'] = digest(json.dumps(plan,sort_keys=True).encode())
    return plan


_base_run_phase=engine.run_phase
for _name in ('SUITE','OLD_NATIVE','ORIGINS','OFFICIAL_FILES','LIMITATIONS','KNOWN_REFERENCE_DEFECTS','DEFECTS','CASEPLAN',
        'source_paths','fixture','require_rejection','phases_for','steps_for','plan_receipt','argument_parser','executable_evidence','check_spawns','run_phase','validate_phase'):
    setattr(engine,_name,globals()[_name])
plan_receipt, command_argv, source_bindings = engine.plan_receipt, engine.command_argv, engine.source_bindings
source_evidence, RecordingRunner = engine.source_evidence, engine.RecordingRunner


def report_passes(report):
    return engine.report_passes(report)


def main(argv=None):
    args=argument_parser().parse_args(argv)
    if args.output.expanduser().resolve()==FIRST_ATTEMPT:
        raise ValueError('report cannot overwrite preserved first failed frozen receipt')
    # The shared runner has a subrule-specific default-name convenience only.
    # Always pass JSON's explicit output rather than touching that shared module.
    output=args.output
    if args.reference_only and output==ROOT/'artifacts/json-validation.json':
        output=ROOT/'artifacts/json-reference.json'
    return engine.main(list(sys.argv[1:] if argv is None else argv)+['--output',str(output)])


if __name__=='__main__':
    def terminate(signum,frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM,terminate)
    sys.exit(main())
