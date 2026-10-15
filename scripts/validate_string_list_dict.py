#!/usr/bin/env python3
"""Real CLI string_list_dict contract checks, not application/Bazel-wide parity.

Reference-only preparation never runs the native executable or claims parity.
Full execution requires an explicitly frozen native SHA256. No namespace loader,
fake builtins, altered upstream fixtures, or fallback execution is used.
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

from validate import ROOT, COMMON_FLAGS, file_evidence, reference_output_paths, validate_executable, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256, validate_binaries
import validate_repository_rules as direct
import validate_module_extensions as extensions


SOURCES = [ROOT / p for p in ('scripts/validate_string_list_dict.py',
    'tests/test_validate_string_list_dict.py', 'scripts/validate.py',
    'scripts/validate_direct_repositories.py', 'scripts/validate_repository_rules.py',
    'scripts/validate_module_extensions.py')]
quote, digest = direct.quote, direct.digest
evidence, snapshot, valid_evidence = direct.evidence, direct.snapshot, direct.valid_evidence
success = extensions.success
ROOT_MODULE = 'module(name="dict_contract")\n'
INITIAL = '{"z":["before","before"],"a":[]}'
BEFORE = b'dict\nz:list:before,before\na:list:\n'
AFTER = b'dict\nz:list:before,before,after\na:list:\nnew:list:new\n'
EMPTY = b'dict\n'
SERIALIZE = ('def render(d):\n    return type(d)+"\\n"+"".join('
             '[k+":"+type(d[k])+":"+",".join(d[k])+"\\n" for k in d.keys()])\n')
MUTATE = 'D["z"].append("after")\nD["new"]=["new"]\n'


def case(name, probes, kind='differential'):
    return dict(name=name, probes=list(probes), kind=kind)


BAD_VALUES = {'key-int': '{1:["x"]}', 'value-string': '{"z":"x"}',
    'element-int': '{"z":[1]}', 'nested-list': '{"z":[["x"]]}',
    'key-label': '{Label("//:input"):["x"]}', 'value-label': '{"z":[Label("//:input")]}',
    'element-none': '{"z":[None]}', 'value-none': '{"z":None}', 'not-dict': '["x"]'}
BAD_CTORS = {'default-none': 'default=None', 'default-callable': 'default=render',
    'doc-int': 'doc=1', 'allow-int': 'allow_empty=1', 'mandatory-int': 'mandatory=1',
    'two-positional': 'True,{}'}
CASES = [
    case('custom', ('snapshots', 'tuples', 'sequence-tuples', 'none-default', 'mandatory-empty')),
    case('direct', ('module-late', 'tuples', 'none-default', 'mandatory-empty')),
    case('ordinary', ('per-call',)),
    case('tags', ('module-late', 'tuples', 'none-default', 'mandatory-empty', 'empty-nonempty')),
    case('provenance', ('direct-first', 'direct-repeat', 'direct-reorder', 'direct-restore',
                        'ordinary-first', 'ordinary-repeat', 'ordinary-reorder', 'ordinary-restore')),
    case('typing', tuple('custom.'+p for p in BAD_VALUES) + tuple('direct.'+p for p in BAD_VALUES)
         + tuple('ctor.'+p for p in BAD_CTORS)
         + tuple(r+'.mandatory-'+p for r in ('custom', 'direct', 'ordinary', 'tags') for p in ('missing', 'none'))),
    case('controls', tuple(r+'.'+p for r in ('custom', 'direct', 'tags')
         for p in ('freeze-dict', 'freeze-list', 'empty-explicit', 'empty-default', 'nested-empty'))),
    case('native-boundary', ('configurable', 'configurable-none', 'direct.ascii-key', 'direct.ascii-element',
         'tags.ascii-key', 'tags.ascii-element'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASES]
UNRELATED = re.compile(extensions.UNRELATED.pattern + r'|\bnetwork\b|connection refused|timed? out|'
    r'no space left|out of memory|killed by|not found in.*registry', re.I)
DIAGNOSTICS = {
    'key': r'(?:expected[^\n]*string[^\n]*dict key element|(?:attribute [\'\"]d[\'\"]|string_list_dict)[^\n]*(?:key[^\n]*string|string[^\n]*key))',
    'value': r'(?:expected[^\n]*list\(string\)[^\n]*dict value element|(?:attribute [\'\"]d[\'\"]|string_list_dict)[^\n]*(?:list|tuple|sequence))',
    'element': r'(?:expected[^\n]*string[^\n]*element \d+ of dict value element|(?:attribute [\'\"]d[\'\"]|string_list_dict)[^\n]*(?:element|list|value)[^\n]*string)',
    'dict': r'(?:attribute [\'\"]d[\'\"]|string_list_dict)[^\n]*(?:dict|dictionary)|expected[^\n]*dict[^\n]*attribute [\'\"]d[\'\"]',
    'mandatory': r'mandatory attribute [\'\"]d[\'\"][^\n]*(?:missing|specified|required)|attribute [\'\"]d[\'\"][^\n]*(?:mandatory|required|missing)|missing value for mandatory attribute [\'\"]d[\'\"]',
    'empty': r'(?:attribute [\'\"]d[\'\"]|[\'\"]d[\'\"] attribute|in d attribute of)[^\n]*(?:empty|non.empty)|(?:empty|non.empty)[^\n]*attribute [\'\"]d[\'\"]',
    'freeze-dict': r'(?:trying to mutate a frozen dict value|Cannot mutate frozen (?:dict|Dict|FrozenDict)|FrozenDict[^\n]*(?:mutat|immutable))',
    'freeze-list': r'(?:trying to mutate a frozen list value|Cannot mutate frozen (?:list|List|FrozenList)|FrozenList[^\n]*(?:mutat|immutable))',
    'default-none': r'(?:parameter [\'\"]default[\'\"]|default)[^\n]*(?:NoneType|None)[^\n]*dict',
    'default-callable': r'(?:parameter [\'\"]default[\'\"]|default)[^\n]*(?:function|callable)[^\n]*dict',
    'doc-int': r'(?:parameter [\'\"]doc[\'\"]|\bdoc\b)[^\n]*(?:string|str|None)',
    'allow-int': r'\ballow_empty\b[^\n]*bool',
    'mandatory-int': r'\bmandatory\b[^\n]*bool',
    'two-positional': r'string_list_dict[^\n]*(?:positional|argument)|(?:too many|unexpected)[^\n]*positional',
    'configurable': r'(?:string_list_dict[^\n]*)?configurable[^\n]*(?:unsupported|not supported)',
    'ascii': r'(?:non.ASCII|ASCII.only|must (?:be|contain)[^\n]*ASCII|only ASCII)',
}
NATIVE_SHAPE = r'(?m:^error: (?:attribute "d": )?expected a dict of strings to a list of strings \(or tuple of strings\)\s*$)'
NATIVE_DIAGNOSTICS = {
    'key': NATIVE_SHAPE+r'|cannot record MODULE attributes[^\n]*: tag dictionary key must be a string',
    'value': NATIVE_SHAPE,
    'element': NATIVE_SHAPE,
    'dict': NATIVE_SHAPE,
    'default-none': NATIVE_SHAPE,
    'default-callable': NATIVE_SHAPE,
    'mandatory': r'missing mandatory (?:(?:repository|tag) )?attribute "d"',
    # RuleErrorConsumer.completeAttributeMessage form (in d attribute of r rule //:x: ...).
    'empty': r'(?:attribute d: attribute must be non empty|in d attribute of [^\n]*: attribute must be non empty)',
    'two-positional': r'Found 1 extra positional argument\(s\) for call to string_list_dict',
    'freeze-dict': r'(?m:^error: Immutable\s*$)',
    'freeze-list': r'(?m:^error: Immutable\s*$)',
}


def require_rejection(record, diagnostic, engine='reference'):
    code, text = record['returncode'], record['stderr']
    matched = re.search(DIAGNOSTICS[diagnostic], text, re.I)
    if engine == 'native' and diagnostic in NATIVE_DIAGNOSTICS:
        matched = matched or re.search(NATIVE_DIAGNOSTICS[diagnostic], text, re.I)
        if re.search(NATIVE_SHAPE, text):
            context = ('attr.string_list_dict(default=None)' if diagnostic == 'default-none'
                else 'attr.string_list_dict(default=render)' if diagnostic == 'default-callable' else None)
            matched = matched and (context in text if context else
                ('attribute "d": expected' in text or 'A=attr.string_list_dict(default=' in text))
    if (type(code) is not int or not 0 < code < 128 or code in (101, 124, 125, 126, 127)
            or record['timed_out'] or record.get('spawn_error') or record.get('interrupted')
            or record.get('output_truncated') or record['stdout'].strip() or UNRELATED.search(text)
            or not matched):
        raise ValueError('expected controlled string_list_dict rejection: '+diagnostic)


def fixture(name, probe):
    item = next((c for c in CASES if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    route = name if name in ('custom', 'direct', 'ordinary', 'tags') else probe.split('.')[0]
    ctor, argument, expected = 'attr.string_list_dict(default=D,doc=None)', 'd=D', BEFORE
    prefix = 'D='+INITIAL+'\n'
    after_ctor, call_before, call_after, mutation = '', '', '', ''
    diagnostic, failure_command, two = None, 'query', False
    if name == 'provenance':
        route, stage = probe.split('-')
        prefix = 'D={"a":["2"],"z":["1"]}\n' if stage == 'reorder' else 'D={"z":["1"],"a":["2"]}\n'
        expected = b'dict\na:list:2\nz:list:1\n' if stage == 'reorder' else b'dict\nz:list:1\na:list:2\n'
        # Keep callback/default source identical; only call input order changes.
        ctor = 'attr.string_list_dict()'
    if probe == 'snapshots':
        after_ctor, argument, two = MUTATE, '', True
        # The second instance snapshots at BUILD macro invocation, not MODULE end.
        expected = BEFORE+BEFORE
    elif probe == 'per-call':
        two, expected = True, BEFORE+AFTER
    elif probe == 'module-late':
        call_after, expected = MUTATE, AFTER
    elif probe == 'tuples':
        prefix = 'D={"z":("before","before"),"a":()}\n'
    elif probe == 'none-default':
        argument = 'd=None'
    elif probe == 'mandatory-empty':
        ctor, argument, expected = 'attr.string_list_dict(mandatory=True,default=D)', 'd={}', EMPTY
    elif probe == 'empty-nonempty':
        ctor, argument, expected = 'attr.string_list_dict(False)', 'd={}', EMPTY
    if name == 'typing':
        route, bad = probe.split('.')
        if route == 'ctor':
            route, ctor, diagnostic = 'direct', 'attr.string_list_dict('+BAD_CTORS[bad]+')', bad
        elif bad.startswith('mandatory-'):
            ctor = 'attr.string_list_dict(mandatory=True,default=D)'
            argument = '' if bad == 'mandatory-missing' else 'd=None'
            diagnostic = 'mandatory'
        else:
            argument = 'd='+BAD_VALUES[bad]
            diagnostic = ('key' if bad.startswith('key-') else 'value' if bad in ('value-string', 'value-none')
                          else 'dict' if bad == 'not-dict' else 'element')
            if 'label' in bad:
                # Label is a .bzl builtin, not a BUILD/MODULE builtin. Exercise
                # real typed constructor conversion rather than undefined names.
                ctor, argument = 'attr.string_list_dict(default='+BAD_VALUES[bad]+')', ''
    elif name == 'controls':
        route, control = probe.split('.')
        if control.startswith('freeze-'):
            mutation = 'd["new"]=["bad"]' if control == 'freeze-dict' else 'd["z"].append("bad")'
            diagnostic = control
            failure_command = 'build' if route == 'custom' else 'query'
        else:
            ctor = 'attr.string_list_dict(False)'
            argument = '' if control == 'empty-default' else 'd={"z":[]}' if control == 'nested-empty' else 'd={}'
            expected = b'dict\nz:list:\n' if control == 'nested-empty' else EMPTY
            if route == 'custom' and control != 'nested-empty':
                diagnostic, failure_command = 'empty', 'build'
    elif name == 'native-boundary':
        if probe in ('configurable', 'configurable-none'):
            route, ctor, diagnostic = 'direct', 'attr.string_list_dict(configurable='+('None' if probe.endswith('-none') else 'False')+')', 'configurable'
        else:
            route, bad = probe.split('.')
            argument = 'd={"é":["x"]}' if bad == 'ascii-key' else 'd={"z":["é"]}'
            diagnostic = 'ascii'
    files = {'MODULE.bazel': ROOT_MODULE, 'input': 'input\n'}
    args = (','+argument) if argument else ''
    attrs = '{"d":A}'
    definitions = SERIALIZE+prefix+'A='+ctor+'\n'+after_ctor
    if route == 'custom':
        if probe == 'sequence-tuples':
            attrs = ('{"d":A,"words":attr.string_list(default=("x","y")),'
                     '"inputs":attr.label_list(allow_files=True,default=(":input",)),'
                     '"outs":attr.output_list(),"setting":attr.label(default=":setting")}')
            definitions += ('Info=provider(fields=["value"])\n'
                'def _setting(ctx):\n    return [Info(value=ctx.build_setting_value)]\n'
                'setting=rule(implementation=_setting,build_setting=config.string_list())\n')
            args += ',outs=("extra.txt",)'
        body = ('d=ctx.attr.d\n'+(mutation+'\n' if mutation else '')
                +'out=ctx.actions.declare_file(ctx.label.name+".txt")\n'
                'ctx.actions.write(out,render(d))\nreturn [DefaultInfo(files=depset([out]))]')
        if probe == 'sequence-tuples':
            body = ('d=ctx.attr.d\n'
                'out=ctx.actions.declare_file(ctx.label.name+".txt")\n'
                'extra=ctx.outputs.outs[0]\n'
                'ctx.actions.write(extra,"extra\\n")\n'
                'ctx.actions.write(out,render(d)+type(ctx.attr.words)+":"+",".join(ctx.attr.words)+"\\n"'
                '+type(ctx.attr.inputs)+":"+str(len(ctx.attr.inputs))+"\\n"'
                '+type(ctx.outputs.outs)+":"+str(len(ctx.outputs.outs))+"\\n"'
                '+type(ctx.attr.setting[Info].value)+":"+",".join(ctx.attr.setting[Info].value)+"\\n")\n'
                'return [DefaultInfo(files=depset([out,extra]))]')
            expected = BEFORE+b'list:x,y\nlist:1\nlist:1\nlist:x,y\nextra\n'
        definitions += 'def _impl(ctx):\n'+indent(body)+'r=rule(implementation=_impl,attrs='+attrs+')\n'
        if probe == 'snapshots':
            definitions += ('def targets():\n    r(name="one")\n    D='+INITIAL+'\n'
                '    r(name="two",d=D)\n'+indent(MUTATE))
            build = 'load(":defs.bzl","targets")\ntargets()\n'
        else:
            build = 'load(":defs.bzl","r")\nD='+INITIAL+'\n'
            if probe == 'tuples':
                build = 'load(":defs.bzl","r")\n'+prefix
            build += 'r(name="one"'+args+')\n'
        if probe == 'sequence-tuples':
            build = build.replace('load(":defs.bzl","r")', 'load(":defs.bzl","r","setting")')
            build += 'setting(name="setting",build_setting_default=("x","y"))\n'
        labels = ['//:one']+(['//:two'] if two else [])
        query = '//:one'
    else:
        body = 'd=ctx.attr.d\n'+(mutation+'\n' if mutation and route != 'tags' else '')
        body += ('ctx.file("data",render(d),executable=True)\n'
                 'ctx.file("BUILD.bazel",\'exports_files(["data"],visibility=["//visibility:public"])\\n\',executable=False)')
        definitions += 'def _impl(ctx):\n'+indent(body)+'r=repository_rule(implementation=_impl,attrs='+attrs+')\n'
        if route == 'direct':
            files['MODULE.bazel'] += 'r=use_repo_rule("//:defs.bzl","r")\n'+prefix+call_before+'r(name="one"'+args+')\n'+call_after
        else:
            files['MODULE.bazel'] += 'e=use_extension("//:ext.bzl","e")\nuse_repo(e,"one"'+(',"two"' if two else '')+')\n'
            ext = 'load(":defs.bzl","r")\n'
            if route == 'tags':
                ext += SERIALIZE+prefix+'A='+ctor+'\n'
                extbody = ('for m in ctx.modules:\n    for t in m.tags.item:\n        d=t.d\n'
                    + ('        '+mutation+'\n' if mutation else '')+'        r(name="one",d=d)')
                ext += 'def _ext(ctx):\n'+indent(extbody)+'e=module_extension(implementation=_ext,tag_classes={"item":tag_class(attrs={"d":A})})\n'
                files['MODULE.bazel'] += prefix+'e.item('+argument+')\n'+call_after
            else:
                extbody = prefix+'r(name="one"'+args+')\n'
                if two:
                    extbody += MUTATE+'r(name="two",d=D)\n'
                ext += 'def _ext(ctx):\n'+indent(extbody)+'e=module_extension(implementation=_ext)\n'
            files['ext.bzl'] = ext
        labels = ['@one//:data']+(['@two//:data'] if two else [])
        query, build = '@one//:data', ''
        if name == 'provenance':
            # Defaults cannot accidentally account for the order identity test.
            definitions = definitions.replace(prefix, 'D='+INITIAL+'\n', 1)
    files['defs.bzl'] = definitions
    files['BUILD.bazel'] = build+('filegroup(name="sources",srcs='+repr(labels)+')\n'
        'genrule(name="result",srcs='+repr(labels)+',outs=["result.txt"],cmd="cat $(SRCS) > $@")\n')
    return dict(files={p: s.encode() for p, s in files.items()}, expected=expected, route=route,
        diagnostic=diagnostic, failure_command=failure_command, query=query,
        repos=0 if route == 'custom' else 2 if two else 1,
        actions=3 if route == 'custom' and (two or probe == 'sequence-tuples') else 2 if route == 'custom' else 1)


def indent(text):
    return ''.join('    '+line+'\n' for line in text.splitlines())


def phases_for(item, reference_only=False):
    engines = ('reference',) if reference_only else ('reference', 'native')
    if item['kind'] == 'native_boundary':
        engines = () if reference_only else ('native',)
    return [(p, e) for p in item['probes'] for e in engines]


def steps_for(item, probe, engine):
    value = fixture(item['name'], probe)
    steps = [('query', value['query'], [])]
    if value['diagnostic']:
        return steps+([('build', '//:result', [])] if value['failure_command'] == 'build' else [])
    # Build the consumer first so custom cold action counts include its inputs.
    for _ in range(2):
        steps.append(('build', '//:result', []))
        if engine == 'reference':
            steps.append(('cquery', '//:result', ['--output=files']))
    if value['repos']:
        steps.append(('build', '//:sources', []))
        if engine == 'reference':
            steps.append(('cquery', '//:sources', ['--output=files']))
    return steps


def require_query(record, value, engine):
    expected = value['query']
    if engine == 'native' and value['repos']:
        expected = '@@+'+('r' if value['route'] == 'direct' else 'e')+'+one//:data'
    if success(record).splitlines() != [expected]:
        raise ValueError('query did not return exact planned target identity')


def materialize(root, workspace, value, update=False):
    if not update and workspace.exists():
        # Keep the workspace inode alive: deleting it stops the genuine server.
        for path in workspace.iterdir():
            if path.is_symlink() or path.is_file():
                path.unlink()
            else:
                shutil.rmtree(path)
    workspace.mkdir(parents=True, exist_ok=True)
    owned = {workspace / p: raw for p, raw in value['files'].items()}
    registry = root / 'registry'
    owned[registry / 'bazel_registry.json'] = b'{}\n'
    for path, raw in owned.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
    return registry, list(owned)


def action_counts(record, expected):
    found = re.findall(r'(\d+) actions executed, (\d+) cache hits', record['stderr'])
    if found != [tuple(map(str, expected))]:
        raise ValueError('wrong exact cold/warm native action counts')


def phase_reference(reference, item, probe, root):
    if item['name'] != 'provenance':
        return reference
    # Bazel's warm repository equality/cache keys disregard map insertion order.
    # Each genuine reference stage is therefore a fresh batch build with a fresh
    # contents cache; native stages still share one cache to test invalidation.
    # Sharing only the genuine installation directory does not fabricate builtins.
    isolated = DirectReference(Path(reference.startup[0]), reference.workspace,
        root / ('reference-'+probe), reference.runner, 'string-list-dict')
    isolated.startup.insert(1, '--batch')
    isolated.startup = [a if not a.startswith('--output_user_root=') else
        '--output_user_root='+str(reference.output_user_root) for a in isolated.startup]
    return isolated


def run_phase(args, runner, scratch, workspace, reference, item, probe, engine, root, update):
    if engine == 'reference':
        reference = phase_reference(reference, item, probe, root)
    value = fixture(item['name'], probe)
    registry, paths = materialize(root, workspace, value, update)
    cache = root / 'native-cache'
    phase = dict(probe=probe, engine=engine, cache=str(cache), registry=str(registry),
                 runs=[], outputs=[], sources=[], status='FAIL', inputs_before=snapshot(paths))
    try:
        for index, (command, target, extra) in enumerate(steps_for(item, probe, engine)):
            argv = extensions.command_argv(args, reference, engine, cache, registry, command, target, extra)
            record = runner.run(item['name']+'.'+probe+'.'+engine+'.'+str(index), argv, workspace)
            phase['runs'].append(record['id'])
            if value['diagnostic'] and command == value['failure_command']:
                require_rejection(record, value['diagnostic'], engine)
                if engine == 'native':
                    phase['publication_entries'] = direct.no_publication(cache)
                break
            success(record)
            if command == 'query':
                require_query(record, value, engine)
            output_step = command == ('build' if engine == 'native' else 'cquery')
            if output_step:
                output_paths = (direct.native_paths(record, target, scratch) if engine == 'native'
                    else reference_output_paths(record['stdout'], workspace, reference.output_base))
                if target == '//:result':
                    if len(output_paths) != 1 or output_paths[0].name != 'result.txt':
                        raise ValueError('expected one actual consumer output')
                    entry = evidence(output_paths[0])
                    phase['outputs'].append(entry)
                    if not valid_evidence(entry, value['expected'], True) or entry['mode'] != 0o555:
                        raise ValueError('consumer bytes differ from fixed expectation: '+repr(base64.b64decode(entry['bytes_base64'])))
                    if engine == 'native':
                        warm = len(phase['outputs']) == 2 or (item['name'] == 'provenance' and probe.endswith(('-repeat', '-restore')))
                        action_counts(record, (0, value['actions']) if warm else (value['actions'], 0))
                else:
                    for path in output_paths:
                        resolved = path.resolve(strict=True)
                        entry = evidence(resolved)
                        phase['sources'].append(entry)
                        entry['logical_path'] = str(path)
                        if engine == 'native':
                            tree = direct.publication_root(path, cache)
                            entry['tree'] = str(tree)
                            entry['repo_boundary'] = evidence(tree / 'REPO.bazel')
                            entry['module_absent'] = not (tree / 'MODULE.bazel').exists()
        if engine == 'native' and not value['diagnostic']:
            phase['publication_entries'] = sorted(p.name for p in direct.trees_directory(cache).glob('*'))
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    finally:
        phase['inputs_after'] = snapshot(paths)
        if phase['inputs_before'] != phase['inputs_after']:
            phase.update(status='FAIL', error='CLI mutated source inputs')
    return phase


def validate_phase(phase, report, args, workspace, reference, item):
    probe, engine = phase['probe'], phase['engine']
    if engine == 'reference':
        reference = phase_reference(reference, item, probe, Path(phase['cache']).parent)
    value = fixture(item['name'], probe)
    if phase['status'] != 'PASS' or phase.get('error') or phase['inputs_before'] != phase['inputs_after']:
        raise ValueError('failed phase or changed source inputs')
    planned = {str(workspace / p): raw for p, raw in value['files'].items()}
    planned[str(Path(phase['registry']) / 'bazel_registry.json')] = b'{}\n'
    if set(planned) != set(phase['inputs_before']):
        raise ValueError('missing or unplanned source inputs')
    for path, raw in planned.items():
        entry = phase['inputs_before'][path]
        if entry['path'] != path or entry['kind'] != 'file' or not valid_evidence(entry, raw):
            raise ValueError('source differs from immutable fixture plan')
    steps = steps_for(item, probe, engine)
    if len(phase['runs']) != len(steps):
        raise ValueError('missing or extra command')
    records = []
    for index, (rid, (command, target, extra)) in enumerate(zip(phase['runs'], steps)):
        record = report['runs'][rid]
        argv = extensions.command_argv(args, reference, engine, Path(phase['cache']), Path(phase['registry']), command, target, extra)
        if (record['argv'] != argv or record['cwd'] != str(workspace) or record['timeout_seconds'] != args.timeout
                or record['name'] != item['name']+'.'+probe+'.'+engine+'.'+str(index)):
            raise ValueError('command provenance mismatch')
        records.append(record)
        if value['diagnostic'] and command == value['failure_command']:
            require_rejection(record, value['diagnostic'], engine)
        else:
            success(record)
            if command == 'query':
                require_query(record, value, engine)
    if value['diagnostic']:
        if phase['outputs'] or phase['sources'] or (engine == 'native' and phase['publication_entries']):
            raise ValueError('rejected phase published output')
        return
    output_records = [(record, target) for record, (cmd, target, _) in zip(records, steps)
                      if cmd == ('build' if engine == 'native' else 'cquery')]
    listing = []
    for record, target in output_records:
        paths = (direct.native_paths(record, target, Path(report['scratch_directory'])) if engine == 'native'
                 else reference_output_paths(record['stdout'], workspace, reference.output_base))
        listing.append((record, target, paths))
    if len(phase['outputs']) != 2:
        raise ValueError('missing cold/warm outputs')
    for index, entry in enumerate(phase['outputs']):
        record, target, paths = listing[index]
        if (target != '//:result' or list(map(str, paths)) != [entry['path']]
                or Path(entry['path']).name != 'result.txt' or entry['kind'] != 'file'
                or not valid_evidence(entry, value['expected'], True) or entry['mode'] != 0o555):
            raise ValueError('consumer output is not tied to genuine command bytes')
        if engine == 'native':
            warm = index == 1 or (item['name'] == 'provenance' and probe.endswith(('-repeat', '-restore')))
            action_counts(record, (0, value['actions']) if warm else (value['actions'], 0))
    if len(phase['sources']) != value['repos']:
        raise ValueError('missing repository source outputs')
    if value['repos']:
        paths = listing[-1][2]
        if sorted(map(str, paths)) != sorted(e['logical_path'] for e in phase['sources']):
            raise ValueError('repository evidence not linked to build output')
        raws = [BEFORE, AFTER] if item['name'] == 'ordinary' else [value['expected']]
        for entry, raw in zip(phase['sources'], raws):
            if entry['kind'] != 'file' or not valid_evidence(entry, raw, True):
                raise ValueError('repository source bytes or mode mismatch')
            if engine == 'native':
                tree = Path(entry['tree'])
                if (tree.parent != direct.trees_directory(Path(phase['cache']))
                        or not re.fullmatch('[0-9a-f]{64}', tree.name) or Path(entry['path']) != tree / 'data'
                        or entry['module_absent'] is not True
                        or entry['repo_boundary']['path'] != str(tree / 'REPO.bazel')
                        or not valid_evidence(entry['repo_boundary'], b'')):
                    raise ValueError('repository publication boundary mismatch')
            elif not Path(entry['path']).is_relative_to(reference.output_base.parent):
                raise ValueError('reference source escaped private output root')
    if engine == 'native':
        count = value['repos'] * (2 if item['name'] == 'provenance' and probe.endswith(('-reorder', '-restore')) else 1)
        entries = phase['publication_entries']
        if len(entries) != count or len(set(entries)) != count or any(not re.fullmatch('[0-9a-f]{64}', e) for e in entries):
            raise ValueError('unexpected publication/staging tree count')


def validate_check(check, report, args, workspace, reference):
    item = next(c for c in CASES if c['name'] == check['name'])
    expected = phases_for(item, args.reference_only)
    if (check['kind'] != item['kind'] or [(p['probe'], p['engine']) for p in check['phases']] != expected
            or check.get('error') or check['status'] != ('SKIP' if not expected else 'PASS')):
        raise ValueError('missing, reordered, failed or unexpected phases')
    for phase in check['phases']:
        validate_phase(phase, report, args, workspace, reference, item)
    if item['name'] == 'provenance' and not args.reference_only:
        for route in ('direct', 'ordinary'):
            phases = [p for p in check['phases'] if p['engine'] == 'native' and p['probe'].startswith(route+'-')]
            trees = [p['sources'][0]['tree'] for p in phases]
            if not (trees[0] == trees[1] == trees[3] and trees[2] != trees[0]):
                raise ValueError('ordered attributes did not roundtrip repository identity')
            if len({p['cache'] for p in phases}) != 1:
                raise ValueError('provenance stages did not reuse a cache')


def report_passes(report):
    try:
        args = argument_parser().parse_args([])
        for key, value in report['configuration'].items():
            setattr(args, key, Path(value) if key in ('native', 'reference', 'output', 'scratch_parent') else value)
        if (report['planned_checks'] != CASES or [(c['name'], c['kind']) for c in report['checks']] != PLAN
                or report['plan_sha256'] != digest(json.dumps(CASES, sort_keys=True).encode())
                or report['errors'] or report['cleanup'] or not report['scratch_cleaned']
                or not report['reference_shutdown_succeeded'] or report['sources_before'] != report['sources_after']
                or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(p == e['path'] and valid_evidence(e) for p, e in report['sources_before'].items())
                or report['executables'] != report['executables_after']
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['reference_release'] != 'bazel 9.3.0'):
            return False
        if (report['completed_phases'] != sum(len(phases_for(c, args.reference_only)) for c in CASES)
                or report['completed_checks'] != sum(bool(phases_for(c, args.reference_only)) for c in CASES)):
            return False
        engines = ('reference',) if args.reference_only else ('reference', 'native')
        if set(report['executables']) != set(engines):
            return False
        if not args.reference_only and report['executables']['native']['sha256'] != args.native_sha256:
            return False
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        if report['cleanup_receipt'] != dict(path=str(scratch), removed=True):
            return False
        reference = DirectReference(args.reference, workspace, scratch, None, 'string-list-dict')
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for r in runs:
            engine = next((e for e in engines if str(getattr(args, e)) == r['argv'][0]), None)
            if (engine is None or r['executable_sha256'] != report['executables'][engine]['sha256']
                    or not r['environment_keys'] or not r['environment']):
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(r[field+'_base64'], validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != r[field]):
                    return False
        for record, name, argv, timeout in (
                (runs[0], 'reference.version', reference.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], 'string-list-dict.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS), min(args.timeout, 15))):
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


def argument_parser():
    parser = direct.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/string-list-dict-validation.json', scratch_parent=Path('/tmp/opencode'))
    parser.add_argument('--reference-only', action='store_true', help='Never execute native; cannot produce a parity PASS')
    parser.add_argument('--native-sha256', help='Required frozen candidate hash for native execution')
    return parser


def executable_evidence(args):
    if args.reference_only:
        validate_executable(args.reference, official=True)
        result = {'reference': file_evidence(args.reference)}
        if result['reference']['sha256'] != REFERENCE_SHA256:
            raise ValueError('reference SHA256 is not pinned Bazel 9.3.0')
        return result
    if not args.native_sha256 or not re.fullmatch('[0-9a-f]{64}', args.native_sha256):
        raise ValueError('--native-sha256 frozen candidate is required (no baseline/fallback)')
    result = validate_binaries(args)
    if result['native']['sha256'] != args.native_sha256:
        raise ValueError('native executable differs from frozen candidate')
    return result


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.output in [args.native, args.reference, *SOURCES] or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests', 'src')):
        raise ValueError('report cannot overwrite source or executable')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
        planned_checks=CASES, plan_sha256=digest(json.dumps(CASES, sort_keys=True).encode()),
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        scratch_cleaned=False, reference_shutdown_succeeded=False,
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        limitations=['Bounded string_list_dict contract, not application or full Bazel parity.',
            'Explicit configurable and repository/MODULE ASCII restrictions are native-only boundaries.',
            'Reference is genuine pinned Bazel; its real builtin closure may download.',
            'Custom ctx.actions.write and genrule consumers execute; no test/run or user workspace.',
            'Reference-only preparation cannot claim native parity.',
            'Provenance reference stages use isolated batch contents caches: upstream warm dictionary equality ignores order.',
            'Repository executable bits checked; repository writable modes are not equated with read-only action-cache modes.'])
    scratch = reference = None
    runner = extensions.RecordingRunner(report, args.timeout)
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = executable_evidence(args)
        scratch = Path(tempfile.mkdtemp(prefix='string-list-dict-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(ROOT_MODULE)
        reference = DirectReference(args.reference, workspace, scratch, runner, 'string-list-dict')
        reference.started = True
        report['reference_release'] = success(runner.run('reference.version', reference.startup+['version', '--gnu_format'], workspace)).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('wrong genuine reference release')
        for item in CASES:
            check = dict(name=item['name'], kind=item['kind'], status='FAIL', phases=[])
            report['checks'].append(check)
            for probe, engine in phases_for(item, args.reference_only):
                group = probe.split('-')[0] if item['name'] == 'provenance' else probe
                root = scratch / 'cases' / item['name'] / group
                update = item['name'] == 'provenance' and not probe.endswith('-first')
                check['phases'].append(run_phase(args, runner, scratch, workspace, reference, item, probe, engine, root, update))
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
                shutil.rmtree(scratch)
                report['scratch_cleaned'] = not scratch.exists()
                report['cleanup_receipt'] = dict(path=str(scratch), removed=report['scratch_cleaned'])
            except OSError as exc:
                report['errors'].append('cleanup: '+str(exc))
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['executables_after'] = executable_evidence(args)
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
