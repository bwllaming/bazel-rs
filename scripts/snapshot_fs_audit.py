#!/usr/bin/env python3
"""strace audit of the analysis-snapshot recording (PLAN-L2 §1b item 2, L2-W3).

Runs bazel-rs under `strace -f -y -e trace=%file,getdents64` with
BAZEL_RS_SNAPSHOT=record and BAZEL_RS_SNAPSHOT_DUMP, takes the recording window
from the marker stats crate::observed_fs makes when it opens and closes
(/.bazel-rs-snapshot-marker/window-{open,close}), and checks that every path a
file syscall touched inside the window — outside the cache dir and a short list
of process/system paths — is covered by the recorded set:

  * the path itself was observed (OBS), is an integrity entry or a repository
    root / manifest (INT, REPO) — a READ also covers its open/fstat;
  * it is an ancestor of a CANON-recorded path (realpath(3) readlinks every
    prefix) or the directory of a READDIR (getdents64 on it);
  * it lies inside a verified repository root (the integrity walk).

Default: a copy of tests/fixtures/snapshot_audit/workspace (plus the symlinks the
fixture needs) built twice with `build --nobuild //...` (the first run warms the
private cache; the second is audited) and once more with a full `build`.
`--workspace DIR` audits another workspace instead (pass the bazel-rs arguments
after `--`; it must not be ~/code's default cache).

usage: snapshot_fs_audit.py [--binary BIN] [--workspace DIR] [--cache DIR]
                            [--keep] [--report FILE] [-- ARG...]
Exit 0 iff no unrecorded path was found (and every run succeeded).
"""
import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / 'tests' / 'fixtures' / 'snapshot_audit' / 'workspace'
MARKER_OPEN = '/.bazel-rs-snapshot-marker/window-open'
MARKER_CLOSE = '/.bazel-rs-snapshot-marker/window-close'

# Process and system paths that are no inputs of planning: /proc, /sys (cgroup
# CPU quota for available_parallelism), /dev, the window markers, and the host
# facts of the workspace status (hostname, user), which is recomputed on every
# run after planning (PLAN-L2 §1b "Not recorded", §1d finalize).
IGNORED_PREFIXES = ('/proc/', '/sys/', '/dev/', '/.bazel-rs-snapshot-marker/')
IGNORED_EXACT = {'/proc', '/sys', '/dev', '/etc/ld.so.cache', '/etc/localtime',
                 '/etc/hostname', '/etc/passwd'}

CALL = re.compile(r'^(?:\[pid\s+)?(\d+)\]?\s+([a-z0-9_]+)\((.*)$')
RESUMED = re.compile(r'^(?:\[pid\s+)?(\d+)\]?\s+<\.\.\. ([a-z0-9_]+) resumed>')


def unescape_dump(token):
    """crate::observed_fs::escape: `\\xNN` for every non-graphic byte."""
    out = bytearray()
    i = 0
    raw = token.encode('ascii', 'surrogateescape')
    while i < len(raw):
        if raw[i:i + 2] == b'\\x' and i + 4 <= len(raw):
            out.append(int(raw[i + 2:i + 4], 16))
            i += 4
        else:
            out.append(raw[i])
            i += 1
    return os.fsdecode(bytes(out))


def unescape_strace(text):
    """strace's C-style string escapes (\\", \\\\, \\n, \\t, \\NNN octal, \\xNN)."""
    out = bytearray()
    raw = text.encode('utf-8', 'surrogateescape')
    i = 0
    while i < len(raw):
        c = raw[i]
        if c == 0x5c and i + 1 < len(raw):  # backslash
            n = raw[i + 1]
            if n in b'01234567':
                j = i + 1
                digits = b''
                while j < len(raw) and len(digits) < 3 and raw[j] in b'01234567':
                    digits += bytes([raw[j]])
                    j += 1
                out.append(int(digits, 8))
                i = j
                continue
            if n == ord('x') and i + 3 < len(raw):
                out.append(int(raw[i + 2:i + 4], 16))
                i += 4
                continue
            out.append({ord('n'): 10, ord('t'): 9, ord('r'): 13, ord('v'): 11, ord('f'): 12}.get(n, n))
            i += 2
            continue
        out.append(c)
        i += 1
    return os.fsdecode(bytes(out))


def strace_strings(args):
    """Quoted string arguments in order, plus `fd</path>` annotations (strace -y)."""
    strings = []
    fds = []
    i = 0
    while i < len(args):
        c = args[i]
        if c == '"':
            j = i + 1
            buf = []
            while j < len(args) and args[j] != '"':
                if args[j] == '\\' and j + 1 < len(args):
                    buf.append(args[j:j + 2])
                    j += 2
                else:
                    buf.append(args[j])
                    j += 1
            strings.append(unescape_strace(''.join(buf)))
            i = j + 1
        elif c == '<':
            j = args.find('>', i)
            if j < 0:
                break
            fds.append(args[i + 1:j])
            i = j + 1
        else:
            i += 1
    return strings, fds


def parse_strace(log, cwd):
    """(index, syscall, [paths]) per traced call, in log order."""
    events = []
    for index, line in enumerate(log.splitlines()):
        if RESUMED.match(line):
            continue
        m = CALL.match(line)
        if not m:
            continue
        name, args = m.group(2), m.group(3)
        strings, fds = strace_strings(args)
        paths = []
        if name == 'getdents64':
            if fds:
                paths.append(('dir', unescape_strace(fds[0])))
        elif strings:
            first = strings[0]
            # *at calls: a relative path is relative to the dirfd (strace -y
            # shows it as fd</dir>) or to the cwd (AT_FDCWD).
            if name.endswith('at') or name in ('statx', 'openat2', 'newfstatat'):
                head = args.split(',', 1)[0]
                if not first.startswith('/') and head.strip() != 'AT_FDCWD' and fds:
                    first = os.path.join(unescape_strace(fds[0]), first)
            if not first.startswith('/'):
                first = os.path.join(cwd, first)
            paths.append(('path', os.path.normpath(first)))
            if name in ('rename', 'renameat', 'renameat2', 'link', 'linkat', 'symlink', 'symlinkat') and len(strings) > 1:
                second = strings[1]
                if not second.startswith('/'):
                    second = os.path.join(cwd, second)
                paths.append(('path', os.path.normpath(second)))
        events.append((index, name, paths, line))
    return events


def load_dump(path):
    observed, canon_targets, readdirs, roots = set(), set(), set(), []
    for line in Path(path).read_text().splitlines():
        parts = line.split(' ')
        if parts[0] == 'OBS':
            p = unescape_dump(parts[1])
            observed.add(p)
            if parts[2] == 'CANON':
                canon_targets.add(p)
                if parts[3].startswith('path:'):
                    canon_targets.add(unescape_dump(parts[3][5:]))
            if parts[2].startswith('READDIR'):
                readdirs.add(p)
        elif parts[0] == 'INT':
            observed.add(unescape_dump(parts[1]))
        elif parts[0] == 'REPO':
            root = unescape_dump(parts[3])
            roots.append(root)
            observed.add(root)
            if parts[4].startswith('manifest:') and parts[4] != 'manifest:-':
                observed.add(unescape_dump(parts[4][len('manifest:'):]))
    ancestors = set()
    for target in canon_targets:
        p = Path(target)
        for parent in [p] + list(p.parents):
            ancestors.add(str(parent))
    return observed, ancestors, readdirs, roots


def audit(log, dump, cwd, cache, extra_ignored):
    events = parse_strace(log, cwd)
    start = end = None
    for i, (_, _, paths, _) in enumerate(events):
        for _, p in paths:
            if p == MARKER_OPEN and start is None:
                start = i
            if p == MARKER_CLOSE and start is not None and end is None:
                end = i
    if start is None or end is None:
        return None, ['window markers not found in the trace (is BAZEL_RS_SNAPSHOT=record set?)']
    observed, ancestors, readdirs, roots = load_dump(dump)
    ignored = [str(Path(cache).resolve())] + [str(Path(p).resolve()) for p in extra_ignored]
    unrecorded = {}
    touched = 0
    for _, name, paths, line in events[start + 1:end]:
        for kind, p in paths:
            if p in IGNORED_EXACT or p.startswith(IGNORED_PREFIXES):
                continue
            if any(p == prefix or p.startswith(prefix + '/') for prefix in ignored):
                continue
            touched += 1
            if kind == 'dir':
                if p in readdirs:
                    continue
            elif p in observed or p in ancestors:
                continue
            if any(p == root or p.startswith(root + '/') for root in roots):
                continue
            unrecorded.setdefault(p, (name, line.strip()))
    return touched, [f'{p}  [{name}] {line[:200]}' for p, (name, line) in sorted(unrecorded.items())]


def prepare_fixture(base):
    workspace = base / 'workspace'
    shutil.copytree(FIXTURE, workspace, symlinks=True)
    os.symlink('input.txt', workspace / 'link.txt')
    os.symlink('deeper', workspace / 'pkg' / 'data' / 'alias')
    return workspace


def run(binary, workspace, cache, args, dump, log, tmp):
    env = {
        'PATH': '/usr/bin:/bin',
        'HOME': str(tmp / 'home'),
        'TMPDIR': str(tmp / 'tmp'),
        'BAZEL_RS_SNAPSHOT': 'record',
        'BAZEL_RS_SNAPSHOT_DUMP': str(dump),
    }
    for d in ('home', 'tmp'):
        (tmp / d).mkdir(parents=True, exist_ok=True)
    argv = ['strace', '-f', '-y', '-qq', '-s', '4096', '-e', 'trace=%file,getdents64', '-o', str(log),
            str(binary), '--ignore_all_rc_files', f'--cache-dir={cache}'] + args
    proc = subprocess.run(argv, cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, timeout=3600)
    return proc


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--binary', default=str(Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')) / 'release' / 'bazel-rs'))
    ap.add_argument('--workspace', default=None)
    ap.add_argument('--cache', default=None)
    ap.add_argument('--keep', action='store_true')
    ap.add_argument('--report', default=None)
    ap.add_argument('--ignore', action='append', default=[], help='extra path prefix to ignore (e.g. a private HOME)')
    ap.add_argument('args', nargs='*')
    a = ap.parse_args()
    if shutil.which('strace') is None:
        sys.exit('strace not found')
    base = Path(tempfile.mkdtemp(prefix='snapshot-audit-', dir=os.environ.get('TMPDIR')))
    lines = []
    ok = True
    try:
        if a.workspace:
            workspace = Path(a.workspace).resolve()
            warmups = []
            runs = [a.args or ['build', '--nobuild', '//...']]
        else:
            workspace = prepare_fixture(base)
            flags = ['--experimental-builtin-modules']
            # Cold runs fetch registry files, write the lockfile and publish
            # @bazel_tools (tainted, not audited); then the warm runs.
            warmups = [['build', '--nobuild'] + flags + ['//...'],
                       ['query'] + flags + ['deps(//...)']]
            runs = [['build', '--nobuild'] + flags + ['//...'],
                    ['build'] + flags + ['//...'],
                    ['query'] + flags + ['deps(//...)']]
        cache = Path(a.cache).resolve() if a.cache else base / 'cache'
        if str(cache).startswith('/home/coder/code'):
            sys.exit('refusing a cache dir inside ~/code')
        for i, args in enumerate(warmups + runs):
            dump = base / f'dump-{i}.txt'
            log = base / f'strace-{i}.log'
            proc = run(Path(a.binary), workspace, cache, args, dump, log, base)
            lines.append(f'run {i}: {" ".join(args)} -> rc={proc.returncode}')
            if proc.returncode != 0:
                ok = False
                lines.append(proc.stderr[-4000:])
                continue
            summary = [l for l in proc.stderr.splitlines() if l.startswith('bazel-rs-snapshot:')]
            lines.extend('  ' + l for l in summary)
            if i < len(warmups):
                lines.append('  (warm-up run: not audited)')
                continue
            touched, unrecorded = audit(log.read_text(errors='surrogateescape'), dump, str(workspace),
                                        cache, [base / 'home', base / 'tmp'] + a.ignore)
            if touched is None:
                ok = False
                lines.extend('  ' + u for u in unrecorded)
                continue
            lines.append(f'  window: {touched} path touches outside the cache; unrecorded: {len(unrecorded)}')
            for u in unrecorded[:200]:
                lines.append('    UNRECORDED ' + u)
            if unrecorded:
                ok = False
    finally:
        if not a.keep:
            shutil.rmtree(base, ignore_errors=True)
        else:
            lines.append(f'kept {base}')
    text = '\n'.join(lines) + '\n' + ('AUDIT CLEAN\n' if ok else 'AUDIT FAILED\n')
    sys.stdout.write(text)
    if a.report:
        Path(a.report).write_text(text)
    sys.exit(0 if ok else 1)


if __name__ == '__main__':
    main()
