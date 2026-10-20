#!/usr/bin/env python3
"""Bounded N1 Label.repo_name preparation/differential; never full Label parity.

No native defaults or fallback. Run --native-preparation on a frozen candidate
before attempting paired acceptance. Reference-only is preparation, not acceptance.
Context7 quota was exhausted during upstream research; pinned local Java is used.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile
from types import SimpleNamespace

import validate_repository_environment as controlled
import validate_repository_rules as artifacts
from validate import file_evidence, positive_seconds, reference_output_paths, validate_executable, write_report
from validate_direct_repositories import REFERENCE_SHA256

ROOT = Path(__file__).resolve().parents[1]
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
COMMIT = 'e89755babf3e941bc1e9d8120b1cd48854ab9dfa'
JAVA = 'src/main/java/com/google/devtools/build/lib/cmdline/Label.java'
JAVA_SHA = '9845de146667f32459dd693dd7eb7197e5dc3b640ff497891874c629c7d14417'
CONTRACT_SHA = '3c1654986f88c5b5d146c73d054e38a33d9674a802383583bfc3c60147d86338'
PLATFORM_SHA = '3384eb1c30762704fbe38e440204e114154086c8fc8a8c2e3e28441028c019a8'
PLATFORM_NAME = 'platforms-1.0.0.tar.gz'
PLATFORM_FIXTURE = ROOT / 'tests/fixtures/label_repo_name/platforms_archive.json'
PLATFORM_IDENTITY = dict(version='1.0.0', basename=PLATFORM_NAME, sha256=PLATFORM_SHA, size_bytes=7879,
    release_url='https://github.com/bazelbuild/platforms/releases/download/1.0.0/'+PLATFORM_NAME)
PLATFORM_PROVENANCE = dict(kind='verified_existing_genuine_cas_archive',
    source='/tmp/opencode/symbolic-macro-edge-contract/bazel-user-root/cache/repos/v1/content_addressable/sha256/'+PLATFORM_SHA+'/file',
    source_sha256=PLATFORM_SHA, reference_source_commit=COMMIT,
    offline_pattern_source='scripts/validate_starlark_sets.py',
    offline_pattern_sha256='a315da9a94d014aeec809ec6ffe3fbde8b80e3817e5e31c84683c02579853592')
CASES = ('pipeline', 'analysis', 'assignment', 'unknown-field', 'root-unmapped',
         'external-unmapped', 'external-empty')
POSITIVE = CASES[:2]
# Labels naming a repository the owner's mapping does not see. Since
# integration-15 (N45 deferred invalid Labels, N43 single Label methods) native
# constructs them like Bazel and the repo_name getter rejects: no divergence left.
UNMAPPED = CASES[4:]
LIMITATIONS = [
    'REFERENCE_PREPARATION is not native acceptance; native preparation must precede paired use.',
    'Unknown apparent Labels: both engines construct, then the repo_name getter rejects with Bazel\'s invalid-Label text.',
    'Label(existingLabel) is a KNOWN_EXISTING_UNSUPPORTED_PROFILE, deliberately not exercised.',
    'Full dir, direct print/repr parity, deprecation CLI flags, deferred invalid Labels are excluded.',
    'Only selected Label surface; no participant-module repo_name implementation is implied.',
    'Context7 quota exhausted in source research; no successful Context7 retrieval claimed.',
    'Only the exact genuine platforms 1.0.0 archive is staged from the portable fixture; downloads remain disabled.',
    'First attempt validated ZERO getter cases: missing offline platforms. Its FAIL evidence is retained unchanged.',
]
digest = artifacts.digest


def encoded(raw):
    return dict(bytes_base64=base64.b64encode(raw).decode(), sha256=digest(raw), size_bytes=len(raw))


def decoded(item):
    raw = base64.b64decode(item['bytes_base64'], validate=True)
    if item['sha256'] != digest(raw) or item['size_bytes'] != len(raw):
        raise ValueError('byte evidence mismatch')
    return raw


def platform_bytes(item):
    if (set(item) != {*PLATFORM_IDENTITY, 'provenance', 'bytes_base64'}
            or {k: item[k] for k in PLATFORM_IDENTITY} != PLATFORM_IDENTITY
            or item['provenance'] != PLATFORM_PROVENANCE):
        raise ValueError('platforms release/source provenance mismatch')
    raw = decoded(item)
    if base64.b64encode(raw).decode() != item['bytes_base64']:
        raise ValueError('noncanonical platforms base64')
    return raw


def platform_evidence(scratch, *, staged=True):
    fixture_raw = PLATFORM_FIXTURE.read_bytes()
    item = json.loads(fixture_raw)
    raw = platform_bytes(item)
    path = scratch / 'distdir' / PLATFORM_NAME
    expected = dict(encoded(raw), path=str(path), kind='file', mode=0o644)
    if staged:
        if path.is_symlink() or path.parent.is_symlink() or artifacts.evidence(path) != expected:
            raise ValueError('missing/changed/private-distdir platforms archive')
    return dict(fixture_sha256=digest(fixture_raw), identity=PLATFORM_IDENTITY,
        provenance=PLATFORM_PROVENANCE,
        provenance_sha256=digest(json.dumps(PLATFORM_PROVENANCE, sort_keys=True).encode()), archive=expected)


def stage_platforms(scratch):
    raw = platform_bytes(json.loads(PLATFORM_FIXTURE.read_bytes()))
    distdir = scratch / 'distdir'
    distdir.mkdir()
    path = distdir / PLATFORM_NAME
    with path.open('xb') as stream:
        stream.write(raw)
    path.chmod(0o644)
    return platform_evidence(scratch)


SURFACE = '''def surface(value):
    return [str(value), value.repo_name, value.workspace_name, type(value), type(value.repo_name), getattr(value, "repo_name", "DEFAULT"), hasattr(value, "repo_name"), "repo_name" in dir(value), getattr(value, "missing_field", "DEFAULT"), hasattr(value, "missing_field")]
'''
HELPER = SURFACE + '''FROZEN = Label("//:dep.txt")
def external():
    values = [FROZEN, Label("@self_dep//:dep.txt"), Label("@shared//:other.txt"), Label("@@//:root.txt")]
    return [surface(v) for v in values]
'''
ROOT_HELPER = SURFACE + '''FROZEN = Label("//:root.txt")
def root_values():
    values = [FROZEN, Label("@@//:root.txt"), Label("@root_self//:root.txt"), Label("@//:root.txt"), Label("@shared//:dep.txt"), Label("@other_root//:other.txt"), Label("@@unfetched+segments+name//no/package:no_target")]
    dep = values[4]
    reconstructed = Label("@@" + dep.repo_name + "//:dep.txt")
    keys = {dep: "stable", FROZEN: "main"}
    return {"root": [surface(v) for v in values], "equal": [values[0] == values[1], values[0] == values[2], dep == reconstructed, keys[reconstructed] == "stable", keys[values[1]] == "main", len(keys) == 2]}
'''
EXTENSION = '''load(":helper.bzl", "surface", "external")
def _repo(ctx):
    print("N1_REPO")
    ctx.file("data.txt", ctx.attr.text, executable=False)
    ctx.file("generated.bzl", 'VALUE = Label("//:data.txt")\\n', executable=False)
    ctx.file("BUILD.bazel", 'exports_files(["data.txt", "generated.bzl"], visibility=["//visibility:public"])\\n', executable=False)
make = repository_rule(implementation=_repo, attrs={"text": attr.string()})
def _extension(ctx):
    print("N1_EXTENSION")
    tags = []
    participants = []
    for mod in ctx.modules:
        participants.append([mod.name, hasattr(mod, "repo_name"), getattr(mod, "repo_name", "ABSENT")])
        for tag in mod.tags.item:
            tags.append([mod.name, tag.key, surface(tag.value)])
    data = {"external": external(), "extension": surface(Label("//:extension.bzl")), "tags": tags, "participants": participants}
    make(name="made", text=json.encode(data) + "\\n")
observe = module_extension(implementation=_extension, tag_classes={"item": tag_class(attrs={"key": attr.string(), "value": attr.label(default="//:dep.txt")})})
'''
ANALYSIS = '''load(":root.bzl", "surface", "root_values")
load("@made//:generated.bzl", "VALUE")
def _impl(ctx):
    print("N1_ANALYSIS")
    data = root_values()
    data["analysis"] = [surface(ctx.label), surface(ctx.file.src.owner), surface(ctx.attr.src.label), surface(VALUE)]
    out = ctx.actions.declare_file("result.txt")
    ctx.actions.write(out, json.encode(data) + "\\n")
    return [DefaultInfo(files=depset([out]))]
consume = rule(implementation=_impl, attrs={"src": attr.label(default="@made//:data.txt", allow_single_file=True)})
'''


def fixture(case):
    if case not in CASES:
        raise ValueError('unplanned case')
    files = {
        'MODULE.bazel': 'module(name="root_contract",version="1.0",repo_name="root_self")\nbazel_dep(name="dep",version="1.0",repo_name="shared")\nbazel_dep(name="other",version="1.0",repo_name="other_root")\nlocal_path_override(module_name="dep",path="dep")\nlocal_path_override(module_name="other",path="other")\n',
        'dep/MODULE.bazel': 'module(name="dep",version="1.0",repo_name="self_dep")\nbazel_dep(name="other",version="1.0",repo_name="shared")\n',
        'other/MODULE.bazel': 'module(name="other",version="1.0")\n',
        'other/BUILD.bazel': 'exports_files(["other.txt"])\n', 'other/other.txt': 'other\n',
        'dep/BUILD.bazel': 'exports_files(["dep.txt","helper.bzl","extension.bzl"])\n',
        'dep/dep.txt': 'dep\n', 'root.txt': 'root\n', 'dep/helper.bzl': HELPER,
        'root.bzl': ROOT_HELPER, 'dep/extension.bzl': EXTENSION,
    }
    if case in POSITIVE:
        for owner, prefix, tags in (
            ('', '@shared', [('root-local', '//:root.txt'), ('root-shared', '@shared//:dep.txt'), ('root-self', '@root_self//:root.txt')]),
            ('dep/', '', [('dep-local', '//:dep.txt'), ('dep-shared', '@shared//:other.txt'), ('dep-self', '@self_dep//:dep.txt')])):
            files[owner+'MODULE.bazel'] += 'obs=use_extension("'+prefix+'//:extension.bzl","observe")\n'
            for key, value in tags:
                files[owner+'MODULE.bazel'] += 'obs.item(key='+json.dumps(key)+',value='+json.dumps(value)+')\n'
        files['dep/MODULE.bazel'] += 'obs.item(key="dep-default")\n'
        files['MODULE.bazel'] += 'use_repo(obs,"made")\n'
        if case == 'pipeline':
            files['BUILD.bazel'] = 'genrule(name="consume",srcs=["@made//:data.txt"],outs=["result.txt"],cmd="cat $(SRCS) > $@")\nfilegroup(name="result",srcs=[":consume","@made//:data.txt"])\n'
        else:
            files['consumer.bzl'] = ANALYSIS
            files['BUILD.bazel'] = 'load(":consumer.bzl","consume")\nconsume(name="result")\n'
    else:
        external = case.startswith('external')
        path = 'dep/probe.bzl' if external else 'probe.bzl'
        label = '@//:root.txt' if case == 'external-empty' else '@not_visible//:missing' if case in UNMAPPED else '//:root.txt'
        operation = 'value.repo_name = "changed"' if case == 'assignment' else 'result = value.missing_field' if case == 'unknown-field' else 'result = value.repo_name'
        files[path] = ('def probe():\n    print("N1_BEGIN '+case+'")\n    value = Label('+json.dumps(label)+')\n'
                       '    print("N1_CONSTRUCTED '+case+' " + type(value))\n    '+operation+'\n    print("N1_AFTER '+case+'")\n')
        files['BUILD.bazel'] = 'load("'+('@shared//:probe.bzl' if external else ':probe.bzl')+'","probe")\nprobe()\nfilegroup(name="result",srcs=[])\n'
    return {p: text.encode() for p, text in files.items()}


def surface(label):
    if not re.fullmatch(r'@@[^/]*//[^:]*:[^:]+', label):
        raise ValueError('not a canonical string')
    repo = label[2:].split('//', 1)[0]
    return [label, repo, repo, 'Label', 'string', repo, True, True, 'DEFAULT', False]


def check_data(case, data):
    dep, other, main = '@@dep+//:dep.txt', '@@other+//:other.txt', '@@//:root.txt'
    if case == 'pipeline':
        expected = dict(external=list(map(surface, [dep, dep, other, main])),
            extension=surface('@@dep+//:extension.bzl'),
            participants=[['root_contract', False, 'ABSENT'], ['dep', False, 'ABSENT']],
            tags=[[owner, key, surface(label)] for owner, key, label in [
                ('root_contract', 'root-local', main), ('root_contract', 'root-shared', dep),
                ('root_contract', 'root-self', main), ('dep', 'dep-local', dep),
                ('dep', 'dep-shared', other), ('dep', 'dep-self', dep), ('dep', 'dep-default', dep)]])
    elif case == 'analysis':
        # Canonical generated identity comes from actual str(File.owner), never
        # guessed by replacing prefixes. The paired comparison preserves it raw.
        generated = data['analysis'][1][0]
        if not re.fullmatch(r'@@[^/]+//:data.txt', generated):
            raise ValueError('missing discovered generated owner')
        expected = dict(root=list(map(surface, [main]*4+[dep, other,
            '@@unfetched+segments+name//no/package:no_target'])), equal=[True]*6,
            analysis=[surface('@@//:result')]+[surface(generated)]*3)
    else:
        raise ValueError('unexpected data case')
    if data != expected:
        raise ValueError('wrong selected Label surface/owner/equality: '+case)


def markers(row):
    return re.findall(r'(?m)^(?:DEBUG: [^\n]+?:\d+:\d+: )?(N1_[^\n]*)$', row['stderr'])


def rejection(row, case, engine, files):
    if files != fixture(case) or case in POSITIVE:
        raise ValueError('rejection detached from planned raw fixture')
    controlled.healthy(row)
    if artifacts.UNRELATED.search(row['stderr']) or artifacts.BOUNDARY_INFRASTRUCTURE_ERROR.search(row['stderr']):
        raise ValueError('fallback or unrelated infrastructure in negative query')
    if re.search(r'\b(?:\d+\s+actions?\s+executed|\d+\s+(?:action\s+)?cache\s+hits?|\d+\s+total\s+actions?)\b',
                 row['stderr'], re.I):
        raise ValueError('negative query reported action execution or cache counters')
    if row['returncode'] != (7 if engine == 'reference' else 1) or row['stdout']:
        raise ValueError('wrong controlled rejection status/output')
    expected = ['N1_BEGIN '+case, 'N1_CONSTRUCTED '+case+' Label']
    if markers(row) != expected:
        raise ValueError('wrong constructor/getter/callback phase')
    path = 'dep/probe.bzl' if case.startswith('external') else 'probe.bzl'
    line = 5
    source = files[path].decode().splitlines()[line-1]
    text = row['stderr']
    site = ('probe.bzl", line '+str(line)+', column ' if engine == 'reference' else ':probe.bzl:'+str(line)+':')
    if site not in text or source.strip() not in text:
        raise ValueError('rejection missing bound source/site')
    if engine == 'reference':
        pattern = (r'cannot set \.repo_name field of Label value' if case == 'assignment' else
                   r"'Label' value has no field or method 'missing_field'" if case == 'unknown-field' else
                   r"(?m)^Error: " + re.escape(invalid_label_message(case)) + '$')
    else:
        # Since N33 the interpreter reports Bazel's own assignment/unknown-field
        # texts (formerly "Operation `.repo_name=` not supported on type `Label`"
        # and "Object of type `Label` has no attribute `missing_field`"), anchored
        # as the whole native error line. Since integration-15 an unmapped
        # apparent repository yields a deferred invalid Label whose getter fails
        # with Bazel's text (formerly an eager "unknown apparent repository"
        # construction error).
        pattern = (r'(?m)^error: cannot set \.repo_name field of Label value$' if case == 'assignment' else
                   r"(?m)^error: 'Label' value has no field or method 'missing_field'$" if case == 'unknown-field' else
                   r"(?m)^error: " + re.escape(invalid_label_message(case)) + '$')
    if not re.search(pattern, text):
        raise ValueError('wrong Label field/type/visibility diagnostic')


def invalid_label_message(case):
    """Label.getRepository on an invalid repository (Label.java, RepositoryName.java)."""
    if case not in UNMAPPED:
        raise ValueError('not an unmapped Label case')
    repo = '' if case == 'external-empty' else 'not_visible'
    owner = '@@dep+' if case.startswith('external') else '@@'
    target = '//:root.txt' if case == 'external-empty' else '//:missing'
    return ("'repo_name' is not allowed on invalid Label @@[unknown repo '" + repo + "' requested from "
            + owner + ']' + target)


def source_paths():
    return sorted(set([*ROOT.glob('scripts/*.py'), ROOT / 'tests/test_validate_label_repo_name.py',
        *(ROOT / 'src').rglob('*.rs'), ROOT / 'Cargo.lock',
        ROOT / 'vendor/starlark-0.14.2/src/values/error.rs',
        *(p for p in (ROOT / 'tests/fixtures/label_repo_name').rglob('*') if p.is_file()),
        *(p for p in REGISTRY.rglob('*') if p.is_file())]))


def fingerprints(paths):
    return {str(p): dict(sha256=digest(p.read_bytes()), mode=p.stat().st_mode & 0o7777) for p in paths}


def pins(args, scratch):
    binaries = {}
    for engine in (('reference',) if args.reference_only else ('reference', 'native')):
        path = getattr(args, engine)
        validate_executable(path, official=True)
        item = file_evidence(path)
        if item['sha256'] != (REFERENCE_SHA256 if engine == 'reference' else args.native_sha256):
            raise ValueError('executable hash mismatch: '+engine)
        binaries[engine] = item
    java = (args.official_source / JAVA).read_bytes()
    if digest(java) != JAVA_SHA:
        raise ValueError('official Label.java pin mismatch')
    return dict(sources=fingerprints(source_paths()), executables=binaries, official={JAVA: JAVA_SHA},
        registry=fingerprints(p for p in (scratch / 'registry').rglob('*') if p.is_file()),
        platforms_distfile=platform_evidence(scratch))


class Runner(controlled.RecordingRunner):
    def __init__(self, report, args, scratch):
        super().__init__(report, args.timeout, scratch)
        self.args = args

    def run(self, name, argv, cwd, timeout=None):
        if pins(self.args, self.scratch) != self.report['pins_before']:
            raise ValueError('source/registry/executable drift before command')
        start = len(self.report['runs'])
        try:
            return super().run(name, argv, cwd, timeout)
        finally:
            for row in self.report['runs'][start:]:
                pid = row.get('owned_client_pid')
                present = False
                if pid:
                    try:
                        os.killpg(pid, 0)
                        present = True
                        os.killpg(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                row['group_cleanup'] = dict(pgid=pid, checked=bool(pid), killed_remaining=present)
                row['stdin_bytes'] = 0
                row['platforms_distfile_sha256'] = PLATFORM_SHA


def argv_for(args, scratch, case, engine, command):
    base = scratch / engine / case
    if engine == 'native':
        return artifacts.native_command(args.native, base / 'cache', scratch / 'registry', command, '//:result')
    startup = [str(args.reference), '--batch', '--ignore_all_rc_files',
        '--output_user_root='+str(base / 'user'), '--output_base='+str(base / 'output'),
        '--host_jvm_args=-XX:ActiveProcessorCount=2']
    if command == 'version':
        return startup + ['version', '--gnu_format']
    return startup + [command, '--registry='+(scratch / 'registry').as_uri(),
        '--repository_cache='+str(base / 'repo-cache'), '--repo_contents_cache='+str(base / 'contents'),
        '--repository_disable_download', '--lockfile_mode=off', '--incompatible_autoload_externally=',
        '--distdir='+str(scratch / 'distdir'),
        '--noshow_progress', '--color=no', '--curses=no'] + (
        ['--jobs=2', '--experimental_convenience_symlinks=ignore'] if command in ('build', 'cquery') else []) + (
        ['--output=files'] if command == 'cquery' else []) + ['//:result']


def sequence(engine):
    return [(case, state, command) for case in CASES for state in (('cold', 'warm') if case in POSITIVE else ('cold',))
        for command in (('build', 'cquery') if engine == 'reference' and case in POSITIVE else
                        ('build',) if case in POSITIVE else ('query',))]


def ordered_run_plan(args, scratch):
    """Derive chronology from trusted case/configuration rules, never observations."""
    if type(args.reference_only) is not bool or type(args.native_preparation) is not bool or (
            args.reference_only and args.native_preparation):
        raise ValueError('invalid execution mode')
    plan = []

    def append(name, argv, cwd, engine=None, observation=None):
        sha = REFERENCE_SHA256 if engine == 'reference' else args.native_sha256 if engine == 'native' else None
        plan.append(dict(id=len(plan), name=name, argv=argv, cwd=str(cwd),
                         executable_sha256=sha, observation=observation))

    def official(phase):
        for suffix, words in [('head', ['rev-parse', 'HEAD']),
                              ('status', ['status', '--porcelain=v1', '--untracked-files=no'])]:
            append('official.'+phase+'.'+suffix,
                   ['/usr/bin/git', '--no-optional-locks', '-C', str(args.official_source)]+words, scratch)

    official('before')
    append('reference.version', argv_for(args, scratch, 'version', 'reference', 'version'), scratch, 'reference')
    engines = ('reference',) if args.reference_only else ('native',) if args.native_preparation else ('reference', 'native')
    for engine in engines:
        for case, state, command in sequence(engine):
            append('.'.join((engine, case, state, command)), argv_for(args, scratch, case, engine, command),
                   scratch / engine / case / 'workspace', engine, (engine, case, state, command))
    official('after')
    return plan


def check_run_order(report, args, scratch):
    plan = ordered_run_plan(args, scratch)
    if len(report['runs']) != len(plan):
        raise ValueError('chronological run plan length mismatch')
    for row, expected in zip(report['runs'], plan):
        if type(row['id']) is not int or any(row.get(k) != v for k, v in expected.items() if k != 'observation'):
            raise ValueError('chronological run plan mismatch at row '+str(expected['id']))
    expected = [(p['id'], *p['observation']) for p in plan if p['observation'] is not None]
    actual = [(o['run'], o['engine'], o['case'], o['state'], o['command']) for o in report['observations']]
    if actual != expected:
        raise ValueError('observations detached from chronological run plan')


def check_outputs(case, outputs):
    if set(outputs) != ({'data.txt', 'result.txt'} if case == 'pipeline' else {'result.txt'}):
        raise ValueError('wrong producer/consumer outputs')
    for name, item in outputs.items():
        raw = decoded(item)
        # Completed action outputs use Bazel OutputPermissions.READONLY (0555),
        # also src/action.rs OUTPUT_MODE, even for non-executable write requests.
        # Repository source files are not completed action outputs.
        mode = 0o644 if name == 'data.txt' else 0o555
        if item['kind'] != 'file' or item['mode'] != mode:
            raise ValueError('wrong output file/mode')
        if not raw.endswith(b'\n'):
            raise ValueError('missing output newline')
        check_data(case, json.loads(raw))
    if case == 'pipeline' and decoded(outputs['data.txt']) != decoded(outputs['result.txt']):
        raise ValueError('consumer did not preserve producer bytes')


def check_inventory(obs, cache):
    manifests, inventory = obs['metadata'], obs['inventory']
    ids = obs['publications']
    if len(ids) != 1 or not re.fullmatch('[0-9a-f]{64}', ids[0]):
        raise ValueError('unexpected repository publication count/identity')
    root = artifacts.trees_directory(cache) / ids[0]
    path = str(cache / 'repositories/direct/manifests' / (ids[0]+'.json'))
    if set(manifests) != {path} or set(inventory) != {str(root)}:
        raise ValueError('publication/manifest/inventory detached')
    item, tree = manifests[path], inventory[str(root)]
    data = json.loads(decoded(item))
    if (not artifacts.valid_evidence(item) or item['path'] != path
            or data['format'] != 1 or data['profile'] != 'direct-file-template-v1' or data['identity'] != ids[0]):
        raise ValueError('wrong native publication manifest')
    controlled.check_publication_schema(data, 'wrong native publication manifest')
    controlled.check_publication_privacy(data, 'wrong native publication manifest')
    expected = {'REPO.bazel': b'', 'generated.bzl': b'VALUE = Label("//:data.txt")\n',
        'BUILD.bazel': b'exports_files(["data.txt", "generated.bzl"], visibility=["//visibility:public"])\n'}
    if set(tree['files']) != {*expected, 'data.txt'}:
        raise ValueError('unexpected repository file inventory')
    entries = {'': dict(directory=True, mode=tree['mode'], size=0, sha256='')}
    for name, entry in tree['files'].items():
        raw = decoded(entry)
        if entry['path'] != str(root / name) or entry['kind'] != 'file' or entry['mode'] != 0o644:
            raise ValueError('wrong repository file path/type/mode')
        if name in expected and raw != expected[name]:
            raise ValueError('repository scaffolding bytes changed')
        if name == 'data.txt':
            check_data('pipeline', json.loads(raw))
        entries[name] = dict(directory=False, mode=entry['mode'], size=len(raw), sha256=digest(raw))
    if data['files'] != entries:
        raise ValueError('manifest not bound to actual repository inventory')
    if 'data.txt' in obs['outputs'] and obs['outputs']['data.txt']['path'] != str(root / 'data.txt'):
        raise ValueError('consumer source is not the observed publication')


def run_engine(args, report, runner, scratch, engine):
    for case in CASES:
        workspace = scratch / engine / case / 'workspace'
        workspace.mkdir(parents=True)
        files = fixture(case)
        controlled.materialize(workspace, dict(files=files))
        for _, state, command in (s for s in sequence(engine) if s[0] == case):
            obs = dict(case=case, engine=engine, state=state, command=command,
                classification='PAIRED_GETTER',
                inputs={p: encoded(raw) for p, raw in files.items()}, run=len(report['runs']))
            report['observations'].append(obs)
            row = runner.run('.'.join((engine, case, state, command)), argv_for(args, scratch, case, engine, command), workspace)
            obs['inputs_after'] = {p: encoded((workspace / p).read_bytes()) for p in files}
            if obs['inputs'] != obs['inputs_after']:
                raise ValueError('fixture bytes changed')
            cache = scratch / engine / case / 'cache'
            if case not in POSITIVE:
                rejection(row, case, engine, files)
                artifacts.no_publication(cache)
                unexpected = [p for p in (workspace.parent / 'output').rglob('result.txt') if p.is_file()]
                obs['no_outputs'] = not unexpected
                if unexpected:
                    raise ValueError('negative produced outputs')
            else:
                controlled.success(row)
                if engine == 'native' or command == 'cquery':
                    ref = SimpleNamespace(output_base=workspace.parent / 'output')
                    outputs, roots = controlled.capture_outputs(row, engine, scratch, workspace, ref, cache)
                    obs.update(outputs=outputs, roots=roots)
                    check_outputs(case, outputs)
                    if engine == 'native':
                        obs['metadata'] = controlled.native_metadata(cache)
                        obs['publications'] = sorted(p.name for p in artifacts.trees_directory(cache).glob('*'))
                        obs['inventory'] = {str(root): dict(mode=root.stat().st_mode & 0o7777,
                            files={p.relative_to(root).as_posix(): artifacts.evidence(p) for p in root.rglob('*') if p.is_file()})
                            for root in artifacts.trees_directory(cache).glob('*')}
                        check_inventory(obs, cache)


def validate_report(report):
    """Replay raw inputs, commands, streams, artifacts and all planned coverage."""
    if report.get('pins_before') != report.get('pins_after'):
        raise ValueError('before/after pin drift')
    if report['errors']:
        raise ValueError('recorded execution or criterion failure: '+'; '.join(report['errors']))
    args = SimpleNamespace(**report['configuration'])
    for key in ('reference', 'native', 'official_source'):
        if getattr(args, key):
            setattr(args, key, Path(getattr(args, key)))
    scratch = Path(report['scratch'])
    pins_before = report['pins_before']
    if (set(pins_before) != {'sources', 'executables', 'official', 'registry', 'platforms_distfile'}
            or pins_before['official'] != {JAVA: JAVA_SHA}
            or pins_before['executables'] != report['executables']
            or set(pins_before['executables']) != ({'reference'} if args.reference_only else {'reference', 'native'})
            or not pins_before['sources'] or not pins_before['registry']):
        raise ValueError('missing source/executable/registry pins')
    if pins_before['platforms_distfile'] != platform_evidence(scratch, staged=False):
        raise ValueError('platforms archive/provenance/staging replay mismatch')
    for engine, item in pins_before['executables'].items():
        if item['path'] != str(getattr(args, engine)) or item['sha256'] != (REFERENCE_SHA256 if engine == 'reference' else args.native_sha256):
            raise ValueError('wrong required executable pin')
    for category in ('sources', 'registry'):
        for path, item in pins_before[category].items():
            if (not re.fullmatch('[0-9a-f]{64}', item['sha256']) or type(item['mode']) is not int
                    or not 0 <= item['mode'] <= 0o7777 or not Path(path).is_absolute()):
                raise ValueError('invalid source/registry fingerprints')
    if pins_before['sources'] != fingerprints(source_paths()):
        raise ValueError('report source binding no longer matches current checkout')
    registry = {str(scratch / 'registry' / p.relative_to(REGISTRY)): item
                for path, item in fingerprints(p for p in REGISTRY.rglob('*') if p.is_file()).items()
                for p in [Path(path)]}
    if pins_before['registry'] != registry:
        raise ValueError('copied registry is not the source registry')
    if report['limitations'] != LIMITATIONS or report['contract_sha256'] != CONTRACT_SHA:
        raise ValueError('scope/source contract drift')
    if report['cleanup'] != dict(path=str(scratch), removed=True):
        raise ValueError('missing scratch cleanup')
    check_run_order(report, args, scratch)
    runs = report['runs']
    for i, row in enumerate(runs):
        if row['id'] != i or row['stdin_bytes'] != 0:
            raise ValueError('run identity/stdin mismatch')
        if row['platforms_distfile_sha256'] != PLATFORM_SHA:
            raise ValueError('pre-command platforms archive verification missing')
        if row['controlled_environment'] != controlled.encoded_environment(controlled.controlled_environment(scratch, {})):
            raise ValueError('ambient environment contamination')
        if row['timeout_seconds'] != args.timeout:
            raise ValueError('deadline mismatch')
        if row['group_cleanup'] != dict(pgid=row['owned_client_pid'], checked=True, killed_remaining=False):
            raise ValueError('uncontrolled surviving process group')
        for stream in ('stdout', 'stderr'):
            raw = base64.b64decode(row[stream+'_base64'], validate=True)
            if (row[stream] != raw.decode('utf-8', errors='replace') or digest(raw) != row[stream+'_sha256']
                    or digest(raw) != row[stream+'_full_sha256'] or len(raw) != row[stream+'_full_size_bytes']):
                raise ValueError('corrupt raw command stream')
        controlled.healthy(row)
    engines = ('reference',) if args.reference_only else ('native',) if args.native_preparation else ('reference', 'native')
    expected = [(e, *s) for e in engines for s in sequence(e)]
    actual = [(o['engine'], o['case'], o['state'], o['command']) for o in report['observations']]
    if actual != expected:
        raise ValueError('missing/extra/reordered planned coverage')
    bound = []
    seen = {}
    for obs in report['observations']:
        engine, case, state, command = obs['engine'], obs['case'], obs['state'], obs['command']
        row = runs[obs['run']]
        bound.append(obs['run'])
        files = fixture(case)
        if obs['inputs'] != {p: encoded(raw) for p, raw in files.items()} or obs['inputs_after'] != obs['inputs']:
            raise ValueError('fixture/raw source mismatch')
        if (row['argv'] != argv_for(args, scratch, case, engine, command)
                or row['cwd'] != str(scratch / engine / case / 'workspace')
                or row['name'] != '.'.join((engine, case, state, command))
                or row['executable_sha256'] != report['pins_before']['executables'][engine]['sha256']):
            raise ValueError('command/executable detached from case')
        if obs['classification'] != 'PAIRED_GETTER':
            raise ValueError('divergence mislabeled parity')
        if case not in POSITIVE:
            rejection(row, case, engine, files)
            if obs.get('no_outputs') is not True or any(k in obs for k in ('outputs', 'metadata', 'roots')):
                raise ValueError('negative output/publication')
            continue
        controlled.success(row)
        expected_markers = ['N1_EXTENSION', 'N1_REPO'] + (['N1_ANALYSIS'] if case == 'analysis' else [])
        observed = markers(row)
        # Warm callbacks are observations, not a cross-engine cache oracle.
        if observed != [m for m in expected_markers if m in observed]:
            raise ValueError('wrong positive callback phases')
        if state == 'cold' and command == 'build' and markers(row) != expected_markers:
            raise ValueError('cold producer callbacks absent')
        if engine == 'reference' and command == 'build':
            if 'outputs' in obs:
                raise ValueError('invented build output listing')
            continue
        outputs = obs['outputs']
        check_outputs(case, outputs)
        if engine == 'native' and any(len(line.split('\t')) != 2 or not line.startswith('//:result\t')
                                      for line in row['stdout'].splitlines()):
            raise ValueError('wrong native output label/listing')
        listed = (list(map(str, reference_output_paths(row['stdout'], Path(row['cwd']), scratch / engine / case / 'output')))
                  if engine == 'reference' else [line.split('\t')[1] for line in row['stdout'].splitlines()])
        if sorted(listed) != sorted(x['logical_path'] for x in outputs.values()):
            raise ValueError('output detached from real CLI listing')
        for item in outputs.values():
            if not Path(item['path']).is_relative_to(scratch) or '..' in Path(item['path']).parts:
                raise ValueError('output escaped scratch')
        if set(obs['roots']) != ({'data.txt'} if case == 'pipeline' else set()):
            raise ValueError('wrong producer roots')
        if case == 'pipeline' and obs['roots']['data.txt'] != str(Path(outputs['data.txt']['path']).parent):
            raise ValueError('producer root detached')
        key = engine, case
        if engine == 'native':
            # Repository callbacks may re-execute on warm builds. Reuse is proved
            # by publication inventory and consumer action hits, not print silence.
            import validate_string_list_dict as common
            common.action_counts(row, (1, 0) if state == 'cold' else (0, 1))
            check_inventory(obs, scratch / engine / case / 'cache')
        stable = ({p: (decoded(v), v['mode'], v['path']) for p, v in outputs.items()}, obs['roots'],
                  obs.get('metadata'), obs.get('publications'), obs.get('inventory'))
        if state == 'warm' and seen.get(key) != stable:
            raise ValueError('cold/warm bytes/mode/cache identity moved')
        seen[key] = stable
    # Four pin commands and one version command are part of the evidence, too.
    other = [r for r in runs if r['id'] not in bound]
    names = ['official.before.head', 'official.before.status', 'reference.version', 'official.after.head', 'official.after.status']
    if [r['name'] for r in other] != names:
        raise ValueError('unplanned/missing command')
    for row in other:
        controlled.success(row)
        if row['name'] == 'reference.version':
            if (row['argv'] != argv_for(args, scratch, 'version', 'reference', 'version') or row['stdout'].strip() != 'bazel 9.3.0'
                    or row['executable_sha256'] != REFERENCE_SHA256 or row['cwd'] != str(scratch)):
                raise ValueError('wrong reference version command')
        else:
            head = row['name'].endswith('head')
            expected_argv = ['/usr/bin/git', '--no-optional-locks', '-C', str(args.official_source)] + (
                ['rev-parse', 'HEAD'] if head else ['status', '--porcelain=v1', '--untracked-files=no'])
            if row['argv'] != expected_argv or row['stdout'].strip() != (COMMIT if head else '') or row['cwd'] != str(scratch):
                raise ValueError('official source HEAD/status mismatch')
    if not args.reference_only and not args.native_preparation:
        for case in POSITIVE:
            # Exact canonical bytes, no guessed identity normalization.
            a, b = seen['reference', case][0], seen['native', case][0]
            if {p: x[:2] for p, x in a.items()} != {p: x[:2] for p, x in b.items()}:
                raise ValueError('paired canonical bytes/modes differ')
    if 'reference' in engines:
        producer = next(o for o in report['observations'] if o['engine'] == 'reference'
                        and o['case'] == 'pipeline' and 'outputs' in o)['outputs']['data.txt']
        external = scratch / 'reference/pipeline/output/execroot/_main/external'
        identity = Path(producer['logical_path']).relative_to(external).parts
        if len(identity) != 2 or identity[1] != 'data.txt':
            raise ValueError('genuine generated identity missing from real producer path')
        analysis = json.loads(seen['reference', 'analysis'][0]['result.txt'][0])
        if analysis['analysis'][1][0] != '@@'+identity[0]+'//:data.txt':
            raise ValueError('generated Label owner detached from discovered producer provenance')
    return True


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--reference', type=Path, required=True)
    p.add_argument('--native', type=Path)
    p.add_argument('--native-sha256')
    p.add_argument('--native-preparation-report', type=Path,
                   help='required full-mode receipt from an all-case frozen-candidate native preparation')
    mode = p.add_mutually_exclusive_group()
    mode.add_argument('--reference-only', action='store_true')
    mode.add_argument('--native-preparation', action='store_true')
    p.add_argument('--official-source', type=Path, required=True)
    p.add_argument('--scratch-parent', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--timeout', type=positive_seconds, default=180)
    return p


def main(argv=None):
    args = parser().parse_args(argv)
    if not args.reference_only and (not args.native or not re.fullmatch('[0-9a-f]{64}', args.native_sha256 or '')):
        raise ValueError('full/native preparation requires explicit --native and --native-sha256')
    if args.reference_only and (args.native or args.native_sha256):
        raise ValueError('reference-only must not select native')
    for key, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, key, value.resolve())
    if not args.reference_only and not args.native_preparation:
        if not args.native_preparation_report:
            raise ValueError('full mode requires prior all-case --native-preparation-report')
        prepared = json.loads(args.native_preparation_report.read_bytes())
        validate_report(prepared)
        if (prepared['status'] != 'NATIVE_PREPARATION_PASS'
                or prepared['configuration']['native'] != str(args.native)
                or prepared['configuration']['native_sha256'] != args.native_sha256
                or prepared['pins_before']['sources'] != fingerprints(source_paths())):
            raise ValueError('native preparation does not bind this frozen candidate and QA source')
    if args.output.exists() or args.output.is_relative_to(ROOT) or args.output.is_relative_to(args.official_source):
        raise ValueError('output must be new and outside source checkouts; first failure is preserved')
    report = dict(schema_version=1, status='FAIL', errors=[], runs=[], observations=[], limitations=LIMITATIONS,
        contract_sha256=CONTRACT_SHA, configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()})
    scratch = Path(tempfile.mkdtemp(prefix='label-repo-name-', dir=args.scratch_parent))
    report['scratch'] = str(scratch)
    runner = None
    try:
        (scratch / 'home').mkdir()
        (scratch / 'tmp').mkdir()
        shutil.copytree(REGISTRY, scratch / 'registry')
        stage_platforms(scratch)
        report['pins_before'] = pins(args, scratch)
        report['executables'] = report['pins_before']['executables']
        runner = Runner(report, args, scratch)
        official_commands(runner, args, scratch, 'before')
        row = runner.run('reference.version', argv_for(args, scratch, 'version', 'reference', 'version'), scratch)
        controlled.success(row)
        if row['stdout'].strip() != 'bazel 9.3.0':
            raise ValueError('wrong genuine version')
        for engine in (('reference',) if args.reference_only else ('native',) if args.native_preparation else ('reference', 'native')):
            run_engine(args, report, runner, scratch, engine)
    except (Exception, KeyboardInterrupt) as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        try:
            if runner:
                official_commands(runner, args, scratch, 'after')
            report['pins_after'] = pins(args, scratch)
        except Exception as exc:
            report['errors'].append('final pins: '+str(exc))
        shutil.rmtree(scratch)
        report['cleanup'] = dict(path=str(scratch), removed=not scratch.exists())
        try:
            validate_report(report)
            report['status'] = 'REFERENCE_PREPARATION_PASS' if args.reference_only else 'NATIVE_PREPARATION_PASS' if args.native_preparation else 'SCOPED_PAIRED_PASS'
        except Exception as exc:
            report['errors'].append('replay: '+str(exc))
        write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return int(report['status'] == 'FAIL')


def official_commands(runner, args, scratch, phase):
    for suffix, words in [('head', ['rev-parse', 'HEAD']), ('status', ['status', '--porcelain=v1', '--untracked-files=no'])]:
        row = runner.run('official.'+phase+'.'+suffix,
            ['/usr/bin/git', '--no-optional-locks', '-C', str(args.official_source)]+words, scratch)
        controlled.success(row)
        if row['stdout'].strip() != (COMMIT if suffix == 'head' else ''):
            raise ValueError('official source HEAD/status mismatch')


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
