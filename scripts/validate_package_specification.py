#!/usr/bin/env python3
"""Bounded package-group/provider/visibility differential; never full Bazel parity.

Reference-only preparation never executes native. Native execution requires an
explicit frozen SHA. Fixtures use genuine providers, real actions and the existing
file registry; the reference is never a fallback. No upstream crash is replayed.
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

import validate_symbolic_macros as macros

ROOT, COMMON_FLAGS = macros.ROOT, macros.COMMON_FLAGS
common, direct = macros.common, macros.direct
digest, evidence, snapshot, valid_evidence = macros.digest, macros.evidence, macros.snapshot, macros.valid_evidence
success, DirectReference = macros.success, macros.DirectReference
REFERENCE_SHA256, SOURCE_PIN, REGISTRY = macros.REFERENCE_SHA256, macros.SOURCE_PIN, macros.REGISTRY
SOURCES = list(dict.fromkeys([ROOT / 'scripts/validate_package_specification.py',
    ROOT / 'tests/test_validate_package_specification.py', *macros.SOURCES]))
CASEPLAN = [
    common.case('producer', ('core', 'equal-distinct', 'external-canonical', 'label-includes', 'extension-global')),
    common.case('contains', ('constructor', 'keyword', 'type', 'missing', 'extra', 'relative', 'immutable')),
    common.case('algebra', ('subtree', 'root', 'public', 'private-negative', 'union-diamond', 'repo-special',
        'negative-public', 'negative-private', 'target-suffix', 'trailing-slash')),
    common.case('constraints', ('and', 'or', 'any', 'source', 'output', 'empty-rule', 'list-missing',
        'none', 'legacy', 'mixed', 'instance', 'unexported-after-empty')),
    common.case('placement', ('plain', 'default', 'and-missing', 'or-missing', 'substring', 'empty-erases', 'srcs')),
    common.case('visibility', ('rule-allow', 'rule-deny', 'output-allow', 'output-deny', 'source-allow', 'source-deny', 'root-missing',
        'same-package-missing', 'source-missing', 'implicit-source-missing', 'wrong-kind',
        'macro-definition-allow', 'macro-definition-deny', 'macro-producer-allow', 'macro-producer-deny',
        'public-normalized', 'external-isolation')),
    common.case('macro', ('valid-public', 'invalid', 'cached-invalid', 'rule-before', 'rule-after',
        'output-before', 'output-after', 'reserved-before', 'reserved-after', 'initialization', 'extension', 'repository')),
    common.case('graph', ('standalone-query', 'missing', 'wrong-kind', 'cycle')),
    common.case('native-boundary', ('literal-space', 'literal-angle', 'literal-stars', 'depth-cached',
        'return-info', 'nested-info'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASEPLAN]
LIMITATIONS = [
    'Bounded current ASCII paths and target configuration; no full Bazel/application/performance claim.',
    'Literal spaces, angle brackets and stars are native profile boundaries, not upstream grammar errors.',
    'Depth 257 is a native graph budget, not an upstream rejection; no expanded query language.',
    'Direct PackageSpecificationInfo return crashes genuine 9.3; never replayed. Nested payload rejection is native-only.',
    'No BUILD existing_rule(s), transitions, toolchains or arbitrary configured analysis claim.',
    'Reference-only preparation cannot report native parity PASS.',
]
MODULE = 'module(name="package_contract")\n'
READY, ENTERED, WRONG = 'PACKAGE_READY', 'PACKAGE_CONSUMER', 'WRONG_KIND_CALLBACK'
KEYS = 'Custom=provider()\nOther=provider()\nNotPackageSpecificationInfo=provider()\n'
ATTR = 'attr.label(providers=[PackageSpecificationInfo])'


def consumer(body, attrs=None, prefix=''):
    attrs = attrs or {'g': ATTR, 'gs': 'attr.label_list(providers=[PackageSpecificationInfo])'}
    return (prefix+'def _impl(ctx):\n'+common.indent('print("'+ENTERED+'")\n'+body)+
        'check=rule(implementation=_impl,attrs={'+','.join(repr(k)+':'+v for k, v in attrs.items())+'})\n'
        'print("'+READY+'")\n')


def write(expression):
    return ('out=ctx.actions.declare_file(ctx.label.name+".out")\n'
        'ctx.actions.write(out,'+expression+',is_executable=True)\n'
        'return [DefaultInfo(files=depset([out]))]\n')


def group(name='g', packages=('//foo',), includes=()):
    return 'package_group(name='+repr(name)+',packages='+repr(list(packages))+',includes='+repr(list(includes))+')\n'


def external(files):
    files['MODULE.bazel'] += ('bazel_dep(name="ext",version="1.0",repo_name="alias")\n'
        'local_path_override(module_name="ext",path="ext")\n')
    files['ext/MODULE.bazel'] = 'module(name="ext",version="1.0")\n'
    files['ext/BUILD.bazel'] = group(packages=['//foo'])


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    body = ('g=ctx.attr.g\np=g[PackageSpecificationInfo]\n'
        +write('str([p.contains(Label("//foo:a")),p.contains("//foo:b"),p.contains("//foo/sub:x"),'
                 'DefaultInfo in g,g[DefaultInfo].files.to_list()==[],g.files.to_list()==[], '
                 'ctx.attr.gs[0][PackageSpecificationInfo].contains("//foo:x")])'))
    files = {'MODULE.bazel': MODULE, 'BUILD.bazel': '', 'defs/BUILD.bazel': '',
        'defs/check.bzl': consumer(body), 'app/BUILD.bazel':
        'load("//defs:check.bzl","check")\n'+group()+'check(name="check",g=":g",gs=[":g"])\n'}
    value = dict(target='//app:check', expected={'check.out': b'[True, True, False, True, True, True, True]'},
        actions=1, diagnostic=None, stage='success', queries=[], genuine=False,
        markers=[READY], callback=True, forbidden=[WRONG])

    def reject(diagnostic, stage='preflight'):
        value.update(diagnostic=diagnostic, stage=stage, expected={}, actions=0, callback=stage == 'callback')

    def membership(packages, expressions, expected, declarations='', dep=':g'):
        files['app/BUILD.bazel'] = ('load("//defs:check.bzl","check")\n'+declarations+group(packages=packages)
            +'check(name="check",g='+repr(dep)+')\n')
        files['defs/check.bzl'] = consumer('p=ctx.attr.g[PackageSpecificationInfo]\n'+write('str(['+','.join(expressions)+'])'))
        value['expected'] = {'check.out': expected.encode()}

    if name == 'producer':
        if probe == 'equal-distinct':
            files['app/BUILD.bazel'] += group('other', ['//bar'])+'check(name="equal",g=":g",gs=[":other",":g"])\n'
            files['defs/check.bzl'] = consumer('a=ctx.attr.g[PackageSpecificationInfo]\nb=ctx.attr.gs[0][PackageSpecificationInfo]\n'
                +write('str([a==b,{a:"same"}[b],a.contains("//foo:x"),b.contains("//foo:x"),ctx.attr.gs[1][PackageSpecificationInfo].contains("//foo:x")])'))
            value.update(target='//app:equal', expected={'equal.out': b'[True, "same", True, False, True]'})
        elif probe == 'external-canonical':
            external(files)
            membership([], ['p.contains("//foo:x")', 'p.contains("@alias//foo:x")',
                'p.contains(Label("@alias//foo:x"))', 'p.contains("@ext+//foo:x")',
                'p.contains("@@ext+//foo:x")'], '[False, False, True, True, True]', dep='@alias//:g')
        elif probe == 'label-includes':
            files['defs/check.bzl'] += ('def declare():\n'
                '    native.package_group(name="leaf",packages=("//foo",))\n'
                '    native.package_group(name="g",includes=(Label("//app:leaf"),":leaf"))\n')
            files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check","declare")\ndeclare()\ncheck(name="check",g=":g",gs=[":g"])\n'
        elif probe == 'extension-global':
            files['defs/ext.bzl'] = ('K=PackageSpecificationInfo\n'
                'def _repo(ctx):\n    ctx.file("BUILD.bazel",\'exports_files(["data"])\')\n'
                '    ctx.file("data","extension-provider",executable=False)\n'
                'r=repository_rule(implementation=_repo)\n'
                'def _ext(ctx):\n    if type(K)!="Provider" or K!=PackageSpecificationInfo:\n        fail("KEY_MISMATCH")\n'
                '    r(name="made")\ne=module_extension(implementation=_ext)\n')
            files['MODULE.bazel'] += 'e=use_extension("//defs:ext.bzl","e")\nuse_repo(e,"made")\n'
            files['app/BUILD.bazel'] = 'genrule(name="check",srcs=["@made//:data"],outs=["check.out"],cmd="cat $(SRCS) > $@")\n'
            value.update(expected={'check.out': b'extension-provider'}, markers=[], callback=False)
    elif name == 'contains':
        if probe == 'constructor':
            files['defs/check.bzl'] = 'p=PackageSpecificationInfo()\n'+files['defs/check.bzl']
            value['markers'] = []
            reject('constructor', 'loading')
        else:
            call = {'keyword': 'p.contains(target="//foo:x")', 'type': 'p.contains(1)',
                'missing': 'p.contains()', 'extra': 'p.contains("//foo:x","//foo:y")',
                'relative': 'p.contains(":x")', 'immutable': 'p.contains=None'}[probe]
            files['defs/check.bzl'] = consumer('p=ctx.attr.g[PackageSpecificationInfo]\n'+call+'\nreturn []\n')
            reject(probe, 'callback')
    elif name == 'algebra':
        external(files)
        if probe == 'subtree':
            membership(['//foo/...', '-//foo/private'], ['p.contains("//foo:x")', 'p.contains("//foo/sub:x")',
                'p.contains("//foobar:x")', 'p.contains("//foo/private:x")', 'p.contains("@@ext+//foo:x")'], '[True, True, False, False, False]')
        elif probe == 'root':
            membership(['//'], ['p.contains("//:x")', 'p.contains("//foo:x")'], '[True, False]')
        elif probe == 'public':
            membership(['public'], ['p.contains("//foo:x")', 'p.contains("@@ext+//foo:x")'], '[True, True]')
        elif probe == 'private-negative':
            membership(['private', '-//foo'], ['p.contains("//foo:x")', 'p.contains("//bar:x")'], '[False, False]')
        elif probe == 'union-diamond':
            declarations = group('leaf', ['//foo/private'])+group('left', [], [':leaf'])+group('right', [], [':leaf'])
            membership(['//foo/...', '-//foo/private'], ['p.contains("//foo/private:x")',
                'p.contains("//foo/ok:x")', 'p.contains("//bar:x")'], '[True, True, False]', declarations)
            files['app/BUILD.bazel'] = files['app/BUILD.bazel'].replace("includes=[])\ncheck", "includes=[':left',':right',':leaf'])\ncheck")
        elif probe == 'repo-special':
            membership(['@alias//foo:__pkg__', '@@ext+//bar:__subpackages__', '//...'],
                ['p.contains(Label("@alias//foo:x"))', 'p.contains("@@ext+//bar/sub:x")',
                 'p.contains("@@ext+//foobar:x")', 'p.contains("//main:x")'], '[True, True, False, True]')
        else:
            spec = {'negative-public': '-public', 'negative-private': '-private',
                'target-suffix': '//foo:target', 'trailing-slash': '//foo/'}[probe]
            files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check")\n'+group(packages=[spec])
            reject('negate' if probe.startswith('negative-') else 'grammar', 'loading')
    elif name == 'constraints':
        providers = {'and': '[DefaultInfo,Custom,Custom,platform_common.TemplateVariableInfo]',
            'or': '((Other,),(DefaultInfo,Custom,platform_common.TemplateVariableInfo))',
            'any': '[[],[Custom]]', 'source': '[Custom]', 'output': '[Custom]', 'empty-rule': '[Custom]',
            'list-missing': '[Custom]', 'none': 'None', 'legacy': '["PackageSpecificationInfo"]',
            'mixed': '[Custom,[Other]]', 'instance': '[Custom()]', 'unexported-after-empty': '[[],[provider()]]'}[probe]
        prefix = KEYS+'def _producer(ctx):\n    return [Custom(),platform_common.TemplateVariableInfo({"X":"x"})]\nproducer=rule(implementation=_producer)\n'
        attrs = {'g': 'attr.label(providers='+providers+',allow_files=True)',
            'gs': 'attr.label_list(providers='+providers+',allow_files=True)'}
        files['defs/check.bzl'] = consumer(write('str([[DefaultInfo in t,Custom in t] for t in [ctx.attr.g]+ctx.attr.gs])'), attrs, prefix)
        dep = ':producer' if probe in ('and', 'or', 'list-missing') else ':empty'
        declaration = 'producer(name="producer")\nfilegroup(name="empty")\n'
        if probe == 'source':
            dep, declaration = ':input', declaration+'exports_files(["input"])\n'
            files['app/input'] = 'source'
        elif probe == 'output':
            dep = ':generated'
            declaration += 'genrule(name="generate",outs=["generated"],cmd="printf generated > $@")\n'
            # The consumer examines the target's providers, not its file bytes:
            # the generating action is not reachable from the written output.
        files['app/BUILD.bazel'] = ('load("//defs:check.bzl","check","producer")\n'+declaration+
            'check(name="check",g='+repr(dep)+',gs='+repr([dep, ':empty'] if probe == 'list-missing' else [dep])+')\n')
        value['expected'] = {'check.out': b'[[True, True], [True, True]]' if probe in ('and', 'or') else b'[[True, False], [True, False]]'}
        if probe in ('empty-rule', 'list-missing'):
            reject('required')
        elif probe in ('none', 'legacy', 'mixed', 'instance', 'unexported-after-empty'):
            reject('export' if probe == 'unexported-after-empty' else 'constraint-shape', 'loading')
            value['markers'] = []
    elif name == 'placement':
        providers = {'plain': '', 'default': '[DefaultInfo]', 'and-missing': '[PackageSpecificationInfo,Custom]',
            'or-missing': '[[PackageSpecificationInfo,Custom],[Other]]', 'substring': '[NotPackageSpecificationInfo]',
            'empty-erases': '[[],[PackageSpecificationInfo]]', 'srcs': ''}[probe]
        attrs = {k: 'attr.'+kind+'('+('providers='+providers if providers else '')+')'
            for k, kind in [('g', 'label'), ('gs', 'label_list')]}
        files['defs/check.bzl'] = consumer('g=ctx.attr.g\n'+write('str([PackageSpecificationInfo in g,DefaultInfo in g,Custom in g,NotPackageSpecificationInfo in g,ctx.attr.gs[0][PackageSpecificationInfo].contains("//foo:x")])'), attrs, KEYS)
        value['expected'] = {'check.out': b'[True, True, False, False, True]'}
        if probe in ('plain', 'default', 'empty-erases', 'srcs'):
            reject('misplaced')
        if probe == 'srcs':
            files['app/BUILD.bazel'] = group()+'filegroup(name="check",srcs=[":g"])\n'
            value['markers'] = []
    elif name == 'graph':
        value.update(target='//app:g', expected={}, actions=0, callback=False)
        value['queries'] = [('//app:g', ['//app:g'])]
        if probe == 'standalone-query':
            value['queries'] += [('//app:all', ['//app:check']), ('//app/...', ['//app:check'])]
        else:
            includes = {'missing': ':absent', 'wrong-kind': ':wrong', 'cycle': ':other'}[probe]
            files['app/BUILD.bazel'] = 'load("//defs:check.bzl","wrong")\n'+group(includes=[includes])
            files['defs/check.bzl'] = ('def _wrong(ctx):\n    fail("'+WRONG+'")\nwrong=rule(implementation=_wrong)\nprint("'+READY+'")\n')
            files['app/BUILD.bazel'] += group('other', [], [':g']) if probe == 'cycle' else 'wrong(name="wrong")\n'
            reject({'missing': 'missing-target', 'wrong-kind': 'group-kind', 'cycle': 'cycle'}[probe])
    elif name == 'visibility':
        visibility_fixture(files, value, probe, reject)
    elif name == 'macro':
        macro_fixture(files, value, probe, reject)
    elif name == 'native-boundary':
        if probe.startswith('literal-'):
            spec = {'literal-space': '//foo bar', 'literal-angle': '//foo<bar', 'literal-stars': '//foo/**'}[probe]
            files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check")\n'+group(packages=[spec])
            reject('literal-boundary', 'loading')
        elif probe == 'depth-cached':
            files['app/BUILD.bazel'] = ''.join(group('g'+str(i), [], [':g'+str(i+1)] if i < 257 else []) for i in range(258))
            value.update(target=['//app:g1', '//app:g0'], markers=[])
            reject('depth')
        else:
            expression = 'p' if probe == 'return-info' else 'Custom(payload=[p])'
            files['defs/check.bzl'] = consumer('p=ctx.attr.g[PackageSpecificationInfo]\nreturn ['+expression+']\n', prefix=KEYS)
            reject('return-info', 'callback')
    if value['diagnostic'] and value['stage'] == 'loading':
        value['failure_command'] = 'query'
    else:
        value['failure_command'] = 'build'
    value['files'] = {p: raw.encode() if isinstance(raw, str) else raw for p, raw in files.items()}
    return value


def visibility_fixture(files, value, probe, reject):
    files['defs/check.bzl'] = consumer(write('"visible"'), {'g': 'attr.label(allow_files=True)'})
    files['lib/BUILD.bazel'] = (group('friends', ['//app'])+
        'genrule(name="item",outs=["item.out"],cmd="printf item > $@",visibility=[":friends"])\n'
        'exports_files(["data"],visibility=[":friends"])\n')
    files['lib/data'] = 'data'
    dep = '//lib:item.out' if probe.startswith('output-') else '//lib:data' if probe.startswith('source-') else '//lib:item'
    files['app/BUILD.bazel'] = 'load("//defs:check.bzl","check")\ncheck(name="check",g='+repr(dep)+')\n'
    # These are analysis visibility edges. The consumer writes independent bytes
    # and does not use prerequisite artifacts as action inputs.
    value.update(expected={'check.out': b'visible'}, actions=1)
    if probe in ('rule-deny', 'output-deny', 'source-deny'):
        files['lib/BUILD.bazel'] = files['lib/BUILD.bazel'].replace("['//app']", "['//other']")
        reject('visibility')
    elif probe in ('root-missing', 'same-package-missing', 'source-missing', 'implicit-source-missing', 'wrong-kind'):
        files['app/BUILD.bazel'] = ('load("//defs:check.bzl","check","wrong")\n'
            'check(name="check",visibility=[":absent"])\n'
            'check(name="same",g=":check")\n')
        files['defs/check.bzl'] += 'def _wrong(ctx):\n    fail("'+WRONG+'")\nwrong=rule(implementation=_wrong)\n'
        if probe == 'same-package-missing':
            value['target'] = '//app:same'
        elif probe in ('source-missing', 'implicit-source-missing'):
            files['app/input'] = 'input'
            files['app/BUILD.bazel'] = ('exports_files(["input"],visibility=[":absent"])\n' if probe == 'source-missing' else
                'package(default_visibility=[":absent"])\nfilegroup(name="eligible",srcs=["input"])\n')
            value.update(target='//app:input', markers=[])
        elif probe == 'wrong-kind':
            files['app/BUILD.bazel'] = files['app/BUILD.bazel'].replace(':absent', ':wrong')+'wrong(name="wrong")\n'
        value['queries'] = [(value['target'], [value['target']])]
        reject('group-kind' if probe == 'wrong-kind' else 'missing-target')
    elif probe.startswith('macro-definition-'):
        grant = '//defs' if probe.endswith('allow') else '//app'
        files['lib/BUILD.bazel'] = files['lib/BUILD.bazel'].replace("['//app']", repr([grant]))
        files['defs/check.bzl'] += ('def _macro(name,visibility):\n'
            '    check(name=name,g="//lib:item",visibility=visibility)\nm=macro(implementation=_macro)\n')
        files['app/BUILD.bazel'] = 'load("//defs:check.bzl","m")\nm(name="check")\n'
        if probe.endswith('deny'):
            reject('visibility')
    elif probe.startswith('macro-producer-'):
        files['defs/check.bzl'] += ('def _macro(name,visibility):\n'
            '    native.genrule(name=name,outs=[name+".out"],cmd="printf item > $@",visibility=visibility)\n'
            'm=macro(implementation=_macro)\n')
        files['lib/BUILD.bazel'] = ('load("//defs:check.bzl","m")\n'
            +group('friends', ['//app' if probe.endswith('allow') else '//other'])
            +'m(name="item",visibility=[":friends"])\n')
        if probe.endswith('deny'):
            reject('visibility')
    elif probe == 'public-normalized':
        files['defs/check.bzl'] += ('def _macro(name,visibility):\n'
            '    check(name=name,visibility=visibility)\nm=macro(implementation=_macro)\n')
        files['app/BUILD.bazel'] = 'load("//defs:check.bzl","m")\nm(name="check",visibility=[":absent","//visibility:public"])\n'
        value['actions'] = 1
    elif probe == 'external-isolation':
        external(files)
        files['ext/defs/BUILD.bazel'] = ''
        files['ext/defs/m.bzl'] = ('def _macro(name,visibility):\n'
            '    native.filegroup(name=name,srcs=["//lib:item"],visibility=visibility)\nm=macro(implementation=_macro)\n')
        # Absolute strings in native declarations resolve in the BUILD package.
        files['app/BUILD.bazel'] = 'load("@alias//defs:m.bzl","m")\nm(name="check")\n'
        files['lib/BUILD.bazel'] = files['lib/BUILD.bazel'].replace("['//app']", "['//defs']")
        value['markers'] = []
        reject('visibility')


def macro_fixture(files, value, probe, reject):
    files['defs/check.bzl'] = ('def _macro(name,visibility):\n'
        '    native.package_group(name=name+"_group",packages=["//foo"])\nm=macro(implementation=_macro)\n'
        +files['defs/check.bzl'])
    files['app/BUILD.bazel'] = ('load("//defs:check.bzl","m","check")\n'
        'package(default_visibility=["//visibility:private"])\nm(name="demo")\n'
        'check(name="check",g=":demo_group",gs=[":demo_group"])\n')
    if probe == 'valid-public':
        files['other/BUILD.bazel'] = 'load("//defs:check.bzl","check")\ncheck(name="check",g="//app:demo_group",gs=["//app:demo_group"])\n'
        value['target'] = '//other:check'
    elif probe in ('invalid', 'cached-invalid'):
        files['defs/check.bzl'] = files['defs/check.bzl'].replace('name=name+"_group"', 'name="outside"')
        files['app/BUILD.bazel'] = 'load("//defs:check.bzl","m")\nm(name="demo")\n'+group('parent', [], [':outside'])
        value['target'] = '//app:outside' if probe == 'invalid' else ['//app:parent', '//app:outside']
        value['queries'] = [('//app:outside', ['//app:outside'])]
        reject('namespace')
    elif probe.endswith(('-before', '-after')):
        kind, order = probe.split('-')
        declaration = {'rule': 'filegroup(name="demo_group")\n',
            'output': 'genrule(name="other",outs=["demo_group"],cmd="printf no > $@")\n',
            'reserved': 'm(name="demo_group")\n'}[kind]
        # The reserved macro need not emit its own name to reserve it.
        files['app/BUILD.bazel'] = 'load("//defs:check.bzl","m")\n'+(
            declaration+'m(name="demo")\n' if order == 'before' else 'm(name="demo")\n'+declaration)
        reject('collision', 'loading')
    else:
        if probe == 'initialization':
            files['defs/check.bzl'] += 'native.package_group(name="forbidden")\n'
            reject('declaration-phase', 'loading')
        else:
            files['defs/context.bzl'] = ('def _callback(ctx):\n    print("CONTEXT_ENTERED")\n'
                '    native.package_group(name="forbidden")\n'
                +('r=repository_rule(implementation=_callback)\n' if probe == 'repository' else
                  'e=module_extension(implementation=_callback)\n'))
            files['MODULE.bazel'] += ('r=use_repo_rule("//defs:context.bzl","r")\nr(name="made")\n' if probe == 'repository' else
                'e=use_extension("//defs:context.bzl","e")\nuse_repo(e,"made")\n')
            value.update(target='@made//:data', markers=['CONTEXT_ENTERED'])
            reject('declaration-phase', 'loading')


DIAGNOSTICS = {
    'constructor': r"(?:Provider[^\n]*not callable|PackageSpecificationInfo[^\n]*(?:not callable|cannot be called)|Operation `?call\(\)`? not supported on type `?Provider`?(?=$|[.\s]))",
    'keyword': r'(?:contains[^\n]*(?:named argument|keyword|positional.only)|(?:unexpected|extra)[^\n]*(?:target|keyword)|^error: Found `target` extra named parameter\(s\) for call to function$)',
    'type': r'(?:contains|target)[^\n]*(?:Label or string|Label.*str|int.*Label)',
    'missing': r'(?:contains[^\n]*(?:missing|required|positional)|(?:missing|required)[^\n]*target|^error: Wrong number of positional arguments, expected 1, got 0$)',
    'extra': r'(?:contains[^\n]*(?:positional|argument)|Wrong number of positional arguments[^\n]*1[^\n]*2)',
    'relative': r'(?:invalid label[^\n]*(?:absolute|begin)|(?:canonical|absolute)[^\n]*label|label[^\n]*(?:absolute|canonical))',
    'immutable': r'(?:cannot set[^\n]*contains|PackageSpecificationInfo[^\n]*(?:immutable|field)|immutable[^\n]*PackageSpecificationInfo|^error: Operation `\.contains=` not supported on type `PackageSpecificationInfo`$)',
    'negate': r'(?:cannot negate[^\n]*(?:public|private)|negative[^\n]*(?:public|private)|(?:public|private)[^\n]*negat)',
    'grammar': r'(?:invalid package (?:name|specification)|package[^\n]*(?:trailing|target suffix|end with)|invalid package path "[^"\n]+": empty, \'\.\' and \'\.\.\' path components are not allowed)',
    'required': r'(?:does not have mandatory providers|required providers[^\n]*(?:missing|not satisfied)|missing[^\n]*(?:required|mandatory)[^\n]*provider)',
    'constraint-shape': r'(?:providers[^\n]*(?:sequence|Provider|constructor)|providers[^\n]*(?:list|tuple|None)|Provider[^\n]*providers)',
    'export': r'(?:Providers should be top.level|provider[^\n]*(?:exported|top.level))',
    'misplaced': r'(?:package.group[^\n]*misplaced|misplaced[^\n]*package.group)',
    'group-kind': r'(?:does not refer to a package group|(?:includes|visibility)[^\n]*(?:not|expected|requires)[^\n]*package.group)',
    'cycle': r'(?:cycle in dependency graph|package.group[^\n]*cycl|cycl[^\n]*package.group)',
    'declaration-phase': r'package_group[^\n]*(?:only[^\n]*BUILD|not allowed[^\n]*(?:initialization|module|repository))|^error: rule declaration outside package evaluation$',
    'literal-boundary': r'(?:package|label)[^\n]*(?:unsupported|not supported|ASCII|invalid character)',
    'depth': r'package.group[^\n]*(?:depth|budget|limit)',
    'return-info': r'PackageSpecificationInfo[^\n]*(?:return|payload|unsupported|not supported)|(?:return|payload)[^\n]*PackageSpecificationInfo',
    **{k: macros.DIAGNOSTICS[k] for k in ('missing-target', 'namespace', 'visibility')},
    'collision': macros.DIAGNOSTICS['collision']+r'|(?:package group|filegroup rule|generated file)[^\n]*conflicts with existing|output //app:demo_group collides with a rule name$',
}


def require_rejection(record, diagnostic):
    code, text = record['returncode'], record['stderr']
    stale = re.search(r'(?:not found|not defined|no (?:field|attribute))[^\n]*(?:PackageSpecificationInfo|package_group)|'
        r'(?:PackageSpecificationInfo|package_group)[^\n]*(?:not found|not defined)', macros.error_lines(text), re.I)
    # No missing-global test belongs in this suite: every negative needs the actual feature.
    if (diagnostic not in DIAGNOSTICS or type(code) is not int or not 0 < code < 128
            or code in (37, 101, 124, 125, 126, 127) or record['timed_out']
            or any(record.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or record['stdout'].strip() or stale or common.UNRELATED.search(text)
            or not re.search(DIAGNOSTICS[diagnostic], macros.error_lines(text), re.I | re.M)):
        raise ValueError('expected controlled package specification rejection: '+diagnostic)


phases_for = common.phases_for
command_argv, action_store, no_actions = macros.command_argv, macros.action_store, macros.no_actions
materialize = macros.materialize


def steps_for(value, engine):
    steps = [('query', target, []) for target, _ in value['queries']]
    if value['diagnostic']:
        return steps+[(value['failure_command'], value['target'], [])]
    for _ in range(2):
        steps.append(('build', value['target'], []))
        if engine == 'reference':
            steps.append(('cquery', value['target'], ['--output=files']))
    return steps


def output_paths(record, value, engine, scratch, workspace, reference):
    if engine == 'native':
        if not value['expected']:
            if success(record) != '':
                raise ValueError('zero-artifact group returned unexpected output')
            return []
        return direct.native_paths(record, value['target'], scratch)
    return macros.reference_output_paths(record['stdout'], workspace, reference.output_base)


def plan_receipt(reference_only=False):
    plan = [dict(family=c['name'], probe=p, engine=e, stage=fixture(c['name'], p)['stage'],
        steps=steps_for(fixture(c['name'], p), e)) for c in CASEPLAN for p, e in phases_for(c, reference_only)]
    return dict(families=len(CASEPLAN), phases=len(plan), commands=2+sum(len(p['steps']) for p in plan),
        sha256=digest(json.dumps(plan, sort_keys=True).encode()))


def require_markers(record, value, command, first, engine, rejection):
    for marker in value['forbidden']:
        if marker in record['stderr']:
            raise ValueError('wrong-kind implementation executed')
    if first or engine == 'native':
        for marker in value['markers']:
            if not macros.marker_count(record, marker):
                raise ValueError('missing loading/phase marker: '+marker)
    entered = macros.marker_count(record, ENTERED)
    if (command == 'query' or (rejection and value['stage'] != 'callback')) and entered:
        raise ValueError('analysis callback executed before prerequisite validation')
    if value['callback'] and command == 'build' and (first or engine == 'native') and not entered:
        raise ValueError('missing actual consumer callback')


def run_phase(args, runner, scratch, workspace, reference, item, probe, engine):
    value = fixture(item['name'], probe)
    paths = materialize(workspace, value)
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    phase = dict(probe=probe, engine=engine, stage=value['stage'], cache=str(cache), runs=[], outputs=[], status='FAIL',
        inputs_before=snapshot(paths), action_store_before=action_store(cache) if engine == 'native' else None)
    try:
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
                require_rejection(r, value['diagnostic'])
            else:
                success(r)
                if command == ('build' if engine == 'native' else 'cquery'):
                    paths_out = output_paths(r, value, engine, scratch, workspace, reference)
                    phase['outputs'].append([evidence(p) for p in paths_out])
            require_markers(r, value, command, i == 0, engine, rejection)
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
            require_rejection(r, value['diagnostic'])
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
    parser.set_defaults(output=ROOT / 'artifacts/package-specification-validation.json')
    parser.add_argument('--reference-source', type=Path, default=Path('/tmp/opencode/bazel-9.3-reference'))
    return parser


# Byte pins from both authoritative contract receipts. Check the checkout, not a
# self-reported source revision or a modified upstream tree.
OFFICIAL_FILES = {
    'analysis/PackageSpecificationProvider.java': '32d7e6ab7d40a73b2b4215f7c0a30080fe53ac79c44fd417c7b4e4ff57b72a2d',
    'packages/PackageSpecification.java': '68034c13c31d5a360084f0c265eefa46fd2e0b80e16a72785c776dba629ec93c',
    'analysis/CommonPrerequisiteValidator.java': '473f263c43f0122a7da1f56bdb95b00f35148d09d16722c27d2e34cc32119ccb',
    'analysis/RuleContext.java': '0f6dcffac7286a9056d050624bd29e73cefc4138dd9dc24708dec63e147b41e2',
    'analysis/starlark/StarlarkAttrModule.java': '388421c44c623c1c6625fd9f2b059d2a7d1e13b8d45e7c96173f24866a917967',
}


def reference_source_evidence(path):
    result = subprocess.run(['git', '-C', str(path), 'rev-parse', 'HEAD'], capture_output=True, timeout=10, check=True)
    if result.stdout.decode().strip() != SOURCE_PIN:
        raise ValueError('wrong pinned official source revision')
    files = {name: digest((path / 'src/main/java/com/google/devtools/build/lib' / name).read_bytes()) for name in OFFICIAL_FILES}
    if files != OFFICIAL_FILES:
        raise ValueError('modified pinned official source')
    return dict(path=str(path), commit=SOURCE_PIN, files=files)


class RecordingRunner(macros.RecordingRunner):
    """Bind both ELF candidates and pinned source before every real invocation."""

    def __init__(self, report, args):
        super().__init__(report, args.timeout)
        self.args = args

    def run(self, name, argv, cwd, timeout=None):
        binaries = common.executable_evidence(self.args)
        source = reference_source_evidence(self.args.reference_source)
        if binaries != self.report['executables'] or source != self.report['official_source_before']:
            raise ValueError('pre-command executable/source binding changed')
        r = super().run(name, argv, cwd, timeout)
        r['preflight_binding_sha256'] = digest(json.dumps(dict(executables=binaries, source=source), sort_keys=True).encode())
        return r


def process_state(reference):
    path = reference.output_base / 'server/server.pid.txt'
    pid = int(path.read_text().strip()) if path.exists() else None
    if pid is not None and pid <= 1:
        raise ValueError('invalid private reference PID')
    return dict(pidfile=str(path), pid=pid)


def shutdown(reference, report):
    # Do not use the shared fallback that inspects process argv. A failed graceful
    # shutdown is a failed receipt and retains scratch for safe owner recovery.
    before = process_state(reference)
    r = reference.runner.run('package-specification.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS),
        reference.workspace, timeout=min(reference.runner.timeout, 15))
    success(r)
    remaining = sorted(str(p) for p in reference.output_base.rglob('server.pid.txt'))
    alive = before['pid'] is not None and Path('/proc', str(before['pid'])).exists()
    report['server_receipt'] = dict(before, proc_exists_after=alive, remaining_pidfiles=remaining)
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
        if (report['schema_version'] != 1 or report['status'] not in ('FAIL', expected_status) or report['planned_checks'] != CASEPLAN
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
        if not args.reference_only and (not re.fullmatch('[0-9a-f]{64}', args.native_sha256 or '')
                or report['executables']['native']['sha256'] != args.native_sha256):
            return False
        if (report['completed_phases'] != report['plan']['phases']
                or report['completed_checks'] != sum(bool(phases_for(c, args.reference_only)) for c in CASEPLAN)):
            return False
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        reference = DirectReference(args.reference, workspace, scratch, None, 'package-specification')
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
                    or r['preflight_binding_sha256'] != binding or 'environment' in r or 'environment_keys' in r):
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(r[field+'_base64'], validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != r[field]):
                    return False
        for r, name, argv, timeout in (
                (runs[0], 'reference.version', reference.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], 'package-specification.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS), min(args.timeout, 15))):
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
    if args.reference_only and args.output == ROOT / 'artifacts/package-specification-validation.json':
        args.output = ROOT / 'artifacts/package-specification-reference.json'
    for field in ('native', 'reference', 'output', 'scratch_parent', 'reference_source'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if (args.output in [args.native, args.reference, *SOURCES] or args.output.is_relative_to(args.reference_source)
            or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests', 'src'))):
        raise ValueError('report cannot overwrite source or executable')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
        planned_checks=CASEPLAN, plan=plan_receipt(args.reference_only), limitations=LIMITATIONS,
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        scratch_cleaned=False, reference_shutdown_succeeded=False,
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
    runner = RecordingRunner(report, args)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = common.executable_evidence(args)
        report['official_source_before'] = reference_source_evidence(args.reference_source)
        scratch = Path(tempfile.mkdtemp(prefix='package-specification-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(MODULE)
        shutil.copytree(REGISTRY, scratch / 'registry')
        report['registry_before'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        reference = DirectReference(args.reference, workspace, scratch, runner, 'package-specification')
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
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        macros.write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return 130 if interrupted else 0 if report['status'] in ('PASS', 'REFERENCE_ONLY_PASS') else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
