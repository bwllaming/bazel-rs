#!/usr/bin/env python3
"""Eight real-CLI built-in-module checks; NOT full Bazel compatibility.

Two genuine 9.3.0 differentials and six native policy/cache checks. Native uses
only the captured file registry, with remote access disabled by default. The
reference may download genuine sources. Unit tests do not launch either engine.
"""

import base64
from concurrent.futures import ThreadPoolExecutor
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import sys
import tarfile
import tempfile

from validate import ROOT, COMMON_FLAGS, BUILD_FLAGS, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256, validate_binaries
from validate_repository_rules import (RecordingRunner, evidence, valid_evidence, snapshot,
                                       success as common_success, native_paths, argument_parser as common_parser)


FIXTURE = ROOT / 'tests/fixtures/builtin_modules'
RESOURCES = ROOT / 'resources/bazel_tools'
TARGET = '@bazel_tools//tools/build_defs/repo:utils.bzl'
CANONICAL = '@' + TARGET
HELPER = 'tools/build_defs/repo/utils.bzl'
SELECTOR = '--experimental-builtin-modules'
ROOT_MODULE = 'module(name = "opencode_builtin_fixture", version = "1.0")\n'
BUILD = "load(':copy.bzl', 'copy')\ncopy()\n"
MACRO = ("load('@bazel_tools//tools/build_defs/repo:utils.bzl', 'update_attrs')\n"
         "def copy():\n"
         "    values = update_attrs(struct(name = 'copy'), [], {'srcs': ['@bazel_tools//tools/build_defs/repo:utils.bzl'], "
         "'outs': ['copy.txt'], 'cmd': 'cat $(SRCS) > $@'})\n"
         "    native.genrule(**values)\n")
STRACE = Path('/usr/bin/strace')


def case(name, probes, kind='native_boundary'):
    return dict(name=name, kind=kind, probes=list(probes))


CASES = [
    case('genuine.query-and-source', ('empty-root',), 'differential'),
    case('genuine.helper-and-copy', ('older-platforms',), 'differential'),
    case('native.profile', ('default', 'false', 'malformed', 'filesystem-override')),
    case('native.collisions', ('root-name', 'ignored-dev-alias', 'root-repo-name')),
    case('native.no-registry-fallback', ('empty-registry',)),
    case('native.metadata-not-filesystem', ('metadata-consumed', 'filesystem-retained')),
    case('native.cache-repair', ('cold', 'warm', 'bytes', 'mode', 'extra', 'missing-manifest')),
    case('native.parallel-distribution', ('standalone', 'parallel-1', 'parallel-2')),
]
PLAN = [(c['name'], c['kind']) for c in CASES]
DIAGNOSTICS = {
    # N51: every module maps @bazel_tools (ModuleThreadContext.java:383-401);
    # the explicit profile fails only when its contents are demanded.
    'explicit': r'the built-in repository @@bazel_tools is visible to every module but has no contents in the explicit-module profile',
    'malformed': r'--experimental-builtin-modules expects true or false',
    'filesystem-override': r'unsupported option --override_repository; MODULE overrides do not replace the built-in repository filesystem',
    # ModuleThreadContext.buildModule (:383-401), in every profile since N51.
    'collision': r"The repo name 'bazel_tools' cannot be defined as a built-in dependency at <builtin> as it is already defined .+, 'bazel_tools' is a built-in dependency and cannot be used by any 'bazel_dep' or 'use_repo' directive",
    'empty-registry': r'module rules_license@1\.0\.0 was not found in registries \[/[^\]\n]+\]',
    'metadata-consumed': r'module metadata_only_probe@8\.7 was not found in registries \[/[^\]\n]+\]',
}
INFRASTRUCTURE = re.compile(
    r'panick?ed|\bpanic\b|internal error|FATAL:|crashed|segmentation fault|'
    r'falling back|fallback to|timed out|timeout|\bdownload\b|\bnetwork\b|'
    r'connection (?:refused|reset)|SSLHandshake|UnknownHost|permission denied|access denied|'
    r'parse error|syntax error|unexpected token|no (?:MODULE.bazel|workspace)|'
    r'(?:workspace|MODULE.bazel)[^\n]*(?:not found|does not exist)|could not determine current package', re.I)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def json_digest(value):
    return digest(json.dumps(value, sort_keys=True).encode())


def success(record):
    if type(record['returncode']) is not int:
        raise ValueError('missing integer process exit status')
    return common_success(record)


def fingerprint(path):
    """Stream hashes rather than putting the 22 MB archive/binaries in JSON."""
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise ValueError('expected non-symlink regular file: ' + str(path))
    value = hashlib.sha256()
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return dict(path=str(path), sha256=value.hexdigest(), size_bytes=info.st_size,
                mode=stat.S_IMODE(info.st_mode))


def fingerprints(paths):
    return {str(p): fingerprint(p) for p in sorted(paths)}


def source_paths():
    fixed = [ROOT / p for p in (
        'Cargo.toml', 'Cargo.lock', 'scripts/validate_builtin_modules.py',
        'tests/test_validate_builtin_modules.py', 'scripts/validate.py',
        'scripts/validate_direct_repositories.py', 'scripts/validate_repository_rules.py')]
    return sorted(set(fixed + list((ROOT / 'src').rglob('*.rs'))
                      + [p for p in FIXTURE.rglob('*') if p.is_file()]
                      + [RESOURCES / p for p in ('manifest.json', 'MODULE.bazel', 'embedded_tools.tar.gz')]
                      + ([ROOT / 'build.rs'] if (ROOT / 'build.rs').exists() else [])))


def load_inputs():
    manifest = json.loads((RESOURCES / 'manifest.json').read_bytes())
    expectations = json.loads((FIXTURE / 'expectations.json').read_bytes())
    provenance = json.loads((FIXTURE / 'provenance.json').read_bytes())
    if (manifest['distribution_sha256'] != REFERENCE_SHA256 or manifest['bazel_version'] != '9.3.0'
            or manifest['file_count'] != 421 or len(manifest['files']) != 421
            or expectations['reference_sha256'] != REFERENCE_SHA256
            or expectations['reference_version'] != '9.3.0'):
        raise ValueError('missing genuine pinned built-in/captured registry identity')
    entries = {v['path']: v for v in manifest['files']}
    if len(entries) != 421 or any(v['mode'] != 0o755 for v in entries.values()):
        raise ValueError('invalid distributed manifest')
    if fingerprint(RESOURCES / 'embedded_tools.tar.gz')['sha256'] != manifest['archive_sha256']:
        raise ValueError('distributed archive hash mismatch')
    for relative, expected in provenance['files'].items():
        actual = fingerprint(FIXTURE / relative)
        if (actual['sha256'] != expected['sha256'] or actual['size_bytes'] != expected['size_bytes']
                or not expected['source_url'].startswith('https://bcr.bazel.build/')):
            raise ValueError('captured registry provenance mismatch: ' + relative)
    for item in expectations['cases']:
        for kind in ('graph', 'lockfile'):
            if fingerprint(FIXTURE / item['reference_' + kind])['sha256'] != item[kind + '_sha256']:
                raise ValueError('captured reference evidence changed')
    roots = {v['name']: v['root_module'] for v in expectations['cases']}
    if set(roots) != {'empty-root', 'older-platforms'} or roots['empty-root'] != ROOT_MODULE:
        raise ValueError('unexpected fixture case plan')
    with tarfile.open(RESOURCES / 'embedded_tools.tar.gz', 'r:gz') as archive:
        member = archive.getmember(HELPER)
        if not member.isfile():
            raise ValueError('official helper is not a regular archive member')
        with archive.extractfile(member) as stream:
            raw = stream.read()
    if digest(raw) != entries[HELPER]['sha256'] or len(raw) != entries[HELPER]['size']:
        raise ValueError('official helper does not match distributed manifest')
    return manifest, roots, raw


def require_rejection(record, diagnostic):
    code = record['returncode']
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record['timed_out']
            or record.get('spawn_error') or record.get('interrupted') or record.get('output_truncated')
            or record['stdout'].strip() or INFRASTRUCTURE.search(record['stderr'])
            or not re.search(DIAGNOSTICS[diagnostic], record['stderr'])):
        raise ValueError('expected controlled native rejection: ' + diagnostic)


def require_query(record, engine):
    """Only the one built-in's @/@@ spelling is normalized, never arbitrary aliases."""
    labels = success(record).splitlines()
    if labels not in ([[CANONICAL]] if engine == 'native' else [[TARGET], [CANONICAL]]):
        raise ValueError('expected exactly the genuine built-in helper label')
    return dict(raw_labels=labels, canonical_labels=[CANONICAL])


def native_command(executable, cache, registry, command, target, selector=SELECTOR, extra=()):
    # Explicit lockfile mode and local registry: no MODULE.bazel.lock I/O and
    # never the default https://bcr.bazel.build/ registry.
    return [str(executable), '--ignore_all_rc_files', '--cache-dir=' + str(cache), command,
            '--lockfile_mode=off', '--registry=' + registry.as_uri(), *([selector] if selector else []),
            *extra, target]


def traced_command(argv, trace):
    return [str(STRACE), '-f', '-e', 'trace=execve,execveat', '-o', str(trace), *argv]


def require_native_only_trace(value, executable):
    if not valid_evidence(value) or value['kind'] != 'file':
        raise ValueError('missing actual exec trace')
    text = base64.b64decode(value['bytes_base64']).decode('utf-8', errors='strict')
    executions = [line for line in text.splitlines() if 'execve' in line]
    expected = r'(?:\d+\s+)?execve\(' + re.escape(json.dumps(str(executable))) + r', \[.*\], .*\)\s+= 0'
    if (len(executions) != 1 or not re.fullmatch(expected, executions[0])
            or not re.search(r'\+\+\+ exited with 0 \+\+\+', text)):
        raise ValueError('trace must show only initial copied-native exec, no other exec attempts')
    return dict(exec_attempts=1, initial_executable=str(executable), no_other_exec_attempts=True)


def trees_directory(cache):
    return cache / 'repositories/builtins/trees'


def no_publication(cache):
    entries = sorted(str(p) for p in trees_directory(cache).glob('*'))
    if entries:
        raise ValueError('rejected command extracted or staged a built-in tree')
    return entries


def expected_directories(manifest):
    return sorted({p.as_posix() for v in manifest['files'] for p in Path(v['path']).parents
                   if p != Path('.')})


def inspect_tree(cache, manifest):
    trees = trees_directory(cache)
    roots = [p for p in trees.glob('*') if re.fullmatch('[0-9a-f]{64}', p.name)]
    if len(roots) != 1:
        raise ValueError('expected one unique content-identity tree, not archive-hash guessing')
    root = roots[0]
    sidecar = trees / (root.name + '.json')
    if sorted(p.name for p in trees.iterdir()) != sorted([root.name, sidecar.name]):
        raise ValueError('extra tree or leaked staging/sidecar file')
    if sidecar.is_symlink() or sidecar.read_bytes() != (RESOURCES / 'manifest.json').read_bytes():
        raise ValueError('cache sidecar differs from distributed manifest')
    files, directories = {}, {}
    pending = [root]
    while pending:
        directory = pending.pop()
        info = directory.lstat()
        if not stat.S_ISDIR(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o755:
            raise ValueError('invalid tree directory type/mode')
        directories[directory.relative_to(root).as_posix()] = 0o755
        for path in sorted(directory.iterdir()):
            if stat.S_ISDIR(path.lstat().st_mode):
                pending.append(path)
            else:
                value = fingerprint(path)
                files[path.relative_to(root).as_posix()] = {
                    'sha256': value['sha256'], 'size': value['size_bytes'], 'mode': value['mode']}
    expected = {v['path']: {k: v[k] for k in ('sha256', 'size', 'mode')} for v in manifest['files']}
    if files != expected or directories != {p: 0o755 for p in ['.', *expected_directories(manifest)]}:
        raise ValueError('tree differs from immutable manifest (bytes/mode/missing/extra)')
    return dict(root=str(root), files=files, directories=directories, sidecar=fingerprint(sidecar),
                entries=sorted(p.name for p in trees.iterdir()),
                publication_stat={str(p.relative_to(root)): {'inode': p.stat().st_ino, 'mtime_ns': p.stat().st_mtime_ns}
                                  for p in (root, root / 'WORKSPACE')})


def phases_for(item):
    return [(probe, engine) for probe in item['probes']
            for engine in (('reference', 'native') if item['kind'] == 'differential' else ('native',))]


def specification(item, probe, roots):
    spec = dict(module=roots.get(probe, ROOT_MODULE), build='', target=TARGET,
                selector=SELECTOR, extra=[], diagnostic=None, metadata=None)
    if item['name'] == 'genuine.helper-and-copy':
        spec.update(build=BUILD, target='//:copy')
    elif item['name'] == 'native.profile':
        if probe == 'default':
            # The built-in modules are the default (no selector, no rejection).
            spec.update(selector=None)
        elif probe == 'false':
            spec.update(selector=SELECTOR + '=false', diagnostic='explicit')
        elif probe == 'malformed':
            spec.update(selector=SELECTOR + '=maybe', diagnostic='malformed')
        else:
            spec.update(extra=['--override_repository=bazel_tools=metadata'], diagnostic='filesystem-override')
    elif item['name'] == 'native.collisions':
        spec['module'] = {
            'root-name': 'module(name = "bazel_tools")\n',
            'root-repo-name': 'module(name = "root", repo_name = "bazel_tools")\n',
            'ignored-dev-alias': ROOT_MODULE + 'bazel_dep(name = "absentdev", version = "1", repo_name = "bazel_tools", dev_dependency = True)\n',
        }[probe]
        spec.update(diagnostic='collision', extra=['--ignore_dev_dependency=true'])
    elif item['name'] == 'native.no-registry-fallback':
        spec['diagnostic'] = 'empty-registry'
    elif item['name'] == 'native.metadata-not-filesystem':
        spec['module'] += 'local_path_override(module_name = "bazel_tools", path = "metadata")\n'
        spec['metadata'] = 'module(name = "bazel_tools", version = "8.7")\n'
        spec['target'] = CANONICAL
        if probe == 'metadata-consumed':
            spec['metadata'] += 'bazel_dep(name = "metadata_only_probe", version = "8.7")\n'
            spec['diagnostic'] = probe
    return spec


def steps_for(item, engine):
    if item['name'] == 'genuine.query-and-source':
        return [('query', TARGET, []), ('build', TARGET, [])] + ([('cquery', TARGET, ['--output=files'])] if engine == 'reference' else [])
    if item['name'] == 'genuine.helper-and-copy':
        return [('build', '//:copy', [])] + ([('cquery', '//:copy', ['--output=files'])] if engine == 'reference' else [])
    return [('query', CANONICAL if item['name'] == 'native.metadata-not-filesystem' else TARGET, [])]


def prepare(workspace, spec):
    if workspace.exists():
        shutil.rmtree(workspace)
    files = {'MODULE.bazel': spec['module'], 'BUILD.bazel': spec['build']}
    if spec['build'] == BUILD:
        files['copy.bzl'] = MACRO
    if spec['metadata'] is not None:
        files.update({'metadata/MODULE.bazel': spec['metadata'], 'metadata/marker': 'metadata is not repository contents\n',
                      'metadata/BUILD.bazel': 'exports_files(["marker"])\n'})
    for relative, text in files.items():
        path = workspace / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return [workspace / p for p in files]


def mutate_cache(cache, probe):
    root = next(p for p in trees_directory(cache).iterdir() if re.fullmatch('[0-9a-f]{64}', p.name))
    path = (trees_directory(cache) / (root.name + '.json') if probe == 'missing-manifest'
            else root / ('unexpected-entry' if probe == 'extra' else 'WORKSPACE'))
    before = evidence(path) if path.exists() else None
    if probe == 'bytes':
        path.write_bytes(b'intentional cache corruption\n')
    elif probe == 'mode':
        path.chmod(0o644)
    elif probe == 'extra':
        path.write_bytes(b'intentional extra cache file\n')
    elif probe == 'missing-manifest':
        path.unlink()
    else:
        raise ValueError('unplanned mutation')
    return dict(probe=probe, path=str(path), before=before, after=evidence(path) if path.exists() else None)


def invoke_phase(runner, reference, workspace, phase, item, spec, executable, manifest, helper, scratch):
    engine = phase['engine']
    records = []
    for index, (command, target, extra) in enumerate(steps_for(item, engine)):
        name = '.'.join([item['name'], phase['probe'], engine, str(index)])
        if engine == 'native':
            argv = native_command(executable, Path(phase['cache']), Path(phase['registry']), command,
                                  target, spec['selector'], spec['extra'])
            if phase['probe'] == 'standalone':
                trace = scratch / 'native-exec.log'
                argv = traced_command(argv, trace)
            record = runner.run(name, argv, workspace)
        else:
            reference.registry = Path(phase['registry'])
            record = reference.run(name, command, *extra, target)
        phase['runs'].append(record['id'])
        records.append(record)
        if spec['diagnostic']:
            require_rejection(record, spec['diagnostic'])
        elif command == 'query':
            phase['query'] = require_query(record, engine)
        else:
            success(record)
    if phase['probe'] == 'standalone':
        phase['exec_trace'] = evidence(trace)
        phase['exec_trace_result'] = require_native_only_trace(phase['exec_trace'], executable)
    if spec['diagnostic']:
        phase['diagnostic'] = spec['diagnostic']
        phase['publication_entries'] = no_publication(Path(phase['cache']))
    else:
        if engine == 'native':
            phase['tree'] = inspect_tree(Path(phase['cache']), manifest)
        if item['kind'] == 'differential':
            if engine == 'native':
                label = '//:copy' if item['name'] == 'genuine.helper-and-copy' else CANONICAL
                paths = native_paths(records[-1], label, scratch)
            else:
                paths = reference_output_paths(success(records[-1]), workspace, reference.output_base)
            if len(paths) != 1 or not paths[0].resolve().is_relative_to(scratch):
                raise ValueError('expected one private actual CLI output')
            phase['artifact'] = evidence(paths[0].resolve(strict=True))
            phase['artifact']['logical_path'] = str(paths[0])
            if phase['artifact']['kind'] != 'file' or not valid_evidence(phase['artifact'], helper):
                raise ValueError('CLI output does not contain official raw helper bytes')
        if spec['metadata'] is not None and ('marker' in phase['tree']['files']
                                             or phase['tree']['files']['MODULE.bazel']['sha256'] == digest(spec['metadata'].encode())):
            raise ValueError('metadata override replaced genuine repository filesystem')


def run_case(args, runner, native_runner, scratch, reference, item, manifest, roots, helper):
    check = dict(name=item['name'], kind=item['kind'], input=item, status='FAIL', parity_claim=False, phases=[])
    runner.report['checks'].append(check)
    case_root = scratch / 'cases' / item['name']
    workspace = reference.workspace
    registry = FIXTURE / 'registry'
    if item['name'] == 'native.no-registry-fallback':
        registry = case_root / 'empty-registry'
        registry.mkdir(parents=True)
        (registry / 'bazel_registry.json').write_text('{}\n')
    registry_files = sorted(p for p in registry.rglob('*') if p.is_file())
    shared = item['name'] in ('native.cache-repair', 'native.parallel-distribution')
    prior_inputs = None
    for probe in item['probes']:
        if probe == 'parallel-2':
            continue
        spec = specification(item, probe, roots)
        paths = prepare(workspace, spec)
        mutation = None
        cache = case_root / ('native-cache' if shared and probe != 'standalone' else probe + '-cache')
        if item['name'] == 'native.cache-repair' and probe not in ('cold', 'warm'):
            try:
                mutation = mutate_cache(cache, probe)
            except (OSError, ValueError, StopIteration) as exc:
                check['error'] = str(exc)
                continue
        phases = []
        for engine in (('reference', 'native') if item['kind'] == 'differential' else ('native',)):
            phases.append(dict(probe=probe, engine=engine, runs=[], status='FAIL', cache=str(cache), registry=str(registry)))
        if probe == 'parallel-1':
            phases.append(dict(phases[0], probe='parallel-2', runs=[]))
        before = snapshot(paths)
        registry_before = json_digest(fingerprints(registry_files))
        for phase in phases:
            phase.update(inputs_before=before, registry_before=registry_before)
            if mutation:
                phase['mutation'] = mutation
            if prior_inputs is not None:
                phase['previous_inputs'] = prior_inputs
        check['phases'].extend(phases)

        def execute_phase(phase, local):
            try:
                invoke_phase(local, reference, workspace, phase, item, spec, args.distributed_native,
                             manifest, helper, scratch)
                phase['status'] = 'PASS'
            except Exception as exc:
                phase['error'] = type(exc).__name__ + ': ' + str(exc)
            finally:
                phase['inputs_after'] = snapshot(paths)
                phase['registry_after'] = json_digest(fingerprints(registry_files))
                if phase['inputs_after'] != before or phase['registry_after'] != registry_before:
                    phase.update(status='FAIL', error='CLI changed frozen inputs/registry')

        if probe == 'parallel-1':
            locals_ = [RecordingRunner({'runs': []}, args.timeout) for _ in phases]
            for local in locals_:
                local.env = dict(native_runner.env)
            with ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(execute_phase, phase, local) for phase, local in zip(phases, locals_)]
                for future in futures:
                    future.result()
            for phase, local in zip(phases, locals_):
                offset = len(runner.report['runs'])
                for record in local.report['runs']:
                    record['id'] += offset
                    runner.report['runs'].append(record)
                phase['runs'] = [i + offset for i in phase['runs']]
        else:
            for phase in phases:
                execute_phase(phase, runner if phase['engine'] == 'reference' else native_runner)
        prior_inputs = before
    try:
        validate_check(check, runner.report, manifest, roots, helper)
        check.update(status='PASS', parity_claim=item['kind'] == 'differential')
    except (ValueError, KeyError, IndexError) as exc:
        check['error'] = str(exc)
        if any(p['engine'] == 'reference' and p['status'] != 'PASS' for p in check['phases']):
            check['status'] = 'REFERENCE_FAILURE'


def validate_check(check, report, manifest, roots, helper):
    item = next(c for c in CASES if c['name'] == check['name'])
    if (check['input'] != item or check['kind'] != item['kind']
            or [(p['probe'], p['engine']) for p in check['phases']] != phases_for(item)):
        raise ValueError('missing, duplicate, unexpected or reordered phases')
    scratch = Path(report['scratch_directory'])
    reference = DirectReference(Path(report['configuration']['reference']), scratch / 'workspace', scratch, None, 'builtin-modules')
    expected_files = {v['path']: {k: v[k] for k in ('sha256', 'size', 'mode')} for v in manifest['files']}
    for phase in check['phases']:
        spec = specification(item, phase['probe'], roots)
        case_root = scratch / 'cases' / item['name']
        shared = item['name'] in ('native.cache-repair', 'native.parallel-distribution') and phase['probe'] != 'standalone'
        expected_cache = case_root / ('native-cache' if shared else phase['probe'] + '-cache')
        expected_registry = (case_root / 'empty-registry' if item['name'] == 'native.no-registry-fallback'
                             else FIXTURE / 'registry')
        if phase['cache'] != str(expected_cache) or phase['registry'] != str(expected_registry):
            raise ValueError('private cache/captured file-registry provenance mismatch')
        if (phase['status'] != 'PASS' or phase.get('error') or not phase['inputs_before']
                or phase['inputs_before'] != phase['inputs_after']
                or phase['registry_before'] != phase['registry_after']):
            raise ValueError('failed phase or changed frozen inputs')
        expected_inputs = {'MODULE.bazel': spec['module'], 'BUILD.bazel': spec['build']}
        if spec['build'] == BUILD:
            expected_inputs['copy.bzl'] = MACRO
        if spec['metadata'] is not None:
            expected_inputs.update({'metadata/MODULE.bazel': spec['metadata'], 'metadata/marker': 'metadata is not repository contents\n',
                                    'metadata/BUILD.bazel': 'exports_files(["marker"])\n'})
        if set(phase['inputs_before']) != {str(reference.workspace / p) for p in expected_inputs}:
            raise ValueError('wrong planned input files')
        for path, text in expected_inputs.items():
            value = phase['inputs_before'][str(reference.workspace / path)]
            if value['kind'] != 'file' or not valid_evidence(value, text.encode()):
                raise ValueError('wrong actual MODULE/BUILD input')
        steps = steps_for(item, phase['engine'])
        if len(phase['runs']) != len(steps):
            raise ValueError('missing planned real invocations')
        records = []
        for index, (run_id, (command, target, extra)) in enumerate(zip(phase['runs'], steps)):
            record = report['runs'][run_id]
            if phase['engine'] == 'native':
                expected = native_command(Path(report['distributed_native']['path']), Path(phase['cache']),
                                          Path(phase['registry']), command, target, spec['selector'], spec['extra'])
                if phase['probe'] == 'standalone':
                    expected = traced_command(expected, scratch / 'native-exec.log')
                    if (phase['exec_trace']['path'] != str(scratch / 'native-exec.log')
                            or phase['exec_trace_result'] != require_native_only_trace(
                                phase['exec_trace'], Path(report['distributed_native']['path']))):
                        raise ValueError('standalone process trace provenance mismatch')
                if record['environment']['PATH'] != report['native_path'] or any('REFERENCE' in k for k in record['environment_keys']):
                    raise ValueError('native process environment provenance mismatch')
            else:
                expected = (reference.startup + [command] + list(COMMON_FLAGS)
                            + (list(BUILD_FLAGS) if command in ('build', 'cquery') else [])
                            + ['--repository_cache=' + str(scratch / 'repository-cache'),
                               '--repo_contents_cache=' + str(scratch / 'repository-contents'),
                               '--registry=' + Path(phase['registry']).as_uri(), '--registry=https://bcr.bazel.build']
                            + extra + [target])
            if (record['argv'] != expected or record['cwd'] != str(reference.workspace)
                    or record['name'] != '.'.join([item['name'], phase['probe'], phase['engine'], str(index)])):
                raise ValueError('run provenance mismatch')
            records.append(record)
            if spec['diagnostic']:
                require_rejection(record, spec['diagnostic'])
            elif command == 'query':
                if phase['query'] != require_query(record, phase['engine']):
                    raise ValueError('query evidence mismatch')
            else:
                success(record)
        if spec['diagnostic']:
            if phase['diagnostic'] != spec['diagnostic'] or phase['publication_entries'] != []:
                raise ValueError('missing pre-publication rejection evidence')
        elif phase['engine'] == 'native':
            tree = phase['tree']
            root = Path(tree['root'])
            if (root.parent != trees_directory(Path(phase['cache'])) or not re.fullmatch('[0-9a-f]{64}', root.name)
                    or tree['files'] != expected_files
                    or tree['directories'] != {p: 0o755 for p in ['.', *expected_directories(manifest)]}
                    or tree['sidecar']['sha256'] != report['sources_before'][str(RESOURCES / 'manifest.json')]['sha256']
                    or tree['entries'] != sorted([root.name, root.name + '.json'])):
                raise ValueError('incomplete manifest tree evidence')
        if item['kind'] == 'differential':
            artifact = phase['artifact']
            if artifact['kind'] != 'file' or not valid_evidence(artifact, helper):
                raise ValueError('missing actual raw-byte artifact')
            paths = (native_paths(records[-1], '//:copy' if item['name'] == 'genuine.helper-and-copy' else CANONICAL, scratch)
                     if phase['engine'] == 'native' else reference_output_paths(success(records[-1]), reference.workspace, reference.output_base))
            if list(map(str, paths)) != [artifact['logical_path']] or not Path(artifact['path']).is_relative_to(scratch):
                raise ValueError('artifact not linked to actual CLI output')
    if item['kind'] == 'differential':
        one, two = [p['artifact'] for p in check['phases']]
        if (base64.b64decode(one['bytes_base64']) != base64.b64decode(two['bytes_base64'])
                or bool(one['mode'] & 0o111) != bool(two['mode'] & 0o111)):
            raise ValueError('genuine raw bytes/executable mode comparison differs')
    if item['name'] in ('native.cache-repair', 'native.parallel-distribution'):
        if len({p['tree']['root'] for p in check['phases'] if p['probe'] != 'standalone'}) != 1:
            raise ValueError('cache identity changed across shared-cache probes')
    if item['name'] == 'native.cache-repair':
        if check['phases'][0]['tree']['publication_stat'] != check['phases'][1]['tree']['publication_stat']:
            raise ValueError('warm demand did not reuse the verified publication')
        for phase in check['phases'][2:]:
            mutation = phase['mutation']
            probe = phase['probe']
            if mutation['probe'] != probe or mutation['before'] == mutation['after']:
                raise ValueError('missing intentional mutation evidence')
            before, after = mutation['before'], mutation['after']
            root = Path(phase['tree']['root'])
            expected_path = (root.parent / (root.name + '.json') if probe == 'missing-manifest'
                             else root / ('unexpected-entry' if probe == 'extra' else 'WORKSPACE'))
            if mutation['path'] != str(expected_path):
                raise ValueError('mutation was not confined to the planned cache entry')
            for value in (before, after):
                if value is not None and value['path'] != str(expected_path):
                    raise ValueError('mutation evidence path mismatch')
            if probe == 'missing-manifest':
                if not valid_evidence(before) or after is not None:
                    raise ValueError('missing sidecar removal receipt')
            elif probe == 'extra':
                if before is not None or not valid_evidence(after, b'intentional extra cache file\n'):
                    raise ValueError('missing extra-file receipt')
            elif (not valid_evidence(before) or not valid_evidence(after)
                  or before['sha256'] != expected_files['WORKSPACE']['sha256'] or before['mode'] != 0o755
                  or (probe == 'bytes' and not valid_evidence(after, b'intentional cache corruption\n'))
                  or (probe == 'mode' and (before['sha256'] != after['sha256'] or before['mode'] != 0o755 or after['mode'] != 0o644))):
                raise ValueError('missing byte/mode corruption receipt')
    if item['name'] == 'native.metadata-not-filesystem':
        before, after = check['phases']
        if after['previous_inputs'] != before['inputs_after']:
            raise ValueError('missing explicit metadata-input transition')
        changed = [p for p in after['previous_inputs'] if after['previous_inputs'][p] != after['inputs_before'][p]]
        if changed != [str(reference.workspace / 'metadata/MODULE.bazel')]:
            raise ValueError('metadata transition changed unexpected inputs')


def report_passes(report):
    try:
        if (report['planned_checks'] != CASES or report['case_inputs_sha256'] != json_digest(CASES)
                or [(c['name'], c['kind']) for c in report['checks']] != PLAN
                or report['errors'] or report['cleanup'] or report['scratch_cleaned'] is not True
                or report['cleanup_receipt'] != {'path': report['scratch_directory'], 'removed': True}
                or report['reference_shutdown_succeeded'] is not True or report['sources_unchanged'] is not True
                or not report['sources_before'] or report['sources_before'] != report['sources_after']
                or report['completed_checks'] != 8 or report['completed_differential_cases'] != 2
                or report['reference_release'] != 'bazel 9.3.0'
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['distributed_native']['sha256'] != report['executables']['native']['sha256']
                or report['path_trap_not_invoked'] is not True):
            return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(record[field + '_base64'], validate=True)
                if (record[field + '_sha256'] != digest(raw) or raw.decode('utf-8', errors='replace') != record[field]
                        or record[field + '_full_sha256'] != digest(raw) or record[field + '_full_size_bytes'] != len(raw)):
                    return False
        scratch = Path(report['scratch_directory'])
        reference = DirectReference(Path(report['configuration']['reference']), scratch / 'workspace', scratch, None, 'builtin-modules')
        if (runs[0]['name'] != 'reference.version' or runs[0]['argv'] != reference.startup + ['version', '--gnu_format']
                or success(runs[0]).strip() != 'bazel 9.3.0' or runs[-1]['name'] != 'builtin-modules.shutdown'
                or runs[-1]['argv'] != reference.startup + ['shutdown'] + list(COMMON_FLAGS)):
            return False
        success(runs[-1])
        helper = base64.b64decode(report['official_helper_base64'], validate=True)
        if digest(helper) != next(v['sha256'] for v in report['manifest']['files'] if v['path'] == HELPER):
            return False
        used = [0, len(runs) - 1]
        for check in report['checks']:
            if check['status'] != 'PASS' or check.get('error') or check['parity_claim'] != (check['kind'] == 'differential'):
                return False
            validate_check(check, report, report['manifest'], report['root_modules'], helper)
            used.extend(i for p in check['phases'] for i in p['runs'])
        return sorted(used) == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def argument_parser():
    parser = common_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/builtin-modules-validation.json')
    return parser


def finish(reference, temporary, report):
    if reference is not None:
        try:
            report['reference_shutdown_attempted'] = True
            reference.shutdown()
            record = report['runs'][-1]
            if record['name'] != 'builtin-modules.shutdown':
                raise ValueError('missing actual shutdown invocation')
            success(record)
            report['reference_shutdown_succeeded'] = not report['cleanup']
        except Exception as exc:
            report['errors'].append('reference shutdown: ' + str(exc))
    if temporary is not None:
        try:
            scratch = Path(temporary.name)
            report['path_trap_not_invoked'] = not (scratch / 'path-trap.log').exists()
            temporary.cleanup()
            report['scratch_cleaned'] = not scratch.exists()
            report['cleanup_receipt'] = dict(path=str(scratch), removed=report['scratch_cleaned'])
        except OSError as exc:
            report['errors'].append('scratch cleanup: ' + str(exc))


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for key in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, key, getattr(args, key).expanduser().resolve())
    if (args.output in (args.native, args.reference) or args.output in source_paths()
            or any(args.output.is_relative_to(ROOT / p) for p in ('src', 'scripts', 'tests', 'resources'))):
        raise ValueError('output must not overwrite sources/resources/fixtures/executables')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
                  planned_checks=CASES, case_inputs_sha256=json_digest(CASES),
                  configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                  scratch_cleaned=False, sources_unchanged=False, reference_shutdown_succeeded=False,
                  reference_shutdown_attempted=False,
                  started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  limitations=[
                      'Eight cases: two genuine raw-byte/executable-mode comparisons; six native boundaries. Not full compatibility.',
                      'Only @bazel_tools versus @@bazel_tools query prefix spelling is normalized; raw labels are retained.',
                      'Native has one explicit file registry and no remote-enabling flag. Reference may download genuine BCR sources.',
                      'PATH trap non-invocation is not proof against absolute-path subprocesses and is not a network sandbox.',
                      'Standalone native query is traced with strace: only its initial exec is permitted; unavailable/failed tracing fails QA.',
                      'Copied ELF runs outside the Rust project with REFERENCE-named environment variables unset; resources are not copied.',
                      'Metadata override is deliberately synthetic test-only metadata, never a replacement production builtin tree.',
                      'The metadata dependency rejection proves metadata consumption; no mod graph or full extension-execution parity claim.',
                      'Source/binary hashes are capture evidence, not independent proof that the supplied ELF was compiled from these sources.',
                      'Durations are diagnostic, not benchmark results; no Bazel test/run commands.'])
    runner = RecordingRunner(report, args.timeout)
    native_runner = RecordingRunner(report, args.timeout)
    temporary = reference = None
    interrupted = False
    try:
        report['sources_before'] = fingerprints(source_paths())
        manifest, roots, helper = load_inputs()
        report.update(manifest=manifest, root_modules=roots, official_helper_base64=base64.b64encode(helper).decode())
        report['executables'] = validate_binaries(args)
        temporary = tempfile.TemporaryDirectory(prefix='builtin-modules-', dir=args.scratch_parent)
        scratch = Path(temporary.name)
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        prepare(workspace, specification(CASES[0], 'empty-root', roots))
        bin_dir = scratch / 'bin'
        bin_dir.mkdir()
        args.distributed_native = bin_dir / 'native-bazel'
        shutil.copy2(args.native, args.distributed_native)
        report['distributed_native'] = fingerprint(args.distributed_native)
        for name in ('bazel', 'bazelisk'):
            trap = bin_dir / name
            trap.write_text('#!/bin/sh\nprintf "PATH trap invoked\\n" >> ' + str(scratch / 'path-trap.log') + '\nexit 97\n')
            trap.chmod(0o755)
        # Do not retain arbitrary user PATH entries (including reference wrappers).
        # Only shell utilities needed by the trusted root genrule are exposed.
        for name, path in (('cat', '/bin/cat'), ('sh', '/bin/sh')):
            (bin_dir / name).symlink_to(path)
        native_runner.env = {k: v for k, v in native_runner.env.items() if 'REFERENCE' not in k}
        native_runner.env['PATH'] = str(bin_dir)
        report['native_path'] = str(bin_dir)
        reference = DirectReference(args.reference, workspace, scratch, runner, 'builtin-modules')
        reference.started = True
        version = runner.run('reference.version', reference.startup + ['version', '--gnu_format'], workspace)
        report['reference_release'] = success(version).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('reference version mismatch')
        for item in CASES:
            try:
                run_case(args, runner, native_runner, scratch, reference, item, manifest, roots, helper)
            except Exception as exc:
                report['errors'].append(item['name'] + ': ' + type(exc).__name__ + ': ' + str(exc))
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__ + ': ' + str(exc))
    finally:
        finish(reference, temporary, report)
        try:
            report['sources_after'] = fingerprints(source_paths())
            report['sources_unchanged'] = report.get('sources_before') == report['sources_after']
        except OSError as exc:
            report['errors'].append('source evidence: ' + str(exc))
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['completed_differential_cases'] = sum(c['status'] == 'PASS' and c['kind'] == 'differential' for c in report['checks'])
        if report_passes(report):
            report['status'] = 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        write_report(args.output, report)
    print('{} (scoped built-in modules only): {}'.format(report['status'], args.output))
    return 130 if interrupted else 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
