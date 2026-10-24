#!/usr/bin/env python3
"""Summarize the analysis snapshot's soak logs of one cache (L2-W5h).

usage: snapshot_soak_report.py [--json] CACHE_DIR

Reads `<cache>/snapshots/events.log` and `verify.log` (each after its rotated
generation `<name>.1`; crate::snapshot::telemetry writes both, docs/
ANALYSIS_SNAPSHOT.md "Soak telemetry") and the snapshot store
`<cache>/snapshots/v1/`, and prints:

  * the number of events per mode and kind (hit, miss, saved, not-saved,
    verified, mismatch, healed, evicted, recorded), misses and unsaved runs
    by reason;
  * the snapshot keys (one per command line), and per key: its lookups after
    the key was first saved (or first hit), the hits, the misses for a
    changed input (a file, an environment variable, a tree, a local
    repository), the misses for no input at all (`absent` without a removal
    of the snapshot since its save, `unreadable`, `capture`, `panic`,
    `other`), and the hit rate on unchanged re-runs = hits / (hits + those);
    an `absent` miss after the snapshot was evicted, healed, removed by a
    verify mismatch or by a noisy status command is counted apart;
  * verified hits by command (verify mode);
  * always-miss patterns: at least three consecutive misses of one key with
    the same reason and path and no hit between them;
  * heals by reason (`<rule>:<first differing entry>`) with their paths;
  * every verify.log mismatch line, verbatim;
  * hit latency (the attempt's wall time): p50 and p95, per mode and command;
  * the store: snapshot files and bytes, the logs' sizes.

`--json` prints the same as one JSON object. Exit status 0 (2 when the cache
has no snapshot logs).
"""
import argparse
import collections
import json
import math
import os
import sys
from pathlib import Path

EVENTS_LOG = 'events.log'
VERIFY_LOG = 'verify.log'
# Miss classes that name a changed input (crate::snapshot::telemetry::classify_miss).
INPUT_CHANGES = {'env', 'read', 'readdir', 'stat', 'lstat', 'canon', 'readlink', 'integrity', 'tree',
                 'replay', 'local-tree', 'status-stderr'}
# Events that remove the key's snapshot (a later `absent` miss is explained).
REMOVALS = {'healed', 'evicted'}


def unescape(token):
    """crate::observed_fs::escape: `\\xNN` for every non-graphic byte and `\\`."""
    if token in ('', '-'):
        return token
    out = bytearray()
    raw = token.encode('utf-8', 'surrogateescape')
    i = 0
    while i < len(raw):
        if raw[i:i + 2] == b'\\x' and i + 4 <= len(raw):
            try:
                out.append(int(raw[i + 2:i + 4], 16))
                i += 4
                continue
            except ValueError:
                pass
        out.append(raw[i])
        i += 1
    return out.decode('utf-8', 'replace')


def parse_line(line):
    """One log line: `<UTC time> <unix seconds> name=value ... targets=<pattern>...`
    as a dict of its (escaped) fields plus `time`, `seconds` and `targets`
    (unescaped patterns); None for a line that is no such line."""
    parts = line.rstrip('\n').split(' ')
    if len(parts) < 3 or not parts[1].lstrip('-').isdigit():
        return None
    entry = {'time': parts[0], 'seconds': int(parts[1]), 'targets': []}
    for i in range(2, len(parts)):
        part = parts[i]
        if part.startswith('targets='):
            patterns = [part[len('targets='):]] + parts[i + 1:]
            entry['targets'] = [unescape(p) for p in patterns if p]
            break
        name, sep, value = part.partition('=')
        if not sep:
            return None
        entry[name] = value
    return entry


def read_generations(directory, name):
    """The lines of `<name>.1` then `<name>` (oldest first)."""
    lines = []
    for path in (directory / (name + '.1'), directory / name):
        try:
            lines.extend(path.read_text(encoding='utf-8', errors='surrogateescape').splitlines())
        except OSError:
            pass
    return lines


def milliseconds(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def percentile(values, fraction):
    """Nearest-rank percentile of `values` (None when empty)."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def store_size(snapshots):
    """Snapshot files and bytes below `<cache>/snapshots/v1`, and the logs' sizes."""
    files = total = 0
    v1 = snapshots / 'v1'
    if v1.is_dir():
        for entry in v1.iterdir():
            if entry.name.endswith('.snap') and entry.is_file():
                files += 1
                total += entry.stat().st_size
    logs = {}
    for name in (EVENTS_LOG, EVENTS_LOG + '.1', VERIFY_LOG, VERIFY_LOG + '.1'):
        path = snapshots / name
        if path.is_file():
            logs[name] = path.stat().st_size
    return {'snapshots': files, 'bytes': total, 'logs': logs}


def analyze(events, verify_lines, store=None):
    """The report of `events` (parsed events.log lines, oldest first) and the
    raw `verify_lines`, as a dict (see the module docs)."""
    counts = collections.Counter((e.get('mode', '-'), e.get('kind', '-')) for e in events)
    misses = collections.Counter(e.get('reason', '-') for e in events if e.get('kind') == 'miss')
    not_saved = collections.Counter(e.get('reason', '-') for e in events if e.get('kind') == 'not-saved')
    keys = collections.OrderedDict()
    for event in events:
        key = event.get('key', '-')
        if key == '-' or event.get('kind') == 'recorded':
            continue
        info = keys.setdefault(key, {'key': key, 'command': None, 'targets': [], 'lookups': 0, 'hits': 0,
                                     'changed': 0, 'unexplained': 0, 'after_removal': 0, 'before_save': 0,
                                     'saves': 0, 'not_saved': collections.Counter(), 'events': []})
        if event.get('command', '-') != '-' and info['command'] is None:
            info['command'] = unescape(event['command'])
            info['targets'] = event.get('targets', [])
        info['events'].append(event)
    always_miss = []
    for info in keys.values():
        saved = removed = False
        run = None
        for event in info['events']:
            kind = event.get('kind')
            reason = event.get('reason', '-')
            if kind == 'saved':
                info['saves'] += 1
                saved, removed = True, False
            elif kind == 'not-saved':
                info['not_saved'][reason] += 1
            elif kind in REMOVALS or (kind == 'mismatch' and reason != 'cannot-verify'):
                removed = True
            if kind in ('hit', 'miss'):
                if kind == 'hit':
                    # A hit implies a save (perhaps before the log began).
                    saved = True
                    info['lookups'] += 1
                    info['hits'] += 1
                    if run and run['count'] >= 3:
                        always_miss.append(run)
                    run = None
                    continue
                if saved:
                    info['lookups'] += 1
                    if reason in INPUT_CHANGES:
                        info['changed'] += 1
                    elif reason == 'absent' and removed:
                        info['after_removal'] += 1
                    else:
                        info['unexplained'] += 1
                else:
                    info['before_save'] += 1
                if reason == 'status-stderr':
                    removed = True
                signature = (reason, event.get('path', '-'))
                if run and run['signature'] == signature:
                    run['count'] += 1
                    run['last'] = event['time']
                else:
                    if run and run['count'] >= 3:
                        always_miss.append(run)
                    run = {'key': info['key'], 'command': info['command'], 'targets': info['targets'],
                           'signature': signature, 'count': 1, 'first': event['time'], 'last': event['time']}
        if run and run['count'] >= 3:
            always_miss.append(run)
        unchanged = info['hits'] + info['unexplained']
        info['unchanged'] = unchanged
        info['hit_rate'] = (info['hits'] / unchanged) if unchanged else None
    heals = [e for e in events if e.get('kind') == 'healed']
    latencies = collections.defaultdict(list)
    for event in events:
        if event.get('kind') == 'hit':
            ms = milliseconds(event.get('ms'))
            if ms is not None:
                latencies[('all', 'all')].append(ms)
                latencies[(event.get('mode', '-'), unescape(event.get('command', '-')))].append(ms)
    report = {
        'events': len(events),
        'span': [events[0]['time'], events[-1]['time']] if events else None,
        'counts': {f'{mode}/{kind}': n for (mode, kind), n in sorted(counts.items())},
        'misses': dict(misses.most_common()),
        'not_saved': dict(not_saved.most_common()),
        'keys': [
            {name: value for name, value in info.items() if name != 'events'}
            | {'not_saved': dict(info['not_saved'])}
            for info in keys.values()
        ],
        'verified_by_command': dict(collections.Counter(
            unescape(e.get('command', '-')) for e in events if e.get('kind') == 'verified')),
        'always_miss': [
            {'key': run['key'], 'command': run['command'], 'targets': run['targets'], 'reason': run['signature'][0],
             'path': unescape(run['signature'][1]), 'count': run['count'], 'first': run['first'], 'last': run['last']}
            for run in always_miss
        ],
        'heals': dict(collections.Counter(e.get('reason', '-') for e in heals)),
        'heal_lines': [
            {'time': e['time'], 'key': e.get('key'), 'reason': e.get('reason'), 'path': unescape(e.get('path', '-')),
             'command': unescape(e.get('command', '-')), 'ms': milliseconds(e.get('ms'))}
            for e in heals
        ],
        'verify_mismatches': list(verify_lines),
        'hit_latency_ms': {
            f'{mode}/{command}': {'hits': len(values), 'p50': percentile(values, 0.5),
                                  'p95': percentile(values, 0.95)}
            for (mode, command), values in sorted(latencies.items())
        },
        'store': store,
    }
    return report


def number(value, digits=1):
    return '-' if value is None else f'{value:.{digits}f}'


def render(report, cache):
    lines = [f'analysis snapshot soak report: {cache}']
    span = report['span']
    lines.append(f"events: {report['events']}" + (f' ({span[0]} .. {span[1]})' if span else ''))
    lines.append('')
    lines.append('events per mode/kind:')
    for name, n in report['counts'].items():
        lines.append(f'  {name:24} {n}')
    if report['misses']:
        lines.append('misses by reason: ' + ', '.join(f'{k} {v}' for k, v in report['misses'].items()))
    if report['not_saved']:
        lines.append('not saved by reason: ' + ', '.join(f'{k} {v}' for k, v in report['not_saved'].items()))
    lines.append('')
    lines.append(f"keys: {len(report['keys'])}")
    lines.append('  hit rate on unchanged re-runs per key (lookups after the first save; changed = a miss naming '
                 'an input; unexplained = absent without a removal, unreadable, capture, panic, other):')
    for info in sorted(report['keys'], key=lambda info: -info['lookups']):
        targets = ' '.join(info['targets'])
        lines.append(
            f"  {info['key']} {info['command'] or '-'} {targets}: lookups {info['lookups']}, hits {info['hits']}, "
            f"changed {info['changed']}, unexplained {info['unexplained']}, after removal {info['after_removal']}, "
            f"before the first save {info['before_save']}, saves {info['saves']}; hit rate on unchanged re-runs "
            + (f"{100 * info['hit_rate']:.1f}% ({info['hits']}/{info['unchanged']})"
               if info['hit_rate'] is not None else '-')
            + (('; not saved: ' + ', '.join(f'{k} {v}' for k, v in info['not_saved'].items()))
               if info['not_saved'] else ''))
    lines.append('')
    verified = report['verified_by_command']
    lines.append('verified hits by command: ' + (', '.join(f'{k} {v}' for k, v in sorted(verified.items()))
                                                 if verified else 'none'))
    lines.append('')
    lines.append(f"always-miss patterns (>= 3 consecutive misses, same reason and path, no hit between): "
                 f"{len(report['always_miss'])}")
    for run in report['always_miss']:
        lines.append(f"  {run['key']} {run['command'] or '-'} {' '.join(run['targets'])}: {run['count']} x "
                     f"{run['reason']} {run['path']} ({run['first']} .. {run['last']})")
    lines.append('')
    heals = report['heals']
    lines.append('heals by reason: ' + (', '.join(f'{k} {v}' for k, v in sorted(heals.items())) if heals else 'none'))
    for heal in report['heal_lines']:
        lines.append(f"  {heal['time']} {heal['key']} {heal['command']} {heal['reason']} {heal['path']} "
                     f"({number(heal['ms'])} ms)")
    lines.append('')
    lines.append(f"verify mismatches (verify.log, verbatim): {len(report['verify_mismatches'])}")
    for line in report['verify_mismatches']:
        lines.append('  ' + line)
    lines.append('')
    lines.append('hit latency (attempt wall, ms):')
    for name, stats in report['hit_latency_ms'].items():
        lines.append(f"  {name:24} hits {stats['hits']}, p50 {number(stats['p50'])}, p95 {number(stats['p95'])}")
    if not report['hit_latency_ms']:
        lines.append('  no hits')
    store = report['store']
    if store is not None:
        lines.append('')
        logs = ', '.join(f'{k} {v} bytes' for k, v in sorted(store['logs'].items())) or 'none'
        lines.append(f"store: {store['snapshots']} snapshot(s), {store['bytes']} bytes "
                     f"({store['bytes'] / (1 << 20):.1f} MiB); logs: {logs}")
    return '\n'.join(lines) + '\n'


def load(cache):
    snapshots = Path(cache) / 'snapshots'
    events = [e for e in (parse_line(l) for l in read_generations(snapshots, EVENTS_LOG)) if e is not None]
    verify_lines = [l for l in read_generations(snapshots, VERIFY_LOG) if l.strip()]
    return events, verify_lines, store_size(snapshots)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--json', action='store_true', help='print the report as JSON')
    ap.add_argument('cache')
    a = ap.parse_args(argv)
    events, verify_lines, store = load(a.cache)
    report = analyze(events, verify_lines, store)
    if a.json:
        sys.stdout.write(json.dumps(report, indent=1, default=str) + '\n')
    else:
        sys.stdout.write(render(report, os.path.abspath(a.cache)))
    return 0 if (events or verify_lines) else 2


if __name__ == '__main__':
    sys.exit(main())
