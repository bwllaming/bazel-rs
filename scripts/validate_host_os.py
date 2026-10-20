#!/usr/bin/env python3
"""Controlled Linux/x86_64 host metadata QA, not full Bazel/platform parity.

Reference preparation never executes native. Full scoped CLI execution requires
an explicit frozen candidate and an independently pinned K binary for migration.
Cross-host injections and frozen loader ownership are separate native unit gates.
repository_ctx.os.environ and module_ctx.os.environ/getenv/environ= are a
single-state differential over the controlled client environment. No ambient
environment enumeration or host override.
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

import validate_repository_environment as controlled
import validate_repository_rules as direct
import validate_string_list_dict as common
from validate import file_evidence, reference_output_paths, validate_executable, write_report
from validate_direct_repositories import DirectReference, REFERENCE_SHA256

ROOT = Path(__file__).resolve().parents[1]
REGISTRY = ROOT / 'tests/fixtures/builtin_modules/registry'
OFFICIAL_COMMIT = 'e89755babf3e941bc1e9d8120b1cd48854ab9dfa'
JAVA = 'src/main/java/'
OFFICIAL = {JAVA + path: sha for path, sha in {
    'com/google/devtools/build/lib/bazel/repository/starlark/StarlarkOS.java':
        '387d1f882d4b81fea5b80ae2961aae5bdffec71c6efb149ddd677c262405a213',
    'com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java':
        'dcdc9a92b48523146b2870b87d06c9c9d40efcf15d10f86853c9fe128204a231',
    'com/google/devtools/build/lib/bazel/bzlmod/RegularRunnableExtension.java':
        '54978dc8dc76652fdf8050ed256d40e16394ad5fbcfa7d0bfd6f39a7cf464abb',
    'com/google/devtools/build/lib/bazel/bzlmod/ModuleExtensionEvalFactors.java':
        '5315b7dcfaa0fad2245981eb6e0267744563cfe2699905e88e4a163e98813bc0',
    'com/google/devtools/build/lib/bazel/repository/DigestWriter.java':
        'a13bc4c01bccff496d6349340d9b082f8110a71428aec5edba2d884ac97a00c2',
    'com/google/devtools/build/lib/util/OS.java':
        '6105d83a2c6027652f7a56d67d39735086da72533a32c1f3ec857e7fba6ee97a',
    'net/starlark/java/lib/json/Json.java':
        'c130342af69ef9b80ee2d4c65fdbd7a985ffacaa5876ae02090c97d651d7b17b',
}.items()}
REPR = '<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkOS>'
A, UNUSED = controlled.A, controlled.UNUSED
digest, evidence, snapshot = direct.digest, direct.evidence, direct.snapshot
valid_evidence = direct.valid_evidence
ROUTES = ('direct', 'ordinary')
FLAGS = {'default': '', '00': ',os_dependent=False,arch_dependent=False',
         '10': ',os_dependent=True,arch_dependent=False',
         '01': ',os_dependent=False,arch_dependent=True',
         '11': ',os_dependent=True,arch_dependent=True'}
FACTORS = {'default': 'general', '00': 'general', '10': 'os:linux',
           '01': 'arch:amd64', '11': 'os:linux,arch:amd64'}
BINDINGS = {'os-int': 'os_dependent=1', 'arch-string': 'arch_dependent="yes"',
            'os-none': 'os_dependent=None', 'arch-none': 'arch_dependent=None',
            'positional': 'False,False'}
CASEPLAN = [
    common.case('metadata', ('direct', *FLAGS)),
    common.case('immutable', ('direct.name', 'ordinary.arch', 'extension.name')),
    common.case('binding', tuple(BINDINGS)),
    common.case('cache', ROUTES),
    common.case('getenv-control', ROUTES),
    common.case('environ', ROUTES + ('extension', 'ctor')),
    common.case('native-boundary', tuple(r+'.whole-json' for r in ROUTES) +
        ('extension.whole-json', 'proxy.os', 'analysis.os', 'build.os'), 'native_boundary'),
    common.case('migration', ROUTES, 'native_version_migration'),
]
UNIT_GATES = [
    'Synthetic native host/factor matrix: unconditional direct AND ordinary IDs, selected extension factors only.',
    'Getter name versus canonical OS factor; identity token excluded from all input hashes.',
    'Saved/frozen OS aliases, dictionary keys and callbacks survive freeze and loader drop.',
    'ExecutionInputs legacy serialization, versioned repository-host-v1 envelope, resource/unsupported-target errors.',
]
LIMITATIONS = [
    'Linux/x86_64 compile-target view linux/amd64 only; not runtime/emulation detection or all-platform support.',
    'Same-host outputs do not prove cross-host flag keying; required native-unit receipts are separate.',
    'Reference whole-object JSON is source-derived only and UNPROBED; whole-json is NATIVE_BOUNDARY.',
    'repository_ctx.os.environ is ONE cold state per route: Bazel records no dependency for it (StarlarkOS.java:49-54) and may serve a cached repository while native re-executes every invocation, so cross-invocation environment transitions are not differential; module_ctx.os.environ, module_ctx.getenv and module_extension(environ=...) are differential probes (extension, ctor).',
    'K-to-L migration is NATIVE_VERSION_MIGRATION, never reference parity; the pinned K baseline predates N51/N52/N61, so every repository rekeys once (N61 identity stability holds from N61 on).',
    'A new repository ID with equal logical bytes may hit the consumer action cache.',
    'No ambient names/values, JVM property overrides, public host injection, SDK/application or full acceptance claim.',
    'BUILD probe checks only undefined global os at its bound compile-time source site; not full BUILD namespace parity.',
]


def source_paths():
    # Computed from this checkout, never a main-worktree absolute constant.
    return sorted(set([ROOT / 'Cargo.toml', ROOT / 'Cargo.lock',
        *ROOT.glob('scripts/*.py'), *ROOT.glob('tests/*.py'), *ROOT.glob('tests/*.rs'),
        *(ROOT / 'src').rglob('*.rs'),
        *(p for p in REGISTRY.rglob('*') if p.is_file()),
        *(p for p in (ROOT / 'tests/fixtures/host_os').rglob('*') if p.is_file())]))


def fingerprints(paths):
    return {str(p): dict(sha256=digest(p.read_bytes()), mode=p.stat().st_mode & 0o7777)
            for p in sorted(paths)}


def official_pins(args, scratch, verify_git=False):
    env = controlled.controlled_environment(scratch, {})
    def git(*words):
        return subprocess.check_output(['/usr/bin/git', '-C', str(args.official_source), *words],
            env=env, stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=args.timeout)
    if verify_git and git('rev-parse', 'HEAD').decode().strip() != OFFICIAL_COMMIT:
        raise ValueError('wrong official source commit')
    for path, sha in OFFICIAL.items():
        raw = (args.official_source / path).read_bytes()
        if digest(raw) != sha or (verify_git and git('show', OFFICIAL_COMMIT+':'+path) != raw):
            raise ValueError('official source pin mismatch: '+path)
    return dict(OFFICIAL)


def executables(args):
    result = common.executable_evidence(args)
    if not args.reference_only:
        if not args.baseline_native or not re.fullmatch('[0-9a-f]{64}', args.baseline_sha256 or ''):
            raise ValueError('independently pinned K baseline binary required for migration')
        validate_executable(args.baseline_native, official=True)
        result['baseline'] = file_evidence(args.baseline_native)
        if result['baseline']['sha256'] != args.baseline_sha256 or args.baseline_sha256 == args.native_sha256:
            raise ValueError('wrong/identical K baseline binary; no fallback')
    return result


class RecordingRunner(controlled.RecordingRunner):
    """Reuse K's explicit environment runner, never the ambient Runner constructor."""
    def __init__(self, report, args, scratch):
        super().__init__(report, args.timeout, scratch)
        self.args = args

    def guard(self):
        if (fingerprints(source_paths()) != self.report['sources_before']
                or executables(self.args) != self.report['executables']
                or official_pins(self.args, self.scratch) != self.report['official_sources_before']
                or fingerprints(p for p in (self.scratch / 'registry').rglob('*') if p.is_file())
                != self.report['registry_before']):
            raise ValueError('source/registry/official/binary drift before command')

    def run(self, name, argv, cwd, timeout=None):
        # Cleanup must still run after source movement; it cannot rescue acceptance.
        if name != 'host-os.shutdown':
            self.guard()
        row = super().run(name, argv, cwd, timeout)
        row['precommand_pins_verified'] = name != 'host-os.shutdown'
        return row


METADATA = '''def metadata(ctx):
    host = ctx.os
    name = getattr(host, "name")
    arch = getattr(host, "arch")
    keyed = {host: "safe-key"}
    return {
        "context_type": type(ctx), "type": type(host), "dir": dir(host),
        "str": str(host), "repr": repr(host),
        "same_context_object": host == ctx.os,
        "hashable_lookup": keyed[ctx.os],
        "name": name, "arch": arch,
        "name_type": type(name), "arch_type": type(arch),
        "name_str": str(name), "arch_str": str(arch),
        "name_repr": repr(name), "arch_repr": repr(arch),
        "name_json": json.encode(name), "arch_json": json.encode(arch),
        "lower_platform": name.lower(),
        "short_ascii_name_ops": [name.startswith("lin"), name.endswith("ux"), name.lower() == "linux", name == "Linux"],
        "arch_spellings": [arch == "amd64", arch == "x86_64"],
    }
'''


def golden(context):
    return dict(context_type=context, type='repository_os', dir=['arch', 'environ', 'name'],
        str=REPR, repr=REPR, same_context_object=True, hashable_lookup='safe-key',
        name='linux', arch='amd64', name_type='string', arch_type='string',
        name_str='linux', arch_str='amd64', name_repr='"linux"', arch_repr='"amd64"',
        name_json='"linux"', arch_json='"amd64"', lower_platform='linux',
        short_ascii_name_ops=[True, True, True, False], arch_spellings=[True, False])


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':'))+'\n').encode()


def fixture(name, probe):
    item = next((c for c in CASEPLAN if c['name'] == name), None)
    if item is None or probe not in item['probes']:
        raise ValueError('unknown planned fixture')
    route, _, variant = probe.partition('.')
    ordinary = route != 'direct'
    if name == 'metadata':
        ordinary = probe != 'direct'
    if name in ('binding',):
        ordinary = True
    repository_body = ''
    extension_body = ''
    ctor = FLAGS.get(probe, '') if name == 'metadata' else ''
    repository_ctor = ''
    diagnostic = None
    expression = '"CONST\\n"'
    attrs = '{"text":attr.string()}'
    repository_args = 'text="null"'
    if name == 'metadata':
        expression = ('json.encode({"extension":json.decode(ctx.attr.text),'
            '"repository":metadata(ctx),"selected":ctx.os.name.lower()+"/"+ctx.os.arch})+"\\n"')
        if ordinary:
            repository_args = 'text=json.encode(metadata(ctx))'
    if name == 'immutable':
        diagnostic = 'immutable-'+variant
        body = 'host=ctx.os\nhost.'+variant+'="controlled"\n'
        if route == 'extension':
            extension_body = body
        else:
            repository_body = body
    if name == 'binding':
        ctor = ','+BINDINGS[probe]
        diagnostic = 'positional' if probe == 'positional' else probe.split('-')[0]+'_dependent'
    if name == 'cache':
        attrs = '{"text":attr.string(),"src":attr.label(default="//:input.tpl",allow_single_file=True)}'
        repository_body = 'selected=ctx.os.name+"/"+ctx.os.arch\n'
    if name == 'getenv-control':
        repository_ctor = ',environ=['+repr(A)+']'
        expression = 'ctx.getenv('+repr(A)+',"DEFAULT")+":"+ctx.os.name+"/"+ctx.os.arch+"\\n"'
    if name == 'environ':
        # StarlarkOS.environ is the command repoEnv: the whole client environment,
        # for repository_ctx and module_ctx alike; module_ctx.getenv and
        # module_extension(environ=...) are extension environment inputs.
        observed = ('[ctx.os.environ.get('+repr(A)+',"DEFAULT"),sorted(ctx.os.environ.keys()),'
                    'getattr(ctx.os,"environ",{})==ctx.os.environ,type(ctx.os.environ)')
        getenv = 'ctx.getenv('+repr(A)+',"DEFAULT")'
        if route == 'extension':
            ordinary = True
            extension_body = 'observed=json.encode('+observed+','+getenv+'])\n'
            repository_args = 'text=observed'
            expression = 'ctx.attr.text+"\\n"'
        elif route == 'ctor':
            ordinary = True
            ctor = ',environ=['+repr(A)+']'
            extension_body = 'observed='+getenv+'\n'
            repository_args = 'text=observed'
            expression = 'ctx.attr.text+"\\n"'
        else:
            expression = 'json.encode('+observed+'])+"\\n"'
    if name == 'native-boundary':
        diagnostic = variant
        if variant == 'whole-json':
            body = 'value=json.encode(ctx.os)\n'
            if route == 'extension':
                extension_body = body
            else:
                repository_body = body
    body = 'print("HOST_REPO_CALLBACK")\n'+repository_body
    if name == 'cache':
        body += 'ctx.template("data.txt",ctx.attr.src,{"@HOST@":selected},executable=False)\n'
    else:
        body += 'ctx.file("data.txt",'+expression+',executable=False)\n'
    body += 'ctx.file("BUILD.bazel",\'exports_files(["data.txt"],visibility=["//visibility:public"])\\n\',executable=False)\n'
    defs = (METADATA if name == 'metadata' else '')+'def _repo(ctx):\n'+common.indent(body)
    defs += 'make=repository_rule(implementation=_repo,attrs='+attrs+repository_ctor+')\n'
    ext = 'load(":defs.bzl","make"'+(',"metadata"' if name == 'metadata' else '')+')\n'
    ext += 'def _ext(ctx):\n'+common.indent('print("HOST_EXT_CALLBACK")\n'+extension_body+
        'make(name="made",'+repository_args+')\n')
    ext += 'ext=module_extension(_ext'+ctor+')\n'
    module = 'module(name="host_os_contract")\n'
    module += ('e=use_extension("//:ext.bzl","ext")\nuse_repo(e,"made")\n' if ordinary else
               'make=use_repo_rule("//:defs.bzl","make")\nmake(name="made",text="null")\n')
    build = ('exports_files(["input.tpl"])\n'
        'genrule(name="consume",srcs=["@made//:data.txt"],outs=["result.txt"],cmd="cat $(SRCS) > $@")\n'
        'filegroup(name="result",srcs=[":consume","@made//:data.txt"])\n')
    files = dict(zip(('MODULE.bazel', 'BUILD.bazel', 'defs.bzl', 'ext.bzl', 'input.tpl'),
                     (module, build, defs, ext, '@HOST@source-v1\n')))
    if name == 'native-boundary' and route == 'proxy':
        files['MODULE.bazel'] += 'value=e.os.name\n'
        diagnostic = 'proxy-os'
    if name == 'native-boundary' and route in ('analysis', 'build'):
        if route == 'analysis':
            files['consumer.bzl'] = ('def _impl(ctx):\n    print("HOST_ANALYSIS_CALLBACK")\n'
                '    value=ctx.os\n    return []\nconsume=rule(implementation=_impl)\n')
            files['BUILD.bazel'] = 'load(":consumer.bzl","consume")\nconsume(name="result")\n'
        else:
            # Undefined names fail before execution. BUILD has no print builtin,
            # so no callback marker can establish this compile-time boundary.
            files['BUILD.bazel'] = 'value=os.name\nfilegroup(name="result",srcs=[])\n'
        diagnostic = route+'-os'
    return dict(files={p: v.encode() for p, v in files.items()}, ordinary=ordinary,
                diagnostic=diagnostic, name=name, probe=probe)


def states(name):
    if name == 'cache':
        return [('cold', {}, False), ('warm', {}, False), ('source-change', {}, True),
                ('source-warm', {}, True), ('restore', {}, False)]
    if name == 'environ':
        return [('cold', {A: 'alpha'}, False)]
    if name == 'getenv-control':
        return [('cold', {A: 'alpha'}, False), ('warm', {A: 'alpha'}, False),
                ('watched', {A: 'beta'}, False), ('unrelated', {A: 'beta', UNUSED: 'unused'}, False),
                ('restore', {A: 'alpha'}, False), ('absent', {}, False)]
    return [('cold', {}, False)]


def expected_bytes(value, env, changed):
    if value['name'] == 'metadata':
        return json_bytes(dict(extension=golden('module_ctx') if value['ordinary'] else None,
                               repository=golden('repository_ctx'), selected='linux/amd64'))
    if value['name'] == 'cache':
        return b'linux/amd64source-v2\n' if changed else b'linux/amd64source-v1\n'
    if value['name'] == 'getenv-control':
        return (env.get(A, 'DEFAULT')+':linux/amd64\n').encode()
    if value['name'] == 'environ':
        if value['probe'] == 'ctor':
            return (env.get(A, 'DEFAULT')+'\n').encode()
        keys = sorted(controlled.controlled_environment(Path('/unused'), env))
        observed = [env.get(A, 'DEFAULT'), keys, True, 'dict']
        if value['probe'] == 'extension':
            observed.append(env.get(A, 'DEFAULT'))
        return (json.dumps(observed, separators=(',', ':'))+'\n').encode()
    return b'CONST\n'


def phases_for(item, reference_only):
    if item['kind'] != 'differential':
        return [] if reference_only else [(p, 'migration' if item['name'] == 'migration' else 'native')
                                         for p in item['probes']]
    return [(p, e) for p in item['probes'] for e in (('reference',) if reference_only else ('reference', 'native'))]


def command_argv(args, ref, engine, cache, registry, command):
    if engine != 'reference':
        executable = args.baseline_native if engine == 'baseline' else args.native
        return direct.native_command(executable, cache, registry, command, '//:result')
    flags = [f for f in controlled.SAFE_FLAGS if not f.startswith('--lockfile_mode')]
    return ref.startup+[command]+flags+list(controlled.BUILD_FLAGS)+[
        '--lockfile_mode=update', '--repository_cache='+str(ref.output_base.parent / 'repository-cache'),
        '--repo_contents_cache='+str(ref.output_base.parent / 'repository-contents'),
        '--registry='+registry.as_uri(), '--registry=https://bcr.bazel.build']+(
        ['--output=files'] if command == 'cquery' else [])+['//:result']


def markers(row):
    return re.findall(r'(?m)^(?:DEBUG: [^\n]*?:\d+:\d+: )?(HOST_(?:REPO_CALLBACK|EXT_CALLBACK|ANALYSIS_CALLBACK|BUILD_PHASE))\s*$', row['stderr'])


def require_rejection(row, value, engine):
    # Missing ctx.os cannot stand in for an unsupported environ/JSON getter.
    if engine not in ('native', 'reference'):
        raise ValueError('unknown diagnostic engine')
    diagnostic = value['diagnostic']
    text = row['stderr']
    health_text = text
    if diagnostic == 'build-os':
        # Suppress only this expected undefined-name error, not crashes,
        # infrastructure errors or a different missing builtin such as print.
        # Since N33 the native compiler reports Bazel's own undefined-name
        # text (formerly "Variable `os` not found").
        health_text = (re.sub(r"(?m)^bazel-rs: error: name 'os' is not defined$", '', text)
                       if engine == 'native' else '')
    controlled.healthy(dict(row, stderr=health_text))
    if (not 0 < row['returncode'] < 128 or row['stdout'].strip()
            or re.search(r'panicked|FATAL:|parse error|unexpected token|permission denied|failed to download', text, re.I)):
        raise ValueError('uncontrolled rejection')
    patterns = {
        'whole-json': r'cannot encode unsupported value as JSON',
        'proxy-os': r'(?:os[^\n]*(?:method|tag)|(?:method|tag)[^\n]*os)',
        'analysis-os': r'(?:[Aa]ttribute|field)[^\n]*os|os[^\n]*(?:attribute|field)',
        'build-os': r'(?:Variable [`\'"]?os|\bos\b)[^\n]*(?:not found|not defined)|undefined[^\n]*\bos\b',
        'os_dependent': r'(?:os_dependent[^\n]*bool|bool[^\n]*os_dependent)',
        'arch_dependent': r'(?:arch_dependent[^\n]*bool|bool[^\n]*arch_dependent)',
        'positional': r'(?:module_extension[^\n]*positional|extra positional argument|positional argument[^\n]*module_extension)',
    }
    if diagnostic.startswith('immutable-'):
        field = diagnostic.split('-')[1]
        required_source = 'host.'+field+'="controlled"'
        if engine == 'reference':
            pattern = r'cannot set \.'+field+r' field of repository_os value'
        else:
            # Only the planned host-field assignment may accept this native
            # diagnostic. Since N33 the native interpreter reports Bazel's
            # field-assignment text (formerly "Operation `.<field>=` not
            # supported on type `repository_os`"); the source site stays bound.
            # Do not generalize to other fields, types or phases.
            planned = fixture('immutable', value['probe'])
            if (value['name'] != 'immutable' or field not in ('name', 'arch')
                    or diagnostic != planned['diagnostic'] or value['files'] != planned['files']):
                raise ValueError('unplanned native immutable assignment')
            source = 'ext.bzl' if value['probe'].startswith('extension.') else 'defs.bzl'
            lines = planned['files'][source].decode().splitlines()
            assignment = '    '+required_source
            line = lines.index(assignment)+1
            error = 'error: cannot set .'+field+' field of repository_os value'
            pattern = r'(?m)^'+re.escape(error)+r'$'
            if (re.findall(r'(?m)^error: [^\n]*$', text) != [error]
                    or not re.search(r'(?m)^ --> //:'+re.escape(source)+':'+str(line)+r':5$', text)
                    or not re.search(r'(?m)^'+str(line)+r' \| '+re.escape(assignment)+r'$', text)
                    or row['stdout']):
                raise ValueError('wrong native immutable diagnostic body/site')
    elif engine == 'native' and diagnostic == 'proxy-os':
        # In MODULE, e.os is the existing tag proxy, NOT module_ctx.os.
        # Only its second attribute access at this bound preflight source site
        # may accept this diagnostic; it proves no metadata getter succeeded.
        planned = fixture('native-boundary', 'proxy.os')
        if (value['name'] != planned['name'] or value['probe'] != planned['probe']
                or value['files'] != planned['files']):
            raise ValueError('unplanned native tag-proxy boundary')
        source = 'MODULE.bazel'
        required_source = 'value=e.os.name'
        line = planned['files'][source].decode().splitlines().index(required_source)+1
        workspace = Path(row.get('cwd', ''))
        # Since N33: Bazel's own text for the same expression (genuine 9.3.0:
        # "Error: 'TagCallable' value has no field or method 'name'"); formerly
        # "Object of type `TagCallable` has no attribute `name`".
        error = "bazel-rs: error: 'TagCallable' value has no field or method 'name'"
        pattern = r'(?m)^'+re.escape(error)+r'$'
        if (not workspace.is_absolute() or '..' in workspace.parts or workspace.name != 'workspace'
                or re.findall(r'(?m)^(?:bazel-rs: )?error: [^\n]*$', text) != [error]
                or not re.search(r'(?m)^ --> '+re.escape(str(workspace / source))+':'+str(line)+r':7$', text)
                or not re.search(r'(?m)^'+str(line)+r' \| '+re.escape(required_source)+r'$', text)
                or re.search(r'\d+ (?:actions executed|cache hits)', text) or row['stdout']):
            raise ValueError('wrong native tag-proxy diagnostic body/preflight site')
    elif engine == 'native' and diagnostic == 'build-os':
        planned = fixture('native-boundary', 'build.os')
        if (value['name'] != planned['name'] or value['probe'] != planned['probe']
                or value['files'] != planned['files']):
            raise ValueError('unplanned native BUILD namespace boundary')
        required_source = 'value=os.name'
        workspace = Path(row.get('cwd', ''))
        # Genuine 9.3.0 for the same BUILD line: "name 'os' is not defined".
        error = "bazel-rs: error: name 'os' is not defined"
        pattern = r'(?m)^'+re.escape(error)+r'$'
        if (not workspace.is_absolute() or '..' in workspace.parts or workspace.name != 'workspace'
                or re.findall(r'(?m)^(?:bazel-rs: )?error: [^\n]*$', text) != [error]
                or not re.search(r'(?m)^ --> '+re.escape(str(workspace / 'BUILD.bazel'))+r':1:7$', text)
                or not re.search(r'(?m)^1 \| '+re.escape(required_source)+r'$', text)
                or re.search(r'\d+ (?:actions executed|cache hits)|syntax error|no such target|missing target', text, re.I)
                or row['stdout']):
            raise ValueError('wrong native BUILD undefined-os diagnostic body/source site')
    else:
        pattern = patterns[diagnostic]
        required_source = None
    # The generic patterns describe the diagnostic message: match them with
    # absolute paths masked, so a workspace or worktree path (e.g. one
    # containing "tag" and "os") never stands in for the message text.
    message = re.sub(r'/[^\s:\'"`]+', '<path>', text) if pattern == patterns.get(diagnostic) else text
    if not re.search(pattern, message) or (required_source and required_source not in text.replace(' ', '')):
        raise ValueError('wrong diagnostic body/site: '+diagnostic)
    route = value['probe'].split('.')[0]
    expected = ([] if value['name'] == 'binding' or route in ('ctor', 'proxy', 'build') else
                ['HOST_ANALYSIS_CALLBACK'] if route == 'analysis' else
                ['HOST_EXT_CALLBACK'] if route == 'extension' else
                (['HOST_EXT_CALLBACK'] if value['ordinary'] else [])+['HOST_REPO_CALLBACK'])
    if markers(row) != expected:
        raise ValueError('wrong rejection callback phase')
    if engine == 'native' and (diagnostic.startswith('immutable-') or diagnostic in ('proxy-os', 'build-os')):
        all_markers = re.findall(r'(?m)^(?:DEBUG: [^\n]*?:\d+:\d+: )?(HOST_[A-Z_]+)\s*$', text)
        if all_markers != expected:
            raise ValueError('unexpected native rejection after/phase marker')
    if 'os' in diagnostic and diagnostic not in ('analysis-os', 'build-os', 'proxy-os'):
        if re.search(r'(?:no|has no|missing) (?:attribute|field)[^\n]*[\'"]os', text):
            raise ValueError('missing host model is not a boundary')


def lockfile_factors(workspace, probe):
    path = workspace / 'MODULE.bazel.lock'
    data = json.loads(path.read_bytes())
    extensions = data.get('moduleExtensions', {})
    own = {k: v for k, v in extensions.items() if k.endswith('//:ext.bzl%ext')}
    if len(own) != 1:
        raise ValueError('missing genuine own extension lockfile entry')
    factors = next(iter(own.values()))
    if list(factors) != [FACTORS[probe]]:
        raise ValueError('wrong genuine lockfile evaluation factor')
    attrs = factors[FACTORS[probe]]['generatedRepoSpecs']['made']['attributes']
    if json.loads(attrs['text']) != golden('module_ctx'):
        raise ValueError('typed repository attrs detached from extension metadata')
    return dict(file=evidence(path), own=own)


def capture(row, engine, scratch, workspace, ref, cache, raw):
    outputs, roots = controlled.capture_outputs(row, engine, scratch, workspace, ref, cache)
    if set(outputs) != {'data.txt', 'result.txt'} or set(roots) != {'data.txt'}:
        raise ValueError('missing repository/root-consumer outputs')
    for name, item in outputs.items():
        if not valid_evidence(item, raw, name == 'result.txt') or item['kind'] != 'file':
            raise ValueError('wrong exact output bytes/mode')
        if name == 'result.txt' and item['mode'] != 0o555:
            raise ValueError('consumer output must be sealed executable mode 0555')
    result = dict(outputs=outputs, roots=roots)
    if engine != 'reference':
        root = Path(roots['data.txt'])
        if root.parent != direct.trees_directory(cache) or not re.fullmatch('[0-9a-f]{64}', root.name):
            raise ValueError('wrong content-addressed repository identity')
        metadata = controlled.native_metadata(cache)
        for path, item in metadata.items():
            data = json.loads(base64.b64decode(item['bytes_base64']))
            if (data['format'] != 1 or data['profile'] != 'direct-file-template-v1'
                    or data['identity'] != Path(path).stem):
                raise ValueError('legacy manifest profile/schema changed')
        result['metadata'] = metadata
        result['publications'] = sorted(p.name for p in direct.trees_directory(cache).iterdir())
    return result


def check_cache(observations, name):
    roots = [o['roots'] for o in observations]
    if roots[0] != roots[1] or roots[0] == roots[2] or roots[2] != roots[3] or roots[0] != roots[4]:
        raise ValueError('cold/warm/change/unrelated/restore identity mismatch')
    if name == 'getenv-control' and roots[5] in roots[:5]:
        raise ValueError('absent environment observation did not rekey')


def native_reuses(name, index, engine='native'):
    """N61: the publication-reuse steps of the multi-state native phases —
    the specification pointer addresses a publication whose recorded world is
    unchanged (warm; an undeclared environment change; a source that stays
    changed; a restored declared value; the second build of the K-to-L
    migration pair)."""
    if engine == 'migration':
        return index == 3
    if name == 'cache':
        return index in (1, 3)
    if name == 'getenv-control':
        return index in (1, 3, 4)
    return False


def run_phase(args, runner, scratch, workspace, ref, item, probe, engine):
    name = item['name']
    value = fixture(name, probe)
    controlled.materialize(workspace, value)
    cache = scratch / 'cases' / name / probe / 'cache'
    phase = dict(name=name, probe=probe, engine=engine, cache=str(cache), observations=[], runs=[], status='FAIL')
    runner.report['phases'].append(phase)
    seen_bytes, seen_roots = set(), set()
    sequence = [(i, engine, state) for i, state in enumerate(states(name))]
    if engine == 'migration':
        sequence = [(i, actual, ('migration-'+str(i), {}, False)) for i, actual in enumerate(
            ('baseline', 'baseline', 'native', 'native'))]
        if any(b'ctx.os' in b for b in value['files'].values()):
            raise ValueError('migration callback must not read host metadata')
    for index, actual, (label, env, changed) in sequence:
        runner.values = env
        if name == 'cache':
            (workspace / 'input.tpl').write_bytes(b'@HOST@source-v2\n' if changed else value['files']['input.tpl'])
        inputs = snapshot(workspace / p for p in value['files'])
        raw = expected_bytes(value, env, changed)
        for command in (('build', 'cquery') if actual == 'reference' and not value['diagnostic'] else ('build',)):
            row = runner.run(name+'.'+probe+'.'+actual+'.'+label+'.'+command,
                command_argv(args, ref, actual, cache, scratch / 'registry', command), workspace)
            phase['runs'].append(row['id'])
            obs = dict(index=index, engine=actual, command=command, run=row['id'],
                       inputs_before=inputs, inputs_after=snapshot(workspace / p for p in value['files']))
            phase['observations'].append(obs)
            if inputs != obs['inputs_after']:
                raise ValueError('fixture source movement during command')
            if value['diagnostic']:
                require_rejection(row, value, actual)
                if actual == 'native':
                    direct.no_publication(cache)
                obs['no_publication'] = True
                continue
            controlled.success(row)
            if actual != 'reference':
                expected_markers = (['HOST_EXT_CALLBACK'] if value['ordinary'] else [])+['HOST_REPO_CALLBACK']
                # N61: a reuse step never runs the repository callback; the
                # extension callback still prints (lockfile off, so module
                # extensions re-evaluate every invocation).
                if actual == 'native' and native_reuses(name, index, engine):
                    expected_markers = [m for m in expected_markers if m == 'HOST_EXT_CALLBACK']
                if markers(row) != expected_markers:
                    raise ValueError('native callback missing or wrong phase')
            if name == 'metadata' and value['ordinary'] and actual == 'reference' and command == 'build':
                obs['lockfile'] = lockfile_factors(workspace, probe)
            if actual == 'reference' and command == 'build':
                continue
            obs.update(capture(row, 'native' if actual == 'baseline' else actual,
                               scratch, workspace, ref, cache, raw))
            if actual != 'reference':
                # Consumer input logical label and mode remain fixed. Repository
                # provenance/source-only changes are not action-cache inputs.
                common.action_counts(row, migration_counts(engine, index, raw in seen_bytes))
                seen_bytes.add(raw)
                seen_roots.add(Path(obs['roots']['data.txt']).name)
                if obs['publications'] != sorted(seen_roots):
                    raise ValueError('unplanned publication/staging residue')
                if {Path(p).stem for p in obs['metadata']} != seen_roots:
                    raise ValueError('missing old/new manifests')
    outputs = [o for o in phase['observations'] if 'outputs' in o]
    if engine == 'native' and name in ('cache', 'getenv-control'):
        check_cache(outputs, name)
    if engine == 'migration':
        roots = [o['roots'] for o in outputs]
        # The pinned K baseline (622afa65) predates N51/N52/N61, so the K-to-L
        # change rekeys every repository once (observed on the integration-18
        # pre-gate binary); N61's cross-version stability starts with N61 itself
        # (tests/repository_warm_reuse.rs). Each binary stays stable cold/warm.
        if roots[0] != roots[1] or roots[2] != roots[3] or roots[0] == roots[2]:
            raise ValueError('missing one-time K-to-L identity migration')
        inventories = []
        for obs in outputs:
            identity = Path(obs['roots']['data.txt']).name
            manifests = obs['metadata']
            entry = next(v for p, v in manifests.items() if Path(p).stem == identity)
            inventories.append(json.loads(base64.b64decode(entry['bytes_base64']))['files'])
        if any(v != inventories[0] for v in inventories):
            raise ValueError('migration changed constant output inventory')
    phase['status'] = 'PASS'
    return phase



def migration_counts(engine, index, seen):
    """Expected (executed, hits) for one build of a phase.

    Before N61 the K-to-L migration's first L build (index 2) rebuilt once:
    the native action cache moved to native-action-v8 keys and manifests
    (N41 x N42; v5 on main before N23d/N36, v7 in integration-13), and a
    manifest of another format, or another key, is a cache miss
    (docs/HARDLINK_STAGING.md, "pre-v8 entries rebuild once"). N61 pins
    repository identities across native versions and does not touch action
    keys, but the pinned K baseline (622afa65) predates native-action-v8, so
    the first L build still rebuilds once (observed on the integration-18
    pre-gate binary: K 1/0, 0/1; L 1/0); every other build keeps the plain
    seen-bytes rule.
    """
    if engine == 'migration' and index == 2:
        return (1, 0)
    return (0, 1) if seen else (1, 0)

def shutdown(ref, runner, report):
    pidfile = ref.output_base / 'server/server.pid.txt'
    pid = int(pidfile.read_text().strip()) if pidfile.exists() else None
    if pid is not None and pid <= 1:
        raise ValueError('invalid owned server pid')
    runner.values = {}
    row = runner.run('host-os.shutdown', ref.startup+['shutdown'], ref.workspace, min(runner.timeout, 15))
    state = controlled.pid_state(pid)
    forced = False
    if state == 'LIVE':
        # Verify only the known owned PID, not arbitrary system process lists.
        cmdline = Path('/proc', str(pid), 'cmdline').read_bytes().split(b'\0')
        if os.fsencode('--output_base='+str(ref.output_base)) not in cmdline:
            raise ValueError('refusing to kill unverified PID')
        os.kill(pid, signal.SIGKILL)
        forced = True
    report['server_cleanup'] = dict(owned_pid=pid, state=state, forced=forced, run=row['id'])
    controlled.success(row)
    if state == 'LIVE' or forced:
        raise ValueError('forced cleanup cannot pass')


def argument_parser():
    parser = common.argument_parser()
    parser.description = __doc__
    parser.set_defaults(native=ROOT / 'target/release/bazel-rs',
        reference=ROOT / '.direnv/reference/bazel-9.3.0-linux-x86_64',
        output=ROOT / 'artifacts/host-os-validation.json')
    parser.add_argument('--official-source', type=Path, default=Path('/tmp/opencode/bazel-9.3-reference'))
    parser.add_argument('--baseline-native', type=Path)
    parser.add_argument('--baseline-sha256')
    return parser


def report_passes(report):
    """Closed recorded-run gate; synthetic unit records never constitute execution."""
    try:
        reference_only = report['configuration']['reference_only']
        planned = [(c['name'], p, e) for c in CASEPLAN for p, e in phases_for(c, reference_only)]
        if (report['errors'] or report['planned_checks'] != CASEPLAN
                or report['required_native_unit_proofs'] != UNIT_GATES
                or report['limitations'] != LIMITATIONS
                or report['sources_before'] != report['sources_after']
                or report['registry_before'] != report['registry_after']
                or report['official_sources_before'] != OFFICIAL or report['official_sources_after'] != OFFICIAL
                or report['executables'] != report['executables_after']
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or [(p['name'], p['probe'], p['engine']) for p in report['phases']] != planned
                or any(p['status'] != 'PASS' for p in report['phases'])
                or not report['cleanup_receipt']['removed'] or report['server_cleanup']['forced']
                or report['server_cleanup']['state'] == 'LIVE'):
            return False
        if (set(report['sources_before']) != set(map(str, source_paths()))
                or report['native_unit_gate_status'] != 'SEPARATE_RECEIPTS_REQUIRED_NOT_CLI_PROVEN'):
            return False
        for path, pin in report['sources_before'].items():
            if not Path(path).is_relative_to(ROOT) or not re.fullmatch('[0-9a-f]{64}', pin['sha256']):
                return False
        binaries = report['executables']
        if set(binaries) != ({'reference'} if reference_only else {'reference', 'native', 'baseline'}):
            return False
        if not reference_only and (binaries['native']['sha256'] != report['configuration']['native_sha256']
                or binaries['baseline']['sha256'] != report['configuration']['baseline_sha256']
                or binaries['native']['sha256'] == binaries['baseline']['sha256']):
            return False
        runs = report['runs']
        used = [0]+[rid for phase in report['phases'] for rid in phase['runs']]+[len(runs)-1]
        if used != list(range(len(runs))) or runs[0]['stdout'].strip() != 'bazel 9.3.0':
            return False
        for i, row in enumerate(runs):
            controlled.healthy(dict(row, stderr='' if '.build.os.' in row['name'] else row['stderr']))
            if row['id'] != i or row['executable_sha256'] != next(
                    v['sha256'] for v in binaries.values() if v['path'] == row['argv'][0]):
                return False
            if i < len(runs)-1 and not row['precommand_pins_verified']:
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(row[field+'_base64'], validate=True)
                if (digest(raw) != row[field+'_sha256'] or digest(raw) != row[field+'_full_sha256']
                        or len(raw) != row[field+'_full_size_bytes'] or raw.decode('utf-8', errors='replace') != row[field]):
                    return False
        args = argument_parser().parse_args([])
        for key, value in report['configuration'].items():
            setattr(args, key, Path(value) if key in ('native', 'reference', 'output', 'scratch_parent',
                'official_source', 'baseline_native') and value is not None else value)
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        ref = DirectReference(args.reference, workspace, scratch, None, 'host-os')
        if (report['cleanup_receipt'] != dict(path=str(scratch), removed=True)
                or report['server_cleanup']['run'] != len(runs)-1):
            return False
        for row, name, command, timeout in ((runs[0], 'reference.version', ['version', '--gnu_format'], args.timeout),
                (runs[-1], 'host-os.shutdown', ['shutdown'], min(args.timeout, 15))):
            controlled.success(row)
            if (row['name'] != name or row['argv'] != ref.startup+command or row['cwd'] != str(workspace)
                    or row['timeout_seconds'] != timeout or row['controlled_environment'] !=
                    controlled.encoded_environment(controlled.controlled_environment(scratch, {}))):
                return False
        for phase in report['phases']:
            validate_phase(phase, report, args, scratch, workspace, ref)
        return True
    except (KeyError, ValueError, TypeError, StopIteration):
        return False


def validate_phase(phase, report, args, scratch, workspace, ref):
    """Recheck exact planned commands, inputs, outputs and identity transitions."""
    name, probe, engine = phase['name'], phase['probe'], phase['engine']
    value = fixture(name, probe)
    cache = scratch / 'cases' / name / probe / 'cache'
    if phase['cache'] != str(cache):
        raise ValueError('detached cache')
    sequence = [(i, engine, state) for i, state in enumerate(states(name))]
    if engine == 'migration':
        sequence = [(i, actual, ('migration-'+str(i), {}, False)) for i, actual in enumerate(
            ('baseline', 'baseline', 'native', 'native'))]
    steps = [(i, actual, state, command) for i, actual, state in sequence for command in
             (('build', 'cquery') if actual == 'reference' and not value['diagnostic'] else ('build',))]
    if len(phase['observations']) != len(steps) or len(phase['runs']) != len(steps):
        raise ValueError('incomplete/extra steps')
    seen_bytes, seen_roots, output_steps = set(), set(), []
    for rid, obs, (index, actual, (label, env, changed), command) in zip(phase['runs'], phase['observations'], steps):
        row = report['runs'][rid]
        if (obs['run'] != rid or obs['index'] != index or obs['engine'] != actual or obs['command'] != command
                or row['name'] != name+'.'+probe+'.'+actual+'.'+label+'.'+command
                or row['argv'] != command_argv(args, ref, actual, cache, scratch / 'registry', command)
                or row['cwd'] != str(workspace) or row['timeout_seconds'] != args.timeout
                or row['controlled_environment'] != controlled.encoded_environment(
                    controlled.controlled_environment(scratch, env))):
            raise ValueError('command/environment/phase provenance mismatch')
        planned = dict(value['files'])
        if name == 'cache' and changed:
            planned['input.tpl'] = b'@HOST@source-v2\n'
        if (obs['inputs_before'] != obs['inputs_after']
                or set(obs['inputs_before']) != {str(workspace / p) for p in planned}):
            raise ValueError('fixture source mismatch')
        for p, raw in planned.items():
            item = obs['inputs_before'][str(workspace / p)]
            if (not valid_evidence(item, raw, False) or item['kind'] != 'file'
                    or item['path'] != str(workspace / p) or item['mode'] != 0o644):
                raise ValueError('fixture differs from immutable plan')
        if value['diagnostic']:
            require_rejection(row, value, actual)
            if not obs['no_publication'] or any(k in obs for k in ('outputs', 'roots', 'metadata')):
                raise ValueError('rejection has output publication')
            continue
        controlled.success(row)
        callback_plan = (['HOST_EXT_CALLBACK'] if value['ordinary'] else [])+['HOST_REPO_CALLBACK']
        # N61: a reuse step never runs the repository callback (the extension
        # callback still prints with the lockfile off).
        if actual == 'native' and native_reuses(name, index, engine):
            callback_plan = [m for m in callback_plan if m == 'HOST_EXT_CALLBACK']
        if actual != 'reference' and markers(row) != callback_plan:
            raise ValueError('wrong native callback phase')
        if actual == 'reference' and (any(m not in callback_plan for m in markers(row))
                or len(markers(row)) != len(set(markers(row))) or (command == 'cquery' and markers(row))):
            raise ValueError('unexpected reference callback phase')
        if name == 'metadata' and value['ordinary'] and actual == 'reference' and command == 'build':
            lock = obs['lockfile']
            if not valid_evidence(lock['file']) or lock['file']['path'] != str(workspace / 'MODULE.bazel.lock'):
                raise ValueError('missing bound genuine lockfile')
            data = json.loads(base64.b64decode(lock['file']['bytes_base64']))
            own = {k: v for k, v in data['moduleExtensions'].items() if k.endswith('//:ext.bzl%ext')}
            if own != lock['own'] or len(own) != 1:
                raise ValueError('detached lockfile projection')
            factors = next(iter(own.values()))
            if (list(factors) != [FACTORS[probe]] or json.loads(
                    factors[FACTORS[probe]]['generatedRepoSpecs']['made']['attributes']['text']) != golden('module_ctx')):
                raise ValueError('wrong genuine factors/typed attributes')
        if actual == 'reference' and command == 'build':
            if any(k in obs for k in ('outputs', 'roots', 'metadata')):
                raise ValueError('reference build invented output list')
            continue
        raw = expected_bytes(value, env, changed)
        if set(obs['outputs']) != {'data.txt', 'result.txt'} or set(obs['roots']) != {'data.txt'}:
            raise ValueError('missing source/consumer')
        # Native parser only validates path shape here, not existing paths after cleanup.
        if actual != 'reference':
            listed = []
            for line in row['stdout'].splitlines():
                parts = line.split('\t')
                if len(parts) != 2 or parts[0] != '//:result':
                    raise ValueError('wrong native output label')
                listed.append(parts[1])
        else:
            listed = list(map(str, reference_output_paths(row['stdout'], workspace, ref.output_base)))
        if sorted(listed) != sorted(o['logical_path'] for o in obs['outputs'].values()):
            raise ValueError('outputs detached from CLI listing')
        for p, item in obs['outputs'].items():
            if (not valid_evidence(item, raw, p == 'result.txt') or item['kind'] != 'file'
                    or not Path(item['path']).is_relative_to(scratch)
                    or '..' in Path(item['logical_path']).parts
                    or (p == 'result.txt' and item['mode'] != 0o555)):
                raise ValueError('wrong output bytes/mode/path')
        root = Path(obs['roots']['data.txt'])
        if Path(obs['outputs']['data.txt']['path']).parent != root:
            raise ValueError('repository root detached from source')
        if actual != 'reference':
            if root.parent != direct.trees_directory(cache) or not re.fullmatch('[0-9a-f]{64}', root.name):
                raise ValueError('wrong repository identity')
            seen_roots.add(root.name)
            if (obs['publications'] != sorted(seen_roots) or set(obs['metadata']) !=
                    {str(cache / 'repositories/direct/manifests' / (identity+'.json')) for identity in seen_roots}):
                raise ValueError('unplanned/missing publication')
            for path, item in obs['metadata'].items():
                if not valid_evidence(item) or item['path'] != path:
                    raise ValueError('invalid manifest evidence')
                data = json.loads(base64.b64decode(item['bytes_base64']))
                if (data['format'] != 1
                        or data['profile'] != 'direct-file-template-v1' or data['identity'] != Path(path).stem):
                    raise ValueError('legacy manifest schema/profile changed')
                controlled.check_publication_schema(data, 'legacy manifest schema/profile changed')
                controlled.check_publication_privacy(data, 'legacy manifest schema/profile changed')
            common.action_counts(row, migration_counts(engine, index, raw in seen_bytes))
            seen_bytes.add(raw)
        elif any(k in obs for k in ('metadata', 'publications')):
            raise ValueError('reference cannot claim native identity')
        output_steps.append(obs)
    if engine == 'native' and name in ('cache', 'getenv-control'):
        check_cache(output_steps, name)
    if engine == 'migration':
        roots = [o['roots'] for o in output_steps]
        # The pinned K baseline predates N51/N52/N61: one-time K-to-L rekey
        # (see run_phase); each binary stays stable cold/warm.
        if roots[0] != roots[1] or roots[2] != roots[3] or roots[0] == roots[2]:
            raise ValueError('missing one-time K-to-L rekey')
        inventories = [json.loads(base64.b64decode(o['metadata'][str(cache / 'repositories/direct/manifests' /
            (Path(o['roots']['data.txt']).name+'.json'))]['bytes_base64']))['files'] for o in output_steps]
        if any(v != inventories[0] for v in inventories):
            raise ValueError('migration changed bytes/modes')


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ('native', 'reference', 'output', 'scratch_parent', 'official_source', 'baseline_native'):
        if getattr(args, field) is not None:
            setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.output in [args.native, args.reference, args.baseline_native, *source_paths()] or any(
            args.output.is_relative_to(ROOT / p) for p in ('src', 'scripts', 'tests', 'vendor')):
        raise ValueError('report cannot overwrite source/executable')
    report = dict(schema_version=1, status='FAIL', errors=[], runs=[], phases=[],
        planned_checks=CASEPLAN, required_native_unit_proofs=UNIT_GATES,
        native_unit_gate_status='SEPARATE_RECEIPTS_REQUIRED_NOT_CLI_PROVEN', limitations=LIMITATIONS,
        configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()})
    scratch = ref = runner = None
    try:
        report['sources_before'] = fingerprints(source_paths())
        report['executables'] = executables(args)
        scratch = Path(tempfile.mkdtemp(prefix='host-os-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        for name in ('workspace', 'home', 'tmp'):
            (scratch / name).mkdir()
        workspace = scratch / 'workspace'
        (workspace / 'MODULE.bazel').write_text('module(name="host_os_contract")\n')
        shutil.copytree(REGISTRY, scratch / 'registry')
        report['registry_before'] = fingerprints(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        report['official_sources_before'] = official_pins(args, scratch, verify_git=True)
        runner = RecordingRunner(report, args, scratch)
        ref = DirectReference(args.reference, workspace, scratch, runner, 'host-os')
        row = runner.run('reference.version', ref.startup+['version', '--gnu_format'], workspace)
        controlled.success(row)
        if row['stdout'].strip() != 'bazel 9.3.0':
            raise ValueError('wrong reference version')
        for item in CASEPLAN:
            for probe, engine in phases_for(item, args.reference_only):
                run_phase(args, runner, scratch, workspace, ref, item, probe, engine)
    except (Exception, KeyboardInterrupt) as exc:
        report['errors'].append(type(exc).__name__+': '+str(exc))
    finally:
        if ref is not None:
            try:
                shutdown(ref, runner, report)
            except Exception as exc:
                report['errors'].append('shutdown: '+str(exc))
        try:
            report['sources_after'] = fingerprints(source_paths())
            report['executables_after'] = executables(args)
            if scratch is not None:
                report['official_sources_after'] = official_pins(args, scratch, verify_git=True)
                report['registry_after'] = fingerprints(p for p in (scratch / 'registry').rglob('*') if p.is_file())
        except Exception as exc:
            report['errors'].append('final evidence: '+str(exc))
        if scratch is not None:
            shutil.rmtree(scratch)
            report['cleanup_receipt'] = dict(path=str(scratch), removed=not scratch.exists())
        if report_passes(report):
            report['status'] = 'REFERENCE_ONLY_PASS' if args.reference_only else 'SCOPED_CLI_PASS'
        write_report(args.output, report)
    print(report['status']+': '+str(args.output))
    return 0 if report['status'] != 'FAIL' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
