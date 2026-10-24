#!/usr/bin/env python3
"""A/B equivalence and benchmark harness for bazel-rs analysis-performance work.

Runs two bazel-rs binaries (A = baseline, B = candidate) on a read-only
workspace (default /home/coder/code) and checks that they are equivalent,
then reports their timings. docs/PERF_LAYER1.md describes the recipe, the
target sets and how to read the output.

usage: ab_bench.py --a BIN_A --b BIN_B --cache-a DIR --cache-b DIR --out OUTDIR
                   [--mode nobuild|build] [--warmups W] [--runs N]
                   (--lockfile-seed LOCKFILE [--lockfile-dir DIR] | --lockfile-mode off)
                   [--extra-env K=V ...]
                   [--extra-env-a K=V ...] [--extra-env-b K=V ...]
                   [--extra-arg ARG ...] [--sides ab|a|b] [--timeout S]
                   [--workspace DIR] [--compare-only] TARGET...

Each run of each binary happens from the workspace, under `env -i` with a
private HOME and TMPDIR:

  <bin> --ignore_all_rc_files --cache-dir=<cache> build [--nobuild]
        --experimental-builtin-modules <lockfile flags> --remote_cache=
        --noremote_upload_local_results [EXTRA_ARG...] TARGET...

with BAZEL_RS_DUMP_ACTIONS=1 and BAZEL_RS_TIMING=1. Lockfile flags (one of
the two is required; there is no silent default):
  - with --lockfile-seed (the update-mode recipe): the seed is copied ONCE
    to DIR/lock-a and DIR/lock-b (DIR = --lockfile-dir, default OUTDIR), and
    each side passes --lockfile_mode=update --native-lockfile=DIR/lock-<side>.
    The workspace's own lockfile is never written.
  - with an explicit --lockfile-mode off: --lockfile_mode=off (it re-fetches
    registry files and re-evaluates module extensions on every run, ~10 s of
    network wait on ~/code: not the user's default, see docs/PERF_LAYER1.md).

The sides run interleaved (a0 b0 a1 b1 ...): W warm-up runs, then N measured
runs, W+N in total. With the default W=0, `--runs N` behaves exactly like the
former ab_equiv.py (N runs, the last one compared).

Equivalence (A vs B, last run of each side): identical exit code, stdout,
stderr outside the action dump and PERF lines, and the normalized
BAZEL_RS_DUMP_ACTIONS dump (cache-dir prefix -> <CACHE>,
repositories/direct/trees/<hash> -> trees/<RUN>, lock-a/lock-b -> lock-<SIDE>).
Differences are written to OUTDIR/diff-{rest,dump,stdout}.{a,b}.

Measurements per run (OUTDIR/<side>-<i>.json): wall time, the child's
user/sys CPU and max RSS (os.wait4 rusage), the PERF marks (time, rss, hwm),
PERF-STAGE totals and counters, and PERF-ALLOC rows when present. The
report's summary has the medians over the measured runs.

Outputs: OUTDIR/<side>-<i>.{out,err,json}, OUTDIR/report.json; the process
exits 0 iff the sides are equivalent (or only one side ran). --compare-only
rebuilds report.json from the files already in OUTDIR without running
anything. Canary: the workspace's git HEAD and MODULE.bazel.lock sha256 are
recorded before and after; this script never modifies the workspace.
NEVER pass the workspace's default cache (<workspace>/bazel-rs-out).
"""
import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

WORKSPACE = Path('/home/coder/code')

# Normalisation of per-run noise in outputs.
TREES = re.compile(r'repositories/direct/trees/[0-9a-f]{8,}')
LOCKSIDE = re.compile(r'/lock-[ab]\b')

# PERF output shapes (src/lib.rs perf_mark, src/plan_timings.rs report,
# src/engine/alloc_report.rs). `hwm=` is absent from older binaries.
PERF_MARK = re.compile(r'PERF\s+([0-9.]+)s rss=(\d*)kB (\S+)(?: hwm=(\d*)kB)?')
STAGE_TOTAL = re.compile(r'PERF-STAGE total (\S+) ([0-9.]+)s(?: count=(\d+))?(?: covered=([0-9.]+)%)?')
ALLOC_ROW = re.compile(r'PERF-ALLOC(-DETAIL|-HEAP)? (\d+) (\S+)(?: heaps=(\d+))?$')
COUNTER = re.compile(r'(\w+)=([0-9.]+)')


def sha(path):
    path = Path(path)
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None


def canary(workspace):
    head = subprocess.run(['git', '-C', str(workspace), 'rev-parse', 'HEAD'],
                          capture_output=True, text=True).stdout.strip()
    return {'head': head, 'lock': sha(Path(workspace) / 'MODULE.bazel.lock')}


def norm(text, cache):
    """Replace per-side and per-run noise: the cache prefix, the side's
    private lockfile name and content-addressed repository tree paths."""
    text = text.replace(str(cache), '<CACHE>')
    text = LOCKSIDE.sub('/lock-<SIDE>', text)
    return TREES.sub('repositories/direct/trees/<RUN>', text)


def split_err(text, cache):
    """Split normalized stderr into (action dump, other lines, PERF lines).
    The dump is every `ACTION ` line with its indented continuation lines."""
    dump, rest, perf = [], [], []
    in_action = False
    for line in text.splitlines():
        line = norm(line, cache)
        if line.startswith('PERF'):
            perf.append(line)
            in_action = False
            continue
        if line.startswith('ACTION '):
            in_action = True
            dump.append(line)
            continue
        if in_action and line.startswith('  '):
            dump.append(line)
            continue
        in_action = False
        rest.append(line)
    return dump, rest, perf


def _kb(text):
    return int(text) if text else None


def perf_marks(perf):
    """`PERF <t>s rss=<kB>kB <mark> [hwm=<kB>kB]` lines by mark name (the
    last occurrence wins). `rss_mb` is kept for ab_equiv.py compatibility."""
    marks = {}
    for line in perf:
        match = PERF_MARK.match(line)
        if not match:
            continue
        rss = _kb(match.group(2))
        marks[match.group(3)] = {
            't': float(match.group(1)),
            'rss_kb': rss,
            'rss_mb': rss // 1024 if rss is not None else None,
            'hwm_kb': _kb(match.group(4)),
        }
    return marks


def stage_totals(perf):
    """`PERF-STAGE total <stage> <s>s count=<n>` (and the plan_span row's
    `covered=<pct>%`) by stage."""
    totals = {}
    for line in perf:
        match = STAGE_TOTAL.match(line)
        if not match:
            continue
        row = {'s': float(match.group(2))}
        if match.group(3) is not None:
            row['count'] = int(match.group(3))
        if match.group(4) is not None:
            row['covered_pct'] = float(match.group(4))
        totals[match.group(1)] = row
    return totals


def counters(line):
    """`key=value` numeric fields of one line."""
    out = {}
    for key, value in COUNTER.findall(line):
        out[key] = float(value) if '.' in value else int(value)
    return out


def perf_counters(perf):
    """The W0-A counter rows: flush batches, views and the allocation
    report's summary, counts and type sizes."""
    rows = {}
    for line in perf:
        for prefix, name in (('PERF-STAGE flush-batch ', 'flush_batch'),
                             ('PERF-STAGE views ', 'views'),
                             ('PERF-ALLOC-SUMMARY ', 'alloc_summary'),
                             ('PERF-ALLOC-COUNTS ', 'alloc_counts'),
                             ('PERF-ALLOC-SIZES ', 'alloc_sizes')):
            if line.startswith(prefix):
                rows[name] = counters(line[len(prefix):])
    return rows


def alloc_rows(perf, kind=''):
    """`PERF-ALLOC <bytes> <name>` rows (largest first, as printed); with
    kind='-DETAIL' or '-HEAP' the rule-part or frozen-heap rows."""
    rows = []
    for line in perf:
        match = ALLOC_ROW.match(line)
        if match and (match.group(1) or '') == kind:
            row = {'bytes': int(match.group(2)), 'name': match.group(3)}
            if match.group(4) is not None:
                row['heaps'] = int(match.group(4))
            rows.append(row)
    return rows


def median(values):
    values = [v for v in values if v is not None]
    return round(statistics.median(values), 3) if values else None


def summarize(records):
    """Medians over the given run records (the measured runs)."""
    summary = {'runs': len(records)}
    for key in ('wall', 'user', 'sys', 'cpu', 'maxrss_kb'):
        summary[key] = median([r.get(key) for r in records])
    marks = sorted({m for r in records for m in r.get('marks', {})})
    summary['marks'] = {
        m: {field: median([r['marks'].get(m, {}).get(field) for r in records])
            for field in ('t', 'rss_kb', 'hwm_kb')}
        for m in marks}
    stages = sorted({s for r in records for s in r.get('stages', {})})
    summary['stages_s'] = {s: median([r['stages'].get(s, {}).get('s') for r in records])
                           for s in stages}
    return summary


def run_one(bin_, cache, out, args, extra_env, tag, workspace, timeout):
    """One run; writes OUTDIR/<tag>.{out,err} and returns its rc, wall time
    and the child's rusage."""
    home = out / f'{tag}-home'
    tmp = out / f'{tag}-tmp'
    home.mkdir(parents=True, exist_ok=True)
    tmp.mkdir(parents=True, exist_ok=True)
    env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'TMPDIR': str(tmp),
           'BAZEL_RS_DUMP_ACTIONS': '1', 'BAZEL_RS_TIMING': '1'}
    env.update(extra_env)
    argv = [bin_, '--ignore_all_rc_files', f'--cache-dir={cache}', 'build'] + args
    with open(out / f'{tag}.out', 'wb') as stdout, open(out / f'{tag}.err', 'wb') as stderr:
        started = time.monotonic()
        proc = subprocess.Popen(argv, cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                                stdout=stdout, stderr=stderr)
        killer = threading.Timer(timeout, proc.kill)
        killer.start()
        try:
            _, status, usage = os.wait4(proc.pid, 0)
        finally:
            killer.cancel()
        wall = time.monotonic() - started
    proc.returncode = os.waitstatus_to_exitcode(status)
    return {'rc': proc.returncode, 'wall': round(wall, 3),
            'user': round(usage.ru_utime, 3), 'sys': round(usage.ru_stime, 3),
            'cpu': round(usage.ru_utime + usage.ru_stime, 3),
            'maxrss_kb': usage.ru_maxrss, 'argv': argv}


def lockfile_mode_problem(args):
    """Why the lockfile flags cannot run (None when they can): the update-mode
    recipe needs --lockfile-seed; --lockfile_mode=off only with an explicit
    --lockfile-mode off, never as a silent default."""
    mode = getattr(args, 'lockfile_mode', None)
    if args.lockfile_seed:
        if mode == 'off':
            return '--lockfile-seed and --lockfile-mode off contradict each other'
        return None
    if mode == 'off':
        return None
    if mode == 'update':
        return '--lockfile-mode update needs --lockfile-seed (a private copy of the workspace lockfile)'
    return ('pass --lockfile-seed <private copy of the workspace lockfile> (the update-mode recipe, '
            "the user's default) or an explicit --lockfile-mode off")


def side_args(args, out, side):
    if args.lockfile_seed:
        lock = Path(args.lockfile_dir or out) / f'lock-{side}'
        if not lock.exists():
            lock.write_bytes(Path(args.lockfile_seed).read_bytes())
        lockfile = ['--lockfile_mode=update', f'--native-lockfile={lock}']
    elif getattr(args, 'lockfile_mode', None) == 'off':
        lockfile = ['--lockfile_mode=off']
    else:
        raise ValueError(lockfile_mode_problem(args))
    return ((['--nobuild'] if args.mode == 'nobuild' else []) + ['--experimental-builtin-modules']
            + lockfile + ['--remote_cache=', '--noremote_upload_local_results']
            + args.extra_arg + args.targets)


def parse_run(out, tag, cache, fallback=None):
    """Everything known about one finished run, from its files (`fallback`:
    the run's rc/wall from an older report when it has no <tag>.json)."""
    meta_path = out / f'{tag}.json'
    record = json.loads(meta_path.read_text()) if meta_path.exists() else dict(fallback or {})
    record['tag'] = tag
    err = (out / f'{tag}.err').read_text(errors='replace')
    _, _, perf = split_err(err, cache)
    record['marks'] = perf_marks(perf)
    record['stages'] = stage_totals(perf)
    record.update(perf_counters(perf))
    for key, kind in (('alloc', ''), ('alloc_detail', '-DETAIL'), ('alloc_heap', '-HEAP')):
        rows = alloc_rows(perf, kind)
        if rows:
            record[key] = rows
    if record['marks']:
        # Peak RSS of the run: the largest hwm the marks saw.
        hwms = [m['hwm_kb'] for m in record['marks'].values() if m.get('hwm_kb')]
        record['peak_hwm_kb'] = max(hwms) if hwms else None
    return record


def run_tags(out, side):
    """`<side>-<i>` tags with an .err file, in run order."""
    found = []
    for path in out.glob(f'{side}-*.err'):
        index = path.stem.split('-', 1)[1]
        if index.isdigit():
            found.append(int(index))
    return [f'{side}-{i}' for i in sorted(found)]


def compare(out, caches, tags, runs):
    """Equivalence of the two sides' last runs; writes diff files."""
    parts = {}
    for side in 'ab':
        cache = caches[side]
        tag = tags[side]
        err = (out / f'{tag}.err').read_text(errors='replace')
        stdout = norm((out / f'{tag}.out').read_text(errors='replace'), cache)
        dump, rest, _ = split_err(err, cache)
        parts[side] = {'stdout': stdout.splitlines(), 'dump': dump, 'rest': rest}
    rc = {side: runs[side][-1].get('rc') for side in 'ab'}
    result = {
        'compared': tags,
        'rc_equal': None if None in rc.values() else rc['a'] == rc['b'],
        'stdout_equal': parts['a']['stdout'] == parts['b']['stdout'],
        'stderr_rest_equal': parts['a']['rest'] == parts['b']['rest'],
        'dump_equal': parts['a']['dump'] == parts['b']['dump'],
        'dump_actions': {s: sum(1 for l in parts[s]['dump'] if l.startswith('ACTION ')) for s in 'ab'},
    }
    for key in ('rest', 'dump', 'stdout'):
        for side in 'ab':
            stale = out / f'diff-{key}.{side}'
            if stale.exists():
                stale.unlink()
        if parts['a'][key] != parts['b'][key]:
            for side in 'ab':
                (out / f'diff-{key}.{side}').write_text('\n'.join(parts[side][key]) + '\n')
    result['equivalent'] = all(result[k] is not False for k in
                               ('rc_equal', 'stdout_equal', 'stderr_rest_equal', 'dump_equal'))
    return result


def previous_run(previous, side, tag):
    """A run's record in an older report.json: ab_bench's (by tag) or
    ab_equiv.py's (a list of {rc, wall} by run index)."""
    index = int(tag.split('-', 1)[1])
    for position, record in enumerate((previous or {}).get(side, [])):
        if record.get('tag', f'{side}-{position}') == tag or ('tag' not in record and position == index):
            return {k: v for k, v in record.items() if k in ('rc', 'wall', 'user', 'sys', 'cpu', 'maxrss_kb', 'warmup')}
    return None


def build_report(out, config, caches, sides, warmups, canaries, previous_runs=None):
    runs = {side: [parse_run(out, tag, caches[side], previous_run(previous_runs, side, tag))
                   for tag in run_tags(out, side)] for side in sides}
    summary = {side: summarize(runs[side][warmups:] or runs[side]) for side in sides}
    report = {'config': config, 'runs': runs, 'summary': summary}
    report.update(canaries)
    if set(sides) == {'a', 'b'} and runs['a'] and runs['b']:
        tags = {side: runs[side][-1]['tag'] for side in 'ab'}
        report['compare'] = compare(out, caches, tags, runs)
        report['equivalent'] = report['compare']['equivalent']
    else:
        report['equivalent'] = None
    return report


def brief(report):
    """The console digest: verdict, medians and stage totals per side."""
    digest = {'equivalent': report.get('equivalent'),
              'canary_unchanged': report.get('canary_unchanged')}
    if 'compare' in report:
        digest['compare'] = {k: v for k, v in report['compare'].items() if k != 'compared'}
    for side, summary in report['summary'].items():
        marks = summary['marks']
        digest[side] = {
            'runs': summary['runs'], 'wall': summary['wall'], 'user': summary['user'],
            'sys': summary['sys'], 'maxrss_kb': summary['maxrss_kb'],
            'plan.start': marks.get('plan.start', {}).get('t'),
            'plan.phase1_analysis_done': marks.get('plan.phase1_analysis_done', {}).get('t'),
            'plan.end': marks.get('plan.end', {}).get('t'),
            'execute.end': marks.get('execute.end', {}).get('t'),
            'hwm_kb': max([m.get('hwm_kb') or 0 for m in marks.values()] or [0]) or None,
            'stages_s': summary['stages_s'],
        }
    return digest


def parse_env(pairs):
    env = {}
    for pair in pairs:
        key, sep, value = pair.partition('=')
        if not sep:
            sys.exit(f'--extra-env expects K=V, got {pair!r}')
        env[key] = value
    return env


def absolutize(args):
    """Make every path argument absolute against the harness's own working directory.

    The binaries run with cwd=<workspace>, so a relative cache, output, lockfile or binary path
    would otherwise resolve INSIDE the workspace (a relative HOME/TMPDIR under --out, a relative
    --cache-dir or --native-lockfile)."""
    for name in ('a', 'b', 'cache_a', 'cache_b', 'out', 'lockfile_seed', 'lockfile_dir', 'workspace'):
        value = getattr(args, name, None)
        if value:
            setattr(args, name, str(Path(value).resolve()))
    return args


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--a', help='baseline binary (side A)')
    ap.add_argument('--b', help='candidate binary (side B)')
    ap.add_argument('--cache-a', required=True)
    ap.add_argument('--cache-b', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--mode', default='nobuild', choices=['nobuild', 'build'])
    ap.add_argument('--warmups', type=int, default=0, help='warm-up runs per side (not in the medians)')
    ap.add_argument('--runs', type=int, default=2, help='measured runs per side after the warm-ups')
    ap.add_argument('--extra-env', action='append', default=[], help='K=V for both sides')
    ap.add_argument('--extra-env-a', action='append', default=[], help='K=V for side A only')
    ap.add_argument('--extra-env-b', action='append', default=[], help='K=V for side B only')
    ap.add_argument('--extra-arg', action='append', default=[])
    ap.add_argument('--lockfile-seed', default=None,
                    help='update-mode recipe: copy this lockfile once to OUT/lock-{a,b} and pass '
                         '--lockfile_mode=update --native-lockfile=OUT/lock-<side> (required unless '
                         '--lockfile-mode off)')
    ap.add_argument('--lockfile-mode', default=None, choices=['update', 'off'],
                    help="'update' (needs --lockfile-seed) or an explicit 'off' (--lockfile_mode=off, "
                         'without a seed); never chosen silently')
    ap.add_argument('--lockfile-dir', default=None,
                    help='where lock-{a,b} live (default: OUT); share one across target sets so each side '
                         'keeps one private lockfile')
    ap.add_argument('--sides', default='ab', choices=['ab', 'a', 'b'], help='run only one side (no comparison)')
    ap.add_argument('--timeout', type=float, default=7200, help='seconds per run before it is killed')
    ap.add_argument('--workspace', default=str(WORKSPACE))
    ap.add_argument('--compare-only', action='store_true',
                    help='rebuild report.json from the files in OUT without running anything')
    ap.add_argument('targets', nargs='*')
    args = absolutize(ap.parse_args(argv))

    workspace = Path(args.workspace)
    out = Path(args.out)
    caches = {'a': args.cache_a, 'b': args.cache_b}
    for path in (args.cache_a, args.cache_b, args.out, args.lockfile_dir):
        if not path:
            continue
        resolved = Path(path)
        if resolved == workspace or str(resolved).startswith(str(workspace) + os.sep):
            sys.exit(f'refusing: {path} is inside the workspace {workspace}')
    sides = list(args.sides)
    config = {'argv': sys.argv if argv is None else argv, 'mode': args.mode, 'warmups': args.warmups,
              'runs': args.runs, 'targets': args.targets, 'workspace': str(workspace),
              'binaries': {side: {'path': getattr(args, side), 'sha256': sha(getattr(args, side))}
                           for side in sides if getattr(args, side)},
              'lockfile_seed': {'path': args.lockfile_seed, 'sha256': sha(args.lockfile_seed)}
              if args.lockfile_seed else None,
              'lockfile_mode': 'update' if args.lockfile_seed else args.lockfile_mode}

    if args.compare_only:
        previous = out / 'report.json'
        canaries, previous_runs, warmups = {}, None, args.warmups
        if previous.exists():
            old = json.loads(previous.read_text())
            canaries = {k: old[k] for k in ('canary_before', 'canary_after', 'canary_unchanged') if k in old}
            config = old.get('config', config)
            previous_runs = old.get('runs')
            warmups = config.get('warmups', warmups)
        report = build_report(out, config, caches, sides, warmups, canaries, previous_runs)
    else:
        if not args.targets:
            sys.exit('no targets')
        lockfile_problem = lockfile_mode_problem(args)
        if lockfile_problem:
            sys.exit(lockfile_problem)
        for side in sides:
            if not getattr(args, side):
                sys.exit(f'--{side} is required')
        out.mkdir(parents=True, exist_ok=True)
        common = parse_env(args.extra_env)
        env = {'a': {**common, **parse_env(args.extra_env_a)}, 'b': {**common, **parse_env(args.extra_env_b)}}
        before = canary(workspace)
        total = args.warmups + args.runs
        for i in range(total):
            for side in sides:
                tag = f'{side}-{i}'
                record = run_one(getattr(args, side), caches[side], out, side_args(args, out, side),
                                 env[side], tag, workspace, args.timeout)
                record['warmup'] = i < args.warmups
                (out / f'{tag}.json').write_text(json.dumps(record, indent=1))
                print(f'{side} run {i}{" (warm-up)" if record["warmup"] else ""}: rc={record["rc"]} '
                      f'wall={record["wall"]:.1f}s user={record["user"]:.1f}s sys={record["sys"]:.1f}s '
                      f'maxrss={record["maxrss_kb"] // 1024}MB', flush=True)
        after = canary(workspace)
        canaries = {'canary_before': before, 'canary_after': after, 'canary_unchanged': before == after}
        if args.lockfile_seed:
            lock_dir = Path(args.lockfile_dir or out)
            config['lockfiles_after'] = {side: sha(lock_dir / f'lock-{side}') for side in sides}
        report = build_report(out, config, caches, sides, args.warmups, canaries)
    (out / 'report.json').write_text(json.dumps(report, indent=1))
    print(json.dumps(brief(report), indent=1))
    sys.exit(0 if report['equivalent'] is not False else 1)


if __name__ == '__main__':
    main()
