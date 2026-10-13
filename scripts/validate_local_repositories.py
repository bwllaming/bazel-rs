#!/usr/bin/env python3
"""Bounded local repository/version QA, not full bazel_features or application parity.

The genuine archive is never rewritten. Full generated files are consumed as
label-backed templates, not by evaluating its unsupported bzl_library BUILD.
Native callbacks run on each fresh CLI invocation; genuine persistent-server
callback frequency is intentionally NOT equated with this serverless contract.
"""

import ast
import base64
import datetime
import json
from pathlib import Path
import re
import shutil
import signal
import sys
import tarfile
import tempfile

import validate_string_list_dict as common
from validate import ROOT, COMMON_FLAGS, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256
import validate_repository_rules as direct
import validate_module_extensions as extensions


ARCHIVE = ROOT / 'tests/fixtures/pax_comments/bazel_features-1.42.1.tar.gz'
ARCHIVE_SHA256 = '8189bac9a6bf9cc155a854c4cbebfebf58b9ca7a2d0a67645f7d0c1f83c523ac'
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
SOURCES = list(dict.fromkeys([ROOT / 'scripts/validate_local_repositories.py',
    ROOT / 'tests/test_validate_local_repositories.py', ARCHIVE, *common.SOURCES,
    *sorted(p for p in REGISTRY.rglob('*') if p.is_file())]))
digest, evidence, snapshot, valid_evidence = direct.digest, direct.evidence, direct.snapshot, direct.valid_evidence
success = extensions.success
CASEPLAN = [
    common.case('lifecycle', tuple(r+'.'+v for r in ('direct', 'ordinary') for v in ('default', 'false', 'true'))),
    common.case('phase', ('shared-transitive', 'build-native-absent', 'module-native-absent')),
    common.case('genuine', ('full-four-files', 'version-functional', 'minimal-globals-functional')),
    common.case('laziness', ('unused', 'failure-after-write')),
    common.case('typing', ('direct.int', 'ordinary.int', 'direct.string', 'ordinary.string')),
    common.case('native-boundary', ('configure', 'environ', 'escape'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASEPLAN]
DIAGNOSTICS = {
    'local': r'(?:local[^\n]*(?:bool|boolean)|(?:bool|boolean)[^\n]*local)',
    'native': r'(?:name [\'\"]native[\'\"] is not defined|Variable [`\'\"]native[`\'\"] not found|undefined[^\n]*native)',
    'failure': r'(?:Error in fail:|error:)[^\n]*LOCAL_AFTER_WRITE',
    'configure': r'configure[^\n]*(?:unsupported|not supported)',
    'environ': r'environ[^\n]*(?:unsupported|not supported)',
    'escape': r'(?:path|repository)[^\n]*(?:escape|traversal|\.\.|relative|outside)',
}
GLOBALS_PRESENT = ('macro', 'PackageSpecificationInfo', 'RunEnvironmentInfo', 'set', 'subrule', 'DefaultInfo')
GLOBALS_NAMES = ('cc_proto_aspect', 'CcSharedLibraryInfo', 'CcSharedLibraryHintInfo', 'JavaInfo',
    'JavaPluginInfo', 'macro', 'PackageSpecificationInfo', 'ProtoInfo', 'PyCcLinkParamsProvider',
    'PyInfo', 'PyRuntimeInfo', 'RunEnvironmentInfo', 'set', 'subrule', 'DefaultInfo', '__TestingOnly_NeverAvailable')
MIN_NAMES = ('DefaultInfo', '__TestingOnly_NeverAvailable')
MODULE = 'module(name="local_contract")\n'
PUBLIC = 'exports_files(["a","b"],visibility=["//visibility:public"])\n'
MARKER = 'NATIVERUN'


def archive_plan():
    """Lexical archive inspection only; never executes/emulates Starlark."""
    if digest(ARCHIVE.read_bytes()) != ARCHIVE_SHA256:
        raise ValueError('genuine archive SHA256 changed')
    files, directories = {}, []
    with tarfile.open(ARCHIVE) as archive:
        for member in archive:
            parts = Path(member.name).parts
            if parts[0] != 'bazel_features-1.42.1' or '..' in parts or Path(member.name).is_absolute():
                raise ValueError('unsafe archive member')
            name = Path('features', *parts[1:]).as_posix()
            if member.isdir():
                directories.append(name)
            elif member.isfile():
                if name in files:
                    raise ValueError('duplicate archive file')
                files[name] = (archive.extractfile(member).read(), member.mode)
            else:
                raise ValueError('unexpected archive member type')
    if len(files) != 41 or len(directories) != 7:
        raise ValueError('incomplete genuine archive manifest')
    return files, sorted(directories)


def generated_expectations(minimal=False):
    files, _ = archive_plan()
    result = {}
    for helper, output in (('globals', 'globals.bzl'), ('version', 'version.bzl')):
        tree = ast.parse(files['features/private/'+helper+'_repo.bzl'][0])
        calls = [n for n in ast.walk(tree) if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)
                 and n.func.attr == 'file' and isinstance(n.args[0], ast.Constant) and n.args[0].value == 'BUILD.bazel']
        if len(calls) != 1:
            raise ValueError('genuine BUILD literal changed')
        result[helper+'-BUILD'] = ast.literal_eval(calls[0].args[1]).encode()
    names = MIN_NAMES if minimal else GLOBALS_NAMES
    result['globals.bzl'] = ('globals = struct(\n'+''.join(
        "    %s = getattr(getattr(native, 'legacy_globals', None), '%s', %s),\n" %
        (name, name, name if name in GLOBALS_PRESENT else 'None') for name in names)+')').encode()
    result['version.bzl'] = b"version = '9.3.0'"
    return result


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned case/probe')
    files = {'MODULE.bazel': MODULE, 'BUILD.bazel': ''}
    expected, diagnostic, callbacks, repos = {'result.txt': b'9.3.0\n9.3.0\n'}, None, 1, 1
    generated, genuine, actions = {}, False, 1
    route = probe.split('.')[0] if name in ('lifecycle', 'typing') else 'direct'
    local = probe.split('.')[1] if name == 'lifecycle' else 'true'
    keyword = '' if local == 'default' else ',local='+('False' if local == 'false' else 'True')
    if name == 'typing':
        keyword, diagnostic = ',local='+('1' if probe.endswith('.int') else '"yes"'), 'local'
    if name == 'native-boundary' and probe in ('configure', 'environ'):
        keyword += ',configure=True' if probe == 'configure' else ',environ=["QA_UNUSED"]'
        diagnostic = probe
    body = ('print("NATIVERUN")\nctx.file("a",native.bazel_version+"\\n",executable=True)\n'
            'ctx.file("b",native.bazel_version+"\\n",executable=True)\n'
            'ctx.file("BUILD.bazel",'+repr(PUBLIC)+',executable=False)')
    if name == 'laziness':
        body += '\nfail("LOCAL_AFTER_WRITE")'
        if probe == 'failure-after-write':
            diagnostic = 'failure'
        else:
            callbacks, repos, expected = 0, 0, {'result.txt': b'unused\n'}
    if name == 'native-boundary' and probe == 'escape':
        body = 'ctx.file("../outside","forbidden")'
        diagnostic = 'escape'
    files['defs.bzl'] = 'def _impl(ctx):\n'+common.indent(body)+'r=repository_rule(implementation=_impl'+keyword+')\n'
    if route == 'ordinary':
        files['ext.bzl'] = 'load(":defs.bzl","r")\ndef _ext(ctx):\n    r(name="made")\ne=module_extension(implementation=_ext)\n'
        files['MODULE.bazel'] += 'e=use_extension("//:ext.bzl","e")\nuse_repo(e,"made")\n'
    else:
        files['MODULE.bazel'] += 'r=use_repo_rule("//:defs.bzl","r")\nr(name="made")\n'
    files['BUILD.bazel'] = ('filegroup(name="sources",srcs=["@made//:a","@made//:b"])\n'
        'genrule(name="result",srcs=["@made//:a","@made//:b"],outs=["result.txt"],cmd="cat $(SRCS) > $@")\n')
    if name == 'laziness' and probe == 'unused':
        files['BUILD.bazel'] = 'genrule(name="result",outs=["result.txt"],cmd="printf \'unused\\n\' > $@")\n'
    if name == 'phase':
        if probe == 'shared-transitive':
            files['transitive.bzl'] = 'observed=getattr(native,"bazel_version",None)\n'
            files['defs.bzl'] = ('load(":transitive.bzl","observed")\n'
                'INITIAL=native.bazel_version\ndef _impl(ctx):\n    print("NATIVERUN")\n'
                '    ctx.file("a",observed+"/"+INITIAL+"/"+native.bazel_version+"\\n")\n'
                '    ctx.file("b","phase\\n")\n    ctx.file("BUILD.bazel",'+repr(PUBLIC)+')\n'
                'r=repository_rule(implementation=_impl,local=True)\n')
            files['consumer.bzl'] = ('load(":transitive.bzl","observed")\n'
                'def check():\n    if observed != None or getattr(native,"bazel_version",None) != None:\n'
                '        fail("BUILD_LOADER_VERSION_LEAK")\n')
            files['BUILD.bazel'] = 'load(":consumer.bzl","check")\ncheck()\n'+files['BUILD.bazel']
            expected = {'result.txt': b'9.3.0/9.3.0/9.3.0\nphase\n'}
        else:
            diagnostic = 'native'
            files['MODULE.bazel'] = MODULE + ('x=native.bazel_version\n' if probe == 'module-native-absent' else '')
            files['BUILD.bazel'] = 'x=native.bazel_version\n' if probe == 'build-native-absent' else 'filegroup(name="result")\n'
    if name == 'genuine':
        genuine, callbacks = True, 0
        expected_files = generated_expectations(probe == 'minimal-globals-functional')
        files['MODULE.bazel'] = (MODULE+'bazel_dep(name="bazel_features",version="1.42.1")\n'
            'local_path_override(module_name="bazel_features",path="features")\n'
            'e=use_extension("//:ext.bzl","e")\nuse_repo(e,"g","v","copied")\n')
        prefix = ('load("@bazel_features//private:globals_repo.bzl","globals_repo")\n'
            'load("@bazel_features//private:version_repo.bzl","version_repo")\n'
            'load("@bazel_features//private:globals.bzl","GLOBALS")\n')
        generated = expected_files if probe == 'full-four-files' else {}
        if probe == 'full-four-files':
            names = list(expected_files)
            definitions = 'def _copy(ctx):\n'
            for index, output in enumerate(names):
                definitions += '    ctx.template('+repr(output)+',ctx.attr.src'+str(index)+',{},executable=True)\n'
            definitions += ('    ctx.file("BUILD.bazel",'+repr('exports_files('+repr(names)+',visibility=["//visibility:public"])\n')+',executable=False)\n'
                'copier=repository_rule(implementation=_copy,attrs={'+','.join(
                    repr('src'+str(i))+':attr.label(mandatory=True,allow_single_file=True)' for i in range(4))+'})\n')
            files['copy.bzl'] = definitions
            prefix += 'load(":copy.bzl","copier")\n'
            extbody = ('globals_repo(name="g",globals=GLOBALS)\nversion_repo(name="v")\n'
                'copier(name="copied",src0="@g//:BUILD.bazel",src1="@v//:BUILD.bazel",'
                'src2="@g//:globals.bzl",src3="@v//:version.bzl")')
            files['BUILD.bazel'] = ''.join('genrule(name="out'+str(i)+'",srcs=["@copied//:'+n+'"],outs=['+repr(n+'.out')+'],cmd="cat $(SRCS) > $@")\n' for i, n in enumerate(names))
            files['BUILD.bazel'] += 'filegroup(name="result",srcs='+repr([':out'+str(i) for i in range(4)])+')\n'
            expected, repos, actions = {n+'.out': raw for n, raw in expected_files.items()}, 3, 4
        else:
            files['MODULE.bazel'] = files['MODULE.bazel'].replace('use_repo(e,"g","v","copied")', 'use_repo(e,"g","v")')
            extbody = ('globals_repo(name="g",globals={"DefaultInfo":("0.0.1",""),"__TestingOnly_NeverAvailable":("1000000000.0.0","")})\nversion_repo(name="v")')
            if probe == 'version-functional':
                files['BUILD.bazel'] = ('load("@v//:version.bzl","version")\n'
                    'genrule(name="result",outs=["result.txt"],cmd="printf %s "+version+" > $@")\n')
                expected = {'result.txt': b'9.3.0'}
            else:
                files['consumer.bzl'] = ('load("@g//:globals.bzl","globals")\n'
                    'def _impl(ctx):\n    if globals.DefaultInfo != DefaultInfo or globals.__TestingOnly_NeverAvailable != None:\n'
                    '        fail("GENUINE_MIN_GLOBALS_MISMATCH")\n'
                    '    out=ctx.actions.declare_file("result.txt")\n'
                    '    ctx.actions.write(out,"provider identity and None\\n")\n'
                    '    return [DefaultInfo(files=depset([out]))]\nr=rule(implementation=_impl)\n')
                files['BUILD.bazel'] = 'load(":consumer.bzl","r")\nr(name="result")\n'
                expected = {'result.txt': b'provider identity and None\n'}
            repos = 1
        files['ext.bzl'] = prefix+'def _ext(ctx):\n'+common.indent(extbody)+'e=module_extension(implementation=_ext)\n'
        files.pop('defs.bzl', None)
    return dict(files={p: s.encode() for p, s in files.items()}, expected=expected,
        target='@missing//:result' if name == 'phase' and probe == 'module-native-absent' else '//:result',
        diagnostic=diagnostic, callbacks=callbacks, repos=repos, actions=actions,
        genuine=genuine, generated=generated)


def phases_for(item, reference_only=False):
    return common.phases_for(item, reference_only)


def steps_for(value, engine):
    if value['diagnostic']:
        return [('build', value['target'], [])]
    return [('build', '//:result', [])]+([('cquery', '//:result', ['--output=files'])] if engine == 'reference' else [])+[
        ('build', '//:result', [])]+([('cquery', '//:result', ['--output=files'])] if engine == 'reference' else [])


def require_rejection(record, diagnostic):
    text, code = record['stderr'], record['returncode']
    unrelated = common.UNRELATED.pattern
    if diagnostic == 'failure':
        # This precise analysis trailer reports our intentional callback fail,
        # not a network/download failure. All other fetch failures stay fatal.
        text = re.sub(r'which failed to fetch\. no such package [^\n]*: LOCAL_AFTER_WRITE',
                      'intentional callback rejection', text)
    if diagnostic == 'native':
        # Undefined global is a genuine static BUILD/MODULE validation error.
        unrelated = unrelated.replace('|syntax error', '')
    if (type(code) is not int or not 0 < code < 128 or code in (101, 124, 125, 126, 127)
            or record['timed_out'] or any(record.get(k) for k in ('spawn_error', 'interrupted', 'output_truncated'))
            or record['stdout'].strip() or re.search(unrelated, text, re.I)
            or not re.search(DIAGNOSTICS[diagnostic], text, re.I)):
        raise ValueError('expected controlled local repository rejection: '+diagnostic)


def materialize(workspace, value):
    # Preserve workspace inode for the actual persistent reference server.
    for path in workspace.iterdir():
        if path.is_symlink() or path.is_file():
            path.unlink()
        else:
            shutil.rmtree(path)
    files = {p: (raw, 0o644) for p, raw in value['files'].items()}
    if value['genuine']:
        archive, directories = archive_plan()
        files.update(archive)
        for directory in directories:
            (workspace / directory).mkdir(parents=True, exist_ok=True)
    for name, (raw, mode) in files.items():
        path = workspace / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
        path.chmod(mode)
    return [workspace / p for p in files]


def manifest(workspace, genuine):
    if not genuine:
        return None
    root = workspace / 'features'
    return dict(files=sorted(str(p.relative_to(workspace)) for p in root.rglob('*') if p.is_file()),
        directories=sorted(['features']+[str(p.relative_to(workspace)) for p in root.rglob('*') if p.is_dir()]))


def publications(cache):
    return sorted(p.name for p in direct.trees_directory(cache).glob('*'))


def generated_snapshot(value, engine, cache, reference):
    if not value['generated']:
        return {}
    base = direct.trees_directory(cache) if engine == 'native' else reference.output_base / 'external'
    result = {}
    for helper in ('globals', 'version'):
        candidates = [p for p in base.glob('*/'+helper+'.bzl') if p.is_file()
                      and (p.parent / 'BUILD.bazel').is_file()
                      and (p.parent / 'BUILD.bazel').read_bytes() == value['generated'][helper+'-BUILD']]
        if len(candidates) != 1:
            raise ValueError('expected one actual generated '+helper+' repository, found '+str(candidates))
        path = candidates[0].resolve(strict=True)
        result[helper+'.bzl'] = evidence(path)
        result[helper+'-BUILD'] = evidence(path.parent / 'BUILD.bazel')
    return result


def run_phase(args, runner, scratch, workspace, reference, item, probe, engine):
    value = fixture(item['name'], probe)
    paths = materialize(workspace, value)
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    registry = scratch / 'registry'
    phase = dict(probe=probe, engine=engine, cache=str(cache), runs=[], outputs=[], generated=[],
        trees=[], status='FAIL', inputs_before=snapshot(paths), manifest_before=manifest(workspace, value['genuine']))
    try:
        for index, (command, target, extra) in enumerate(steps_for(value, engine)):
            argv = extensions.command_argv(args, reference, engine, cache, registry, command, target, extra)
            record = runner.run(item['name']+'.'+probe+'.'+engine+'.'+str(index), argv, workspace)
            phase['runs'].append(record['id'])
            if value['diagnostic']:
                require_rejection(record, value['diagnostic'])
                if engine == 'native':
                    direct.no_publication(cache)
                break
            success(record)
            if command == ('build' if engine == 'native' else 'cquery'):
                output_paths = direct.native_paths(record, target, scratch) if engine == 'native' else reference_output_paths(record['stdout'], workspace, reference.output_base)
                phase['outputs'].append([evidence(p) for p in output_paths])
                phase['generated'].append(generated_snapshot(value, engine, cache, reference))
                if engine == 'native':
                    phase['trees'].append(publications(cache))
        phase['publication_entries'] = publications(cache) if engine == 'native' else []
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    finally:
        phase['inputs_after'] = snapshot(paths)
        phase['manifest_after'] = manifest(workspace, value['genuine'])
    return phase


def validate_phase(phase, report, args, workspace, reference, item):
    probe, engine = phase['probe'], phase['engine']
    value = fixture(item['name'], probe)
    if (phase['status'] != 'PASS' or phase.get('error') or phase['inputs_before'] != phase['inputs_after']
            or phase['manifest_before'] != phase['manifest_after']):
        raise ValueError('failed phase or changed source inputs')
    files = {p: (raw, 0o644) for p, raw in value['files'].items()}
    expected_manifest = None
    if value['genuine']:
        archive, directories = archive_plan()
        files.update(archive)
        expected_manifest = dict(files=sorted(archive), directories=directories)
    if phase['manifest_before'] != expected_manifest:
        raise ValueError('incomplete or altered archive file/directory manifest')
    planned = {str(workspace / p): (raw, mode) for p, (raw, mode) in files.items()}
    if set(planned) != set(phase['inputs_before']):
        raise ValueError('missing or unplanned source inputs')
    for path, (raw, mode) in planned.items():
        e = phase['inputs_before'][path]
        if e['path'] != path or e['kind'] != 'file' or e['mode'] != mode or not valid_evidence(e, raw):
            raise ValueError('source differs from immutable fixture plan')
    scratch = Path(report['scratch_directory'])
    cache = scratch / 'cases' / item['name'] / probe / 'native-cache'
    if phase['cache'] != str(cache):
        raise ValueError('unplanned native cache')
    steps = steps_for(value, engine)
    if len(phase['runs']) != len(steps):
        raise ValueError('missing or extra commands')
    output_index = 0
    for index, (rid, (command, target, extra)) in enumerate(zip(phase['runs'], steps)):
        record = report['runs'][rid]
        argv = extensions.command_argv(args, reference, engine, cache, scratch / 'registry', command, target, extra)
        if (record['argv'] != argv or record['cwd'] != str(workspace) or record['timeout_seconds'] != args.timeout
                or record['name'] != item['name']+'.'+probe+'.'+engine+'.'+str(index)):
            raise ValueError('command provenance mismatch')
        if value['diagnostic']:
            require_rejection(record, value['diagnostic'])
            continue
        success(record)
        if engine == 'native':
            # Match callback output lines, not source snippets or replayed reference logs.
            markers = re.findall(r'(?m)^(?:DEBUG: [^\n]*?:\d+:\d+: )?NATIVERUN\s*$', record['stderr'])
            if len(markers) != value['callbacks']:
                raise ValueError('fresh CLI did not execute each demanded callback exactly once')
            common.action_counts(record, (value['actions'], 0) if index == 0 else (0, value['actions']))
        if command != ('build' if engine == 'native' else 'cquery'):
            continue
        paths = direct.native_paths(record, target, scratch) if engine == 'native' else reference_output_paths(record['stdout'], workspace, reference.output_base)
        entries = phase['outputs'][output_index]
        if list(map(str, paths)) != [e['path'] for e in entries] or len(entries) != len(value['expected']):
            raise ValueError('outputs not tied to exact actual CLI result')
        if {Path(e['path']).name for e in entries} != set(value['expected']):
            raise ValueError('missing or unexpected consumer output')
        for e in entries:
            if e['kind'] != 'file' or e['mode'] != 0o555 or not valid_evidence(e, value['expected'][Path(e['path']).name], True):
                raise ValueError('consumer bytes or mode mismatch')
        generated = phase['generated'][output_index]
        if set(generated) != set(value['generated']):
            raise ValueError('missing genuine callback output file')
        for key, e in generated.items():
            path = Path(e['path'])
            base = direct.trees_directory(cache) if engine == 'native' else reference.output_base.parent
            if (not path.is_relative_to(base) or path.name != ('BUILD.bazel' if key.endswith('-BUILD') else key)
                    or e['kind'] != 'file' or not valid_evidence(e, value['generated'][key], True)):
                raise ValueError('genuine callback file bytes/provenance mismatch')
        output_index += 1
    if value['diagnostic']:
        if phase['outputs'] or phase['generated'] or phase['trees'] or phase['publication_entries']:
            raise ValueError('rejected phase published outputs')
        return
    if len(phase['outputs']) != 2 or len(phase['generated']) != 2:
        raise ValueError('missing cold/warm execution evidence')
    if phase['outputs'][0] != phase['outputs'][1] or phase['generated'][0] != phase['generated'][1]:
        raise ValueError('unchanged inputs changed cold/warm paths, bytes or modes')
    if engine == 'native':
        trees = phase['publication_entries']
        if (len(trees) != value['repos'] or len(set(trees)) != len(trees)
                or any(not re.fullmatch('[0-9a-f]{64}', t) for t in trees) or phase['trees'] != [trees, trees]):
            raise ValueError('publication identity/count changed or staging tree remained')
    elif phase['trees'] or phase['publication_entries']:
        raise ValueError('reference phase contains native publication claims')


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
    parser.set_defaults(output=ROOT / 'artifacts/local-repositories-validation.json')
    return parser


def report_passes(report):
    try:
        args = argument_parser().parse_args([])
        for key, value in report['configuration'].items():
            setattr(args, key, Path(value) if key in ('native', 'reference', 'output', 'scratch_parent') else value)
        if (report['planned_checks'] != CASEPLAN or [(c['name'], c['kind']) for c in report['checks']] != PLAN
                or report['plan_sha256'] != digest(json.dumps(CASEPLAN, sort_keys=True).encode())
                or report['errors'] or report['cleanup'] or not report['scratch_cleaned']
                or not report['reference_shutdown_succeeded'] or report['sources_before'] != report['sources_after']
                or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(p == e['path'] and valid_evidence(e) for p, e in report['sources_before'].items())
                or report['sources_before'][str(ARCHIVE)]['sha256'] != ARCHIVE_SHA256
                or report['executables'] != report['executables_after']
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['reference_release'] != 'bazel 9.3.0'):
            return False
        engines = ('reference',) if args.reference_only else ('reference', 'native')
        if set(report['executables']) != set(engines):
            return False
        if not args.reference_only and report['executables']['native']['sha256'] != args.native_sha256:
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
        for path, original in registry.items():
            e = report['registry_before'][path]
            if e != dict(original, path=path):
                return False
        reference = DirectReference(args.reference, workspace, scratch, None, 'local-repositories')
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for r in runs:
            engine = next((e for e in engines if str(getattr(args, e)) == r['argv'][0]), None)
            if engine is None or r['executable_sha256'] != report['executables'][engine]['sha256'] or not r['environment'] or not r['environment_keys']:
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(r[field+'_base64'], validate=True)
                if (digest(raw) != r[field+'_sha256'] or digest(raw) != r[field+'_full_sha256']
                        or len(raw) != r[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != r[field]):
                    return False
        for record, name, argv, timeout in (
                (runs[0], 'reference.version', reference.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], 'local-repositories.shutdown', reference.startup+['shutdown']+list(COMMON_FLAGS), min(args.timeout, 15))):
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
    for field in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.output in [args.native, args.reference, *SOURCES] or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests', 'src')):
        raise ValueError('report cannot overwrite source or executable')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
        planned_checks=CASEPLAN, plan_sha256=digest(json.dumps(CASEPLAN, sort_keys=True).encode()),
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        scratch_cleaned=False, reference_shutdown_succeeded=False,
        started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        limitations=['Bounded local metadata/version contract; not full Bazel/application parity.',
            'Native is serverless: both local values execute once per demanded repository per fresh CLI invocation.',
            'Reference persistent-server lifecycle differs; print counts are not compared or treated as reference callback evidence.',
            'Pinned prior genuine lifecycle audit: first 1/1, same-instance 0/0, after shutdown local 1/nonlocal 0.',
            'Original bazel_features 1.42.1 fixture, not an assertion of application dependency version.',
            'Full GLOBALS callback bytes are consumed without evaluating unsupported upstream bzl_library BUILD or full features.bzl.',
            'Minimal globals functional probe proves only DefaultInfo identity and unavailable None, not the full globals SDK.',
            'No wrapper, fake builtins, fallback, user workspace, HTTP server or credential modifications.',
            'Reference-only preparation never executes native or claims native parity.'])
    runner = extensions.RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = common.executable_evidence(args)
        scratch = Path(tempfile.mkdtemp(prefix='local-repositories-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(MODULE)
        shutil.copytree(REGISTRY, scratch / 'registry')
        report['registry_before'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        reference = DirectReference(args.reference, workspace, scratch, runner, 'local-repositories')
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
