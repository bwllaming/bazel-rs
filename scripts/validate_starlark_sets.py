#!/usr/bin/env python3
"""Bounded Set integration, not full Bazel parity. Never emulate reference crashes.

Reference-only mode does not inspect or execute native. Full mode requires a new
frozen ELF SHA. Synthetic unit receipts are harness tests, not engine evidence.
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
common, direct = utility.common, utility.direct
digest, evidence, snapshot, valid_evidence = utility.digest, utility.evidence, utility.snapshot, utility.valid_evidence
success, DirectReference = utility.success, utility.DirectReference
REGISTRY, REFERENCE_SHA256, SOURCE_PIN = utility.REGISTRY, utility.REFERENCE_SHA256, utility.SOURCE_PIN
SUITE = 'starlark-sets'
REPORT_KEYS = set('schema_version status runs phases errors cleanup owned_pids planned_checks plan limitations known_reference_defects known_parity_gaps membership_gap_closure integration_origin configuration started_at sources_before sources_after executables executables_after official_source_before official_source_after platforms_distfile scratch_directory registry_before registry_after cleanup_receipt completed_phases finished_at native_claim'.split())
OLD_NATIVE = '49afbf236ecba1389912041b67b06a2d270236d075a3302cddda023b32cc9521'
INTEGRATION_ORIGIN = dict(path='artifacts/starlark-set-integration-contract.json',
    sha256='d777381c0bcbd5b6452e8529f4d2e518d5992233ae900ab6e8ac5485cf5e6cf9')
OFFICIAL_FILES = {
    'src/main/java/net/starlark/java/eval/StarlarkSet.java': '1ff27780d41c2d2d1404d62968b8e4219a981bf24a90f96f41685dcad3135bf7',
    'src/main/java/net/starlark/java/eval/MethodLibrary.java': '97dbe205311c763385d88045ceac925faffffc77d3a0bc2217e2cf1eec6f6c27',
    'src/main/java/com/google/devtools/build/lib/analysis/configuredtargets/AbstractConfiguredTarget.java': '6ec77df09263c0e18d3443dd5911180156c43df4a5df2c2fc42aab01307fbbfc',
    'src/main/java/com/google/devtools/build/lib/packages/StarlarkProvider.java': '7a67cbf347d3e3bf966ff7f4db3f0c150cf61b4e15c397912bb5e7347d47e91a',
}
PLATFORM_SHA = '3384eb1c30762704fbe38e440204e114154086c8fc8a8c2e3e28441028c019a8'
SOURCES = sorted(set([Path(__file__).resolve(), ROOT / 'tests/test_validate_starlark_sets.py',
    ROOT / 'Cargo.toml', ROOT / 'Cargo.lock', ROOT / 'vendor/starlark-0.14.2.provenance.json',
    ROOT / 'tests/fixtures/builtin_modules/provenance.json',
    *ROOT.glob('scripts/*.py'), *ROOT.glob('src/**/*.rs'),
    *ROOT.glob('vendor/starlark-0.14.2/**/*.rs'),
    *ROOT.glob('vendor/starlark-0.14.2/**/Cargo.toml'),
    *(p for p in REGISTRY.rglob('*') if p.is_file()),
    *(p for p in (ROOT / 'resources').rglob('*') if p.is_file())]))
KEY_PROBES = ('membership-set', 'membership-len', 'membership-string', 'membership-none',
    'membership-int', 'unexported-membership', 'unexported-index', 'exported-keys')
CASEPLAN = [
    common.case('constructor', ('metadata', 'hash-key', 'member', 'index', 'not-provider', 'target-key')+KEY_PROBES),
    common.case('globals', ('build', 'module', 'ordinary-bzl', 'module-bzl')),
    common.case('frozen', ('factory', 'base-error', 'provider-error')),
    common.case('algebra', ('ordered', 'elems', 'self-xor-augmented', 'self-xor-method')),
    common.case('boundaries', ('unhashable-element', 'set-unhashable', 'depset', 'args', 'disabled-flag', 'native-field')),
    common.case('provider', ('generated-closure',)),
    common.case('extension', ('repository-action',)),
]
NATIVE_ONLY = {'depset', 'args', 'disabled-flag', 'native-field'}
DEFECTS = ('self-xor-augmented', 'self-xor-method')
KNOWN_REFERENCE_DEFECTS = [dict(id=p, classification='INTENTIONAL_DIVERGENCE',
    reference_status='KNOWN_REFERENCE_DEFECT', reference_returncode=37,
    signature='java.util.ConcurrentModificationException', native_result='([], [])\n',
    parity_claim=False, evidence='CURRENT_EXECUTION; original edge contract is not rewritten') for p in DEFECTS]
KNOWN_PARITY_GAPS = [dict(id='preexisting-target-invalid-key-membership', classification='HISTORICAL_DIFFERENTIAL_MISMATCH',
    original_classification='KNOWN_DIFFERENTIAL_MISMATCH',
    scope='set/len/string membership keys, not provider indexing or attribute constraints',
    reference=dict(returncode=1, diagnostic='Type Target only supports querying by object constructors',
        receipt='artifacts/starlark-set-membership-discovery.json', sha256='dff7826e2991f423a0d7b47a710d28f8f71b97afb35a6f9dd62a3f800683771d'),
    native=dict(status='DEBUG_PREPARATION_ONLY_NOT_FROZEN_H2_PASS', returncode=0, result=False,
        executable_sha256='a2038a9d04e03e17f99cea8ac7e08989cc83c70c61f5b0e036674f3835ddff40',
        receipt='artifacts/starlark-sets-debug-preparation.json', sha256='0c6d412039a9f1826c454f1588f72fb6457b58968961eb1a2e8e3a182bde3542'),
    earlier_reference_preparation=dict(receipt='artifacts/starlark-sets-reference.json',
        sha256='f8201f0d3abfcd3edb6704a68a45b2e3be43d32877799be21a38fa03fe39a8c7',
        phases=21, commands=70, status='REFERENCE_ONLY_PASS', native_closure_claim=False),
    parity_claim=False, current_matrix_claim=False,
    closure_requires='New paired native/reference rejected-key phases plus actual valid exported-key action bytes; reference-only cannot close this gap')]
KEY_EDGE_ORIGIN = dict(path='artifacts/starlark-target-provider-key-edge-contract.json',
    sha256='7a329e35179b1d55960b54f368aa765aad5785ad9442131714b528a5b87803ba')
PRESERVED_REPORTS = {
    ROOT / 'artifacts/starlark-sets-reference.json',
    ROOT / 'artifacts/starlark-sets-reference-first-attempt.json',
    *(ROOT / v['receipt'] for gap in KNOWN_PARITY_GAPS for v in (gap['reference'], gap['native'])),
    ROOT / KEY_EDGE_ORIGIN['path'],
}
PRE_FIX_DEBUG_SHA = KNOWN_PARITY_GAPS[0]['native']['executable_sha256']
LIMITATIONS = [
    'PASS_ONLY_BOUNDED_PROFILE, never full Bazel/application parity.',
    'REQUIRED known_reference_defects: multi-element self-XOR crashes pinned Bazel with RC37; native safely empties the original alias. INTENTIONAL_DIVERGENCE, NOT reference parity PASS.',
    'Native-only unsupported CLI disable flag, Args/depset set sequences and native.set are not differential checks.',
    'Explicit list(s) crosses action and string_list boundaries; no all-attrs Set acceptance.',
    'ASCII string.elems arithmetic only: native string.elems proxy type differs from reference list. No broad type, Unicode or general formatter parity.',
    'Constructor metadata is scoped; general frozen-provider hashing and provider identity policy are not broadened.',
    'HISTORICAL Target invalid-key membership mismatch remains immutable: reference rejected set/len/string while old debug returned False. New paired membership/type/export checks and valid-key bytes are REQUIRED. membership_gap_closure can report RESOLVED_FOR_FROZEN_CANDIDATE only after actual full-mode native/reference validation; reference-only never closes it.',
    'Reference warm incremental state is not native disk cache: restart is required for the producer reference two-hit proof.',
    'Reference action outputs 0555 and repository files 0644 are checked separately; Unix mode does not establish executable/test rule support.',
    'Reference-only preparation never executes native, claims native success, or reuses old H1 binary.',
]

# Exact initial file bytes from the immutable genuine three-family contract.
PROVIDER = '''P = provider(fields = ["files"])

def _producer(ctx):
    generated = ctx.actions.declare_file(ctx.label.name + ".generated")
    ctx.actions.run_shell(
        inputs = [ctx.file.b],
        outputs = [generated],
        arguments = [ctx.file.b.path, generated.path],
        command = 'cat "$1" > "$2"',
        mnemonic = "H2SetProducer",
    )
    return [P(files = set([generated, ctx.file.a, generated, ctx.file.a]))]

producer = rule(implementation = _producer, attrs = {
    "a": attr.label(allow_single_file = True),
    "b": attr.label(allow_single_file = True),
})

def _consumer(ctx):
    files = list(ctx.attr.dep[P].files)
    if len(files) != 2:
        fail("provider set did not deduplicate Files")
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        inputs = files,
        outputs = [out],
        arguments = [out.path] + [f.path for f in files],
        command = 'out="$1"; shift; cat "$@" > "$out"',
        mnemonic = "H2SetConsumer",
    )
    return [DefaultInfo(files = depset([out]))]

consumer = rule(implementation = _consumer, attrs = {"dep": attr.label(providers = [P])})

def _bad(ctx):
    files = ctx.attr.dep[P].files
    files.add(list(files)[0])
    fail("unexpected mutable provider payload")

bad = rule(implementation = _bad, attrs = {"dep": attr.label(providers = [P])})
'''
PROVIDER_BUILD = ('load(":defs.bzl", "producer", "consumer", "bad")\n'
    'producer(name = "producer", a = "a.txt", b = "b.txt")\n'
    'consumer(name = "consume", dep = ":producer")\nbad(name = "bad", dep = ":producer")\n')
BASE = '''BASE = set(["b", "a", "b"])

def copy_base():
    return set(BASE)
'''
FROZEN = '''load(":base.bzl", "BASE", "copy_base")

def _impl(ctx):
    s = copy_base()
    alias = s
    pure = s | set(["c", "b"])
    lines = ["base=" + ",".join(list(BASE)), "pure=" + ",".join(list(pure))]
    s |= set(["c", "b"])
    lines.append("union=" + ",".join(list(alias)))
    s -= set(["b"])
    lines.append("difference=" + ",".join(list(alias)))
    s &= set(["c", "a", "z"])
    lines.append("intersection=" + ",".join(list(alias)))
    s ^= set(["c", "d"])
    lines.append("symmetric=" + ",".join(list(s)))
    lines.append("alias=" + ",".join(list(alias)))
    lines.append("base_after=" + ",".join(list(BASE)))
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(output = out, content = "\\n".join(lines) + "\\n", is_executable = False)
    return [DefaultInfo(files = depset([out]))]

frozen_copy = rule(implementation = _impl)

def _bad(ctx):
    BASE.add("z")
    fail("unexpected mutable loaded BASE")

bad = rule(implementation = _bad)
'''
EXTENSION = '''load(":base.bzl", "BASE")

def _repo_impl(ctx):
    ctx.file("data", "\\n".join(ctx.attr.items) + "\\n", executable = False)
    ctx.file("BUILD.bazel", 'exports_files(["data"], visibility = ["//visibility:public"])\\n', executable = False)

_data_repo = repository_rule(implementation = _repo_impl, attrs = {"items": attr.string_list()})

def _extension_impl(ctx):
    items = set(BASE)
    for mod in ctx.modules:
        for tag in mod.tags.values:
            items.update(tag.items)
    if list(BASE) != ["b", "a"]:
        fail("frozen BASE changed")
    _data_repo(name = "generated", items = list(items))

ext = module_extension(
    implementation = _extension_impl,
    tag_classes = {"values": tag_class(attrs = {"items": attr.string_list()})},
)

def _consumer(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        inputs = [ctx.file.src], outputs = [out],
        arguments = [ctx.file.src.path, out.path],
        command = 'cat "$1" > "$2"', mnemonic = "H2ExtensionConsumer",
    )
    return [DefaultInfo(files = depset([out]))]

consumer = rule(implementation = _consumer, attrs = {"src": attr.label(allow_single_file = True)})
'''
EXTENSION_MODULE = '''module(name = "h2_extension")
e = use_extension("//:defs.bzl", "ext")
e.values(items = ["b", "c", "b"])
e.values(items = ["d", "a", "d"])
use_repo(e, "generated")
'''
REPO_BUILD = b'exports_files(["data"], visibility = ["//visibility:public"])\n'


def write(expression):
    return ('print("SET_ACTION_READY")\nout=ctx.actions.declare_file("consume.out")\n'
        'ctx.actions.write(out,'+expression+')\nreturn [DefaultInfo(files=depset([out]))]\n')


def fixture(family, probe):
    if not any(c['name'] == family and probe in c['probes'] for c in CASEPLAN):
        raise ValueError('unknown planned probe')
    files = {'MODULE.bazel': 'module(name="set_contract")\n',
        'BUILD.bazel': 'load(":defs.bzl","check")\ncheck(name="consume")\n'}
    v = dict(target='//:consume', expected=b'ok\n', actions=1, diagnostic=None,
        changed=None, markers=['SET_CALLBACK', 'SET_ACTION_READY'], stage='callback',
        genuine=False, native_only=family == 'boundaries' and probe in NATIVE_ONLY,
        defect=probe in DEFECTS)
    body, prefix = write('"ok\\n"'), ''
    if family == 'provider' or probe == 'provider-error':
        files.update({'MODULE.bazel': 'module(name = "h2_provider")\n', 'BUILD.bazel': PROVIDER_BUILD,
            'defs.bzl': PROVIDER, 'a.txt': 'A\n', 'b.txt': 'B\n'})
        v.update(expected=b'B\nA\n', actions=2, changed=('b.txt', b'B2\n', b'B2\nA\n'), genuine=True, markers=[])
        if probe == 'provider-error':
            v.update(target='//:bad', diagnostic='frozen', changed=None)
    elif family == 'frozen':
        files.update({'MODULE.bazel': 'module(name = "h2_frozen")\n', 'base.bzl': BASE, 'defs.bzl': FROZEN,
            'BUILD.bazel': 'load(":defs.bzl", "frozen_copy", "bad")\nfrozen_copy(name = "consume")\nbad(name = "bad")\n'})
        v.update(genuine=True, markers=[], expected=b'base=b,a\npure=b,a,c\nunion=b,a,c\ndifference=a,c\nintersection=a,c\nsymmetric=a,d\nalias=a,d\nbase_after=b,a\n',
            changed=('base.bzl', BASE.replace('["b", "a", "b"]', '["q", "a", "q"]').encode(),
                b'base=q,a\npure=q,a,c,b\nunion=q,a,c,b\ndifference=q,a,c\nintersection=a,c\nsymmetric=a,d\nalias=a,d\nbase_after=q,a\n'))
        if probe == 'base-error':
            v.update(target='//:bad', diagnostic='frozen', changed=None)
    elif family == 'extension' or probe == 'module-bzl':
        files.update({'MODULE.bazel': EXTENSION_MODULE, 'base.bzl': 'BASE = set(["b", "a", "b"])\n',
            'defs.bzl': EXTENSION,
            'BUILD.bazel': 'load(":defs.bzl", "consumer")\nconsumer(name = "consume", src = "@generated//:data")\n'})
        v.update(genuine=True, markers=[], expected=b'b\na\nc\nd\n',
            changed=('MODULE.bazel', EXTENSION_MODULE.replace('["d", "a", "d"]', '["d", "a", "e"]').encode(), b'b\na\nc\nd\ne\n'))
    elif family == 'constructor':
        if probe == 'metadata':
            files.update({'one.bzl': 'R=set\n', 'two.bzl': 'R=set\n'})
            prefix = 'load(":one.bzl",R1="R")\nload(":two.bzl",R2="R")\n'
            body = write('repr((R1==R2,R1==set,R2==set,R1!=list,repr(R1),str(R2),type(R1),dir(R2),hasattr(R1,"type"),repr(R1([2,1,2]))))+"\\n"')
            v['expected'] = b'(True, True, True, True, "<built-in function set>", "<built-in function set>", "builtin_function_or_method", [], False, "set([2, 1])")\n'
        elif probe in ('hash-key', 'member', 'index', 'not-provider'):
            expr = {'hash-key': '{set:"x"}', 'member': 'set([set,set])', 'index': 'set[int]',
                'not-provider': 'rule(implementation=_child,attrs={"dep":attr.label(providers=[set])})'}[probe]
            prefix = 'def _child(ctx):\n    return []\nx='+expr+'\n'
            v.update(diagnostic='constructor-hash' if probe in ('hash-key', 'member') else probe, stage='loading')
        else:
            prefix = 'def _child(ctx):\n    return []\nchild=rule(implementation=_child)\n'
            files['BUILD.bazel'] = 'load(":defs.bzl","check","child")\nchild(name="child")\ncheck(name="consume",dep=":child")\n'
            if probe.startswith('membership-'):
                key = {'membership-set': 'set', 'membership-len': 'len', 'membership-string': '"x"',
                    'membership-none': 'None', 'membership-int': '7'}[probe]
                body = 'x='+key+' in ctx.attr.dep\n'+body
                v['diagnostic'] = 'membership-builtin' if probe in ('membership-set', 'membership-len') else probe
            elif probe in ('unexported-membership', 'unexported-index', 'exported-keys'):
                # BOX's provider is never assigned directly to an exported name;
                # construction is during loading, not inside the analysis callback.
                prefix = 'def make():\n    return provider()\nBOX=[make()]\nP=provider()\n'+prefix
                if probe == 'exported-keys':
                    body = write('repr((P in ctx.attr.dep,DefaultInfo in ctx.attr.dep,len(ctx.attr.dep[DefaultInfo].files.to_list())))+"\\n"')
                    v['expected'] = b'(False, True, 0)\n'
                else:
                    body = 'x='+('BOX[0] in ctx.attr.dep' if probe == 'unexported-membership' else 'ctx.attr.dep[BOX[0]]')+'\n'+body
                    v['diagnostic'] = probe
            else:
                body = 'x=ctx.attr.dep[set]\n'+body
                v['diagnostic'] = 'target-key'
    elif family == 'globals':
        if probe == 'module':
            # Root MODULE is actually evaluated; no lazy-local-query loophole.
            files['MODULE.bazel'] += 'print("SET_MODULE",list(set([2,1,2])))\n'
            v['markers'].append('SET_MODULE [2, 1]')
        elif probe == 'build':
            # BUILD print is not in the native bounded profile. Use the actual
            # BUILD-global constructor result as an attribute and output bytes.
            files['BUILD.bazel'] = 'load(":defs.bzl","check")\ncheck(name="consume",words=list(set(["b","a","b"])))\n'
            body = write('repr(ctx.attr.words)+"\\n"')
            v['expected'] = b'["b", "a"]\n'
        else:
            prefix = 'S=set([2,1,2])\nprint("SET_BZL",list(S))\n'
            v['markers'].append('SET_BZL [2, 1]')
    elif family == 'algebra':
        if probe in DEFECTS:
            operation = 's ^= s' if probe.endswith('augmented') else 's.symmetric_difference_update(s)'
            prefix = ('def probe():\n    s=set([1,2]); alias=s\n    print("SET_BEFORE_SELF_XOR",list(s))\n    '
                +operation+'\n    return repr((list(s),list(alias)))\nRESULT=probe()\n')
            body = write('RESULT+"\\n"')
            v['expected'] = b'([], [])\n'
            v['markers'].append('SET_BEFORE_SELF_XOR [1, 2]')
        elif probe == 'elems':
            prefix = 'E="bc".elems()\n'
            body = ('s=set("ab".elems())\nu=s.union(E)\ni=s.intersection(E)\nd=s.difference(E)\n'
                'small=set("b".elems()).issubset(E)\nr=s.update(E)\n'
                +write('repr((list(u),list(i),list(d),small,r,list(s)))+"\\n"'))
            v['expected'] = b'(["a", "b", "c"], ["b"], ["a"], True, None, ["a", "b", "c"])\n'
        else:
            body = ('s=set([3,1,3,2])\na=s\np=s | set([2,4])\ns |= set([4])\ns -= set([1])\n'
                's &= set([4,3,8])\ns ^= set([4,5])\n'
                +write('repr((list(p),list(s),list(a)))+"\\n"'))
            v['expected'] = b'([3, 1, 2, 4], [3, 5], [3, 5])\n'
    else:
        expressions = {'unhashable-element': 'set([[1]])', 'set-unhashable': '{set():1}',
            'depset': 'depset(set([1]))', 'args': 'ctx.actions.args().add_all(set(["x"]))', 'native-field': 'native.set()',
            'disabled-flag': 'set()'}
        body = 'x='+expressions[probe]+'\n'+body
        v['diagnostic'] = probe
    if 'defs.bzl' not in files:
        attrs = ',attrs={"dep":attr.label()}' if probe == 'target-key' or probe in KEY_PROBES else ''
        if family == 'globals' and probe == 'build':
            attrs = ',attrs={"words":attr.string_list()}'
        files['defs.bzl'] = prefix+'def _impl(ctx):\n'+common.indent('print("SET_CALLBACK")\n'+body)+'check=rule(implementation=_impl'+attrs+')\n'
    if v['diagnostic']:
        v.update(expected=None, actions=0)
        v['markers'] = [] if v['stage'] == 'loading' or probe == 'disabled-flag' else ['SET_CALLBACK'] if not v['genuine'] else []
    v['files'] = {p: text.encode() for p, text in files.items()}
    return v


DIAGNOSTICS = {
    'frozen': r'(?:trying to mutate a frozen set|Cannot mutate frozen set|Immutable)',
    'constructor-hash': r'(?:unhashable type[^\n]*builtin_function_or_method|builtin_function_or_method[^\n]*not hashable)',
    'index': r'(?:builtin_function_or_method[^\n]*(?:operator|index)|Operation `\[\]` not supported for types `builtin_function_or_method`)',
    'not-provider': r'(?:providers[^\n]*(?:Provider|provider)|(?:Provider|provider)[^\n]*builtin_function_or_method)',
    'target-key': r'(?:Target[^\n]*(?:provider|Provider)|Target only supports indexing by object constructors|(?:key|index)[^\n]*(?:provider|Provider)|unsupported provider key)',
    'unhashable-element': r'(?:unhashable[^\n]*list|list[^\n]*not hashable)',
    'set-unhashable': r'(?:unhashable[^\n]*set|set[^\n]*not hashable)',
    'depset': r'(?:depset[^\n]*(?:list|tuple|sequence)|expected[^\n]*(?:list|tuple)[^\n]*set)',
    'args': r'expected list or tuple, got set',
    'disabled-flag': r'(?:unsupported|unknown|unrecognized)[^\n]*incompatible_enable_starlark_set',
    'native-field': r'(?:no (?:attribute|field|method)[^\n]*[\x27"`]set|has no attribute[^\n]*[\x27"`]set)',
}
KEY_DIAGNOSTICS = {
    **{name: dict(native=r'Target membership requires an exported provider constructor',
        reference=r'Type Target only supports querying by object constructors, got '+kind+r' instead')
        for name, kind in [('membership-builtin', 'builtin_function_or_method'), ('membership-string', 'string'),
            ('membership-none', 'NoneType'), ('membership-int', 'int')]},
    'unexported-membership': dict(native=r'Target membership requires an exported provider constructor',
        reference=r'only supports querying by exported providers\. Assign the provider a name in a top-level assignment statement\.'),
    'unexported-index': dict(native=r'Target indexing requires an exported provider constructor',
        reference=r'only supports indexing by exported providers\. Assign the provider a name in a top-level assignment statement\.'),
}


def require_rejection(r, diagnostic, engine):
    code, text = r['returncode'], r['stderr']
    codes = (1, 2) if engine == 'native' and diagnostic == 'disabled-flag' else (1,) if engine == 'native' else (1, 7)
    if diagnostic in KEY_DIAGNOSTICS:
        codes = (1,)
    pattern = KEY_DIAGNOSTICS[diagnostic][engine] if diagnostic in KEY_DIAGNOSTICS else DIAGNOSTICS[diagnostic]
    if (type(code) is not int or code not in codes or r['timed_out'] or r['stdout'].strip()
            or any(r.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or common.UNRELATED.search(text)
            or re.search(r'(?:[\x27"`]set[\x27"`][^\n]*(?:not found|not defined))', text)
            or not re.search(pattern, utility.error_lines(text), re.I)):
        raise ValueError('expected anchored controlled Set rejection: '+diagnostic)
    if utility.marker_count(r, 'SET_ACTION_READY') or 'SUBCOMMAND:' in text:
        raise ValueError('rejection reached action publication')


def require_defect(r):
    if (type(r['returncode']) is not int or r['returncode'] != 37 or r['timed_out'] or r['stdout']
            or any(r.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or 'FATAL: bazel crashed due to an internal error.' not in r['stderr']
            or 'Caused by: java.util.ConcurrentModificationException' not in r['stderr']
            or 'StarlarkSet.symmetricDifferenceUpdate(StarlarkSet.java:788)' not in r['stderr']
            or not utility.marker_count(r, 'SET_BEFORE_SELF_XOR [1, 2]')
            or utility.marker_count(r, 'SET_CALLBACK') or utility.marker_count(r, 'SET_ACTION_READY')):
        raise ValueError('required exact known reference self-XOR crash, not ordinary rejection')


def phases_for(item, reference_only=False):
    return [(p, e) for p in item['probes'] for e in ('reference', 'native')
        if not (reference_only and e == 'native')
        and not (item['name'] == 'boundaries' and p in NATIVE_ONLY and e == 'reference')]


def steps_for(family, probe, engine):
    v = fixture(family, probe)
    if v['defect'] and engine == 'reference':
        return [('defect', 'query', '//:consume', [])]
    if v['diagnostic']:
        return [('reject', 'build', v['target'], ['--incompatible_enable_starlark_set=false'] if probe == 'disabled-flag' else [])]
    steps = []
    for stage in ['cold', 'warm']+(['restart'] if family == 'provider' and engine == 'reference' else [])+(['changed'] if v['changed'] else []):
        if stage == 'restart':
            steps.append(('shutdown', 'shutdown', '', []))
        steps.append((stage, 'build', v['target'], []))
        if engine == 'reference':
            steps.append((stage, 'cquery', v['target'], ['--output=files']))
    if family == 'provider':
        steps.append(('default-empty', 'cquery' if engine == 'reference' else 'build', '//:producer', ['--output=files'] if engine == 'reference' else []))
        if engine == 'reference':
            steps.append(('graph', 'aquery', 'deps(//:consume)', ['--output=jsonproto']))
    if family == 'extension' or probe == 'module-bzl':
        steps.extend([('repository', 'cquery' if engine == 'reference' else 'build', '@generated//:data', ['--output=files'] if engine == 'reference' else []),
            ('canonical', 'cquery' if engine == 'reference' else 'query', '@generated//:data',
                ['--output=starlark', '--starlark:expr="@@%s//%s:%s" % (target.label.repo_name,target.label.package,target.label.name)'] if engine == 'reference' else [])])
    return steps


def plan_receipt(reference_only):
    entries = []
    for item in CASEPLAN:
        for p, e in phases_for(item, reference_only):
            v = fixture(item['name'], p)
            entries.append(dict(family=item['name'], probe=p, engine=e, steps=steps_for(item['name'], p, e),
                fixture={k: digest(raw) for k, raw in v['files'].items()}, expected=None if v['expected'] is None else digest(v['expected'])))
    return dict(families=len(CASEPLAN), probes=sum(len(c['probes']) for c in CASEPLAN), phases=len(entries),
        commands=2+sum(len(e['steps']) for e in entries), sha256=digest(json.dumps(entries, sort_keys=True).encode()))


def argument_parser():
    parser = common.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/starlark-sets-validation.json')
    parser.add_argument('--reference-source', type=Path, default=Path('/tmp/opencode/bazel-9.3-reference'))
    parser.add_argument('--platforms-distfile', type=Path, default=Path('/tmp/opencode/symbolic-macro-edge-contract/bazel-user-root/cache/repos/v1/content_addressable/sha256') / PLATFORM_SHA / 'file')
    return parser


def executable_evidence(args):
    if not args.reference_only and args.native_sha256 in (OLD_NATIVE, PRE_FIX_DEBUG_SHA):
        raise ValueError('old H1/pre-fix debug binary cannot be frozen H2 candidate')
    return common.executable_evidence(args)


def source_evidence(path):
    commit = subprocess.check_output(['git', '-C', str(path), 'rev-parse', 'HEAD'], timeout=10).decode().strip()
    files = {p: digest((path / p).read_bytes()) for p in OFFICIAL_FILES}
    if commit != SOURCE_PIN or files != OFFICIAL_FILES:
        raise ValueError('pinned official source changed')
    return dict(path=str(path), commit=commit, files=files)


def source_bindings():
    # Hash-only binding keeps the complete production/vendor surface compact.
    return {str(p): dict(path=str(p), sha256=digest(p.read_bytes()), size=p.stat().st_size,
        mode=p.stat().st_mode & 0o7777) for p in SOURCES}


class RecordingRunner(utility.RecordingRunner):
    def __init__(self, report, args):
        super().__init__(report, args.timeout)
        self.args = args

    def run(self, name, argv, cwd, timeout=None):
        binding = dict(executables=executable_evidence(self.args), source=source_evidence(self.args.reference_source),
            production=source_bindings())
        if binding != dict(executables=self.report['executables'], source=self.report['official_source_before'],
                production=self.report['sources_before']):
            raise ValueError('pre-command binary/source binding changed')
        r = super().run(name, argv, cwd, timeout)
        r['preflight_binding_sha256'] = digest(json.dumps(binding, sort_keys=True).encode())
        return r


def command_argv(args, ref, engine, cache, root, step):
    stage, command, target, extra = step
    if command == 'shutdown':
        return ref.startup+['shutdown']+list(COMMON_FLAGS)
    flags = []
    if engine == 'reference':
        flags = ['--incompatible_autoload_externally=', '--noshow_progress', '--repository_disable_download', '--distdir='+str(root / 'distdir')]
        if command == 'build':
            flags += ['--subcommands', '--execution_log_json_file='+str(cache.parent / (stage+'.execution.json'))]
    argv = utility.extensions.command_argv(args, ref, engine, cache, root / 'registry', command, target, extra, flags)
    # Known crashing probes use a private batch JVM, never the live server.
    if stage == 'defect':
        argv.insert(1, '--batch')
        argv = [a.replace(str(ref.output_base), str(cache.parent / 'crash-output-base'))
            .replace(str(ref.output_user_root), str(cache.parent / 'crash-user-root')) for a in argv]
    return argv


def json_objects(raw):
    text, result, decoder = raw.decode(), [], json.JSONDecoder()
    while text.strip():
        text = text.lstrip()
        value, end = decoder.raw_decode(text)
        result.append(value)
        text = text[end:]
    return result


def check_spawns(spawns, family, probe, stage, expected):
    names = sorted(s['mnemonic'] for s in spawns)
    want = (['H2SetConsumer', 'H2SetProducer'] if family == 'provider' else
        ['H2ExtensionConsumer'] if family == 'extension' or probe == 'module-bzl' else [])
    if names != ([] if stage in ('warm', 'restart') else want):
        raise ValueError('actual producer/consumer execution dependency missing')
    producer = b'B2\n' if stage == 'changed' else b'B\n'
    for spawn in spawns:
        if spawn['exitCode'] != 0 or spawn.get('cacheHit') or not spawn['actualOutputs']:
            raise ValueError('invalid action execution evidence')
        mnemonic = spawn['mnemonic']
        inputs = ({'b.txt': producer} if mnemonic == 'H2SetProducer' else
            {'producer.generated': producer, 'a.txt': b'A\n'} if mnemonic == 'H2SetConsumer' else
            {'external/+ext+generated/data': expected})
        actual = {i['path']: i for i in spawn['inputs']}
        if len(actual) != len(inputs):
            raise ValueError('action input count differs from genuine dependency closure')
        for suffix, raw in inputs.items():
            matches = [i for path, i in actual.items() if path == suffix or path.endswith('/'+suffix)]
            if len(matches) != 1 or matches[0]['digest']['hash'] != digest(raw):
                raise ValueError('action dependency input hash/path mismatch')
        output = spawn['actualOutputs']
        raw = producer if mnemonic == 'H2SetProducer' else expected
        name = 'producer.generated' if mnemonic == 'H2SetProducer' else 'consume.out'
        if len(output) != 1 or not output[0]['path'].endswith('/'+name) or output[0]['digest']['hash'] != digest(raw):
            raise ValueError('action output is not the demanded generated artifact')


def check_graph(graph):
    fragments = {str(p['id']): p for p in graph['pathFragments']}
    def path(key, seen=()):
        key = str(key)
        if key in seen:
            raise ValueError('cyclic artifact path')
        f = fragments[key]
        return (path(f['parentId'], seen+(key,))+'/' if 'parentId' in f else '')+f['label']
    artifacts = {str(a['id']): path(a['pathFragmentId']) for a in graph['artifacts']}
    deps = {str(d['id']): d for d in graph['depSetOfFiles']}
    def inputs(ids, seen=()):
        result = set()
        for key in map(str, ids):
            if key in seen:
                raise ValueError('cyclic input depset')
            d = deps[key]
            result.update(artifacts[str(a)] for a in d.get('directArtifactIds', []))
            result.update(inputs(d.get('transitiveDepSetIds', []), seen+(key,)))
        return result
    actions = {a['mnemonic']: a for a in graph['actions']}
    if len(graph['actions']) != 2 or set(actions) != {'H2SetConsumer', 'H2SetProducer'}:
        raise ValueError('actual demanded producer missing from graph')
    producer, consumer = actions['H2SetProducer'], actions['H2SetConsumer']
    outputs = {artifacts[str(i)] for i in producer['outputIds']}
    if (len(outputs) != 1 or not next(iter(outputs)).endswith('/producer.generated')
            or inputs(producer['inputDepSetIds']) != {'b.txt'}
            or inputs(consumer['inputDepSetIds']) != outputs | {'a.txt'}):
        raise ValueError('consumer did not demand genuine provider Files')


def output_paths(r, target, engine, root, workspace, ref):
    if target == '@generated//:data' and engine == 'native':
        # Require the known canonical identity; never normalize arbitrary labels.
        target = '@@+ext+generated//:data'
    return (direct.native_paths(r, target, root) if engine == 'native' else
        utility.reference_output_paths(r['stdout'], workspace, ref.output_base))


def remember_pid(ref, report):
    path = ref.output_base / 'server/server.pid.txt'
    if path.exists():
        pid = int(path.read_text().strip())
        if pid <= 1:
            raise ValueError('invalid owned PID')
        if pid not in report['owned_pids']:
            report['owned_pids'].append(pid)


def run_phase(args, runner, root, workspace, ref, family, probe, engine):
    v = fixture(family, probe)
    # The shared materializer's "genuine" switch installs its unrelated skylib
    # archive. Our original functional files are already complete and exact.
    paths = utility.materialize(workspace, dict(v, genuine=False))
    cache = root / 'cases' / family / probe / engine / 'cache'
    cache.parent.mkdir(parents=True, exist_ok=True)
    phase = dict(family=family, probe=probe, engine=engine, status='FAIL', runs=[], outputs={}, logs={},
        input_snapshots={}, cache=str(cache), actions_before=utility.action_store(cache), extra_outputs={})
    try:
        for i, step in enumerate(steps_for(family, probe, engine)):
            stage, cmd, target, _ = step
            if stage == 'changed' and cmd == 'build':
                (workspace / v['changed'][0]).write_bytes(v['changed'][1])
            phase['input_snapshots'][str(i)] = snapshot(paths)
            remember_pid(ref, runner.report)
            r = runner.run(family+'.'+probe+'.'+engine+'.'+str(i),
                command_argv(args, ref, engine, cache, root, step), workspace,
                timeout=min(args.timeout, 15) if cmd == 'shutdown' else args.timeout)
            remember_pid(ref, runner.report)
            phase['runs'].append(r['id'])
            if stage == 'defect':
                require_defect(r)
            elif stage == 'reject':
                require_rejection(r, v['diagnostic'], engine)
                if engine == 'reference':
                    log = cache.parent / 'reject.execution.json'
                    phase['logs']['reject'] = evidence(log) if log.exists() else dict(path=str(log), absent=True)
            else:
                success(r)
                if cmd == 'build' and engine == 'reference':
                    phase['logs'][stage] = evidence(cache.parent / (stage+'.execution.json'))
                if stage in ('cold', 'warm', 'restart', 'changed', 'repository') and cmd == ('build' if engine == 'native' else 'cquery'):
                    outs = output_paths(r, target, engine, root, workspace, ref)
                    phase['outputs'][stage] = [evidence(p) for p in outs]
                    if stage == 'repository' and len(outs) == 1:
                        phase['extra_outputs']['BUILD.bazel'] = evidence(outs[0].parent / 'BUILD.bazel')
                    if family == 'provider':
                        generated = (list((cache / 'actions').rglob('producer.generated')) if engine == 'native'
                            else [outs[0].parent / 'producer.generated'])
                        phase['extra_outputs'][stage] = [evidence(p) for p in generated if p.is_file()]
        phase['status'] = 'KNOWN_REFERENCE_DEFECT' if v['defect'] and engine == 'reference' else 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    finally:
        phase['inputs_after'] = snapshot(paths)
        phase['actions_after'] = utility.action_store(cache)
        phase['forbidden_outputs'] = sorted(str(p) for base in (workspace, cache, ref.output_base / 'execroot')
            for p in base.rglob('rejected.marker'))
    return phase


def check_input(entries, files, workspace):
    if set(entries) != {str(workspace / p) for p in files}:
        raise ValueError('unplanned source files')
    for p, raw in files.items():
        e = entries[str(workspace / p)]
        if e['path'] != str(workspace / p) or e['kind'] != 'file' or e['mode'] != 0o644 or not valid_evidence(e, raw):
            raise ValueError('fixture bytes/modes differ from plan')


def check_file(e, raw, mode):
    if e['kind'] != 'file' or e['mode'] != mode or not valid_evidence(e, raw):
        raise ValueError('actual output bytes/hash/mode mismatch')


def validate_phase(p, report, args, workspace, ref):
    family, probe, engine = p['family'], p['probe'], p['engine']
    item = next(c for c in CASEPLAN if c['name'] == family)
    if (probe, engine) not in phases_for(item, args.reference_only):
        raise ValueError('unplanned/native-only phase')
    v = fixture(family, probe)
    expected_status = 'KNOWN_REFERENCE_DEFECT' if v['defect'] and engine == 'reference' else 'PASS'
    if p['status'] != expected_status or p.get('error') or p['forbidden_outputs']:
        raise ValueError('failed phase or false defect parity claim')
    root = Path(report['scratch_directory'])
    cache = root / 'cases' / family / probe / engine / 'cache'
    if p['cache'] != str(cache):
        raise ValueError('cache binding changed')
    utility.no_actions(p['actions_before'], cache)
    if any(x['exists'] for x in p['actions_before'].values()):
        raise ValueError('cold cache already existed')
    if v['diagnostic'] or (v['defect'] and engine == 'reference'):
        utility.no_actions(p['actions_after'], cache)
    steps = steps_for(family, probe, engine)
    if len(p['runs']) != len(steps) or len(set(p['runs'])) != len(steps) or set(p['input_snapshots']) != {str(i) for i in range(len(steps))}:
        raise ValueError('missing/extra/duplicate commands or source snapshots')
    files, expected_outputs, expected_logs = v['files'].copy(), set(), set()
    for i, (rid, step) in enumerate(zip(p['runs'], steps)):
        stage, cmd, target, _ = step
        if stage == 'changed' and cmd == 'build':
            files[v['changed'][0]] = v['changed'][1]
        check_input(p['input_snapshots'][str(i)], files, workspace)
        r = report['runs'][rid]
        if (type(rid) is not int or r['id'] != rid or r['name'] != family+'.'+probe+'.'+engine+'.'+str(i)
                or r['cwd'] != str(workspace) or r['argv'] != command_argv(args, ref, engine, cache, root, step)
                or r['timeout_seconds'] != (min(args.timeout, 15) if cmd == 'shutdown' else args.timeout)):
            raise ValueError('command binding differs from plan')
        if stage == 'defect':
            require_defect(r)
            continue
        if stage == 'reject':
            require_rejection(r, v['diagnostic'], engine)
            if engine == 'reference':
                expected_logs.add('reject')
                log = p['logs']['reject']
                path = str(cache.parent / 'reject.execution.json')
                if log != dict(path=path, absent=True):
                    if log['path'] != path or not valid_evidence(log) or json_objects(base64.b64decode(log['bytes_base64'])):
                        raise ValueError('rejected analysis executed an action')
        else:
            success(r)
        if cmd == 'build' and (i == 0 or engine == 'native' or stage == 'changed'):
            for marker in v['markers']:
                if not utility.marker_count(r, marker):
                    raise ValueError('missing actual phase/callback marker: '+marker)
        if v['diagnostic']:
            if v['stage'] == 'loading' and utility.marker_count(r, 'SET_CALLBACK'):
                raise ValueError('callback before validation')
            continue
        if cmd == 'build' and stage not in ('repository', 'default-empty'):
            if engine == 'native':
                common.action_counts(r, (0, v['actions']) if stage == 'warm' else (v['actions'], 0))
            else:
                expected_logs.add(stage)
                log = p['logs'][stage]
                if log['path'] != str(cache.parent / (stage+'.execution.json')) or not valid_evidence(log):
                    raise ValueError('execution log provenance')
                spawns = json_objects(base64.b64decode(log['bytes_base64']))
                check_spawns(spawns, family, probe, stage, v['changed'][2] if stage == 'changed' else v['expected'])
                if stage == 'restart' and not re.search(r'\b2 action cache hit', r['stderr']):
                    raise ValueError('reference restart did not prove two persistent cache hits')
        if stage == 'default-empty' and r['stdout'] != '':
            raise ValueError('producer implicit DefaultInfo.files is not empty')
        if stage == 'graph':
            check_graph(json.loads(r['stdout']))
        if stage == 'canonical' and r['stdout'] != '@@+ext+generated//:data\n':
            raise ValueError('external File canonical label differs')
        if stage not in ('cold', 'warm', 'restart', 'changed', 'repository') or cmd != ('build' if engine == 'native' else 'cquery'):
            continue
        expected_outputs.add(stage)
        paths = output_paths(r, target, engine, root, workspace, ref)
        entries = p['outputs'][stage]
        if len(entries) != 1 or [str(x) for x in paths] != [e['path'] for e in entries]:
            raise ValueError('missing output or unbound output evidence')
        raw = v['changed'][2] if stage in ('changed', 'repository') else v['expected']
        check_file(entries[0], raw, 0o644 if stage == 'repository' else 0o555)
        if stage == 'repository':
            check_file(p['extra_outputs']['BUILD.bazel'], REPO_BUILD, 0o644)
            if p['extra_outputs']['BUILD.bazel']['path'] != str(paths[0].parent / 'BUILD.bazel'):
                raise ValueError('repository BUILD path mismatch')
        if family == 'provider':
            generated = p['extra_outputs'][stage]
            # Changed native cache can retain the original immutable action entry.
            expected = {b'B\n', b'B2\n'} if stage == 'changed' and engine == 'native' else {b'B2\n' if stage == 'changed' else b'B\n'}
            if {base64.b64decode(e['bytes_base64']) for e in generated} != expected:
                raise ValueError('generated producer output missing or wrong')
            for e in generated:
                check_file(e, base64.b64decode(e['bytes_base64']), 0o555)
                if not Path(e['path']).is_relative_to(cache if engine == 'native' else ref.output_base) or Path(e['path']).name != 'producer.generated':
                    raise ValueError('generated producer path binding')
    check_input(p['inputs_after'], files, workspace)
    if set(p['outputs']) != expected_outputs or set(p['logs']) != expected_logs:
        raise ValueError('missing/extra output or execution log phases')
    extra_keys = expected_outputs if family == 'provider' else {'BUILD.bazel'} if 'repository' in expected_outputs else set()
    if set(p['extra_outputs']) != extra_keys:
        raise ValueError('missing/extra producer or repository evidence')
    if 'cold' in p['outputs'] and p['outputs']['cold'] != p['outputs']['warm']:
        raise ValueError('warm output changed')
    if engine == 'native' and not v['diagnostic']:
        store = p['actions_after']['actions']
        if store['path'] != str(cache / 'actions') or store['exists'] is not True or not store['entries']:
            raise ValueError('positive native action store was never populated')
        for stage, entries in p['outputs'].items():
            if stage != 'repository':
                for e in entries:
                    if str(Path(e['path']).relative_to(cache / 'actions')) not in store['entries']:
                        raise ValueError('output absent from inspected actual action store')


def membership_closure(report, reference_only):
    """Current execution evidence, never a retcon of the pre-fix debug result.

    Called while constructing even failed reports. Only independently replayed
    paired phases can close the historical mismatch; reference preparation is
    explicitly unable to make a native closure claim.
    """
    args = argument_parser().parse_args([])
    for key, value in report['configuration'].items():
        setattr(args, key, Path(value) if isinstance(getattr(args, key), Path) else value)
    root = Path(report.get('scratch_directory', '/unexecuted'))
    workspace = root / 'workspace'
    ref = DirectReference(args.reference, workspace, root, None, SUITE)
    engines = ('reference',) if reference_only else ('reference', 'native')
    verified = {e: True for e in engines}
    observations = []
    for probe in KEY_PROBES:
        for engine in engines:
            matches = [p for p in report['phases'] if p['family'] == 'constructor'
                and p['probe'] == probe and p['engine'] == engine]
            observation = dict(probe=probe, engine=engine, verified=False, runs=[])
            observations.append(observation)
            if len(matches) != 1:
                verified[engine] = False
                continue
            phase = matches[0]
            try:
                validate_phase(phase, report, args, workspace, ref)
                for rid in phase['runs']:
                    r = report['runs'][rid]
                    if r['executable_sha256'] != report['executables'][engine]['sha256']:
                        raise ValueError('closure command was not executed by the recorded candidate')
                    observation['runs'].append(dict(id=rid, returncode=r['returncode'],
                        executable_sha256=r['executable_sha256'], stdout_sha256=digest(r['stdout'].encode()),
                        stderr_sha256=digest(r['stderr'].encode())))
                observation['output_sha256'] = {stage: [e['sha256'] for e in entries]
                    for stage, entries in phase['outputs'].items()}
                observation['verified'] = True
            except (KeyError, ValueError, TypeError, IndexError, AttributeError, OSError):
                verified[engine] = False
    frozen = (not reference_only and re.fullmatch('[0-9a-f]{64}', args.native_sha256 or '') is not None
        and args.native_sha256 not in (OLD_NATIVE, PRE_FIX_DEBUG_SHA)
        and report.get('executables', {}).get('native', {}).get('sha256') == args.native_sha256
        and report.get('executables') == report.get('executables_after')
        and report.get('sources_before') == report.get('sources_after')
        and report.get('official_source_before') == report.get('official_source_after'))
    native_verified = bool(frozen and verified.get('native') and verified['reference'])
    status = ('RESOLVED_FOR_FROZEN_CANDIDATE' if native_verified else
        'REFERENCE_PREPARED_NATIVE_NOT_RUN' if reference_only and verified['reference'] else 'INCOMPLETE')
    return dict(id=KNOWN_PARITY_GAPS[0]['id'], status=status,
        historical_classification='HISTORICAL_DIFFERENTIAL_MISMATCH',
        historical_metadata_sha256=digest(json.dumps(KNOWN_PARITY_GAPS, sort_keys=True).encode()),
        edge_contract_origin=KEY_EDGE_ORIGIN, required_probes=list(KEY_PROBES),
        native_closure_verified=native_verified, reference_verified=verified['reference'],
        candidate_sha256=args.native_sha256 if frozen else None, observations=observations,
        scope='Only the planned invalid/exported provider-key membership and indexing checks; unrelated self-XOR crashes remain INTENTIONAL_DIVERGENCE')


def report_passes(report):
    try:
        if set(report) != REPORT_KEYS:
            return False
        args = argument_parser().parse_args([])
        if set(report['configuration']) != set(vars(args)):
            return False
        for key, value in report['configuration'].items():
            setattr(args, key, Path(value) if isinstance(getattr(args, key), Path) else value)
        status = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS_ONLY_BOUNDED_PROFILE'
        start, end = [datetime.datetime.fromisoformat(report[k]) for k in ('started_at', 'finished_at')]
        if start.tzinfo is None or end.tzinfo is None or end < start:
            return False
        native_commands = sum(len(steps_for(c['name'], probe, engine)) for c in CASEPLAN
            for probe, engine in phases_for(c, args.reference_only) if engine == 'native')
        if report['native_claim'] != dict(status='NOT_RUN' if args.reference_only else 'EXECUTED_BOUNDED_PROFILE',
                commands=native_commands, pass_claim=not args.reference_only):
            return False
        if (report['status'] not in ('FAIL', status) or report['schema_version'] != 1
                or report['plan'] != plan_receipt(args.reference_only) or report['planned_checks'] != CASEPLAN
                or report['limitations'] != LIMITATIONS or report['known_reference_defects'] != KNOWN_REFERENCE_DEFECTS
                or report['known_parity_gaps'] != KNOWN_PARITY_GAPS
                or report['membership_gap_closure'] != membership_closure(report, args.reference_only)
                or report['membership_gap_closure']['status'] != ('REFERENCE_PREPARED_NATIVE_NOT_RUN'
                    if args.reference_only else 'RESOLVED_FOR_FROZEN_CANDIDATE')
                or report['integration_origin'] != INTEGRATION_ORIGIN or report['errors'] or report['cleanup']
                or report['sources_before'] != report['sources_after'] or set(report['sources_before']) != set(map(str, SOURCES))
                or report['executables'] != report['executables_after']
                or report['official_source_before'] != report['official_source_after']
                or report['official_source_before'] != dict(path=str(args.reference_source), commit=SOURCE_PIN, files=OFFICIAL_FILES)):
            return False
        for path, e in report['sources_before'].items():
            if e['path'] != path or not re.fullmatch('[0-9a-f]{64}', e['sha256']) or type(e['size']) is not int or e['size'] < 0:
                return False
        engines = ('reference',) if args.reference_only else ('reference', 'native')
        if set(report['executables']) != set(engines) or report['executables']['reference']['sha256'] != REFERENCE_SHA256:
            return False
        if not args.reference_only and (args.native_sha256 in (OLD_NATIVE, PRE_FIX_DEBUG_SHA) or not re.fullmatch('[0-9a-f]{64}', args.native_sha256 or '') or report['executables']['native']['sha256'] != args.native_sha256):
            return False
        root = Path(report['scratch_directory'])
        workspace = root / 'workspace'
        ref = DirectReference(args.reference, workspace, root, None, SUITE)
        receipt = report['cleanup_receipt']
        if (receipt != dict(path=str(root), removed=True, pids={str(p): False for p in report['owned_pids']}, remaining_pidfiles=[])
                or not report['owned_pids'] or any(type(p) is not int or p <= 1 for p in report['owned_pids'])):
            return False
        if report['registry_before'] != report['registry_after'] or set(report['registry_before']) != {str(root / 'registry' / p.relative_to(REGISTRY)) for p in REGISTRY.rglob('*') if p.is_file()}:
            return False
        for path, e in report['registry_before'].items():
            original = str(REGISTRY / Path(path).relative_to(root / 'registry'))
            if not valid_evidence(e) or e['sha256'] != report['sources_before'][original]['sha256'] or e['path'] != path:
                return False
        if report['platforms_distfile']['sha256'] != PLATFORM_SHA or not valid_evidence(report['platforms_distfile']):
            return False
        runs = report['runs']
        if len(runs) != report['plan']['commands'] or [r['id'] for r in runs] != list(range(len(runs))):
            return False
        binding = digest(json.dumps(dict(executables=report['executables'], source=report['official_source_before'],
            production=report['sources_before']), sort_keys=True).encode())
        for r in runs:
            engine = next((e for e in engines if str(getattr(args, e)) == r['argv'][0]), None)
            if engine is None or r['executable_sha256'] != report['executables'][engine]['sha256'] or r['preflight_binding_sha256'] != binding or 'environment' in r or 'environment_keys' in r:
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(r[field+'_base64'], validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != r[field]):
                    return False
        for r, name, argv, timeout in [(runs[0], 'reference.version', ref.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], SUITE+'.shutdown', ref.startup+['shutdown']+list(COMMON_FLAGS), min(args.timeout, 15))]:
            if r['name'] != name or r['argv'] != argv or r['cwd'] != str(workspace) or r['timeout_seconds'] != timeout:
                return False
            success(r)
        if runs[0]['stdout'].strip() != 'bazel 9.3.0':
            return False
        expected = [(c['name'], p, e) for c in CASEPLAN for p, e in phases_for(c, args.reference_only)]
        if [(p['family'], p['probe'], p['engine']) for p in report['phases']] != expected or report['completed_phases'] != len(expected):
            return False
        used = [0]
        for p in report['phases']:
            validate_phase(p, report, args, workspace, ref)
            used.extend(p['runs'])
        return used+[len(runs)-1] == list(range(len(runs)))
    except (KeyError, ValueError, TypeError, IndexError, AttributeError, StopIteration, OSError):
        return False


def main(argv=None):
    args = argument_parser().parse_args(argv)
    if args.reference_only and args.output == ROOT / 'artifacts/starlark-sets-validation.json':
        args.output = ROOT / 'artifacts/starlark-sets-membership-reference.json'
    for key, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, key, value.expanduser().resolve())
    if (args.output in [args.native, args.reference, args.platforms_distfile, *SOURCES, *PRESERVED_REPORTS]
            or args.output.name.endswith('-contract.json') or args.output.is_relative_to(args.reference_source)
            or any(args.output.is_relative_to(ROOT / p) for p in ('src', 'scripts', 'tests', 'resources', 'vendor', '.direnv'))):
        raise ValueError('report cannot overwrite source, executable or original contracts')
    report = dict(schema_version=1, status='FAIL', runs=[], phases=[], errors=[], cleanup=[], owned_pids=[],
        planned_checks=CASEPLAN, plan=plan_receipt(args.reference_only), limitations=LIMITATIONS,
        known_reference_defects=KNOWN_REFERENCE_DEFECTS, integration_origin=INTEGRATION_ORIGIN,
        known_parity_gaps=KNOWN_PARITY_GAPS,
        native_claim=dict(status='NOT_RUN' if args.reference_only else 'EXECUTED_BOUNDED_PROFILE',
            commands=sum(len(steps_for(c['name'], p, e)) for c in CASEPLAN for p, e in phases_for(c, args.reference_only) if e == 'native'),
            pass_claim=not args.reference_only),
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
    root = ref = None
    runner = RecordingRunner(report, args)
    interrupted = False
    try:
        report['sources_before'] = source_bindings()
        report['executables'] = executable_evidence(args)
        report['official_source_before'] = source_evidence(args.reference_source)
        report['platforms_distfile'] = evidence(args.platforms_distfile)
        if report['platforms_distfile']['sha256'] != PLATFORM_SHA:
            raise ValueError('genuine platforms distfile differs')
        root = Path(tempfile.mkdtemp(prefix=SUITE+'-', dir=args.scratch_parent))
        report['scratch_directory'] = str(root)
        workspace = root / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text('module(name="set_contract")\n')
        shutil.copytree(REGISTRY, root / 'registry')
        report['registry_before'] = snapshot(p for p in (root / 'registry').rglob('*') if p.is_file())
        (root / 'distdir').mkdir()
        (root / 'distdir/platforms-1.0.0.tar.gz').write_bytes(args.platforms_distfile.read_bytes())
        ref = DirectReference(args.reference, workspace, root, runner, SUITE)
        r = runner.run('reference.version', ref.startup+['version', '--gnu_format'], workspace)
        if success(r).strip() != 'bazel 9.3.0':
            raise ValueError('wrong reference release')
        remember_pid(ref, report)
        for item in CASEPLAN:
            for probe, engine in phases_for(item, args.reference_only):
                phase = run_phase(args, runner, root, workspace, ref, item['name'], probe, engine)
                report['phases'].append(phase)
                try:
                    validate_phase(phase, report, args, workspace, ref)
                except Exception as exc:
                    phase.update(status='FAIL', error=phase.get('error') or str(exc))
                print(item['name'], probe, engine, phase['status'], phase.get('error', ''), flush=True)
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        if ref is not None:
            try:
                remember_pid(ref, report)
                success(runner.run(SUITE+'.shutdown', ref.startup+['shutdown']+list(COMMON_FLAGS), ref.workspace, timeout=min(args.timeout, 15)))
                report['registry_after'] = snapshot(p for p in (root / 'registry').rglob('*') if p.is_file())
                pids = {str(p): Path('/proc', str(p)).exists() for p in report['owned_pids']}
                remaining = sorted(str(p) for p in root.rglob('server.pid.txt'))
                if any(pids.values()) or remaining:
                    raise ValueError('owned PID/server files remain; scratch retained')
                shutil.rmtree(root)
                report['cleanup_receipt'] = dict(path=str(root), removed=not root.exists(), pids=pids, remaining_pidfiles=remaining)
            except Exception as exc:
                report['errors'].append('cleanup: '+str(exc))
        try:
            report['sources_after'] = source_bindings()
            report['executables_after'] = executable_evidence(args)
            report['official_source_after'] = source_evidence(args.reference_source)
        except Exception as exc:
            report['errors'].append('final bindings: '+str(exc))
        report['completed_phases'] = sum(p['status'] in ('PASS', 'KNOWN_REFERENCE_DEFECT') for p in report['phases'])
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report['membership_gap_closure'] = membership_closure(report, args.reference_only)
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS_ONLY_BOUNDED_PROFILE'
        utility.write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return 130 if interrupted else 0 if report['status'] != 'FAIL' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
