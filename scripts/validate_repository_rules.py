#!/usr/bin/env python3
"""Twenty-one scoped direct repository-rule checks, NOT full Bazel byte parity.

Explicit execution only: real native ELF and pinned genuine Bazel 9.3.0, private
registries/caches, real built-in closure. Unit tests never launch either CLI.
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
import tempfile

from validate import ROOT, Runner, reference_output_paths, require_success, write_report
from validate_direct_repositories import (DirectReference, REFERENCE_SHA256,
                                          BOUNDARY_INFRASTRUCTURE_ERROR, STALE_EXTENSION_ERROR,
                                          argument_parser as common_parser,
                                          validate_binaries)


SOURCES = [ROOT / p for p in (
    'scripts/validate_repository_rules.py', 'scripts/validate_direct_repositories.py',
    'scripts/validate.py', 'tests/test_validate_repository_rules.py',
    'tests/test_validate_direct_repositories.py', 'tests/test_validate.py')]
ROOT_MODULE = 'module(name = "contractroot", version = "1.0")\n'
DEP_MODULE = 'module(name = "contractdep", version = "1.0")\n'
PROXY = 'maker = use_repo_rule("//:defs.bzl", "maker")\n'
CALL = 'maker(name = "made")\n'
BINARY = b'A B\r\nUTF8:\xc3\xa9 raw:\xff\x00\r\n'
BINARY_RESULT = b'C C\r\nUTF8:\xc3\xa9 raw:\xff\x00\r\n'
OUTSIDE = b'outside sentinel: never modify\n'
FAIL_MARKER = 'RULE_ROLLBACK_SENTINEL'
LAZY_MARKER = 'UNUSED_CALLBACK_MUST_NOT_RUN'
ROOT_CANONICAL = '+maker+made'
DEP_CANONICAL = 'contractdep++maker+made'
# Plain `query` prints Label.getDisplayForm(main repo mapping) (N39): the visible
# apparent name for the root's repo, `@@canonical` for the invisible dependency
# repo. Genuine Bazel 9.3.0 prints exactly these lines for these two targets.
ROOT_DISPLAY = '@made//:data'
DEP_DISPLAY = '@@' + DEP_CANONICAL + '//:data'
MAX_STREAM = 1024 * 1024


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def quote(text):
    """Quote ASCII Starlark data; controls use octal, never JSON Unicode escapes."""
    if not text.isascii():
        raise ValueError('trusted generated Starlark strings must be ASCII')
    escapes = {'"': '\\"', '\\': '\\\\', '\n': '\\n', '\r': '\\r', '\t': '\\t'}
    return '"' + ''.join(escapes.get(char, '\\%03o' % ord(char) if ord(char) < 32 or ord(char) == 127 else char)
                         for char in text) + '"'


def rule(body, attrs='{}', prefix=''):
    return (prefix + 'def _impl(ctx):\n' + ''.join('    ' + line + '\n' for line in body.splitlines())
            + 'maker = repository_rule(implementation = _impl, attrs = ' + attrs + ')\n')


def exports(names):
    return 'exports_files(' + repr(list(names)) + ')\n'


def build_file(names):
    return 'ctx.file("BUILD.bazel", ' + quote(exports(names)) + ', executable = False)'


BASIC = rule(build_file(['data']) + '\nctx.file("data", "hello Ada\\n", executable = False)\nreturn None')
HELPER = ('load(":leaf.bzl", "decorate")\n'
          'def _private(ctx):\n'
          '    return decorate(ctx.attr.person)\n'
          'def write_data(ctx):\n'
          '    ctx.file("data", _private(ctx), executable = False)\n')
LEAF = 'PREFIX = ["hello "]\ndef decorate(value):\n    return PREFIX[0] + value + "\\n"\n'
ATTRS = ('{"person": attr.string(default = "default-person"), "count": attr.int(default = 7), '
         '"enabled": attr.bool(default = True), "words": attr.string_list(default = ["x", "y"]), '
         '"required": attr.string(mandatory = True), '
         '"_private": attr.string(default = "secret"), "optional": attr.label(), '
         '"source": attr.label(default = Label("//private:source")), '
         '"sources": attr.label_list(default = [Label("//private:source")])}')
FILE_DEFS = rule(
    'if ctx.attr.count != 7 or not ctx.attr.enabled or ctx.attr.words != ["x", "y"]:\n'
    '    fail("ATTR_DEFAULTS_WRONG")\n'
    'if ctx.attr.required != "supplied" or ctx.attr._private != "secret" or ctx.attr.optional != None:\n'
    '    fail("ATTR_VALUES_WRONG")\n'
    'if type(ctx.attr.source) != "Label" or type(ctx.attr.sources[0]) != "Label":\n'
    '    fail("LABEL_IS_NOT_LABEL")\n'
    + build_file(['data', 'nested/overwritten', 'empty', 'toggle', 'octal']) + '\n'
    'write_data(ctx)\nctx.file("empty")\n'
    'ctx.file("octal", "\\000", executable = False)\n'
    'ctx.file("nested/overwritten", "old")\n'
    'ctx.file("nested/overwritten", "new\\n", executable = False, legacy_utf8 = False)\n'
    'ctx.file("toggle", "off", executable = False)\nctx.file("toggle", "on\\n")\nreturn None',
    ATTRS, 'load(":helper.bzl", "write_data")\n')
TEMPLATE_DEFS = rule(
    'if type(ctx.attr._source) != "Label":\n    fail("LABEL_IS_NOT_LABEL")\n'
    + build_file(['data', 'nonexec', 'same']) + '\n'
    'ctx.template("data", ctx.attr._source, substitutions = {"A": "B", "B": "C", "": "NO"})\n'
    'ctx.template("nonexec", ctx.attr._source, substitutions = {"A": "B", "B": "C", "": "NO"}, executable = False, watch_template = "auto")\n'
    'ctx.file("same", "A B\\n", executable = False)\n'
    'ctx.template("same", "same", substitutions = {"A": "B", "B": "C"})',
    '{"_source": attr.label(default = Label("//private:source"))}')
OWNER_DEFS = rule(build_file(['data']) + '\n'
                  'ctx.file("data", ctx.name + "|" + ctx.attr.name + "|" + ctx.original_name + "\\n", executable = False)')
GENERATED_CHILD = rule('ctx.template("data", ctx.attr._source, substitutions = {"A": "B", "B": "C"}, executable = False)\n'
                       + build_file(['data']), '{"_source": attr.label(default = Label("//:seed"))}')
# repository_ctx.execute/which (StarlarkBaseExternalContext.java:1970-2108,
# 2323-2383): no shell is inserted, environment overrides apply, and the
# exec_result carries the exit code and both streams. Bazel accepts this.
EXECUTE_DEFS = rule(
    build_file(['data']) + '\n'
    'result = ctx.execute(["/bin/sh", "-c", ' + quote('printf "%s|%s" "$1" "$EXEC_VALUE"; printf err >&2; exit 3')
    + ', "sh", "one"], environment = {"EXEC_VALUE": "two"}, quiet = True)\n'
    'found = ctx.which("sh")\n'
    'ctx.file("data", "%d|%s|%s|%s|%s\\n" % (result.return_code, result.stdout, result.stderr, found != None, '
    'ctx.path("data").basename), executable = False)')
GENERATED_SEED = rule(build_file(['defs.bzl', 'seed']) + '\n'
                      'ctx.file("defs.bzl", ' + quote(GENERATED_CHILD) + ', executable = False)\n'
                      'ctx.file("seed", "A B\\n", executable = False)')


def case(name, kind='differential', probes=('result',)):
    return dict(name=name, kind=kind, probes=list(probes))


CASES = [
    case('file.transitive-attrs-overwrite'),
    case('template.binary-label-and-stage'),
    case('identity.root-and-dependency'),
    case('generated.definition-and-template'),
    case('lazy.unused-callback'),
    case('group.unused-missing-attribute'),
    case('group.unused-unknown-attribute'),
    case('execute.host-process'),
    # N26: Bazel-faithful behavior that used to be native boundaries.
    case('literal.latin1-octal', probes=('result', 'raw-source')),
    case('return.metadata-and-values', probes=('repo-metadata', 'attrs-metadata', 'dict', 'integer')),
    case('patch.label-strip-directory'),
    case('resolver.free-chr'),
    case('native.rollback', 'native_boundary'),
    case('native.path-containment', 'native_boundary', ('dotdot', 'absolute', 'source-symlink')),
    case('native.unsupported', 'native_boundary', ('facts_version_unsupported',)),
    # N38: raw non-ASCII .bzl literals are Latin-1 (literal.latin1-octal raw-source);
    # only MODULE.bazel raw non-ASCII and the \x/\u/\U escapes remain native rejections.
    case('native.ascii-profile', 'native_boundary', ('module-unicode', 'hex', 'u', 'U')),
    case('native.template-cycle', 'native_boundary'),
    case('native.refresh-and-repair', 'native_boundary', ('cold', 'helper-change', 'input-change', 'repair')),
    case('native.parallel-publication', 'native_boundary', ('parallel-1', 'parallel-2')),
    case('native.main-query-lazy', 'native_boundary'),
    case('native.initialization-budget', 'native_boundary'),
]
PLAN = [(c['name'], c['kind']) for c in CASES]
DIAGNOSTICS = {
    'missing': r'(?:mandatory|required|missing)[^\n]*\brequired\b|\brequired\b[^\n]*(?:mandatory|required|missing)',
    'unknown': r'(?:unknown|unexpected|no such|does not have)[^\n]*\bunknown_attr\b|\bunknown_attr\b[^\n]*(?:unknown|unexpected|not defined)',
    'rollback': r'(?:Error in fail:|\bfail(?:ed)?[: (]|error:)[^\n]*' + FAIL_MARKER,
    'path': r'(?:path|file)[^\n]*(?:relative|escap|outside|parent|absolute|unsafe|clean)|(?:unsafe|absolute)[^\n]*path',
    'symlink': r'(?:symlink|symbolic link)[^\n]*(?:unsupported|forbidden|not supported|unsafe|regular)|(?:unsupported|unsafe|not supported)[^\n]*(?:symlink|symbolic link)|must be a regular file, not a symlink',
    'return': r'(?:return|returned)[^\n]*(?:None|unsupported|not supported)|(?:only|must)[^\n]*None',
    'facts_version_unsupported': r'\bmodule_extension with nonzero facts_version is unsupported\b',
    'ascii': r'(?:ASCII|non.ASCII|Unicode|unicode|escape|chr)[^\n]*(?:unsupported|not supported|forbidden|invalid|only|reject)|(?:unsupported|invalid|non.ASCII)[^\n]*(?:string|literal|escape|chr)',
    # Resolver.java:941-953 / Module.java:250-256: Bazel has no chr builtin.
    'undefined_chr': r"\bname 'chr' is not defined\b",
    'cycle': r'(?:repository|template|materializ)[^\n]*(?:cycle|cyclic)|(?:cycle|cyclic)[^\n]*(?:repository|template|made|other)',
    'ticks': r'\bticks?\b[^\n]*(?:limit|budget|exceed)|(?:limit|budget|exceed)[^\n]*\bticks?\b',
}
UNRELATED = re.compile(r'panick?ed|\bpanic\b|internal error|FATAL:|crashed|segmentation fault|'
                       r'falling back|fallback to|timed out|timeout|could not download|download failed|'
                       r'connection refused|network (?:error|unavailable)|SSLHandshake|UnknownHost|'
                       r'parse error|unexpected token', re.I)


def success(record):
    text = require_success(record)
    if (record.get('spawn_error') or record.get('interrupted') or record.get('output_truncated')
            or UNRELATED.search(record['stderr'])):
        raise ValueError('interrupted, truncated, fallback or unrelated invocation')
    return text


def require_rejection(record, diagnostic):
    code = record['returncode']
    text = record['stderr']
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record['timed_out']
            or record.get('spawn_error') or record.get('interrupted') or record.get('output_truncated')
            or record['stdout'].strip() or UNRELATED.search(text)
            or (diagnostic == 'facts_version_unsupported'
                and (BOUNDARY_INFRASTRUCTURE_ERROR.search(text) or STALE_EXTENSION_ERROR.search(text)))
            or (diagnostic in ('missing', 'unknown') and LAZY_MARKER in text)
            or (diagnostic not in ('ascii', 'undefined_chr') and re.search(r'syntax error', text, re.I))
            or not re.search(DIAGNOSTICS[diagnostic], text, re.I)):
        raise ValueError('expected controlled repository-rule rejection: ' + diagnostic)


class RecordingRunner(Runner):
    def run(self, name, argv, cwd, timeout=None):
        start = len(self.report['runs'])
        try:
            return super().run(name, argv, cwd, timeout)
        finally:
            for record in self.report['runs'][start:]:
                record['environment'] = {key: self.env[key] for key in (
                    'PATH', 'HOME', 'TMPDIR', 'DO_NOT_TRACK', 'BAZEL_METRICS',
                    'NEXT_TELEMETRY_DISABLED') if key in self.env}
                record['environment_keys'] = sorted(self.env)
                for field in ('stdout', 'stderr'):
                    raw = (base64.b64decode(record[field + '_base64']) if field + '_base64' in record
                           else record[field].encode())
                    record[field + '_full_sha256'] = digest(raw)
                    record[field + '_full_size_bytes'] = len(raw)
                    if len(raw) > MAX_STREAM:
                        record['output_truncated'] = True
                        raw = raw[:MAX_STREAM]
                    record[field] = raw.decode('utf-8', errors='replace')
                    record[field + '_base64'] = base64.b64encode(raw).decode('ascii')
                    record[field + '_sha256'] = digest(raw)


def evidence(path):
    info = path.lstat()
    result = dict(path=str(path), mode=stat.S_IMODE(info.st_mode))
    if stat.S_ISLNK(info.st_mode):
        raw = os.fsencode(os.readlink(path))
        result['kind'] = 'symlink'
    elif stat.S_ISREG(info.st_mode):
        with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
            raw = stream.read()
        result['kind'] = 'file'
    else:
        raise ValueError('expected regular file or recorded link: ' + str(path))
    return dict(result, bytes_base64=base64.b64encode(raw).decode('ascii'),
                sha256=digest(raw), size_bytes=len(raw))


def valid_evidence(value, raw=None, executable=None):
    try:
        data = base64.b64decode(value['bytes_base64'], validate=True)
        return (value['kind'] in ('file', 'symlink') and type(value['mode']) is int
                and 0 <= value['mode'] <= 0o7777 and bool(value['path'])
                and value['sha256'] == digest(data) and value['size_bytes'] == len(data)
                and (raw is None or data == raw)
                and (executable is None or bool(value['mode'] & 0o111) == executable))
    except (KeyError, TypeError, ValueError):
        return False


def snapshot(paths):
    return {str(path): evidence(path) for path in sorted(paths)}


def fixture(name, probe='result', outside=None):
    """All commands act on these real rule definitions, never precreated outputs."""
    files = {'MODULE.bazel': ROOT_MODULE + PROXY + CALL, 'BUILD.bazel': '',
             'defs.bzl': BASIC, 'helper.bzl': HELPER, 'leaf.bzl': LEAF,
             'private/BUILD.bazel': '', 'private/source': BINARY}
    expected = {'data': (b'hello Ada\n', False)}
    diagnostic = None
    target = '@made//:data'
    if name == 'file.transitive-attrs-overwrite':
        files['MODULE.bazel'] = ROOT_MODULE + PROXY + 'maker(name = "made", required = "supplied", person = "Ada")\n'
        files['defs.bzl'] = FILE_DEFS
        expected.update({'nested/overwritten': (b'new\n', False), 'empty': (b'', True),
                         'toggle': (b'on\n', True), 'octal': (b'\x00', False)})
    elif name == 'template.binary-label-and-stage':
        files['defs.bzl'] = TEMPLATE_DEFS
        expected = {'data': (BINARY_RESULT, True), 'nonexec': (BINARY_RESULT, False), 'same': (b'C C\n', True)}
    elif name == 'identity.root-and-dependency':
        files['MODULE.bazel'] = ROOT_MODULE + 'bazel_dep(name = "contractdep", version = "1.0", repo_name = "dep")\n' + PROXY + CALL
        files['defs.bzl'] = OWNER_DEFS
        expected = {'root': ((ROOT_CANONICAL + '|' + ROOT_CANONICAL + '|made\n').encode(), False),
                    'dep': ((DEP_CANONICAL + '|' + DEP_CANONICAL + '|made\n').encode(), False)}
    elif name == 'generated.definition-and-template':
        files['defs.bzl'] = GENERATED_SEED
        files['MODULE.bazel'] = (ROOT_MODULE + PROXY + 'maker(name = "seed")\n'
                                 'child = use_repo_rule("@seed//:defs.bzl", "maker")\nchild(name = "made")\n')
        expected = {'data': (b'C C\n', False)}
    elif name == 'lazy.unused-callback':
        files['defs.bzl'] = rule('if ctx.attr.poison:\n    fail("' + LAZY_MARKER + '")\n'
                                 + build_file(['data']) + '\nctx.file("data", "hello Ada\\n", executable = False)',
                                 '{"poison": attr.bool(default = False)}')
        files['MODULE.bazel'] += 'maker(name = "unused", poison = True)\n'
    elif name.startswith('group.'):
        files['defs.bzl'] = rule('fail("' + LAZY_MARKER + '")', '{"required": attr.string(mandatory = True)}')
        files['MODULE.bazel'] = ROOT_MODULE + PROXY + 'maker(name = "made", required = "ok")\n'
        if name.endswith('missing-attribute'):
            files['MODULE.bazel'] += 'maker(name = "unused")\n'
            diagnostic = 'missing'
        else:
            files['MODULE.bazel'] += 'maker(name = "unused", required = "ok", unknown_attr = "bad")\n'
            diagnostic = 'unknown'
    elif name == 'native.rollback':
        files['defs.bzl'] = rule(build_file(['data']) + '\nctx.file("data", "partial")\nfail("' + FAIL_MARKER + '")')
        diagnostic = 'rollback'
    elif name == 'native.path-containment':
        if probe == 'source-symlink':
            files['defs.bzl'] = TEMPLATE_DEFS
            diagnostic = 'symlink'
        else:
            output = '../outside-marker' if probe == 'dotdot' else str(outside)
            files['defs.bzl'] = rule('ctx.file(' + quote(output) + ', "ESCAPED")\n' + build_file(['data']))
            diagnostic = 'path'
    elif name == 'native.unsupported':
        body = {'facts_version_unsupported': 'return None'}[probe]
        files['defs.bzl'] = rule(build_file(['data']) + '\nctx.file("data", "partial")\n' + body)
        files['defs.bzl'] += 'ordinary = module_extension(implementation = _impl, facts_version = 1)\n'
        diagnostic = probe
    elif name == 'literal.latin1-octal':
        # Lexer.java:372-395: an octal escape is the char NNN & 0xff, which
        # repository_ctx.file writes as that single byte.
        files['defs.bzl'] = rule(build_file(['data']) + '\nctx.file("data", value, executable = False)',
                                 prefix='load(":helper.bzl", "value")\n')
        files['helper.bzl'] = 'load(":leaf.bzl", _value = "value")\nvalue = _value\n'
        files['leaf.bzl'] = 'value = "\\377\\200A" + str(len("\\377")) + "\\n"\n'
        expected = {'data': (b'\xff\x80A1\n', False)}
        if probe == 'raw-source':
            # N38: StarlarkUtil.createParserInput -> ParserInput.fromLatin1 (well-
            # formed UTF-8 .bzl bytes are one Latin-1 char each), so len("é") == 2
            # and repository_ctx.file writes the internal bytes back unchanged
            # (StarlarkBaseExternalContext.java:1500-1520). Was native.ascii-profile
            # closure-unicode (a native rejection before N38).
            files['leaf.bzl'] = 'value = "é" + str(len("é")) + "\\n"\n'
            expected = {'data': (b'\xc3\xa92\n', False)}
    elif name == 'return.metadata-and-values':
        # RepositoryFetchFunction.java:681-695; StarlarkRepositoryContext.java:611-656.
        body = {'repo-metadata': 'return ctx.repo_metadata(reproducible = True)',
                'attrs-metadata': 'return ctx.repo_metadata(attrs_for_reproducibility = {"name": ctx.name})',
                'dict': 'return {"answer": "value"}', 'integer': 'return 42'}[probe]
        files['defs.bzl'] = rule(build_file(['data']) + '\nctx.file("data", "hello Ada\\n", executable = False)\n' + body)
    elif name == 'patch.label-strip-directory':
        # StarlarkRepositoryContext.java:475-567 with PatchUtil (a/b prefixes, -p1).
        files['private/fix.patch'] = ('--- a/data\t2024-05-14 13:52:59.000000000 +0000\n'
                                      '+++ b/data\t2026-06-04 19:22:29.815049510 +0000\n'
                                      '@@ -1 +1 @@\n-hello Ada\n+patched Ada\n')
        files['defs.bzl'] = rule(build_file(['data', 'sub/data']) + '\n'
                                 'ctx.file("data", "hello Ada\\n", executable = False)\n'
                                 'ctx.patch(Label("//private:fix.patch"), strip = 1)\n'
                                 'ctx.file("sub/data", "hello Ada\\n", executable = False)\n'
                                 'ctx.patch(Label("//private:fix.patch"), 1, directory = "sub", watch_patch = "no")')
        expected = {'data': (b'patched Ada\n', False), 'sub/data': (b'patched Ada\n', False)}
    elif name == 'resolver.free-chr':
        files['defs.bzl'] = rule(build_file(['data']) + '\nctx.file("data", value)', prefix='load(":helper.bzl", "value")\n')
        files['helper.bzl'] = 'load(":leaf.bzl", "value")\n'
        files['leaf.bzl'] = 'value = "ASCII output"\ndef unused():\n    return str(chr)\n'
        diagnostic = 'undefined_chr'
    elif name == 'execute.host-process':
        files['defs.bzl'] = EXECUTE_DEFS
        expected = {'data': (b'3|one|two|err|True|data\n', False)}
    elif name == 'native.ascii-profile':
        expression = {'module-unicode': 'str(len("é"))',
                      'hex': 'str(len("\\x41"))',
                      'u': 'str(len("\\u0041"))', 'U': 'str(len("\\U00000041"))'}[probe]
        if probe == 'module-unicode':
            files['MODULE.bazel'] += 'unused = ' + expression + '\n'
        else:
            files['defs.bzl'] = rule(build_file(['data']) + '\nctx.file("data", value)', prefix='load(":helper.bzl", "value")\n')
            files['helper.bzl'] = 'load(":leaf.bzl", "value")\n'
            files['leaf.bzl'] = 'value = ' + expression + '\n'
        diagnostic = 'ascii'
    elif name == 'native.template-cycle':
        files['defs.bzl'] = rule('ctx.template("data", ctx.attr.source)\n' + build_file(['data']), '{"source": attr.label(mandatory = True)}')
        files['MODULE.bazel'] = (ROOT_MODULE + PROXY + 'maker(name = "made", source = "@other//:data")\n'
                                 'maker(name = "other", source = "@made//:data")\n')
        diagnostic = 'cycle'
    elif name == 'native.refresh-and-repair':
        files['defs.bzl'] = rule(build_file(['data']) + '\nctx.template("data", ctx.attr._source, substitutions = {"A": replacement}, executable = False)',
                                 '{"_source": attr.label(default = Label("//private:source"))}',
                                 'load(":helper.bzl", "replacement")\n')
        files['helper.bzl'] = 'load(":leaf.bzl", _replacement = "replacement")\nreplacement = _replacement\n'
        files['leaf.bzl'] = 'replacement = "' + ('X' if probe == 'cold' else 'Y') + '"\n'
        files['private/source'] = b'A\n' if probe in ('cold', 'helper-change') else b'A A\r\n\xff'
        expected = {'data': ((b'X\n' if probe == 'cold' else b'Y\n' if probe == 'helper-change' else b'Y Y\r\n\xff'), False)}
    elif name == 'native.main-query-lazy':
        files['defs.bzl'] = 'fail("INVALID_UNUSED_DEFINITION_EVALUATED")\n'
        files['MODULE.bazel'] += 'unused = str(len("é"))\n'
        files['BUILD.bazel'] = 'exports_files(["data"])\n'
        files['data'] = b'main only\n'
        target = '//:data'
    elif name == 'native.initialization-budget':
        # A legal top-level comprehension must exhaust evaluator ticks during
        # definition loading, before _impl can run or any repository can publish.
        files['defs.bzl'] = 'INITIALIZATION = [i + 1 for i in range(1000000)]\n' + BASIC
        diagnostic = 'ticks'
    # Root consumers read real generated files, including a trusted copy action.
    if not diagnostic and name != 'native.main-query-lazy':
        labels = (['@made//:data', '@dep//:payload'] if name == 'identity.root-and-dependency'
                  else ['@made//:' + key for key in expected])
        files['BUILD.bazel'] = ('filegroup(name = "data", srcs = ' + repr(labels) + ')\n'
                                'genrule(name = "copy", srcs = ["@made//:data"], outs = ["copy.txt"], '
                                'cmd = "cat $(SRCS) > $@")\n')
    return dict(files={p: v.encode() if isinstance(v, str) else v for p, v in files.items()},
                expected=expected, diagnostic=diagnostic, target=target)


def materialize(case_root, workspace, name, probe, *, update=False):
    outside = case_root / 'outside-marker'
    if not update:
        if workspace.exists():
            shutil.rmtree(workspace)
        workspace.mkdir(parents=True)
        outside.parent.mkdir(parents=True, exist_ok=True)
        outside.write_bytes(OUTSIDE)
    value = fixture(name, probe, outside)
    owned = {workspace / p: raw for p, raw in value['files'].items()}
    registry = case_root / 'registry'
    owned[registry / 'bazel_registry.json'] = b'{}\n'
    if name == 'identity.root-and-dependency':
        source = case_root / 'dependency'
        entry = registry / 'modules/contractdep/1.0'
        owned.update({source / 'MODULE.bazel': (DEP_MODULE + PROXY + CALL).encode(),
                      source / 'BUILD.bazel': b'filegroup(name = "payload", srcs = ["@made//:data"], visibility = ["//visibility:public"])\n',
                      source / 'defs.bzl': OWNER_DEFS.encode(),
                      entry / 'MODULE.bazel': (DEP_MODULE + PROXY + CALL).encode(),
                      entry / 'source.json': json.dumps({'type': 'local_path', 'path': str(source)}).encode(),
                      entry.parent / 'metadata.json': b'{"versions":["1.0"],"yanked_versions":{}}\n'})
    for path, raw in owned.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink():
            path.unlink()
        path.write_bytes(raw)
    if name == 'native.path-containment' and probe == 'source-symlink':
        source = workspace / 'private/source'
        source.unlink()
        source.symlink_to(outside)
    value.update(paths=[*owned, outside], registry=registry, outside=outside)
    return value


def native_command(executable, cache, registry, command, target):
    return [str(executable), '--ignore_all_rc_files', '--cache-dir=' + str(cache), command,
            '--lockfile_mode=off', '--registry=' + registry.as_uri(), target]


def native_paths(record, target, scratch):
    paths = []
    for line in success(record).splitlines():
        parts = line.split('\t')
        if len(parts) != 2 or parts[0] != target:
            raise ValueError('expected exact root LABEL<TAB>PATH, not normalized aliases')
        path = Path(parts[1])
        if (not path.is_absolute() or '..' in path.parts or path in paths
                or not path.resolve().is_relative_to(scratch.resolve())):
            raise ValueError('unsafe/duplicate native artifact path')
        paths.append(path)
    if not paths:
        raise ValueError('missing native artifacts')
    return paths


def trees_directory(cache):
    return cache / 'repositories/direct/trees'


def publication_root(path, cache):
    """Trace the actual build output, never guess among several generated trees."""
    path = path.resolve(strict=True)
    trees = trees_directory(cache).resolve()
    for parent in path.parents:
        if parent.parent == trees and re.fullmatch('[0-9a-f]{64}', parent.name):
            return parent
    raise ValueError('native source output did not identify a direct published digest root')


def no_publication(cache):
    entries = list(trees_directory(cache).glob('*'))
    if entries:
        raise ValueError('rejection published a complete or staging tree: ' + str(entries))
    return []


def source_artifacts(paths, expected, *, cache=None, reference=None, owner=False):
    result, roots = {}, {}
    for path in paths:
        resolved = path.resolve(strict=True)
        if cache is not None:
            root = publication_root(path, cache)
        else:
            # Genuine cquery identity supplies the canonical name; no alias rewrite.
            logical = path.relative_to(reference.output_base / 'execroot/_main/external')
            root = (reference.output_base / 'external' / logical.parts[0]).resolve(strict=True)
            if not resolved.is_relative_to(root) or not root.is_relative_to(reference.output_base.parent):
                raise ValueError('reference artifact escaped private genuine repository cache')
        relative = resolved.relative_to(root).as_posix()
        value = evidence(resolved)
        if owner:
            raw = base64.b64decode(value['bytes_base64'])
            key = next((k for k, (data, _) in expected.items() if raw == data), None)
        else:
            key = relative
        if key not in expected or key in result or value['kind'] != 'file' or not valid_evidence(value, *expected[key]):
            raise ValueError('source artifact bytes/mode/identity mismatch: ' + str(path))
        result[key] = value
        roots[key] = str(root)
        if cache is not None:
            if (root / 'MODULE.bazel').exists() or (root / 'MODULE.bazel').is_symlink():
                raise ValueError('generated repository fabricated MODULE.bazel')
            repo = evidence(root / 'REPO.bazel')
            if repo['kind'] != 'file' or not valid_evidence(repo, b''):
                raise ValueError('generated repository missing empty REPO.bazel boundary')
            value['repo_boundary'] = repo
            value['module_absent'] = True
        value['logical_path'] = str(path)
    if set(result) != set(expected):
        raise ValueError('missing expected repository artifact')
    return dict(files=result, roots=roots)


def require_canonical(record, canonical):
    if success(record).splitlines() != ['@@' + canonical + '//:data']:
        raise ValueError('observed reference canonical identity differs from contract')


def require_display(record, key):
    """Native plain query: Bazel's display form, not a native-only @@ spelling.

    The canonical names themselves are proven by the published bytes
    (ctx.name/ctx.attr.name per repository) and the reference cquery.
    """
    if success(record).splitlines() != [ROOT_DISPLAY if key == 'root' else DEP_DISPLAY]:
        raise ValueError('native query label differs from Bazel display form')


def phases_for(item):
    engines = ('reference', 'native') if item['kind'] == 'differential' else ('native',)
    return [(probe, engine) for probe in item['probes'] for engine in engines]


def steps_for(item, probe, engine):
    value = fixture(item['name'], probe, Path('/placeholder/outside-marker'))
    if value['diagnostic'] or item['name'] == 'native.main-query-lazy':
        return [('query', value['target'], [])]
    result = [('build', '//:data', [])]
    if engine == 'reference':
        result.append(('cquery', '//:data', ['--output=files']))
    if item['name'] == 'identity.root-and-dependency':
        for target in ('@made//:data', '@@' + DEP_CANONICAL + '//:data'):
            result.append(('cquery', target, ['--output=starlark', '--starlark:expr=str(target.label)'])
                          if engine == 'reference' else ('query', target, []))
    result.append(('build', '//:copy', []))
    if engine == 'reference':
        result.append(('cquery', '//:copy', ['--output=files']))
    return result


def run_case(args, runner, scratch, workspace, reference, item):
    check = dict(name=item['name'], kind=item['kind'], input=item, status='FAIL', parity_claim=False, phases=[])
    runner.report['checks'].append(check)
    case_root = scratch / 'cases' / item['name']
    shared_cache = case_root / 'native-cache'
    previous = None

    def invoke(phase, engine, command, target, extra=()):
        label = item['name'] + '.' + phase['probe'] + '.' + engine + '.' + str(len(phase['runs']))
        result = (runner.run(label, native_command(args.native, phase_cache, value['registry'], command, target), workspace)
                  if engine == 'native' else reference.run(label, command, *extra, target))
        phase['runs'].append(dict(id=result['id'], command=command, target=target, extra=list(extra)))
        return result

    def build(phase, engine, record=None):
        result = record if record is not None else invoke(phase, engine, 'build', '//:data')
        if engine == 'native':
            paths = native_paths(result, '//:data', scratch)
        else:
            success(result)
            listing = invoke(phase, engine, 'cquery', '//:data', ('--output=files',))
            paths = reference_output_paths(success(listing), workspace, reference.output_base)
        phase['artifacts'] = source_artifacts(paths, value['expected'], cache=phase_cache if engine == 'native' else None,
                                              reference=reference, owner=item['name'] == 'identity.root-and-dependency')
        if item['name'] == 'identity.root-and-dependency':
            phase['canonical'] = {}
            for key, canonical in (('root', ROOT_CANONICAL), ('dep', DEP_CANONICAL)):
                target = '@made//:data' if key == 'root' else '@@' + canonical + '//:data'
                if engine == 'reference':
                    result = invoke(phase, engine, 'cquery', target, ('--output=starlark', '--starlark:expr=str(target.label)'))
                    require_canonical(result, canonical)
                    logical = Path(phase['artifacts']['files'][key]['logical_path'])
                    if logical.parts[-2:] != (canonical, 'data'):
                        raise ValueError('cquery file path disagrees with observed canonical label')
                else:
                    require_display(invoke(phase, engine, 'query', target), key)
                phase['canonical'][key] = '@@' + canonical + '//:data'
        result = invoke(phase, engine, 'build', '//:copy')
        if engine == 'native':
            outputs = native_paths(result, '//:copy', scratch)
        else:
            success(result)
            outputs = reference_output_paths(success(invoke(phase, engine, 'cquery', '//:copy', ('--output=files',))), workspace, reference.output_base)
        expected = value['expected']['root' if item['name'] == 'identity.root-and-dependency' else 'data'][0]
        if len(outputs) != 1 or outputs[0].name != 'copy.txt':
            raise ValueError('copy action returned wrong artifact')
        phase['copy'] = evidence(outputs[0])
        if phase['copy']['kind'] != 'file' or not valid_evidence(phase['copy'], expected):
            raise ValueError('copy action did not read actual generated bytes')

    try:
        for probe in item['probes']:
            refresh = item['name'] == 'native.refresh-and-repair'
            phase_cache = shared_cache if refresh or item['name'] == 'native.parallel-publication' else case_root / probe / 'native-cache'
            if item['name'] == 'native.parallel-publication' and probe == 'parallel-2':
                continue
            value = materialize(case_root / probe if not refresh else case_root, workspace, item['name'], probe,
                                update=refresh and probe != 'cold')
            reference.registry = value['registry']
            mutation = None
            if refresh and previous is not None:
                mutation = {'kind': probe, 'before': previous['inputs_after'], 'after': snapshot(value['paths'])}
                if probe == 'repair':
                    damaged = Path(previous['artifacts']['files']['data']['path'])
                    before = evidence(damaged)
                    damaged.chmod(0o644)
                    damaged.write_bytes(b'CORRUPT PUBLISHED TREE\n')
                    mutation['published'] = {'before': before, 'after': evidence(damaged)}
            for engine in ('reference', 'native') if item['kind'] == 'differential' else ('native',):
                phase = dict(probe=probe, engine=engine, runs=[], status='FAIL', cache=str(phase_cache),
                             registry=str(value['registry']), inputs_before=snapshot(value['paths']))
                check['phases'].append(phase)
                if mutation:
                    phase['intentional_mutation'] = mutation
                try:
                    if value['diagnostic']:
                        require_rejection(invoke(phase, engine, 'query', value['target']), value['diagnostic'])
                        phase['diagnostic'] = value['diagnostic']
                        if engine == 'native':
                            phase['publication_entries'] = no_publication(phase_cache)
                    elif item['name'] == 'native.main-query-lazy':
                        if success(invoke(phase, engine, 'query', '//:data')).splitlines() != ['//:data']:
                            raise ValueError('main-only query did not return normal target')
                        phase['publication_entries'] = no_publication(phase_cache)
                    elif item['name'] == 'native.parallel-publication':
                        # Independent Runner records avoid ID races; launch both real CLIs
                        # before awaiting either, then merge in deterministic probe order.
                        locals_ = [RecordingRunner({'runs': []}, args.timeout) for _ in range(2)]
                        for local in locals_:
                            local.env = dict(runner.env)
                        with ThreadPoolExecutor(max_workers=2) as pool:
                            futures = [pool.submit(local.run, item['name'] + '.parallel-' + str(i + 1) + '.native.0',
                                                   native_command(args.native, phase_cache, value['registry'], 'build', '//:data'), workspace)
                                       for i, local in enumerate(locals_)]
                            records = [future.result() for future in futures]
                        for i, record in enumerate(records):
                            if i:
                                phase = dict(probe='parallel-2', engine='native', runs=[], status='FAIL', cache=str(phase_cache),
                                             registry=str(value['registry']), inputs_before=snapshot(value['paths']))
                                check['phases'].append(phase)
                            record['id'] = len(runner.report['runs'])
                            runner.report['runs'].append(record)
                            phase['runs'].append(dict(id=record['id'], command='build', target='//:data', extra=[]))
                            build(phase, engine, record)
                            phase['inputs_after'] = snapshot(value['paths'])
                            phase['status'] = 'PASS'
                    else:
                        build(phase, engine)
                    phase['status'] = 'PASS'
                except (OSError, ValueError):
                    if engine == 'reference':
                        check['status'] = 'REFERENCE_FAILURE'
                    raise
                finally:
                    phase['inputs_after'] = snapshot(value['paths'])
                    if phase['inputs_before'] != phase['inputs_after']:
                        raise ValueError('CLI changed frozen fixture bytes/modes/link identity')
                    if not valid_evidence(evidence(value['outside']), OUTSIDE):
                        raise ValueError('outside sentinel changed')
                previous = phase
        validate_check(check, runner.report, args, workspace, reference)
        check.update(status='PASS', parity_claim=item['kind'] == 'differential')
    except (OSError, ValueError) as exc:
        check['error'] = str(exc)


def validate_check(check, report, args, workspace, reference):
    """Re-evaluate stored evidence; case counts alone can never produce PASS."""
    item = next(c for c in CASES if c['name'] == check['name'])
    if (check['input'] != item or check['kind'] != item['kind']
            or [(p['probe'], p['engine']) for p in check['phases']] != phases_for(item)):
        raise ValueError('missing, duplicate, unexpected or reordered phases')
    for phase in check['phases']:
        value = fixture(item['name'], phase['probe'], Path(phase['registry']).parent / 'outside-marker')
        if (phase['status'] != 'PASS' or not phase['inputs_before'] or phase['inputs_before'] != phase['inputs_after']
                or not all(valid_evidence(v) and k == v['path'] for k, v in phase['inputs_before'].items())):
            raise ValueError('invalid or changed input evidence')
        for relative, raw in value['files'].items():
            entry = phase['inputs_before'][str(workspace / relative)]
            source_link = (item['name'] == 'native.path-containment' and phase['probe'] == 'source-symlink'
                           and relative == 'private/source')
            expected = os.fsencode(Path(phase['registry']).parent / 'outside-marker') if source_link else raw
            if entry['kind'] != ('symlink' if source_link else 'file') or not valid_evidence(entry, expected):
                raise ValueError('input snapshot is not the planned real rule fixture')
        if [(r['command'], r['target'], r['extra']) for r in phase['runs']] != steps_for(item, phase['probe'], phase['engine']):
            raise ValueError('missing, duplicate or unexpected commands')
        records = []
        for index, link in enumerate(phase['runs']):
            record = report['runs'][link['id']]
            expected_name = item['name'] + '.' + phase['probe'] + '.' + phase['engine'] + '.' + str(index)
            if record['name'] != expected_name or record['cwd'] != str(workspace):
                raise ValueError('run provenance mismatch')
            if phase['engine'] == 'native':
                expected = native_command(args.native, Path(phase['cache']), Path(phase['registry']), link['command'], link['target'])
            else:
                from validate import COMMON_FLAGS, BUILD_FLAGS
                expected = (reference.startup + [link['command']] + list(COMMON_FLAGS)
                            + (list(BUILD_FLAGS) if link['command'] in ('build', 'cquery') else [])
                            + ['--repository_cache=' + str(reference.output_base.parent / 'repository-cache'),
                               '--repo_contents_cache=' + str(reference.output_base.parent / 'repository-contents'),
                               '--registry=' + Path(phase['registry']).as_uri(), '--registry=https://bcr.bazel.build']
                            + link['extra'] + [link['target']])
            if record['argv'] != expected:
                raise ValueError('command provenance mismatch')
            records.append(record)
        if value['diagnostic']:
            if len(records) != 1 or phase['diagnostic'] != value['diagnostic']:
                raise ValueError('missing controlled rejection')
            require_rejection(records[0], value['diagnostic'])
            if phase['engine'] == 'native' and phase['publication_entries'] != []:
                raise ValueError('rejection publication evidence missing')
        elif item['name'] == 'native.main-query-lazy':
            if len(records) != 1 or success(records[0]).splitlines() != ['//:data'] or phase['publication_entries'] != []:
                raise ValueError('invalid main-only laziness evidence')
        else:
            for record in records:
                success(record)
            files = phase['artifacts']['files']
            if set(files) != set(value['expected']) or set(phase['artifacts']['roots']) != set(files):
                raise ValueError('missing actual artifact evidence')
            for key, (raw, executable) in value['expected'].items():
                entry = files[key]
                if entry['kind'] != 'file' or not valid_evidence(entry, raw, executable):
                    raise ValueError('artifact bytes or executable mode mismatch')
                root = Path(phase['artifacts']['roots'][key])
                if not Path(entry['path']).is_relative_to(root):
                    raise ValueError('artifact root mismatch')
                if phase['engine'] == 'native':
                    if (root.parent != trees_directory(Path(phase['cache'])) or not re.fullmatch('[0-9a-f]{64}', root.name)
                            or entry['module_absent'] is not True or not valid_evidence(entry['repo_boundary'], b'')):
                        raise ValueError('missing native publication boundary evidence')
            source_run = next(r for r, link in zip(records, phase['runs']) if link['command'] == 'build' and link['target'] == '//:data')
            if phase['engine'] == 'native':
                paths = native_paths(source_run, '//:data', Path(report['scratch_directory']))
            else:
                listing = next(r for r, link in zip(records, phase['runs']) if link['command'] == 'cquery' and link['target'] == '//:data')
                paths = reference_output_paths(success(listing), workspace, reference.output_base)
            if sorted(map(str, paths)) != sorted(v['logical_path'] for v in files.values()):
                raise ValueError('artifact evidence not linked to build output')
            copy = phase['copy']
            raw = value['expected']['root' if item['name'] == 'identity.root-and-dependency' else 'data'][0]
            if copy['kind'] != 'file' or not valid_evidence(copy, raw):
                raise ValueError('missing copy bytes')
            copy_run = next(r for r, link in zip(records, phase['runs']) if link['command'] == ('build' if phase['engine'] == 'native' else 'cquery') and link['target'] == '//:copy')
            copy_paths = (native_paths(copy_run, '//:copy', Path(report['scratch_directory'])) if phase['engine'] == 'native'
                          else reference_output_paths(success(copy_run), workspace, reference.output_base))
            if list(map(str, copy_paths)) != [copy['path']]:
                raise ValueError('copy evidence not linked to action output')
            if item['name'] == 'identity.root-and-dependency':
                if phase['canonical'] != {'root': '@@' + ROOT_CANONICAL + '//:data', 'dep': '@@' + DEP_CANONICAL + '//:data'}:
                    raise ValueError('canonical identity mismatch')
                for key, canonical in (('root', ROOT_CANONICAL), ('dep', DEP_CANONICAL)):
                    observed = next(r for r, link in zip(records, phase['runs']) if link['target'] == ('@made//:data' if key == 'root' else '@@' + canonical + '//:data'))
                    if phase['engine'] == 'native':
                        require_display(observed, key)
                    else:
                        require_canonical(observed, canonical)
    if item['name'] == 'native.refresh-and-repair':
        phases = check['phases']
        for before, after in zip(phases, phases[1:]):
            mutation = after['intentional_mutation']
            if mutation['before'] != before['inputs_after'] or mutation['after'] != after['inputs_before']:
                raise ValueError('unrecorded intentional input mutation')
            old = before['artifacts']['roots']['data']
            new = after['artifacts']['roots']['data']
            if (after['probe'] == 'repair') != (old == new):
                raise ValueError('refresh/repair publication identity mismatch')
        if not valid_evidence(phases[-1]['intentional_mutation']['published']['after'], b'CORRUPT PUBLISHED TREE\n'):
            raise ValueError('missing deliberate corruption evidence')
    if item['name'] == 'native.parallel-publication':
        if check['phases'][0]['artifacts']['roots'] != check['phases'][1]['artifacts']['roots']:
            raise ValueError('parallel publication selected different trees')


def report_passes(report):
    try:
        if ([(c['name'], c['kind']) for c in report['checks']] != PLAN or report['planned_checks'] != CASES
                or report['case_inputs_sha256'] != digest(json.dumps(CASES, sort_keys=True).encode())
                or report['errors'] or report['cleanup'] or report['scratch_cleaned'] is not True
                or report['reference_shutdown_succeeded'] is not True or report['sources_unchanged'] is not True
                or report['sources_before'] != report['sources_after'] or set(report['sources_before']) != set(map(str, SOURCES))
                or not all(valid_evidence(v) for v in report['sources_before'].values())
                or report['reference_release'] != 'bazel 9.3.0'
                or report['executables']['reference']['sha256'] != REFERENCE_SHA256
                or report['completed_checks'] != 21 or report['completed_differential_cases'] != 12):
            return False
        args = argument_parser().parse_args([])
        args.native = Path(report['configuration']['native'])
        args.reference = Path(report['configuration']['reference'])
        scratch = Path(report['scratch_directory'])
        workspace = scratch / 'workspace'
        reference = DirectReference(args.reference, workspace, scratch, None, 'repository-rules')
        for engine in ('native', 'reference'):
            executable = report['executables'][engine]
            if (executable['path'] != report['configuration'][engine]
                    or not re.fullmatch('[0-9a-f]{64}', executable['sha256'])
                    or type(executable['size_bytes']) is not int or executable['size_bytes'] <= 0):
                return False
        runs = report['runs']
        if [r['id'] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            if not record['environment_keys'] or not record['environment']:
                return False
            for field in ('stdout', 'stderr'):
                raw = base64.b64decode(record[field + '_base64'], validate=True)
                if (len(raw) > MAX_STREAM or digest(raw) != record[field + '_sha256']
                        or raw.decode('utf-8', errors='replace') != record[field]
                        or record[field + '_full_sha256'] != digest(raw)
                        or record[field + '_full_size_bytes'] != len(raw)):
                    return False
        from validate import COMMON_FLAGS
        if (runs[0]['argv'] != reference.startup + ['version', '--gnu_format']
                or runs[0]['name'] != 'reference.version' or runs[0]['cwd'] != str(workspace)
                or success(runs[0]).strip() != 'bazel 9.3.0' or runs[-1]['name'] != 'repository-rules.shutdown'
                or runs[-1]['argv'] != reference.startup + ['shutdown'] + list(COMMON_FLAGS)
                or runs[-1]['cwd'] != str(workspace)
                or runs[-1]['timeout_seconds'] != min(report['configuration']['timeout'], 15)):
            return False
        success(runs[-1])
        used = [0, len(runs) - 1]
        for check in report['checks']:
            if check['status'] != 'PASS' or check.get('error') or check['parity_claim'] != (check['kind'] == 'differential'):
                return False
            validate_check(check, report, args, workspace, reference)
            used += [r['id'] for p in check['phases'] for r in p['runs']]
        return sorted(used) == list(range(len(runs)))
    except (KeyError, TypeError, ValueError, IndexError, AttributeError, OSError, StopIteration):
        return False


def argument_parser():
    parser = common_parser()
    parser.description = __doc__
    parser.set_defaults(output=ROOT / 'artifacts/repository-rules-validation.json')
    return parser


def finish(reference, scratch, report):
    if reference is not None:
        try:
            report['reference_shutdown_attempted'] = True
            reference.shutdown()
            shutdown = report['runs'][-1]
            if shutdown['name'] != 'repository-rules.shutdown':
                raise ValueError('shutdown did not record a real command')
            success(shutdown)
            report['reference_shutdown_succeeded'] = not report['cleanup']
        except Exception as exc:
            report['errors'].append('reference shutdown: ' + str(exc))
    if scratch is not None:
        try:
            for marker in (scratch / 'cases').rglob('outside-marker'):
                if not valid_evidence(evidence(marker), OUTSIDE):
                    report['errors'].append('outside sentinel changed before cleanup: ' + str(marker))
            shutil.rmtree(scratch)
            report['scratch_cleaned'] = not scratch.exists()
            report['cleanup_receipt'] = {'path': str(scratch), 'removed': report['scratch_cleaned']}
        except (OSError, ValueError) as exc:
            report['errors'].append('scratch cleanup: ' + str(exc))


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for key in ('native', 'reference', 'output', 'scratch_parent'):
        setattr(args, key, getattr(args, key).expanduser().resolve())
    if (args.output in [args.native, args.reference, *SOURCES]
            or any(args.output.is_relative_to(ROOT / p) for p in ('scripts', 'tests'))):
        raise ValueError('output must not overwrite sources, fixtures or executables')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = dict(schema_version=1, status='FAIL', runs=[], checks=[], errors=[], cleanup=[],
                  scratch_cleaned=False, reference_shutdown_attempted=False, reference_shutdown_succeeded=False,
                  sources_unchanged=False, planned_checks=CASES,
                  case_inputs_sha256=digest(json.dumps(CASES, sort_keys=True).encode()),
                  configuration={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                  started_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  limitations=['21 cases: twelve differential, nine native policy/lifecycle boundaries. No full Bazel parity.',
                               'Raw non-ASCII MODULE.bazel literals are a native boundary; .bzl sources and octal escapes denote Latin-1 chars written as single bytes; raw templates preserve arbitrary bytes.',
                               'Callback returns follow RepositoryFetchFunction (dict keys checked, repo_metadata/other values accepted); reproducibility has no native caching effect.',
                               'Nonzero module_extension facts_version is unsupported at definition load; this native-only boundary is not differential parity or a callback execution claim.',
                               'Genuine reference built-ins may download from BCR; no fake platforms or bazel_tools.',
                               'Bytes and executable bits compared; read-only cache permission differences are recorded.',
                               'Elapsed subprocess durations are diagnostics, not performance measurements.',
                               'Environment values are recorded only for a safe allowlist; other inherited names only.',
                               'Output streams capped at 1 MiB; truncation fails verification. No Bazel test/run commands.'])
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report['sources_before'] = snapshot(SOURCES)
        report['executables'] = validate_binaries(args)
        scratch = Path(tempfile.mkdtemp(prefix='repository-rules-', dir=args.scratch_parent))
        report['scratch_directory'] = str(scratch)
        workspace = scratch / 'workspace'
        workspace.mkdir()
        (workspace / 'MODULE.bazel').write_text(ROOT_MODULE)
        reference = DirectReference(args.reference, workspace, scratch, runner, 'repository-rules')
        reference.started = True
        report['reference_release'] = success(runner.run('reference.version', reference.startup + ['version', '--gnu_format'], workspace)).strip()
        if report['reference_release'] != 'bazel 9.3.0':
            raise ValueError('reference is not Bazel 9.3.0')
        for item in CASES:
            run_case(args, runner, scratch, workspace, reference, item)
    except KeyboardInterrupt:
        interrupted = True
        report['errors'].append('interrupted')
    except Exception as exc:
        report['errors'].append(type(exc).__name__ + ': ' + str(exc))
    finally:
        finish(reference, scratch, report)
        try:
            report['sources_after'] = snapshot(SOURCES)
            report['sources_unchanged'] = report.get('sources_before') == report['sources_after']
        except OSError as exc:
            report['errors'].append('source evidence: ' + str(exc))
        report['completed_checks'] = sum(c['status'] == 'PASS' for c in report['checks'])
        report['completed_differential_cases'] = sum(c['status'] == 'PASS' and c['kind'] == 'differential' for c in report['checks'])
        if report_passes(report):
            report['status'] = 'PASS'
        report['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print('{} (scoped repository-rule execution only): {}'.format(report['status'], args.output))
    return 130 if interrupted else 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
