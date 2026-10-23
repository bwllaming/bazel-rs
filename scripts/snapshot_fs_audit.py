#!/usr/bin/env python3
"""strace audit of the analysis-snapshot recording (PLAN-L2 §1b item 2, L2-W3).

Runs bazel-rs under `strace -f -y -e trace=%file,getdents64,%process` with
BAZEL_RS_SNAPSHOT=record and BAZEL_RS_SNAPSHOT_DUMP, takes the recording window
from the marker stats crate::observed_fs makes when it opens and closes
(/.bazel-rs-snapshot-marker/window-{open,close}), and checks that every path a
file syscall touched inside the window — outside the cache dir and a short list
of process/system paths — is covered by the recorded set:

  * the path itself was observed (OBS), is an integrity entry or a repository
    root / manifest (INT, REPO) — a READ also covers its open/fstat;
  * it is on the resolution chain of a CANON-recorded path (realpath(3)
    readlinks every prefix, through link targets; simulated now) or an
    ancestor of its result;
  * it is a listed directory: getdents64 on an fd whose openat path (the
    logical one; strace -y shows the resolved one) has a READDIR record;
  * it lies inside a verified repository root (the integrity walk).

Not audited: calls of a thread between the tainted-section markers
(/.bazel-rs-snapshot-marker/tainted-{begin,end}: a repository rule or module
extension executing, which taints the run or — a local repository rule — is
replayed and compared instead), and every call of a process that execs inside
the window (processes such executions start; threads are joined to their
process through the traced clone calls).

The record mode's replay dry run (L2-W4R, after the window, bracketed by
/.bazel-rs-snapshot-marker/replay-{begin,end}) is audited too: a replay may
touch only recorded paths (as above), the cache dir (its stage, the store's
trees) and pinned repository roots (ROOT lines of the dump) — and, inside the
execution of a rule the dump marks impure (REPLAY ... flags:environ,execute,
which,...; bracketed per thread by /.bazel-rs-snapshot-marker/replay-exec/NAME
and .../replay-exec-end), any host path. Processes started by a replay are
skipped like those of the window.

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
# Bracket, on the executing thread, a repository rule or module extension
# executing (crate::observed_fs::tainted_section): the run is tainted, its
# accesses need no record. Processes started inside the window (they execve)
# belong to such executions too.
MARKER_TAINTED_BEGIN = '/.bazel-rs-snapshot-marker/tainted-begin'
MARKER_TAINTED_END = '/.bazel-rs-snapshot-marker/tainted-end'
# The replay dry run (crate::snapshot::replay) and, per thread, one replayed
# execution (the marker names the repository).
MARKER_REPLAY_BEGIN = '/.bazel-rs-snapshot-marker/replay-begin'
MARKER_REPLAY_END = '/.bazel-rs-snapshot-marker/replay-end'
MARKER_REPLAY_EXEC = '/.bazel-rs-snapshot-marker/replay-exec/'
MARKER_REPLAY_EXEC_END = '/.bazel-rs-snapshot-marker/replay-exec-end'

# Process and system paths that are no inputs of planning: /proc, /sys (cgroup
# CPU quota for available_parallelism), /dev, the window markers, and the host
# facts of the workspace status (hostname, user), which is recomputed on every
# run after planning (PLAN-L2 §1b "Not recorded", §1d finalize).
IGNORED_PREFIXES = ('/proc/', '/sys/', '/dev/', '/.bazel-rs-snapshot-marker/')
IGNORED_EXACT = {'/proc', '/sys', '/dev', '/etc/ld.so.cache', '/etc/localtime',
                 '/etc/hostname', '/etc/passwd'}

CALL = re.compile(r'^(?:\[pid\s+)?(\d+)\]?\s+([a-z0-9_]+)\((.*)$')
UNFINISHED = re.compile(r'^(?:\[pid\s+)?(\d+)\]?\s+([a-z0-9_]+)\((.*) <unfinished \.\.\.>$')
RESUMED = re.compile(r'^(?:\[pid\s+)?(\d+)\]?\s+<\.\.\. ([a-z0-9_]+) resumed>(.*)$')
RETURN = re.compile(r'\)\s+=\s+(-?\d+)(?:<([^>]*)>)?(?:\s+\w+)?(?:\s+\(.*\))?\s*$')
FD_ARG = re.compile(r'^\s*(\d+)<')


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


def complete_calls(log):
    """(tid, syscall, argument text with the result, line) per call; a call
    strace split into `<unfinished ...>` and `<... resumed>` is joined."""
    pending = {}
    for line in log.splitlines():
        m = UNFINISHED.match(line)
        if m:
            pending[int(m.group(1))] = (m.group(2), m.group(3), line)
            continue
        m = RESUMED.match(line)
        if m:
            tid, name = int(m.group(1)), m.group(2)
            if tid in pending and pending[tid][0] == name:
                _, args, first = pending.pop(tid)
                yield tid, name, args + m.group(3), first.strip() + ' ' + line.strip()
            continue
        m = CALL.match(line)
        if m:
            yield int(m.group(1)), m.group(2), m.group(3), line


def parse_strace(log, cwd):
    """(tid, syscall, [paths], line, result) per traced call, in log order;
    `result` is (return value, fd path annotation) or None."""
    events = []
    for tid, name, args, line in complete_calls(log):
        ret = RETURN.search(args)
        result = (int(ret.group(1)), ret.group(2)) if ret else None
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
        events.append((tid, name, paths, line, result))
    return events


def realpath_chain(path):
    """The paths realpath(3) readlinks resolving `path`, simulated now."""
    visited = set()
    pending = [c for c in path.split('/') if c]
    resolved = '/'
    hops = 0
    while pending:
        component = pending.pop(0)
        if component == '.':
            continue
        if component == '..':
            resolved = os.path.dirname(resolved) or '/'
            continue
        candidate = os.path.join(resolved, component)
        visited.add(candidate)
        try:
            target = os.readlink(candidate)
        except OSError:
            resolved = candidate
            continue
        hops += 1
        if hops > 40:
            break
        if target.startswith('/'):
            resolved = '/'
        pending = [c for c in target.split('/') if c] + pending
    return visited


def load_replay(path):
    """The replay records' impurity flags by repository and the pinned roots
    (REPLAY and ROOT lines of the dump)."""
    impure, pinned = {}, []
    for line in Path(path).read_text().splitlines():
        parts = line.split(' ')
        if parts[0] == 'REPLAY' and len(parts) > 4:
            flags = next((p[len('flags:'):] for p in parts if p.startswith('flags:')), '-')
            impure[unescape_dump(parts[1])] = flags != '-'
        elif parts[0] == 'ROOT' and len(parts) > 2:
            pinned.append(unescape_dump(parts[2]))
    return impure, pinned


def load_dump(path):
    observed, canon_targets, readdirs, roots = set(), set(), set(), []
    canonicalized = set()
    for line in Path(path).read_text().splitlines():
        parts = line.split(' ')
        if parts[0] == 'OBS':
            p = unescape_dump(parts[1])
            observed.add(p)
            if parts[2] == 'CANON':
                canon_targets.add(p)
                canonicalized.add(p)
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
    return observed, ancestors, readdirs, roots, canonicalized


def audit(log, dump, cwd, cache, extra_ignored, replay=None):
    """(touched, unrecorded) of the recording window; with `replay` (a dict),
    the replay section's counts are stored in it: touched, unrecorded,
    executions (replayed executions seen), present (markers found)."""
    events = parse_strace(log, cwd)
    start = end = None
    replay_start = replay_end = None
    for i, (_, _, paths, _, _) in enumerate(events):
        for _, p in paths:
            if p == MARKER_OPEN and start is None:
                start = i
            if p == MARKER_CLOSE and start is not None and end is None:
                end = i
            if p == MARKER_REPLAY_BEGIN and replay_start is None:
                replay_start = i
            if p == MARKER_REPLAY_END and replay_start is not None and replay_end is None:
                replay_end = i
    if start is None or end is None:
        return None, ['window markers not found in the trace (is BAZEL_RS_SNAPSHOT=record set?)']
    observed, ancestors, readdirs, roots, canonicalized = load_dump(dump)
    # realpath(3) readlinks every prefix of the path it resolves, through
    # link targets: a CANON record covers that whole chain.
    for path in canonicalized:
        ancestors |= realpath_chain(path)
    ignored = [str(Path(cache).resolve())] + [str(Path(p).resolve()) for p in extra_ignored]
    sections = [(start, end)]
    if replay_start is not None and replay_end is not None:
        sections.append((replay_start, replay_end))
    # Process tree (strace -f with %process): threads join their process,
    # and a process that execs inside the window was started by a repository
    # rule or extension execution (tainted, never recorded) — or, in the
    # replay section, by a replayed local rule.
    process = {}
    started = set()
    for i, (tid, name, _, line, result) in enumerate(events):
        pid = process.setdefault(tid, tid)
        if name in ('clone', 'clone3', 'fork', 'vfork') and result and result[0] > 0:
            child = result[0]
            process[child] = pid if 'CLONE_THREAD' in line else child
        elif name == 'execve' and any(a < i < b for a, b in sections):
            started.add(pid)

    def covered(kind, p, line, opened, pid, extra_roots=()):
        if kind == 'dir':
            fd = FD_ARG.match(line.split('(', 1)[1]) if '(' in line else None
            logical = opened.get((pid, int(fd.group(1)))) if fd else None
            if logical is not None:
                p = logical
        if p in IGNORED_EXACT or p.startswith(IGNORED_PREFIXES):
            return p, None
        if any(p == prefix or p.startswith(prefix + '/') for prefix in ignored):
            return p, None
        if kind == 'dir':
            if p in readdirs:
                return p, True
        elif p in observed or p in ancestors:
            return p, True
        if any(p == root or p.startswith(root + '/') for root in list(roots) + list(extra_roots)):
            return p, True
        return p, False

    opened = {}
    depth = {}
    unrecorded = {}
    touched = 0
    for tid, name, paths, line, result in events[start + 1:end]:
        pid = process.get(tid, tid)
        if pid in started:
            continue
        markers = [p for _, p in paths if p in (MARKER_TAINTED_BEGIN, MARKER_TAINTED_END)]
        if markers:
            depth[tid] = depth.get(tid, 0) + (1 if markers[0] == MARKER_TAINTED_BEGIN else -1)
            continue
        if depth.get(tid, 0) > 0:
            continue
        if name in ('openat', 'open', 'openat2') and result and result[0] >= 0 and paths:
            # getdents64 shows the fd's resolved path; remember what was opened.
            opened[(pid, result[0])] = paths[0][1]
        for kind, p in paths:
            p, ok = covered(kind, p, line, opened, pid)
            if ok is None:
                continue
            touched += 1
            if not ok:
                unrecorded.setdefault(p, (name, line.strip()))
    if replay is not None:
        impure, pinned = load_replay(dump)
        replay.update(present=len(sections) > 1, touched=0, unrecorded=[], executions=0)
        if len(sections) > 1:
            current = {}
            replay_unrecorded = {}
            replay_opened = {}
            for tid, name, paths, line, result in events[replay_start + 1:replay_end]:
                pid = process.get(tid, tid)
                if pid in started:
                    continue
                execs = [p for _, p in paths if p.startswith(MARKER_REPLAY_EXEC) or p == MARKER_REPLAY_EXEC_END]
                if execs:
                    if execs[0] == MARKER_REPLAY_EXEC_END:
                        current.pop(tid, None)
                    else:
                        current[tid] = unescape_dump(execs[0][len(MARKER_REPLAY_EXEC):])
                        replay['executions'] += 1
                    continue
                if tid in current and impure.get(current[tid], False):
                    # An impure rule's execution may read the host (which,
                    # execute, os.environ, host readdir): its replay compares
                    # what it produced instead.
                    continue
                if name in ('openat', 'open', 'openat2') and result and result[0] >= 0 and paths:
                    replay_opened[(pid, result[0])] = paths[0][1]
                for kind, p in paths:
                    p, ok = covered(kind, p, line, replay_opened, pid, pinned)
                    if ok is None:
                        continue
                    replay['touched'] += 1
                    if not ok:
                        replay_unrecorded.setdefault(p, (name, line.strip()))
            replay['unrecorded'] = [f'{p}  [{name}] {line[:200]}' for p, (name, line) in sorted(replay_unrecorded.items())]
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
    argv = ['strace', '-f', '-y', '-qq', '-s', '4096', '-e', 'trace=%file,getdents64,%process', '-o', str(log),
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
            replay = {}
            touched, unrecorded = audit(log.read_text(errors='surrogateescape'), dump, str(workspace),
                                        cache, [base / 'home', base / 'tmp'] + a.ignore, replay)
            if touched is None:
                ok = False
                lines.extend('  ' + u for u in unrecorded)
                continue
            lines.append(f'  window: {touched} path touches outside the cache; unrecorded: {len(unrecorded)}')
            for u in unrecorded[:200]:
                lines.append('    UNRECORDED ' + u)
            if unrecorded:
                ok = False
            if replay.get('present'):
                lines.append(f"  replay: {replay['executions']} replayed executions, {replay['touched']} path touches "
                             f"outside the cache (impure executions exempt); unrecorded: {len(replay['unrecorded'])}")
                for u in replay['unrecorded'][:200]:
                    lines.append('    REPLAY-UNRECORDED ' + u)
                if replay['unrecorded']:
                    ok = False
            else:
                lines.append('  replay: no replay section (no local repository rule executed)')
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
