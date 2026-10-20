#!/usr/bin/env python3
"""N2 bounded Label passthrough harness. Preparation is NOT native acceptance.

No interpreter emulation, fallback, ambient environment, or ignored fixture input.
Pointer identity/frozen-owner lifetime remain Rust unit obligations. Invalid
(unmapped) Labels are paired: both engines construct and pass them through, and
the getter rejects with Bazel's text (integration-15; formerly native rejected
construction eagerly, an intentional divergence).
Both execution modes require the parent's freeze gate: bindings include Rust.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'tests/fixtures/label_passthrough'
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
REFERENCE_SHA = 'd302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8'
OFFICIAL_COMMIT = 'e89755babf3e941bc1e9d8120b1cd48854ab9dfa'
PLATFORM_SHA = '3384eb1c30762704fbe38e440204e114154086c8fc8a8c2e3e28441028c019a8'
OFFICIAL = {'src/main/java/' + p: h for p, h in {
    'com/google/devtools/build/lib/analysis/starlark/StarlarkRuleClassFunctions.java':
        '9f7adf40e847abbe26eb8ffe6c6ef476ffd08a6b657290226bd0f7a1990ea496',
    'com/google/devtools/build/lib/cmdline/Label.java':
        '9845de146667f32459dd693dd7eb7197e5dc3b640ff497891874c629c7d14417',
    'com/google/devtools/build/lib/starlarkbuildapi/StarlarkRuleFunctionsApi.java':
        'd831393e6f96ae3c3effffcd7bee9e5e2a8edf3acdfae7cbbafe302dda18f306',
    'net/starlark/java/annot/Param.java':
        '3014de7bc7fb2bb40b8f1e8f0ec648bd923eb5777963c9d21111e0dfcae28104',
    'net/starlark/java/eval/BuiltinFunction.java':
        '8e5deff6ffe25900a2d049914f50b5c15c76dd65cb40e50c1ae68ce4a909b1a2',
    'net/starlark/java/eval/MethodLibrary.java':
        '97dbe205311c763385d88045ceac925faffffc77d3a0bc2217e2cf1eec6f6c27',
    'net/starlark/java/eval/ParamDescriptor.java':
        '1d81b99b244840b36210e815740bcf8e9d0bbe7fdba2968ed7cf098ddd5075f4',
}.items()}
BAD_TYPES = {'none': ('None', 'NoneType'), 'bool': ('True', 'bool'),
             'int': ('1', 'int'), 'list': ('[]', 'list'), 'dict': ('{}', 'dict'),
             'struct': ('struct(x=1)', 'struct')}
CALLS = {**{'wrong-type-' + n: 'Label(' + v[0] + ')' for n, v in BAD_TYPES.items()},
         'missing': 'Label()', 'extra': 'Label("//:one", "//:two")',
         'keyword-string': 'Label(input="//:same")',
         'keyword-label': 'Label(input=Label("//:same"))',
         'keyword-native-name': 'Label(label_string="//:same")',
         'keyword-unknown': 'Label(unknown="//:same")',
         'positional-plus-keyword': 'Label("//:one", input="//:two")'}
INVALID = ('invalid-passthrough', 'invalid-getter-after-passthrough')
QUERIES = ('positive', *CALLS, *INVALID)
BUILDS = ('custom', 'extension')
DATA = b'@@//:same\n@@dep+//:same\ndep+\n'
REPOSITORY_FILES = {'BUILD.bazel': b'exports_files(["data.txt"])\n', 'data.txt': DATA, 'REPO.bazel': b''}
COPY_ROWS = [(n, repo) for n, repo in (
    ('root-fresh', ''), ('external-frozen-root-reader', 'dep+'),
    ('external-to-root-helper', 'dep+'), ('root-to-external-helper', ''),
    ('external-shared-to-root', 'other+'), ('root-shared-to-external', 'dep+'),
    ('root-local-string', ''), ('external-local-string', 'dep+'),
    ('external-shared-string', 'other+'), ('nested-copy', 'dep+'))]
FORBIDDEN = re.compile(r'fallback|fall(?:ing|s|en)?[\s-]+back|delegat(?:e|ing|ion)|not implemented|unsupported|not supported|'
    r'panicked|segmentation fault|fatal error|internal error|outofmemory|java\.lang\.[\w.]*Exception|'
    r'permission denied|connection (?:refused|reset)|failed to (?:download|fetch)|error downloading|'
    r'could not resolve|syntax error|Variable .+ not found|name .+ is not defined|'
    r'has no attribute|Unknown attribute|No such file or directory', re.I)
# Query-only guard, NOT the positive build/cache oracle. Reject action reports
# even if their count is zero: no execution/cache summary belongs to a query.
ACTION = re.compile(r'\b\d+\s+(?:(?:(?:total|cached|remote|local)\s+)?actions?\b|(?:remote\s+|disk\s+|local\s+)?cache\s+hits?\b|process(?:es)?\b)|'
    r'\bcache[\s-]+hits?\b|\bactions?\s+executed\b|\b(?:total\s+)?actions?\s*[:=]\s*\d+|'
    r'\b(?:executing|executed)\s+(?:\d+\s+)?actions?\b|'
    r'^INFO: Build completed successfully\b|^SUBCOMMAND:|^LP_.*CALLBACK$', re.M | re.I)
# src/package.rs appends this literal to loading errors; src/main.rs prints the
# returned error with eprintln!, supplying exactly the final LF. Both source
# files are in source_paths()/the frozen executable binding. This is NOT a
# general exception for unsupported APIs, builds, successful calls or reference.
# Since the native-rule-class increment (docs/NATIVE_BUILD_RULES.md) the footer
# names the registered native rule classes and accepts select() at loading.
# Captures retained from earlier increments carry the legacy literal; either
# exact, terminal footer is accepted, nothing else.
NATIVE_LOADING_FOOTER = ('Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md '
                         'and Starlark rule() definitions. select() resolves against modelled '
                         'config_setting values (docs/CONFIGURATION.md).\n')
# Footer of captures made between the native-rule-class and configuration increments.
PREVIOUS_NATIVE_LOADING_FOOTER = ('Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md '
                                  'and Starlark rule() definitions. select() values load, but native '
                                  'analysis does not resolve them.\n')
LEGACY_NATIVE_LOADING_FOOTER = ('Supported rules: filegroup, genrule, and local custom rules. '
                                'select and non-local repositories remain unsupported.\n')
NATIVE_LOADING_FOOTERS = (NATIVE_LOADING_FOOTER, PREVIOUS_NATIVE_LOADING_FOOTER,
                          LEGACY_NATIVE_LOADING_FOOTER)
FOOTER_HINT = re.compile(r'\bsupported\s+rules:|local custom rules\.|non-local repositories remain|'
                         r'Starlark rule\(\) definitions\.|native analysis does not resolve', re.I)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def blob(raw):
    return dict(base64=base64.b64encode(raw).decode('ascii'), size=len(raw), sha256=sha(raw))


def unblob(value):
    require(set(value) == {'base64', 'size', 'sha256'}, 'raw evidence schema')
    raw = base64.b64decode(value['base64'], validate=True)
    require(type(value['size']) is int and value == blob(raw), 'raw bytes/hash mismatch')
    return raw


def file_record(path):
    require(stat.S_ISREG(path.lstat().st_mode), 'not a regular file: ' + str(path))
    return dict(mode=stat.S_IMODE(path.stat().st_mode), raw=blob(path.read_bytes()))


def archive():
    meta = json.loads((FIXTURES / 'provenance.json').read_bytes())
    require(meta['reference_sha256'] == REFERENCE_SHA and meta['official_commit'] == OFFICIAL_COMMIT
            and meta['reference_version'] == '9.3.0', 'provenance pin')
    expected = dict(url='https://github.com/bazelbuild/platforms/releases/download/1.0.0/platforms-1.0.0.tar.gz',
                    sha256=PLATFORM_SHA, size=7879, encoding='base64')
    require({k: meta['platforms'][k] for k in expected} == expected, 'platform provenance pin')
    raw = base64.b64decode(b''.join((FIXTURES / 'platforms-1.0.0.tar.gz.base64').read_bytes().split()), validate=True)
    require(len(raw) == 7879 and sha(raw) == PLATFORM_SHA, 'genuine platform archive pin')
    return raw


def fixture(case):
    require(case in (*QUERIES, *BUILDS), 'unplanned case')
    files = {str(p.relative_to(FIXTURES / 'workspace')): p.read_bytes()
             for p in sorted((FIXTURES / 'workspace').rglob('*')) if p.is_file()}
    if case in CALLS:
        files['probe.bzl'] = ('def run_probe():\n    print("BEFORE_CALL")\n    result = ' + CALLS[case]
            + '\n    print("UNEXPECTED_AFTER_CALL", result)\n    native.filegroup(name="ok")\n').encode()
    elif case in INVALID:
        files['probe.bzl'] = ('''def run_probe():
    print("BEFORE_INVALID_CONSTRUCTION")
    original = Label("@not_visible//:missing")
    print("INVALID_CONSTRUCTED")
    copy = Label(original)
    if copy != original:
        fail("INVALID_EQUALITY")
    print("INVALID_PASSTHROUGH_OK")
''' + ('''    print("BEFORE_GETTER")
    value = copy.workspace_name
    print("UNEXPECTED_AFTER_GETTER", value)
''' if case == INVALID[1] else '') + '    native.filegroup(name="ok")\n').encode()
    elif case in BUILDS:
        files['producer.bzl'] = (FIXTURES / 'producer.bzl').read_bytes()
        if case == 'custom':
            files['BUILD.bazel'] = b'load(":producer.bzl", "declare_custom")\ndeclare_custom()\n'
        else:
            files['ext.bzl'] = (FIXTURES / 'extension.bzl').read_bytes()
            files['MODULE.bazel'] += b'e=use_extension("//:ext.bzl", "files")\nuse_repo(e, "made")\n'
            files['BUILD.bazel'] = (b'genrule(name="consumer", srcs=["@made//:data.txt"], '
                b'outs=["result.txt"], cmd="cat $(SRCS) > $@")\n'
                b'filegroup(name="result", srcs=["@made//:data.txt", ":consumer"])\n')
    return files


def inputs(case):
    return {p: dict(mode=0o644, raw=blob(raw)) for p, raw in fixture(case).items()}


def plan(reference_only):
    """ONE chronological plan, including auxiliary pin commands. Never filter/sort rows."""
    engines = ('reference',) if reference_only else ('reference', 'native')
    result = [('aux', owner, 'before', command) for owner in ('checkout', 'official') for command in ('head', 'status')]
    result += [('version', e, '', 'version') for e in engines]
    result += [(c, e, 'query', 'query') for c in QUERIES for e in engines]
    result += [(c, e, stage, command) for c in BUILDS for e in engines for stage in ('cold', 'warm')
               for command in ('build', 'cquery' if e == 'reference' else 'query')]
    result += [('aux', owner, 'after', command) for owner in ('checkout', 'official') for command in ('head', 'status')]
    return [list(step) for step in result]


def environment(scratch):
    return dict(PATH='/usr/bin:/bin', HOME=str(scratch / 'home'), TMPDIR=str(scratch / 'tmp'), LC_ALL='C')


def locations(scratch, step):
    case, engine, _, _ = step
    base = scratch / 'cases' / case / engine
    return base / 'workspace', base / 'cache', base / 'output'


def command(config, scratch, step):
    case, engine, stage, verb = step
    if case == 'aux':
        cwd = ROOT if engine == 'checkout' else Path(config['official_source'])
        return ['/usr/bin/git', '--no-optional-locks', '-C', str(cwd), *(['rev-parse', 'HEAD'] if verb == 'head'
            else ['status', '--porcelain=v1', '--untracked-files=no'])], str(cwd)
    workspace, cache, output = locations(scratch, step)
    exe = config[engine]
    argv = [exe, '--ignore_all_rc_files']
    if engine == 'reference':
        argv += ['--batch', '--output_user_root=' + str(scratch / 'user-root'), '--output_base=' + str(output)]
    else:
        argv += ['--cache-dir=' + str(cache)]
    argv += [verb]
    if case == 'version':
        return argv, str(scratch)
    argv += ['--lockfile_mode=off', '--registry=' + (scratch / 'registry').as_uri()]
    if engine == 'reference':
        argv += ['--repository_disable_download', '--incompatible_autoload_externally=', '--noshow_progress',
                 '--color=no', '--curses=no', '--repository_cache=' + str(scratch / 'repository-cache'),
                 '--repo_contents_cache=' + str(scratch / 'repo-contents')]
        if case in BUILDS:
            argv += ['--distdir=' + str(scratch / 'distdir')]
        if verb == 'build':
            argv += ['--subcommands', '--execution_log_json_file=' + str(output.parent / (stage + '.execution.json'))]
        if verb == 'cquery':
            argv += ['--output=files']
    argv += ['//:result' if case in BUILDS else '//:ok']
    return argv, str(workspace)


def source_paths():
    # Excludes unrelated QA and ignored artifacts; includes changing Rust => freeze required.
    return sorted({Path(__file__).resolve(), ROOT / 'tests/test_validate_label_passthrough.py',
        ROOT / 'Cargo.toml', ROOT / 'Cargo.lock', *ROOT.glob('build.rs'),
        *(ROOT / 'src').rglob('*.rs'), *ROOT.glob('tests/*.rs'),
        *(p for directory in (FIXTURES, REGISTRY, ROOT / 'vendor') for p in directory.rglob('*') if p.is_file())})


def bindings(config):
    sources = {str(p.relative_to(ROOT)): dict(sha256=sha(p.read_bytes()), size=p.stat().st_size,
               mode=stat.S_IMODE(p.stat().st_mode)) for p in source_paths()}
    official = {p: sha((Path(config['official_source']) / p).read_bytes()) for p in OFFICIAL}
    require(official == OFFICIAL, 'official source pin')
    binaries = {}
    for engine in (('reference',) if config['reference_only'] else ('reference', 'native')):
        path = Path(config[engine])
        raw = path.read_bytes()
        expected = REFERENCE_SHA if engine == 'reference' else config['native_sha256']
        require(raw[:4] == b'\x7fELF' and sha(raw) == expected and os.access(path, os.X_OK), 'executable pin: ' + engine)
        binaries[engine] = dict(path=str(path), sha256=sha(raw), size=len(raw), mode=stat.S_IMODE(path.stat().st_mode))
    return dict(sources=sources, official=official, binaries=binaries, platform=blob(archive()))


def binding_sha(value):
    return sha(json.dumps(value, sort_keys=True, separators=(',', ':')).encode())


def private_inputs(scratch):
    return dict(registry={str(p.relative_to(scratch / 'registry')): sha(p.read_bytes())
        for p in sorted((scratch / 'registry').rglob('*')) if p.is_file()},
        distfile=blob((scratch / 'distdir/platforms-1.0.0.tar.gz').read_bytes()))


def expected_private(binding):
    prefix = 'tests/fixtures/builtin_modules/registry/'
    return dict(registry={p.removeprefix(prefix): v['sha256'] for p, v in binding['sources'].items()
                         if p.startswith(prefix)}, distfile=binding['platform'])


def markers(text):
    # Only whole bare marker lines, optionally the genuine DEBUG location prefix.
    return re.findall(r'(?m)^(?:DEBUG: [^\n]+\.bzl:\d+:\d+: )?'
        r'((?:BEFORE_CALL|UNEXPECTED_AFTER_CALL|BEFORE_INVALID_CONSTRUCTION|INVALID_CONSTRUCTED|'
        r'INVALID_PASSTHROUGH_OK|BEFORE_GETTER|UNEXPECTED_AFTER_GETTER|POSITIVE_COMPLETE|'
        r'FROZEN_EXPORT_TWO_READERS_OK|COPY_OK|LP_[A-Z_]+)(?: [^\n]*)?)$', text)


def native_negative_body(text):
    """Classify a view only: keep the raw stderr and its byte/hash receipt intact."""
    if not FOOTER_HINT.search(text):
        return text
    # No strip()/splitlines()/whitespace normalization: the exact footer must
    # occupy one whole terminal line, with the LF emitted by main's eprintln!.
    footer = next((f for f in NATIVE_LOADING_FOOTERS if text.endswith('\n' + f)), None)
    require(footer is not None, 'modified or nonterminal native loading footer')
    body = text[:-len(footer)]
    require(not FOOTER_HINT.search(body), 'duplicate or embedded native loading footer')
    return body


def clean_row(row, *, case=None, engine=None):
    text = unblob(row['stderr']).decode('utf-8')
    require(type(row['returncode']) is int and not row['timeout'] and not row['spawn_error']
            and row['group_gone'] is True and not row['cleanup_signals'], 'process failure/cleanup')
    if engine == 'native' and case in (*CALLS, *INVALID) and row['returncode'] == 1:
        text = native_negative_body(text)
    require(not FORBIDDEN.search(text + unblob(row['stdout']).decode('utf-8')), 'fallback/infrastructure/crash/global failure')
    return text


def success(row, text):
    require(row['returncode'] == 0 and not re.search(r'(?m)^(?:ERROR:|Error(?: in |:)|error:)', text), 'unexpected successful-command diagnostic')


def bazel_body(case):
    """Bazel 9.3 Label() binder text (BuiltinFunction/ParamDescriptor, pinned in OFFICIAL)."""
    if case.startswith('wrong-type-'):
        typename = BAD_TYPES[case.removeprefix('wrong-type-')][1]
        return "in call to Label(), parameter 'input' got value of type '" + typename + "', want 'string or Label'"
    if case == 'missing':
        return 'Label() missing 1 required positional argument: input'
    if case == 'extra':
        return 'Label() accepts no more than 1 positional argument but got 2'
    if case in ('keyword-native-name', 'keyword-unknown'):
        return "Label() got unexpected keyword argument '" + ('unknown' if case == 'keyword-unknown' else 'label_string') + "'"
    require(case in ('keyword-string', 'keyword-label', 'positional-plus-keyword'), 'unplanned binding case')
    return "Label() got named argument for positional-only parameter 'input'"


def diagnostic(case, engine, text):
    body = bazel_body(case)
    if engine == 'reference':
        require('Error in Label: ' + body in text.splitlines(), 'wrong reference type/binding category')
    else:
        # Since N33 the native binder reports Bazel's own Label() text (formerly
        # starlark-rust "Type of parameter `input` doesn't match ...", "Wrong
        # number of positional arguments ..." and "Found `x` extra named
        # parameter(s) ..."). Fail closed: exactly one native primary diagnostic,
        # equal to the Bazel body, never an arbitrary nonzero.
        errors = re.findall(r'(?m)^error: (.+)$', text)
        require(len(errors) == 1, 'one native primary diagnostic required')
        if case.startswith('wrong-type-'):
            require(errors[0] == body, 'wrong native type/parameter category')
        elif case in ('extra', 'missing'):
            require(errors[0] == body, 'wrong native arity' if case == 'extra' else 'wrong native missing parameter')
        else:
            require(errors[0] == body, 'wrong native positional-only binding')
        # Bind the error to the one real Label call frame AND source span, not
        # a disconnected source echo.
        source = 'result = ' + CALLS[case]
        frames = re.findall(r'(?m)^  \* //:probe\.bzl:3, in run_probe\n      ([^\n]+)\n', text)
        spans = re.findall(r'(?m)^ --> //:probe\.bzl:3:14\n  \|\n3 \|     ([^\n]+)\n', text)
        require(frames == spans == [source], 'wrong native binder operation/callsite')


def query_result(row, case, engine):
    text = clean_row(row, case=case, engine=engine)
    out = unblob(row['stdout'])
    seen = markers(text)
    require(not ACTION.search(text) and not any(s.startswith('LP_') for s in seen), 'query executed action/callback')
    require(row['outputs'] == {} and row['publication'] == {} and row['action_files'] == [], 'query published action output')
    require(row.get('action_files_before', []) == [] and row['execution_log'] is None, 'query action provenance')
    if case == 'positive':
        success(row, text)
        expected = ['COPY_OK ' + n + ' @@' + repo + '//:same ' + repo + ' dict=1 string_hash_equal=True' for n, repo in COPY_ROWS]
        expected += ['FROZEN_EXPORT_TWO_READERS_OK @@dep+//:same external-frozen-key', 'POSITIVE_COMPLETE']
        require(row['returncode'] == 0 and out == b'//:ok\n' and seen == expected, 'positive logical equality/lexical/frozen contract')
        return 'PARITY_VALUE_BEHAVIOR'
    if case in CALLS:
        require(row['returncode'] == (7 if engine == 'reference' else 1) and not out
                and seen == ['BEFORE_CALL'], 'not a bound loading rejection')
        source = 'result = ' + CALLS[case]
        require(source in text and re.search(r'probe\.bzl(?:", line |:)3\b', text), 'wrong target operation/callsite')
        diagnostic(case, engine, text)
        return 'PARITY_REJECTION_CATEGORY'
    require(case in INVALID, 'unplanned query')
    # Integration-15: native constructs the unmapped Label (deferred invalid
    # Label), copies and compares it like Bazel; only the getter rejects.
    expected = ['BEFORE_INVALID_CONSTRUCTION', 'INVALID_CONSTRUCTED', 'INVALID_PASSTHROUGH_OK']
    if case == INVALID[0]:
        success(row, text)
        require(row['returncode'] == 0 and out == b'//:ok\n' and seen == expected, 'invalid Label passthrough')
        return 'PARITY_INVALID_PASSTHROUGH'
    expected += ['BEFORE_GETTER']
    body = "'workspace_name' is not allowed on invalid Label @@[unknown repo 'not_visible' requested from @@]//:missing"
    require(row['returncode'] == (7 if engine == 'reference' else 1) and not out and seen == expected and
        ('Error: ' if engine == 'reference' else 'error: ') + body in text.splitlines()
        and 'value = copy.workspace_name' in text and re.search(r'probe\.bzl(?:", line |:)10\b', text), 'invalid Label getter phase/visibility')
    if engine == 'native':
        require(re.findall(r'(?m)^error: (.+)$', text) == [body], 'one native primary diagnostic required')
    return 'PARITY_INVALID_GETTER'


def output_paths(row, scratch, step):
    workspace, _, output = locations(scratch, step)
    text = unblob(row['stdout']).decode()
    paths = []
    for line in text.splitlines():
        if step[1] == 'native':
            parts = line.split('\t')
            require(len(parts) == 2 and parts[0] == '//:result', 'native output framing')
            path = Path(parts[1])
            require(path.is_absolute(), 'native output not absolute')
        else:
            relative = Path(line)
            require(line and not relative.is_absolute() and '..' not in relative.parts, 'unsafe reference output')
            path = (output / 'execroot/_main' if relative.parts[0] in ('bazel-out', 'external') else workspace) / relative
        require('..' not in path.parts and path.is_relative_to(scratch) and path not in paths, 'unsafe/duplicate output')
        paths.append(path)
    require({p.name for p in paths} == {'data.txt', 'result.txt'} and len(paths) == 2, 'output inventory')
    return paths


def publication(cache, data_path):
    trees = cache / 'repositories/direct/trees'
    root = next((p for p in data_path.parents if p.parent == trees), None)
    require(root is not None and re.fullmatch('[a-f0-9]{64}', root.name), 'actual publication identity')
    return dict(identity=root.name, roots=sorted(p.name for p in trees.iterdir()),
        manifests={p.name: file_record(p) for p in sorted((trees.parent / 'manifests').iterdir())},
        inventory={str(p.relative_to(root)): file_record(p) for p in sorted(root.rglob('*'))})


def check_publication(value):
    require(set(value) == {'identity', 'roots', 'manifests', 'inventory'}, 'publication schema')
    identity = value['identity']
    require(re.fullmatch('[a-f0-9]{64}', identity) and value['roots'] == [identity]
            and set(value['manifests']) == {identity + '.json'}, 'extra/staging/missing publication')
    expected = {p: dict(mode=0o644, raw=blob(raw)) for p, raw in REPOSITORY_FILES.items()}
    require(value['inventory'] == expected, 'repository source inventory/0644/bytes')
    manifest = json.loads(unblob(value['manifests'][identity + '.json']['raw']))
    # Integration-18 (N61): native manifests append the pre-execution
    # specification digest ('spec', 64 hex, the specs/ pointer name) and the
    # recorded world ('recorded', the bytes of a JSON document) after the four
    # legacy fields; nothing else may join them.
    require(list(manifest)[:4] == ['format', 'profile', 'identity', 'files']
            and list(manifest)[4:] in ([], ['spec', 'recorded'])
            and (len(manifest) == 4 or (re.fullmatch('[0-9a-f]{64}', str(manifest['spec']))
                 and isinstance(manifest['recorded'], list)
                 and all(type(b) is int and 0 <= b < 256 for b in manifest['recorded'])
                 and isinstance(json.loads(bytes(manifest['recorded'])), dict))), 'publication manifest N61 fields')
    manifest = {k: manifest[k] for k in ('format', 'profile', 'identity', 'files')}
    # repository_store::snapshot visits (root, root) first, recording the empty
    # relative path as a real directory entry. Do not discard it or accept an
    # empty-file digest in its place. These are this fixture's observed modes.
    require(manifest == dict(format=1, profile='direct-file-template-v1', identity=identity,
        files={'': dict(directory=True, mode=0o755, size=0, sha256=''),
               **{p: dict(directory=False, mode=0o644, size=len(raw), sha256=sha(raw))
                  for p, raw in REPOSITORY_FILES.items()}})
        and type(manifest['format']) is int
        and all(type(entry[key]) is kind for entry in manifest['files'].values()
                for key, kind in (('directory', bool), ('mode', int), ('size', int), ('sha256', str))),
        'publication manifest content')


def callback_sequences(step):
    case, engine, stage, verb = step
    analysis = ('LP_ANALYSIS_CALLBACK',)
    extension = ('LP_EXTENSION_CALLBACK',)
    full = analysis if case == 'custom' else extension + ('LP_REPOSITORY_CALLBACK',)
    if engine == 'native':
        if verb == 'build':
            # Fresh process: exact sequence even with reused actions. Integration-18
            # (N61): a warm build reuses the unchanged repository publication through
            # its specification pointer, so the repository callback runs only cold.
            if case == 'extension' and stage == 'warm':
                return (extension,)
            return (full,)
        # A loading-only query may demand the extension mapping, never analysis
        # or repository materialization. This is not a cache-hit inference.
        return ((),) if case == 'custom' else ((), extension)
    if stage == 'cold' and verb == 'build':
        return (full,)
    # Pinned 33-command receipt 2d2ff384...: rows 22-24 print analysis once;
    # rows 26-28 print extension once, with repository output suppressed.
    # Permit total debug suppression on these later reference steps, but never
    # an arbitrary subset, replayed repository-only line, duplicate or reversal.
    return ((), analysis if case == 'custom' else extension)


def reference_artifacts(row, scratch, step):
    """Trusted fixture layout + independently captured cquery output, never log paths."""
    case, _, stage, _ = step
    query_step = [case, 'reference', stage, 'cquery']
    require(row['step'] == query_step, 'execution log paired with wrong artifact query')
    build_result(row, scratch, query_step)  # Actual output framing, bytes, modes and phase.
    _, _, output = locations(scratch, query_step)
    execroot = output / 'execroot/_main'
    consumer = next(Path(p) for p in row['outputs'] if Path(p).name == 'result.txt')
    require(consumer.is_relative_to(execroot), 'consumer outside planned execroot')
    relative = consumer.relative_to(execroot)
    require(len(relative.parts) == 4 and relative.parts[0] == 'bazel-out'
            and re.fullmatch(r'[A-Za-z0-9_+.-]+', relative.parts[1])
            and relative.parts[1] not in ('.', '..') and relative.parts[2:] == ('bin', 'result.txt'),
            'consumer outside fixture configuration/bin root')
    # The custom producer and consumer share configuration and root package.
    # The extension source is the literal files/made repository in the fixture.
    producer = relative.with_name('data.txt') if case == 'custom' else Path('external/+files+made/data.txt')
    require(set(row['outputs']) == {str(execroot / producer), str(consumer)}, 'producer outside fixture artifact root')
    return producer.as_posix(), relative.as_posix()


def bind_reference_spawn(spawn, artifact_row, scratch, step):
    producer, consumer = reference_artifacts(artifact_row, scratch, step)
    setup = 'external/bazel_tools/tools/genrule/genrule-setup.sh'
    require(sorted(p['path'] for p in spawn['inputs']) == sorted([producer, setup])
            and [p['path'] for p in spawn['actualOutputs']] == [consumer]
            and spawn.get('listedOutputs') == [consumer] and spawn.get('targetLabel') == '//:consumer',
            'consumer execution paths detached from captured artifacts')
    require(spawn.get('commandArgs') == ['/bin/bash', '-c', 'source ' + setup + '; cat ' + producer + ' > ' + consumer],
            'literal producer-to-consumer command differs')
    digest = dict(hash=sha(DATA), sizeBytes=str(len(DATA)), hashFunctionName='SHA-256')
    actual_input = next(p for p in spawn['inputs'] if p['path'] == producer)
    require(actual_input.get('digest') == digest and spawn['actualOutputs'][0].get('digest') == digest,
            'consumer execution digest detached from captured bytes')


def build_result(row, scratch, step, artifact_row=None):
    case, engine, stage, verb = step
    text = clean_row(row)
    success(row, text)
    require(tuple(markers(text)) in callback_sequences(step), 'wrong producer callback sequence/count/phase')
    if stage == 'cold' and verb == 'build':
        require(row.get('action_files_before', []) == [], 'cold cache already populated')
    captures = verb == ('build' if engine == 'native' else 'cquery')
    if captures:
        paths = output_paths(row, scratch, step)
        require(set(row['outputs']) == {str(p) for p in paths}, 'output detached from command')
        for p in paths:
            item = row['outputs'][str(p)]
            mode = 0o644 if case == 'extension' and p.name == 'data.txt' else 0o555
            require(item == dict(mode=mode, raw=blob(DATA)), 'output bytes/hash/exact sealed mode')
        if engine == 'native' and case == 'extension':
            check_publication(row['publication'])
            _, cache, _ = locations(scratch, step)
            require(str(cache / 'repositories/direct/trees' / row['publication']['identity'] / 'data.txt')
                    in row['outputs'], 'publication identity detached from actual producer')
        else:
            require(not row['publication'], 'unplanned native publication claim')
    else:
        require(not row['outputs'] and not row['publication'], 'unplanned output observation')
    if engine == 'native' and verb == 'build':
        actions = 2 if case == 'custom' else 1
        counts = re.findall(r'(\d+) actions executed, (\d+) cache hits', text)
        require(counts == [(str(actions if stage == 'cold' else 0), str(0 if stage == 'cold' else actions))], 'native cold/warm consumer action cache')
    if engine == 'native' and verb == 'query':
        require(unblob(row['stdout']) == b'//:result\n' and not ACTION.search(text.replace('LP_EXTENSION_CALLBACK', '').replace('LP_REPOSITORY_CALLBACK', '')), 'post-build query executed action')
    if engine == 'reference' and verb == 'build':
        require(row['execution_log'] is not None, 'missing actual execution log')
        raw = unblob(row['execution_log']).decode()
        decoder, spawns = json.JSONDecoder(), []
        while raw.strip():
            obj, end = decoder.raw_decode(raw.lstrip())
            spawns.append(obj)
            raw = raw.lstrip()[end:]
        require(len(spawns) == (1 if stage == 'cold' else 0), 'reference cold/warm consumer execution')
        for spawn in spawns:
            require(spawn['exitCode'] == 0 and not spawn.get('cacheHit') and spawn['mnemonic'] == 'Genrule',
                    'consumer action did not complete')
            if artifact_row is not None:
                bind_reference_spawn(spawn, artifact_row, scratch, step)
        # The plan deliberately builds BEFORE querying. A live build cannot be
        # PASS until the next cquery independently captures its real artifacts.
        if artifact_row is None:
            return 'REFERENCE_BUILD_PENDING_OUTPUT_BINDING'
        reference_artifacts(artifact_row, scratch, step)
    else:
        require(row['execution_log'] is None, 'unplanned execution log')
    return 'PARITY_PRODUCER_CONSUMER'


def complete_reference_pair(rows, scratch):
    """Live promotion after the planned adjacent cquery; never a separate filtered order."""
    query = rows[-1]
    case, engine, stage, verb = query['step']
    if case in BUILDS and engine == 'reference' and verb == 'cquery':
        build = rows[-2]
        require(build['step'] == [case, engine, stage, 'build'] and build['id'] + 1 == query['id']
                and build['result'] == 'REFERENCE_BUILD_PENDING_OUTPUT_BINDING', 'detached live build/query pair')
        build['result'] = build_result(build, scratch, build['step'], artifact_row=query)


def action_files(scratch, step):
    _, cache, output = locations(scratch, step)
    roots = (cache / 'actions', cache / 'artifacts', cache / 'repositories/direct/trees',
             output / 'execroot/_main/bazel-out')
    return sorted(str(p) for root in roots for p in root.rglob('*') if p.is_file())


def group_exists(pid):
    try:
        os.killpg(pid, 0)
        return True
    except ProcessLookupError:
        return False


def run(config, scratch, step, rid, binding, records):
    require(bindings(config) == binding, 'pre-command source/executable changed')
    require(private_inputs(scratch) == expected_private(binding), 'private registry/archive changed')
    argv, cwd = command(config, scratch, step)
    row = dict(id=rid, step=step, argv=argv, cwd=cwd, env=environment(scratch), timeout_seconds=config['timeout'],
        binding_sha256=binding_sha(binding), inputs_before={}, inputs_after={}, outputs={}, publication={}, action_files=[], action_files_before=[],
        execution_log=None, returncode=None, timeout=False, spawn_error=None, cleanup_signals=[], group_gone=False,
        private_before=private_inputs(scratch), private_after=None, pid=None,
        stdout=blob(b''), stderr=blob(b''))
    records.append(row)  # Even a failed spawn/capture retains the chronological raw receipt.
    if step[0] in (*QUERIES, *BUILDS):
        workspace, cache, output = locations(scratch, step)
        row['inputs_before'] = {p: file_record(workspace / p) for p in fixture(step[0])}
        row['action_files_before'] = action_files(scratch, step)
    start = time.monotonic()
    process = None
    try:
        process = subprocess.Popen(argv, cwd=cwd, env=row['env'], stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        row['pid'] = process.pid
        try:
            stdout, stderr = process.communicate(timeout=config['timeout'])
        except subprocess.TimeoutExpired:
            row['timeout'] = True
            os.killpg(process.pid, signal.SIGKILL)
            row['cleanup_signals'].append('SIGKILL')
            stdout, stderr = process.communicate()
        row.update(returncode=process.returncode, stdout=blob(stdout), stderr=blob(stderr))
    except OSError as error:
        row['spawn_error'] = str(error)
        raise
    finally:
        if process is not None:
            if group_exists(process.pid):
                os.killpg(process.pid, signal.SIGKILL)
                row['cleanup_signals'].append('SIGKILL')
            if process.poll() is None:
                stdout, stderr = process.communicate()
                row.update(returncode=process.returncode, stdout=blob(stdout), stderr=blob(stderr))
            row['group_gone'] = not group_exists(process.pid)
        row['elapsed_seconds'] = time.monotonic() - start
    if step[0] in (*QUERIES, *BUILDS):
        row['inputs_after'] = {p: file_record(workspace / p) for p in fixture(step[0])}
        row['action_files'] = action_files(scratch, step)
    if step[0] in BUILDS and row['returncode'] == 0:
        if step[3] == ('build' if step[1] == 'native' else 'cquery'):
            for p in output_paths(row, scratch, step):
                require(p.resolve(strict=True).is_relative_to(scratch), 'resolved output escaped scratch')
                row['outputs'][str(p)] = file_record(p.resolve(strict=True))
                if step[:2] == ['extension', 'native'] and p.name == 'data.txt':
                    row['publication'] = publication(cache, p.resolve(strict=True))
        if step[1] == 'reference' and step[3] == 'build':
            row['execution_log'] = blob((output.parent / (step[2] + '.execution.json')).read_bytes())
    require(bindings(config) == binding, 'post-command source/executable changed')
    row['private_after'] = private_inputs(scratch)
    require(row['private_after'] == row['private_before'], 'private registry/archive changed')
    return row


def check_order(report):
    expected = plan(report['config']['reference_only'])
    require(report['plan'] == expected and len(report['runs']) == len(expected), 'incomplete chronological run plan')
    require(report['observations'] == list(range(len(expected))), 'detached observation references')
    for index, (row, step) in enumerate(zip(report['runs'], expected)):
        require(type(row['id']) is int and row['id'] == index and row['step'] == step, 'chronological run order')
        argv, cwd = command(report['config'], Path(report['scratch']), step)
        require(row['argv'] == argv and row['cwd'] == cwd, 'chronological command binding')


def auxiliary_result(row, step, previous):
    case, engine, stage, verb = step
    clean_row(row)
    require(row['returncode'] == 0, 'auxiliary pin/version command')
    if case == 'version':
        out = unblob(row['stdout']).decode()
        require((re.search(r'(?m)^Build label: 9\.3\.0$', out) if engine == 'reference'
                 else re.search(r'(?m)^bazel-rs [\w.+-]+ \(experimental; compatibility target Bazel 9\.3\.0\)$', out)), 'version identity')
        return 'VERSION'
    require(unblob(row['stderr']) == b'', 'auxiliary pin/status stderr')
    key = (engine, verb)
    if engine == 'official':
        require(unblob(row['stdout']) == ((OFFICIAL_COMMIT + '\n').encode() if verb == 'head' else b''), 'official commit/status')
    elif verb == 'head':
        require(re.fullmatch(rb'[0-9a-f]{40}\n', unblob(row['stdout'])), 'checkout commit')
    if stage == 'before':
        previous[key] = row['stdout']
    else:
        require(previous[key] == row['stdout'], 'checkout/source status changed')
    return 'PIN'


def replay(report, config, current_binding):
    require(set(report) == set('schema config scratch status cleanup binding_before binding_after plan runs observations'.split()), 'report schema')
    require(report['schema'] == 1 and report['config'] == config, 'replay configuration binding')
    require(report['binding_before'] == report['binding_after'] == current_binding, 'current source/executable binding')
    require(report['cleanup'] is True and not Path(report['scratch']).exists(), 'scratch cleanup')
    check_order(report)
    scratch, previous = Path(report['scratch']), {}
    for index, (row, step) in enumerate(zip(report['runs'], plan(config['reference_only']))):
        require(set(row) == set('id step argv cwd env timeout_seconds binding_sha256 inputs_before inputs_after outputs publication action_files action_files_before execution_log returncode timeout spawn_error cleanup_signals group_gone pid elapsed_seconds stdout stderr result private_before private_after'.split()), 'run schema')
        require(row['env'] == environment(scratch) and row['timeout_seconds'] == config['timeout']
                and row['binding_sha256'] == binding_sha(current_binding), 'environment/deadline/source binding')
        require(row['private_before'] == row['private_after'] == expected_private(current_binding), 'private registry/archive binding')
        require(type(row['pid']) is int and row['pid'] > 1 and 0 <= row['elapsed_seconds'] <= config['timeout'] + 10, 'process receipt')
        case, engine, stage, verb = step
        if case in ('aux', 'version'):
            require(not any(row[k] for k in ('inputs_before', 'inputs_after', 'outputs', 'publication', 'action_files', 'action_files_before'))
                    and row['execution_log'] is None, 'auxiliary command has workload observations')
        if case in ('aux', 'version'):
            result = auxiliary_result(row, step, previous)
        else:
            require(row['inputs_before'] == row['inputs_after'] == inputs(case), 'fixture/source/callsite snapshot')
            artifact_row = report['runs'][index + 1] if case in BUILDS and engine == 'reference' and verb == 'build' else None
            result = query_result(row, case, engine) if case in QUERIES else build_result(row, scratch, step, artifact_row)
            if case in BUILDS and row['outputs']:
                key = (case, engine)
                value = (row['outputs'], row['publication'])
                if stage == 'cold':
                    previous[key] = value
                else:
                    require(previous[key] == value, 'warm identity/manifest/inventory/output changed')
        require(row['result'] == result, 'observation result/scope mismatch')
    require(report['status'] == ('REFERENCE_PREPARATION_ONLY' if config['reference_only'] else 'BOUNDED_PAIRED_PASS'), 'false acceptance status')
    return report['status']


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--reference', type=Path, required=True)
    p.add_argument('--official-source', type=Path, required=True)
    p.add_argument('--native', type=Path)
    p.add_argument('--native-sha256')
    p.add_argument('--reference-only', action='store_true')
    p.add_argument('--candidate-frozen', action='store_true', help='parent explicitly approved source/candidate freeze')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--replay', action='store_true')
    p.add_argument('--timeout', type=int, default=180)
    return p


def configuration(args):
    require(args.candidate_frozen, 'parent candidate freeze required even for reference-only')
    require(1 <= args.timeout <= 600, 'bounded timeout')
    require(args.reference_only or (args.native is not None and
            re.fullmatch('[a-f0-9]{64}', args.native_sha256 or '')), 'full mode requires explicit native path/sha256')
    require(not args.reference_only or (args.native is None and args.native_sha256 is None), 'reference-only cannot bind/execute native')
    return dict(reference=str(args.reference.resolve()), official_source=str(args.official_source.resolve()),
        native=str(args.native.resolve()) if args.native else None, native_sha256=args.native_sha256,
        reference_only=args.reference_only, timeout=args.timeout)


def main(argv=None):
    args = parser().parse_args(argv)
    config = configuration(args)
    output = args.output.resolve()
    require(not any(output.is_relative_to(ROOT / p) for p in ('src', 'scripts', 'tests', 'vendor'))
            and output not in source_paths() and str(output) not in (config['reference'], config['native'])
            and not output.is_relative_to(Path(config['official_source'])), 'unsafe report destination')
    binding = bindings(config)
    if args.replay:
        print(replay(json.loads(output.read_bytes()), config, binding))
        return 0
    require(not output.exists(), 'refuse to overwrite retained report')
    scratch = Path(tempfile.mkdtemp(prefix='label-passthrough-', dir='/tmp/opencode'))
    report = dict(schema=1, config=config, scratch=str(scratch), status='FAIL', cleanup=False,
        binding_before=binding, binding_after=None, plan=plan(config['reference_only']), runs=[], observations=[])
    try:
        for name in ('home', 'tmp', 'distdir'):
            (scratch / name).mkdir()
        (scratch / 'distdir/platforms-1.0.0.tar.gz').write_bytes(archive())
        shutil.copytree(REGISTRY, scratch / 'registry')
        prepared, pins = set(), {}
        for index, step in enumerate(report['plan']):
            case, engine, _, _ = step
            if case in (*QUERIES, *BUILDS) and (case, engine) not in prepared:
                workspace, _, _ = locations(scratch, step)
                for name, raw in fixture(case).items():
                    p = workspace / name
                    p.parent.mkdir(parents=True, exist_ok=True)
                    p.write_bytes(raw)
                    p.chmod(0o644)
                prepared.add((case, engine))
            row = run(config, scratch, step, index, binding, report['runs'])
            report['observations'].append(index)
            if case in QUERIES:
                row['result'] = query_result(row, case, engine)
            elif case in BUILDS:
                row['result'] = build_result(row, scratch, step)
                complete_reference_pair(report['runs'], scratch)
            else:
                row['result'] = auxiliary_result(row, step, pins)
        report['binding_after'] = bindings(config)
        report['status'] = 'REFERENCE_PREPARATION_ONLY' if config['reference_only'] else 'BOUNDED_PAIRED_PASS'
    finally:
        shutil.rmtree(scratch)
        report['cleanup'] = not scratch.exists()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2) + '\n')
    try:
        replay(report, config, binding)
    except Exception:
        report['status'] = 'FAIL'
        output.write_text(json.dumps(report, indent=2) + '\n')
        raise
    print(report['status'] + ': ' + str(output))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
