#!/usr/bin/env python3
"""Scoped real-Git override validation; Python tests do not run either Bazel CLI.

Only private file-URL origins are used. The genuine reference may use BCR for
its built-in closure. Native-only rejection/cache/isolation probes are not parity.
"""

import base64
from contextlib import contextmanager
import datetime
import json
from pathlib import Path
import re
import shlex
import shutil
import signal
import sys
import tempfile

from validate import ROOT, COMMON_FLAGS, BUILD_FLAGS, file_evidence, reference_output_paths, write_report
from validate_modules import compare_graphs, expected_graph, parse_graph
from validate_patches import (PatchReference, RecordingRunner, REFERENCE_SHA256,
                              PROFILE, argument_parser as patch_argument_parser,
                              digest, evidence_matches, native_command, patch_text,
                              snapshot, validate_binaries)


GIT = Path('/usr/bin/git')
NS = 'opencode_gitcheck_'
DEP = NS + 'dep'
MODULE = 'module(name = "' + DEP + '", version = "7.0")\n# first café\n'
PATCHED_MODULE = MODULE.replace('first café', 'patched café')
LEAF_MODULE = MODULE + 'bazel_dep(name = "' + NS + 'leaf", version = "2.0")\n'
BUILD = ('package(default_visibility = ["//visibility:public"])\n'
         'exports_files(["data", "MODULE.bazel"])\n'
         'filegroup(name = "sources", srcs = ["data", "MODULE.bazel"])\n'
         'genrule(name = "copy", srcs = ["data"], outs = ["copy.txt"], '
         'cmd = "cat $(location data) > $@")\n')


def data(stage='first'):
    return ('header\n' + stage + ' café\nfooter\n').encode('utf-8')


def case(name, mode='graph', diagnostic=None, **options):
    return dict(name=name, mode=mode, diagnostic=diagnostic,
                kind='native_only' if name.startswith('native.') else 'differential', **options)


CASES = [
    case('graph.pinned-first-commit'),
    case('graph.patched-module-dependency', leaf=True),
    case('materialize.module-source-once', 'source', patched=True),
    case('materialize.strip-prefix', 'source', patched=True, prefix='nested/pkg'),
    case('native.warm-origin-unavailable', 'warm'),
    case('native.patch-edit-immutable', 'edit', patched=True),
    case('native.corrupt-tree-repair', 'repair'),
    case('native.git-configuration-isolation', 'poison'),
    case('native.module-name-before-publication', diagnostic='module_name', bad_module='name'),
    case('native.malformed-module-before-publication', diagnostic='module_syntax', bad_module='syntax'),
    case('native.unknown-commit', diagnostic='unknown_commit', unknown_commit=True),
    case('native.tag-before-fetch', diagnostic='tag', selector='tag'),
    case('native.branch-before-fetch', diagnostic='branch', selector='branch'),
    case('native.abbreviated-commit-before-fetch', diagnostic='commit', selector='abbreviated'),
    case('native.submodules-before-fetch', diagnostic='submodules', submodules=True),
    case('native.commands-before-fetch', diagnostic='commands', commands=True),
    case('native.external-patches-before-fetch', diagnostic='external_patches', external_patches=True),
    case('native.traversal-prefix', diagnostic='prefix_path', prefix='../escape'),
    case('native.symlink-prefix', diagnostic='prefix_symlink', prefix='linked', symlink=True),
]
PLAN = [(item['name'], item['kind']) for item in CASES]
BEFORE_FETCH = frozenset(('tag', 'branch', 'commit', 'submodules', 'commands', 'external_patches'))
# Semantic contracts plus narrowly captured first-run native diagnostics. Tests
# retain the actual stderr; accepting an unrelated failure is never a parity pass.
DIAGNOSTICS = {
    'module_name': (r'MODULE\.bazel[^\n]*(?:name[^\n]*(?:mismatch|expected|does not match)|expected[^\n]*name)'
                    r'|module name mismatch at /[^\n]*/MODULE\.bazel: requested "opencode_gitcheck_dep", declared "opencode_gitcheck_wrong"'),
    'module_syntax': (r'MODULE\.bazel[^\n]*(?:parse error|syntax error|unexpected token)[^\n]*'
                      r'|(?:parse error|syntax error|unexpected token)[^\n]*MODULE\.bazel[^\n]*'
                      # Integration-15: the location is the override repository's MODULE.bazel
                      # label, Bazel's ModuleFile location for non-registry overrides
                      # (ModuleFileFunction.java:769-776, getUnambiguousCanonicalForm), not
                      # the former native staging-tree path.
                      r'|(?m:^bazel-rs: error: Parse error: unexpected new line, expected expression\n'
                      r'--> @@opencode_gitcheck_dep\+//:MODULE\.bazel:1:1(?:\n|$))'),
    'unknown_commit': r'(?:git[^\n]*(?:unknown|missing|not found)[^\n]*commit|(?:not our ref|couldn.t find remote ref|unadvertised object)[^\n]*[0-9a-f]{40})',
    'tag': r'git[^\n]*(?:\btags?\b[^\n]*(?:unsupported|not supported|not implemented)|(?:unsupported|not supported)[^\n]*\btags?\b)',
    'branch': r'git[^\n]*(?:\bbranch(?:es)?\b[^\n]*(?:unsupported|not supported|not implemented)|(?:unsupported|not supported)[^\n]*\bbranch(?:es)?\b)',
    'commit': r'git[^\n]*commit[^\n]*(?:40|full[^\n]*(?:sha|hash)|abbreviated[^\n]*(?:unsupported|not supported))',
    'submodules': r'git[^\n]*(?:\b(?:init_)?submodules?\b[^\n]*(?:unsupported|not supported|not implemented)|(?:unsupported|not supported)[^\n]*\b(?:init_)?submodules?\b)',
    'commands': (r'(?:git|patch)[^\n]*(?:patch_cmds[^\n]*(?:unsupported|not supported)|(?:unsupported|not supported)[^\n]*patch_cmds)'
                 r'|\bpatch_cmds are unsupported for materialization; no commands are executed\b'),
    'external_patches': r'(?:git|patch)[^\n]*(?:external[^\n]*(?:unsupported|not supported|main repository)|(?:unsupported|not supported|main repository)[^\n]*external)',
    'prefix_path': r'(?:strip_prefix|strip prefix)[^\n]*(?:unsafe|traversal|parent|\.\.)',
    'prefix_symlink': (r'(?:strip_prefix|strip prefix)[^\n]*symlink|symlink[^\n]*(?:strip_prefix|strip prefix)'
                       r'|unsupported Git tree mode: symlinks and special entries are forbidden'
                       r'|invalid committed Git symlink "linked": directory target is unsupported: repository root'),
}
UNRELATED = re.compile(r'panicked|internal error|crashed|falling back|fallback to|SSLHandshake|'
                       r'certificate|connection refused|network unreachable|could not resolve|'
                       r'failed to download|could not download|authentication failed|permission denied', re.I)


def success(record):
    if (type(record['returncode']) is not int or record['returncode'] != 0 or record['timed_out']
            or record.get('spawn_error') or record.get('interrupted') or UNRELATED.search(record['stderr'])):
        raise ValueError('expected successful invocation; see runs[' + str(record['id']) + ']')
    return record['stdout']


def require_rejection(record, diagnostic):
    code, text = record['returncode'], record['stderr']
    match = re.search(DIAGNOSTICS[diagnostic], text, re.I)
    # Only the expected MODULE parse diagnostic may consume syntax-error text;
    # another parser failure elsewhere in stderr must still fail this check.
    unrelated_syntax = text[:match.start()] + text[match.end():] if diagnostic == 'module_syntax' and match else text
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record['timed_out']
            or record.get('spawn_error') or record.get('interrupted') or record['stdout'].strip()
            or UNRELATED.search(text) or not match
            or re.search(r'syntax error|unexpected token|parse error', unrelated_syntax, re.I)
            or (diagnostic != 'unknown_commit' and re.search(r'\bfatal:', text, re.I))):
        raise ValueError('expected controlled Git rejection: ' + diagnostic)


def private_environment(root):
    """Do not inherit user Git config, credentials, agents, wrappers or rc paths."""
    for name in ('home', 'xdg', 'tmp', 'hooks', 'template'):
        (root / name).mkdir(parents=True, exist_ok=True)
    for name in ('global.gitconfig', 'system.gitconfig'):
        (root / name).write_bytes(b'')
    return {'PATH': '/usr/bin:/bin', 'HOME': str(root / 'home'),
            'XDG_CONFIG_HOME': str(root / 'xdg'), 'TMPDIR': str(root / 'tmp'),
            'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8', 'USER': 'gitcheck', 'LOGNAME': 'gitcheck',
            'GIT_CONFIG_GLOBAL': str(root / 'global.gitconfig'),
            'GIT_CONFIG_SYSTEM': str(root / 'system.gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_TERMINAL_PROMPT': '0', 'GIT_CONFIG_COUNT': '0',
            'GIT_AUTHOR_NAME': 'Private Fixture', 'GIT_AUTHOR_EMAIL': 'fixture@example.invalid',
            'GIT_COMMITTER_NAME': 'Private Fixture', 'GIT_COMMITTER_EMAIL': 'fixture@example.invalid',
            'GIT_AUTHOR_DATE': '2001-01-01T00:00:00+0000',
            'GIT_COMMITTER_DATE': '2001-01-01T00:00:00+0000',
            'DO_NOT_TRACK': '1', 'BAZEL_METRICS': '0', 'NEXT_TELEMETRY_DISABLED': '1'}


@contextmanager
def environment(runner, values):
    previous = runner.env
    runner.env = dict(values)
    try:
        yield
    finally:
        runner.env = previous


def source_patch(stage='patched'):
    # Offset, but all context must match exactly; application twice must fail.
    return (patch_text('MODULE.bazel', MODULE, PATCHED_MODULE)
            + patch_text('data', data().decode(), data(stage).decode()).replace(
                '@@ -1,3 +1,3 @@', '@@ -8,3 +8,3 @@'))


def override_attributes(item, origin, first, marker):
    attrs = {'module_name': DEP, 'remote': origin.as_uri(), 'commit': first,
             'patches': ['//:git-local.patch'] if item.get('leaf') or item.get('patched') else [],
             'patch_strip': 1}
    if item.get('selector') in ('tag', 'branch'):
        del attrs['commit']
        attrs[item['selector']] = 'fixture-main'
    elif item.get('selector') == 'abbreviated':
        attrs['commit'] = first[:12]
    if item.get('unknown_commit'):
        attrs['commit'] = 'f' * 40
    if item.get('prefix'):
        attrs['strip_prefix'] = item['prefix']
    if item.get('submodules'):
        attrs['init_submodules'] = True
    if item.get('commands'):
        attrs['patch_cmds'] = ['printf forbidden > ' + shlex.quote(str(marker))]
    if item.get('external_patches'):
        attrs['patches'] = ['@@' + DEP + '+//:external.patch']
    return attrs


def root_module(attrs):
    literal = lambda value: 'True' if value is True else json.dumps(value, ensure_ascii=False)
    return ('module(name = "' + NS + 'root", version = "1.0")\n'
            'bazel_dep(name = "' + DEP + '", version = "99.0", repo_name = "gitdep")\n'
            'git_override(' + ', '.join(key + ' = ' + literal(attrs[key]) for key in sorted(attrs)) + ')\n')


def materialize(case_root, workspace, item, runner, check):
    files = {}

    def put(path, contents):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents.encode('utf-8') if isinstance(contents, str) else contents)
        files[path] = path.read_bytes()

    origin = case_root / 'origin'
    origin.mkdir(parents=True)
    setup_root = case_root / 'git-setup'
    setup_env = private_environment(setup_root)
    check['git_environment'] = setup_env
    check['git_setup_run_ids'] = {}

    def git(phase, *args):
        with environment(runner, setup_env):
            result = runner.run(item['name'] + '.git.' + phase,
                                [str(GIT), '-c', 'core.hooksPath=' + str(setup_root / 'hooks'),
                                 '-c', 'init.templateDir=' + str(setup_root / 'template'), *args],
                                origin, timeout=120)
        check['git_setup_run_ids'][phase] = result['id']
        return success(result).strip()

    git('init', 'init', '--initial-branch=fixture-main', '--object-format=sha1', '.')
    prefix = 'nested/pkg' if item.get('prefix') == 'nested/pkg' else ''
    source = origin / prefix
    module = (MODULE.replace(DEP, NS + 'wrong') if item.get('bad_module') == 'name'
              else 'module(name =\n' if item.get('bad_module') == 'syntax' else MODULE)
    for name, contents in {'MODULE.bazel': module, 'BUILD.bazel': BUILD, 'data': data()}.items():
        put(source / name, contents)
    if prefix:
        put(origin / 'outside.txt', 'must not enter selected source tree\n')
        put(origin / 'MODULE.bazel', 'module(name = "outside_must_not_be_evaluated")\n')
    if item.get('symlink'):
        (origin / 'linked').symlink_to('.', target_is_directory=True)
    git('add-first', 'add', '--all', '--', '.')
    git('commit-first', 'commit', '--no-gpg-sign', '-m', 'first pinned fixture')
    first = git('sha-first', 'rev-parse', '--verify', 'HEAD')
    put(source / 'data', data('second'))
    git('add-second', 'add', '--', 'data' if not prefix else prefix + '/data')
    git('commit-second', 'commit', '--no-gpg-sign', '-m', 'second unselected fixture')
    second = git('sha-second', 'rev-parse', '--verify', 'HEAD')
    first_bytes = git('show-first', 'show', first + ':' + (prefix + '/' if prefix else '') + 'data')
    second_bytes = git('show-second', 'show', second + ':' + (prefix + '/' if prefix else '') + 'data')
    if (not re.fullmatch('[0-9a-f]{40}', first) or not re.fullmatch('[0-9a-f]{40}', second)
            or first == second or first_bytes != data().decode().strip()
            or second_bytes != data('second').decode().strip()):
        raise ValueError('real Git fixture did not produce two distinct proven commits')
    check.update(first_commit=first, second_commit=second, origin=origin.as_uri())
    registry = case_root / 'registry'
    put(registry / 'bazel_registry.json', '{}\n')
    # No registry entry for DEP, including its declared 99.0 version.
    leaf = registry / 'modules' / (NS + 'leaf')
    put(leaf / 'metadata.json', '{"versions":["2.0"],"yanked_versions":{}}\n')
    put(leaf / '2.0/MODULE.bazel', 'module(name = "' + NS + 'leaf", version = "2.0")\n')
    absent = case_root / 'leaf-archive-deliberately-absent.tar'
    put(leaf / '2.0/source.json', json.dumps({'url': absent.as_uri(), 'integrity':
        'sha256-' + base64.b64encode(bytes.fromhex(digest(b'absent leaf archive\n'))).decode()}) + '\n')
    put(workspace / 'BUILD.bazel', '')
    patch = workspace / 'git-local.patch'
    text = (patch_text('MODULE.bazel', MODULE, LEAF_MODULE) if item.get('leaf')
            else source_patch() if item.get('patched') else '')
    put(patch, text)
    marker = case_root / 'PATCH_COMMAND_MUST_NOT_RUN'
    attrs = override_attributes(item, origin, first, marker)
    put(workspace / 'MODULE.bazel', root_module(attrs))
    check['override'] = attrs
    return dict(origin=origin, registry=registry, files=files, patch=patch, marker=marker,
                absent_archive=absent, source=source)


def graph_expected(item):
    # JSON renders the empty selected override version as "_" in the key;
    # version remains the declared MODULE version, not the root's request 99.0.
    node = {'module': 'dep@_', 'version': '7.0', 'alias': 'gitdep'}
    if item.get('leaf'):
        node['dependencies'] = [{'module': 'leaf@2.0'}]
    return expected_graph({'expected': [node]}, {'namespace': NS})


def canonical(target):
    return target.replace('@gitdep//', '@@' + DEP + '+//', 1)


def query_result(record, native=False):
    # Integration-14 (N39): native query prints Bazel's display form, exactly
    # like `bazel query`; the root-visible dep+ repository is @gitdep//.
    labels = success(record).splitlines()
    expected = ['@gitdep//:copy', '@gitdep//:sources']
    if sorted(map(canonical, labels)) != sorted(map(canonical, expected)) or (native and labels != expected):
        raise ValueError('query did not return exactly the two fixture rule labels')
    return success(record)


def native_paths(record, target, scratch):
    paths = []
    for line in success(record).splitlines():
        fields = line.split('\t')
        if len(fields) != 2 or fields[0] != canonical(target):
            raise ValueError('expected canonical LABEL<TAB>ABSOLUTE_OUTPUT_PATH')
        path = Path(fields[1])
        if (not path.is_absolute() or '..' in path.parts or path in paths
                or not path.resolve().is_relative_to(scratch.resolve())):
            raise ValueError('unsafe or duplicate native output path')
        paths.append(path)
    return paths


def expected_sources(item, edited=False):
    module = LEAF_MODULE if item.get('leaf') else PATCHED_MODULE if item.get('patched') else MODULE
    return {'MODULE.bazel': module.encode(), 'data': data('edited' if edited else 'patched' if item.get('patched') else 'first')}


def trees_directory(cache):
    # CLI --cache-dir owns repositories/; Rust source-cache Git owns git/trees.
    return cache / 'repositories/git/trees'


def no_publication(cache):
    entries = list(trees_directory(cache).glob('*'))
    if entries:
        raise ValueError('Git rejection left a published or staged tree: ' + str(entries))


def tree_manifest(root, expected):
    if not root.is_dir() or root.is_symlink():
        raise ValueError('missing real materialized tree')
    if any(path.name == '.git' for path in root.rglob('*')) or (root / 'outside.txt').exists():
        raise ValueError('Git metadata or files outside strip_prefix leaked into sources')
    return {name: file_evidence(root / name, contents, include_bytes=True) for name, contents in expected.items()}


def reference_source_root(paths, output_base):
    """Resolve cquery files, not a guessed publication tree or execroot symlink.

    Bazel's external repo link may target its private contents cache. With
    strip_prefix, individual source links can target .tmp_git_root/nested/pkg.
    Inspect the selected real source directory, not that wrapper's checkout.
    """
    logical_root = output_base / 'execroot/_main/external' / (DEP + '+')
    if (not paths or len(set(paths)) != len(paths)
            or any(path.parent != logical_root for path in paths)
            or {path.name for path in paths} != {'MODULE.bazel', 'data'}):
        raise ValueError('reference source outputs have the wrong repository identity')
    allowed = (output_base.resolve(), (output_base.parent / 'repository-contents').resolve())
    if not all(directory.is_relative_to(output_base.parent.resolve()) for directory in allowed):
        raise ValueError('reference cache location escapes private scratch')
    resolved = [path.resolve(strict=True) for path in paths]
    if (len({path.parent for path in resolved}) != 1
            or any(path.name != original.name or not path.is_file()
                   or not any(path.is_relative_to(directory) for directory in allowed)
                   for original, path in zip(paths, resolved))):
        raise ValueError('reference source links escape its private caches or selected directory')
    return resolved[0].parent


def graph_tree(cache, expected):
    candidates = [path.parent for path in trees_directory(cache).rglob('MODULE.bazel')
                  if not path.is_symlink() and path.read_bytes() == expected['MODULE.bazel']]
    if len(candidates) != 1:
        raise ValueError('graph must publish one unambiguous final Git MODULE tree')
    root = candidates[0]
    return root, tree_manifest(root, expected)


def output_manifest(paths, expected, source=False):
    if len(paths) != len(expected) or len(set(paths)) != len(paths):
        raise ValueError('incomplete or duplicate artifact inventory')
    if source and len({path.parent for path in paths}) != 1:
        raise ValueError('source artifacts do not share a repository root')
    result = {}
    for path in paths:
        if path.name not in expected or path.name in result:
            raise ValueError('unexpected artifact identity')
        if not source and ('external', DEP + '+', 'copy.txt') != path.parts[-3:]:
            raise ValueError('wrong generated repository identity')
        result[path.name] = file_evidence(path, expected[path.name], include_bytes=True)
    return result


def poison_configuration(root, origin):
    """Inert private payloads: only a broken child-isolation boundary can run them."""
    root.mkdir(parents=True)
    markers = [root / name for name in ('HELPER_RAN', 'HOOK_RAN', 'TEMPLATE_RAN', 'SSH_RAN')]
    payloads = {}
    for filename, marker in zip(('helper', 'hooks/post-checkout', 'template/hooks/post-checkout', 'ssh'), markers):
        path = root / filename
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('#!/bin/sh\nprintf forbidden > ' + shlex.quote(str(marker)) + '\nexit 91\n', encoding='utf-8')
        path.chmod(0o755)
        payloads[path] = path.read_bytes()
    config = root / 'poison.gitconfig'
    config.write_text('[url "ssh://127.0.0.1:1/forbidden/"]\n\tinsteadOf = ' + origin.as_uri()
                      + '\n[credential]\n\thelper = ' + str(root / 'helper')
                      + '\n[core]\n\thooksPath = ' + str(root / 'hooks')
                      + '\n[init]\n\ttemplateDir = ' + str(root / 'template') + '\n', encoding='utf-8')
    payloads[config] = config.read_bytes()
    return {'GIT_CONFIG_GLOBAL': str(config), 'GIT_SSH_COMMAND': shlex.quote(str(root / 'ssh'))}, markers, payloads


def phases(item):
    if item['diagnostic']:
        return ['result']
    if item['mode'] == 'graph':
        return ['graph']
    result = ['query', 'sources', 'copy']
    if item['mode'] in ('warm', 'edit', 'repair'):
        result += ['again.sources', 'again.copy']
    return result


def run_case(args, runner, scratch, workspace, reference, item):
    check = {'name': item['name'], 'kind': item['kind'], 'input': item, 'status': 'FAIL',
             'parity_claim': False, 'run_ids': {}, 'artifacts': {}}
    runner.report['checks'].append(check)
    fixture = None
    cache = scratch / 'native' / item['name']

    def invoke(engine, phase, command, target=None):
        name = item['name'] + '.' + engine + '.' + phase
        result = (runner.run(name, native_command(args.native, cache, fixture['registry'], command, target), workspace)
                  if engine == 'native' else reference.run(name, command,
                      *(['--output=files', target] if command == 'cquery' else [target] if target else [])))
        check['run_ids'][engine + '.' + phase] = result['id']
        return result

    def build(engine, phase, edited=False):
        source = phase.endswith('sources')
        target = '@gitdep//:' + ('sources' if source else 'copy')
        result = invoke(engine, phase, 'build', target)
        if engine == 'native':
            paths = native_paths(result, target, scratch)
        else:
            success(result)
            paths = reference_output_paths(success(invoke(engine, phase + '.files', 'cquery', target)),
                                           workspace, reference.output_base)
        expected = expected_sources(item, edited)
        root = reference_source_root(paths, reference.output_base) if source and engine == 'reference' else None
        check['artifacts'][engine + '.' + phase] = output_manifest(
            paths, expected if source else {'copy.txt': expected['data']}, source)
        if source:
            if engine == 'native':
                root = paths[0].parent
                if not root.resolve().is_relative_to(trees_directory(cache).resolve()):
                    raise ValueError('native source artifact is outside owned Git tree cache')
                tree_manifest(root, expected)
            else:
                check['reference_source_root'] = str(root)
                check['reference_source_manifest'] = tree_manifest(root, expected)
            return root

    try:
        fixture = materialize(scratch / 'cases' / item['name'], workspace, item, runner, check)
        check['files_before'] = snapshot(fixture['files'])
        reference.registry = fixture['registry']
        engines = ('native', 'reference') if item['kind'] == 'differential' else ('native',)
        if item['diagnostic']:
            hidden = fixture['origin'].with_name('origin-unavailable')
            before_fetch = item['diagnostic'] in BEFORE_FETCH
            if before_fetch:
                fixture['origin'].rename(hidden)
            try:
                require_rejection(invoke('native', 'result', 'mod'), item['diagnostic'])
                no_publication(cache)
                if before_fetch and list((cache / 'repositories/git').glob('*')):
                    raise ValueError('unsupported option created Git cache state before validation')
                check.update(no_publication=True, origin_unavailable=before_fetch)
            finally:
                if before_fetch:
                    hidden.rename(fixture['origin'])
        elif item['mode'] == 'graph':
            records = {engine: invoke(engine, 'graph', 'mod') for engine in engines}
            for record in records.values():
                success(record)
            compare_graphs(parse_graph(records['native']), parse_graph(records['reference']), graph_expected(item))
            root, evidence = graph_tree(cache, expected_sources(item))
            check.update(graph_tree=str(root), graph_manifest=evidence)
        else:
            query_stdout = {}
            for engine in engines:
                values, markers, payloads = {}, [], {}
                if item['mode'] == 'poison':
                    values, markers, payloads = poison_configuration(scratch / 'poison', fixture['origin'])
                    check['poison_payloads'] = snapshot(payloads)
                    check['poison_environment'] = values
                old_env = dict(runner.env)
                try:
                    with environment(runner, dict(old_env, **values)):
                        query_stdout[engine] = query_result(invoke(engine, 'query', 'query', '@gitdep//:all'),
                                                            native=engine == 'native')
                        if len(set(query_stdout.values())) > 1:
                            raise ValueError('native query stdout differs from genuine Bazel display-form stdout')
                        root = build(engine, 'sources')
                        build(engine, 'copy')
                finally:
                    if item['mode'] == 'poison':
                        check['environment_restored'] = runner.env == old_env
                        check['poison_markers_absent'] = all(not path.exists() for path in markers)
                        check['poison_payloads_after'] = snapshot(payloads)
                        if not check['environment_restored'] or not check['poison_markers_absent']:
                            raise ValueError('native Git inherited malicious configuration or environment was not restored')
                if item['mode'] in ('warm', 'edit', 'repair'):
                    check['cache_root_before'] = str(root)
                    original = fixture['patch'].read_bytes()
                    hidden = fixture['origin'].with_name('origin-unavailable')
                    try:
                        if item['mode'] == 'warm':
                            fixture['origin'].rename(hidden)
                            check['warm_origin_unavailable'] = not fixture['origin'].exists()
                        elif item['mode'] == 'edit':
                            fixture['patch'].write_text(source_patch('edited'), encoding='utf-8')
                            check['edited_patch'] = file_evidence(fixture['patch'], include_bytes=True)
                        else:
                            (root / 'data').write_bytes(b'corrupt cache probe\n')
                            check['corrupt_before'] = file_evidence(root / 'data', include_bytes=True)
                        changed = build(engine, 'again.sources', item['mode'] == 'edit')
                        build(engine, 'again.copy', item['mode'] == 'edit')
                        check['cache_root_after'] = str(changed)
                        if item['mode'] == 'edit':
                            if root == changed:
                                raise ValueError('patch edit reused the old immutable Git tree')
                            check['old_tree_after'] = tree_manifest(root, expected_sources(item))
                    finally:
                        fixture['patch'].write_bytes(original)
                        if hidden.exists():
                            hidden.rename(fixture['origin'])
        check.update(status='PASS', parity_claim=item['kind'] == 'differential')
    except (OSError, ValueError) as exc:
        check['error'] = str(exc)
    finally:
        if fixture is not None:
            try:
                check['files_after'] = snapshot(fixture['files'])
                check['inputs_unchanged'] = check['files_before'] == check['files_after']
                check['command_marker_absent'] = not fixture['marker'].exists()
                check['leaf_archive_absent'] = not fixture['absent_archive'].exists()
                if not all(check[key] for key in ('inputs_unchanged', 'command_marker_absent', 'leaf_archive_absent')):
                    raise ValueError('fixture inputs mutated, command executed, or leaf archive materialized')
            except (OSError, ValueError, KeyError) as exc:
                check.update(status='FAIL', parity_claim=False, error=str(exc))


def manifest_matches(value, expected):
    return set(value) == set(expected) and all(evidence_matches(value[name], contents) for name, contents in expected.items())


def expected_argv(report, item, key):
    """Rebuild exact CLI wiring from the immutable case plan, not recorded argv."""
    scratch = Path(report['scratch_directory'])
    registry = scratch / 'cases' / item['name'] / 'registry'
    engine, phase = key.split('.', 1)
    command = ('mod' if phase in ('graph', 'result') else 'query' if phase == 'query'
               else 'cquery' if phase.endswith('.files') else 'build')
    target = (None if command == 'mod' else '@gitdep//:all' if command == 'query'
              else '@gitdep//:sources' if 'sources' in phase else '@gitdep//:copy')
    if engine == 'native':
        return native_command(Path(report['configuration']['native']), scratch / 'native' / item['name'],
                              registry, command, target)
    ref = PatchReference(Path(report['configuration']['reference']), scratch / 'workspace', scratch, None, 'git')
    ref.registry = registry
    if command == 'mod':
        return ref.startup + ['mod', 'graph'] + list(COMMON_FLAGS) + ref.flags() + PROFILE
    return (ref.startup + [command] + list(COMMON_FLAGS)
            + (list(BUILD_FLAGS) if command in ('build', 'cquery') else []) + ref.flags()
            + (['--output=files'] if command == 'cquery' else []) + [target])


def report_passes(report):
    """Revalidate exact case/run wiring, bytes and cleanup, not just PASS strings."""
    try:
        if ([(c['name'], c['kind']) for c in report['checks']] != PLAN or report['errors']
                or report['planned_checks'] != CASES
                or report['case_inputs_sha256'] != digest(json.dumps(CASES, sort_keys=True).encode())
                or any(report[key] is not True for key in ('scratch_cleaned', 'reference_shutdown_attempted', 'sources_unchanged'))
                or report['sources_before'] != report['sources_after']
                or not all(evidence_matches(e) for e in report['sources_before'].values())
                or report['reference_release'] != 'bazel 9.3.0'
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['completed_differential_cases'] != 4 or report['completed_checks'] != len(CASES)):
            return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            for field in ('stdout', 'stderr'):
                raw = (base64.b64decode(record[field + '_base64'], validate=True) if field + '_base64' in record else record[field].encode())
                if digest(raw) != record[field + '_sha256']:
                    return False
        used = set()

        def linked(index, name):
            if type(index) is not int or index < 0 or index in used:
                raise ValueError('invalid or reused run ID')
            used.add(index)
            record = runs[index]
            if record['name'] != name or not record['argv'] or not record['cwd']:
                raise ValueError('run identity mismatch')
            return record

        success(linked(0, 'reference.version'))
        if runs[0]['stdout'].strip() != 'bazel 9.3.0':
            return False
        success(linked(len(runs) - 1, 'git.shutdown'))
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        ref = PatchReference(Path(report['configuration']['reference']), workspace, scratch, None, 'git')
        if (runs[0]['argv'] != ref.startup + ['version', '--gnu_format']
                or runs[-1]['argv'] != ref.startup + ['shutdown'] + list(COMMON_FLAGS)
                or runs[0]['cwd'] != str(workspace) or runs[-1]['cwd'] != str(workspace)):
            return False
        for item, check in zip(CASES, report['checks']):
            if (check['input'] != item or check['status'] != 'PASS' or check.get('error')
                    or check['parity_claim'] != (item['kind'] == 'differential')
                    or any(check[key] is not True for key in ('inputs_unchanged', 'command_marker_absent', 'leaf_archive_absent'))
                    or not check['files_before'] or check['files_before'] != check['files_after']
                    or not all(evidence_matches(e) for e in check['files_before'].values())):
                return False
            setup_phases = ['init', 'add-first', 'commit-first', 'sha-first', 'add-second', 'commit-second',
                            'sha-second', 'show-first', 'show-second']
            if set(check['git_setup_run_ids']) != set(setup_phases):
                return False
            setup = {}
            case_root = scratch / 'cases' / item['name']
            origin = case_root / 'origin'
            setup_root = case_root / 'git-setup'
            prefix = 'nested/pkg/' if item.get('prefix') == 'nested/pkg' else ''
            commands = {
                'init': ['init', '--initial-branch=fixture-main', '--object-format=sha1', '.'],
                'add-first': ['add', '--all', '--', '.'],
                'commit-first': ['commit', '--no-gpg-sign', '-m', 'first pinned fixture'],
                'sha-first': ['rev-parse', '--verify', 'HEAD'],
                'add-second': ['add', '--', prefix + 'data'],
                'commit-second': ['commit', '--no-gpg-sign', '-m', 'second unselected fixture'],
                'sha-second': ['rev-parse', '--verify', 'HEAD'],
                'show-first': ['show', check['first_commit'] + ':' + prefix + 'data'],
                'show-second': ['show', check['second_commit'] + ':' + prefix + 'data'],
            }
            for phase in setup_phases:
                record = linked(check['git_setup_run_ids'][phase], item['name'] + '.git.' + phase)
                setup[phase] = record
                success(record)
                expected = [str(GIT), '-c', 'core.hooksPath=' + str(setup_root / 'hooks'),
                            '-c', 'init.templateDir=' + str(setup_root / 'template'), *commands[phase]]
                if record['argv'] != expected or record['timeout_seconds'] != 120 or record['cwd'] != str(origin):
                    return False
            for which in ('first', 'second'):
                sha = check[which + '_commit']
                if not re.fullmatch('[0-9a-f]{40}', sha) or setup['sha-' + which]['stdout'].strip() != sha:
                    return False
                if setup['show-' + which]['stdout'].encode() != data(which):
                    return False
            if check['first_commit'] == check['second_commit']:
                return False
            attrs = check['override']
            if (attrs != override_attributes(item, origin, check['first_commit'], case_root / 'PATCH_COMMAND_MUST_NOT_RUN')
                    or attrs['remote'] != check['origin'] or not attrs['remote'].startswith('file:///')
                    or not evidence_matches(check['files_before'][str(workspace / 'MODULE.bazel')], root_module(attrs).encode())):
                return False
            patch = (patch_text('MODULE.bazel', MODULE, LEAF_MODULE) if item.get('leaf')
                     else source_patch() if item.get('patched') else '')
            if not evidence_matches(check['files_before'][str(workspace / 'git-local.patch')], patch.encode()):
                return False
            if not item.get('selector') and not item.get('unknown_commit') and attrs['commit'] != check['first_commit']:
                return False
            engines = ('native', 'reference') if item['kind'] == 'differential' else ('native',)
            wanted = {engine + '.' + phase for engine in engines for phase in phases(item)}
            if item['mode'] == 'source':
                wanted.update(('reference.sources.files', 'reference.copy.files'))
            if set(check['run_ids']) != wanted:
                return False
            records = {}
            for key, index in check['run_ids'].items():
                record = linked(index, item['name'] + '.' + key)
                records[key] = record
                if record['argv'] != expected_argv(report, item, key) or record['cwd'] != str(workspace):
                    return False
                if item['diagnostic']:
                    require_rejection(record, item['diagnostic'])
                else:
                    success(record)
                    if key.endswith('.query'):
                        query_result(record, key.startswith('native.'))
            if ('native.query' in records and 'reference.query' in records
                    and success(records['native.query']) != success(records['reference.query'])):
                return False
            if item['diagnostic']:
                if (check['no_publication'] is not True or check['artifacts']
                        or check['origin_unavailable'] != (item['diagnostic'] in BEFORE_FETCH)):
                    return False
            elif item['mode'] == 'graph':
                compare_graphs(parse_graph(records['native.graph']), parse_graph(records['reference.graph']), graph_expected(item))
                if check['artifacts'] or not manifest_matches(check['graph_manifest'], expected_sources(item)):
                    return False
            else:
                artifact_keys = {key for key in wanted if not key.endswith(('.query', '.files'))}
                if set(check['artifacts']) != artifact_keys:
                    return False
                for key, value in check['artifacts'].items():
                    expected = expected_sources(item, item['mode'] == 'edit' and '.again.' in key)
                    if not manifest_matches(value, expected if key.endswith('sources') else {'copy.txt': expected['data']}):
                        return False
                    paths = (native_paths(records[key], '@gitdep//:' + ('sources' if key.endswith('sources') else 'copy'), scratch)
                             if key.startswith('native.') else reference_output_paths(
                                 success(records[key + '.files']), workspace, ref.output_base))
                    if sorted(map(str, paths)) != sorted(e['path'] for e in value.values()):
                        return False
                    if any(Path(e['path']).name != name for name, e in value.items()):
                        return False
                if item['kind'] == 'differential':
                    root = Path(check['reference_source_root'])
                    allowed = (ref.output_base, scratch / 'repository-contents')
                    if (not root.is_absolute() or '..' in root.parts
                            or not any(root.is_relative_to(directory) for directory in allowed)
                            or not manifest_matches(check['reference_source_manifest'], expected_sources(item))
                            or any(e['path'] != str(root / name) for name, e in check['reference_source_manifest'].items())):
                        return False
            if item['mode'] == 'warm' and check['warm_origin_unavailable'] is not True:
                return False
            if item['mode'] == 'edit' and (check['cache_root_before'] == check['cache_root_after']
                    or not evidence_matches(check['edited_patch'], source_patch('edited').encode())
                    or not manifest_matches(check['old_tree_after'], expected_sources(item))):
                return False
            if item['mode'] == 'repair' and not evidence_matches(check['corrupt_before'], b'corrupt cache probe\n'):
                return False
            if item['mode'] == 'poison' and (check['environment_restored'] is not True
                    or check['poison_markers_absent'] is not True or len(check['poison_payloads']) != 5
                    or check['poison_payloads'] != check['poison_payloads_after']
                    or not all(evidence_matches(e) for e in check['poison_payloads'].values())):
                return False
        return used == set(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError):
        return False


def argument_parser():
    parser = patch_argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/git-validation.json')
    return parser


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    sources = [Path(__file__).resolve(), ROOT / 'scripts/validate.py', ROOT / 'scripts/validate_modules.py',
               ROOT / 'scripts/validate_patches.py', ROOT / 'tests/test_validate_git.py']
    if (args.output in [args.native, args.reference, GIT, *sources]
            or any(args.output.is_relative_to(ROOT / directory) for directory in ('scripts', 'tests'))):
        raise ValueError('report must not overwrite executables, harness sources, or fixtures')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {'schema_version': 1, 'status': 'FAIL', 'checks': [], 'runs': [], 'errors': [], 'cleanup': [],
              'scratch_cleaned': False, 'reference_shutdown_attempted': False, 'sources_unchanged': False,
              'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'configuration': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              'planned_checks': CASES, 'case_inputs_sha256': digest(json.dumps(CASES, sort_keys=True).encode()),
              'reference_sha256_required': REFERENCE_SHA256, 'reference_version_required': '9.3.0',
              'test_commands_executed': False,
              'limitations': ['Scoped pinned Git overrides, not general Git repository-rule parity.',
                              'Private real Git file URLs only; HTTPS and lockfile policy need Rust integration tests.',
                              '19 checks: four differential, fifteen native-only; captured diagnostic regressions are not new CLI evidence.',
                              'Selected version is empty, declared module version remains 7.0; root requests 99.0.',
                              'Exact UTF-8 source/MODULE bytes after strip_prefix and once-only patches; no registry MODULE reset.',
                              'Reference cquery source links resolve within private reference caches; inspect selected sources, not .tmp_git_root wrappers.',
                              'Native Git child deadline/output bounds/group kill need Rust tests; no fake Git here.',
                              'Poison only native child environment; no changes to user HOME, config, credentials or wrappers.',
                              'One official reference server with private genuine caches; BCR for built-in closure only.',
                              'No reference warm-cache parity claim; no network sandbox or absence-of-process audit claim.']}
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(sources)
        report['executables'] = validate_binaries(args)
        report['executables']['git'] = file_evidence(GIT)
        scratch = Path(tempfile.mkdtemp(prefix='git-validation-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        runner.env = private_environment(scratch / 'environment')
        report['invocation_environment'] = dict(runner.env)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        reference = PatchReference(args.reference, workspace, scratch, runner, 'git')
        reference.started = True
        report['reference_release'] = success(runner.run('reference.version',
            reference.startup + ['version', '--gnu_format'], workspace)).strip()
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
                shutil.rmtree(scratch)
                report['scratch_cleaned'] = not scratch.exists()
            except OSError as exc:
                report['errors'].append('scratch cleanup: ' + str(exc))
        try:
            report['sources_after'] = snapshot(sources)
            report['sources_unchanged'] = report.get('sources_before') == report['sources_after']
        except OSError as exc:
            report['errors'].append('source evidence: ' + str(exc))
        report['completed_differential_cases'] = sum(c['kind'] == 'differential' and c['status'] == 'PASS' for c in report['checks'])
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['build_commands_executed'] = any('build' in r['argv'] for r in report['runs'])
        if report_passes(report):
            report['status'] = 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print('{} (scoped real-Git fixtures only): {}'.format(report['status'], args.output))
    return 130 if interrupted else 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
