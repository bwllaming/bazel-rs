#!/usr/bin/env python3
"""Controlled repository environment matrix; not full Bazel/SDK parity.

Never inspect the parent environment, including its key names. Reference-only
preparation is not native acceptance. Native callbacks execute on every fresh
CLI demand; genuine persistent-server callbacks have separate expectations.
"""

import base64
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import validate_string_list_dict as common
import validate_repository_rules as direct
from validate import ROOT, COMMON_FLAGS, BUILD_FLAGS, reference_output_paths, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256


A, B, UNUSED = 'BAZEL_RS_ENV_A', 'BAZEL_RS_ENV_B', 'BAZEL_RS_ENV_UNUSED'
KEYS = frozenset((A, B, UNUSED, '?'))
SECRET = 'SYNTHETIC_SECRET_42'
# The odd-names fixture declares exactly these (plus '?'): recorded watch-probe
# names are compared against the union of every fixture's declared names.
ODD_NAMES = frozenset(('', '=', 'A=B', A+'\0TAIL'))
WATCHED_NAMES = KEYS | ODD_NAMES
# Environment VALUES never persist in native publication metadata — a native
# policy (Bazel's own .marker file stores `ENV:` values in plain text).
# Since N61 a publication manifest additionally carries the pre-execution
# specification digest (`spec`, one hex digest) and the recorded watch probes
# (`recorded`: input digests plus environment NAMES).
VALUES = ('alpha', 'bee', 'unused', 'question', SECRET)
MANIFEST_CORE = frozenset(('format', 'profile', 'identity', 'files'))
MANIFEST_RECORDED = frozenset(('spec', 'recorded'))


def _strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, item in value.items():
            yield key
            yield from _strings(item)
    elif isinstance(value, (list, tuple)):
        for item in value:
            yield from _strings(item)


def check_publication_schema(data, message):
    """Legacy four-field manifests stay valid; only N61's digest/probe fields
    may join them (nothing an environment value could hide in)."""
    if MANIFEST_CORE - set(data) or set(data) - MANIFEST_CORE - MANIFEST_RECORDED:
        raise ValueError(message)
    if 'spec' in data and not re.fullmatch('[0-9a-f]{64}', str(data['spec'])):
        raise ValueError(message)
    recorded = data.get('recorded')
    if recorded is not None and (not isinstance(recorded, list)
            or not all(isinstance(b, int) and 0 <= b < 256 for b in recorded)):
        raise ValueError(message)


def check_publication_privacy(data, message):
    """Native policy: environment names may persist only as recorded watch
    probes and values must stay absent everywhere (Bazel's .marker stores
    ENV: values in plain text; digests here are opaque hex)."""
    probes = None
    if data.get('recorded') is not None:
        try:
            probes = json.loads(bytes(data['recorded']))
        except ValueError:
            raise ValueError(message)
    watched = probes.get('watched_env') if isinstance(probes, dict) else None
    watched = watched if isinstance(watched, dict) else {}
    if not set(watched.get('names', ())) <= WATCHED_NAMES:
        raise ValueError(message)
    redacted = {k: v for k, v in data.items() if k != 'recorded'}
    if isinstance(probes, dict):
        redacted['recorded'] = {k: (dict(v, names=[]) if k == 'watched_env' and isinstance(v, dict) else v)
                                for k, v in probes.items()}
    for text in _strings(redacted):
        if re.fullmatch('[0-9a-f]{64}', text):
            continue
        if any(value in text for value in VALUES) or any(name in text for name in (A, B, UNUSED)):
            raise ValueError(message)
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
SOURCES = list(dict.fromkeys([ROOT / 'scripts/validate_repository_environment.py',
    ROOT / 'tests/test_validate_repository_environment.py', *common.SOURCES,
    *sorted(p for p in REGISTRY.rglob('*') if p.is_file()),
    *sorted((ROOT / 'src').rglob('*.rs')), ROOT / 'Cargo.toml', ROOT / 'Cargo.lock']))
OFFICIAL_COMMIT = 'e89755babf3e941bc1e9d8120b1cd48854ab9dfa'
# Pinned upstream source, not a dependency on an ignored research report.
OFFICIAL = {
    'src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkRepositoryModule.java':
        'c6adf0f521e56419ec22e7980def6b27778bab4d5c5294b3556c2286f5b6bcea',
    'src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java':
        'dcdc9a92b48523146b2870b87d06c9c9d40efcf15d10f86853c9fe128204a231',
    'src/main/java/com/google/devtools/build/lib/bazel/repository/DigestWriter.java':
        'a13bc4c01bccff496d6349340d9b082f8110a71428aec5edba2d884ac97a00c2',
}
ROUTES = ('direct', 'ordinary')
CASEPLAN = [
    common.case('watched', tuple(r+'.'+s for r in ROUTES for s in ('list', 'tuple', 'snapshot'))),
    common.case('identity', tuple(r+'.'+s for r in ROUTES for s in ('declared', 'dynamic'))),
    common.case('defaults', ROUTES),
    common.case('odd', tuple(r+'.'+s for r in ROUTES for s in ('declared', 'dynamic'))),
    common.case('isolation', ROUTES),
    common.case('binding', ('name-keyword', 'default-keyword', 'name-type', 'default-present',
                          'default-absent', 'missing', 'extra', 'environ-none', 'environ-element', 'frozen')),
    common.case('privacy', ROUTES),
    # module_ctx.getenv (N21; genuine Bazel records it as an extension input):
    # the only dependency path is extension getenv -> repository attribute.
    common.case('module-getenv', ('ordinary',)),
    # --repo_env (N40 implements it like Bazel; before N40 these were the native
    # repo-env-inline/repo-env-split rejection probes): RepositoryOptions.repoEnvironment
    # overlays the client environment for repository rules, so the flag value wins,
    # a changed flag value is a watched input, a client change under the override is
    # not, and dropping the flag falls back to the client value. Both argv spellings.
    common.case('repo-env', ('direct.inline', 'ordinary.split')),
    common.case('native-boundary', ('nonascii', 'nonutf8', 'name-limit', 'getter-limit'), 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASEPLAN]
digest, evidence, snapshot, valid_evidence = direct.digest, direct.evidence, direct.snapshot, direct.valid_evidence
SAFE_FLAGS = [f for f in COMMON_FLAGS if not f.startswith('--repo_env')]
DIAGNOSTICS = {
    'keyword': r'getenv[^\n]*(?:positional|named|keyword)|(?:unexpected|not allowed)[^\n]*(?:name|default)',
    'name-type': r'(?:name|parameter[^\n]*0)[^\n]*(?:string|str)|getenv[^\n]*(?:string|str)',
    'default-type': r'default[^\n]*(?:string|str|None)|getenv[^\n]*default',
    'arity': r'getenv[^\n]*(?:argument|parameter)|(?:missing|extra|too many)[^\n]*(?:argument|parameter)',
    'environ-shape': r'environ[^\n]*(?:list|tuple|sequence)|(?:list|tuple|sequence)[^\n]*environ',
    'environ-element': r'environ[^\n]*(?:string|str)|repository_rule[^\n]*(?:element|index)[^\n]*(?:string|str)',
    'frozen': r'(?:frozen|immutable)[^\n]*list|list[^\n]*(?:frozen|immutable)',
    'encoding': r'(?:environment|repository)[^\n]*(?:ASCII|UTF-8|encoding)',
    'limit': r'(?:environment|getenv)[^\n]*(?:limit|budget|too (?:long|many)|exceed)',
}
# Native errors are deliberately value-free constants, not Bazel binder wording.
# Keep logical argument/type/encoding/quota classes distinct: an observation
# failure must never satisfy a bad-default rejection, even when A is present.
NATIVE_ERRORS = {
    'keyword': 'repository getenv requires one or two positional arguments and no keyword arguments',
    'arity': 'repository getenv requires one or two positional arguments and no keyword arguments',
    'name-type': 'repository getenv name must be a string',
    'default-type': 'repository getenv default must be a string or None',
    'environ-shape': 'repository_rule environ must be a list or tuple of strings',
    'environ-element': 'repository_rule environ must be a list or tuple of strings',
    'encoding': 'repository environment values must be ASCII',
    'name-limit': 'repository environment name exceeds 4096 bytes',
    'getter-limit': 'repository environment exceeds 10000 observations per callback',
}
BAD = re.compile(r'panicked|internal error|FATAL:|network unavailable|permission denied|failed to download|'
                 r'parse error|unexpected token|not defined|Variable .+ not found|unknown global', re.I)


def controlled_environment(scratch, values):
    if set(values) - KEYS:
        raise ValueError('unplanned fixture environment key')
    env = {'PATH': '/usr/bin:/bin', 'HOME': str(scratch / 'home'),
           'TMPDIR': str(scratch / 'tmp'), 'LC_ALL': 'C.UTF-8'}
    for key, value in values.items():
        if not isinstance(value, (str, bytes)):
            raise ValueError('fixture value must be text or bytes')
        env[key] = value
    return env


def encoded_environment(env):
    return {k: base64.b64encode(v if isinstance(v, bytes) else v.encode()).decode() for k, v in env.items()}


class RecordingRunner:
    """No shared Runner constructor: it copies the ambient environment."""

    def __init__(self, report, timeout, scratch):
        self.report, self.timeout, self.scratch = report, timeout, scratch
        self.values = {}

    def run(self, name, argv, cwd, timeout=None):
        env = controlled_environment(self.scratch, self.values)
        row = dict(id=len(self.report['runs']), name=name, argv=list(map(str, argv)), cwd=str(cwd),
                   timeout_seconds=self.timeout if timeout is None else timeout,
                   controlled_environment=encoded_environment(env), timed_out=False, client_reaped=False)
        self.report['runs'].append(row)
        child, out, err = None, b'', b''
        try:
            child = subprocess.Popen(row['argv'], cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            row['owned_client_pid'] = child.pid
            try:
                out, err = child.communicate(timeout=row['timeout_seconds'])
            except subprocess.TimeoutExpired:
                row['timed_out'] = True
                os.killpg(child.pid, signal.SIGKILL)
                out, err = child.communicate()
            except BaseException:
                row['interrupted'] = True
                os.killpg(child.pid, signal.SIGKILL)
                out, err = child.communicate()
                raise
        except OSError:
            row['spawn_error'] = True
        finally:
            row['returncode'] = child.returncode if child else None
            row['client_reaped'] = child is not None and child.returncode is not None
            row['executable_sha256'] = next((v['sha256'] for v in self.report['executables'].values()
                                           if v['path'] == row['argv'][0]), None)
            for field, raw in (('stdout', out), ('stderr', err)):
                row[field+'_full_size_bytes'] = len(raw)
                row[field+'_full_sha256'] = digest(raw)
                if len(raw) > direct.MAX_STREAM:
                    row['output_truncated'] = True
                raw = raw[:direct.MAX_STREAM]
                row[field] = raw.decode('utf-8', errors='replace')
                row[field+'_base64'] = base64.b64encode(raw).decode()
                row[field+'_sha256'] = digest(raw)
        return row


def healthy(row):
    if (type(row['returncode']) is not int or row['returncode'] < 0 or row['returncode'] in (101, 124, 125, 126, 127)
            or not row['client_reaped'] or any(row.get(k) for k in ('timed_out', 'spawn_error', 'interrupted', 'output_truncated'))
            or BAD.search(row['stderr'])):
        raise ValueError('uncontrolled process failure')


def success(row):
    healthy(row)
    if row['returncode'] != 0:
        raise ValueError('expected successful fixture command: '+row['name'])


# Starlark.checkMutable (net/starlark/java/eval/Starlark.java:262-265) for a list.
FROZEN_LIST_ERROR = 'trying to mutate a frozen list value'


def require_rejection(row, diagnostic, engine='reference', probe=None):
    healthy(row)
    if engine not in ('reference', 'native'):
        raise ValueError('unknown diagnostic engine')
    pattern = DIAGNOSTICS[diagnostic]
    if engine == 'native':
        key = probe if diagnostic == 'limit' else diagnostic
        if diagnostic == 'limit' and key not in ('name-limit', 'getter-limit'):
            raise ValueError('missing exact native limit class')
        if key in NATIVE_ERRORS:
            pattern = r'(?m)(?:^|: )'+re.escape(NATIVE_ERRORS[key])+r'[ \t]*$'
        if diagnostic == 'frozen':
            # NAMES is an ordinary list global of the frozen defs.bzl module;
            # appending from consumer.bzl fails with Bazel's Starlark.checkMutable
            # text (net/starlark/java/eval/Starlark.java:262-265), which the
            # vendored interpreter reports since N20 (docs/STARLARK_VENDOR.md;
            # before N20 the native text was "Immutable"). The full source trace
            # is still required: accept it only for this planned post-load
            # mutation, not at another evaluation phase/site.
            text = row['stderr']
            call = fixture('binding', 'frozen')['files']['consumer.bzl'].decode().splitlines()[1]
            if (probe != 'frozen'
                    or re.findall(r'(?m)^error: [^\n]*$', text) != ['error: '+FROZEN_LIST_ERROR]
                    or not re.search(r'(?m)^bazel-rs: cannot evaluate //:consumer\.bzl: Traceback \(most recent call last\):$', text)
                    or not re.search(r'(?m)^ --> //:consumer\.bzl:2:1$', text)
                    or not re.search(r'(?m)^2 \| '+re.escape(call)+r'$', text)):
                raise ValueError('expected native frozen mutation at bound consumer source')
            pattern = r'(?m)^error: '+re.escape(FROZEN_LIST_ERROR)+r'$'
    if (not 0 < row['returncode'] < 128 or row['stdout'].strip()
            or not re.search(pattern, row['stderr'], re.I if engine == 'reference' else 0)
            or SECRET in row['stdout']+row['stderr']):
        raise ValueError('expected controlled rejection: '+diagnostic)


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned fixture')
    route, _, variant = probe.partition('.')
    route = route if route in ROUTES else 'direct'
    names = '['+repr(A)+','+repr(A)+']'
    prelude = 'CAPTURED="prefix:"\n'
    tail, diagnostic, callbacks = '', None, 1
    expression = 'ctx.getenv('+repr(A)+',"MISSING")'
    before = ''
    template = name == 'watched'
    attrs = '{}'
    if variant == 'tuple':
        names = '('+repr(A)+','+repr(A)+')'
    if variant == 'snapshot':
        prelude += 'NAMES='+names+'\n'
        names, tail = 'NAMES', 'NAMES.append('+repr(B)+')\n'
    if name in ('identity', 'privacy'):
        expression = '"CONST"'
        if variant == 'dynamic' or name == 'privacy':
            names, before = '[]', 'ctx.getenv('+repr(A)+')\n'
    if name == 'defaults':
        names = '[]'
        expression = '(str(ctx.getenv('+repr(A)+'))+"/"+str(ctx.getenv('+repr(A)+',None))+"/"+ctx.getenv('+repr(A)+',"DEFAULT"))'
    if name == 'odd':
        # NUL is an octal Starlark escape, not a literal byte in the source file.
        odd = '["", "=", "A=B", "'+A+'\\000TAIL", "?"]'
        prelude += 'ODD='+odd+'\n'
        names = 'ODD' if variant == 'declared' else '[]'
        expression = '"|".join([str(ctx.getenv(n))+"/"+ctx.getenv(n,"DEFAULT") for n in ODD])'
    if name == 'module-getenv':
        # The repository callback reads no environment: its value arrives only
        # through the attribute computed by the extension's module_ctx.getenv.
        names, expression, attrs = '[]', 'ctx.attr.text', '{"text":attr.string(mandatory=True)}'
    if name == 'binding':
        template = False
        calls = {'name-keyword': 'ctx.getenv(name='+repr(A)+')',
                 'default-keyword': 'ctx.getenv('+repr(A)+',default="D")',
                 'name-type': 'ctx.getenv(1)', 'default-present': 'ctx.getenv('+repr(A)+',1)',
                 'default-absent': 'ctx.getenv('+repr(A)+',1)', 'missing': 'ctx.getenv()',
                 'extra': 'ctx.getenv('+repr(A)+',"D","EXTRA")'}
        if probe in calls:
            before = calls[probe]+'\n'
            diagnostic = ('keyword' if 'keyword' in probe else 'default-type' if probe.startswith('default-')
                          else 'name-type' if probe == 'name-type' else 'arity')
        elif probe.startswith('environ-'):
            names = 'None' if probe == 'environ-none' else '[1]'
            diagnostic, callbacks = ('environ-shape' if probe == 'environ-none' else 'environ-element'), 0
        else:
            prelude += 'NAMES=['+repr(A)+']\n'
            names, diagnostic, callbacks = 'NAMES', 'frozen', 0
    if name == 'native-boundary':
        if probe in ('nonascii', 'nonutf8'):
            names, diagnostic, callbacks = '['+repr(A)+']', 'encoding', 0
        elif probe == 'name-limit':
            names, diagnostic, callbacks = '['+repr('X'*4097)+']', 'limit', 0
        else:
            names, diagnostic = '[]', 'limit'
            before = 'for i in range(10001):\n    ctx.getenv('+repr(A)+')\n'
    body = 'print("ENV_CALLBACK_A")\n'+before
    body += 'if getattr(ctx.attr,"environ",None) != None:\n    fail("ENV_BECAME_ATTR")\n'
    body += 'value=CAPTURED+'+expression+'+"\\n"\n'
    if template:
        body += 'ctx.template("a.txt",ctx.attr.src,{"@VALUE@":value},executable=False)\n'
    else:
        body += 'ctx.file("a.txt",value,executable=False)\n'
    body += 'ctx.file("BUILD.bazel",\'exports_files(["a.txt"],visibility=["//visibility:public"])\\n\',executable=False)\n'
    if template:
        attrs = '{"src":attr.label(default="//:input.tpl",allow_single_file=True)}'
    defs = (prelude+'def _impl(ctx):\n'+common.indent(body)+
            'r=repository_rule(implementation=_impl,environ='+names+',local=False,attrs='+attrs+')\n'+tail)
    files = {'MODULE.bazel': 'module(name="environment_contract")\n', 'defs.bzl': defs,
             'input.tpl': '@VALUE@source-v1\n'}
    repos = ['made']
    if name == 'isolation':
        files['defs.bzl'] += ('def _other(ctx):\n    print("ENV_CALLBACK_B")\n'
            '    ctx.file("b.txt","other:"+ctx.getenv('+repr(B)+',"MISSING")+"\\n",executable=False)\n'
            '    ctx.file("BUILD.bazel",\'exports_files(["b.txt"],visibility=["//visibility:public"])\\n\',executable=False)\n'
            'other=repository_rule(implementation=_other)\n')
        repos.append('second')
    if route == 'ordinary':
        ext = 'load(":defs.bzl","r"'+(',"other"' if len(repos) == 2 else '')+')\n'
        made = 'r(name="made",text=ctx.getenv('+repr(A)+',"MISSING"))' if name == 'module-getenv' else 'r(name="made")'
        ext += 'def _ext(ctx):\n    '+made+'\n'+('    other(name="second")\n' if len(repos) == 2 else '')
        files['ext.bzl'] = ext+'e=module_extension(implementation=_ext)\n'
        files['MODULE.bazel'] += 'e=use_extension("//:ext.bzl","e")\nuse_repo(e,'+','.join(map(repr, repos))+')\n'
    else:
        files['MODULE.bazel'] += 'r=use_repo_rule("//:defs.bzl","r")\nr(name="made")\n'
        if len(repos) == 2:
            files['MODULE.bazel'] += 'other=use_repo_rule("//:defs.bzl","other")\nother(name="second")\n'
    if name == 'binding' and probe == 'frozen':
        files['consumer.bzl'] = 'load(":defs.bzl","NAMES")\nNAMES.append('+repr(B)+')\n'
    srcs = ['@made//:a.txt']+(['@second//:b.txt'] if len(repos) == 2 else [])
    files['BUILD.bazel'] = ('load(":consumer.bzl","NAMES")\n' if name == 'binding' and probe == 'frozen' else '')
    files['BUILD.bazel'] += ('exports_files(["input.tpl"])\n'
        'genrule(name="consume",srcs='+repr(srcs)+',outs=["result.txt"],cmd="cat $(SRCS) > $@")\n'
        'filegroup(name="result",srcs=[":consume"]+'+repr(srcs)+')\n')
    return dict(files={p: t.encode() for p, t in files.items()}, diagnostic=diagnostic,
                callbacks=callbacks, repos=len(repos), template=template)


def states(name, probe):
    base = {A: 'alpha', B: 'bee', UNUSED: 'unused', '?': 'question'}
    if name == 'binding' or name == 'native-boundary':
        if probe == 'default-absent':
            base.pop(A)
        if probe == 'nonascii':
            base[A] = 'synthetic-é'
        if probe == 'nonutf8':
            base[A] = b'synthetic-\xff'
        return [('reject', base, False)]
    if name == 'defaults':
        absent = {k: v for k, v in base.items() if k != A}
        return [('absent', absent, False), ('warm', absent, False),
                ('empty', dict(base, **{A: ''}), False), ('present', base, False), ('restore', absent, False)]
    if name == 'odd':
        absent = {k: v for k, v in base.items() if k != '?'}
        return [('absent', absent, False), ('warm', absent, False), ('present', base, False), ('restore', absent, False)]
    if name == 'repo-env':
        # The client A changes only under an unchanged override ('unrelated');
        # repo_env_override() supplies the per-state flag value.
        return [('cold', base, False), ('warm', base, False), ('watched', base, False),
                ('unrelated', dict(base, **{A: 'beta', UNUSED: 'different', B: 'bee2'}), False),
                ('restore', base, False), ('dropped', base, False)]
    if name == 'privacy':
        base[A] = SECRET
    result = [('cold', base, False), ('warm', base, False),
              ('watched', dict(base, **{A: 'beta'}), False),
              ('unrelated', dict(base, **{A: 'beta', UNUSED: 'different', B: 'bee2' if name != 'isolation' else 'bee'}), False),
              ('restore', base, False)]
    if name == 'watched':
        result += [('source', base, True), ('source-warm', base, True)]
    return result


REPO_ENV_OVERRIDES = {'cold': 'override-1', 'warm': 'override-1', 'watched': 'override-2',
                      'unrelated': 'override-2', 'restore': 'override-1', 'dropped': None}


def repo_env_override(name, probe, index):
    """The --repo_env value for A in this planned state, or None (no flag)."""
    if name != 'repo-env':
        return None
    return REPO_ENV_OVERRIDES[states(name, probe)[index][0]]


def expected_outputs(name, probe, values, source, override=None):
    value = values.get(A) if override is None else override
    if name in ('identity', 'privacy'):
        text = 'CONST'
    elif name == 'defaults':
        text = '/'.join((str(value), str(value), 'DEFAULT' if value is None else value))
    elif name == 'odd':
        q = values.get('?')
        text = '|'.join(['None/DEFAULT']*4+[str(q)+'/'+('DEFAULT' if q is None else q)])
    else:
        text = 'MISSING' if value is None else value
    data = ('prefix:'+text+'\n'+('source-v2\n' if source else 'source-v1\n' if name == 'watched' else '')).encode()
    result = {'a.txt': data}
    if name == 'isolation':
        result['b.txt'] = ('other:'+values[B]+'\n').encode()
    result['result.txt'] = b''.join(result.values())
    return result


def native_restore_reuses(name, probe):
    """N61: a warm invocation reuses the publication its specification pointer
    addresses. Fixtures whose specification keys the changing value (declared
    environ names, a declared-name list, or an extension-computed attribute)
    keep one publication per value, so a restored state reuses its own; with
    an empty declared set every state shares one specification and the
    pointer follows the latest world, so restoring re-executes exactly like
    Bazel's single external/<name> directory (which holds the last fetch)."""
    variant = probe.partition('.')[2]
    if name in ('defaults', 'privacy'):
        return False
    if name == 'identity':
        return variant != 'dynamic'
    if name == 'odd':
        return variant == 'declared'
    return True


def expected_callbacks(name, probe, engine, index):
    value = fixture(name, probe)
    if value['diagnostic']:
        return ['A']*value['callbacks']
    if index == 0:
        return ['A', 'B'] if value['repos'] == 2 else ['A']
    label = states(name, probe)[index][0]
    if engine == 'native':
        # N61: publications are keyed by the pre-execution specification and
        # revalidated against the recorded inputs, so any state equal to the
        # pointer-addressed publication's world reuses it without executing.
        # Engine-specific differences vs the reference model: a restored
        # earlier value reuses that value's own publication whenever the
        # specification keys it, and per-repository watch sets mean only the
        # repositories whose declared world changed execute (isolation).
        if label in ('warm', 'unrelated', 'source-warm') or (label == 'restore'
                                                and native_restore_reuses(name, probe)):
            return []
        if name == 'isolation':
            return ['A']
        return ['A', 'B'] if value['repos'] == 2 else ['A']
    return [] if label in ('warm', 'unrelated', 'source-warm') else ['A']


def expected_action_inputs(expected_files):
    """Phase-local equivalence key, NOT a reproduction of the native action hash.

    action.rs::prepare keys logical destinations, content digests and executable
    bits, not physical repository source paths. engine.rs expands this fixture's
    fixed genrule command using label-derived exec paths. validate_phase checks
    immutable BUILD/MODULE/definition files and expected artifact bytes/modes
    before using this key. Within that single workspace/phase, action ID, command,
    logical paths, outputs, shell flags, arguments and action environment are thus
    fixed. Only these planned source bytes can change; both source modes are 0
    executable bits. The template is a repository input, not a consumer input.
    """
    labels = {'a.txt': '@made//:a.txt', 'b.txt': '@second//:b.txt'}
    return tuple((labels[name], raw, 0) for name, raw in sorted(expected_files.items())
                 if name != 'result.txt')


def command_argv(args, ref, engine, cache, registry, command, extra=()):
    if engine == 'native':
        return direct.native_command(args.native, cache, registry, command, '//:result')[:-1]+list(extra)+['//:result']
    return (ref.startup+[command]+SAFE_FLAGS+list(BUILD_FLAGS)+[
        '--repository_cache='+str(ref.output_base.parent / 'repository-cache'),
        '--repo_contents_cache='+str(ref.output_base.parent / 'repository-contents'),
        '--registry='+registry.as_uri(), '--registry=https://bcr.bazel.build']+list(extra)+['//:result'])


def materialize(workspace, value):
    for p in workspace.iterdir():
        if p.is_symlink() or p.is_file():
            p.unlink()
        else:
            shutil.rmtree(p)
    for name, raw in value['files'].items():
        p = workspace / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(raw)
        p.chmod(0o644)


def native_metadata(cache):
    result = snapshot(sorted((cache / 'repositories/direct/manifests').glob('*.json')))
    for entry in result.values():
        raw = base64.b64decode(entry['bytes_base64'])
        data = json.loads(raw)
        check_publication_schema(data, 'manifest schema grew environment metadata')
        check_publication_privacy(data, 'environment leaked into publication manifest')
    return result


def capture_outputs(row, engine, scratch, workspace, ref, cache):
    paths = (direct.native_paths(row, '//:result', scratch) if engine == 'native' else
             reference_output_paths(row['stdout'], workspace, ref.output_base))
    outputs, roots = {}, {}
    for p in paths:
        if p.name in outputs:
            raise ValueError('duplicate artifact')
        resolved = p.resolve(strict=True)
        if not resolved.is_relative_to(scratch):
            raise ValueError('artifact escaped private scratch')
        outputs[p.name] = evidence(resolved)
        outputs[p.name]['logical_path'] = str(p)
        if p.name != 'result.txt':
            root = direct.publication_root(p, cache) if engine == 'native' else resolved.parent
            roots[p.name] = str(root)
    return outputs, roots


def phase_steps(name, probe, engine):
    value = fixture(name, probe)
    return [(i, cmd) for i, _ in enumerate(states(name, probe))
            for cmd in (['build'] if value['diagnostic'] or engine == 'native' else ['build', 'cquery'])]


def extras(name, probe, command, index):
    # Every command of a state (build and its cquery listing) carries the same
    # flag, so the listing never observes a different repository environment.
    override = repo_env_override(name, probe, index)
    flags = []
    if override is not None:
        flags = ['--repo_env='+A+'='+override] if probe.endswith('.inline') else ['--repo_env', A+'='+override]
    return flags+(['--output=files'] if command == 'cquery' else [])


def run_phase(args, runner, scratch, workspace, ref, item, probe, engine):
    name = item['name']
    value = fixture(name, probe)
    materialize(workspace, value)
    cache = scratch / 'cases' / name / probe / 'native-cache'
    phase = dict(probe=probe, engine=engine, cache=str(cache), runs=[], observations=[], status='FAIL')
    try:
        for i, command in phase_steps(name, probe, engine):
            label, values, source = states(name, probe)[i]
            runner.values = values
            if value['template']:
                (workspace / 'input.tpl').write_bytes(b'@VALUE@source-v2\n' if source else value['files']['input.tpl'])
            before = snapshot(workspace / p for p in value['files'])
            argv = command_argv(args, ref, engine, cache, scratch / 'registry', command, extras(name, probe, command, i))
            row = runner.run(name+'.'+probe+'.'+engine+'.'+str(i)+'.'+command, argv, workspace)
            phase['runs'].append(row['id'])
            observation = dict(index=i, command=command, inputs_before=before,
                               inputs_after=snapshot(workspace / p for p in value['files']), outputs={}, roots={}, metadata={})
            phase['observations'].append(observation)
            if value['diagnostic']:
                require_rejection(row, value['diagnostic'], engine, probe)
                if engine == 'native':
                    direct.no_publication(cache)
                observation['no_publication'] = True
            else:
                success(row)
                if engine == 'native' or command == 'cquery':
                    observation['outputs'], observation['roots'] = capture_outputs(row, engine, scratch, workspace, ref, cache)
                    if engine == 'native':
                        observation['metadata'] = native_metadata(cache)
                        observation['publications'] = sorted(p.name for p in direct.trees_directory(cache).iterdir())
                        # Constant callbacks produce no intentional secret output. Inspect
                        # every persistent cache file, not just JSON filename conventions.
                        if name == 'privacy':
                            entries = [p for p in cache.rglob('*') if p.is_file()]
                            if any(SECRET.encode() in p.read_bytes() for p in entries):
                                raise ValueError('synthetic secret persisted outside deliberate outputs')
                            observation['privacy_scan'] = {str(p): digest(p.read_bytes()) for p in entries}
        phase['status'] = 'PASS'
    except Exception as exc:
        phase['error'] = type(exc).__name__+': '+str(exc)
    return phase


def validate_phase(phase, report, args, workspace, ref, item):
    name, probe, engine = item['name'], phase['probe'], phase['engine']
    value = fixture(name, probe)
    scratch = Path(report['scratch_directory'])
    cache = scratch / 'cases' / name / probe / 'native-cache'
    steps = phase_steps(name, probe, engine)
    if (phase['status'] != 'PASS' or phase.get('error') or phase['cache'] != str(cache)
            or len(phase['runs']) != len(steps) or len(phase['observations']) != len(steps)):
        raise ValueError('failed or incomplete phase')
    seen_identities, seen_action_inputs, output_steps = set(), set(), []
    for rid, obs, (i, command) in zip(phase['runs'], phase['observations'], steps):
        row = report['runs'][rid]
        label, values, source = states(name, probe)[i]
        if (row['name'] != name+'.'+probe+'.'+engine+'.'+str(i)+'.'+command
                or row['argv'] != command_argv(args, ref, engine, cache, scratch / 'registry', command, extras(name, probe, command, i))
                or row['cwd'] != str(workspace) or row['timeout_seconds'] != args.timeout
                or row['controlled_environment'] != encoded_environment(controlled_environment(scratch, values))
                or (obs['index'], obs['command']) != (i, command)):
            raise ValueError('command/environment provenance mismatch')
        planned = dict(value['files'])
        if value['template'] and source:
            planned['input.tpl'] = b'@VALUE@source-v2\n'
        if (obs['inputs_before'] != obs['inputs_after'] or set(obs['inputs_before']) != {str(workspace / p) for p in planned}):
            raise ValueError('changed/unplanned fixture sources')
        for p, raw in planned.items():
            e = obs['inputs_before'][str(workspace / p)]
            if e['path'] != str(workspace / p) or e['mode'] != 0o644 or not valid_evidence(e, raw):
                raise ValueError('fixture differs from immutable plan')
        markers = sorted(re.findall(r'(?m)^(?:DEBUG: [^\n]*?:\d+:\d+: )?ENV_CALLBACK_([AB])\s*$', row['stderr']))
        expected = expected_callbacks(name, probe, engine, i) if command == 'build' else []
        if markers != expected:
            raise ValueError('engine-specific callback count mismatch')
        if value['diagnostic']:
            require_rejection(row, value['diagnostic'], engine, probe)
            if obs['outputs'] or obs['roots'] or obs['metadata'] or not obs.get('no_publication'):
                raise ValueError('rejection published outputs')
            if re.search(r'\d+ actions executed', row['stderr']):
                raise ValueError('rejection reached actions')
            continue
        success(row)
        if engine == 'reference' and command == 'build':
            if obs['outputs'] or obs['roots'] or obs['metadata']:
                raise ValueError('reference build invented output listing')
            continue
        expected_files = expected_outputs(name, probe, values, source, repo_env_override(name, probe, i))
        if set(obs['outputs']) != set(expected_files) or set(obs['roots']) != set(expected_files)-{'result.txt'}:
            raise ValueError('missing actual source/consumer artifacts')
        listed = (direct.native_paths(row, '//:result', scratch) if engine == 'native' else
                  reference_output_paths(row['stdout'], workspace, ref.output_base))
        if sorted(map(str, listed)) != sorted(e['logical_path'] for e in obs['outputs'].values()):
            raise ValueError('artifacts detached from actual CLI output')
        for p, raw in expected_files.items():
            e = obs['outputs'][p]
            if (e['kind'] != 'file' or not valid_evidence(e, raw, p == 'result.txt')
                    or not Path(e['path']).is_relative_to(scratch)
                    or (p == 'result.txt' and e['mode'] != 0o555)):
                raise ValueError('artifact bytes/mode/provenance mismatch')
            if p != 'result.txt' and Path(e['path']).parent != Path(obs['roots'][p]):
                raise ValueError('repository root detached from real artifact')
        if engine == 'native':
            roots = {Path(p).name for p in obs['roots'].values()}
            if any(not re.fullmatch('[0-9a-f]{64}', p) for p in roots):
                raise ValueError('not content-addressed publication')
            if any(Path(p).parent != direct.trees_directory(cache) for p in obs['roots'].values()):
                raise ValueError('publication escaped cache')
            seen_identities |= roots
            if obs['publications'] != sorted(seen_identities):
                raise ValueError('unplanned publication or staging residue')
            if set(obs['metadata']) != {str(cache / 'repositories/direct/manifests' / (p+'.json')) for p in seen_identities}:
                raise ValueError('missing publication manifests')
            for p, e in obs['metadata'].items():
                if not valid_evidence(e):
                    raise ValueError('invalid manifest evidence')
                raw = base64.b64decode(e['bytes_base64'])
                data = json.loads(raw)
                if data['identity'] != Path(p).stem:
                    raise ValueError('manifest environment identity mismatch')
                try:
                    check_publication_schema(data, 'manifest environment disclosure/schema mismatch')
                    check_publication_privacy(data, 'manifest environment disclosure/schema mismatch')
                except ValueError:
                    raise ValueError('manifest environment disclosure/schema mismatch')
            if name == 'privacy' and not obs.get('privacy_scan'):
                raise ValueError('missing full persistent-cache privacy scan')
            # Repository input provenance and consumer action inputs are separate:
            # a new repository tree with the same logical files must hit this
            # fixture's action cache. Never derive expectations from observed bytes.
            action_inputs = expected_action_inputs(expected_files)
            common.action_counts(row, (0, 1) if action_inputs in seen_action_inputs else (1, 0))
            seen_action_inputs.add(action_inputs)
        elif obs['metadata'] or obs.get('publications'):
            raise ValueError('reference cannot claim native publication identity')
        output_steps.append(obs)
    if not value['diagnostic'] and engine == 'native':
        roots = [o['roots'] for o in output_steps]
        if roots[0] != roots[1]:
            raise ValueError('warm publication changed')
        if roots[0]['a.txt'] == roots[2]['a.txt']:
            raise ValueError('watched change did not affect input identity')
        restore = next(i for i, (label, _, _) in enumerate(states(name, probe)) if label == 'restore')
        if roots[restore] != roots[0]:
            raise ValueError('restored inputs did not restore identity')
        if name not in ('defaults', 'odd') and roots[2] != roots[3]:
            raise ValueError('unrelated input contaminated watch set')
        if name == 'isolation' and len({r['b.txt'] for r in roots}) != 1:
            raise ValueError('repository-local watch sets contaminated each other')
        if name == 'watched' and (roots[5] == roots[0] or roots[5] != roots[6]):
            raise ValueError('template dependency transition missing')
        if name in ('identity', 'privacy'):
            inventories = []
            for obs in output_steps:
                root = Path(obs['roots']['a.txt']).name
                e = obs['metadata'][str(cache / 'repositories/direct/manifests' / (root+'.json'))]
                inventories.append(json.loads(base64.b64decode(e['bytes_base64']))['files'])
            if any(x != inventories[0] for x in inventories):
                raise ValueError('constant-output manifest inventory changed')


def phases_for(item, reference_only=False):
    return common.phases_for(item, reference_only)


def argument_parser():
    parser = common.argument_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/repository-environment-validation.json')
    parser.add_argument('--official-source', type=Path, default=Path('/tmp/opencode/bazel-9.3-reference'))
    return parser


def source_pins(args):
    # Inherited environment is not inspected even for nonfixture tool checks.
    commit = subprocess.check_output(['git', '-C', str(args.official_source), 'rev-parse', 'HEAD'], text=True).strip()
    if commit != OFFICIAL_COMMIT:
        raise ValueError('wrong official source commit')
    result = {}
    for relative, expected in OFFICIAL.items():
        raw = (args.official_source / relative).read_bytes()
        blob = subprocess.check_output(['git', '-C', str(args.official_source), 'show', commit+':'+relative])
        if digest(raw) != expected or blob != raw:
            raise ValueError('official source pin mismatch')
        result[relative] = expected
    return result


def pid_state(pid):
    if pid is None:
        return 'ABSENT'
    try:
        state = Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()[0]
        return 'ZOMBIE' if state == 'Z' else 'LIVE'
    except FileNotFoundError:
        return 'ABSENT'


def shutdown(ref, runner, report):
    # Inspect only the PID supplied by this owned private output base. Never scan
    # global process argv or environment; retain the PID even if shutdown removes it.
    pidfile = ref.output_base / 'server/server.pid.txt'
    pid = int(pidfile.read_text().strip()) if pidfile.exists() else None
    if pid is not None and pid <= 1:
        raise ValueError('invalid private server PID')
    runner.values = {}
    row = runner.run('repository-environment.shutdown', ref.startup+['shutdown'], ref.workspace, min(runner.timeout, 15))
    state = pid_state(pid)
    forced = False
    if state == 'LIVE':
        os.kill(pid, signal.SIGKILL)
        forced = True
        time.sleep(0.1)
        state = pid_state(pid)
    report['server_cleanup'] = dict(owned_pid=pid, state=state, forced=forced, run=row['id'])
    success(row)
    if forced or state == 'LIVE':
        raise ValueError('reference required forced server cleanup')


def report_passes(report):
    try:
        args = argument_parser().parse_args([])
        for k, v in report['configuration'].items():
            setattr(args, k, Path(v) if k in ('native', 'reference', 'output', 'scratch_parent', 'official_source') else v)
        if (report['planned_checks'] != CASEPLAN or report['plan_sha256'] != digest(json.dumps(CASEPLAN, sort_keys=True).encode())
                or report['errors'] or report['sources_before'] != report['sources_after']
                or set(report['sources_before']) != set(map(str, SOURCES))
                or report['executables'] != report['executables_after']
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['official_sources_before'] != OFFICIAL or report['official_sources_after'] != OFFICIAL
                or report['reference_release'] != 'bazel 9.3.0'
                or [(c['name'], c['kind']) for c in report['checks']] != PLAN):
            return False
        if any(p != e['path'] or not valid_evidence(e) for p, e in report['sources_before'].items()):
            return False
        engines = {'reference'} if args.reference_only else {'native', 'reference'}
        if set(report['executables']) != engines or (not args.reference_only and report['executables']['native']['sha256'] != args.native_sha256):
            return False
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        if report['cleanup_receipt'] != dict(path=str(scratch), removed=True):
            return False
        cleanup = report['server_cleanup']
        if cleanup['forced'] or cleanup['state'] not in ('ABSENT', 'ZOMBIE'):
            return False
        original = {str(scratch / 'registry' / Path(p).relative_to(REGISTRY)): dict(e, path=str(scratch / 'registry' / Path(p).relative_to(REGISTRY)))
                    for p, e in report['sources_before'].items() if Path(p).is_relative_to(REGISTRY)}
        if report['registry_before'] != original or report['registry_after'] != original:
            return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for row in runs:
            healthy(row)
            if (not isinstance(row['owned_client_pid'], int) or row['owned_client_pid'] <= 1
                    or set(row['controlled_environment']) - (KEYS | {'PATH', 'HOME', 'TMPDIR', 'LC_ALL'})):
                return False
            binary = next(v for v in report['executables'].values() if v['path'] == row['argv'][0])
            if row['executable_sha256'] != binary['sha256']:
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(row[field+'_base64'], validate=True)
                if (digest(raw) != row[field+'_sha256'] or digest(raw) != row[field+'_full_sha256']
                        or len(raw) != row[field+'_full_size_bytes'] or len(raw) > direct.MAX_STREAM
                        or raw.decode('utf-8', errors='replace') != row[field]):
                    return False
        ref = DirectReference(args.reference, workspace, scratch, None, 'repository-environment')
        used = [0]
        for row, name, argv, timeout in (
                (runs[0], 'reference.version', ref.startup+['version', '--gnu_format'], args.timeout),
                (runs[-1], 'repository-environment.shutdown', ref.startup+['shutdown'], min(args.timeout, 15))):
            success(row)
            if (row['name'] != name or row['argv'] != argv or row['cwd'] != str(workspace)
                    or row['timeout_seconds'] != timeout
                    or row['controlled_environment'] != encoded_environment(controlled_environment(scratch, {}))):
                return False
        if runs[0]['stdout'].strip() != 'bazel 9.3.0' or cleanup['run'] != len(runs)-1:
            return False
        for item, check in zip(CASEPLAN, report['checks']):
            pairs = phases_for(item, args.reference_only)
            if ([(p['probe'], p['engine']) for p in check['phases']] != pairs
                    or check['status'] != ('PASS' if pairs else 'SKIP') or check.get('error')):
                return False
            for phase in check['phases']:
                validate_phase(phase, report, args, workspace, ref, item)
                used.extend(phase['runs'])
        used.append(len(runs)-1)
        return used == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ('native', 'reference', 'output', 'scratch_parent', 'official_source'):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.output in [args.native, args.reference, *SOURCES] or any(args.output.is_relative_to(ROOT / p) for p in ('src', 'scripts', 'tests')):
        raise ValueError('report cannot overwrite source/executable')
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[],
        planned_checks=CASEPLAN, plan_sha256=digest(json.dumps(CASEPLAN, sort_keys=True).encode()),
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        limitations=[
            'Only explicit synthetic child environment keys; no host environment names/values inspected.',
            'Native fresh CLI callbacks always execute; genuine warm persistent-server callbacks may skip.',
            'Reference paths are observations, never native content-addressed identity claims.',
            'Genuine builtin toolchain closure may download pinned upstream archives; no synthetic replacement toolchains.',
            'Reference-only preparation cannot certify candidate parity; source movement fails the report.',
            'NonASCII/nonUTF8/quotas are EXPLICIT_NATIVE_BOUNDARY, not Bazel error parity.',
            'Reader error memoization, raw absent/default watch counts, runtime clone ownership, resource digest vectors are core-unit gates.',
            'Initial OS allocation cannot be precharged; no oversized OS spawn or RSS/OOM guarantee.',
            'module_ctx.getenv is differential only through an extension-computed repository attribute (module-getenv).',
            '--repo_env is differential only as a repository_ctx.getenv override of one declared key (repo-env).',
            'No module_extension(environ=...), OS SDK, other flag override, or application completeness claim here (see validate_host_os).'])
    scratch = ref = runner = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['official_sources_before'] = source_pins(args)
        report['executables'] = common.executable_evidence(args)
        scratch = Path(tempfile.mkdtemp(prefix='repository-environment-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        for name in ('workspace', 'home', 'tmp'):
            (scratch / name).mkdir()
        workspace = scratch / 'workspace'
        (workspace / 'MODULE.bazel').write_text('module(name="environment_contract")\n')
        shutil.copytree(REGISTRY, scratch / 'registry')
        report['registry_before'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        runner = RecordingRunner(report, args.timeout, scratch)
        ref = DirectReference(args.reference, workspace, scratch, runner, 'repository-environment')
        row = runner.run('reference.version', ref.startup+['version', '--gnu_format'], workspace)
        success(row)
        report['reference_release'] = row['stdout'].strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('wrong genuine reference version')
        for item in CASEPLAN:
            check = dict(name=item['name'], kind=item['kind'], phases=[], status='FAIL')
            report['checks'].append(check)
            for probe, engine in phases_for(item, args.reference_only):
                phase = run_phase(args, runner, scratch, workspace, ref, item, probe, engine)
                check['phases'].append(phase)
                validate_phase(phase, report, args, workspace, ref, item)
            check['status'] = 'PASS' if check['phases'] else 'SKIP'
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        if ref is not None:
            try:
                shutdown(ref, runner, report)
            except Exception as exc:
                report['errors'].append('shutdown: '+str(exc))
        if scratch is not None:
            report['registry_after'] = snapshot(p for p in (scratch / 'registry').rglob('*') if p.is_file())
            shutil.rmtree(scratch)
            report['cleanup_receipt'] = dict(path=str(scratch), removed=not scratch.exists())
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['official_sources_after'] = source_pins(args)
            report['executables_after'] = common.executable_evidence(args)
        except Exception as exc:
            report['errors'].append('final evidence: '+str(exc))
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'PASS'
        write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return 130 if interrupted else 0 if report['status'] in ('PASS', 'REFERENCE_ONLY_PASS') else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
