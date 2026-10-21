#!/usr/bin/env python3
"""Bounded scalar string_dict CLI contract; never full Bazel/application parity.

Reference preparation and known upstream defects are not differential PASSes.
No ambient environment is inspected, copied, or recorded. Native execution needs
an explicitly frozen candidate hash; there is no fallback executable.
"""

import base64
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile

import validate_string_list_dict as common

# Integration-19: strip the native default memory-cap announcement (one first
# stderr line, native-only safety valve; not a diagnostic). See
# validate_module_extensions.strip_memory_announcement.
MEMORY_ANNOUNCEMENT = re.compile(
    r'\Abazel-rs: memory limit defaulted to \d+ MB \([^\n]*\); set BAZEL_RS_MAX_MEMORY_MB '
    r'\(recommended\) or pass --memory-limit=<MB> to change it; the cap bounds bazel-rs itself, '
    r'not child actions\n')

def strip_memory_announcement(text):
    return MEMORY_ANNOUNCEMENT.sub('', text, count=1)
import validate_repository_environment as controlled
from validate import ROOT, file_evidence, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256

digest, evidence, snapshot = common.digest, common.evidence, common.snapshot
valid_evidence, indent = common.valid_evidence, common.indent
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
OFFICIAL_COMMIT = 'e89755babf3e941bc1e9d8120b1cd48854ab9dfa'
OFFICIAL = {
    'src/main/java/com/google/devtools/build/lib/starlarkbuildapi/StarlarkAttrModuleApi.java':
        'af70c851882fa049034184dbb6f6580731cfa738d79dfb8abcf61af176257670',
    'src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkAttrModule.java':
        '388421c44c623c1c6625fd9f2b059d2a7d1e13b8d45e7c96173f24866a917967',
}
SOURCES = list(dict.fromkeys([Path(__file__).resolve(), ROOT / 'tests/test_validate_string_dict.py',
    *common.SOURCES, Path(controlled.__file__).resolve(), ROOT / 'Cargo.toml', ROOT / 'Cargo.lock',
    *sorted((ROOT / 'src').rglob('*.rs')), *sorted(p for p in REGISTRY.rglob('*') if p.is_file())]))
INITIAL = '{"z":"before","a":"","label":"@missing//pkg:target"}'
MUTATE = 'D["z"]="after"\nD["new"]="new"\n'
SERIALIZE = ('def render(d):\n    return type(d)+"\\nKEYS="+"|".join(d.keys())'
    '+"\\nVALUES="+"|".join(d.values())+"\\nTYPES="+"|".join([type(v) for v in d.values()])+"\\n"\n')
BEFORE = b'dict\nKEYS=z|a|label\nVALUES=before||@missing//pkg:target\nTYPES=string|string|string\n'
AFTER = b'dict\nKEYS=z|a|label|new\nVALUES=after||@missing//pkg:target|new\nTYPES=string|string|string|string\n'
EMPTY = b'dict\nKEYS=\nVALUES=\nTYPES=\n'
BAD_VALUES = {'key-int':'{1:"x"}', 'key-bool':'{True:"x"}', 'value-list':'{"z":["x"]}',
    'value-tuple':'{"z":("x",)}', 'value-int':'{"z":1}', 'value-bool':'{"z":False}',
    'value-none':'{"z":None}', 'value-dict':'{"z":{}}', 'not-dict':'["x"]'}
BAD_CTORS = {'default-none':'default=None', 'default-callable':'default=render',
    'doc-int':'doc=1', 'allow-int':'allow_empty=1', 'mandatory-int':'mandatory=0',
    'two-positional':'True,{}', 'values':'values=["x"]', 'duplicate':'True,allow_empty=True'}
case = common.case
CASES = [
    case('constructor', ('omitted','named','positional','alias')+tuple(BAD_CTORS)),
    case('ordinary', ('ordered','unicode','frozen')),
    case('validation', tuple('custom.'+p for p in BAD_VALUES)+tuple('direct.'+p for p in BAD_VALUES)
         +('custom.empty-explicit','custom.empty-default','custom.empty-string','direct.empty','tags.empty')),
    case('defaults', ('custom.snapshot','custom.call-snapshot','custom.none','custom.mandatory-empty',
         'custom.mandatory-missing','custom.mandatory-none','direct.snapshot','direct.none',
         'direct.mandatory-missing','direct.mandatory-none','tags.none','tags.mandatory-none')),
    case('lifecycle', tuple(r+'.'+s for r in ('custom','direct') for s in ('first','repeat','change','restore'))),
    case('repositories', ('direct.module-late','tags.module-late','ordinary.per-call','direct.frozen','tags.frozen')),
    case('templates', ('direct.ordered','ordinary.ordered')),
    case('native-profile', ('configurable-false','configurable-true','configurable-none','configurable-int',
         'direct.unicode-key','direct.unicode-value','tags.unicode-key','tags.unicode-value',
         'loaded-build-ctor','loaded-build-helper'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASES]
DIAGNOSTICS = {
    'key': r"expected value of type 'string' for dict key element",
    'value': r"expected value of type 'string' for dict value element",
    'dict': r"expected value of type 'dict\(string, string\)' for attribute 'd'",
    'mandatory': r"(?:missing value for mandatory attribute 'd'|mandatory attribute 'd' isn't being specified)",
    'empty': r'(?:attribute d: attribute must be non empty|in d attribute of[^\n]*attribute must be non empty)',
    'frozen': r'trying to mutate a frozen dict value',
    'default-none': r"parameter 'default' got value of type 'NoneType', want 'dict'",
    'default-callable': r"parameter 'default' got value of type 'function', want 'dict'",
    'doc-int': r"parameter 'doc' got value of type 'int', want 'string or NoneType'",
    'allow-int': r"parameter 'allow_empty' got value of type 'int', want 'bool'",
    'mandatory-int': r"parameter 'mandatory' got value of type 'int', want 'bool'",
    'two-positional': r'string_dict\(\) accepts no more than 1 positional argument but got 2',
    'values': r"string_dict\(\) got unexpected keyword argument 'values'",
    'duplicate': r"(?:multiple values for (?:parameter |argument )?'allow_empty'|allow_empty[^\n]*multiple)",
    'configurable': r'string_dict[^\n]*configurable[^\n]*(?:unsupported|not supported)',
    'ascii': r'(?:non.ASCII|ASCII.only|only ASCII|must[^\n]*ASCII)',
}
# No native 'frozen' alternative: since N20 the native interpreter prints Bazel's
# Starlark.checkMutable text (Starlark.java:262-265; docs/STARLARK_VENDOR.md).
NATIVE = {
    'key': r'(?:string_dict[^\n]*key[^\n]*string|attribute "d"[^\n]*key[^\n]*string|tag dictionary key must be a string)',
    'value': r'(?:string_dict[^\n]*value[^\n]*string|attribute "d"[^\n]*value[^\n]*string)',
    'dict': r'(?:attribute "d": |string_dict[^\n]*)expected a dict of strings to strings',
    'mandatory': r'missing mandatory (?:(?:repository|tag) )?attribute "d"',
    'two-positional': r'Found 1 extra positional argument\(s\) for call to string_dict',
    'values': r'(?:string_dict[^\n]*values|values[^\n]*string_dict)[^\n]*(?:unexpected|parameter|argument)',
    'duplicate': r'(?:allow_empty[^\n]*(?:multiple|twice)|(?:multiple|twice)[^\n]*allow_empty)',
}
BAD = re.compile(common.UNRELATED.pattern+r'|has no attribute|Unknown attribute|Variable .+ not found|name .+ is not defined', re.I)
# N33: the native interpreter reports Bazel's binder texts; each body is exactly
# genuine Bazel 9.3.0's "Error in string_dict: <body>" for the same fixture
# (previously starlark-rust's "Type of parameter `allow_empty` doesn't match, ...",
# "Found `values` extra named parameter(s) ...", "Argument `allow_empty` occurs more
# than once"). The native trace/location binding below is unchanged.
NATIVE_CTOR_ERRORS = {
    'allow-int': "in call to string_dict(), parameter 'allow_empty' got value of type 'int', want 'bool'",
    'mandatory-int': "in call to string_dict(), parameter 'mandatory' got value of type 'int', want 'bool'",
    'values': "string_dict() got unexpected keyword argument 'values'",
    'duplicate': "string_dict() got multiple values for argument 'allow_empty'",
}
NATIVE_SCALAR_ERROR = 'attribute "d": expected a dict of strings to strings'
# BUILD loading footer (src/package.rs NATIVE_LOADING_FOOTER) and the literal of
# captures made before the native-rule-class increment (docs/NATIVE_BUILD_RULES.md).
NATIVE_LOADING_FOOTERS = (
    'Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and Starlark rule() '
    'definitions. select() resolves against modelled config_setting values (docs/CONFIGURATION.md).\n',
    'Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and Starlark rule() '
    'definitions. select() values load, but native analysis does not resolve them.\n',
    'Supported rules: filegroup, genrule, and local custom rules. select and non-local '
    'repositories remain unsupported.\n',
)


def require_bound_native_diagnostic(row, diagnostic):
    """Observed native wording only; a generic shape error is NOT Java field detail.

The key/value category comes from the immutable invalid fixture. The error body
alone cannot distinguish the categories and must never be accepted without its
planned query, source expression, and location. Replay additionally binds argv,
input bytes and absence of outputs/publication before accepting the phase.
"""
    if diagnostic in NATIVE_CTOR_ERRORS:
        name, probe, path, column = 'constructor', diagnostic, '//:defs.bzl', 3
        value = fixture(name,probe)
        source = 'A=attr.string_dict('+BAD_CTORS[probe]+')'
        line = value['files']['defs.bzl'].decode().splitlines().index(source)+1
        header = 'bazel-rs: cannot evaluate //:defs.bzl: Traceback (most recent call last):'
        error, footers = NATIVE_CTOR_ERRORS[diagnostic], ('',)
    else:
        matched = re.fullmatch(r'validation\.(custom\.(?:key-(?:int|bool)|value-(?:list|tuple|int|bool|none|dict)))\.native\.0',row.get('name',''))
        if not matched or not Path(row.get('cwd','')).is_absolute():
            raise ValueError('unbound native scalar diagnostic')
        name, probe = 'validation', matched[1]
        value = fixture(name,probe)
        if value['diagnostic'] != diagnostic:
            raise ValueError('wrong native scalar fixture category')
        source = 'r(name="one",d='+BAD_VALUES[probe.split('.')[1]]+')'
        line = value['files']['BUILD.bazel'].decode().splitlines().index(source)+1
        path, column = str(Path(row['cwd'])/'BUILD.bazel'), 1
        header, error = 'bazel-rs: Traceback (most recent call last):', NATIVE_SCALAR_ERROR
        footers = NATIVE_LOADING_FOOTERS
    trace = (header+'\n'+f'  * {path}:{line}, in <module>\n      {source}\nerror: {error}\n'
        +f' --> {path}:{line}:{column}\n  |\n{line} | {source}\n'
        +'  | '+' '*(column-1)+'^'*len(source[column-1:])+'\n  |\n\n')
    if (row.get('name') != name+'.'+probe+'.native.0'
            or strip_memory_announcement(row['stderr']) not in [trace + footer for footer in footers]
            or row['stdout'] != '' or row['returncode'] != 1):
        raise ValueError('unbound native '+diagnostic+' diagnostic')
    return error


def require_rejection(row, diagnostic, engine='reference'):
    if engine not in ('reference','native'):
        raise ValueError('unknown engine')
    pattern = DIAGNOSTICS[diagnostic]
    if engine == 'native':
        pattern += '|'+NATIVE.get(diagnostic, r'(?!)')
        if (diagnostic in NATIVE_CTOR_ERRORS or diagnostic in ('key','value')
                and (row.get('name','').startswith('validation.custom.') or NATIVE_SCALAR_ERROR in row['stderr'])):
            error = require_bound_native_diagnostic(row,diagnostic)
            pattern = r'(?m:^error: '+re.escape(error)+r'$)'
        if diagnostic in ('default-none','default-callable'):
            pattern += r'|(?m:^error: expected a dict of strings to strings$)'
            source = 'attr.string_dict('+BAD_CTORS[diagnostic]+')'
            if source not in row['stderr']:
                raise ValueError('unbound default diagnostic')
        if diagnostic == 'doc-int':
            # Observed native constructor rejection, not Java diagnostic parity.
            # Bind the entire trace to this planned pre-callback query and source;
            # nearby wording, another constructor, or additional output is not proof.
            source = 'A=attr.string_dict(doc=1)'
            lines = fixture('constructor','doc-int')['files']['defs.bzl'].decode().splitlines()
            line = lines.index(source)+1
            error = 'error: attr.string_dict doc must be a string or None'
            trace = ('bazel-rs: cannot evaluate //:defs.bzl: Traceback (most recent call last):\n'
                f'  * //:defs.bzl:{line}, in <module>\n      {source}\n{error}\n'
                f' --> //:defs.bzl:{line}:3\n  |\n{line} | {source}\n'
                +'  |   '+'^'*len(source[2:])+'\n  |\n\n')
            if (row.get('name') != 'constructor.doc-int.native.0' or strip_memory_announcement(row['stderr']) != trace
                    or row['stdout'] != '' or row['returncode'] != 1):
                raise ValueError('unbound native doc-int diagnostic')
            pattern = r'(?m:^'+re.escape(error)+r'$)'
    if (type(row['returncode']) is not int or not 0 < row['returncode'] < 128
            or row['returncode'] in (37,101,124,125,126,127) or row['stdout'].strip()
            or any(row.get(k) for k in ('timed_out','spawn_error','interrupted','output_truncated'))
            or BAD.search(row['stderr']) or not re.search(pattern, row['stderr'])):
        raise ValueError('expected controlled scalar string_dict rejection: '+diagnostic)


def fixture(name, probe):
    item = next((c for c in CASES if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned fixture')
    route, _, variant = probe.partition('.')
    if route not in ('custom','direct','ordinary','tags'):
        route, variant = 'custom', probe
    ctor, arg, prefix = 'attr.string_dict(default=D,doc=None)', 'd=D', 'D='+INITIAL+'\n'
    expected, diagnostic, failcmd = BEFORE, None, 'query'
    after_ctor = after_call = mutation = ''
    two = False
    if name == 'constructor':
        arg = ''
        if probe in BAD_CTORS:
            ctor, diagnostic = 'attr.string_dict('+BAD_CTORS[probe]+')', probe
        elif probe == 'omitted':
            ctor, expected = 'attr.string_dict()', EMPTY
        elif probe == 'positional':
            ctor = 'attr.string_dict(True,default=D)'
        elif probe == 'named':
            ctor = 'attr.string_dict(allow_empty=True,default=D,doc=None,mandatory=False)'
        elif probe == 'alias':
            prefix += 'ctor=attr.string_dict\n'
            ctor = 'ctor(default=D)'
    if name == 'ordinary' and probe == 'unicode':
        prefix = 'D={"é":"☃","label":"@missing//pkg:target"}\n'
        expected = 'dict\nKEYS=é|label\nVALUES=☃|@missing//pkg:target\nTYPES=string|string\n'.encode()
    if name == 'validation':
        if variant in BAD_VALUES:
            arg = 'd='+BAD_VALUES[variant]
            diagnostic = 'key' if variant.startswith('key') else 'dict' if variant == 'not-dict' else 'value'
        else:
            ctor = 'attr.string_dict(False)'
            arg, expected = ('d={"z":""}', b'dict\nKEYS=z\nVALUES=\nTYPES=string\n') if variant == 'empty-string' else ('d={}', EMPTY)
            if variant == 'empty-default':
                arg = ''
            if route == 'custom' and variant != 'empty-string':
                diagnostic, failcmd = 'empty', 'build'
    if name == 'defaults':
        if variant == 'snapshot':
            after_ctor, arg = MUTATE, ''
        elif variant == 'call-snapshot':
            after_call = MUTATE
        elif variant == 'none':
            arg = 'd=None'
        elif variant.startswith('mandatory'):
            ctor = 'attr.string_dict(mandatory=True,default=D)'
            arg = '' if variant == 'mandatory-missing' else 'd=None' if variant == 'mandatory-none' else 'd={}'
            expected = EMPTY
            if variant != 'mandatory-empty':
                diagnostic = 'mandatory'
    if name == 'lifecycle':
        if variant == 'change':
            prefix = 'D={"z":"changed","a":"","label":"@missing//pkg:target"}\n'
            expected = BEFORE.replace(b'before',b'changed')
        ctor = 'attr.string_dict()'
    if variant == 'module-late':
        after_call, expected = MUTATE, AFTER
    if variant == 'per-call':
        two, expected = True, BEFORE+AFTER
    if variant == 'frozen':
        mutation, diagnostic = 'd["new"]="bad"\n', 'frozen'
        failcmd = 'build' if route == 'custom' else 'query'
    if name == 'native-profile':
        if probe.startswith('configurable-'):
            ctor = 'attr.string_dict(configurable='+{'false':'False','true':'True','none':'None','int':'1'}[probe.split('-')[1]]+')'
            diagnostic = 'configurable'
        elif variant.startswith('unicode'):
            arg = 'd={"é":"x"}' if variant.endswith('key') else 'd={"z":"é"}'
            diagnostic = 'ascii'
    files = {'MODULE.bazel':'module(name="dict_contract")\n'}
    definitions = SERIALIZE+prefix+'A='+ctor+'\n'+after_ctor
    args = ','+arg if arg else ''
    if route == 'custom':
        body = 'd=ctx.attr.d\n'+mutation+'out=ctx.actions.declare_file(ctx.label.name+".txt")\nctx.actions.write(out,render(d))\nreturn [DefaultInfo(files=depset([out]))]'
        definitions += 'def _impl(ctx):\n'+indent(body)+'r=rule(implementation=_impl,attrs={"d":A})\n'
        build_prefix = prefix.replace('ctor=attr.string_dict\n','')
        build = 'load(":defs.bzl","r")\n'+build_prefix+'r(name="one"'+args+')\n'+after_call
        labels, query = ['//:one'], '//:one'
    else:
        body = 'd=ctx.attr.d\n'+(mutation if route != 'tags' else '')
        if name == 'templates':
            # Sequential replacement order is observable: A -> B -> DONE.
            prefix = 'D={"A":"B","B":"DONE"}\n'
            definitions = SERIALIZE+prefix+'A=attr.string_dict()\n'
            body += 'ctx.file("input","A",executable=False)\nctx.template("data","input",substitutions=d,executable=True)\n'
            expected = b'DONE'
        else:
            body += 'ctx.file("data",render(d),executable=True)\n'
        body += 'ctx.file("BUILD.bazel",\'exports_files(["data"],visibility=["//visibility:public"])\\n\',executable=False)'
        definitions += 'def _impl(ctx):\n'+indent(body)+'r=repository_rule(implementation=_impl,attrs={"d":A})\n'
        if route == 'direct':
            files['MODULE.bazel'] += 'r=use_repo_rule("//:defs.bzl","r")\n'+prefix+'r(name="one"'+args+')\n'+after_call
        else:
            files['MODULE.bazel'] += 'e=use_extension("//:ext.bzl","e")\nuse_repo(e,"one"'+(',"two"' if two else '')+')\n'
            ext = 'load(":defs.bzl","r")\n'
            if route == 'tags':
                ext += SERIALIZE+prefix+'A='+ctor+'\n'
                extbody = 'for m in ctx.modules:\n    for t in m.tags.item:\n        d=t.d\n'+indent(indent(mutation))+'        r(name="one",d=d)'
                ext += 'def _ext(ctx):\n'+indent(extbody)+'e=module_extension(implementation=_ext,tag_classes={"item":tag_class(attrs={"d":A})})\n'
                files['MODULE.bazel'] += prefix+'e.item('+arg+')\n'+after_call
            else:
                extbody = prefix+'r(name="one"'+args+')\n'
                if two:
                    extbody += MUTATE+'r(name="two",d=D)\n'
                ext += 'def _ext(ctx):\n'+indent(extbody)+'e=module_extension(implementation=_ext)\n'
            files['ext.bzl'] = ext
        labels, query, build = ['@one//:data']+(['@two//:data'] if two else []), '@one//:data', ''
    if name == 'native-profile' and probe.startswith('loaded-build'):
        definitions += ('ctor=attr.string_dict\ndef helper():\n    return ctor()\n'
            'def observe(a):\n    if type(a)!="Attribute" or repr(a)!="<attr.string_dict>":\n'
            '        fail("INVALID_NATIVE_STRING_DICT_METADATA")\n'
            '    print("NATIVE_METADATA",type(a),repr(a))\n')
        # BUILD has neither attr nor print. The direct case still invokes the
        # loaded constructor IN BUILD; only observation runs in the .bzl helper.
        build = 'load(":defs.bzl","r","ctor","helper","observe")\nobserve('+('ctor()' if probe.endswith('ctor') else 'helper()')+')\nr(name="one")\n'
    files['defs.bzl'] = definitions
    files['BUILD.bazel'] = build+'filegroup(name="sources",srcs='+repr(labels)+')\ngenrule(name="result",srcs='+repr(labels)+',outs=["result.txt"],cmd="cat $(SRCS) > $@")\n'
    return dict(files={p:s.encode() for p,s in files.items()}, expected=expected, route=route,
        diagnostic=diagnostic, failure_command=failcmd, query=query,
        repos=0 if route == 'custom' else 2 if two else 1, actions=2 if route == 'custom' else 1)


def phases_for(item, reference_only=False):
    return common.phases_for(item, reference_only)


def steps_for(item, probe, engine):
    value = fixture(item['name'],probe)
    steps = [('query',value['query'])]
    if value['diagnostic']:
        return steps+([('build','//:result')] if value['failure_command'] == 'build' else [])
    for _ in range(2):
        steps.append(('build','//:result'))
        if engine == 'reference':
            steps.append(('cquery','//:result'))
    if value['repos']:
        steps.append(('build','//:sources'))
        if engine == 'reference':
            steps.append(('cquery','//:sources'))
    return steps


def command_argv(args, ref, engine, cache, registry, command, target):
    argv = common.extensions.command_argv(args,ref,engine,cache,registry,command,target,
                                         ['--output=files'] if command == 'cquery' else [])
    argv = [v for v in argv if not v.startswith('--repo_env')]
    if engine == 'reference':
        argv[-1:-1] = ['--noshow_progress','--incompatible_autoload_externally=']
    return argv


def manifest(paths):
    return {str(p):dict(sha256=digest(p.read_bytes()),size_bytes=p.stat().st_size) for p in paths}


def source_pins(args, env):
    commit = subprocess.check_output(['/usr/bin/git','-C',str(args.official_source),'rev-parse','HEAD'],env=env,text=True).strip()
    if commit != OFFICIAL_COMMIT:
        raise ValueError('wrong official source commit')
    for relative, sha in OFFICIAL.items():
        raw = (args.official_source / relative).read_bytes()
        blob = subprocess.check_output(['/usr/bin/git','-C',str(args.official_source),'show',commit+':'+relative],env=env)
        if digest(raw) != sha or raw != blob:
            raise ValueError('official source pin mismatch')
    return dict(OFFICIAL)


class RecordingRunner(controlled.RecordingRunner):
    def run(self, name, argv, cwd, timeout=None):
        report = self.report
        if manifest(SOURCES) != report['sources_before'] or executable_evidence(self.args) != report['executables']:
            raise ValueError('precommand source/executable binding changed')
        if source_pins(self.args,controlled.controlled_environment(self.scratch,{})) != OFFICIAL:
            raise ValueError('precommand official source changed')
        row = super().run(name,argv,cwd,timeout)
        row['preflight_binding_sha256'] = digest(json.dumps(report['sources_before'],sort_keys=True).encode())
        return row


def run_phase(args, runner, scratch, workspace, ref, item, probe, engine):
    value = fixture(item['name'],probe)
    group = probe.split('.')[0] if item['name'] == 'lifecycle' else probe
    root = scratch / 'cases' / item['name'] / group / engine
    registry, paths = common.materialize(root,workspace,value,item['name']=='lifecycle' and not probe.endswith('.first'))
    shutil.copytree(REGISTRY,registry,dirs_exist_ok=True)
    paths = list(workspace / p for p in value['files'])+sorted(p for p in registry.rglob('*') if p.is_file())
    phase = dict(probe=probe,engine=engine,cache=str(root/'native-cache'),registry=str(registry),
                 runs=[],outputs=[],sources=[],status='FAIL',inputs_before=snapshot(paths))
    try:
        for index,(command,target) in enumerate(steps_for(item,probe,engine)):
            row = runner.run(item['name']+'.'+probe+'.'+engine+'.'+str(index),
                command_argv(args,ref,engine,Path(phase['cache']),registry,command,target),workspace)
            phase['runs'].append(row['id'])
            if value['diagnostic'] and command == value['failure_command']:
                require_rejection(row,value['diagnostic'],engine)
                if engine == 'native':
                    phase['publication_entries'] = common.direct.no_publication(Path(phase['cache']))
                break
            common.success(row)
            if command == 'query':
                common.require_query(row,value,engine)
                if item['name']=='native-profile' and probe.startswith('loaded-build'):
                    require_metadata(row)
            if command == ('build' if engine == 'native' else 'cquery'):
                paths_out = common.direct.native_paths(row,target,scratch) if engine == 'native' else reference_output_paths(row['stdout'],workspace,ref.output_base)
                entries = [evidence(p) for p in paths_out]
                phase['outputs' if target == '//:result' else 'sources'].extend(entries)
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    finally:
        phase['inputs_after'] = snapshot(paths)
    return phase


def require_metadata(row):
    if not re.search(r'(?m)(?:^|: )NATIVE_METADATA Attribute <attr\.string_dict>$',row['stderr']):
        raise ValueError('native BUILD constructor did not expose valid metadata')


def validate_phase(phase, report, args, workspace, ref, item):
    probe, engine = phase['probe'],phase['engine']
    value = fixture(item['name'],probe)
    if phase['status']!='PASS' or phase.get('error') or phase['inputs_before']!=phase['inputs_after']:
        raise ValueError('failed phase or mutated inputs')
    planned = {str(workspace/p):raw for p,raw in value['files'].items()}
    planned.update({str(Path(phase['registry'])/p.relative_to(REGISTRY)):p.read_bytes() for p in REGISTRY.rglob('*') if p.is_file()})
    if set(planned)!=set(phase['inputs_before']):
        raise ValueError('unplanned source inputs')
    for p,raw in planned.items():
        entry = phase['inputs_before'][p]
        if entry['path']!=p or entry['kind']!='file' or not valid_evidence(entry,raw):
            raise ValueError('source differs from immutable fixture')
    steps = steps_for(item,probe,engine)
    if len(phase['runs'])!=len(steps):
        raise ValueError('missing or extra command')
    output_index = source_index = 0
    for index,(rid,(command,target)) in enumerate(zip(phase['runs'],steps)):
        row = report['runs'][rid]
        expected_argv = command_argv(args,ref,engine,Path(phase['cache']),Path(phase['registry']),command,target)
        if (row['argv']!=expected_argv or row['cwd']!=str(workspace) or row['timeout_seconds']!=args.timeout
                or row['name']!=item['name']+'.'+probe+'.'+engine+'.'+str(index)):
            raise ValueError('command provenance mismatch')
        if value['diagnostic'] and command == value['failure_command']:
            require_rejection(row,value['diagnostic'],engine)
            continue
        common.success(row)
        if command == 'query':
            common.require_query(row,value,engine)
            if item['name']=='native-profile' and probe.startswith('loaded-build'):
                require_metadata(row)
        if command != ('build' if engine == 'native' else 'cquery'):
            continue
        paths = common.direct.native_paths(row,target,Path(report['scratch_directory'])) if engine=='native' else reference_output_paths(row['stdout'],workspace,ref.output_base)
        if target == '//:result':
            entries = phase['outputs'][output_index:output_index+1]
            raws = [value['expected']]
            if engine == 'native':
                warm = output_index == 1 or item['name']=='lifecycle' and probe.endswith(('.repeat','.restore'))
                common.action_counts(row,(0,value['actions']) if warm else (value['actions'],0))
            output_index += 1
        else:
            entries, raws = phase['sources'], [BEFORE,AFTER] if value['repos']==2 else [value['expected']]
            source_index += len(entries)
        if list(map(str,paths))!=[e['path'] for e in entries] or len(entries)!=len(raws):
            raise ValueError('output not tied to command paths')
        for entry,raw in zip(entries,raws):
            if entry['kind']!='file' or not valid_evidence(entry,raw,True) or target=='//:result' and entry['mode']!=0o555:
                raise ValueError('output bytes or mode mismatch')
            if engine=='reference' and not Path(entry['path']).is_relative_to(ref.output_base.parent):
                raise ValueError('reference output escaped private root')
    if value['diagnostic']:
        if phase['outputs'] or phase['sources'] or engine=='native' and phase['publication_entries']:
            raise ValueError('rejection published outputs')
    elif output_index!=2 or len(phase['outputs'])!=2 or source_index!=value['repos']:
        raise ValueError('missing cold/warm/source outputs')


# Original immutable receipt provenance is informational; no runtime/unit read of
# ignored research artifacts. A fresh exact-fixture reference crash is mandatory.
DEFECT_PROVENANCE = dict(contract_sha256='8abc45b939b67a0fa4b6ceb80b5488b92ddee4b340634a75f7491e3b7ce91058',
    reference_sha256=REFERENCE_SHA256,source_commit=OFFICIAL_COMMIT,source_files=OFFICIAL,
    original_stderr_sha256='aa8da323598f2e7b00198931d1d6e57eba3de8927c6ddf3ef0dadcf3868b005f')
DEFECT_FRAMES = ('Caused by: java.lang.NullPointerException\n'
    '\tat com.google.common.base.Preconditions.checkNotNull(Preconditions.java:902)\n'
    '\tat com.google.devtools.build.lib.cmdline.BazelModuleContext.ofInnermostBzlOrThrow(BazelModuleContext.java:165)\n'
    '\tat com.google.devtools.build.lib.packages.LabelConverter.forBzlEvaluatingThread(LabelConverter.java:42)\n'
    '\tat com.google.devtools.build.lib.analysis.starlark.StarlarkAttrModule.createAttribute(StarlarkAttrModule.java:377)')


def defect_fixture():
    return {'MODULE.bazel':b'module(name="dict_contract")\n',
        'defs.bzl':b'ctor=attr.string_dict\n',
        'BUILD.bazel':b'load(":defs.bzl","ctor")\nprint("ALIAS_TYPE",type(ctor()))\nfilegroup(name="probe")\n'}


def require_defect(row, workspace):
    if (type(row['returncode']) is not int or row['returncode']!=37 or row['stdout']
            or any(row.get(k) for k in ('timed_out','spawn_error','interrupted','output_truncated'))
            or not row['stderr'].startswith('FATAL: bazel crashed due to an internal error. Printing stack trace:\n')
            or DEFECT_FRAMES not in row['stderr']
            or '\tat <starlark>.string_dict(<builtin>:0)\n' not in row['stderr']
            or '\tat <starlark>.<toplevel>('+str(workspace)+'/BUILD.bazel:2)\n' not in row['stderr']):
        raise ValueError('known reference crash differs from pinned diagnostic/site')


def argument_parser():
    parser = common.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT/'artifacts/string-dict-validation.json',scratch_parent=ROOT/'artifacts')
    parser.add_argument('--official-source',type=Path,default=Path('/tmp/opencode/bazel-9.3-reference'))
    return parser


def executable_evidence(args):
    return common.executable_evidence(args)


def report_passes(report):
    try:
        args = argument_parser().parse_args([])
        for k,v in report['configuration'].items():
            setattr(args,k,Path(v) if k in ('native','reference','output','scratch_parent','official_source') else v)
        if (report['planned_checks']!=CASES or report['errors'] or report['sources_before']!=report['sources_after']
                or set(report['sources_before'])!=set(map(str,SOURCES)) or report['executables']!=report['executables_after']
                or report['official_sources_before']!=OFFICIAL or report['official_sources_after']!=OFFICIAL
                or report['executables']['reference']['sha256']!=REFERENCE_SHA256
                or report['known_reference_defects']['provenance']!=DEFECT_PROVENANCE):
            return False
        engines = {'reference'} if args.reference_only else {'reference','native'}
        if set(report['executables'])!=engines or not args.reference_only and report['executables']['native']['sha256']!=args.native_sha256:
            return False
        scratch = Path(report['scratch_directory']); workspace = scratch/'workspace'
        if report['cleanup_receipt']!=dict(path=str(scratch),removed=True):
            return False
        cleanup = report['server_cleanup']
        if cleanup['forced'] or cleanup['state'] not in ('ABSENT','ZOMBIE'):
            return False
        runs = report['runs']; binding = digest(json.dumps(report['sources_before'],sort_keys=True).encode())
        if [r['id'] for r in runs]!=list(range(len(runs))):
            return False
        for row in runs:
            binary = next(v for v in report['executables'].values() if v['path']==row['argv'][0])
            if (row['executable_sha256']!=binary['sha256'] or row['preflight_binding_sha256']!=binding
                    or row['controlled_environment']!=controlled.encoded_environment(controlled.controlled_environment(scratch,{}))
                    or 'environment' in row or 'environment_keys' in row or not row['client_reaped']):
                return False
            for field in ('stdout','stderr'):
                raw = base64.b64decode(row[field+'_base64'],validate=True)
                if (digest(raw)!=row[field+'_sha256'] or digest(raw)!=row[field+'_full_sha256']
                        or len(raw)!=row[field+'_full_size_bytes'] or len(raw)>common.direct.MAX_STREAM
                        or raw.decode('utf-8',errors='replace')!=row[field]):
                    return False
        ref = DirectReference(args.reference,workspace,scratch,None,'string-dict')
        for row,name,argv,timeout in (
            (runs[0],'reference.version',ref.startup+['version','--gnu_format'],args.timeout),
            (runs[-1],'repository-environment.shutdown',ref.startup+['shutdown'],min(args.timeout,15))):
            common.success(row)
            if row['name']!=name or row['argv']!=argv or row['cwd']!=str(workspace) or row['timeout_seconds']!=timeout:
                return False
        if runs[0]['stdout'].strip()!='bazel 9.3.0' or cleanup['run']!=len(runs)-1:
            return False
        used = [0]
        if [(c['name'],c['kind']) for c in report['checks']]!=PLAN:
            return False
        for item,check in zip(CASES,report['checks']):
            pairs = phases_for(item,args.reference_only)
            if ([(p['probe'],p['engine']) for p in check['phases']]!=pairs
                    or check['status']!=('PASS' if pairs else 'SKIP') or check.get('error')):
                return False
            for phase in check['phases']:
                validate_phase(phase,report,args,workspace,ref,item); used.extend(phase['runs'])
        defect = report['known_reference_defects']; row = runs[defect['run']]
        require_defect(row,workspace)
        if (defect['classification']!='KNOWN_REFERENCE_DEFECT_NOT_PARITY' or defect['inputs_before']!=defect['inputs_after']
                or row['argv']!=command_argv(args,ref,'reference',scratch/'defect-cache',scratch/'defect-registry','query','//:probe')
                or row['cwd']!=str(workspace) or row['name']!='known-reference-defect.loaded-build-ctor'
                or row['timeout_seconds']!=args.timeout):
            return False
        planned = {str(workspace/p):raw for p,raw in defect_fixture().items()}
        planned.update({str(scratch/'defect-registry'/p.relative_to(REGISTRY)):p.read_bytes() for p in REGISTRY.rglob('*') if p.is_file()})
        if set(planned)!=set(defect['inputs_before']):
            return False
        if any(not valid_evidence(defect['inputs_before'][p],raw) for p,raw in planned.items()):
            return False
        used += [defect['run'],len(runs)-1]
        return used==list(range(len(runs)))
    except (KeyError,TypeError,ValueError,IndexError,AttributeError,OSError,StopIteration):
        return False


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for key in ('native','reference','output','scratch_parent','official_source'):
        # Preserve worktree-facing executable symlinks; evidence resolves bytes.
        setattr(args,key,getattr(args,key).absolute())
    if not args.output.is_relative_to(ROOT/'artifacts') or not args.scratch_parent.is_relative_to(ROOT/'artifacts'):
        raise ValueError('reports/scratch must remain in this worktree artifacts')
    args.scratch_parent.mkdir(parents=True,exist_ok=True)
    report = dict(schema_version=1,status='FAIL',runs=[],checks=[],errors=[],planned_checks=CASES,
        configuration={k:str(v) if isinstance(v,Path) else v for k,v in vars(args).items()},
        limitations=['PASS_ONLY_BOUNDED_PROFILE; not full Java diagnostics or application parity.',
            'Native scalar-shape rejection is bound to the invalid fixture; it does not reproduce Java key/value diagnostic detail.',
            'Explicit configurable values and repository/tag non-ASCII are native-only boundaries.',
            'BUILD loaded constructor/helper are native metadata profiles, not paired reference PASS.',
            'Known reference crash is mandatory and separate; never reproduce a JVM crash in native.',
            'run_shell.env projection UNEXECUTED; no source-derived action-env parity claim.',
            'select and symbolic-macro user attrs remain unsupported; no broad reflection parity.',
            'Host string spoofing, frozen-owner lifetime and payload budgets belong to independent core units.'])
    scratch = ref = runner = None
    try:
        report['sources_before'] = manifest(SOURCES)
        report['executables'] = executable_evidence(args)
        scratch = Path(tempfile.mkdtemp(prefix='string-dict-',dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        for name in ('workspace','home','tmp'):
            (scratch/name).mkdir()
        env = controlled.controlled_environment(scratch,{})
        report['official_sources_before'] = source_pins(args,env)
        runner = RecordingRunner(report,args.timeout,scratch); runner.args = args
        workspace = scratch/'workspace'
        (workspace/'MODULE.bazel').write_text('module(name="dict_contract")\n')
        ref = DirectReference(args.reference,workspace,scratch,runner,'string-dict')
        row = runner.run('reference.version',ref.startup+['version','--gnu_format'],workspace)
        if common.success(row).strip()!='bazel 9.3.0':
            raise ValueError('wrong genuine reference release')
        for item in CASES:
            check = dict(name=item['name'],kind=item['kind'],phases=[],status='FAIL'); report['checks'].append(check)
            for probe,engine in phases_for(item,args.reference_only):
                phase = run_phase(args,runner,scratch,workspace,ref,item,probe,engine)
                check['phases'].append(phase)
                validate_phase(phase,report,args,workspace,ref,item)
            check['status'] = 'PASS' if check['phases'] else 'SKIP'
        registry,paths = common.materialize(scratch/'defect',workspace,dict(files=defect_fixture()))
        registry = scratch/'defect-registry'; shutil.copytree(REGISTRY,registry)
        paths = [workspace/p for p in defect_fixture()]+sorted(p for p in registry.rglob('*') if p.is_file())
        defect = dict(classification='KNOWN_REFERENCE_DEFECT_NOT_PARITY',provenance=DEFECT_PROVENANCE,inputs_before=snapshot(paths))
        report['known_reference_defects'] = defect
        row = runner.run('known-reference-defect.loaded-build-ctor',command_argv(args,ref,'reference',scratch/'defect-cache',registry,'query','//:probe'),workspace)
        defect.update(run=row['id'],inputs_after=snapshot(paths))
        require_defect(row,workspace)
    except (Exception,KeyboardInterrupt) as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        if ref is not None:
            try:
                # Cleanup must run even if the source-binding preflight stopped QA.
                cleanup_runner = controlled.RecordingRunner(report,args.timeout,scratch)
                controlled.shutdown(ref,cleanup_runner,report)
                report['runs'][-1]['preflight_binding_sha256'] = digest(json.dumps(report['sources_before'],sort_keys=True).encode())
            except Exception as exc:
                report['errors'].append('cleanup: '+str(exc))
        if scratch is not None:
            try:
                report['official_sources_after'] = source_pins(args,controlled.controlled_environment(scratch,{}))
                shutil.rmtree(scratch)
                report['cleanup_receipt'] = dict(path=str(scratch),removed=not scratch.exists())
            except Exception as exc:
                report['errors'].append('scratch cleanup: '+str(exc))
        try:
            report['sources_after'] = manifest(SOURCES); report['executables_after'] = executable_evidence(args)
        except Exception as exc:
            report['errors'].append('final evidence: '+str(exc))
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PREPARATION_PASS' if args.reference_only else 'PASS_ONLY_BOUNDED_PROFILE'
        write_report(args.output,report)
    print(report['status']+': '+str(args.output))
    return 0 if report['status']!='FAIL' else 1


if __name__=='__main__':
    def terminate(signum,frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM,terminate)
    sys.exit(main())
