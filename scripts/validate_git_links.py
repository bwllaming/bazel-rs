#!/usr/bin/env python3
"""Scoped Git FILE-symlink tree fidelity, not general action symlink parity.

Unit tests run private real Git only. Running this script explicitly invokes the
native CLI and pinned genuine Bazel. Some diagnostic patterns have captured
regressions; new policy probes still require actual CLI evidence, never mocks.
"""

import base64
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
import tempfile

import validate_git as shared
from validate import ROOT, COMMON_FLAGS, BUILD_FLAGS, file_evidence, reference_output_paths, write_report
from validate_patches import (PatchReference, RecordingRunner, REFERENCE_SHA256, PROFILE,
                              digest, evidence_matches, native_command, patch_text,
                              snapshot, validate_binaries)


LINK = 'e2e/smoke/.bazelversion'
RAW = '../../.bazelversion'
VERSION = b'9.3.0\n'
PATCHED_VERSION = b'9.3.0-patched\n'
OUTSIDE = b'private outside sentinel: must not be read through a link or changed\n'
MALFORMED_LINK = 'malformedlink'
MALFORMED_TARGET = b'data\0../outside'
MALFORMED_PAYLOAD = 'raw-target'
SOURCES = [ROOT / 'scripts' / name for name in (
    'validate_git_links.py', 'validate_git.py', 'validate_patches.py',
    'validate_modules.py', 'validate.py')] + [ROOT / 'tests/test_validate_git_links.py']


def case(name, mode='source', diagnostic=None, **options):
    return dict(name=name, mode=mode, diagnostic=diagnostic,
                kind='native_only' if name.startswith('native.') else 'differential', **options)


CASES = [
    case('materialize.distroless-shaped-link'),
    case('materialize.root-build-link', shape='build'),
    case('materialize.two-link-chain', shape='chain'),
    case('materialize.safe-strip-prefix', prefix='nested/pkg'),
    case('native.offline-warm-link', mode='warm'),
    case('native.cache-kind-target-and-referent-repair', mode='repair'),
    *[case('native.reject-' + shape, diagnostic=shape, shape=shape)
      for shape in ('absolute', 'escape', 'dangling', 'cycle', 'directory')],
    case('native.nonexistent-intermediate-dotdot', diagnostic='dangling', shape='trap'),
    case('native.prefix-containment', diagnostic='escape', shape='prefix_escape', prefix='nested/pkg'),
    case('native.patch-regular-referent', patch='referent'),
    case('native.patch-delete-referent', diagnostic='dangling', patch='delete'),
    # PatchUtil replaces a patched link leaf with a regular file (referent kept).
    case('native.patch-symlink-leaf', patch='link'),
    case('native.root-module-link', diagnostic='module_link', shape='module'),
    case('native.selected-consumer-boundaries', mode='consumers', shape='consumers'),
    case('native.reject-committed-nul-target', diagnostic='malformed_target', shape='nul_blob'),
]
PLAN = [(value['name'], value['kind']) for value in CASES]
DIAGNOSTICS = {
    'absolute': r'(?:symlink|symbolic link)[^\n]*absolute',
    'escape': r'(?:symlink|symbolic link)[^\n]*(?:escap|outside|external|above[^\n]*root)',
    'dangling': r'(?:symlink|symbolic link)[^\n]*(?:dangling|missing|nonexistent|does not exist|not found)',
    'cycle': r'(?:symlink|symbolic link)[^\n]*(?:cycle|cyclic|loop|too many|hop limit)',
    'directory': r'(?:symlink|symbolic link)[^\n]*(?:directory target|targets? (?:a )?director|resolves? to (?:a )?director)',
    'module_link': (r'(?<![\w./])MODULE\.bazel[^\n]*(?:symlink|symbolic link)[^\n]*(?:unsupported|not supported|forbidden|refus|cannot)'
                    r'|(?m:^bazel-rs: Git file is not regular \(symlinks/special files forbidden\): /[^\n]*/MODULE\.bazel$)'),
    # New native-only contract: require the malformed committed entry identity,
    # target semantics and explicit unsafe/control/NUL reason on the SAME line.
    'malformed_target': (r'(?m:^(?=[^\n]*(?:symlink|symbolic link))(?=[^\n]*\btarget\b)'
                         r'(?=[^\n]*\bmalformedlink\b)(?=[^\n]*(?:\bunsafe\b|\bNUL\b|control (?:character|byte)|target contains a forbidden character))'
                         r'[^\n]+$)'),
}
UNRELATED = re.compile(shared.UNRELATED.pattern + r'|syntax error|parse error|unexpected token|\bfatal:', re.I)


def require_rejection(record, diagnostic):
    code, text = record['returncode'], record['stderr']
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record['timed_out']
            or record.get('spawn_error') or record.get('interrupted') or record['stdout'].strip()
            or UNRELATED.search(text) or not re.search(DIAGNOSTICS[diagnostic], text, re.I)):
        raise ValueError('expected controlled Git link rejection: ' + diagnostic)


def entry_evidence(path):
    """lstat before every read; link metadata contains RAW target, never referent bytes."""
    info = path.lstat()
    value = {'path': str(path), 'mode': stat.S_IMODE(info.st_mode)}
    if stat.S_ISLNK(info.st_mode):
        raw = os.readlink(os.fsencode(path))
        value.update(kind='symlink', raw_target=os.fsdecode(raw),
                     target_base64=base64.b64encode(raw).decode('ascii'),
                     target_size_bytes=len(raw), target_sha256=digest(raw))
    elif stat.S_ISREG(info.st_mode):
        # Even a leaf swapped after lstat must not turn this into a following read.
        with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
            raw = stream.read()
        value.update(kind='file', bytes_base64=base64.b64encode(raw).decode('ascii'),
                     size_bytes=len(raw), sha256=digest(raw))
    elif stat.S_ISDIR(info.st_mode):
        value.update(kind='directory')
    else:
        raise ValueError('unsupported inventory entry: ' + str(path))
    return value


def inventory(root, *, origin=False):
    """Complete recursive inventory, never descending through directory symlinks."""
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError('inventory root must be a real directory')
    result = {}

    def visit(directory):
        for path in sorted(directory.iterdir()):
            name = path.relative_to(root).as_posix()
            if origin and name == '.git':
                continue
            value = entry_evidence(path)
            result[name] = value
            if value['kind'] == 'directory':
                visit(path)

    visit(root)
    return result


def valid_entry(value, kind=None, contents=None):
    try:
        if kind is not None and value['kind'] != kind:
            return False
        if not isinstance(value['path'], str) or type(value['mode']) is not int:
            return False
        if value['kind'] == 'file':
            # The reference may make its cache read-only; Git tracks the
            # executable bit, not owner-write permission. These fixtures are all
            # non-executable regular files.
            return value['mode'] in (0o444, 0o644) and evidence_matches(value, contents)
        if value['kind'] == 'symlink':
            raw = base64.b64decode(value['target_base64'], validate=True)
            return (value['mode'] == 0o777 and os.fsencode(value['raw_target']) == raw
                    and value['target_size_bytes'] == len(raw) and value['target_sha256'] == digest(raw)
                    and 'bytes_base64' not in value and 'sha256' not in value
                    and (contents is None or raw == contents))
        return value['kind'] == 'directory' and contents is None and 0 <= value['mode'] <= 0o777
    except (KeyError, TypeError, ValueError):
        return False


def layout(item, case_root, *, patched=False):
    """Declarative known inputs; no reading any Git origin (especially unsafe links)."""
    files = {'MODULE.bazel': ('file', shared.MODULE.encode()),
             'BUILD.bazel': ('file', shared.BUILD.encode()), 'data': ('file', shared.data()),
             '.bazelversion': ('file', PATCHED_VERSION if patched and item.get('patch') == 'referent' else VERSION),
             '.bazelversion.same': ('file', VERSION), LINK: ('symlink', RAW.encode())}
    if patched and item.get('patch') == 'link':
        # Bazel's PatchUtil deletes the link and writes a fresh regular file.
        files[LINK] = ('file', PATCHED_VERSION)
    shape = item.get('shape')
    if shape == 'build':
        files['BUILD.real'] = files['BUILD.bazel']
        files['BUILD.bazel'] = ('symlink', b'BUILD.real')
    elif shape == 'chain':
        files[LINK] = ('symlink', b'./version-link')
        files['e2e/smoke/version-link'] = ('symlink', b'../.././.bazelversion')
    elif shape == 'absolute':
        files[LINK] = ('symlink', os.fsencode(case_root / 'outside-marker'))
    elif shape == 'escape':
        files[LINK] = ('symlink', b'../../../outside-marker')
    elif shape in ('dangling', 'trap'):
        files[LINK] = ('symlink', b'../../missing' if shape == 'dangling' else b'../../missing/../.bazelversion')
    elif shape == 'cycle':
        files[LINK] = ('symlink', b'loop')
        files['e2e/smoke/loop'] = ('symlink', b'.bazelversion')
    elif shape == 'directory':
        files[LINK] = ('symlink', b'../smoke')
    elif shape == 'prefix_escape':
        # Valid in the full checkout, invalid in the selected root. Keep the raw
        # target unchanged: lexical rewriting would hide the containment defect.
        files[LINK] = ('symlink', b'../../../outside.txt')
    elif shape == 'module':
        files['MODULE.real'] = files['MODULE.bazel']
        files['MODULE.bazel'] = ('symlink', b'MODULE.real')
    elif shape == 'consumers':
        files.update({'defs.real.bzl': ('file', b'value = "unused"\n'),
                      'bzl/native.bzl': ('symlink', b'../defs.real.bzl'),
                      'bzl/BUILD.bazel': ('file', b'load(":native.bzl", "value")\nfilegroup(name = "selected", srcs = [])\n'),
                      'artifact/link': ('symlink', b'../data'),
                      'artifact/BUILD.bazel': ('file', b'filegroup(name = "selected", srcs = ["link"])\n')})
    return files


def full_layout(item, case_root):
    prefix = item.get('prefix', '')
    result = {(prefix + '/' if prefix else '') + name: value for name, value in layout(item, case_root).items()}
    if prefix:
        result.update({'MODULE.bazel': ('file', b'module(name = "unselected_root")\n'),
                       'nested/outside.txt': ('file', b'outside selected prefix\n')})
    return result


def committed_layout(item, case_root):
    """The NUL-containing link exists ONLY as a committed mode-120000 blob.

    Git checkout can truncate it to a valid 'data' link; creating a filesystem
    link would lose the adversarial bytes (and os.symlink rightly rejects NUL).
    """
    result = full_layout(item, case_root)
    if item.get('shape') == 'nul_blob':
        result[MALFORMED_LINK] = ('symlink', MALFORMED_TARGET)
    return result


def patch_contents(item):
    action = item.get('patch')
    if action == 'referent':
        return patch_text('.bazelversion', VERSION.decode(), PATCHED_VERSION.decode())
    if action == 'delete':
        return patch_text('.bazelversion', VERSION.decode(), '')
    if action == 'link':
        return patch_text(LINK, VERSION.decode(), PATCHED_VERSION.decode())
    return ''


def expanded_layout(files):
    result = dict(files)
    for name in files:
        for parent in Path(name).parents:
            if str(parent) != '.':
                result[parent.as_posix()] = ('directory', None)
    return result


def manifest_matches(value, files, root):
    expected = expanded_layout(files)
    return (set(value) == set(expected) and all(
        valid_entry(value[name], kind, contents) and value[name]['path'] == str(root / name)
        for name, (kind, contents) in expected.items()))


def materialized_evidence(root, item, case_root):
    files = layout(item, case_root, patched=True)
    tree = inventory(root)
    if not manifest_matches(tree, files, root):
        raise ValueError('published tree differs in entry kind, raw target, bytes, or complete inventory')
    reads = {}
    # Only now, for positive fixtures whose entire metadata has matched, read
    # through each expected internal leaf. Never use this on malicious origins.
    for name, (kind, _) in files.items():
        if kind != 'symlink':
            continue
        path = root / name
        resolved = path.resolve(strict=True)
        if not resolved.is_relative_to(root.resolve()) or not stat.S_ISREG(resolved.lstat().st_mode):
            raise ValueError('positive fixture link did not resolve to an internal regular file')
        referent = resolved.relative_to(root.resolve()).as_posix()
        expected = files[referent]
        if expected[0] != 'file':
            raise ValueError('unproven link referent')
        reads[name] = dict(file_evidence(path, expected[1], include_bytes=True), referent=referent)
    return {'root': str(root), 'entries': tree, 'link_reads': reads}


def tree_evidence_matches(value, item, case_root):
    root = Path(value['root'])
    files = layout(item, case_root, patched=True)
    links = {name for name, (kind, _) in files.items() if kind == 'symlink'}
    if not manifest_matches(value['entries'], files, root) or set(value['link_reads']) != links:
        return False
    for name in links:
        read = value['link_reads'][name]
        # Known positive fixture chains; compare the actual terminal identity too.
        referent = ('BUILD.real' if name == 'BUILD.bazel' else 'defs.real.bzl' if name == 'bzl/native.bzl'
                    else 'data' if name == 'artifact/link' else '.bazelversion')
        if (read['path'] != str(root / name) or read['referent'] != referent
                or not evidence_matches(read, files[referent][1])):
            return False
    return True


def setup_commands(commit=None):
    return {'init': ['init', '--initial-branch=fixture-main', '--object-format=sha1', '.'],
            'add': ['add', '--all', '--', '.'],
            'commit': ['commit', '--no-gpg-sign', '-m', 'pinned real symlink fixture'],
            'sha': ['rev-parse', '--verify', 'HEAD'],
            'index': ['ls-files', '--stage', '-z'],
            'tree': ['ls-tree', '-rz', commit or 'HEAD']}


def blob_oid(raw):
    return hashlib.sha1(b'blob ' + str(len(raw)).encode() + b'\0' + raw).hexdigest()


def malformed_commands(commit=None):
    oid = blob_oid(MALFORMED_TARGET)
    pin = commit or 'HEAD'
    return {'hash-blob': ['hash-object', '-w', '--no-filters', '--', MALFORMED_PAYLOAD],
            'index-blob': ['update-index', '--add', '--cacheinfo', '120000,' + oid + ',' + MALFORMED_LINK],
            'commit-malformed': ['commit', '--no-gpg-sign', '-m', 'pinned malformed symlink blob'],
            'sha-malformed': ['rev-parse', '--verify', 'HEAD'],
            'parent-malformed': ['rev-parse', '--verify', pin + '^'],
            'index-malformed': ['ls-files', '--stage', '-z'],
            'tree-malformed': ['ls-tree', '-rz', pin],
            'blob-size': ['cat-file', '-s', oid],
            'blob-bytes': ['cat-file', 'blob', oid]}


def malformed_blob_evidence(raw):
    return dict(kind='git_symlink_blob', path=MALFORMED_LINK, git_mode='120000',
                object_id=blob_oid(raw), size_bytes=len(raw), sha256=digest(raw),
                bytes_base64=base64.b64encode(raw).decode('ascii'))


def malformed_input_paths(origin, check):
    git = origin / '.git'
    return [git / name for name in ('index', 'HEAD', 'refs/heads/fixture-main')] + [
        git / 'objects' / oid[:2] / oid[2:]
        for oid in (blob_oid(MALFORMED_TARGET), check['base_commit'], check['commit'])]


def git_argv(case_root, command):
    setup = case_root / 'git-setup'
    return [str(shared.GIT), '-c', 'core.hooksPath=' + str(setup / 'hooks'),
            '-c', 'init.templateDir=' + str(setup / 'template'), *command]


def tracked_entries(text, tree=False):
    result = {}
    if not text.endswith('\0'):
        raise ValueError('Git inventory is not NUL terminated')
    for entry in text[:-1].split('\0'):
        metadata, name = entry.split('\t', 1)
        fields = metadata.split(' ')
        mode, sha = (fields[0], fields[2]) if tree else (fields[0], fields[1])
        if (len(fields) != 3 or fields[1 if tree else 2] != ('blob' if tree else '0')
                or name in result or not re.fullmatch('[0-9a-f]{40}', sha)):
            raise ValueError('unexpected Git tracked entry')
        result[name] = (mode, sha)
    return result


def expected_tracked(files):
    return {name: ('120000' if kind == 'symlink' else '100644',
                   blob_oid(raw))
            for name, (kind, raw) in files.items()}


def attributes(item, origin, commit):
    result = {'module_name': shared.DEP, 'remote': origin.as_uri(), 'commit': commit,
              'patch_strip': 1, 'patches': ['//:git-local.patch'] if item.get('patch') else []}
    if item.get('prefix'):
        result['strip_prefix'] = item['prefix']
    return result


def materialize(case_root, workspace, item, runner, check):
    origin = case_root / 'origin'
    origin.mkdir(parents=True)
    marker = case_root / 'outside-marker'
    marker.write_bytes(OUTSIDE)
    files = full_layout(item, case_root)
    for name, (kind, raw) in files.items():
        path = origin / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if kind == 'symlink':
            os.symlink(os.fsdecode(raw), path)
        else:
            path.write_bytes(raw)
    env = shared.private_environment(case_root / 'git-setup')
    check.update(git_environment=env, git_setup_run_ids={})

    def git(phase, command):
        with shared.environment(runner, env):
            record = runner.run(item['name'] + '.git.' + phase, git_argv(case_root, command), origin, timeout=120)
        check['git_setup_run_ids'][phase] = record['id']
        return shared.success(record)

    for phase in setup_commands():
        text = git(phase, setup_commands(check.get('commit'))[phase])
        if phase == 'sha':
            check['commit'] = text.strip()
            if not re.fullmatch('[0-9a-f]{40}', check['commit']):
                raise ValueError('fixture commit is not a full SHA1')
        if phase in ('index', 'tree') and tracked_entries(text, phase == 'tree') != expected_tracked(files):
            raise ValueError('Git did not register the complete fixture as real blobs/120000 links')
    if item.get('shape') == 'nul_blob':
        check['base_commit'] = check['commit']
        payload = origin / MALFORMED_PAYLOAD
        payload.write_bytes(MALFORMED_TARGET)
        try:
            check['malformed_payload'] = entry_evidence(payload)
            for phase in malformed_commands():
                text = git(phase, malformed_commands(check['commit'])[phase])
                if phase == 'hash-blob':
                    if text.strip() != blob_oid(MALFORMED_TARGET):
                        raise ValueError('malformed Git blob OID mismatch')
                    payload.unlink()  # Never tracked, never present at the native invocation.
                elif phase == 'sha-malformed':
                    check['commit'] = text.strip()
                    if not re.fullmatch('[0-9a-f]{40}', check['commit']) or check['commit'] == check['base_commit']:
                        raise ValueError('malformed Git object did not produce a distinct full commit')
                elif phase == 'parent-malformed' and text.strip() != check['base_commit']:
                    raise ValueError('malformed commit lost its proven base commit')
                elif phase in ('index-malformed', 'tree-malformed'):
                    if tracked_entries(text, phase == 'tree-malformed') != expected_tracked(committed_layout(item, case_root)):
                        raise ValueError('malformed link was not committed as the exact mode-120000 blob')
                elif phase == 'blob-size' and text.strip() != str(len(MALFORMED_TARGET)):
                    raise ValueError('malformed Git blob size mismatch')
                elif phase == 'blob-bytes':
                    raw = text.encode('utf-8')  # ASCII plus NUL is lossless in Runner stdout.
                    if raw != MALFORMED_TARGET:
                        raise ValueError('malformed Git blob was truncated or changed')
                    check['malformed_blob'] = malformed_blob_evidence(raw)
        finally:
            if payload.exists():
                payload.unlink()
        check['malformed_payload_removed'] = not payload.exists()
        check['malformed_link_worktree_absent'] = not os.path.lexists(origin / MALFORMED_LINK)
        if not check['malformed_link_worktree_absent']:
            raise ValueError('malformed target must remain index-only, not an OS symlink')
    registry = case_root / 'registry'
    registry.mkdir()
    workspace.mkdir(parents=True, exist_ok=True)
    attrs = attributes(item, origin, check['commit'])
    owned = {workspace / 'MODULE.bazel': shared.root_module(attrs).encode(),
             workspace / 'BUILD.bazel': b'', workspace / 'git-local.patch': patch_contents(item).encode(),
             registry / 'bazel_registry.json': b'{}\n', marker: OUTSIDE}
    for path, raw in owned.items():
        path.write_bytes(raw)
    check['override'] = attrs
    return dict(origin=origin, registry=registry, owned=owned, marker=marker,
                git_inputs=malformed_input_paths(origin, check) if item.get('shape') == 'nul_blob' else [])


def inputs_snapshot(fixture):
    return {'origin': inventory(fixture['origin'], origin=True),
            'owned': {str(path): entry_evidence(path) for path in fixture['owned']},
            'git_inputs': {str(path): entry_evidence(path) for path in fixture['git_inputs']}}


def tamper_cache(root, probe):
    """Deliberate owned-cache mutations: unlink the link, NEVER write through it."""
    path = root / LINK
    if probe in ('retarget', 'regular'):
        if not stat.S_ISLNK(path.lstat().st_mode):
            raise ValueError('tamper precondition: expected original symlink')
        path.unlink()
        if probe == 'retarget':
            os.symlink('../../.bazelversion.same', path)
        else:
            with path.open('xb') as stream:
                stream.write(VERSION)
    elif probe == 'referent':
        path = root / '.bazelversion'
        if not stat.S_ISREG(path.lstat().st_mode):
            raise ValueError('tamper precondition: expected regular referent')
        with os.fdopen(os.open(path, os.O_WRONLY | os.O_TRUNC | os.O_NOFOLLOW), 'wb') as stream:
            stream.write(b'corrupt referent\n')
    else:
        raise ValueError('unknown cache probe')
    return entry_evidence(path)


def phases(item):
    if item['diagnostic']:
        return ['result']
    result = ['query', 'sources', 'copy']
    if item['mode'] == 'warm':
        result += ['warm.sources', 'warm.copy']
    if item['mode'] == 'repair':
        result += [probe + '.' + phase for probe in ('retarget', 'regular', 'referent') for phase in ('sources', 'copy')]
    if item['mode'] == 'consumers':
        result += ['bzl', 'artifact']
    return result


def invocation(phase):
    if phase == 'result':
        return 'mod', None
    if phase == 'bzl':
        return 'query', '@gitdep//bzl:all'
    if phase.startswith('artifact'):
        return 'cquery' if phase.endswith('.files') else 'build', ARTIFACT
    if phase == 'query':
        return 'query', '@gitdep//:all'
    return ('cquery' if phase.endswith('.files') else 'build',
            '@gitdep//:' + ('sources' if 'sources' in phase else 'copy'))


ARTIFACT = '@gitdep//artifact:selected'
BZL_TARGETS = ['@gitdep//bzl:selected']


def bzl_result(record, native=False):
    """N26: a .bzl loaded through an internal Git link (bzl/native.bzl ->
    ../defs.real.bzl) loads in Bazel (its FileValue follows links) and natively
    (in-root source_links resolution), so the package query must succeed."""
    labels = shared.success(record).splitlines()
    if (sorted(map(shared.canonical, labels)) != list(map(shared.canonical, BZL_TARGETS))
            or (native and labels != list(map(shared.canonical, labels)))):
        raise ValueError('query through the linked .bzl did not return exactly the fixture target')


def artifact_path_ok(engine, path, scratch, item, output_base):
    """The filegroup's one source is artifact/link -> ../data.

    Native lists the resolved, symlink-free referent in its own Git cache; the
    reference cquery lists the link's own path under its execroot.
    """
    if engine == 'native':
        trees = shared.trees_directory(scratch / 'native' / item['name'])
        return path.name == 'data' and path.is_absolute() and '..' not in path.parts and path.is_relative_to(trees)
    return path == output_base / 'execroot/_main/external' / (shared.DEP + '+') / 'artifact/link'


def artifact_manifest(engine, paths, scratch, item, output_base):
    if len(paths) != 1 or not artifact_path_ok(engine, paths[0], scratch, item, output_base):
        raise ValueError('linked source artifact has the wrong identity: ' + engine)
    return {'link': file_evidence(paths[0], shared.data(), include_bytes=True)}


def expected_argv(report, item, key):
    scratch = Path(report['scratch_directory'])
    engine, phase = key.split('.', 1)
    command, target = invocation(phase)
    registry = scratch / 'cases' / item['name'] / 'registry'
    if engine == 'native':
        return native_command(Path(report['configuration']['native']), scratch / 'native' / item['name'], registry, command, target)
    ref = PatchReference(Path(report['configuration']['reference']), scratch / 'workspace', scratch, None, 'git-links')
    ref.registry = registry
    return (ref.startup + [command] + list(COMMON_FLAGS)
            + (list(BUILD_FLAGS) if command in ('build', 'cquery') else []) + ref.flags()
            + (['--output=files'] if command == 'cquery' else []) + [target])


def run_case(args, runner, scratch, workspace, reference, item):
    check = dict(name=item['name'], kind=item['kind'], input=item, status='FAIL', parity_claim=False,
                 run_ids={}, artifacts={}, trees={}, tampering={})
    runner.report['checks'].append(check)
    case_root = scratch / 'cases' / item['name']
    cache = scratch / 'native' / item['name']
    fixture = None

    def invoke(engine, phase):
        command, target = invocation(phase)
        name = item['name'] + '.' + engine + '.' + phase
        result = (runner.run(name, native_command(args.native, cache, fixture['registry'], command, target), workspace)
                  if engine == 'native' else reference.run(name, command,
                      *(['--output=files', target] if command == 'cquery' else [target])))
        check['run_ids'][engine + '.' + phase] = result['id']
        return result

    def build(engine, phase):
        source = phase.endswith('sources')
        result = invoke(engine, phase)
        if engine == 'native':
            paths = shared.native_paths(result, invocation(phase)[1], scratch)
        else:
            shared.success(result)
            paths = reference_output_paths(shared.success(invoke(engine, phase + '.files')), workspace, reference.output_base)
        expected = {'data': shared.data(), 'MODULE.bazel': shared.MODULE.encode()} if source else {'copy.txt': shared.data()}
        root = None
        if source:
            root = (shared.reference_source_root(paths, reference.output_base) if engine == 'reference' else paths[0].parent)
            if engine == 'native' and not root.resolve().is_relative_to(shared.trees_directory(cache).resolve()):
                raise ValueError('source artifact outside owned native Git cache')
            check['trees'][engine + '.' + phase] = materialized_evidence(root, item, case_root)
        check['artifacts'][engine + '.' + phase] = shared.output_manifest(paths, expected, source)
        return root

    def artifact(engine):
        # A source artifact through an internal Git link builds in Bazel and
        # natively, reading the referent's bytes.
        result = invoke(engine, 'artifact')
        if engine == 'native':
            paths = shared.native_paths(result, ARTIFACT, scratch)
        else:
            shared.success(result)
            paths = reference_output_paths(shared.success(invoke(engine, 'artifact.files')), workspace, reference.output_base)
        check['artifacts'][engine + '.artifact'] = artifact_manifest(engine, paths, scratch, item, reference.output_base)

    try:
        fixture = materialize(case_root, workspace, item, runner, check)
        check['inputs_before'] = inputs_snapshot(fixture)
        reference.registry = fixture['registry']
        if item['diagnostic']:
            require_rejection(invoke('native', 'result'), item['diagnostic'])
            shared.no_publication(cache)
            check['no_publication'] = True
        else:
            for engine in ('native', 'reference') if item['kind'] == 'differential' else ('native',):
                shared.query_result(invoke(engine, 'query'), native=engine == 'native')
                root = build(engine, 'sources')
                build(engine, 'copy')
                if item['mode'] == 'warm':
                    hidden = fixture['origin'].with_name('origin-unavailable')
                    fixture['origin'].rename(hidden)
                    try:
                        check['origin_unavailable'] = not fixture['origin'].exists()
                        build(engine, 'warm.sources')
                        build(engine, 'warm.copy')
                    finally:
                        hidden.rename(fixture['origin'])
                if item['mode'] == 'repair':
                    for probe in ('retarget', 'regular', 'referent'):
                        check['tampering'][probe] = tamper_cache(root, probe)
                        repaired = build(engine, probe + '.sources')
                        build(engine, probe + '.copy')
                        if repaired != root:
                            raise ValueError('repair did not restore the same immutable cache tree')
                if item['mode'] == 'consumers':
                    bzl_result(invoke(engine, 'bzl'), native=engine == 'native')
                    artifact(engine)
            if item['mode'] == 'consumers' and item['kind'] != 'differential':
                # Reference comparison for the linked .bzl and source-artifact phases.
                bzl_result(invoke('reference', 'bzl'))
                artifact('reference')
        check.update(status='PASS', parity_claim=item['kind'] == 'differential')
    except (OSError, ValueError) as exc:
        check['error'] = str(exc)
    finally:
        if fixture is not None:
            try:
                check['inputs_after'] = inputs_snapshot(fixture)
                check['inputs_unchanged'] = check['inputs_before'] == check['inputs_after']
                check['outside_marker_unchanged'] = entry_evidence(fixture['marker']) == check['inputs_before']['owned'][str(fixture['marker'])]
                if not check['inputs_unchanged'] or not check['outside_marker_unchanged']:
                    raise ValueError('fixture or private outside sentinel changed')
            except (OSError, ValueError, KeyError) as exc:
                check.update(status='FAIL', parity_claim=False, error=str(exc))


def report_passes(report):
    """Offline strict evidence revalidation; no synthetic report is CLI evidence."""
    try:
        if ([(c['name'], c['kind']) for c in report['checks']] != PLAN or report['errors'] or report['cleanup']
                or report['schema_version'] != 1 or report['test_commands_executed'] is not False
                or report['build_commands_executed'] is not True
                or report['planned_checks'] != CASES
                or report['case_inputs_sha256'] != digest(json.dumps(CASES, sort_keys=True).encode())
                or any(report[key] is not True for key in ('scratch_cleaned', 'reference_shutdown_attempted', 'sources_unchanged'))
                or report['sources_before'] != report['sources_after']
                or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(evidence_matches(value) and value['path'] == name for name, value in report['sources_before'].items())
                or report['reference_release'] != 'bazel 9.3.0'
                or report['reference_sha256_required'] != REFERENCE_SHA256
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['completed_checks'] != len(CASES) or report['completed_differential_cases'] != 4):
            return False
        for name, value in report['executables'].items():
            if (name not in ('native', 'reference', 'git') or not re.fullmatch('[0-9a-f]{64}', value['sha256'])
                    or type(value['size_bytes']) is not int or value['size_bytes'] <= 0
                    or value['path'] != (str(shared.GIT) if name == 'git' else report['configuration'][name])):
                return False
        if set(report['executables']) != {'native', 'reference', 'git'}:
            return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            for field in ('stdout', 'stderr'):
                raw = (base64.b64decode(record[field + '_base64'], validate=True) if field + '_base64' in record else record[field].encode())
                if digest(raw) != record[field + '_sha256'] or raw.decode('utf-8', errors='replace') != record[field]:
                    return False
        used = set()
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        ref = PatchReference(Path(report['configuration']['reference']), workspace, scratch, None, 'git-links')

        def linked(index, name, argv, cwd, timeout=None):
            if type(index) is not int or index < 0 or index in used:
                raise ValueError('invalid or reused run ID')
            used.add(index)
            value = runs[index]
            if (value['name'] != name or value['argv'] != argv or value['cwd'] != str(cwd)
                    or value['timeout_seconds'] != (report['configuration']['timeout'] if timeout is None else timeout)):
                raise ValueError('run wiring mismatch')
            return value

        version = linked(0, 'reference.version', ref.startup + ['version', '--gnu_format'], workspace)
        if shared.success(version).strip() != 'bazel 9.3.0':
            return False
        shared.success(linked(len(runs) - 1, 'git-links.shutdown', ref.startup + ['shutdown'] + list(COMMON_FLAGS),
                              workspace, min(report['configuration']['timeout'], 15)))
        for item, check in zip(CASES, report['checks']):
            case_root = scratch / 'cases' / item['name']
            origin = case_root / 'origin'
            if (check['input'] != item or check['status'] != 'PASS' or check.get('error')
                    or check['parity_claim'] != (item['kind'] == 'differential')
                    or check['inputs_unchanged'] is not True or check['outside_marker_unchanged'] is not True
                    or check['inputs_before'] != check['inputs_after']
                    or not manifest_matches(check['inputs_before']['origin'], full_layout(item, case_root), origin)
                    or not re.fullmatch('[0-9a-f]{40}', check['commit'])):
                return False
            attrs = attributes(item, origin, check['commit'])
            owned = {workspace / 'MODULE.bazel': shared.root_module(attrs).encode(), workspace / 'BUILD.bazel': b'',
                     workspace / 'git-local.patch': patch_contents(item).encode(),
                     case_root / 'registry/bazel_registry.json': b'{}\n', case_root / 'outside-marker': OUTSIDE}
            if (check['override'] != attrs or set(check['inputs_before']['owned']) != set(map(str, owned))
                    or not all(valid_entry(check['inputs_before']['owned'][str(path)], 'file', raw)
                               and check['inputs_before']['owned'][str(path)]['path'] == str(path) for path, raw in owned.items())):
                return False
            malformed = item.get('shape') == 'nul_blob'
            base = check['base_commit'] if malformed else check['commit']
            commands = setup_commands(base)
            if malformed:
                commands.update(malformed_commands(check['commit']))
                if (not re.fullmatch('[0-9a-f]{40}', base) or base == check['commit']
                        or check['malformed_blob'] != malformed_blob_evidence(MALFORMED_TARGET)
                        or check['malformed_payload_removed'] is not True
                        or check['malformed_link_worktree_absent'] is not True
                        or not valid_entry(check['malformed_payload'], 'file', MALFORMED_TARGET)
                        or check['malformed_payload']['path'] != str(origin / MALFORMED_PAYLOAD)):
                    return False
            elif any(key in check for key in ('base_commit', 'malformed_blob', 'malformed_payload',
                                             'malformed_payload_removed', 'malformed_link_worktree_absent')):
                return False
            git_inputs = check['inputs_before']['git_inputs']
            wanted_inputs = malformed_input_paths(origin, check) if malformed else []
            if (set(git_inputs) != set(map(str, wanted_inputs))
                    or not all(valid_entry(value, 'file') and value['path'] == path for path, value in git_inputs.items())):
                return False
            if malformed and not evidence_matches(git_inputs[str(origin / '.git/refs/heads/fixture-main')], (check['commit'] + '\n').encode()):
                return False
            if set(check['git_setup_run_ids']) != set(commands):
                return False
            for phase, command in commands.items():
                record = linked(check['git_setup_run_ids'][phase], item['name'] + '.git.' + phase,
                                git_argv(case_root, command), origin, 120)
                text = shared.success(record)
                if phase in ('sha', 'parent-malformed') and text.strip() != base:
                    return False
                if phase == 'sha-malformed' and text.strip() != check['commit']:
                    return False
                if phase in ('index', 'tree') and tracked_entries(text, phase == 'tree') != expected_tracked(full_layout(item, case_root)):
                    return False
                if phase in ('index-malformed', 'tree-malformed') and tracked_entries(text, phase == 'tree-malformed') != expected_tracked(committed_layout(item, case_root)):
                    return False
                if phase == 'hash-blob' and text.strip() != blob_oid(MALFORMED_TARGET):
                    return False
                if phase == 'blob-size' and text.strip() != str(len(MALFORMED_TARGET)):
                    return False
                if phase == 'blob-bytes' and text.encode('utf-8') != MALFORMED_TARGET:
                    return False
            engines = ('native', 'reference') if item['kind'] == 'differential' else ('native',)
            wanted = {engine + '.' + phase for engine in engines for phase in phases(item)}
            if item['kind'] == 'differential':
                wanted.update(('reference.sources.files', 'reference.copy.files'))
            link_artifacts = set()
            if item['mode'] == 'consumers':
                wanted.update(('reference.bzl', 'reference.artifact', 'reference.artifact.files'))
                link_artifacts = {'native.artifact', 'reference.artifact'}
            if set(check['run_ids']) != wanted:
                return False
            records = {}
            for key, index in check['run_ids'].items():
                record = linked(index, item['name'] + '.' + key, expected_argv(report, item, key), workspace)
                records[key] = record
                diagnostic = item['diagnostic']
                if diagnostic:
                    require_rejection(record, diagnostic)
                elif key.endswith('.bzl'):
                    bzl_result(record, native=key.startswith('native.'))
                else:
                    shared.success(record)
                    if key.endswith('.query'):
                        shared.query_result(record, key.startswith('native.'))
            artifact_keys = {key for key in wanted if key.endswith(('.sources', '.copy'))}
            tree_keys = {key for key in artifact_keys if key.endswith('.sources')}
            if set(check['artifacts']) != artifact_keys | link_artifacts or set(check['trees']) != tree_keys:
                return False
            for key in link_artifacts:
                engine = key.split('.', 1)[0]
                evidence = check['artifacts'][key]
                paths = (shared.native_paths(records[key], ARTIFACT, scratch) if engine == 'native'
                         else reference_output_paths(shared.success(records[key + '.files']), workspace, ref.output_base))
                if (set(evidence) != {'link'} or not evidence_matches(evidence['link'], shared.data())
                        or [Path(evidence['link']['path'])] != paths
                        or not artifact_path_ok(engine, paths[0], scratch, item, ref.output_base)):
                    return False
            if item['diagnostic'] and check['no_publication'] is not True:
                return False
            for key in artifact_keys:
                source = key.endswith('.sources')
                expected = {'MODULE.bazel': shared.MODULE.encode(), 'data': shared.data()} if source else {'copy.txt': shared.data()}
                evidence = check['artifacts'][key]
                if not shared.manifest_matches(evidence, expected):
                    return False
                paths = (shared.native_paths(records[key], invocation(key.split('.', 1)[1])[1], scratch)
                         if key.startswith('native.') else reference_output_paths(shared.success(records[key + '.files']), workspace, ref.output_base))
                if sorted(map(str, paths)) != sorted(value['path'] for value in evidence.values()):
                    return False
                if any(Path(value['path']).name != name for name, value in evidence.items()):
                    return False
                if source:
                    tree = check['trees'][key]
                    root = Path(tree['root'])
                    if not root.is_absolute() or '..' in root.parts or not tree_evidence_matches(tree, item, case_root):
                        return False
                    if key.startswith('native.'):
                        if (not root.is_relative_to(shared.trees_directory(scratch / 'native' / item['name']))
                                or any(path.parent != root for path in paths)):
                            return False
                    elif (not any(root.is_relative_to(allowed) for allowed in (ref.output_base, scratch / 'repository-contents'))
                          or any(path.parent != ref.output_base / 'execroot/_main/external' / (shared.DEP + '+') for path in paths)):
                        return False
                elif any(path.parts[-3:] != ('external', shared.DEP + '+', 'copy.txt') for path in paths):
                    return False
            if item['mode'] == 'warm' and (check['origin_unavailable'] is not True
                    or check['trees']['native.sources']['root'] != check['trees']['native.warm.sources']['root']):
                return False
            if item['mode'] == 'repair':
                root = Path(check['trees']['native.sources']['root'])
                specs = {'retarget': ('symlink', b'../../.bazelversion.same', LINK),
                         'regular': ('file', VERSION, LINK), 'referent': ('file', b'corrupt referent\n', '.bazelversion')}
                if set(check['tampering']) != set(specs):
                    return False
                for probe, (kind, raw, name) in specs.items():
                    value = check['tampering'][probe]
                    if (not valid_entry(value, kind, raw) or value['path'] != str(root / name)
                            or check['trees']['native.' + probe + '.sources']['root'] != str(root)):
                        return False
            elif check['tampering']:
                return False
        return used == set(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError):
        return False


def argument_parser():
    parser = shared.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/git-links-validation.json')
    return parser


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if (args.output in [args.native, args.reference, shared.GIT, *SOURCES]
            or any(args.output.is_relative_to(ROOT / directory) for directory in ('scripts', 'tests'))):
        raise ValueError('report must not overwrite executables, harness sources, or fixtures')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = dict(schema_version=1, status='FAIL', checks=[], runs=[], errors=[], cleanup=[],
                  scratch_cleaned=False, reference_shutdown_attempted=False, sources_unchanged=False,
                  started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  configuration={key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
                  planned_checks=CASES, case_inputs_sha256=digest(json.dumps(CASES, sort_keys=True).encode()),
                  reference_sha256_required=REFERENCE_SHA256, test_commands_executed=False,
                  limitations=['19 cases: four differential, fifteen native-only. Unit tests are NOT actual CLI evidence.',
                               'Scoped Git repository FILE-link tree fidelity, not general action symlink semantics.',
                               'Real private file-URL origins; genuine Bazel 9.3.0 may use BCR only for its built-in closure.',
                               'Reference trees come from regular cquery sources in private reference caches, never native caches.',
                               'Complete lstat/readlink inventory precedes positive link reads; malicious origins are never followed.',
                               'Directory links and root MODULE links remain unsupported. A source artifact and a .bzl load (N26) through an internal link work and are compared with the reference; a patched link leaf becomes a regular file (PatchUtil), see docs/ARCHIVE_SYMLINKS.md.',
                               'A real mode-120000 NUL-target blob is committed via the Git index, never created as an OS link; raw blob bytes and both commits are proven.',
                               'Archive links, other unsafe-character permutations, 40-hop limits and old/new patch-path matrices need native unit coverage.',
                               'MODULE/load and committed-NUL-target diagnostic classes have captured CLI regressions; only this report records actual invocation outcomes.'])
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = validate_binaries(args)
        report['executables']['git'] = file_evidence(shared.GIT)
        scratch = Path(tempfile.mkdtemp(prefix='git-links-validation-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        runner.env = shared.private_environment(scratch / 'environment')
        report['invocation_environment'] = dict(runner.env)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        reference = PatchReference(args.reference, workspace, scratch, runner, 'git-links')
        reference.started = True
        report['reference_release'] = shared.success(runner.run('reference.version', reference.startup + ['version', '--gnu_format'], workspace)).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('reference is not official Bazel 9.3.0')
        for item in CASES:
            run_case(args, runner, scratch, workspace, reference, item)
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__ + ': ' + str(exc))
    finally:
        if reference is not None:
            try:
                report['reference_shutdown_attempted'] = True
                reference.shutdown()
            except Exception as exc:
                report['errors'].append('reference shutdown: ' + str(exc))
        if scratch is not None:
            try:
                # Check every already-created sentinel immediately before cleanup.
                for marker in (scratch / 'cases').glob('*/outside-marker'):
                    if not valid_entry(entry_evidence(marker), 'file', OUTSIDE):
                        report['errors'].append('outside marker changed before cleanup: ' + str(marker))
                shutil.rmtree(scratch)  # rmtree unlinks symlinks; it does not traverse them.
                report['scratch_cleaned'] = not scratch.exists()
            except (OSError, ValueError) as exc:
                report['errors'].append('scratch cleanup: ' + str(exc))
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['sources_unchanged'] = report.get('sources_before') == report['sources_after']
        except OSError as exc:
            report['errors'].append('source evidence: ' + str(exc))
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['completed_differential_cases'] = sum(c['kind'] == 'differential' and c['status'] == 'PASS' for c in report['checks'])
        report['build_commands_executed'] = any('build' in r['argv'] for r in report['runs'])
        if report_passes(report):
            report['status'] = 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print('{} (scoped Git FILE-link fixtures only): {}'.format(report['status'], args.output))
    return 130 if interrupted else 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
