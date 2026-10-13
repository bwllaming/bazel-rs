#!/usr/bin/env python3
"""Bounded functional no-user-attrs symbolic macros, not full Bazel/SDK parity.

Only the explicitly SHA-frozen native candidate may execute. The pinned genuine
Bazel ELF is an independent reference, never a fallback. No user workspace,
environment serialization, fabricated host globals, or altered upstream archive.
"""

import base64
import datetime
import json
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile

import validate_existing_rules as existing
import validate_string_list_dict as common
import validate_repository_rules as direct
import validate_module_extensions as extensions
from validate import ROOT, COMMON_FLAGS, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256


digest, evidence, snapshot, valid_evidence = direct.digest, direct.evidence, direct.snapshot, direct.valid_evidence
success, RecordingRunner = extensions.success, existing.RecordingRunner
REGISTRY = existing.REGISTRY
SOURCE_PIN = 'e89755babf3e941bc1e9d8120b1cd48854ab9dfa'
SOURCES = list(dict.fromkeys([ROOT / 'scripts/validate_symbolic_macros.py',
    ROOT / 'tests/test_validate_symbolic_macros.py', ROOT / 'scripts/validate_existing_rules.py',
    ROOT / 'scripts/validate_local_repositories.py', *common.SOURCES,
    *sorted(p for p in REGISTRY.rglob('*') if p.is_file())]))
CASEPLAN = [
    common.case('functional', ('private-closure', 'factory-owner-alias', 'module-initialization')),
    common.case('visibility', ('normalized-fresh-list', 'late-default', 'late-default-private', 'same-definition', 'caller-to-private',
        'ordinary-rule', 'ordinary-output', 'ordinary-file', 'different-definition',
        'factory-not-lexical', 'external-same-path', 'package-group')),
    common.case('sources', ('macro-only', 'explicit-export', 'ordinary-before', 'ordinary-after',
        'custom-default-unselected', 'reserved-name')),
    common.case('namespace', ('valid-rule-outside-output', 'dynamic-artifact',
        'outside-output-first', 'outside-output-second', 'invalid-generator', 'empty-suffix', 'unselected-invalid')),
    common.case('collisions', ('macro', 'rule-before', 'rule-after', 'output-before', 'output-after',
        'export-before', 'export-after')),
    common.case('typing', ('missing-name', 'integer-name', 'positional', 'unknown-none',
        'return-value', 'unexported', 'builtin-implementation', 'len-implementation', 'provider-implementation', 'frozen-visibility')),
    common.case('phase', ('constructor-build', 'invocation-init', 'repository-invocation',
        'extension-invocation', 'glob-helper', 'package-helper')),
    common.case('native-boundary', ('user-attrs', 'inheritance', 'finalizer', 'nested',
        'exports-files', 'name-none', 'build-introspection'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASEPLAN]
LIMITATIONS = [
    'Functional Starlark def/closure macros with empty-effective attrs only; no full Bazel or application parity.',
    'Nonempty attrs, inheritance, finalizers, nesting and in-macro exports_files are native-only unsupported boundaries.',
    'name=None is native-only robustness: known upstream crash is deliberately never rerun.',
    'BUILD native.existing_rule(s) remains absent; no D BUILD introspection parity claim.',
    'Private closures and custom action output bytes are real Starlark, not SDK stubs.',
    'Full bazel_features generated globals still require other unsupported providers; no full features compatibility claim.',
    'Reference callback frequency on its persistent server is not equated to fresh native CLI evaluation.',
    'Reference-only preparation never executes native and cannot report parity PASS.',
    'Queries permit invalid macro namespaces; analysis checks only visited target names, not all artifacts.',
]
MODULE = 'module(name="symbolic_contract")\n'
RULE = ('def _write(ctx):\n'
    '    out=ctx.actions.declare_file(ctx.attr.out)\n'
    '    ctx.actions.write(out,ctx.attr.text,is_executable=True)\n'
    '    return [DefaultInfo(files=depset([out]))]\n'
    'write=rule(implementation=_write,attrs={"out":attr.string(),"text":attr.string()})\n')
CALLBACK = 'print("MACRO_CALLBACK")\n'
READY = 'print("MACRO_READY")\n'


def macro(body, constructor='implementation=_impl', prefix=''):
    return prefix+'def _impl(name,visibility):\n'+common.indent(CALLBACK+body)+'m=macro('+constructor+')\n'+READY


def gen(name='name', out='name+".out"', srcs=None, visibility='visibility', text='macro-data'):
    args = 'name='+name+',outs=['+out+']'
    if srcs is None:
        args += ',cmd='+repr('printf '+text+' > $@')
    else:
        args += ',srcs='+srcs+',cmd="cat $(SRCS) > $@"'
    if visibility is not None:
        args += ',visibility='+visibility
    return 'native.genrule('+args+')\n'


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    files = {'MODULE.bazel': MODULE, 'BUILD.bazel': '', 'defs/BUILD.bazel': '',
        'defs/m.bzl': macro(gen()), 'app/BUILD.bazel': 'load("//defs:m.bzl","m")\nm(name="demo")\n'}
    target, expected, actions = '//app:demo', {'demo.out': b'macro-data'}, 1
    diagnostic, queries, negative_queries, markers = None, [], [], ['MACRO_READY', 'MACRO_CALLBACK']
    if name == 'functional':
        if probe == 'private-closure':
            body = (CALLBACK+'write(name=name+"_custom",out=name+".custom",text=captured+native.package_name())\n'
                +gen('name+"-native"', 'name+".native"', text='native-data', visibility=None)
                +'native.filegroup(name=name+".dot",srcs=[":"+name+"_custom"])\n'
                +'native.filegroup(name=name,srcs=[":"+name+".dot",":"+name+"-native"],visibility=visibility)\n')
            files['defs/m.bzl'] = (RULE+'def factory(captured):\n    def impl(name,visibility):\n'
                +common.indent(common.indent(body))+'    return macro(implementation=impl,attrs={"removed":None},inherit_attrs=None,finalizer=False,doc="closure")\n'
                '_private=factory("closure:")\nm=_private\n'
                'def _zero(name,visibility):\n    pass\nzero=macro(implementation=_zero)\n'+READY)
            files['alias/BUILD.bazel'] = ''
            files['alias/m.bzl'] = 'load("//defs:m.bzl",original="m",original_zero="zero")\nm=original\nzero=original_zero\n'
            files['app/BUILD.bazel'] = ('load("//alias:m.bzl","m","zero")\n'
                'zero(name="empty")\nzero(name="empty_child")\nm(name="demo",visibility=["//visibility:public"])\n')
            expected, actions = {'demo.custom': b'closure:app', 'demo.native': b'native-data'}, 2
        elif probe == 'factory-owner-alias':
            factory_fixture(files)
            expected = {'demo.out': b'factory-owner-b'}
        else:
            files['defs/m.bzl'] = macro('pass\n')+('def _repo(ctx):\n'
                '    ctx.file("data",repr(m),executable=False)\n'
                '    ctx.file("BUILD.bazel",\'exports_files(["data"],visibility=["//visibility:public"])\\n\')\n'
                'r=repository_rule(implementation=_repo)\n')
            files['MODULE.bazel'] += 'r=use_repo_rule("//defs:m.bzl","r")\nr(name="made")\n'
            files['app/BUILD.bazel'] = 'genrule(name="demo",srcs=["@made//:data"],outs=["demo.out"],cmd="cat $(SRCS) > $@")\n'
            expected, markers = {'demo.out': b'<macro m>'}, ['MACRO_READY']
    elif name == 'visibility':
        if probe == 'package-group':
            files['defs/m.bzl'] = macro('if [x.package+":"+x.name for x in visibility] != ["app:__pkg__","app:friends"]:\n    fail("GROUP_VISIBILITY_NOT_FORWARDED")\n'+gen())
            files['app/BUILD.bazel'] = ('load("//defs:m.bzl","m")\n'
                'package_group(name="friends",packages=["//app"])\n'
                'm(name="demo",visibility=[":friends"])\n'
                'filegroup(name="consumer",srcs=[":demo"])\n')
            target = '//app:consumer'
            queries = [('//app:friends', ['//app:friends'])]
        elif probe == 'normalized-fresh-list':
            body = ('if type(visibility) != "list" or [type(x) for x in visibility] != ["Label" for x in visibility]:\n'
                '    fail("VISIBILITY_NOT_TYPED_LIST")\n'
                'write(name=name,out=name+".out",text="|".join([x.package+":"+x.name for x in visibility]),visibility=visibility)\n')
            files['defs/m.bzl'] = macro(body, prefix=RULE)
            files['app/BUILD.bazel'] = ('load("//defs:m.bzl","m")\npackage(default_visibility=["//visibility:public"])\n'
                'm(name="none",visibility=None)\nm(name="empty",visibility=[])\n'
                'm(name="recursive",visibility=["//:__subpackages__"])\n'
                'v=["//z:__pkg__","//app:__pkg__","//visibility:private","//z:__pkg__","//app:__pkg__"]\n'
                'm(name="duplicates",visibility=v)\nv.append("//a:__pkg__")\n'
                'm(name="fresh",visibility=v)\nm(name="public",visibility=["//z:__pkg__","//visibility:public"])\n'
                'filegroup(name="demo",srcs=[":none",":empty",":recursive",":duplicates",":fresh",":public"])\n')
            expected = {'none.out': b'visibility:public', 'empty.out': b'app:__pkg__',
                'recursive.out': b':__subpackages__|app:__pkg__',
                'duplicates.out': b'app:__pkg__|app:__pkg__|z:__pkg__|z:__pkg__',
                'fresh.out': b'a:__pkg__|app:__pkg__|app:__pkg__|z:__pkg__|z:__pkg__',
                'public.out': b'visibility:public'}
            actions = 6
        elif probe in ('late-default', 'late-default-private'):
            files['app/BUILD.bazel'] += ('genrule(name="ordinary",outs=["ordinary.out"],cmd="printf ordinary > $@")\n'
                'package(default_visibility=["//visibility:public"])\n')
            files['other/BUILD.bazel'] = ('genrule(name="result",srcs=["//app:ordinary"],outs=["result.out"],cmd="cat $(SRCS) > $@")\n'
                'filegroup(name="denied",srcs=["//app:demo"])\n')
            target, expected, actions = '//other:result', {'result.out': b'ordinary'}, 2
            # The callback checks its earlier snapshot independently of late ordinary visibility.
            files['defs/m.bzl'] = macro('if [x.package+":"+x.name for x in visibility] != ["app:__pkg__"]:\n    fail("LATE_DEFAULT_CHANGED_SNAPSHOT")\n'+gen())
            if probe == 'late-default-private':
                target, diagnostic = '//other:denied', 'visibility'
        elif probe == 'same-definition':
            body = ('if name == "producer":\n'+common.indent(gen(visibility=None))
                +'else:\n'+common.indent(gen(srcs='[":producer"]')))
            files['defs/m.bzl'] = macro(body)
            files['app/BUILD.bazel'] = 'load("//defs:m.bzl","m")\nm(name="producer")\nm(name="demo")\n'
            actions = 2
        elif probe == 'caller-to-private':
            files['defs/m.bzl'] = macro(gen(visibility='None'))
            files['app/BUILD.bazel'] = ('load("//defs:m.bzl","m")\npackage(default_visibility=["//visibility:public"])\n'
                'm(name="demo")\nfilegroup(name="consumer",srcs=[":demo"])\n')
            target, diagnostic = '//app:consumer', 'visibility'
        elif probe.startswith('ordinary-'):
            kind = probe.removeprefix('ordinary-')
            files['app/data'] = 'ordinary-private'
            files['app/BUILD.bazel'] = ('load("//defs:m.bzl","m")\n'
                'exports_files(["data"],visibility=["//visibility:private"])\n'
                'genrule(name="ordinary",outs=["ordinary.out"],cmd="printf private > $@")\nm(name="demo")\n')
            source = {'rule': ':ordinary', 'output': ':ordinary.out', 'file': ':data'}[kind]
            files['defs/m.bzl'] = macro(gen(srcs=repr([source])))
            diagnostic = 'visibility'
        elif probe == 'different-definition':
            files['defs/m.bzl'] = macro(gen(visibility=None))
            files['other/BUILD.bazel'] = ''
            files['other/m.bzl'] = macro(gen(srcs='[":producer"]'))
            files['app/BUILD.bazel'] = ('load("//defs:m.bzl",producer="m")\nload("//other:m.bzl","m")\n'
                'producer(name="producer")\nm(name="demo")\n')
            diagnostic = 'visibility'
        elif probe == 'factory-not-lexical':
            factory_fixture(files)
            files['defs/BUILD.bazel'] = 'filegroup(name="consumer",srcs=["//app:demo"])\n'
            files['app/BUILD.bazel'] = files['app/BUILD.bazel'].replace('m(name="demo")', 'm(name="demo",visibility=[])')
            target, diagnostic = '//defs:consumer', 'visibility'
        else:
            files['MODULE.bazel'] += 'bazel_dep(name="dep",version="1.0")\nlocal_path_override(module_name="dep",path="dep")\n'
            files['dep/MODULE.bazel'] = 'module(name="dep",version="1.0")\n'
            files['dep/defs/BUILD.bazel'] = ''
            files['dep/defs/m.bzl'] = macro(gen(visibility=None))
            files['app/BUILD.bazel'] = 'load("@dep//defs:m.bzl","m")\nm(name="demo")\n'
            files['defs/BUILD.bazel'] = 'filegroup(name="consumer",srcs=["//app:demo"])\n'
            target, diagnostic = '//defs:consumer', 'visibility'
    elif name == 'sources':
        files.pop('defs/m.bzl')
        files['app/m.bzl'] = macro(gen(srcs='[":physical"]'))+('def _unselected(ctx):\n    fail("UNSELECTED_IMPL_EXECUTED")\n'
            'establish=rule(implementation=_unselected,attrs={"dep":attr.label(default=":physical",allow_single_file=True)})\n')
        files['app/physical'] = 'physical-data'
        load = 'load(":m.bzl","m","establish")\n'
        call = 'm(name="demo")\n'
        before = {'explicit-export': 'exports_files(["physical"])\n',
            'ordinary-before': 'filegroup(name="unselected",srcs=["physical"])\n'}.get(probe, '')
        after = {'ordinary-after': 'filegroup(name="unselected",srcs=["physical"])\n',
            'custom-default-unselected': 'establish(name="unselected")\n'}.get(probe, '')
        files['app/BUILD.bazel'] = load+before+call+after
        expected = {'demo.out': b'physical-data'}
        if probe == 'macro-only':
            diagnostic = 'missing-target'
            queries = [('//app:all', ['//app:demo'])]
            negative_queries = ['//app:physical']
        elif probe == 'reserved-name':
            files['app/m.bzl'] = macro('pass\n')
            files['app/demo'] = 'reserved-physical'
            files['app/BUILD.bazel'] = 'load(":m.bzl","m")\nm(name="demo")\nfilegroup(name="consumer",srcs=[":demo"])\n'
            queries = [('//app:all', ['//app:consumer'])]
            target, diagnostic = '//app:consumer', 'missing-target'
            negative_queries = ['//app:demo']
    elif name == 'namespace':
        if probe == 'dynamic-artifact':
            files['defs/m.bzl'] = macro('write(name=name,out="outside_dynamic",text="dynamic-data",visibility=visibility)\n', prefix=RULE)
            expected = {'outside_dynamic': b'dynamic-data'}
        elif probe == 'unselected-invalid':
            files['defs/m.bzl'] = macro(gen('"outside"', '"outside.out"')+gen()+gen('name+"_"', 'name+"_.out"'))
            queries = [('//app:all', ['//app:demo', '//app:demo_', '//app:outside'])]
        elif probe == 'invalid-generator':
            files['defs/m.bzl'] = macro(gen('"outside"', '"demo.out"'))
            target, diagnostic = '//app:demo.out', 'namespace'
            queries = [('//app:all', ['//app:outside']), ('//app:demo.out', ['//app:demo.out'])]
        elif probe == 'empty-suffix':
            files['defs/m.bzl'] = macro(gen('name+"_"', 'name+"_.out"'))
            target, diagnostic = '//app:demo_', 'namespace'
            queries = [('//app:all', ['//app:demo_'])]
        else:
            files['defs/m.bzl'] = macro(gen(out='"outside"'))
            expected = {'outside': b'macro-data'}
            queries = [('//app:all', ['//app:demo']), ('//app:outside', ['//app:outside'])]
            if probe.startswith('outside-output-'):
                target = ['//app:outside', '//app:demo'] if probe.endswith('first') else ['//app:demo', '//app:outside']
                diagnostic = 'namespace'
    elif name == 'collisions':
        files['defs/m.bzl'] = macro('pass\n')
        declaration = {'macro': 'm(name="demo")\n', 'rule': 'filegroup(name="demo")\n',
            'output': 'genrule(name="other",outs=["demo"],cmd="printf never > $@")\n',
            'export': 'exports_files(["demo"])\n'}[probe.split('-')[0]]
        files['app/demo'] = 'physical'
        body = declaration+'m(name="demo")\n' if probe.endswith('before') else 'm(name="demo")\n'+declaration
        files['app/BUILD.bazel'] = 'load("//defs:m.bzl","m")\n'+body
        target, diagnostic = '//app:all', 'collision'
        markers = ['MACRO_READY']
    elif name == 'typing':
        if probe in ('missing-name', 'integer-name', 'positional', 'unknown-none'):
            call = {'missing-name': 'm()', 'integer-name': 'm(name=1)', 'positional': 'm("demo")',
                'unknown-none': 'm(name="demo",unknown=None)'}[probe]
            files['app/BUILD.bazel'] = 'load("//defs:m.bzl","m")\n'+call+'\n'
            diagnostic, markers = probe, ['MACRO_READY']
        elif probe == 'return-value':
            files['defs/m.bzl'] = macro('return 7\n')
            diagnostic = 'return'
        elif probe == 'unexported':
            files['defs/m.bzl'] = ('def _impl(name,visibility):\n    pass\n'
                'container=[macro(implementation=_impl)]\ndef m(**kwargs):\n    container[0](**kwargs)\n'+READY)
            diagnostic, markers = 'unexported', ['MACRO_READY']
        elif probe.endswith('-implementation'):
            implementation = {'builtin-implementation': 'str', 'len-implementation': 'len', 'provider-implementation': 'provider()'}[probe]
            files['defs/m.bzl'] = 'm=macro(implementation='+implementation+')\n'
            diagnostic, markers = 'implementation', []
        else:
            files['defs/m.bzl'] = macro('visibility.append(Label("//other:__pkg__"))\n')
            diagnostic = 'frozen'
    elif name == 'phase':
        if probe == 'constructor-build':
            files['defs/m.bzl'] += 'def construct():\n    return macro(implementation=_impl)\n'
            files['app/BUILD.bazel'] = 'load("//defs:m.bzl","construct")\nconstruct()\n'
            diagnostic, markers = 'constructor-phase', ['MACRO_READY']
        elif probe == 'invocation-init':
            files['defs/m.bzl'] += 'm(name="initialization")\n'
            diagnostic, markers = 'invocation-phase', ['MACRO_READY']
        elif probe in ('repository-invocation', 'extension-invocation'):
            files['defs/m.bzl'] += ('def _callback(ctx):\n    print("CONTEXT_CALLBACK")\n    m(name="forbidden")\n'
                +('r=repository_rule(implementation=_callback)\n' if probe.startswith('repository') else
                  'e=module_extension(implementation=_callback)\n'))
            files['MODULE.bazel'] += ('r=use_repo_rule("//defs:m.bzl","r")\nr(name="made")\n'
                if probe.startswith('repository') else 'e=use_extension("//defs:m.bzl","e")\nuse_repo(e,"made")\n')
            target, diagnostic, markers = '@made//:data', 'invocation-phase', ['MACRO_READY', 'CONTEXT_CALLBACK']
        else:
            function = probe.split('-')[0]
            call = 'native.glob(["*"])' if function == 'glob' else 'native.package(default_visibility=["//visibility:public"])'
            files['defs/m.bzl'] = macro('helper()\n', prefix='def helper():\n    '+call+'\n')
            diagnostic = function+'-phase'
    elif name == 'native-boundary':
        if probe in ('user-attrs', 'inheritance', 'finalizer'):
            kwargs = {'user-attrs': ',attrs={"x":attr.string()}', 'inheritance': ',inherit_attrs="common"', 'finalizer': ',finalizer=True'}[probe]
            files['defs/m.bzl'] = macro('pass\n', 'implementation=_impl'+kwargs)
            diagnostic, markers = probe, []
        elif probe == 'nested':
            files['defs/m.bzl'] = macro('pass\n').replace('m=macro(', 'child=macro(')+macro('child(name=name+"_child")\n')
            diagnostic = probe
        elif probe == 'exports-files':
            files['defs/m.bzl'] = macro('native.exports_files([name])\n')
            files['app/demo'] = 'physical'
            diagnostic = probe
        elif probe == 'name-none':
            files['app/BUILD.bazel'] = 'load("//defs:m.bzl","m")\nm(name=None)\n'
            diagnostic, markers = 'integer-name', ['MACRO_READY']
        else:
            files['defs/m.bzl'] = macro('if hasattr(native,"existing_rule") or hasattr(native,"existing_rules"):\n    fail("BUILD_INTROSPECTION_LEAK")\n'+gen())
    return dict(files={p: raw.encode() if isinstance(raw, str) else raw for p, raw in files.items()},
        target=target, expected=expected, actions=actions, diagnostic=diagnostic, queries=queries,
        negative_queries=negative_queries, markers=markers,
        failure_command='query' if name == 'collisions' else 'build', genuine=False)


def factory_fixture(files):
    files['defs/m.bzl'] = ('def factory():\n    def impl(name,visibility):\n'
        +common.indent(common.indent(CALLBACK+gen(srcs='["//b:secret"]')))
        +'    return macro(implementation=impl,attrs={},doc=None)\n')
    files['b/BUILD.bazel'] = 'exports_files(["data"],visibility=["//visibility:private"])\nfilegroup(name="secret",srcs=["data"])\n'
    files['b/data'] = 'factory-owner-b'
    files['b/m.bzl'] = 'load("//defs:m.bzl","factory")\n_private=factory()\nm=_private\n'+READY
    files['alias/BUILD.bazel'] = ''
    files['alias/m.bzl'] = 'load("//b:m.bzl",original="m")\nm=original\n'
    files['app/BUILD.bazel'] = 'load("//alias:m.bzl","m")\nm(name="demo")\n'


DIAGNOSTICS = {
    'visibility': r'(?:visibility (?:error|violation)|not visible (?:from|to))',
    'missing-target': r'(?:no such target|target[^\n]*not declared|missing (?:declared )?target)',
    'namespace': r'(?:violates macro naming rules|(?:macro|symbolic)[^\n]*(?:namespace|naming))',
    'collision': r'(?:conflicts with an existing (?:macro|target)|(?:duplicate|collision|already (?:exists|declared))[^\n]*(?:macro|target|demo)|(?:macro|target)[^\n]*(?:conflict|duplicate|already))',
    'missing-name': r'(?:missing[^\n]*(?:mandatory|required)[^\n]*name|name[^\n]*(?:required|mandatory|missing))',
    'integer-name': r'(?:name[^\n]*(?:string|str|None)|(?:string|str)[^\n]*(?:name|None))',
    'positional': r'(?:unexpected positional|(?:macro|symbolic)[^\n]*positional|positional[^\n]*(?:not|keyword)|^error: Wrong number of positional arguments, expected 0, got 1$)',
    'unknown-none': r'(?:no such attribute [\'"]unknown|(?:unknown|unexpected|extra)[^\n]*(?:unknown|attribute|keyword))',
    'return': r'(?:macro[^\n]*return[^\n]*(?:None|none)|macro[^\n]*(?:non-None|non.None))',
    'unexported': r'(?:macro[^\n]*(?:not been exported|not exported|unexported|must be exported before invocation)|unexported[^\n]*macro)',
    'implementation': r'implementation[^\n]*(?:Starlark|function|def|callable)',
    'frozen': r'(?:trying to mutate a frozen list|Cannot mutate frozen|^error: Immutable$)',
    'constructor-phase': r'macro\(\)[^\n]*(?:only|initialization|top.level)',
    'invocation-phase': r'(?:symbolic macro[^\n]*(?:only|BUILD)|macro[^\n]*only[^\n]*BUILD)',
    'glob-phase': r'glob\(\)[^\n]*(?:only[^\n]*(?:BUILD|legacy)|not allowed inside a symbolic macro)',
    'package-phase': r'package\(\)[^\n]*(?:only|macro|BUILD)',
    'user-attrs': r'(?:attrs|attributes|user.attrs)[^\n]*(?:unsupported|not supported|empty)',
    'inheritance': r'inherit_attrs[^\n]*(?:unsupported|not supported|None)',
    'finalizer': r'finalizer[^\n]*(?:unsupported|not supported|False)',
    'nested': r'(?:nested[^\n]*(?:macro|unsupported)|macro[^\n]*(?:nested|nesting)[^\n]*(?:unsupported|not supported))',
    'exports-files': r'exports_files[^\n]*(?:unsupported|not supported|macro)',
}


def error_lines(text):
    # Match actual error diagnostics, never a source excerpt containing keywords.
    return '\n'.join(line for line in text.splitlines() if re.match(r'(?i)^(?:error(?: in [^:]+)?|bazel-rs|caused by):', line))


def require_rejection(record, diagnostic):
    code, text = record['returncode'], record['stderr']
    unrelated = common.UNRELATED.pattern
    # Genuine repository phase errors have an intentional failed-to-fetch trailer.
    checked = re.sub(r'which failed to fetch\. no such package[^\n]*symbolic macro[^\n]*', '', text)
    stale = re.search(r'(?:[\'"`]macro[\'"`][^\n]*(?:not found|not defined)|no (?:field|attribute)[^\n]*macro)', text, re.I)
    if (type(code) is not int or not 0 < code < 128 or code in (37, 101, 124, 125, 126, 127)
            or record['timed_out'] or any(record.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or record['stdout'].strip() or re.search(unrelated, checked, re.I) or stale
            or not re.search(DIAGNOSTICS[diagnostic], error_lines(text), re.I | re.M)):
        raise ValueError('expected controlled symbolic macro rejection: '+diagnostic)


def marker_count(record, marker):
    return len(re.findall(r'(?m)^(?:DEBUG: [^\n]*?:\d+:\d+: )?'+re.escape(marker)+r'\s*$', record['stderr']))


def phases_for(item, reference_only=False):
    return common.phases_for(item, reference_only)


def steps_for(value, engine):
    steps = [('query', target, []) for target, _ in value['queries']]
    if value['diagnostic']:
        return steps+[('query', target, []) for target in value['negative_queries']]+[(value['failure_command'], value['target'], [])]
    for _ in range(2):
        steps.append(('build', value['target'], []))
        if engine == 'reference':
            steps.append(('cquery', value['target'], ['--output=files']))
    return steps


def command_argv(args, reference, engine, cache, registry, command, target, extra):
    targets = target if isinstance(target, list) else [target]
    flags = ['--incompatible_autoload_externally=', '--noshow_progress'] if engine == 'reference' else []
    return extensions.command_argv(args, reference, engine, cache, registry, command, targets[-1],
        list(extra)+targets[:-1], flags)


def action_store(cache):
    """Inspect actual action roots, including staging, never an invented cache path."""
    return {name: dict(path=str(cache / name), exists=(cache / name).exists(),
        entries=sorted(str(p.relative_to(cache / name)) for p in (cache / name).rglob('*')))
        for name in ('actions', 'work')}


def no_actions(store, cache):
    if set(store) != {'actions', 'work'}:
        raise ValueError('missing real action-store inspection')
    for name, value in store.items():
        if (set(value) != {'path', 'exists', 'entries'} or value['path'] != str(cache / name)
                or type(value['exists']) is not bool or value['entries']):
            raise ValueError('rejected preflight executed/published actions or inspected wrong path')


def materialize(workspace, value):
    return existing.local.materialize(workspace, value)


def run_phase(args, runner, scratch, workspace, reference, item, probe, engine):
    value = fixture(item['name'], probe)
    paths = materialize(workspace, value)
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    phase = dict(probe=probe, engine=engine, cache=str(cache), runs=[], outputs=[], status='FAIL',
        inputs_before=snapshot(paths), action_store_before=action_store(cache) if engine == 'native' else None)
    try:
        steps = steps_for(value, engine)
        for index, (command, target, extra) in enumerate(steps):
            record = runner.run(item['name']+'.'+probe+'.'+engine+'.'+str(index),
                command_argv(args, reference, engine, cache, scratch / 'registry', command, target, extra), workspace)
            phase['runs'].append(record['id'])
            if value['diagnostic'] and index >= len(value['queries']):
                require_rejection(record, value['diagnostic'])
            else:
                success(record)
                if command == ('build' if engine == 'native' else 'cquery'):
                    paths_out = (direct.native_paths(record, target, scratch) if engine == 'native' else
                        reference_output_paths(record['stdout'], workspace, reference.output_base))
                    phase['outputs'].append([evidence(p) for p in paths_out])
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    finally:
        phase['inputs_after'] = snapshot(paths)
        phase['action_store_after'] = action_store(cache) if engine == 'native' else None
    return phase


def validate_phase(phase, report, args, workspace, reference, item):
    probe, engine = phase['probe'], phase['engine']
    if (probe, engine) not in phases_for(item, args.reference_only):
        raise ValueError('unknown phase/engine or reference claim for native-only boundary')
    value = fixture(item['name'], probe)
    if phase['status'] != 'PASS' or phase.get('error') or phase['inputs_before'] != phase['inputs_after']:
        raise ValueError('failed phase or changed source inputs')
    planned = {str(workspace / p): raw for p, raw in value['files'].items()}
    if set(planned) != set(phase['inputs_before']):
        raise ValueError('missing or unplanned source inputs')
    for path, raw in planned.items():
        e = phase['inputs_before'][path]
        if e['path'] != path or e['kind'] != 'file' or e['mode'] != 0o644 or not valid_evidence(e, raw):
            raise ValueError('source differs from immutable fixture plan')
    scratch = Path(report['scratch_directory'])
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    if phase['cache'] != str(cache):
        raise ValueError('unplanned native cache')
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
    output_index, build_index = 0, 0
    records = []
    for index, (rid, (command, target, extra)) in enumerate(zip(phase['runs'], steps)):
        record = report['runs'][rid]
        records.append(record)
        if (type(rid) is not int or record['id'] != rid
                or record['argv'] != command_argv(args, reference, engine, cache, scratch / 'registry', command, target, extra)
                or record['cwd'] != str(workspace) or record['timeout_seconds'] != args.timeout
                or record['name'] != item['name']+'.'+probe+'.'+engine+'.'+str(index)):
            raise ValueError('command provenance mismatch')
        if value['diagnostic'] and index >= len(value['queries']):
            require_rejection(record, value['diagnostic'])
            continue
        success(record)
        if index < len(value['queries']):
            if sorted(record['stdout'].splitlines()) != value['queries'][index][1]:
                raise ValueError('query did not return exact planned rules/outputs')
            continue
        if command == 'build':
            if engine == 'native':
                common.action_counts(record, (value['actions'], 0) if build_index == 0 else (0, value['actions']))
            build_index += 1
        if command != ('build' if engine == 'native' else 'cquery'):
            continue
        paths = (direct.native_paths(record, target, scratch) if engine == 'native' else
            reference_output_paths(record['stdout'], workspace, reference.output_base))
        entries = phase['outputs'][output_index]
        if (list(map(str, paths)) != [e['path'] for e in entries] or len(entries) != len(value['expected'])
                or {Path(e['path']).name for e in entries} != set(value['expected'])):
            raise ValueError('output evidence not tied to exact CLI result')
        for e in entries:
            if e['kind'] != 'file' or e['mode'] != 0o555 or not valid_evidence(e, value['expected'][Path(e['path']).name], True):
                raise ValueError('consumer bytes or executable mode mismatch')
        output_index += 1
    for marker in value['markers']:
        # First real command after source replacement must load this fixture.
        # Genuine server can suppress later prints; native fresh CLIs cannot.
        if not marker_count(records[0], marker):
            raise ValueError('missing actual initialization/callback marker: '+marker)
        if engine == 'native' and any(not marker_count(r, marker) for r in records):
            raise ValueError('fresh CLI skipped actual callback: '+marker)
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
    parser.set_defaults(output=ROOT / 'artifacts/symbolic-macros-validation.json')
    return parser


def report_passes(report):
    try:
        args = argument_parser().parse_args([])
        if set(report['configuration']) != set(vars(args)):
            return False
        for key, value in report['configuration'].items():
            setattr(args, key, Path(value) if key in ('native', 'reference', 'output', 'scratch_parent') else value)
        expected_status = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS'
        if report['status'] not in ('FAIL', expected_status) or report['reference_source_pin'] != SOURCE_PIN:
            return False
        if (report['planned_checks'] != CASEPLAN or [(c['name'], c['kind']) for c in report['checks']] != PLAN
                or report['plan_sha256'] != digest(json.dumps(CASEPLAN, sort_keys=True).encode())
                or report['limitations'] != LIMITATIONS or report['errors'] or report['cleanup'] or not report['scratch_cleaned']
                or not report['reference_shutdown_succeeded'] or report['sources_before'] != report['sources_after']
                or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(p == e['path'] and e['kind'] == 'file' and valid_evidence(e) for p, e in report['sources_before'].items())
                or report['executables'] != report['executables_after']
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['reference_release'] != 'bazel 9.3.0'):
            return False
        engines = ('reference',) if args.reference_only else ('reference', 'native')
        if set(report['executables']) != set(engines):
            return False
        if not args.reference_only and (not re.fullmatch('[0-9a-f]{64}', args.native_sha256 or '')
                or report['executables']['native']['sha256'] != args.native_sha256):
            return False
        if (report['completed_phases'] != sum(len(phases_for(c, args.reference_only)) for c in CASEPLAN)
                or report['completed_checks'] != sum(bool(phases_for(c, args.reference_only)) for c in CASEPLAN)):
            return False
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        if report['cleanup_receipt'] != dict(path=str(scratch), removed=True):
            return False
        registry = {str(scratch / 'registry' / Path(p).relative_to(REGISTRY)): e for p, e in report['sources_before'].items() if Path(p).is_relative_to(REGISTRY)}
        if report['registry_before'] != report['registry_after'] or set(registry) != set(report['registry_before']):
            return False
        if any(report['registry_before'][p] != dict(e, path=p) for p, e in registry.items()):
            return False
        reference = DirectReference(args.reference, workspace, scratch, None, 'symbolic-macros')
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for r in runs:
            engine = next((e for e in engines if str(getattr(args, e)) == r['argv'][0]), None)
            if (engine is None or r['executable_sha256'] != report['executables'][engine]['sha256']
                    or 'environment' in r or 'environment_keys' in r):
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(r[field+'_base64'], validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != r[field]):
                    return False
        for record, name, argv, timeout in (
                (runs[0], 'reference.version', reference.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], 'symbolic-macros.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS), min(args.timeout, 15))):
            if record['name'] != name or record['argv'] != argv or record['cwd'] != str(workspace) or record['timeout_seconds'] != timeout:
                return False
            success(record)
        if runs[0]['stdout'].strip() != 'bazel 9.3.0':
            return False
        used = [0]
        for check in report['checks']:
            validate_check(check, report, args, workspace, reference)
            used.extend(rid for phase in check['phases'] for rid in phase['runs'])
        used.append(len(runs)-1)
        return used == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def main(argv=None):
    args = argument_parser().parse_args(argv)
    if args.reference_only and args.output == ROOT / 'artifacts/symbolic-macros-validation.json':
        args.output = ROOT / 'artifacts/symbolic-macros-reference.json'
    for field in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.output in [args.native, args.reference, *SOURCES] or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests', 'src')):
        raise ValueError('report cannot overwrite source or executable')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
        planned_checks=CASEPLAN, plan_sha256=digest(json.dumps(CASEPLAN, sort_keys=True).encode()),
        reference_source_pin=SOURCE_PIN,
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        scratch_cleaned=False, reference_shutdown_succeeded=False,
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), limitations=LIMITATIONS)
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = common.executable_evidence(args)
        scratch = Path(tempfile.mkdtemp(prefix='symbolic-macros-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(MODULE)
        shutil.copytree(REGISTRY, scratch / 'registry')
        report['registry_before'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        reference = DirectReference(args.reference, workspace, scratch, runner, 'symbolic-macros')
        reference.started = True
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
                reference.shutdown()
                success(report['runs'][-1])
                report['reference_shutdown_succeeded'] = not report['cleanup']
            except Exception as exc:
                report['errors'].append('shutdown: '+str(exc))
        if scratch is not None:
            try:
                report['registry_after'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
                shutil.rmtree(scratch)
                report['scratch_cleaned'] = not scratch.exists()
                report['cleanup_receipt'] = dict(path=str(scratch), removed=report['scratch_cleaned'])
            except OSError as exc:
                report['errors'].append('cleanup: '+str(exc))
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['executables_after'] = common.executable_evidence(args)
        except Exception as exc:
            report['errors'].append('final evidence: '+str(exc))
        report['completed_phases'] = sum(p['status'] == 'PASS' for c in report['checks'] for p in c['phases'])
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return 130 if interrupted else 0 if report['status'] in ('PASS', 'REFERENCE_ONLY_PASS') else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
