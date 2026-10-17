#!/usr/bin/env python3
"""Strict I0 subrule matrix plus the N24 configuration family; never a full Bazel parity claim.

Original research receipts are immutable, not runtime dependencies. Shared scope
fixtures below are explicitly named derivatives: only their unused local platform
target is omitted (the genuine pinned platforms archive supplies the reference
platform). In particular case01's fragment read is NOT removed or relabelled.
Reference preparation never executes native. Full acceptance needs a NEW frozen
ELF and stable complete source bindings. Error restoration is SOURCE_DERIVED on
Java; separate native host tests must inspect restored state after callback errors.
"""

import base64
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import validate_starlark_sets as shared

utility, common, direct = shared.utility, shared.common, shared.direct
ROOT, COMMON_FLAGS = shared.ROOT, shared.COMMON_FLAGS
digest, evidence, snapshot = shared.digest, shared.evidence, shared.snapshot
valid_evidence, success, DirectReference = shared.valid_evidence, shared.success, shared.DirectReference
REGISTRY, REFERENCE_SHA256, SOURCE_PIN = shared.REGISTRY, shared.REFERENCE_SHA256, shared.SOURCE_PIN
PLATFORM_SHA = shared.PLATFORM_SHA
SUITE = 'subrules'
OLD_NATIVE = 'c0315888f6d531d2ab36dffda7a42482b29cf6de8f282c60ff36f01132b25882'
ORIGINS = {
    'artifacts/subrule-contract.json': '8880915f7cd34f024a847730c81c6568e2e631ac0c7ccf43442bcf50f161f964',
    'artifacts/subrule-scope-contract.json': '48f5a3bbd62f7bee3ce206d5f1d6df2a75670441cbd37558daa3a75ea9ccf0e5',
    'artifacts/subrule-args-factory-contract.json': '355b0ecffd91a17809d171e6e381f1b19d4eecaf0a22be6f3cbaabc9c4dbbab9',
    'artifacts/actions-write-args-contract.json': 'bcffab813e7f1f68b9ce45ebeea054b662a37fd20c016903edd056a75ea9f302',
}
OFFICIAL_FILES = {
    'src/main/java/com/google/devtools/build/lib/starlarkbuildapi/StarlarkRuleFunctionsApi.java': 'd831393e6f96ae3c3effffcd7bee9e5e2a8edf3acdfae7cbbafe302dda18f306',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkRuleClassFunctions.java': '9f7adf40e847abbe26eb8ffe6c6ef476ffd08a6b657290226bd0f7a1990ea496',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkSubrule.java': '9d2115fdf86f1807abaf0405d3a5b36fbb3d9f8abd87aa82440f72e6e46657b6',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkRuleContext.java': '5200266852f65ca66a958a3adaf82a29f9b5cbbd1a604a4e91d7815476985072',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkActionFactory.java': 'ec6cd25ae3e955a73aae9b1c59ab51862baebec937b6d83150ea4ff2d58ec1cf',
    'src/main/java/com/google/devtools/build/lib/packages/semantics/BuildLanguageOptions.java': '2a9bb340d6ba8ed965e1db2c5e5b47c184fac0e30af305ff60e9a7baedb59333',
    'src/main/java/com/google/devtools/build/lib/starlarkbuildapi/StarlarkActionFactoryApi.java': '20c59a65a42bc63eb180b62c6fd91467e4cd10de1deae16ae8256268dc9ec750',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/Args.java': 'ac704917bb3d6814fdb6f642c42d9300d9cac1d6fc624d769d3d41e42225ef1b',
    'src/main/java/com/google/devtools/build/lib/actions/ParameterFile.java': '68d3e5d07f9b0783d98d29feb54b45e4c13dfd9f0e9529f2e04fb4e5a35d8b72',
    'src/main/java/com/google/devtools/build/lib/util/ShellEscaper.java': 'd6f6051220e0c9fbb0153b31b4bfa287a9fc1800eedd4ffff9c4d8d2e3199564',
    'src/main/java/com/google/devtools/build/lib/analysis/actions/ParameterFileWriteAction.java': '64f2f912057fa35c35b5bb834a72ec3c5877cfa21fd9db0e6fbce73ed25e6370',
    'src/main/java/com/google/devtools/build/lib/vfs/OutputPermissions.java': 'c9cb7a4ca61f5b4802ecf395b909a5f267145fd80c53e4892774287792078d6b',
    # N24 configuration family: subrule attrs/toolchains/fragments, late-bound defaults.
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkLateBoundDefault.java': '68f54ff5291c2a2f38739fb5fec40671350f52eb0f493533d48e1de4e37c5abe',
    'src/main/java/com/google/devtools/build/lib/rules/cpp/CppConfiguration.java': 'd0c0fae644272fa8992e44461e8ec2f6655681e370b41594954f4f8f1adf9a71',
    'src/main/java/com/google/devtools/build/lib/rules/apple/AppleConfiguration.java': 'ff9f606dba2dddaa63945136da4638621063bc8f297adccdbeaf23e78ed8e2d5',
    'src/main/java/com/google/devtools/build/lib/rules/apple/AppleCommandLineOptions.java': 'ec8dd72422033bd2a2e76492ade2e26d6c5d2795016ad4a0a3fe278f55e64f6d',
    'src/main/java/com/google/devtools/build/lib/analysis/test/CoverageConfiguration.java': '9f1759880b3e5367d0ffe3fabbb196dc21d837abf0fe00f5e4537a154e1a27c8',
    'src/main/java/com/google/devtools/build/lib/rules/java/JavaConfiguration.java': '23f67a5d8f447043fb9c834fb3a861c37a4d52bdcd77301c735f01a8d6a6de74',
    'src/main/java/com/google/devtools/build/lib/packages/AttributeProvider.java': '995e75ea72f98dfe3e69f3a2b4a95808f064dd66273f6ff15a49a9391c00b046',
    'src/main/java/com/google/devtools/build/lib/packages/BuiltinRestriction.java': '383c157100c35564c11ddcc270f7a2757c9f151f343ac0ce6253bffb5dfb5081',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkAttributesCollection.java': '9b3b300d7e9c25dceafc8a9450dd2511f9b0b83088e11421b6dc3b5086cc7442',
    'src/main/java/com/google/devtools/build/lib/analysis/CommonPrerequisiteValidator.java': '473f263c43f0122a7da1f56bdb95b00f35148d09d16722c27d2e34cc32119ccb',
    'src/main/java/com/google/devtools/build/lib/packages/AttributeValueSource.java': 'c0e66478a7d920e8291cd93e5860c2aec6c1eec1da624103185840073d9d1cb9',
}
# Evaluated on each invocation, not a stale list captured before core handoff.
def source_paths():
    return sorted(set([Path(__file__).resolve(), ROOT / 'tests/test_validate_subrules.py',
        ROOT / 'Cargo.toml', ROOT / 'Cargo.lock', ROOT / 'vendor/starlark-0.14.2.provenance.json',
        ROOT / 'tests/fixtures/builtin_modules/provenance.json',
        *ROOT.glob('scripts/*.py'), *ROOT.glob('src/**/*.rs'), *ROOT.glob('tests/*.rs'),
        *(p for p in (ROOT / 'tests/fixtures/subrules').rglob('*') if p.is_file()),
        *ROOT.glob('vendor/starlark-0.14.2/**/*.rs'), *ROOT.glob('vendor/starlark-0.14.2/**/Cargo.toml'),
        *(p for p in REGISTRY.rglob('*') if p.is_file()),
        *(p for p in (ROOT / 'resources').rglob('*') if p.is_file())]))


CASEPLAN = [
    common.case('constructor', ('metadata', 'unhashable', 'factory-unhashable', 'positional', 'doc', 'unknown', 'missing',
        'len-implementation', 'native-implementation', 'callable-implementation',
        'bad-subrules', 'bad-rule-subrules', 'loading-call', 'unexported')),
    common.case('globals', ('build', 'module', 'ordinary-bzl', 'module-bzl', 'native-field')),
    common.case('functional', ('factory-two-action', 'zeroattrs-no-config-views')),
    common.case('permissions', ('transitive-direct', 'nested-undeclared')),
    common.case('context', ('surface', 'structural', 'locked_direct', 'locked_getattr', 'stale_same')),
    common.case('receivers', ('parent_declare', 'parent_write', 'parent_bound', 'escaped_ctx', 'escaped_actions', 'escaped_bound')),
    common.case('args', ('parent_object', 'parent_bound', 'child_object', 'child_bound', 'returned-builder',
        'write-mixed', 'write-empty', 'write-formatted', 'write-mapped', 'write-list', 'write-tuple')),
    # N24: Bazel-faithful subrule attrs/toolchains/fragments and late-bound
    # defaults. Differential (reference + native); NEW fixtures needing a
    # genuine reference recapture before any acceptance claim.
    common.case('configuration', ('lifted-attrs', 'late-bound-rule', 'read-fragments', 'read-toolchains',
        'undeclared-fragment', 'private-cpp-api', 'override', 'public-attr', 'nonlabel-attr', 'no-default',
        'computed-default', 'bad-cfg', 'two-toolchains', 'mandatory-lifted')),
    common.case('boundaries', ('explicit-flag', 'ordinary-setting-visibility', 'lifted-label-list')),
]
DEFECTS = ('child_object', 'child_bound')
KNOWN_REFERENCE_DEFECTS = [dict(id=p, classification='INTENTIONAL_DIVERGENCE',
    reference_status='KNOWN_REFERENCE_DEFECT', reference_returncode=37,
    signature='NullPointerException: starlarkRuleContext null at SubruleContext.getStarlarkSemantics -> StarlarkActionFactory.args',
    native_result='CONTROLLED_EXPIRED_CONTEXT_ERROR_BEFORE_ALLOCATION', parity_claim=False,
    evidence='CURRENT_EXECUTION_PRIVATE_BATCH; original Args-factory receipt unchanged',
    marker_limit='Buffered BEFORE_FACTORY may be absent on genuine crash; analysis stack and fixture line bind the phase') for p in DEFECTS]
LIMITATIONS = [
    'PASS_ONLY_BOUNDED_PROFILE: lifted private label/label_list subrule attrs (target cfg or a None value), declared cpp fragment members, the non-AEG subrule ctx.toolchains error; no exec-cfg values, AEG toolchains, aspects/inheritance or run/test SDK.',
    'Expired child Args factories are REQUIRED KNOWN_REFERENCE_DEFECT / INTENTIONAL_DIVERGENCE, never ordinary guard parity PASS.',
    'Still-live ancestor Args factories are allowed; returned Args builders survive until root analysis ends.',
    'actions.write(Args) checks exact SHELL_QUOTED UTF-8/LF bytes, including empty Args versus one empty argument. Final 0555 modes do not mean is_executable was ignored.',
    'Existing native map_each eagerly evaluates at Args mutation; Bazel defers it to write execution. write-mapped compares successful bytes ONLY, not callback timing. The recorded reference mapper_failure --nobuild/build phase difference is not a native parity case.',
    'set_param_file_format/use_param_file are supported since N23d (docs/ACTIONS_PARAMS.md, paired receipt tests/fixtures/actions_params/reference_receipt.json); they are no longer subrule boundary probes.',
    'Explicit experimental flag is a native-only unsupported boundary, not Bazel allowlist parity.',
    'configuration family (N24) is differential but NEW: it needs a genuine reference recapture; no prior receipt covers it.',
    'Late-bound fields resolve only where every native configuration has Bazel\'s option default (cpp FDO/malloc/libc/proto/zipper, coverage.output_generator, java launcher/proguard/local optimization: None; apple.xcode_config_label: @bazel_tools//tools/cpp:host_xcodes); others stay explicit analysis gaps.',
    'Unknown fragment names give the interpreter missing-attribute error, not Bazel\'s "There is no configuration fragment named" text. Not compared.',
    'Lifted attr.label_list crashes Bazel 9.3 (InvalidStarlarkValueException); native fails explicitly (boundaries/lifted-label-list), never differential.',
    'Subrule ctx.toolchains positive path needs automatic exec groups, which the engine rejects before analysis; only the non-AEG Bazel error is compared.',
    'Original case01 reads dir(ctx.fragments) and remains outside I0. Named zeroattrs-no-config-views derives from scope contract seed-data fixture, not case01.',
    'Scope/Args fixture derivatives omit only unused local_platform BUILD declaration and use genuine pinned platforms distfile; original receipts never rewritten.',
    'Ordinary ctx full dir is not compared. Bazel ordinary ctx has build_setting_value structurally; native intentionally omits it and checks this only in ordinary-setting-visibility. Subrule ctx dir is exactly actions/fragments/label/toolchains.',
    'Java post-error scope restoration is SOURCE_DERIVED; native host tests, not invented Starlark catch/continuation, verify it.',
    'Root ctx/current actions are native nonfreezable profile; no cross-analysis persisted-context parity claim.',
    'Earlier H2 safe self-XOR ConcurrentModificationException gap remains intentional divergence, not an I0 runtime feature case or this Args NPE.',
    'Reference-only preparation cannot claim native execution or close any native gate.',
]
REPORT_KEYS = set('schema_version status runs phases errors cleanup owned_pids owned_batch_pids planned_checks plan limitations known_reference_defects origins configuration started_at sources_before sources_after executables executables_after official_source_before official_source_after platforms_distfile scratch_directory registry_before registry_after cleanup_receipt completed_phases finished_at native_claim'.split())

SCOPE_HEADER = 'def _sub(ctx, parent=None, mode="", old=None, receiver=None, out=None):\n'
SEED = ('f=ctx.actions.declare_file(ctx.label.name+".seed")\n'
    'ctx.actions.write(f,"seed-data\\n")\nprint("SUBRULE_ENTER",str(ctx.label))\n')
SCOPE_CTOR = 'S=subrule(implementation=_sub,attrs={},toolchains=[],fragments=[],subrules=[])\n'

FACTORY = '''P=provider(fields=["file"])
def factory(prefix):
    def implementation(ctx, source, suffix="!"):
        print("I0_LEAF")
        if prefix+suffix != "seed-!" or ctx.label.package != "consumer":
            fail("BAD_CLOSURE_OR_OWNER")
        out=ctx.actions.declare_file(ctx.label.name+".generated")
        if out.owner != ctx.label:
            fail("BAD_FILE_OWNER")
        ctx.actions.run_shell(inputs=[source],outputs=[out],arguments=[source.path,out.path],command='cat "$1" > "$2"',mnemonic="I0SubruleProducer")
        return P(file=out)
    return subrule(implementation=implementation,attrs={},toolchains=[],fragments=[],subrules=[])
S=factory("seed-")
Alias=S
def _outer(ctx, source, suffix="!"):
    print("I0_OUTER")
    return Alias(source,suffix=suffix)
Outer=subrule(implementation=_outer,subrules=(S,))
'''
CONSUMER = '''load("//owner:defs.bzl","Outer","S","Alias","P")
def _rule(ctx):
    print("I0_RULE")
    if S != Alias or repr(Alias) != "<subrule S>":
        fail("BAD_FIRST_EXPORT_ALIAS")
    p=Outer(ctx.file.src,suffix="!")
    if type(p.file) != "File" or p.file.owner != ctx.label:
        fail("BAD_PROVIDER_FILE")
    out=ctx.actions.declare_file(ctx.label.name+".out")
    ctx.actions.run_shell(inputs=[p.file],outputs=[out],arguments=[p.file.path,out.path],command='cat "$1" > "$2"',mnemonic="I0SubruleConsumer")
    return [DefaultInfo(files=depset([out])),p]
r=rule(implementation=_rule,attrs={"src":attr.label(allow_single_file=True)},subrules=[Outer])
'''

# Exact defs.bzl from actions-write-args-contract.json, copied into tracked
# source so neither fixture generation nor portable units need ignored reports.
# mapper_failure is preserved in the source, but deliberately NOT in CASEPLAN:
# its deferred Java/eager native timing is an existing bounded-profile difference.
WRITE_ARGS = r'''def _mapper(value):
    print("MAP_CALLBACK", value)
    if value == "skip":
        return None
    if value == "two":
        return ["x y", "q'z"]
    return value

def _bad_mapper(value):
    fail("MAP_FAILURE_EXECUTION_MARKER")

def _impl(ctx):
    a = ctx.actions.args()
    name = ctx.label.name
    if name == "mixed":
        for value in ["plain", "space value", "single'quote", "double\"quote", "back\\slash", "", "line1\nline2", "雪é🙂"]:
            a.add(value)
    elif name == "formatted":
        a.add("v w", format="pre:%s:%%")
        a.add_all(["a", "b c", ""], format_each="<%s>", before_each="--value", terminate_with="--end")
    elif name == "mapped":
        a.add_all(["skip", "two", "z"], map_each=_mapper, format_each="[%s]")
    elif name == "mapper_failure":
        a.add_all(["x"], map_each=_bad_mapper)
    outputs = []
    out = ctx.actions.declare_file(name + ".out")
    ctx.actions.write(out, a)
    outputs.append(out)
    if name == "mixed":
        executable = ctx.actions.declare_file(name + ".exec")
        ctx.actions.write(executable, a, is_executable=True)
        outputs.append(executable)
    print("AFTER_WRITE", name)
    return [DefaultInfo(files=depset(outputs))]

r = rule(implementation=_impl)
'''
WRITE_OUTPUTS = {
    'mixed': bytes.fromhex('706c61696e0a2773706163652076616c7565270a2773696e676c65275c272771756f7465270a27646f75626c652271756f7465270a276261636b5c736c617368270a27270a276c696e65310a6c696e6532270a27e99baac3a9f09f9982270a'),
    'empty': b'',
    'formatted': b"'pre:v w:%'\n--value\n'<a>'\n--value\n'<b c>'\n--value\n'<>'\n--end\n",
    'mapped': b"'[x y]'\n'[q'\\''z]'\n'[z]'\n",
}


def write_result(expression='"ok\\n"'):
    return ('out=ctx.actions.declare_file("consume.out")\nctx.actions.write(out,'+expression+')\n'
        'print("I0_ACTION_READY")\nreturn [DefaultInfo(files=depset([out]))]\n')


def scope_fixture(probe):
    """Same genuine scope callbacks; explicit shared BUILD-platform derivative."""
    sub, body = SEED, 'f=S()\nreturn [DefaultInfo(files=depset([f]))]\n'
    expected, actions, diagnostic = {probe+'.seed': b'seed-data\n'}, 1, None
    if probe == 'zeroattrs':
        sub += 'return f\n'
    elif probe in ('locked_direct', 'locked_getattr'):
        sub += ('if mode=="locked_direct":\n    x=parent.label\nelse:\n    x=getattr(parent,"label",None)\n'
            'print("GUARD_BYPASSED",x)\nreturn f\n')
        body = 'f=S(parent=ctx,mode=ctx.label.name)\nreturn [DefaultInfo(files=depset([f]))]\n'
        diagnostic = 'parent-label'
    elif probe == 'structural':
        sub += ('content=str(hasattr(parent,"label"))+"|"+str("label" in dir(parent))+"|"+getattr(parent,"not_a_field","fallback")+"\\n"\n'
            'out=ctx.actions.declare_file(ctx.label.name+".out")\nctx.actions.write(out,content)\nreturn [f,out]\n')
        body = 'files=S(parent=ctx)\nreturn [DefaultInfo(files=depset(files))]\n'
        expected[probe+'.out'] = b'True|True|fallback\n'
        actions = 2
    elif probe in ('parent_declare', 'parent_write', 'parent_bound'):
        sub += ('receiver(out,"forbidden")\n' if probe == 'parent_bound' else
            'if mode=="parent_declare":\n    receiver.declare_file("forbidden")\nelse:\n    receiver.write(out,"forbidden")\n')
        sub += 'print("GUARD_BYPASSED")\nreturn f\n'
        body = ('out=ctx.actions.declare_file(ctx.label.name+".parent")\nwrite=ctx.actions.write\nf=S(receiver=write,out=out)\n' if probe == 'parent_bound' else
            'actions=ctx.actions\nout=actions.declare_file(ctx.label.name+".parent")\nf=S(receiver=actions,out=out,mode=ctx.label.name)\n')
        body += 'return [DefaultInfo(files=depset([f]))]\n'
        diagnostic = 'parent-declare' if probe == 'parent_declare' else 'parent-write'
    elif probe == 'escaped_ctx':
        sub += 'return ctx\n'
        body = 'escaped=S()\nx=escaped.label\nprint("GUARD_BYPASSED",x)\nreturn []\n'
        diagnostic = 'child-label'
    elif probe in ('escaped_actions', 'escaped_bound'):
        sub += 'if mode=="escaped_actions":\n    return ctx.actions\nreturn ctx.actions.write\n'
        body = ('out=ctx.actions.declare_file(ctx.label.name+".parent")\nescaped=S(mode=ctx.label.name)\n'
            'if ctx.label.name=="escaped_actions":\n    escaped.write(out,"forbidden")\nelse:\n    escaped(out,"forbidden")\n'
            'print("GUARD_BYPASSED")\nreturn []\n')
        diagnostic = 'child-write'
    elif probe == 'stale_same':
        sub = 'if old!=None:\n    x=old.label\n    print("GUARD_BYPASSED",x)\n    return None\n'+sub+'return ctx\n'
        body = 'old=S()\nS(old=old)\nreturn []\n'
        diagnostic = 'child-label'
    elif probe == 'args_escape':
        sub += 'args=ctx.actions.args()\nargs.add("from-subrule")\nreturn struct(file=f,args=args)\n'
        body = ('result=S()\nresult.args.add("from-parent")\nout=ctx.actions.declare_file(ctx.label.name+".out")\n'
            'ctx.actions.write(out,result.args)\nreturn [DefaultInfo(files=depset([result.file,out]))]\n')
        expected[probe+'.out'] = b'from-subrule\nfrom-parent\n'
        actions = 2
    else:
        raise ValueError('unknown scope fixture')
    defs = SCOPE_HEADER+common.indent(sub)+SCOPE_CTOR+'def _rule(ctx):\n'+common.indent(body)+'r=rule(implementation=_rule,subrules=[S])\n'
    return defs, expected, actions, diagnostic


def args_factory(probe):
    # Exact original defs.bzl bytes, including the line-7 crash callsite.
    action = ('out=ctx.actions.declare_file(ctx.label.name+".out")\n'
        'ctx.actions.run_shell(outputs=[out],arguments=[args],command="printf \'%s\\\\n\' \\\"$@\\\" > \'"+out.path+"\'")\n')
    create = ('print("BEFORE_FACTORY", "'+probe+'")\nargs=receiver'+('.args()' if probe.endswith('object') else '()')+'\n'
        'args.add("'+probe+'")\nprint("AFTER_FACTORY", "'+probe+'")\n'+action)
    if probe.startswith('parent'):
        sub = create+'return out\n'
        body = 'receiver=ctx.actions'+('.args' if probe.endswith('bound') else '')+'\nout=S(receiver=receiver)\n'
    else:
        sub = 'return ctx.actions'+('.args' if probe.endswith('bound') else '')+'\n'
        body = 'receiver=S()\n'+create
    return ('def _sub(ctx,receiver=None):\n'+common.indent(sub)+SCOPE_CTOR+'def _rule(ctx):\n'
        +common.indent(body+'return [DefaultInfo(files=depset([out]))]\n')+'r=rule(implementation=_rule,subrules=[S])\n')


CONFIGURATION_OUTPUTS = {
    'lifted-attrs': b'outer|data|data.txt|None|Target|File|[]\n',
    'late-bound-rule': b'(None, None, None, None, [])\n',
    'read-fragments': b'fragments\n',
}
CONFIGURATION_REJECTIONS = {
    'read-toolchains': 'analysis', 'undeclared-fragment': 'analysis', 'private-cpp-api': 'analysis', 'override': 'analysis',
    'public-attr': 'loading', 'nonlabel-attr': 'loading', 'no-default': 'loading', 'computed-default': 'loading',
    'bad-cfg': 'loading', 'two-toolchains': 'loading', 'mandatory-lifted': 'loading',
}
# No lifted label_list: Bazel 9.3 crashes passing one to the implementation
# (paired capture A2/lifted/check, exit 37 InvalidStarlarkValueException
# Lists$TransformingRandomAccessList); see the native-only boundary
# lifted-label-list. The unused NoneCfg subrule checks that subrule() accepts
# config.none() (a tool transition, NoConfigTransition.java:96-99; capture
# B1b/cfg_none).
LIFTED_LEAF = ('def _leaf(ctx,word,*,_dep,_single,_late):\n    print("I0_CHILD")\n'
    '    return "|".join([word,_dep.label.name,_single.basename,repr(_late),type(_dep),type(_single)])\n'
    'Leaf=subrule(implementation=_leaf,attrs={"_dep":attr.label(default=":data"),'
    '"_single":attr.label(default=":data.txt",allow_single_file=True),'
    '"_late":attr.label(default=configuration_field(fragment="cpp",name="fdo_optimize"))})\n'
    'NoneCfg=subrule(implementation=_leaf,attrs={"_n":attr.label(default=":data",cfg=config.none())})\n'
    'def _outer(ctx):\n    return Leaf("outer"%s)\nOuter=subrule(implementation=_outer,subrules=[Leaf])\n')


def configuration_fixture(probe, files):
    """(prefix, rule_args, body) of one N24 probe (StarlarkSubrule.java, StarlarkRuleClassFunctions.subrule)."""
    rule_args = ',subrules=[S]'
    body = 'S()\n'+write_result()
    child = 'def _sub(ctx):\n    print("I0_CHILD")\n'
    if probe in ('lifted-attrs', 'override'):
        files['BUILD.bazel'] += 'filegroup(name="data",srcs=["data.txt"])\n'
        files['data.txt'] = 'data\n'
        prefix = LIFTED_LEAF % (',_dep=None' if probe == 'override' else '')
        rule_args = ',subrules=[Outer]'
        body = 'names=[n for n in dir(ctx.attr)+dir(ctx.files)+dir(ctx.file) if "%" in n]\n'+write_result('Outer()+"|"+repr(names)+"\\n"')
    elif probe == 'late-bound-rule':
        prefix = ''
        rule_args = (',attrs={"_m":attr.label(default=configuration_field("cpp","custom_malloc")),'
            '"_c":attr.label(default=configuration_field("coverage","output_generator"),cfg="exec"),'
            '"_z":attr.label(default=configuration_field("cpp","zipper"),cfg="exec",allow_single_file=True)}')
        body = write_result('repr((ctx.attr._m,ctx.attr._c,ctx.attr._z,ctx.file._z,ctx.files._m))+"\\n"')
    elif probe == 'read-fragments':
        prefix = child+'    return type(ctx.fragments)\nS=subrule(implementation=_sub,fragments=["cpp"])\n'
        body = write_result('S()+"\\n"')
    elif probe == 'read-toolchains':
        prefix = child+'    x=ctx.toolchains\n    print("GUARD_BYPASSED")\nS=subrule(implementation=_sub)\n'
    elif probe == 'undeclared-fragment':
        prefix = child+'    x=ctx.fragments.cpp\n    print("GUARD_BYPASSED")\nS=subrule(implementation=_sub,fragments=["java"])\n'
    elif probe == 'private-cpp-api':
        prefix = child+'    x=ctx.fragments.cpp.compilation_mode()\n    print("GUARD_BYPASSED")\nS=subrule(implementation=_sub,fragments=["cpp"])\n'
    else:
        option = {'public-attr': 'attrs={"dep":attr.label(default=":dep")}',
            'nonlabel-attr': 'attrs={"_dep":attr.string(default="x")}',
            'no-default': 'attrs={"_dep":attr.label()}',
            'computed-default': 'attrs={"_dep":attr.label_list(default=lambda: [])}',
            'bad-cfg': 'attrs={"_dep":attr.label(default=":dep",cfg=T)}',
            'two-toolchains': 'toolchains=["//:a","//:b"]',
            'mandatory-lifted': 'attrs={"_dep":attr.label(mandatory=True,default=":dep")}'}[probe]
        if probe == 'mandatory-lifted':
            files['BUILD.bazel'] += 'filegroup(name="dep")\n'
        prefix = 'def _sub(ctx,**kwargs):\n    return None\n'
        if probe == 'bad-cfg':
            # Only Starlark transitions are rejected (StarlarkRuleClassFunctions.java:2177-2181).
            prefix += 'def _t(settings,attr):\n    return {}\nT=transition(implementation=_t,inputs=[],outputs=[])\n'
        prefix += 'S=subrule(implementation=_sub,'+option+')\n'
    return prefix, rule_args, body


def fixture(family, probe):
    if not any(c['name'] == family and probe in c['probes'] for c in CASEPLAN):
        raise ValueError('unknown planned probe')
    files = {'MODULE.bazel': 'module(name="subrule_i0")\n',
        'BUILD.bazel': 'load(":defs.bzl","r")\nr(name="consume")\n'}
    v = dict(target='//:consume', expected={'consume.out': b'ok\n'}, actions=1, diagnostic=None,
        changed=None, markers=['I0_RULE', 'I0_ACTION_READY'], stage='analysis',
        native_only=family == 'boundaries', defect=family == 'args' and probe in DEFECTS,
        spawns=[], origin='NEW_I0_NAMED_FIXTURE')
    prefix = 'def _sub(ctx):\n    return None\nS=subrule(implementation=_sub)\n'
    body = 'S()\n'+write_result()
    rule_args = ',subrules=[S]'
    if family == 'functional' and probe == 'factory-two-action':
        files = {'MODULE.bazel': files['MODULE.bazel'], 'BUILD.bazel': '', 'owner/BUILD.bazel': '',
            'owner/defs.bzl': FACTORY, 'consumer/defs.bzl': CONSUMER,
            'consumer/BUILD.bazel': 'load(":defs.bzl","r")\nr(name="consume",src="seed.txt")\n',
            'consumer/seed.txt': 'seed-A!\n'}
        v.update(target='//consumer:consume', expected={'consume.out': b'seed-A!\n'}, actions=2,
            changed=('consumer/seed.txt', b'seed-B!\n', {'consume.out': b'seed-B!\n'}),
            markers=['I0_RULE','I0_OUTER','I0_LEAF'], spawns=['I0SubruleProducer','I0SubruleConsumer'])
    elif (family == 'functional' or family == 'receivers' or
            family == 'context' and probe != 'surface' or family == 'args' and probe == 'returned-builder'):
        name = 'zeroattrs' if family == 'functional' else 'args_escape' if probe == 'returned-builder' else probe
        defs, expected, actions, diagnostic = scope_fixture(name)
        files.update({'MODULE.bazel': 'module(name="subrule_scope_contract")\n', 'defs.bzl': defs,
            'BUILD.bazel': 'load(":defs.bzl","r")\nr(name="'+name+'")\n'})
        v.update(target='//:'+name, expected=expected, actions=actions, diagnostic=diagnostic,
            markers=['SUBRULE_ENTER @@//:'+name], origin='SCOPE_CONTRACT_SHARED_PLATFORM_DERIVATIVE')
    elif family == 'args' and probe.startswith('write-'):
        name = probe[6:]
        if name in WRITE_OUTPUTS:
            files.update({'MODULE.bazel': 'module(name="actions_write_args_contract")\n', 'defs.bzl': WRITE_ARGS,
                'BUILD.bazel': 'load(":defs.bzl", "r")\n'+''.join('r(name="'+n+'")\n' for n in (*WRITE_OUTPUTS,'mapper_failure'))})
            expected = {name+'.out': WRITE_OUTPUTS[name]}
            if name == 'mixed':
                expected[name+'.exec'] = WRITE_OUTPUTS[name]
            v.update(target='//:'+name,expected=expected,actions=len(expected),markers=['AFTER_WRITE '+name],
                origin='WRITE_ARGS_CONTRACT_SHARED_PLATFORM_DERIVATIVE')
        else:
            payload = '["not Args"]' if name == 'list' else '("not Args",)'
            body = 'out=ctx.actions.declare_file("rejected.marker")\nctx.actions.write(out,'+payload+')\n'+write_result()
            v.update(diagnostic=probe,markers=['I0_RULE'])
    elif family == 'args':
        files.update({'MODULE.bazel': 'module(name="subrule_args_factory_contract")\n', 'defs.bzl': args_factory(probe),
            'BUILD.bazel': 'load(":defs.bzl","r")\nr(name="'+probe+'")\n'})
        v.update(target='//:'+probe, expected={probe+'.out': (probe+'\n').encode()},
            markers=['BEFORE_FACTORY '+probe, 'AFTER_FACTORY '+probe], spawns=['Action'],
            origin='ARGS_FACTORY_CONTRACT_SHARED_PLATFORM_DERIVATIVE')
        if v['defect']:
            v.update(diagnostic='child-args', markers=['BEFORE_FACTORY '+probe])
    elif family == 'constructor':
        if probe == 'metadata':
            files['other.bzl'] = 'R=subrule\n'
            prefix = 'load(":other.bzl",R="R")\n'+prefix+'Alias=S\nOther=subrule(implementation=_sub)\n'
            body = write_result('repr((type(subrule),repr(subrule),str(subrule),dir(subrule),R==subrule,type(S),repr(S),repr(Alias),S==Alias,S==Other,dir(S)))+"\\n"')
            v['expected'] = {'consume.out': b'("builtin_function_or_method", "<built-in function subrule>", "<built-in function subrule>", [], True, "Subrule", "<subrule S>", "<subrule S>", True, False, [])\n'}
        elif probe == 'unexported':
            prefix = 'def _sub(ctx):\n    return None\nBOX=[subrule(implementation=_sub)]\n'
            rule_args = ',subrules=BOX'
            body = 'BOX[0]()\n'+write_result()
            # rule(subrules=BOX) validates exports while defining the rule,
            # before _rule can print. This is a genuine loading rejection.
            v.update(diagnostic='unexported',stage='loading')
        else:
            expressions = {'unhashable': '{S:1}', 'factory-unhashable': '{subrule:1}', 'positional': 'subrule(_sub)', 'doc': 'subrule(implementation=_sub,doc="x")',
                'unknown': 'subrule(implementation=_sub,unknown=True)', 'missing': 'subrule()',
                'len-implementation': 'subrule(implementation=len)', 'native-implementation': 'subrule(implementation=native.glob)',
                'callable-implementation': 'subrule(implementation=provider())', 'bad-subrules': 'subrule(implementation=_sub,subrules=[_sub])',
                'bad-rule-subrules': 'rule(implementation=_sub,subrules=[_sub])', 'loading-call': 'S()'}
            prefix += 'X='+expressions[probe]+'\n'
            v.update(diagnostic=probe, stage='loading')
    elif family == 'globals':
        if probe in ('build', 'module'):
            file = 'BUILD.bazel' if probe == 'build' else 'MODULE.bazel'
            files[file] += 'X=subrule\n'
            v.update(diagnostic=probe+'-global', stage='loading')
        elif probe == 'native-field':
            prefix += 'X=native.subrule\n'
            v.update(diagnostic='native-field', stage='loading')
        elif probe == 'module-bzl':
            files['MODULE.bazel'] += 'e=use_extension("//:ext.bzl","ext")\nuse_repo(e,"made")\n'
            files['ext.bzl'] = (prefix+'def _repo(ctx):\n    ctx.file("data",repr(S)+"\\n",executable=False)\n'
                '    ctx.file("BUILD.bazel",\'exports_files(["data"],visibility=["//visibility:public"])\\n\',executable=False)\n'
                'repo=repository_rule(implementation=_repo)\ndef _ext(ctx):\n    print("I0_MODULE_BZL")\n    repo(name="made")\n'
                'ext=module_extension(implementation=_ext)\n')
            files['BUILD.bazel'] = 'load(":defs.bzl","r")\nr(name="consume",src="@made//:data")\n'
            body = ('out=ctx.actions.declare_file("consume.out")\n'
                'ctx.actions.run_shell(inputs=[ctx.file.src],outputs=[out],arguments=[ctx.file.src.path,out.path],command=\'cat "$1" > "$2"\',mnemonic="I0ModuleConsumer")\n'
                'print("I0_ACTION_READY")\nreturn [DefaultInfo(files=depset([out]))]\n')
            rule_args = ',attrs={"src":attr.label(allow_single_file=True)}'
            v.update(expected={'consume.out': b'<subrule S>\n'}, spawns=['I0ModuleConsumer'])
        else:
            body = 'S()\n'+write_result('repr(S)+"\\n"')
            v['expected'] = {'consume.out': b'<subrule S>\n'}
    elif family == 'permissions':
        prefix = ('def _leaf(ctx):\n    print("I0_FORBIDDEN_LEAF")\n    return None\nLeaf=subrule(implementation=_leaf)\n'
            'def _outer(ctx):\n    print("I0_OUTER")\n    return Leaf()\n'
            'Outer=subrule(implementation=_outer'+(',subrules=[Leaf]' if probe == 'transitive-direct' else '')+')\n')
        rule_args = ',subrules=[Outer]'
        body = ('Leaf()' if probe == 'transitive-direct' else 'Outer()')+'\n'+write_result()
        v.update(diagnostic=probe, markers=['I0_RULE']+(['I0_OUTER'] if probe == 'nested-undeclared' else []))
    elif family == 'configuration':
        prefix, rule_args, body = configuration_fixture(probe, files)
        if probe in CONFIGURATION_OUTPUTS:
            v['expected'] = {'consume.out': CONFIGURATION_OUTPUTS[probe]}
            v['markers'] = ['I0_RULE', 'I0_ACTION_READY'] + (['I0_CHILD'] if probe != 'late-bound-rule' else [])
        else:
            v.update(diagnostic=probe, stage=CONFIGURATION_REJECTIONS[probe])
            if v['stage'] == 'analysis':
                v['markers'] = ['I0_RULE'] + (['I0_CHILD'] if probe in ('read-toolchains', 'undeclared-fragment', 'private-cpp-api') else [])
    elif family == 'context':
        prefix = ('def _sub(ctx,parent):\n'
            '    return repr((type(ctx),dir(ctx),hasattr(ctx,"attr"),hasattr(ctx,"outputs")))+"\\n"\n'
            'S=subrule(implementation=_sub)\n')
        body = 'result=S(ctx)\n'+write_result('result')
        v['expected'] = {'consume.out': b'("subrule_ctx", ["actions", "fragments", "label", "toolchains"], False, False)\n'}
    else:
        if probe == 'ordinary-setting-visibility':
            # First genuine preparation observed True for Bazel; not paired
            # parity. Preserve that discovery, and test native's bounded table.
            body = write_result('repr((hasattr(ctx,"build_setting_value"),"build_setting_value" in dir(ctx)))+"\\n"')
            v['expected'] = {'consume.out': b'(False, False)\n'}
        elif probe == 'lifted-label-list':
            # Native-only: Bazel 9.3 crashes (exit 37) on any lifted label_list.
            prefix = ('def _sub(ctx,_deps):\n    print("I0_CHILD")\n    return None\n'
                'S=subrule(implementation=_sub,attrs={"_deps":attr.label_list(default=[])})\n')
            v['markers'] = ['I0_RULE']
        elif probe in ('attrs', 'toolchains', 'fragments'):
            value = {'attrs': '{"_dep":attr.label(default=":data")}', 'toolchains': '["//:toolchain"]', 'fragments': '["cpp"]'}[probe]
            prefix += 'X=subrule(implementation=_sub,'+probe+'='+value+')\n'
            v['stage'] = 'loading'
        elif probe.startswith('read-'):
            prefix = 'def _sub(ctx):\n    print("I0_CHILD")\n    x=ctx.'+probe[5:]+'\nS=subrule(implementation=_sub)\n'
            v['markers'] = ['I0_RULE','I0_CHILD']
        else:
            v['stage'] = 'options'
        if probe != 'ordinary-setting-visibility':
            v['diagnostic'] = probe
    if 'defs.bzl' not in files and not (family == 'functional' and probe == 'factory-two-action'):
        files['defs.bzl'] = prefix+'def _rule(ctx):\n'+common.indent('print("I0_RULE")\n'+body)+'r=rule(implementation=_rule'+rule_args+')\n'
    if v['diagnostic']:
        v.update(expected={}, actions=0, spawns=[])
        if v['stage'] != 'analysis':
            v['markers'] = []
        elif v['markers'] == ['I0_RULE','I0_ACTION_READY']:
            v['markers'] = ['I0_RULE']
    v['files'] = {p: text.encode() for p, text in files.items()}
    return v


DIAGNOSTICS = {
    'unhashable': r"unhashable type[^\n]*Subrule|Subrule[^\n]*not hashable",
    'factory-unhashable': r"unhashable type[^\n]*builtin_function_or_method|builtin_function_or_method[^\n]*not hashable",
    'positional': r'subrule[^\n]*(?:unexpected positional|positional argument|named.only)|Missing named-only parameter `implementation` for call to `subrule`',
    'doc': r'(?:unexpected|unknown|extra)[^\n]*\bdoc\b|\bdoc\b[^\n]*(?:unexpected|unknown|extra named)',
    'unknown': r'(?:unexpected|unknown|extra)[^\n]*\bunknown\b|`unknown` extra named parameter',
    'missing': r'(?:missing|required|mandatory)[^\n]*implementation|implementation[^\n]*(?:missing|required|mandatory)',
    'len-implementation': r'implementation[^\n]*(?:want .function.|must be a Starlark function)',
    'native-implementation': r'implementation[^\n]*(?:want .function.|must be a Starlark function)',
    'callable-implementation': r'implementation[^\n]*(?:want .function.|must be a Starlark function)',
    'bad-subrules': r'subrules[^\n]*(?:want Subrule|only real Subrule)',
    'bad-rule-subrules': r'subrules[^\n]*(?:want Subrule|only real Subrule)',
    'loading-call': r'(?:only be called from a rule or aspect implementation|subrules are only available during rule analysis)',
    'unexported': r'(?:subrule[^\n]*(?:exported|global variable)|invalid subrule hasn.t been exported)',
    'build-global': r"(?:name 'subrule' is not defined|Variable `subrule` not found)",
    'module-global': r"(?:name 'subrule' is not defined|Variable `subrule` not found)",
    'native-field': r"(?:no (?:attribute|field|method|native function or rule)|has no attribute)[^\n]*['\"`]subrule",
    'transitive-direct': r"rule 'r' must declare 'Leaf' in 'subrules'|subrule [^\n]*Leaf must be declared in subrules of its immediate caller",
    'nested-undeclared': r"subrule Outer must declare Leaf in 'subrules'|subrule [^\n]*Leaf must be declared in subrules of its immediate caller",
    **{probe: re.escape(text) for probe, text in {
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
    }.items()},
    'explicit-flag': r'(?:unsupported|unknown|unrecognized)[^\n]*experimental_rule_extension_api',
    'write-list': r"content[^\n]*(?:string|str)[^\n]*Args|content[^\n]*list[^\n]*(?:want|expected)",
    'write-tuple': r"content[^\n]*(?:string|str)[^\n]*Args|content[^\n]*tuple[^\n]*(?:want|expected)",
}
GUARDS = {'parent-label': ('label','rule'), 'parent-declare': ('actions.declare_file','rule'),
    'parent-write': ('actions.write','rule'), 'child-label': ('label','subrule'),
    'child-write': ('actions.write','subrule'), 'child-args': ('actions.args','subrule')}


def require_rejection(r, diagnostic, engine):
    codes = (1,2) if diagnostic == 'explicit-flag' else (1,) if engine == 'native' else (48,) if diagnostic == 'module-global' else (1,7)
    if diagnostic in GUARDS:
        field, kind = GUARDS[diagnostic]
        pattern = (r'analysis scope receiver is locked or no longer active' if engine == 'native' else
            re.escape("cannot access field or method '"+field+"' of "+kind+' context'))
    else:
        pattern = DIAGNOSTICS[diagnostic]
    checked = r['stderr']
    if diagnostic == 'module-global' and engine == 'reference':
        checked = checked.replace('syntax error in MODULE.bazel file for <root>','')
    if (type(r['returncode']) is not int or r['returncode'] not in codes or r['timed_out'] or r['stdout'].strip()
            or any(r.get(k) for k in ('spawn_error','interrupted','output_truncated'))
            or common.UNRELATED.search(checked)
            or not re.search(pattern, utility.error_lines(r['stderr']), re.I)
            or 'SUBCOMMAND:' in r['stderr'] or any(utility.marker_count(r,m) for m in
                ('I0_ACTION_READY','GUARD_BYPASSED','I0_FORBIDDEN_LEAF','AFTER_FACTORY child_object','AFTER_FACTORY child_bound'))):
        raise ValueError('expected anchored controlled subrule rejection: '+diagnostic)
    if diagnostic not in ('build-global','module-global') and re.search(r"(?:name 'subrule' is not defined|Variable `subrule` not found)", r['stderr']):
        raise ValueError('old missing-global binary is not semantic rejection')


def require_defect(r, probe):
    # Debug events may not be flushed by Bazel's fatal path. Require actual
    # analysis/args callsite, NOT an invented BEFORE_FACTORY observation.
    signatures = ('FATAL: bazel crashed due to an internal error.',
        'Caused by: java.lang.NullPointerException:', 'StarlarkRuleContext.getStarlarkSemantics()',
        'because "this.starlarkRuleContext" is null',
        'StarlarkSubrule$SubruleContext.getStarlarkSemantics(StarlarkSubrule.java:461)',
        'StarlarkActionFactory.args(StarlarkActionFactory.java:1259)',
        'NullPointerException thrown during Starlark evaluation (//:'+probe+')',
        'at <starlark>.args(<builtin>:0)', 'StarlarkRuleConfiguredTargetUtil.evalRule(')
    if (probe not in DEFECTS or type(r['returncode']) is not int or r['returncode'] != 37 or r['timed_out'] or r['stdout']
            or any(r.get(k) for k in ('spawn_error','interrupted','output_truncated'))
            or any(s not in r['stderr'] for s in signatures)
            or not re.search(r'at <starlark>\._rule\([^\n]*/defs\.bzl:7\)', r['stderr'])
            or utility.marker_count(r,'AFTER_FACTORY '+probe) or 'SUBCOMMAND:' in r['stderr']
            or 'ConcurrentModificationException' in r['stderr']):
        raise ValueError('required exact known Args-factory analysis NPE, not parity rejection')


def phases_for(item, reference_only=False):
    return [(p,e) for p in item['probes'] for e in ('reference','native')
        if not (reference_only and e == 'native') and not (item['name'] == 'boundaries' and e == 'reference')]


def steps_for(family, probe, engine):
    v = fixture(family, probe)
    if v['defect'] and engine == 'reference':
        return [('defect','build',v['target'],[])]
    if v['diagnostic']:
        return [('reject','build',v['target'],['--experimental_rule_extension_api=false'] if probe == 'explicit-flag' else [])]
    steps = []
    for stage in ['cold','warm']+(['changed'] if v['changed'] else []):
        steps.append((stage,'build',v['target'],[]))
        if engine == 'reference':
            steps.append((stage,'cquery',v['target'],['--output=files']))
    if family == 'functional' and probe == 'factory-two-action' and engine == 'reference':
        steps.append(('graph','aquery','deps('+v['target']+')',['--output=jsonproto']))
    return steps


def plan_receipt(reference_only):
    entries = []
    for c in CASEPLAN:
        for p,e in phases_for(c,reference_only):
            v = fixture(c['name'],p)
            entries.append(dict(family=c['name'],probe=p,engine=e,steps=steps_for(c['name'],p,e),
                fixture={k:digest(raw) for k,raw in v['files'].items()},
                expected={k:digest(raw) for k,raw in v['expected'].items()}, origin=v['origin'],
                changed=None if not v['changed'] else [v['changed'][0],digest(v['changed'][1]),{k:digest(b) for k,b in v['changed'][2].items()}]))
    return dict(families=len(CASEPLAN),probes=sum(len(c['probes']) for c in CASEPLAN),phases=len(entries),
        commands=2+sum(len(e['steps']) for e in entries),sha256=digest(json.dumps(entries,sort_keys=True).encode()))


def argument_parser():
    parser = common.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/subrules-validation.json')
    parser.add_argument('--reference-source',type=Path,default=Path('/tmp/opencode/bazel-9.3-reference'))
    parser.add_argument('--platforms-distfile',type=Path,default=shared.argument_parser().parse_args([]).platforms_distfile)
    return parser


def executable_evidence(args):
    if not args.reference_only and args.native_sha256 in (OLD_NATIVE,shared.OLD_NATIVE,shared.PRE_FIX_DEBUG_SHA):
        raise ValueError('old pre-subrule binary cannot be frozen I0 candidate')
    return common.executable_evidence(args)


def source_evidence(path):
    commit = subprocess.check_output(['git','-C',str(path),'rev-parse','HEAD'],timeout=10).decode().strip()
    files = {p:digest((path / p).read_bytes()) for p in OFFICIAL_FILES}
    if commit != SOURCE_PIN or files != OFFICIAL_FILES:
        raise ValueError('pinned official source changed')
    return dict(path=str(path),commit=commit,files=files)


def source_bindings():
    return {str(p):dict(path=str(p),sha256=digest(p.read_bytes()),size=p.stat().st_size,mode=p.stat().st_mode & 0o7777) for p in source_paths()}


class RecordingRunner(utility.RecordingRunner):
    def __init__(self,report,args):
        super().__init__(report,args.timeout)
        self.args = args

    def run(self,name,argv,cwd,timeout=None):
        binding = dict(executables=executable_evidence(self.args),source=source_evidence(self.args.reference_source),production=source_bindings())
        if binding != dict(executables=self.report['executables'],source=self.report['official_source_before'],production=self.report['sources_before']):
            raise ValueError('pre-command binary/source binding changed; preparation is not acceptance')
        if '--batch' not in argv:
            r = super().run(name,argv,cwd,timeout)
        else:
            # The common runner reaps children but does not expose their PID.
            # For the two known crashes retain the owned batch handle explicitly;
            # never scrape unrelated process arguments or invent daemon PID data.
            r = dict(id=len(self.report['runs']),name=name,argv=list(map(str,argv)),cwd=str(cwd),
                timeout_seconds=self.timeout if timeout is None else timeout,returncode=None,
                stdout='',stderr='',timed_out=False,executable_sha256=self.report['executables']['reference']['sha256'])
            self.report['runs'].append(r)
            process, out, err = None,b'',b''
            started = time.monotonic()
            try:
                process = subprocess.Popen(argv,cwd=cwd,env=self.env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
                self.report['owned_batch_pids'].append(process.pid)
                try:
                    out,err = process.communicate(timeout=r['timeout_seconds'])
                except subprocess.TimeoutExpired:
                    r['timed_out'] = True
                    os.killpg(process.pid,signal.SIGKILL)
                    out,err = process.communicate()
                except BaseException:
                    r['interrupted'] = True
                    os.killpg(process.pid,signal.SIGKILL)
                    out,err = process.communicate()
                    raise
            except OSError as exc:
                r['spawn_error'] = str(exc)
            finally:
                r['elapsed_seconds'] = time.monotonic()-started
                r['returncode'] = process.returncode if process else None
                r['batch_process'] = dict(pid=process.pid if process else None,reaped=process is not None and process.returncode is not None,
                    alive_after=bool(process and Path('/proc',str(process.pid)).exists()))
                for field,raw in (('stdout',out),('stderr',err)):
                    r.update({field:raw.decode('utf-8',errors='replace'),field+'_base64':base64.b64encode(raw).decode(),
                        field+'_sha256':digest(raw),field+'_full_sha256':digest(raw),field+'_full_size_bytes':len(raw)})
                    if len(raw) > direct.MAX_STREAM:
                        r['output_truncated'] = True
        r['preflight_binding_sha256'] = digest(json.dumps(binding,sort_keys=True).encode())
        return r


def command_argv(args,ref,engine,cache,root,step):
    stage,command,target,extra = step
    flags = []
    if engine == 'reference':
        flags = ['--incompatible_autoload_externally=','--noshow_progress','--repository_disable_download','--distdir='+str(root / 'distdir')]
        if command == 'build':
            flags += ['--subcommands','--shell_executable=/bin/bash','--spawn_strategy=local',
                '--execution_log_json_file='+str(cache.parent / (stage+'.execution.json'))]
    argv = utility.extensions.command_argv(args,ref,engine,cache,root / 'registry',command,target,extra,flags)
    if stage == 'defect':
        argv.insert(1,'--batch')
        argv = [a.replace(str(ref.output_base),str(cache.parent / 'crash-output-base'))
            .replace(str(ref.output_user_root),str(cache.parent / 'crash-user-root')) for a in argv]
    return argv


def output_paths(r,target,engine,root,workspace,ref):
    return direct.native_paths(r,target,root) if engine == 'native' else utility.reference_output_paths(r['stdout'],workspace,ref.output_base)


def check_spawns(spawns,v,stage):
    expected = [] if stage == 'warm' else sorted(v['spawns'])
    if sorted(s['mnemonic'] for s in spawns) != expected:
        raise ValueError('missing or extra actual action execution')
    raw = next(iter(v['changed'][2].values())) if stage == 'changed' else next(iter(v['expected'].values()))
    for s in spawns:
        if s['exitCode'] != 0 or s.get('cacheHit') or len(s['actualOutputs']) != 1:
            raise ValueError('invalid actual spawn')
        name = s['mnemonic']
        expected_inputs = ({'consumer/seed.txt':raw} if name == 'I0SubruleProducer' else
            {'consumer/consume.generated':raw} if name == 'I0SubruleConsumer' else
            {'external/+ext+made/data':raw} if name == 'I0ModuleConsumer' else {})
        actual = s['inputs']
        if len(actual) != len(expected_inputs):
            raise ValueError('wrong dependency closure')
        for suffix,content in expected_inputs.items():
            matches = [i for i in actual if i['path'] == suffix or i['path'].endswith('/'+suffix)]
            if len(matches) != 1 or matches[0]['digest']['hash'] != digest(content):
                raise ValueError('wrong consumed File path/hash')
        suffix = 'consumer/consume.generated' if name == 'I0SubruleProducer' else 'consumer/consume.out' if name == 'I0SubruleConsumer' else next(iter(v['expected']))
        out = s['actualOutputs'][0]
        if not out['path'].endswith('/'+suffix) or out['digest']['hash'] != digest(raw):
            raise ValueError('wrong generated File output/hash')


def check_graph(graph):
    fragments = {str(p['id']):p for p in graph['pathFragments']}
    def path(key,seen=()):
        key = str(key)
        if key in seen:
            raise ValueError('cyclic artifact path')
        p = fragments[key]
        return (path(p['parentId'],seen+(key,))+'/' if 'parentId' in p else '')+p['label']
    artifacts = {str(a['id']):path(a['pathFragmentId']) for a in graph['artifacts']}
    deps = {str(d['id']):d for d in graph['depSetOfFiles']}
    def inputs(ids,seen=()):
        result = set()
        for key in map(str,ids):
            if key in seen:
                raise ValueError('cyclic dependency graph')
            d = deps[key]
            result.update(artifacts[str(i)] for i in d.get('directArtifactIds',[]))
            result.update(inputs(d.get('transitiveDepSetIds',[]),seen+(key,)))
        return result
    actions = {a['mnemonic']:a for a in graph['actions']}
    targets = {str(t['id']):t['label'] for t in graph['targets']}
    if len(graph['actions']) != 2 or set(actions) != {'I0SubruleProducer','I0SubruleConsumer'}:
        raise ValueError('missing producer/consumer graph')
    producer,consumer = actions['I0SubruleProducer'],actions['I0SubruleConsumer']
    outputs = {artifacts[str(i)] for i in producer['outputIds']}
    if (len(outputs) != 1 or not next(iter(outputs)).endswith('/consumer/consume.generated')
            or inputs(producer['inputDepSetIds']) != {'consumer/seed.txt'}
            or inputs(consumer['inputDepSetIds']) != outputs
            or any(targets[str(a['targetId'])] != '//consumer:consume' for a in actions.values())):
        raise ValueError('actual Files not connected/owned by consuming target')


def phase_status(v,engine):
    return ('KNOWN_REFERENCE_DEFECT' if engine == 'reference' else 'INTENTIONAL_DIVERGENCE') if v['defect'] else 'PASS'


def run_phase(args,runner,root,workspace,ref,family,probe,engine):
    v = fixture(family,probe)
    paths = utility.materialize(workspace,dict(v,genuine=False))
    cache = root / 'cases' / family / probe / engine / 'cache'
    cache.parent.mkdir(parents=True,exist_ok=True)
    p = dict(family=family,probe=probe,engine=engine,status='FAIL',runs=[],outputs={},logs={},input_snapshots={},
        cache=str(cache),actions_before=utility.action_store(cache),generated={})
    try:
        for i,step in enumerate(steps_for(family,probe,engine)):
            stage,cmd,target,_ = step
            if stage == 'changed' and cmd == 'build':
                (workspace / v['changed'][0]).write_bytes(v['changed'][1])
            p['input_snapshots'][str(i)] = snapshot(paths)
            shared.remember_pid(ref,runner.report)
            r = runner.run(family+'.'+probe+'.'+engine+'.'+str(i),command_argv(args,ref,engine,cache,root,step),workspace)
            shared.remember_pid(ref,runner.report)
            p['runs'].append(r['id'])
            if cmd == 'build' and engine == 'reference':
                log = cache.parent / (stage+'.execution.json')
                p['logs'][stage] = evidence(log) if log.exists() else dict(path=str(log),absent=True)
            if stage == 'defect':
                require_defect(r,probe)
            elif stage == 'reject':
                require_rejection(r,v['diagnostic'],engine)
            else:
                success(r)
                if stage in ('cold','warm','changed') and cmd == ('build' if engine == 'native' else 'cquery'):
                    outs = output_paths(r,target,engine,root,workspace,ref)
                    p['outputs'][stage] = [evidence(o) for o in outs]
                    if family == 'functional' and probe == 'factory-two-action':
                        generated = list((cache / 'actions').rglob('consume.generated')) if engine == 'native' else [outs[0].parent / 'consume.generated']
                        p['generated'][stage] = [evidence(o) for o in generated if o.is_file()]
        p['status'] = phase_status(v,engine)
    except Exception as exc:
        p['error'] = type(exc).__name__+': '+str(exc)
    finally:
        p['inputs_after'] = snapshot(paths)
        p['actions_after'] = utility.action_store(cache)
        forbidden = ['rejected.marker']
        if v['diagnostic'] and v['origin'] == 'SCOPE_CONTRACT_SHARED_PLATFORM_DERIVATIVE':
            name = v['target'].split(':')[-1]
            forbidden += [name+'.seed',name+'.parent','forbidden']
        p['forbidden_outputs'] = sorted(str(p) for base in (workspace,cache,ref.output_base / 'execroot')
            for name in forbidden for p in base.rglob(name) if p.is_file())
    return p


def validate_phase(p,report,args,workspace,ref):
    family,probe,engine = p['family'],p['probe'],p['engine']
    item = next(c for c in CASEPLAN if c['name'] == family)
    if (probe,engine) not in phases_for(item,args.reference_only):
        raise ValueError('unplanned phase')
    v = fixture(family,probe)
    if p['status'] != phase_status(v,engine) or p.get('error') or p['forbidden_outputs']:
        raise ValueError('failed phase or reference defect falsely treated as parity')
    root = Path(report['scratch_directory'])
    cache = root / 'cases' / family / probe / engine / 'cache'
    if p['cache'] != str(cache):
        raise ValueError('unbound cache')
    utility.no_actions(p['actions_before'],cache)
    if any(x['exists'] for x in p['actions_before'].values()):
        raise ValueError('cold action store existed')
    if v['diagnostic']:
        utility.no_actions(p['actions_after'],cache)
    steps = steps_for(family,probe,engine)
    if len(p['runs']) != len(steps) or len(set(p['runs'])) != len(steps) or set(p['input_snapshots']) != {str(i) for i in range(len(steps))}:
        raise ValueError('missing/duplicate/extra phase commands')
    files,expected_outputs,expected_logs = v['files'].copy(),set(),set()
    for i,(rid,step) in enumerate(zip(p['runs'],steps)):
        stage,cmd,target,_ = step
        if stage == 'changed' and cmd == 'build':
            files[v['changed'][0]] = v['changed'][1]
        shared.check_input(p['input_snapshots'][str(i)],files,workspace)
        r = report['runs'][rid]
        if (type(rid) is not int or r['id'] != rid or r['name'] != family+'.'+probe+'.'+engine+'.'+str(i)
                or r['cwd'] != str(workspace) or r['argv'] != command_argv(args,ref,engine,cache,root,step)
                or r['timeout_seconds'] != args.timeout):
            raise ValueError('command binding differs from plan')
        if stage == 'defect':
            require_defect(r,probe)
            process = r['batch_process']
            if (set(process) != {'pid','reaped','alive_after'} or type(process['pid']) is not int or process['pid'] <= 1
                    or process['pid'] not in report['owned_batch_pids'] or process['reaped'] is not True or process['alive_after'] is not False):
                raise ValueError('missing genuine owned/reaped batch PID')
        elif stage == 'reject':
            require_rejection(r,v['diagnostic'],engine)
        else:
            success(r)
        if stage != 'defect' and cmd == 'build' and (i == 0 or engine == 'native' or stage == 'changed'):
            for marker in v['markers']:
                if not utility.marker_count(r,marker):
                    raise ValueError('missing actual phase marker: '+marker)
        if v['diagnostic'] and v['stage'] != 'analysis' and utility.marker_count(r,'I0_RULE'):
            raise ValueError('loading/options error reached analysis')
        if cmd == 'build' and engine == 'reference':
            expected_logs.add(stage)
            log = p['logs'][stage]
            path = str(cache.parent / (stage+'.execution.json'))
            if v['diagnostic'] and log == dict(path=path,absent=True):
                pass
            else:
                if log['path'] != path or not valid_evidence(log):
                    raise ValueError('unbound execution log')
                spawns = shared.json_objects(base64.b64decode(log['bytes_base64']))
                if v['diagnostic']:
                    if spawns:
                        raise ValueError('rejected analysis executed actions')
                else:
                    check_spawns(spawns,v,stage)
        if v['diagnostic']:
            continue
        if cmd == 'build' and engine == 'native':
            common.action_counts(r,(0,v['actions']) if stage == 'warm' else (v['actions'],0))
        if stage == 'graph':
            check_graph(json.loads(r['stdout']))
        if stage not in ('cold','warm','changed') or cmd != ('build' if engine == 'native' else 'cquery'):
            continue
        expected_outputs.add(stage)
        paths = output_paths(r,target,engine,root,workspace,ref)
        entries = p['outputs'][stage]
        wanted = v['changed'][2] if stage == 'changed' else v['expected']
        if ([str(o) for o in paths] != [e['path'] for e in entries] or len(entries) != len(wanted)
                or {Path(e['path']).name for e in entries} != set(wanted)):
            raise ValueError('missing/unbound actual outputs or wrong DefaultInfo/target')
        for e in entries:
            shared.check_file(e,wanted[Path(e['path']).name],0o555)
        if family == 'functional' and probe == 'factory-two-action':
            generated = p['generated'][stage]
            expected = {b'seed-A!\n',b'seed-B!\n'} if stage == 'changed' and engine == 'native' else {b'seed-B!\n' if stage == 'changed' else b'seed-A!\n'}
            if {base64.b64decode(e['bytes_base64']) for e in generated} != expected:
                raise ValueError('producer generated File missing/wrong')
            for e in generated:
                shared.check_file(e,base64.b64decode(e['bytes_base64']),0o555)
                if not Path(e['path']).is_relative_to(cache if engine == 'native' else ref.output_base) or Path(e['path']).name != 'consume.generated':
                    raise ValueError('producer path not bound')
    shared.check_input(p['inputs_after'],files,workspace)
    if set(p['outputs']) != expected_outputs or set(p['logs']) != expected_logs:
        raise ValueError('missing/extra phase evidence')
    if set(p['generated']) != (expected_outputs if family == 'functional' and probe == 'factory-two-action' else set()):
        raise ValueError('missing/extra generated File evidence')
    if 'cold' in p['outputs'] and p['outputs']['cold'] != p['outputs']['warm']:
        raise ValueError('warm outputs differ')
    if engine == 'native' and not v['diagnostic']:
        store = p['actions_after']['actions']
        if store['path'] != str(cache / 'actions') or store['exists'] is not True or not store['entries']:
            raise ValueError('positive native action cache absent')
        for entries in p['outputs'].values():
            for e in entries:
                if str(Path(e['path']).relative_to(cache / 'actions')) not in store['entries']:
                    raise ValueError('actual output absent from action store')


def report_passes(report):
    """Strict offline replay; bytes, commands, phases and pins must all agree."""
    try:
        if set(report) != REPORT_KEYS:
            return False
        args = argument_parser().parse_args([])
        if set(report['configuration']) != set(vars(args)):
            return False
        for key,value in report['configuration'].items():
            setattr(args,key,Path(value) if isinstance(getattr(args,key),Path) else value)
        status = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS_ONLY_BOUNDED_PROFILE'
        start,end = [datetime.datetime.fromisoformat(report[k]) for k in ('started_at','finished_at')]
        if start.tzinfo is None or end.tzinfo is None or end < start:
            return False
        native_commands = sum(len(steps_for(c['name'],p,e)) for c in CASEPLAN for p,e in phases_for(c,args.reference_only) if e == 'native')
        if report['native_claim'] != dict(status='NOT_RUN' if args.reference_only else 'EXECUTED_BOUNDED_PROFILE',commands=native_commands,pass_claim=not args.reference_only):
            return False
        if (report['status'] not in ('FAIL',status) or report['schema_version'] != 1
                or report['plan'] != plan_receipt(args.reference_only) or report['planned_checks'] != CASEPLAN
                or report['limitations'] != LIMITATIONS or report['known_reference_defects'] != KNOWN_REFERENCE_DEFECTS
                or report['origins'] != ORIGINS or report['errors'] or report['cleanup']
                or report['sources_before'] != report['sources_after'] or set(report['sources_before']) != set(map(str,source_paths()))
                or report['executables'] != report['executables_after']
                or report['official_source_before'] != report['official_source_after']
                or report['official_source_before'] != dict(path=str(args.reference_source),commit=SOURCE_PIN,files=OFFICIAL_FILES)):
            return False
        for path,e in report['sources_before'].items():
            if (e['path'] != path or not re.fullmatch('[0-9a-f]{64}',e['sha256']) or type(e['size']) is not int or e['size'] < 0
                    or type(e['mode']) is not int or not 0 <= e['mode'] <= 0o7777):
                return False
        engines = ('reference',) if args.reference_only else ('reference','native')
        if set(report['executables']) != set(engines) or report['executables']['reference']['sha256'] != REFERENCE_SHA256:
            return False
        for engine in engines:
            if report['executables'][engine]['path'] != str(getattr(args,engine)):
                return False
        if not args.reference_only and (args.native_sha256 in (OLD_NATIVE,shared.OLD_NATIVE,shared.PRE_FIX_DEBUG_SHA)
                or not re.fullmatch('[0-9a-f]{64}',args.native_sha256 or '') or report['executables']['native']['sha256'] != args.native_sha256):
            return False
        root = Path(report['scratch_directory'])
        if not root.is_relative_to(args.scratch_parent) or root == args.scratch_parent:
            return False
        workspace = root / 'workspace'
        ref = DirectReference(args.reference,workspace,root,None,SUITE)
        pids,batch = report['owned_pids'],report['owned_batch_pids']
        if (not pids or len(batch) != len(DEFECTS) or len(set(pids+batch)) != len(pids+batch)
                or any(type(p) is not int or p <= 1 for p in pids+batch)
                or report['cleanup_receipt'] != dict(path=str(root),removed=True,pids={str(p):False for p in pids+batch},remaining_pidfiles=[])):
            return False
        if (report['registry_before'] != report['registry_after'] or set(report['registry_before']) !=
                {str(root / 'registry' / p.relative_to(REGISTRY)) for p in REGISTRY.rglob('*') if p.is_file()}):
            return False
        for path,e in report['registry_before'].items():
            original = str(REGISTRY / Path(path).relative_to(root / 'registry'))
            if not valid_evidence(e) or e['sha256'] != report['sources_before'][original]['sha256'] or e['path'] != path or e['mode'] != 0o644:
                return False
        if report['platforms_distfile']['sha256'] != PLATFORM_SHA or not valid_evidence(report['platforms_distfile']):
            return False
        runs = report['runs']
        if len(runs) != report['plan']['commands'] or [r['id'] for r in runs] != list(range(len(runs))):
            return False
        binding = digest(json.dumps(dict(executables=report['executables'],source=report['official_source_before'],production=report['sources_before']),sort_keys=True).encode())
        for r in runs:
            engine = next((e for e in engines if str(getattr(args,e)) == r['argv'][0]),None)
            if (engine is None or r['executable_sha256'] != report['executables'][engine]['sha256']
                    or r['preflight_binding_sha256'] != binding or 'environment' in r or 'environment_keys' in r
                    or type(r['id']) is not int or type(r['returncode']) is not int or r['timed_out'] is not False
                    or any(r.get(k) for k in ('spawn_error','interrupted','output_truncated'))):
                return False
            for field in ('stdout','stderr'):
                raw = base64.b64decode(r[field+'_base64'],validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8',errors='replace') != r[field]):
                    return False
        if [r['batch_process']['pid'] for r in runs if 'batch_process' in r] != batch:
            return False
        for r,name,argv,timeout in [(runs[0],'reference.version',ref.startup+['version','--gnu_format'],args.timeout),
                (runs[-1],SUITE+'.shutdown',ref.startup+['shutdown']+list(COMMON_FLAGS),min(args.timeout,15))]:
            if r['name'] != name or r['argv'] != argv or r['cwd'] != str(workspace) or r['timeout_seconds'] != timeout:
                return False
            success(r)
        if runs[0]['stdout'].strip() != 'bazel 9.3.0':
            return False
        expected = [(c['name'],p,e) for c in CASEPLAN for p,e in phases_for(c,args.reference_only)]
        if [(p['family'],p['probe'],p['engine']) for p in report['phases']] != expected or report['completed_phases'] != len(expected):
            return False
        used = [0]
        for p in report['phases']:
            validate_phase(p,report,args,workspace,ref)
            used.extend(p['runs'])
        return used+[len(runs)-1] == list(range(len(runs)))
    except (KeyError,ValueError,TypeError,IndexError,AttributeError,StopIteration,OSError):
        return False


def main(argv=None):
    args = argument_parser().parse_args(argv)
    if args.reference_only and args.output == ROOT / 'artifacts/subrules-validation.json':
        args.output = ROOT / 'artifacts/subrules-reference.json'
    for key,value in vars(args).items():
        if isinstance(value,Path):
            setattr(args,key,value.expanduser().resolve())
    if (args.output in [args.native,args.reference,args.platforms_distfile,*source_paths(),*(ROOT / p for p in ORIGINS)]
            or args.output.name.endswith('-contract.json') or args.output.is_relative_to(args.reference_source)
            or any(args.output.is_relative_to(ROOT / p) for p in ('src','scripts','tests','resources','vendor','.direnv'))):
        raise ValueError('report cannot overwrite source, executable or original contracts')
    report = dict(schema_version=1,status='FAIL',runs=[],phases=[],errors=[],cleanup=[],owned_pids=[],owned_batch_pids=[],
        planned_checks=CASEPLAN,plan=plan_receipt(args.reference_only),limitations=LIMITATIONS,
        known_reference_defects=KNOWN_REFERENCE_DEFECTS,origins=ORIGINS,
        native_claim=dict(status='NOT_RUN' if args.reference_only else 'EXECUTED_BOUNDED_PROFILE',
            commands=sum(len(steps_for(c['name'],p,e)) for c in CASEPLAN for p,e in phases_for(c,args.reference_only) if e == 'native'),pass_claim=not args.reference_only),
        configuration={k:str(v) if isinstance(v,Path) else v for k,v in vars(args).items()},
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
    root = ref = None
    runner = RecordingRunner(report,args)
    interrupted = False
    try:
        report['sources_before'] = source_bindings()
        report['executables'] = executable_evidence(args)
        report['official_source_before'] = source_evidence(args.reference_source)
        report['platforms_distfile'] = evidence(args.platforms_distfile)
        if report['platforms_distfile']['sha256'] != PLATFORM_SHA:
            raise ValueError('genuine platforms distfile differs')
        root = Path(tempfile.mkdtemp(prefix=SUITE+'-',dir=args.scratch_parent))
        report['scratch_directory'] = str(root)
        workspace = root / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text('module(name="subrule_i0")\n')
        for key in ('HOME','TMPDIR'):
            path = root / key.lower()
            path.mkdir()
            runner.env[key] = str(path)
        shutil.copytree(REGISTRY,root / 'registry')
        report['registry_before'] = snapshot(p for p in (root / 'registry').rglob('*') if p.is_file())
        (root / 'distdir').mkdir()
        (root / 'distdir/platforms-1.0.0.tar.gz').write_bytes(args.platforms_distfile.read_bytes())
        ref = DirectReference(args.reference,workspace,root,runner,SUITE)
        r = runner.run('reference.version',ref.startup+['version','--gnu_format'],workspace)
        if success(r).strip() != 'bazel 9.3.0':
            raise ValueError('wrong reference release')
        shared.remember_pid(ref,report)
        for item in CASEPLAN:
            for probe,engine in phases_for(item,args.reference_only):
                phase = run_phase(args,runner,root,workspace,ref,item['name'],probe,engine)
                report['phases'].append(phase)
                try:
                    validate_phase(phase,report,args,workspace,ref)
                except Exception as exc:
                    phase.update(status='FAIL',error=phase.get('error') or str(exc))
                print(item['name'],probe,engine,phase['status'],phase.get('error',''),flush=True)
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        if ref is not None:
            try:
                shared.remember_pid(ref,report)
                # Cleanup must still execute after a binding failure. Its missing
                # preflight binding blocks acceptance but never strands a daemon.
                try:
                    r = runner.run(SUITE+'.shutdown',ref.startup+['shutdown']+list(COMMON_FLAGS),ref.workspace,timeout=min(args.timeout,15))
                except ValueError as exc:
                    report['errors'].append('shutdown binding: '+str(exc))
                    r = utility.RecordingRunner.run(runner,SUITE+'.shutdown',ref.startup+['shutdown']+list(COMMON_FLAGS),ref.workspace,timeout=min(args.timeout,15))
                success(r)
                report['registry_after'] = snapshot(p for p in (root / 'registry').rglob('*') if p.is_file())
                pids = {str(p):Path('/proc',str(p)).exists() for p in report['owned_pids']+report['owned_batch_pids']}
                remaining = sorted(str(p) for p in root.rglob('server.pid.txt'))
                if any(pids.values()) or remaining:
                    raise ValueError('owned PID/server files remain; scratch retained')
                shutil.rmtree(root)
                report['cleanup_receipt'] = dict(path=str(root),removed=not root.exists(),pids=pids,remaining_pidfiles=remaining)
            except Exception as exc:
                report['errors'].append('cleanup: '+str(exc))
        elif root is not None:
            shutil.rmtree(root)
        try:
            report['sources_after'] = source_bindings()
            report['executables_after'] = executable_evidence(args)
            report['official_source_after'] = source_evidence(args.reference_source)
        except Exception as exc:
            report['errors'].append('final bindings: '+str(exc))
        report['completed_phases'] = sum(p['status'] in ('PASS','KNOWN_REFERENCE_DEFECT','INTENTIONAL_DIVERGENCE') for p in report['phases'])
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS_ONLY_BOUNDED_PROFILE'
        utility.write_report(args.output,report)
    print(report['status']+': '+str(args.output))
    return 130 if interrupted else 0 if report['status'] != 'FAIL' else 1


if __name__ == '__main__':
    def terminate(signum,frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM,terminate)
    sys.exit(main())
