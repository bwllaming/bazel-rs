#!/usr/bin/env python3
"""Summarize the analysis snapshot's soak logs of one cache (L2-W5h, L2-W5i).

usage: snapshot_soak_report.py [--json] [--workspace DIR] CACHE_DIR

Reads `<cache>/snapshots/events.log`, `verify.log` and `verify-details.log`
(each after its rotated generation `<name>.1`; crate::snapshot::telemetry
writes them, docs/ANALYSIS_SNAPSHOT.md "Soak") and the snapshot store
`<cache>/snapshots/v1/` (and `mismatched/`, the snapshots verify mismatches
set aside), and prints:

  * the number of events per mode and kind (hit, miss, saved, not-saved,
    verified, mismatch, healed, evicted, recorded), misses and unsaved runs
    by reason;
  * the snapshot keys (one per command line), and per key: its lookups once a
    snapshot of it exists (saved, hit, or read by a lookup that missed on an
    input — perhaps saved before the log began), the hits, the misses for a
    changed input (a file, an environment variable, a tree, a local
    repository), the misses for no input at all (`absent` right after a save
    with no removal since, `unreadable`, `capture`, `panic`, `other`), and
    the hit rate on unchanged re-runs = hits / (hits + those); an `absent`
    miss after the snapshot was evicted, healed, set aside by a verify
    mismatch or removed for a noisy status command is counted apart, and so
    is one while none is saved (before the first save, or again after a
    lookup found none);
  * the hit rate on unchanged re-runs over all keys;
  * keys never saved (no save, hit or snapshot read), with their not-saved
    reasons — expected when every run was tainted (`network`, ...) or failed;
  * verified hits by command (verify mode);
  * always-miss patterns: at least three consecutive misses of one key with
    the same reason and path and no hit between them — a save ends the run
    when the misses name an input you may have changed (a workspace or host
    path, or `env`, `replay`, ...: editing one BUILD file three times is no
    pattern), not when they name a path in the cache or an output tree or no
    input at all (`absent` after each save, `unreadable`, ...) — split into
    SUSPECT (a path in the cache or an output tree, or none) and likely your
    edits (a workspace or host path);
  * heals by reason (`<rule>:<first differing entry>`) with their paths;
  * every verify.log mismatch line, verbatim, each with its verify-details.log
    block (where the snapshot went and every difference; L2-W5i);
  * hit latency (the attempt's wall time): p50 and p95, per mode and command;
  * the store: snapshot files and bytes, those set aside in `mismatched/`, the
    logs' sizes.

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
VERIFY_DETAILS_LOG = 'verify-details.log'
# Lines of a verify-details.log block shown in the text report (all in --json).
DETAILS_SHOWN = 40
# Miss classes that name a changed input (crate::snapshot::telemetry::classify_miss).
INPUT_CHANGES = {'env', 'read', 'readdir', 'stat', 'lstat', 'canon', 'readlink', 'integrity', 'tree',
                 'replay', 'local-tree', 'status-stderr'}
# Miss classes that name no input at all (with `absent` after a save and no removal: unexplained).
NO_INPUT = {'absent', 'unreadable', 'capture', 'panic', 'other'}
# Events that remove the key's snapshot (a later `absent` miss is explained).
REMOVALS = {'healed', 'evicted'}
# Path components of output trees (Places).
OUTPUT_TREES = {'bazel-out', 'bazel-bin', 'bazel-testlogs', 'bazel-genfiles', 'bazel-rs-out'}
# Not-saved classes worth reporting when a key is never saved (every taint and `failed` are expected:
# the guide's "not problems", e.g. `network`).
UNEXPECTED_NOT_SAVED = {'capture', 'too-large', 'incomplete', 'other'}


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


def snapshot_files(directory):
    """The `.snap` files below `directory` (sorted) and their bytes."""
    files, total = [], 0
    if directory.is_dir():
        for entry in sorted(directory.iterdir()):
            if entry.name.endswith('.snap') and entry.is_file():
                files.append(entry.name)
                total += entry.stat().st_size
    return files, total


def store_size(snapshots):
    """Snapshot files and bytes below `<cache>/snapshots/v1`, those set aside in
    `mismatched/` (L2-W5i), and the logs' sizes."""
    files, total = snapshot_files(snapshots / 'v1')
    aside, aside_bytes = snapshot_files(snapshots / 'mismatched')
    logs = {}
    for name in (EVENTS_LOG, VERIFY_LOG, VERIFY_DETAILS_LOG):
        for generation in (name, name + '.1'):
            path = snapshots / generation
            if path.is_file():
                logs[generation] = path.stat().st_size
    return {'snapshots': len(files), 'bytes': total, 'logs': logs,
            'mismatched': {'snapshots': aside, 'bytes': aside_bytes}}


def parse_details(lines):
    """verify-details.log blocks: each a header (the mismatch's verify.log line)
    and its lines (indented by two spaces in the log)."""
    blocks = []
    for line in lines:
        if line.startswith('  ') and blocks:
            blocks[-1]['lines'].append(line[2:])
        elif line.strip():
            blocks.append({'header': line, 'lines': []})
    return blocks


class Places:
    """Where a miss's path lies (L2-W5i): `cache` (below the cache dir, as given
    or canonical), `output` (an output tree: a `bazel-*` entry of the workspace
    — Bazel's convenience links, `bazel-rs-out` — or any path through a
    `bazel-out`, `bazel-bin`, `bazel-testlogs`, `bazel-genfiles` or
    `bazel-rs-out` component or genuine Bazel's output user root `_bazel_<user>`),
    `none` (the miss names no path) or `elsewhere` (the workspace or the host)."""

    def __init__(self, cache=None, workspace=None):
        def spellings(path):
            return {os.path.abspath(path), os.path.realpath(path)} if path else set()
        self.cache = spellings(cache)
        if workspace is None and cache and os.path.basename(os.path.abspath(cache)) == 'bazel-rs-out':
            # The default cache, `<workspace>/bazel-rs-out`.
            workspace = os.path.dirname(os.path.abspath(cache))
        self.workspace = spellings(workspace)

    def of(self, path):
        if path in ('', '-'):
            return 'none'
        if any(path == root or path.startswith(root.rstrip('/') + '/') for root in self.cache):
            return 'cache'
        components = path.split('/')
        if any(c in OUTPUT_TREES or c.startswith('_bazel_') for c in components):
            return 'output'
        for root in self.workspace:
            prefix = root.rstrip('/') + '/'
            if path.startswith(prefix) and path[len(prefix):].split('/', 1)[0].startswith('bazel-'):
                return 'output'
        return 'elsewhere'


def ends_on_save(reason, place):
    """Whether a save ends a run of misses: when they name an input you may
    have changed — a workspace or host path, or an input without a path
    (`env`, `replay`, ...): the save took the change in, and the next miss is
    the next change (editing one BUILD file three times is miss, save three
    times, no pattern). Not when they name a path in the cache or an output
    tree, or no input at all (`absent`, `unreadable`, `capture`, `panic`,
    `other`): such a miss coming back after every save is the command's own
    doing (a tool writing into a repository tree), the always-miss the soak
    watches for."""
    if place in ('cache', 'output'):
        return False
    return reason not in NO_INPUT


def analyze(events, verify_lines, store=None, details=(), places=None):
    """The report of `events` (parsed events.log lines, oldest first), the raw
    `verify_lines` and the verify-details.log `details` blocks, as a dict (see
    the module docs); `places` (a Places) classifies the paths misses name."""
    places = places or Places()
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
        if event.get('command', '-') != '-' and info['command'] is None and event.get('kind') != 'evicted':
            info['command'] = unescape(event['command'])
            info['targets'] = event.get('targets', [])
        info['events'].append(event)
    always_miss = []
    for info in keys.values():
        # What the log shows of the key's snapshot: `unknown` (nothing yet:
        # the log may begin after its save), `present` (saved, hit, or read by
        # a lookup that missed on an input), `removed` (healed, evicted, set
        # aside by a verify mismatch, removed for a noisy status command) or
        # `gone` (a lookup found none).
        state = 'unknown'
        run = None

        def close():
            nonlocal run
            if run and run['count'] >= 3:
                always_miss.append(run)
            run = None

        for event in info['events']:
            kind = event.get('kind')
            reason = event.get('reason', '-')
            if kind == 'saved':
                info['saves'] += 1
                state = 'present'
                if run and ends_on_save(run['reason'], run['place']):
                    close()
                elif run:
                    run['saves'] += 1
            elif kind == 'not-saved':
                info['not_saved'][reason] += 1
            elif kind in REMOVALS or (kind == 'mismatch' and reason != 'cannot-verify'):
                state = 'removed'
            elif kind == 'hit':
                state = 'present'
                info['lookups'] += 1
                info['hits'] += 1
                close()
            elif kind == 'miss':
                if reason != 'absent':
                    # The lookup read the key's snapshot (perhaps saved
                    # before the log began).
                    state = 'present'
                if state in ('unknown', 'gone'):
                    # Not saved (yet, or since a lookup found none).
                    info['before_save'] += 1
                    state = 'gone'
                    close()
                    continue
                info['lookups'] += 1
                if reason == 'absent' and state == 'removed':
                    info['after_removal'] += 1
                    state = 'gone'
                    close()
                    continue
                if reason in INPUT_CHANGES:
                    info['changed'] += 1
                else:
                    info['unexplained'] += 1
                if reason == 'status-stderr':
                    state = 'removed'
                elif reason == 'absent':
                    state = 'gone'
                path = unescape(event.get('path', '-'))
                if run and (run['reason'], run['path']) == (reason, path):
                    run['count'] += 1
                    run['last'] = event['time']
                    run['detail'] = unescape(event.get('detail', '-'))
                else:
                    close()
                    place = places.of(path)
                    run = {'key': info['key'], 'command': info['command'], 'targets': info['targets'],
                           'reason': reason, 'path': path, 'place': place,
                           'class': 'suspect' if place in ('cache', 'output', 'none') else 'likely-your-edits',
                           'count': 1, 'saves': 0, 'first': event['time'], 'last': event['time'],
                           'detail': unescape(event.get('detail', '-'))}
        close()
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
    # Keys seen only in evictions (snapshots saved before the log began, or
    # by another binary) are counted, not listed.
    evicted_only = [info for info in keys.values()
                    if all(e.get('kind') == 'evicted' for e in info['events'])]
    listed = [info for info in keys.values() if info not in evicted_only]
    # Keys never saved: no save, no hit, no lookup that read a snapshot.
    never_saved = [
        {'key': info['key'], 'command': info['command'], 'targets': info['targets'],
         'lookups': info['before_save'], 'not_saved': dict(info['not_saved'].most_common()),
         'expected': bool(info['not_saved']) and not (set(info['not_saved']) & UNEXPECTED_NOT_SAVED)}
        for info in listed if info['saves'] == 0 and info['lookups'] == 0
    ]
    hits = sum(info['hits'] for info in listed)
    unchanged = sum(info['unchanged'] for info in listed)
    report = {
        'events': len(events),
        'evicted_only_keys': len(evicted_only),
        'span': [events[0]['time'], events[-1]['time']] if events else None,
        'counts': {f'{mode}/{kind}': n for (mode, kind), n in sorted(counts.items())},
        'misses': dict(misses.most_common()),
        'not_saved': dict(not_saved.most_common()),
        'keys': [
            {name: value for name, value in info.items() if name != 'events'}
            | {'not_saved': dict(info['not_saved'])}
            for info in listed
        ],
        'hit_rate_unchanged': {'hits': hits, 'unchanged': unchanged,
                               'rate': (hits / unchanged) if unchanged else None},
        'never_saved': never_saved,
        'verified_by_command': dict(collections.Counter(
            unescape(e.get('command', '-')) for e in events if e.get('kind') == 'verified')),
        'always_miss': always_miss,
        'heals': dict(collections.Counter(e.get('reason', '-') for e in heals)),
        'heal_lines': [
            {'time': e['time'], 'key': e.get('key'), 'reason': e.get('reason'), 'path': unescape(e.get('path', '-')),
             'command': unescape(e.get('command', '-')), 'ms': milliseconds(e.get('ms'))}
            for e in heals
        ],
        'verify_mismatches': list(verify_lines),
        'verify_details': pair_details(verify_lines, details),
        'hit_latency_ms': {
            f'{mode}/{command}': {'hits': len(values), 'p50': percentile(values, 0.5),
                                  'p95': percentile(values, 0.95)}
            for (mode, command), values in sorted(latencies.items())
        },
        'store': store,
    }
    return report


def pair_details(verify_lines, details):
    """Each verify.log line's verify-details.log block (by its header: the same
    line), in verify.log order: `{'line', 'lines'}` (`lines` None when the block
    is missing: written by an older binary, or rotated away); then the blocks
    whose verify.log line is missing (rotated away), with `line` their header."""
    pending = collections.defaultdict(collections.deque)
    for block in details:
        pending[block['header']].append(block)
    paired = []
    for line in verify_lines:
        block = pending[line].popleft() if pending.get(line) else None
        paired.append({'line': line, 'lines': block['lines'] if block else None})
    for header, blocks in pending.items():
        for block in blocks:
            paired.append({'line': header, 'lines': block['lines'], 'only_details': True})
    return paired


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
    lines.append(f"keys: {len(report['keys'])}" + (
        f" (and {report['evicted_only_keys']} seen only when garbage collection evicted them)"
        if report['evicted_only_keys'] else ''))
    overall = report['hit_rate_unchanged']
    lines.append('hit rate on unchanged re-runs, all keys: '
                 + (f"{100 * overall['rate']:.1f}% ({overall['hits']}/{overall['unchanged']})"
                    if overall['rate'] is not None else '-'))
    lines.append('  per key (lookups once a snapshot exists; changed = a miss naming an input; unexplained = '
                 'absent after a save without a removal, unreadable, capture, panic, other; before a save = '
                 'absent while none was saved):')
    for info in sorted(report['keys'], key=lambda info: -info['lookups']):
        targets = ' '.join(info['targets'])
        lines.append(
            f"  {info['key']} {info['command'] or '-'} {targets}: lookups {info['lookups']}, hits {info['hits']}, "
            f"changed {info['changed']}, unexplained {info['unexplained']}, after removal {info['after_removal']}, "
            f"before a save {info['before_save']}, saves {info['saves']}; hit rate on unchanged re-runs "
            + (f"{100 * info['hit_rate']:.1f}% ({info['hits']}/{info['unchanged']})"
               if info['hit_rate'] is not None else '-')
            + (('; not saved: ' + ', '.join(f'{k} {v}' for k, v in info['not_saved'].items()))
               if info['not_saved'] else ''))
    lines.append('')
    never = report['never_saved']
    lines.append(f"keys never saved (no save, hit or snapshot read in the log): {len(never)}")
    if never:
        lines.append('  expected when every run was tainted - `network` (a remote registry\'s cold run, '
                     '`--lockfile_mode=off`), `repository-rule-executed` (after a fetch), `extension-evaluated`, '
                     '`lockfile-write-pending` (the run after a lockfile change), ... - or failed; worth reporting: '
                     '`capture`, `too-large`, `incomplete`, `other`, or no not-saved event at all')
    for info in never:
        reasons = ', '.join(f'{k} {v}' for k, v in info['not_saved'].items()) or 'no not-saved event'
        lines.append(f"  {info['key']} {info['command'] or '-'} {' '.join(info['targets'])}: lookups {info['lookups']}, "
                     f"not saved: {reasons} [{'expected' if info['expected'] else 'please report'}]")
    lines.append('')
    verified = report['verified_by_command']
    lines.append('verified hits by command: ' + (', '.join(f'{k} {v}' for k, v in sorted(verified.items()))
                                                 if verified else 'none'))
    lines.append('')
    runs = report['always_miss']
    suspect = [run for run in runs if run['class'] == 'suspect']
    yours = [run for run in runs if run['class'] != 'suspect']
    lines.append(f"always-miss patterns (>= 3 consecutive misses of one key, same reason and path, no hit between; "
                 f"a save ends the run unless it names a path in the cache or an output tree or no input): "
                 f"SUSPECT {len(suspect)}, likely your edits {len(yours)}")
    for title, group in (('SUSPECT (a path in the cache or an output tree, or none: something other than your '
                          'edits keeps changing what the snapshot recorded - please report)', suspect),
                         ('likely your edits (a workspace or host path, no save between: e.g. a BUILD file you '
                          'edited while the command kept failing)', yours)):
        if not group:
            continue
        lines.append('  ' + title + ':')
        for run in group:
            lines.append(f"    {run['key']} {run['command'] or '-'} {' '.join(run['targets'])}: {run['count']} x "
                         f"{run['reason']} {run['path']} [{run['place']}]"
                         + (f", {run['saves']} save(s) between" if run['saves'] else '')
                         + f" ({run['first']} .. {run['last']}); last: {run['detail']}")
    lines.append('')
    heals = report['heals']
    lines.append('heals by reason: ' + (', '.join(f'{k} {v}' for k, v in sorted(heals.items())) if heals else 'none'))
    for heal in report['heal_lines']:
        lines.append(f"  {heal['time']} {heal['key']} {heal['command']} {heal['reason']} {heal['path']} "
                     f"({number(heal['ms'])} ms)")
    lines.append('')
    lines.append(f"verify mismatches (verify.log, verbatim, each with its verify-details.log block): "
                 f"{len(report['verify_mismatches'])}")
    for entry in report['verify_details']:
        lines.append('  ' + entry['line'] + (' (verify.log line rotated away)' if entry.get('only_details') else ''))
        if entry['lines'] is None:
            lines.append('    (no verify-details.log block: an older binary, or rotated away)')
            continue
        for detail in entry['lines'][:DETAILS_SHOWN]:
            lines.append('    ' + detail)
        if len(entry['lines']) > DETAILS_SHOWN:
            lines.append(f"    ... ({len(entry['lines']) - DETAILS_SHOWN} more lines in verify-details.log)")
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
        aside = store.get('mismatched') or {'snapshots': [], 'bytes': 0}
        lines.append(f"set aside by verify mismatches (snapshots/mismatched/, the 3 newest kept): "
                     f"{len(aside['snapshots'])} snapshot(s), {aside['bytes']} bytes"
                     + (': ' + ', '.join(aside['snapshots']) if aside['snapshots'] else ''))
    return '\n'.join(lines) + '\n'


def load(cache):
    """`(events, verify_lines, store, details)` of the cache's logs and store."""
    snapshots = Path(cache) / 'snapshots'
    events = [e for e in (parse_line(l) for l in read_generations(snapshots, EVENTS_LOG)) if e is not None]
    verify_lines = [l for l in read_generations(snapshots, VERIFY_LOG) if l.strip()]
    details = parse_details(read_generations(snapshots, VERIFY_DETAILS_LOG))
    return events, verify_lines, store_size(snapshots), details


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--json', action='store_true', help='print the report as JSON')
    ap.add_argument('--workspace', help='the workspace of the cache (its `bazel-*` entries are output trees; '
                                        'default: the parent of a cache named bazel-rs-out)')
    ap.add_argument('cache')
    a = ap.parse_args(argv)
    events, verify_lines, store, details = load(a.cache)
    report = analyze(events, verify_lines, store, details, Places(a.cache, a.workspace))
    if a.json:
        sys.stdout.write(json.dumps(report, indent=1, default=str) + '\n')
    else:
        sys.stdout.write(render(report, os.path.abspath(a.cache)))
    return 0 if (events or verify_lines) else 2


if __name__ == '__main__':
    sys.exit(main())
