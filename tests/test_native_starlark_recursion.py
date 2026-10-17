"""N48: native-owned Starlark must not recurse.

Bazel 9.3 rejects recursive calls (`function 'NAME' called recursively`,
net/starlark/java/eval/StarlarkFunction.java:571-574) and bazel-rs now does too
(docs/STARLARK_VENDOR.md, "N48 recursive calls"). This static scan keeps the
Starlark that bazel-rs ships or tests with free of functions that can call
themselves: the embedded `@bazel_tools` archive, `resources/`, `tests/fixtures`
(files and fixture archives) and Starlark embedded in Rust string literals of
`src/` and `tests/`. Each source is parsed as Python (Starlark's syntax is a
subset; unparsable sources, e.g. Rust format templates, are counted and
skipped), and a per-file static call graph is built: an edge goes from a `def`
to every function of the same file it references by a free name (a call or
passing it as a callback; `NAME = lambda ...` counts as a function). `load()` cycles are impossible in Starlark, so
cross-file cycles cannot be static. Any cycle fails the test unless allowlisted
below with a reason; stale allowlist entries fail too.
"""

import ast
import re
import tarfile
import textwrap
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

# (source prefix, function) -> reason. A source is a path relative to the
# repository root, `ARCHIVE!MEMBER` for archive members, or
# `FILE.rs#LINE` for a Rust string literal starting on LINE.
ALLOWLIST = {
    ('tests/no_recursion.rs', '*'): 'intentional recursion probes of the N48 CLI tests',
    ('resources/bazel_tools/embedded_tools.tar.gz!tools/build_defs/cc/cc_import.bzl', 'cc_import'):
        "upstream Bazel 9.3 @bazel_tools file: a def named like the loaded rules_cc symbol it forwards to",
    ('tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz!lib/selects.bzl', '_config_setting_or_group'):
        'third-party bazel-skylib 1.8.2 (fixture): static mutual reference only, it never recurses at run time',
    ('tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz!lib/selects.bzl', '_config_setting_always_true'):
        'third-party bazel-skylib 1.8.2 (fixture): static mutual reference only, it never recurses at run time',
    ('tests/fixtures/build_settings/skylib/lib/selects.bzl', '_config_setting_or_group'):
        'third-party bazel-skylib (fixture): static mutual reference only, it never recurses at run time',
    ('tests/fixtures/build_settings/skylib/lib/selects.bzl', '_config_setting_always_true'):
        'third-party bazel-skylib (fixture): static mutual reference only, it never recurses at run time',
    ('tests/starlark_sets.rs', 'mutation'):
        'Rust format! template: `{mutation}` is a placeholder, not a call',
}

STARLARK_NAMES = ('BUILD', 'BUILD.bazel', 'BUILD.tools', 'WORKSPACE', 'MODULE.bazel', 'REPO.bazel')


def is_starlark(name):
    base = name.rsplit('/', 1)[-1]
    return base.endswith(('.bzl', '.star', '.sky')) or base in STARLARK_NAMES


def archive_sources(path, prefix, out):
    with tarfile.open(path) as archive:
        for member in archive.getmembers():
            if member.isfile() and is_starlark(member.name):
                name = member.name[2:] if member.name.startswith('./') else member.name
                out[f'{prefix}!{name}'] = archive.extractfile(member).read().decode('utf-8', 'replace')


RAW = re.compile(r'r(#*)"(.*?)"\1', re.S)
PLAIN = re.compile(r'(?<![\w#])"((?:[^"\\]|\\.)*)"', re.S)
DEF = re.compile(r'(^|\n|\\n)\s*def \w+\(')


def rust_sources(path, rel, out):
    text = path.read_text()
    for regex, raw in ((RAW, True), (PLAIN, False)):
        for match in regex.finditer(text):
            body = match.group(2) if raw else match.group(1)
            if not DEF.search(body):
                continue
            if not raw:
                try:
                    body = body.encode('utf-8').decode('unicode_escape')
                except UnicodeDecodeError:
                    continue
            line = text.count('\n', 0, match.start()) + 1
            out[f'{rel}#{line}'] = body


def collect():
    out = {}
    archive_sources(ROOT / 'resources/bazel_tools/embedded_tools.tar.gz',
                    'resources/bazel_tools/embedded_tools.tar.gz', out)
    for path in sorted((ROOT / 'resources').rglob('*')):
        if path.is_file() and is_starlark(path.name):
            out[path.relative_to(ROOT).as_posix()] = path.read_text()
    for path in sorted((ROOT / 'tests/fixtures').rglob('*')):
        rel = path.relative_to(ROOT).as_posix()
        if not path.is_file() or path.is_symlink():
            continue
        if is_starlark(path.name):
            out[rel] = path.read_text(errors='replace')
        elif path.name.endswith(('.tar.gz', '.tgz')):
            archive_sources(path, rel, out)
    for directory in ('src', 'tests'):
        for path in sorted((ROOT / directory).rglob('*.rs')):
            rust_sources(path, path.relative_to(ROOT).as_posix(), out)
    return out


def local_names(function):
    args = function.args
    names = {a.arg for a in args.posonlyargs + args.args + args.kwonlyargs}
    names |= {a.arg for a in (args.vararg, args.kwarg) if a}
    for node in ast.walk(function):
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store):
            names.add(node.id)
        elif isinstance(node, ast.Lambda):
            names |= {a.arg for a in node.args.args}
    return names


class CallGraph(ast.NodeVisitor):
    def __init__(self):
        self.stack = []
        self.refs = {}

    def visit_FunctionDef(self, node):
        self.refs.setdefault(node.name, set())
        # The def's name is bound in its enclosing scope; inside its own body
        # a reference to it is a (recursive) call unless a parameter or local
        # variable shadows it.
        self.stack.append((node.name, local_names(node)))
        self.generic_visit(node)
        self.stack.pop()

    def visit_Assign(self, node):
        # `f = lambda ...: ...` names a function too.
        if len(node.targets) == 1 and isinstance(node.targets[0], ast.Name) and isinstance(node.value, ast.Lambda):
            name = node.targets[0].id
            self.refs.setdefault(name, set())
            self.stack.append((name, {a.arg for a in node.value.args.args}))
            self.visit(node.value)
            self.stack.pop()
        else:
            self.generic_visit(node)

    def visit_Name(self, node):
        if isinstance(node.ctx, ast.Load):
            for function, locals_ in self.stack:
                if node.id not in locals_:
                    self.refs[function].add(node.id)


def parse(text):
    for candidate in (text, textwrap.dedent(text)):
        try:
            return ast.parse(candidate)
        except (SyntaxError, ValueError):
            continue
    return None


def cycles(tree):
    graph = CallGraph()
    graph.visit(tree)
    edges = {f: {r for r in refs if r in graph.refs} for f, refs in graph.refs.items()}
    found = []
    for start in sorted(edges):
        seen, todo = set(), list(edges[start])
        while todo:
            node = todo.pop()
            if node == start:
                found.append(start)
                break
            if node not in seen:
                seen.add(node)
                todo.extend(edges.get(node, ()))
    return found


def allowed(source, function):
    for (prefix, name), _reason in ALLOWLIST.items():
        if (source == prefix or source.startswith(prefix + '#')) and name in ('*', function):
            return (prefix, name)
    return None


class NativeStarlarkRecursionTest(unittest.TestCase):
    def test_native_owned_starlark_has_no_recursive_functions(self):
        sources = collect()
        parsed, unparsable, violations, used = 0, 0, [], set()
        for source, text in sorted(sources.items()):
            tree = parse(text)
            if tree is None:
                unparsable += 1
                continue
            parsed += 1
            for function in cycles(tree):
                key = allowed(source, function)
                if key:
                    used.add(key)
                else:
                    violations.append(f'{source}: {function}')
        # The scan must actually cover the shipped and test Starlark.
        self.assertGreater(sum(1 for s in sources if s.startswith('resources/bazel_tools/embedded_tools.tar.gz!')), 100)
        self.assertGreater(sum(1 for s in sources if '.rs#' in s), 400)
        self.assertGreater(parsed, 1000)
        self.assertLess(unparsable, parsed // 10, 'too many sources could not be parsed')
        self.assertEqual(violations, [], 'functions that can call themselves (direct or mutual) in native-owned Starlark')
        self.assertEqual(sorted(set(ALLOWLIST) - used), [], 'stale allowlist entries')

    def test_scan_detects_direct_mutual_and_callback_recursion(self):
        self.assertEqual(cycles(ast.parse('def f(n):\n    return f(n - 1)\n')), ['f'])
        self.assertEqual(cycles(ast.parse('def a():\n    b()\ndef b():\n    a()\ndef c():\n    a()\n')), ['a', 'b'])
        self.assertEqual(cycles(ast.parse('def k(x):\n    return sorted([x], key = k)\n')), ['k'])
        self.assertEqual(cycles(ast.parse('f = lambda n: f(n - 1) if n else 0\n')), ['f'])
        self.assertEqual(cycles(ast.parse('def outer():\n    def inner():\n        outer()\n    inner()\n')),
                         ['inner', 'outer'])
        # Locals and parameters shadow functions; calling once is not recursion.
        self.assertEqual(cycles(ast.parse('def glob(glob):\n    return glob\ndef g(f):\n    f = 1\n    return f\n')), [])
        self.assertEqual(cycles(ast.parse('def one():\n    return 1\ndef two():\n    return one() + one()\n')), [])


if __name__ == '__main__':
    unittest.main()
