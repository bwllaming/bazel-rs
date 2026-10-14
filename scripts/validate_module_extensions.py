#!/usr/bin/env python3
"""Bounded ordinary module-extension checks; NOT full Bazel/application parity.

Explicit execution uses a native ELF and genuine pinned Bazel 9.3.0. Only
non-isolated None-returning extension callbacks are positive parity claims.
Unit tests exercise this harness, never either CLI or a fake Starlark runtime.
The native os-environ boundary requires the supported host value to be reached;
it replaces the pre-host-metadata missing-os expectation, not its saved results.
"""

import base64
from concurrent.futures import ThreadPoolExecutor
import datetime
import json
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile

from validate import ROOT, COMMON_FLAGS, BUILD_FLAGS, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256, validate_binaries
import validate_repository_rules as direct


SOURCES = list(dict.fromkeys([ROOT / 'scripts/validate_module_extensions.py',
                             ROOT / 'tests/test_validate_module_extensions.py', *direct.SOURCES]))
quote = direct.quote
digest = direct.digest
evidence = direct.evidence
snapshot = direct.snapshot
valid_evidence = direct.valid_evidence
ROOT_MODULE = 'module(name="main", version="1.0")\n'
USAGE = 'e=use_extension("//:ext.bzl", "files")\nuse_repo(e, chosen="made")\n'
FAIL_MARKER = 'EXTENSION_AFTER_RECORD_SENTINEL'
LAZY_MARKER = 'UNUSED_REPOSITORY_CALLBACK_SENTINEL'
LOAD_MARKER = 'UNUSED_EXTENSION_DEFINITION_SENTINEL'
BINARY = b'A B\r\nraw:\xff\x00\r\n'
CANONICAL = '+files+made'


class RecordingRunner(direct.RecordingRunner):
    def run(self, name, argv, cwd, timeout=None):
        start = len(self.report['runs'])
        try:
            return super().run(name, argv, cwd, timeout)
        finally:
            for record in self.report['runs'][start:]:
                binaries = self.report.get('executables', {}).values()
                record['executable_sha256'] = next((b['sha256'] for b in binaries
                                                    if b['path'] == record['argv'][0]), None)


def extension(body, classes='{}', prefix='load(":rules.bzl", "maker")\n', symbol='files'):
    return (prefix + 'def _impl(ctx):\n' + ''.join('    ' + line + '\n' for line in body.splitlines())
            + symbol + '=module_extension(_impl, tag_classes=' + classes
            + ', doc=None, environ=[], os_dependent=False, arch_dependent=False, facts_version=0)\n')


RULE = direct.rule(direct.build_file(['data']) + '\nctx.file("data", ctx.attr.text, executable=False)',
                   '{"text":attr.string(default="hello Ada\\n")}')
EXT = extension('maker(name="ma"+"de")\nreturn None')


def case(name, kind='differential', probes=('result',)):
    return dict(name=name, kind=kind, probes=list(probes))


CASES = [
    case('computed.transitive-private-rule'),
    case('tags.typed-none-and-call-snapshot', probes=('valid', 'mandatory-none')),
    case('callback.whole-group-rollback'),
    case('group.validate-all-fetch-demanded', probes=('unused-valid', 'missing', 'unknown')),
    case('imports.unused-missing'),
    case('participants.bfs-alias-dev-version', probes=('valid', 'invalid-canonical')),
    case('labels.owners-and-forward-sibling'),
    case('identity.private-and-public-alias', probes=('private', 'public')),
    case('native.preparation-cycles', 'native_boundary', ('ordinary', 'direct')),
    case('native.context-and-return', 'native_boundary', ('metadata', 'facts', 'os-environ', 'read', 'integer', 'dict')),
    case('native.isolated-on-demand', 'native_boundary', ('demanded', 'unused')),
    case('native.immutable-projections', 'native_boundary', ('modules', 'tags', 'words')),
    case('native.refresh-and-repair', 'native_boundary', ('cold', 'tag-change', 'callback-change', 'input-change', 'repair')),
    case('native.parallel-publication', 'native_boundary', ('parallel-1', 'parallel-2')),
    case('native.names-and-laziness', 'native_boundary', ('invalid-name', 'unknown-canonical', 'main-only')),
    case('native.tick-budgets', 'native_boundary', ('initialization', 'callback')),
]
PLAN = [(c['name'], c['kind']) for c in CASES]
DIAGNOSTICS = {
    'missing': direct.DIAGNOSTICS['missing'],
    'unknown': direct.DIAGNOSTICS['unknown'],
    'rollback': r'(?:Error in fail:|\bfail(?:ed)?[: (]|error:)[^\n]*' + FAIL_MARKER,
    'import': r'''(?:does not generate|not generated|not provided|missing|not found)[^\n]*\bmissing_repo\b|\bmissing_repo\b[^\n]*(?:not generated|not provided|does not exist|missing|not found)|did not declare imported repository ["']missing_repo["']''',
    'extension-label': r'''no repo visible as ['"]@shared\+['"] here|unknown apparent repository @shared\+ from canonical repository''',
    'cycle': r'(?:extension|repository|prepar)[^\n]*(?:cycle|cyclic)|(?:cycle|cyclic)[^\n]*(?:extension|repository|a_repo|b_repo)',
    'return': r'(?:return|returned)[^\n]*(?:None|unsupported|not supported)|(?:only|must)[^\n]*None',
    'isolated': r'isolat[^\n]*(?:unsupported|not supported)|(?:unsupported|not supported)[^\n]*isolat',
    'immutable': r'\bImmutable\b|(?:frozen|immutable)[^\n]*(?:list|value|mutat)|(?:mutat|append)[^\n]*(?:frozen|immutable)',
    'name': r'invalid repository name[^\n]*a\+b|a\+b[^\n]*(?:invalid|valid repository name)',
    'canonical': r'(?:unknown|not found|not visible|not defined|unavailable|unrecognized)[^\n]*\+unknown\+made|\+unknown\+made[^\n]*(?:unknown|not found|not visible|not defined|unavailable|unrecognized)',
    'ticks': direct.DIAGNOSTICS['ticks'],
    'os-environ': r'(?m)^error: repository_os\.environ is unsupported in the native host metadata profile$',
}
for _member in ('extension_metadata', 'facts', 'read'):
    DIAGNOSTICS[_member] = (r'(?:no (?:attribute|field|method)|has no|not supported|unsupported)[^\n]*\b'
                            + _member + r'\b|\b' + _member
                            + r'\b[^\n]*(?:not supported|unsupported|not available|not found)')
UNRELATED = re.compile(direct.UNRELATED.pattern + r'|permission denied|access denied|syntax error|connection reset|'
                       r'failed to (?:download|fetch)|error downloading', re.I)


def success(record):
    text = direct.success(record)
    if type(record['returncode']) is not int or UNRELATED.search(record['stderr']):
        raise ValueError('unrelated successful invocation diagnostics')
    return text


def require_rejection(record, diagnostic):
    code, text = record['returncode'], record['stderr']
    if (type(code) is not int or not 0 < code < 128 or code in (101, 124, 125, 126, 127)
            or record['timed_out'] or record.get('spawn_error') or record.get('interrupted')
            or record.get('output_truncated') or record['stdout'].strip() or UNRELATED.search(text)
            or LAZY_MARKER in text or LOAD_MARKER in text
            or not re.search(DIAGNOSTICS[diagnostic], text, re.I)):
        raise ValueError('expected controlled module-extension rejection: ' + diagnostic)
    if diagnostic == 'os-environ':
        # The host object exists. Only its deliberately unsupported environ
        # property may fail, after RepoSpec recording and before repo execution.
        # Missing ctx.os/name, a fake empty environ, or a different callback are
        # not substitutes for reaching this exact property getter.
        planned = fixture('native.context-and-return', 'os-environ')['files']['ext.bzl'].decode().splitlines()
        statement = '    value=ctx.os.environ'
        if planned.count(statement) != 1:
            raise ValueError('ambiguous planned os-environ getter')
        line = planned.index(statement)+1
        error = 'error: repository_os.environ is unsupported in the native host metadata profile'
        if (planned[line-2] != '    maker(name="made")'
                or re.findall(r'(?m)^error: [^\n]*$', text) != [error]
                or not re.search(r'(?m)^bazel-rs: module extension //:ext\.bzl: Traceback \(most recent call last\):$', text)
                or not re.search(r'(?m)^ --> //:ext\.bzl:'+str(line)+r':11$', text)
                or not re.search(r'(?m)^'+str(line)+r' \| '+re.escape(statement)+r'$', text)
                or FAIL_MARKER in text or record['stdout']
                or re.search(r'\d+ (?:actions executed|cache hits)|(?m:^HOST_[A-Z_]+\s*$)', text)):
            raise ValueError('expected exact native os-environ property failure after declaration')


def participants(files):
    files['MODULE.bazel'] = ('module(name="main",version="1.0+stamp")\n'
        'bazel_dep(name="z",version="2.0")\nbazel_dep(name="a",version="5")\n'
        'bazel_dep(name="shared",version="1")\n'
        + ''.join('local_path_override(module_name=' + quote(n) + ',path=' + quote(n) + ')\n'
                  for n in ('z', 'a', 'shared', 'deep'))
        + 'e=use_extension("@shared//:ext.bzl","files")\n'
        'other=use_extension("@@shared//:ext.bzl","files")\n'
        'e.note(value="first")\nother.note(value="second")\ne.note(value="third")\n'
        'dev=use_extension("@shared//:ext.bzl","files",dev_dependency=True)\n'
        'dev.note(value="dev")\nuse_repo(e,chosen="made")\n')
    usage = 'e=use_extension("@shared//:ext.bzl","files")\n'
    files['z/MODULE.bazel'] = ('module(name="z",version="2.0+stamp")\n'
        'bazel_dep(name="shared",version="1")\nbazel_dep(name="deep",version="3.0")\n'
        + usage + 'e.note(value="z")\ndev=use_extension("@shared//:ext.bzl","files",dev_dependency=True)\n'
        'dev.note(value="ignored")\n')
    files['a/MODULE.bazel'] = 'module(name="a",version="5")\nbazel_dep(name="shared",version="1")\n' + usage
    files['deep/MODULE.bazel'] = ('module(name="deep",version="3.0")\nbazel_dep(name="shared",version="1")\n'
                                   + usage + 'e.note(value="deep")\n')
    files['shared/MODULE.bazel'] = 'module(name="shared",version="1")\n'
    files['shared/BUILD.bazel'] = ''
    files['shared/rules.bzl'] = RULE
    files['shared/ext.bzl'] = extension(
        'parts=[]\nfor m in ctx.modules:\n'
        '    tags=[t.value+":"+str(ctx.is_dev_dependency(t)) for t in m.tags.note]\n'
        '    parts.append(m.name+":"+m.version+":"+str(m.is_root)+":"+",".join(tags))\n'
        'maker(name="made",text="|".join(parts)+";root="+str(ctx.root_module_has_non_dev_dependency))',
        '{"note":tag_class({"value":attr.string()})}')
    return b'main:1.0:True:first:False,second:False,third:False,dev:True|z:2.0:False:z:False|a:5:False:|deep:3.0:False:deep:False;root=True'


def label_owners(files):
    files['MODULE.bazel'] = ('module(name="main")\nbazel_dep(name="host",version="1",repo_name="sibling")\n'
        'local_path_override(module_name="host",path="host")\n'
        'e=use_extension("//pkg:ext.bzl","files")\ne.input(supplied="//:input")\nuse_repo(e,chosen="made")\n')
    files.update({'host/MODULE.bazel': 'module(name="host",version="1")\n',
                  'host/BUILD.bazel': 'exports_files(["data"])\n', 'host/data': 'HOST\n',
                  'input': 'ROOT\n', 'helper/BUILD.bazel': '', 'helper/input': 'HELPER\n',
                  'pkg/BUILD.bazel': '', 'pkg/local': 'PUBLIC\n', 'defs/BUILD.bazel': ''})
    files['helper/tags.bzl'] = 'def inputs():\n    return tag_class({"supplied":attr.label(),"fallback":attr.label(default=":input")})\n'
    files['defs/rules.bzl'] = direct.rule(
        'if ctx.original_name == "sibling":\n    ' + direct.build_file(['data'])
        + '\n    ctx.file("data","GENERATED\\n",executable=False)\nelse:\n    '
        + direct.build_file(['data', 'explicit', 'supplied', 'fallback', 'relative']) + '\n'
        + '\n'.join('    ctx.template(' + quote(out) + ',ctx.attr.' + attr + ',executable=False)'
                    for out, attr in (('data', 'raw'), ('explicit', 'explicit'), ('supplied', 'supplied'),
                                      ('fallback', 'fallback'), ('relative', 'relative'))),
        '{' + ','.join(quote(a) + ':attr.label()' for a in ('raw', 'explicit', 'supplied', 'fallback', 'relative')) + '}')
    files['pkg/ext.bzl'] = extension(
        'tag=ctx.modules[0].tags.input[0]\n'
        'maker(name="made",raw="@sibling//:data",explicit=Label("@sibling//:data"),'
        'supplied=tag.supplied,fallback=tag.fallback,relative=":local")\nmaker(name="sibling")',
        '{"input":inputs()}', 'load("//defs:rules.bzl","maker")\nload("//helper:tags.bzl","inputs")\n')
    return {key: (raw, False) for key, raw in (('data', b'GENERATED\n'), ('explicit', b'HOST\n'),
            ('supplied', b'ROOT\n'), ('fallback', b'HELPER\n'), ('relative', b'PUBLIC\n'))}


def fixture(name, probe='result'):
    """Pure source/expectation plan. Binary input is a file, not a Starlark escape."""
    item = next((c for c in CASES if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    files = {'MODULE.bazel': ROOT_MODULE + USAGE, 'BUILD.bazel': '', 'rules.bzl': RULE, 'ext.bzl': EXT}
    expected = {'data': (b'hello Ada\n', False)}
    diagnostic, target, canonical, flags, main_only = None, '@chosen//:data', CANONICAL, [], False
    if name == 'computed.transitive-private-rule':
        files['leaf.bzl'] = RULE.replace('maker = repository_rule', '_private = repository_rule') + 'maker=_private\n'
        files['helper.bzl'] = 'load(":leaf.bzl",_maker="maker")\ndef declare(name):\n    return _maker(name=name)\n'
        files['ext.bzl'] = extension('result=declare("ma"+"de")\nif result != None:\n    fail("CALL_MUST_RETURN_NONE")\nreturn None',
                                    prefix='load(":helper.bzl","declare")\n')
    elif name == 'tags.typed-none-and-call-snapshot':
        classes = ('{"make":tag_class({"name":attr.string(mandatory=True),"required":attr.string(mandatory=True),'
                   '"_private":attr.string(mandatory=True),"text":attr.string(default="DEFAULT"),'
                   '"count":attr.int(default=7),"enabled":attr.bool(default=True),'
                   '"optional":attr.label(),"words":attr.string_list(default=["before"])}),"empty":tag_class()}')
        files['MODULE.bazel'] = ROOT_MODULE + USAGE + ('e.make(name="made",required=' + ('None' if probe == 'mandatory-none' else '"yes"')
            + ',_private="secret",text=None,unknown=None)\n')
        files['rules.bzl'] = direct.rule(direct.build_file(['data'])
            + '\nctx.file("data",",".join(ctx.attr.words)+"|"+ctx.attr.text,executable=False)',
            '{"words":attr.string_list(),"text":attr.string()}')
        files['ext.bzl'] = extension(
            't=ctx.modules[0].tags.make[0]\n'
            'if t.name != "made" or t.required != "yes" or t._private != "secret" or t.text != "DEFAULT":\n'
            '    fail("TAG_VALUES_WRONG")\n'
            'if t.count != 7 or not t.enabled or t.optional != None or ctx.modules[0].tags.empty != []:\n'
            '    fail("TAG_DEFAULTS_WRONG")\n'
            'if ctx.is_dev_dependency(t):\n    fail("TAG_DEV_WRONG")\n'
            'words=list(t.words)\nargs={"name":t.name,"words":words,"text":t.text}\n'
            'maker(**args)\nwords.append("after")\nargs["text"]="after"\nreturn None', classes)
        expected = {'data': (b'before|DEFAULT', False)}
        diagnostic = 'missing' if probe == 'mandatory-none' else None
    elif name == 'callback.whole-group-rollback':
        files['rules.bzl'] = direct.rule('fail(' + quote(LAZY_MARKER) + ')')
        files['ext.bzl'] = extension('maker(name="made")\nmaker(name="unused")\nfail(' + quote(FAIL_MARKER) + ')')
        diagnostic = 'rollback'
    elif name == 'group.validate-all-fetch-demanded':
        files['rules.bzl'] = direct.rule('if ctx.original_name == "unused":\n    fail(' + quote(LAZY_MARKER) + ')\n'
            + direct.build_file(['data']) + '\nctx.file("data",ctx.attr.required,executable=False)',
            '{"required":attr.string(mandatory=True)}')
        unused = {'unused-valid': 'required="ignored"', 'missing': '',
                  'unknown': 'required="ignored",unknown_attr="bad"'}[probe]
        files['ext.bzl'] = extension('maker(name="made",required="actual")\nmaker(name="unused",' + unused + ')')
        expected = {'data': (b'actual', False)}
        diagnostic = None if probe == 'unused-valid' else probe
    elif name == 'imports.unused-missing':
        files['MODULE.bazel'] += 'use_repo(e,unused="missing_repo")\n'
        files['rules.bzl'] = direct.rule('fail(' + quote(LAZY_MARKER) + ')')
        diagnostic = 'import'
    elif name == 'participants.bfs-alias-dev-version':
        expected = {'data': (participants(files), False)}
        canonical = 'shared++files+made'
        # use_extension normalizes one leading @ before apparent-name lookup;
        # this is NOT the canonical-label syntax accepted by query/Label().
        # Different raw spellings above must merge; the canonical-looking
        # spelling below must reject rather than bypass the MODULE repo map.
        if probe == 'invalid-canonical':
            files['MODULE.bazel'] = files['MODULE.bazel'].replace('@@shared//:ext.bzl', '@@shared+//:ext.bzl')
            diagnostic = 'extension-label'
    elif name == 'labels.owners-and-forward-sibling':
        expected = label_owners(files)
    elif name == 'identity.private-and-public-alias':
        # Both requested identities coexist and import the SAME generated name.
        symbol = '_private' if probe == 'private' else 'files'
        files['MODULE.bazel'] = ROOT_MODULE + ('p=use_extension("//:ext.bzl","_private")\n'
            'q=use_extension("//:ext.bzl","files")\nuse_repo(p,private_repo="made")\nuse_repo(q,public_repo="made")\n')
        alias = 'private_repo' if probe == 'private' else 'public_repo'
        target, canonical = '@' + alias + '//:data', '+' + symbol + '+made'
        files['ext.bzl'] = extension('maker(name="made")', symbol='_private') + 'files=_private\n'
    elif name == 'native.preparation-cycles':
        files['MODULE.bazel'] = ROOT_MODULE + 'a=use_extension("//:a.bzl","a")\nuse_repo(a,"a_repo")\n'
        files['a.bzl'] = extension('pass', prefix='load("@b_repo//:helper.bzl","value")\n', symbol='a')
        if probe == 'ordinary':
            files['MODULE.bazel'] += 'b=use_extension("//:b.bzl","b")\nuse_repo(b,"b_repo")\n'
            files['b.bzl'] = extension('pass', prefix='load("@a_repo//:helper.bzl","value")\n', symbol='b')
        else:
            files['MODULE.bazel'] += 'b=use_repo_rule("//:b.bzl","maker")\nb(name="b_repo")\n'
            files['b.bzl'] = direct.rule('pass', prefix='load("@a_repo//:helper.bzl","value")\n')
        target, diagnostic = '@a_repo//:data', 'cycle'
    elif name == 'native.context-and-return':
        operation = {'metadata': 'ctx.extension_metadata()', 'facts': 'value=ctx.facts',
                     'os-environ': 'value=ctx.os.environ', 'read': 'ctx.read("x")',
                     'integer': 'return 42', 'dict': 'return {}'}[probe]
        files['ext.bzl'] = extension('maker(name="made")\n' + operation)
        if probe == 'os-environ':
            files['ext.bzl'] = extension('maker(name="made")\n'+operation+'\nfail('+quote(FAIL_MARKER)+')')
            files['rules.bzl'] = direct.rule('fail('+quote(LAZY_MARKER)+')')
        diagnostic = 'return' if probe in ('integer', 'dict') else 'extension_metadata' if probe == 'metadata' else probe
    elif name == 'native.isolated-on-demand':
        files['MODULE.bazel'] = ROOT_MODULE + USAGE.replace('"files")', '"files",isolate=True)')
        # Native MODULE metadata accepts the option; demand must reject, not invent ctx values.
        diagnostic = 'isolated' if probe == 'demanded' else None
        main_only = probe == 'unused'
    elif name == 'native.immutable-projections':
        files['MODULE.bazel'] += 'e.make(words=["before"])\n'
        expression = {'modules': 'ctx.modules', 'tags': 'ctx.modules[0].tags.make',
                      'words': 'ctx.modules[0].tags.make[0].words'}[probe]
        files['ext.bzl'] = extension('maker(name="made")\n' + expression + '.append(None)',
                                    '{"make":tag_class({"words":attr.string_list()})}')
        diagnostic = 'immutable'
    elif name == 'native.refresh-and-repair':
        text = 'A' if probe == 'cold' else 'Z'
        files['MODULE.bazel'] += 'e.make(text=' + quote(text) + ')\n'
        files['input'] = BINARY if probe in ('cold', 'tag-change', 'callback-change') else BINARY + b'new\xfe\n'
        files['rules.bzl'] = direct.rule(direct.build_file(['data'])
            + '\nctx.template("data",ctx.attr.source,substitutions={"A":ctx.attr.text},executable=False)',
            '{"source":attr.label(),"text":attr.string()}')
        suffix = '' if probe in ('cold', 'tag-change') else '!'
        files['ext.bzl'] = extension('maker(name="made",source=Label("//:input"),text=ctx.modules[0].tags.make[0].text+' + quote(suffix) + ')',
                                    '{"make":tag_class({"text":attr.string()})}')
        expected = {'data': (files['input'].replace(b'A', (text + suffix).encode()), False)}
    elif name == 'native.parallel-publication':
        pass
    elif name == 'native.names-and-laziness':
        if probe == 'invalid-name':
            files['ext.bzl'] = extension('maker(name="a+b")\nfail("INVALID_NAME_WAS_ACCEPTED")')
            diagnostic = 'name'
        else:
            files['ext.bzl'] = 'fail(' + quote(LOAD_MARKER) + ')\n'
            if probe == 'unknown-canonical':
                target, diagnostic = '@@+unknown+made//:data', 'canonical'
            else:
                main_only = True
    elif name == 'native.tick-budgets':
        diagnostic = 'ticks'
        files['ext.bzl'] = (extension('maker(name="made")', prefix='load(":rules.bzl","maker")\nINITIALIZATION=[i+1 for i in range(1000000)]\n') if probe == 'initialization'
                            else extension('maker(name="made")\nvalues=[i+1 for i in range(1000000)]'))
    if main_only:
        target = '//:data'
        files['BUILD.bazel'], files['data'] = 'exports_files(["data"])\n', b'main only\n'
    elif not diagnostic:
        repo = target.split('//')[0]
        labels = [repo + '//:' + key for key in expected]
        files['BUILD.bazel'] = ('filegroup(name="data",srcs=' + repr(labels) + ')\n'
            'genrule(name="copy",srcs=[' + quote(target) + '],outs=["copy.txt"],cmd="cat $(SRCS) > $@")\n')
    return dict(files={p: v.encode() if isinstance(v, str) else v for p, v in files.items()},
                expected=expected, diagnostic=diagnostic, target=target, canonical=canonical,
                main_only=main_only, flags=flags)


def materialize(root, workspace, name, probe, *, update=False):
    if not update:
        if workspace.exists():
            shutil.rmtree(workspace)
        workspace.mkdir(parents=True)
    value = fixture(name, probe)
    registry = root / 'registry'
    owned = {workspace / path: raw for path, raw in value['files'].items()}
    owned[registry / 'bazel_registry.json'] = b'{}\n'
    for path, raw in owned.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
    value.update(paths=list(owned), registry=registry)
    return value


def phases_for(item):
    return [(p, e) for p in item['probes'] for e in
            (('reference', 'native') if item['kind'] == 'differential' else ('native',))]


def steps_for(item, probe, engine):
    value = fixture(item['name'], probe)
    if value['diagnostic'] or value['main_only']:
        return [('query', value['target'], [])]
    steps = [('build', '//:data', [])]
    if engine == 'reference':
        steps.append(('cquery', '//:data', ['--output=files']))
    steps.append(('cquery', value['target'], ['--output=starlark', '--starlark:expr=str(target.label)'])
                 if engine == 'reference' else ('query', value['target'], []))
    steps.append(('build', '//:copy', []))
    if engine == 'reference':
        steps.append(('cquery', '//:copy', ['--output=files']))
    return steps


def command_argv(args, reference, engine, cache, registry, command, target, extra, flags=()):
    if engine == 'native':
        return direct.native_command(args.native, cache, registry, command, target)[:-1] + list(flags) + list(extra) + [target]
    return (reference.startup + [command] + list(COMMON_FLAGS)
            + (list(BUILD_FLAGS) if command in ('build', 'cquery') else [])
            + ['--repository_cache=' + str(reference.output_base.parent / 'repository-cache'),
               '--repo_contents_cache=' + str(reference.output_base.parent / 'repository-contents'),
               '--registry=' + registry.as_uri(), '--registry=https://bcr.bazel.build']
            + list(flags) + list(extra) + [target])


def canonical_result(record, canonical):
    if success(record).splitlines() != ['@@' + canonical + '//:data']:
        raise ValueError('canonical query did not preserve requested extension identity')


def run_phase(args, runner, scratch, workspace, reference, item, phase, value, initial=None):
    engine, probe, cache = phase['engine'], phase['probe'], Path(phase['cache'])

    def invoke(command, target, extra=()):
        name = item['name'] + '.' + probe + '.' + engine + '.' + str(len(phase['runs']))
        if initial is not None and not phase['runs']:
            record = initial
        elif engine == 'reference':
            reference.registry = value['registry']
            record = reference.run(name, command, *value['flags'], *extra, target)
        else:
            argv = command_argv(args, reference, engine, cache, value['registry'], command, target, extra, value['flags'])
            record = runner.run(name, argv, workspace)
        phase['runs'].append(dict(id=record['id'], command=command, target=target, extra=list(extra)))
        return record

    def outputs(build_record, target):
        if engine == 'native':
            success(build_record)
            return direct.native_paths(build_record, target, scratch)
        success(build_record)
        return reference_output_paths(success(invoke('cquery', target, ['--output=files'])), workspace, reference.output_base)

    try:
        if value['diagnostic']:
            require_rejection(invoke('query', value['target']), value['diagnostic'])
            phase['diagnostic'] = value['diagnostic']
            if engine == 'native':
                phase['publication_entries'] = direct.no_publication(cache)
        elif value['main_only']:
            if success(invoke('query', value['target'])).splitlines() != ['//:data']:
                raise ValueError('main-only query did not return exact main identity')
            phase['publication_entries'] = direct.no_publication(cache)
        else:
            paths = outputs(invoke('build', '//:data'), '//:data')
            phase['artifacts'] = direct.source_artifacts(paths, value['expected'],
                cache=cache if engine == 'native' else None, reference=reference)
            canonical = invoke('cquery', value['target'], ['--output=starlark', '--starlark:expr=str(target.label)']) if engine == 'reference' else invoke('query', value['target'])
            canonical_result(canonical, value['canonical'])
            phase['canonical'] = '@@' + value['canonical'] + '//:data'
            if engine == 'reference':
                for key, entry in phase['artifacts']['files'].items():
                    logical = Path(entry['logical_path'])
                    if logical.parts[-2:] != (value['canonical'], key):
                        raise ValueError('reference artifact path disagrees with canonical query')
            copies = outputs(invoke('build', '//:copy'), '//:copy')
            if len(copies) != 1 or copies[0].name != 'copy.txt':
                raise ValueError('copy action must return exactly copy.txt')
            phase['copy'] = evidence(copies[0])
            if not valid_evidence(phase['copy'], value['expected']['data'][0]) or phase['copy']['kind'] != 'file':
                raise ValueError('copy action did not consume generated bytes')
            if engine == 'native':
                # In the forward-sibling case two repositories are demanded;
                # in all other successful cases unused RepoSpecs stay unfetched.
                count = 2 if item['name'] == 'labels.owners-and-forward-sibling' else 1
                if item['name'] != 'native.refresh-and-repair':
                    entries = sorted(p.name for p in direct.trees_directory(cache).glob('*'))
                    if len(entries) != count or any(not re.fullmatch('[0-9a-f]{64}', p) for p in entries):
                        raise ValueError('unexpected unused repository fetch or staging publication')
                    phase['publication_entries'] = entries
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['status'] = 'FAIL'
        phase['error'] = type(exc).__name__ + ': ' + str(exc)
    finally:
        try:
            phase['inputs_after'] = snapshot(value['paths'])
            if phase['inputs_before'] != phase['inputs_after']:
                raise ValueError('CLI changed frozen fixture bytes or modes')
        except Exception as exc:
            phase.update(status='FAIL', error='fixture evidence: ' + str(exc))


def run_case(args, runner, scratch, workspace, reference, item):
    check = dict(name=item['name'], kind=item['kind'], input=item, status='FAIL', parity_claim=False, phases=[])
    runner.report['checks'].append(check)
    case_root = scratch / 'cases' / item['name']
    refresh = item['name'] == 'native.refresh-and-repair'
    parallel = item['name'] == 'native.parallel-publication'
    previous = None
    for probe in item['probes']:
        if parallel and probe == 'parallel-2':
            continue
        root = case_root if refresh or parallel else case_root / probe
        cache = root / 'native-cache'
        try:
            value = materialize(root, workspace, item['name'], probe, update=refresh and probe != 'cold')
            mutation = None
            if refresh and previous is not None:
                mutation = dict(kind=probe, before=previous['inputs_after'], after=snapshot(value['paths']))
                if probe == 'repair':
                    damaged = Path(previous['artifacts']['files']['data']['path'])
                    before = evidence(damaged)
                    damaged.chmod(0o644)
                    damaged.write_bytes(b'CORRUPT PUBLISHED TREE\n')
                    mutation['published'] = dict(before=before, after=evidence(damaged))
            engines = ('reference', 'native') if item['kind'] == 'differential' else ('native',)
            prepared = []
            for subprobe, engine in ([(p, 'native') for p in item['probes']] if parallel else [(probe, e) for e in engines]):
                phase = dict(probe=subprobe, engine=engine, status='FAIL', runs=[], cache=str(cache),
                             registry=str(value['registry']), inputs_before=snapshot(value['paths']))
                if mutation is not None:
                    phase['intentional_mutation'] = mutation
                check['phases'].append(phase)
                prepared.append(phase)
            records = [None] * len(prepared)
            if parallel:
                locals_ = [RecordingRunner({'runs': [], 'executables': runner.report.get('executables', {})}, args.timeout)
                           for _ in prepared]
                for local in locals_:
                    local.env = dict(runner.env)
                try:
                    with ThreadPoolExecutor(max_workers=2) as pool:
                        futures = [pool.submit(local.run, item['name'] + '.' + phase['probe'] + '.native.0',
                            command_argv(args, reference, 'native', cache, value['registry'], 'build', '//:data', []), workspace)
                            for local, phase in zip(locals_, prepared)]
                        records = [future.result() for future in futures]
                finally:
                    # Preserve even failed/interrupted subprocess evidence.
                    for local in locals_:
                        for record in local.report['runs']:
                            record['id'] = len(runner.report['runs'])
                            runner.report['runs'].append(record)
            for phase, initial in zip(prepared, records):
                run_phase(args, runner, scratch, workspace, reference, item, phase, value, initial)
                previous = phase
        except Exception as exc:
            check.setdefault('errors', []).append(probe + ': ' + type(exc).__name__ + ': ' + str(exc))
    try:
        validate_check(check, runner.report, args, workspace, reference)
        check.update(status='PASS', parity_claim=item['kind'] == 'differential')
    except Exception as exc:
        check['error'] = str(exc)
        if any(p['engine'] == 'reference' and p['status'] != 'PASS' for p in check['phases']):
            check['status'] = 'REFERENCE_FAILURE'
    return check


def run_all(args, runner, scratch, workspace, reference):
    """One fixture/programming failure never hides the remaining planned cases."""
    for item in CASES:
        before = len(runner.report['checks'])
        try:
            run_case(args, runner, scratch, workspace, reference, item)
        except Exception as exc:
            if len(runner.report['checks']) == before:
                runner.report['checks'].append(dict(name=item['name'], kind=item['kind'], input=item,
                                                    phases=[], parity_claim=False))
            runner.report['checks'][-1].update(status='FAIL', error=type(exc).__name__ + ': ' + str(exc))


def validate_check(check, report, args, workspace, reference):
    """Validate retained bytes and argv after cleanup; counts/labels cannot mint PASS."""
    item = next(c for c in CASES if c['name'] == check['name'])
    if (check['input'] != item or check['kind'] != item['kind'] or check.get('errors')
            or [(p['probe'], p['engine']) for p in check['phases']] != phases_for(item)):
        raise ValueError('missing, duplicate, reordered or unexpected case phases')
    for phase in check['phases']:
        value = fixture(item['name'], phase['probe'])
        if (phase['status'] != 'PASS' or phase.get('error') or not phase['inputs_before']
                or phase['inputs_before'] != phase['inputs_after']):
            raise ValueError('phase failed or input evidence changed')
        expected_inputs = {str(workspace / p): raw for p, raw in value['files'].items()}
        expected_inputs[str(Path(phase['registry']) / 'bazel_registry.json')] = b'{}\n'
        if set(phase['inputs_before']) != set(expected_inputs):
            raise ValueError('unplanned or missing fixture input evidence')
        for path, raw in expected_inputs.items():
            entry = phase['inputs_before'][path]
            if entry['path'] != path or entry['kind'] != 'file' or not valid_evidence(entry, raw):
                raise ValueError('snapshot differs from planned real fixture')
        if [(r['command'], r['target'], r['extra']) for r in phase['runs']] != steps_for(item, phase['probe'], phase['engine']):
            raise ValueError('missing, duplicate or unexpected command')
        records = []
        for index, link in enumerate(phase['runs']):
            record = report['runs'][link['id']]
            argv = command_argv(args, reference, phase['engine'], Path(phase['cache']), Path(phase['registry']),
                                link['command'], link['target'], link['extra'], value['flags'])
            if (record['name'] != item['name'] + '.' + phase['probe'] + '.' + phase['engine'] + '.' + str(index)
                    or record['argv'] != argv or record['cwd'] != str(workspace)
                    or record['timeout_seconds'] != args.timeout):
                raise ValueError('invocation provenance mismatch')
            records.append(record)
        if value['diagnostic']:
            require_rejection(records[0], value['diagnostic'])
            if phase['diagnostic'] != value['diagnostic']:
                raise ValueError('wrong rejection class')
            if phase['engine'] == 'native' and phase['publication_entries'] != []:
                raise ValueError('negative phase published repositories')
            if value['diagnostic'] == 'os-environ' and (phase['engine'] != 'native'
                    or any(key in phase for key in ('artifacts', 'copy', 'canonical'))):
                raise ValueError('os-environ boundary cannot publish outputs or claim reference parity')
            continue
        if value['main_only']:
            if success(records[0]).splitlines() != ['//:data'] or phase['publication_entries'] != []:
                raise ValueError('main query was not lazy')
            continue
        for record in records:
            success(record)
        files, roots = phase['artifacts']['files'], phase['artifacts']['roots']
        if set(files) != set(value['expected']) or set(roots) != set(files):
            raise ValueError('missing generated artifacts')
        for key, (raw, executable) in value['expected'].items():
            entry, root = files[key], Path(roots[key])
            if (entry['kind'] != 'file' or not valid_evidence(entry, raw, executable)
                    or Path(entry['path']) != root / key):
                raise ValueError('generated bytes, mode or root mismatch')
            if phase['engine'] == 'native':
                if (root.parent != direct.trees_directory(Path(phase['cache']))
                        or not re.fullmatch('[0-9a-f]{64}', root.name) or entry['module_absent'] is not True
                        or not valid_evidence(entry['repo_boundary'], b'')
                        or entry['repo_boundary']['path'] != str(root / 'REPO.bazel')):
                    raise ValueError('missing native publication boundary')
            else:
                if (not root.is_relative_to(reference.output_base.parent)
                        or Path(entry['logical_path']).parts[-2:] != (value['canonical'], key)):
                    raise ValueError('reference canonical artifact root mismatch')
        def listing(target):
            record = next(r for r, link in zip(records, phase['runs'])
                          if link['target'] == target and link['command'] == ('build' if phase['engine'] == 'native' else 'cquery'))
            return (direct.native_paths(record, target, Path(report['scratch_directory'])) if phase['engine'] == 'native'
                    else reference_output_paths(success(record), workspace, reference.output_base))
        if sorted(map(str, listing('//:data'))) != sorted(e['logical_path'] for e in files.values()):
            raise ValueError('generated evidence not linked to actual build output')
        copy = phase['copy']
        if (copy['kind'] != 'file' or not valid_evidence(copy, value['expected']['data'][0])
                or list(map(str, listing('//:copy'))) != [copy['path']] or Path(copy['path']).name != 'copy.txt'):
            raise ValueError('copy evidence not linked to real action output')
        canonical_result(next(r for r, link in zip(records, phase['runs']) if link['target'] == value['target']), value['canonical'])
        if phase['canonical'] != '@@' + value['canonical'] + '//:data':
            raise ValueError('canonical evidence mismatch')
        if phase['engine'] == 'native' and item['name'] != 'native.refresh-and-repair':
            count = 2 if item['name'] == 'labels.owners-and-forward-sibling' else 1
            entries = phase['publication_entries']
            if (len(entries) != count or len(set(entries)) != count
                    or any(not re.fullmatch('[0-9a-f]{64}', e) for e in entries)
                    or not {Path(r).name for r in roots.values()}.issubset(entries)):
                raise ValueError('unused-repository laziness evidence missing')
    if item['name'] == 'native.refresh-and-repair':
        for before, after in zip(check['phases'], check['phases'][1:]):
            mutation = after['intentional_mutation']
            if mutation['kind'] != after['probe'] or mutation['before'] != before['inputs_after'] or mutation['after'] != after['inputs_before']:
                raise ValueError('unrecorded intentional mutation')
            changed = [p for p in mutation['before'] if mutation['before'][p] != mutation['after'][p]]
            relative = {'tag-change': 'MODULE.bazel', 'callback-change': 'ext.bzl', 'input-change': 'input', 'repair': None}[after['probe']]
            if changed != ([] if relative is None else [str(workspace / relative)]):
                raise ValueError('unexpected fixture mutation')
            same_root = before['artifacts']['roots'] == after['artifacts']['roots']
            if same_root != (after['probe'] == 'repair'):
                raise ValueError('cache invalidation/repair identity mismatch')
        repair = check['phases'][-1]['intentional_mutation']['published']
        previous = check['phases'][-2]['artifacts']['files']['data']
        if (not valid_evidence(repair['before'], fixture(item['name'], 'input-change')['expected']['data'][0])
                or repair['before']['path'] != previous['path']
                or repair['after']['path'] != previous['path']
                or not valid_evidence(repair['after'], b'CORRUPT PUBLISHED TREE\n')):
            raise ValueError('missing deliberate published-cache corruption')
    if item['name'] == 'native.parallel-publication':
        if check['phases'][0]['artifacts']['roots'] != check['phases'][1]['artifacts']['roots']:
            raise ValueError('parallel CLIs published different roots')


def report_passes(report):
    try:
        if ([(c['name'], c['kind']) for c in report['checks']] != PLAN or report['planned_checks'] != CASES
                or report['case_inputs_sha256'] != digest(json.dumps(CASES, sort_keys=True).encode())
                or report['errors'] or report['cleanup'] or report['scratch_cleaned'] is not True
                or report['reference_shutdown_succeeded'] is not True or report['reference_shutdown_attempted'] is not True
                or report['sources_unchanged'] is not True or report['sources_before'] != report['sources_after']
                or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(k == v['path'] and valid_evidence(v) for k, v in report['sources_before'].items())
                or report['executables'] != report['executables_after']
                or report['reference_release'] != 'bazel 9.3.0'
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['completed_checks'] != len(CASES)
                or report['completed_differential_cases'] != sum(c['kind'] == 'differential' for c in CASES)
                or report['completed_phases'] != sum(len(phases_for(c)) for c in CASES)):
            return False
        args = argument_parser().parse_args([])
        args.native, args.reference = Path(report['configuration']['native']), Path(report['configuration']['reference'])
        args.timeout = report['configuration']['timeout']
        if not isinstance(args.timeout, (float, int)) or not 0 < args.timeout < float('inf'):
            return False
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        if report['cleanup_receipt'] != {'path': str(scratch), 'removed': True}:
            return False
        reference = DirectReference(args.reference, workspace, scratch, None, 'module-extensions')
        for engine in ('native', 'reference'):
            executable = report['executables'][engine]
            if (executable['path'] != report['configuration'][engine]
                    or not re.fullmatch('[0-9a-f]{64}', executable['sha256'])
                    or type(executable['size_bytes']) is not int or executable['size_bytes'] <= 0):
                return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            engine = 'reference' if record['argv'][0] == str(args.reference) else 'native'
            if (record['argv'][0] != report['configuration'][engine]
                    or record['executable_sha256'] != report['executables'][engine]['sha256']
                    or not record['environment_keys'] or not record['environment']):
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(record[field + '_base64'], validate=True)
                if (len(raw) > direct.MAX_STREAM or digest(raw) != record[field + '_sha256']
                        or raw.decode('utf-8', errors='replace') != record[field]
                        or record[field + '_full_sha256'] != digest(raw)
                        or record[field + '_full_size_bytes'] != len(raw)):
                    return False
        if (runs[0]['name'] != 'reference.version' or runs[0]['argv'] != reference.startup + ['version', '--gnu_format']
                or runs[0]['cwd'] != str(workspace) or runs[0]['timeout_seconds'] != args.timeout
                or success(runs[0]).strip() != 'bazel 9.3.0'
                or runs[-1]['name'] != 'module-extensions.shutdown'
                or runs[-1]['argv'] != reference.startup + ['shutdown'] + list(COMMON_FLAGS)
                or runs[-1]['cwd'] != str(workspace) or runs[-1]['timeout_seconds'] != min(args.timeout, 15)):
            return False
        success(runs[-1])
        used = [0, len(runs) - 1]
        for check in report['checks']:
            if check['status'] != 'PASS' or check.get('error') or check['parity_claim'] != (check['kind'] == 'differential'):
                return False
            validate_check(check, report, args, workspace, reference)
            used.extend(r['id'] for phase in check['phases'] for r in phase['runs'])
        return sorted(used) == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def argument_parser():
    parser = direct.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/module-extensions-validation.json')
    return parser


def finish(reference, scratch, report):
    if reference is not None:
        try:
            report['reference_shutdown_attempted'] = True
            reference.shutdown()
            shutdown = report['runs'][-1]
            if shutdown['name'] != 'module-extensions.shutdown':
                raise ValueError('shutdown must record a real invocation')
            success(shutdown)
            report['reference_shutdown_succeeded'] = not report['cleanup']
        except Exception as exc:
            report['errors'].append('reference shutdown: ' + str(exc))
    if scratch is not None:
        try:
            shutil.rmtree(scratch)
            report['scratch_cleaned'] = not scratch.exists()
            report['cleanup_receipt'] = dict(path=str(scratch), removed=report['scratch_cleaned'])
        except OSError as exc:
            report['errors'].append('scratch cleanup: ' + str(exc))


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if (args.output in [args.native, args.reference, *SOURCES]
            or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests'))):
        raise ValueError('report must not overwrite sources or executables')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
        planned_checks=CASES, case_inputs_sha256=digest(json.dumps(CASES, sort_keys=True).encode()),
        scratch_cleaned=False, reference_shutdown_attempted=False, reference_shutdown_succeeded=False,
        sources_unchanged=False, configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        limitations=[
            '16 cases: eight differential, eight native boundaries; not full Bazel or application compatibility.',
            'Only bounded non-isolated None-returning callbacks; no metadata, facts, OS or filesystem context fiction.',
            'Call-time mutable string-list snapshot is observable; unsupported nested dict/int-list attrs are not claimed.',
            'Genuine pinned Bazel built-in closure may download from BCR; no fake builtins or user workspace.',
            'Negative cases compare specific controlled diagnostics, not exact error strings or in-memory RepoSpec state.',
            'No partial generated tree publication is observable; source snapshots freeze each invocation phase.',
            'Raw template bytes and executable bits checked; read-only cache permission differences recorded.',
            'Fresh processes test tags, callback code, binary templates, cache repair and two concurrent native builds.',
            'No Bazel test/run; durations are diagnostics, not performance claims. Streams over 1 MiB fail.',
            'Inherited environment names recorded; values restricted to safe allowlist. No credentials changed.'])
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = validate_binaries(args)
        scratch = Path(tempfile.mkdtemp(prefix='module-extensions-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(ROOT_MODULE)
        reference = DirectReference(args.reference, workspace, scratch, runner, 'module-extensions')
        reference.started = True
        report['reference_release'] = success(runner.run('reference.version', reference.startup + ['version', '--gnu_format'], workspace)).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('reference is not pinned Bazel 9.3.0')
        run_all(args, runner, scratch, workspace, reference)
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__ + ': ' + str(exc))
    finally:
        finish(reference, scratch, report)
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['sources_unchanged'] = report.get('sources_before') == report['sources_after']
            if 'executables' in report:
                report['executables_after'] = validate_binaries(args)
        except Exception as exc:
            report['errors'].append('final source/executable evidence: ' + str(exc))
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['completed_differential_cases'] = sum(c['status'] == 'PASS' and c['kind'] == 'differential' for c in report['checks'])
        report['completed_phases'] = sum(p['status'] == 'PASS' for c in report['checks'] for p in c['phases'])
        if report_passes(report):
            report['status'] = 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print('{} (bounded ordinary module extensions only): {}'.format(report['status'], args.output))
    return 130 if interrupted else 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
