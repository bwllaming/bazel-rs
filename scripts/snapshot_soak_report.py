#!/usr/bin/env python3
"""Summarize the analysis snapshot's soak logs of one workspace (L2-W5h, L2-W5i).

usage: snapshot_soak_report.py [--json] [--workspace DIR] [--archive DIR] CACHE_DIR

The snapshot store of a workspace is `<cache>/worktrees/<name>-<hash>/snapshots/`,
its state directory in the cache (docs/WORKTREES.md: the git worktrees sharing a
cache each have their own): `--workspace DIR` names the workspace (default: the
parent of a cache named bazel-rs-out — the main checkout of the default cache —
else the one workspace that used the cache); a cache of the flat layout before
per-worktree state keeps it in `<cache>/snapshots/`.

Reads the store's `events.log`, `verify.log` and `verify-details.log`
(each after its rotated generation `<name>.1`; crate::snapshot::telemetry
writes them, docs/ANALYSIS_SNAPSHOT.md "Soak") and its snapshots, `v1/`
(and `mismatched/`, the snapshots verify mismatches set aside), and prints:

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
  * verified hits by command (verify mode), and verify-mode hits by outcome:
    verified, hit then mismatch (by class; no clean hit: counted apart from
    the hits and out of the hit rates, L2-W5i), not concluded;
  * always-miss patterns: at least three consecutive misses of one key with
    the same reason and path and no hit between them — a save ends the run
    when the misses name an input you may have changed (a workspace or host
    path, or `env`, `replay`, ...: editing one BUILD file three times is no
    pattern), not when they name a path in the cache or an output tree or no
    input at all (`absent` after each save, `unreadable`, ...) — split into
    SUSPECT (a path in the cache or an output tree, or none) and likely your
    edits (a workspace or host path);
  * heals by reason (`<rule>:<first differing entry>`) with their paths, and
    the failed hits that kept their snapshot (`heal-kept`, L2-W5i);
  * every verify.log mismatch line, verbatim, each with its verify-details.log
    block (where the snapshot went and every difference; L2-W5i);
  * hit latency (the attempt's wall time): p50 and p95, per mode and command;
  * the local-tree check of the hits with local repositories (`local:` of the
    hit's detail, L2-W5i): p50, p95 and max, and on how many it took longer
    than the replay it runs beside;
  * the store: snapshot files and bytes, those set aside in `mismatched/`, the
    logs' sizes.

  * the soak's exit check: each threshold of docs/ANALYSIS_SNAPSHOT.md "Soak"
    with its value here and `ok`, `not yet` or `look` (a person must judge it).

The cache is only read. `--archive DIR` (L2-W5i) first appends the store's log
lines that DIR does not hold yet to DIR/events.log, verify.log and
verify-details.log (never rotated) and reports over DIR: run it daily during a
soak, since the store's logs keep only ~7,500 events (one archive per
workspace). `--json` prints the same
as one JSON object. Exit status 0 (2 when there are no snapshot logs).
"""
import argparse
import collections
import hashlib
import json
import math
import os
import sys
import time
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
# The soak's exit thresholds (docs/ANALYSIS_SNAPSHOT.md "Soak", L2-W5i).
EXIT = {'days': 7, 'verified': 300, 'verified_keys': 20, 'verified_test': 50, 'verified_run': 10,
        'hit_rate': 0.90, 'local_above': 0.01, 'hit_p95_ms': 1000.0, 'store_bytes': 4 << 30}


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


def state_name(workspace):
    """The state directory name of `workspace` in a cache (crate::worktree::
    state_name): its canonical path's last component — ASCII letters, digits,
    `.`, `_` and `-` kept, anything else `_`, at most 40 characters; `root` when
    that leaves nothing or only dots — a dash and the first 8 bytes of the
    SHA-256 of the canonical path, in hex."""
    canonical = os.path.realpath(workspace)
    digest = hashlib.sha256(os.fsencode(canonical)).hexdigest()[:16]
    base = os.path.basename(canonical)
    try:
        base.encode('utf-8')
    except UnicodeEncodeError:
        base = ''
    name = ''.join(c if (c.isascii() and c.isalnum()) or c in '._-' else '_' for c in base)[:40]
    if not name or set(name) == {'.'}:
        name = 'root'
    return f'{name}-{digest}'


def snapshot_store(cache, workspace=None):
    """The snapshot store of `workspace` in `cache` (see the module docs): its
    state directory's `snapshots/`, or the flat layout's `<cache>/snapshots/`
    when only that exists. Without a workspace: the main checkout's for a cache
    named bazel-rs-out when it has one, else the only state directory that has
    a store. Exits when several workspaces used the cache and none was named."""
    cache = Path(cache)
    flat = cache / 'snapshots'
    named = workspace is not None
    if not named and os.path.basename(os.path.abspath(cache)) == 'bazel-rs-out':
        workspace = os.path.dirname(os.path.abspath(cache))
    if workspace is not None:
        own = cache / 'worktrees' / state_name(workspace) / 'snapshots'
        if own.is_dir() or (named and not flat.is_dir()):
            return own
        if flat.is_dir():
            return flat
    stores = sorted(p for p in (cache / 'worktrees').glob('*/snapshots') if p.is_dir())
    if len(stores) > 1:
        names = ', '.join(p.parent.name for p in stores)
        sys.exit(f'several workspaces used {cache} ({names}): name one with --workspace')
    return stores[0] if stores else flat


def store_size(snapshots):
    """Snapshot files and bytes below the store's `v1/`, those set aside in
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
                                     'mismatched_hits': collections.Counter(), 'changed': 0, 'unexplained': 0,
                                     'after_removal': 0, 'before_save': 0, 'saves': 0,
                                     'not_saved': collections.Counter(), 'events': []})
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
        # Verify-mode hits whose outcome (`verified`, `mismatch`) is still to
        # come: a hit then a mismatch is no clean hit (L2-W5i).
        verifying = 0

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
            elif kind in REMOVALS:
                state = 'removed'
            elif kind == 'mismatch':
                if reason != 'cannot-verify':
                    # Set aside (or removed): the next `absent` is explained.
                    state = 'removed'
                if verifying:
                    # The hit before it was no clean hit: counted apart, out
                    # of the hit rate (the mismatch is gated on its own).
                    verifying -= 1
                    info['hits'] -= 1
                    info['mismatched_hits'][reason] += 1
            elif kind == 'verified':
                verifying = max(0, verifying - 1)
            elif kind == 'hit':
                state = 'present'
                info['lookups'] += 1
                info['hits'] += 1
                if event.get('mode') == 'verify':
                    verifying += 1
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
    mismatched_hits = collections.Counter()
    for info in listed:
        mismatched_hits.update(info['mismatched_hits'])
    verify_hits = sum(1 for e in events if e.get('kind') == 'hit' and e.get('mode') == 'verify')
    verified = sum(1 for e in events if e.get('kind') == 'verified')
    report = {
        'events': len(events),
        'evicted_only_keys': len(evicted_only),
        'span': [events[0]['time'], events[-1]['time']] if events else None,
        'counts': {f'{mode}/{kind}': n for (mode, kind), n in sorted(counts.items())},
        'misses': dict(misses.most_common()),
        'not_saved': dict(not_saved.most_common()),
        'keys': [
            {name: value for name, value in info.items() if name != 'events'}
            | {'not_saved': dict(info['not_saved']), 'mismatched_hits': dict(info['mismatched_hits'])}
            for info in listed
        ],
        'verify_outcomes': {'hits': verify_hits, 'verified': verified,
                            'hit_then_mismatch': dict(mismatched_hits.most_common()),
                            'not_concluded': max(0, verify_hits - verified - sum(mismatched_hits.values()))},
        'local_tree': local_tree(events),
        'hit_rate_unchanged': {'hits': hits, 'unchanged': unchanged,
                               'rate': (hits / unchanged) if unchanged else None},
        'never_saved': never_saved,
        'verified_by_command': dict(collections.Counter(
            unescape(e.get('command', '-')) for e in events if e.get('kind') == 'verified')),
        'always_miss': always_miss,
        'heals': dict(collections.Counter(e.get('reason', '-') for e in heals)),
        'heal_kept': sum(1 for e in events if e.get('kind') == 'heal-kept'),
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
    report['exit_check'] = exit_check(report, events)
    return report


def detail_fields(detail):
    """A hit's `detail=` (`key:0.1,inputs:1.0,...,local:4.0,...`) as numbers."""
    fields = {}
    for part in unescape(detail or '-').split(','):
        name, sep, value = part.partition(':')
        if sep:
            try:
                fields[name] = float(value)
            except ValueError:
                pass
    return fields


def local_tree(events):
    """The local-tree check of every hit that has local repositories (L2-W5h
    logs `local:` beside `replay:`; L2-W5i): how long it took and on how many
    hits it took longer than the replay it runs beside (then it is on the
    attempt's critical path)."""
    pairs = []
    for event in events:
        if event.get('kind') != 'hit':
            continue
        fields = detail_fields(event.get('detail'))
        if 'local' in fields and 'replay' in fields and (fields['local'] > 0 or fields['replay'] > 0):
            pairs.append((fields['local'], fields['replay']))
    values = [local for local, _ in pairs]
    above = sum(1 for local, replay in pairs if local > replay)
    return {'hits': len(pairs), 'p50': percentile(values, 0.5), 'p95': percentile(values, 0.95),
            'max': max(values) if values else None, 'local_above_replay': above,
            'share_above': (above / len(pairs)) if pairs else None}


def detail_text(detail):
    """A `detail=` of `name:value,...` pairs as strings (`binary:` of a save)."""
    fields = {}
    for part in unescape(detail or '-').split(','):
        name, sep, value = part.partition(':')
        if sep:
            fields[name] = value
    return fields


def basename(path):
    return path.rstrip('/').rsplit('/', 1)[-1]


def exit_check(report, events):
    """The soak's exit thresholds (docs/ANALYSIS_SNAPSHOT.md "Soak") against
    this report: a list of `{'check', 'value', 'status'}`, status `ok`, `not
    yet` (soak on) or `look` (a person must judge it: explain each, or it
    fails)."""
    def item(check, value, status):
        return {'check': check, 'value': value, 'status': status}

    checks = []
    # UTC days with a lookup (from the unix seconds).
    days = sorted({time.strftime('%Y-%m-%d', time.gmtime(e['seconds'])) for e in events
                   if e.get('kind') in ('hit', 'miss')})
    checks.append(item(f"days with commands >= {EXIT['days']}", len(days),
                       'ok' if len(days) >= EXIT['days'] else 'not yet'))
    verified = report['verified_by_command']
    total = sum(verified.get(command, 0) for command in ('build', 'test', 'run'))
    keys = len({e.get('key') for e in events if e.get('kind') == 'verified'})
    met = (total >= EXIT['verified'] and keys >= EXIT['verified_keys']
           and verified.get('test', 0) >= EXIT['verified_test'] and verified.get('run', 0) >= EXIT['verified_run'])
    checks.append(item(f"verified hits >= {EXIT['verified']} on >= {EXIT['verified_keys']} keys, test >= "
                       f"{EXIT['verified_test']}, run >= {EXIT['verified_run']}",
                       f"{total} on {keys} keys (build {verified.get('build', 0)}, test {verified.get('test', 0)}, "
                       f"run {verified.get('run', 0)})", 'ok' if met else 'not yet'))
    misses = [e for e in events if e.get('kind') == 'miss']
    lockfile = sum(1 for e in misses if basename(unescape(e.get('path', '-'))) in ('MODULE.bazel', 'MODULE.bazel.lock')
                   or unescape(e.get('path', '-')).endswith('.MODULE.bazel'))
    pending = report['not_saved'].get('lockfile-write-pending', 0)
    checks.append(item('a MODULE.bazel / lockfile change (a miss naming one)',
                       f'{lockfile} miss(es) naming one ({pending} run(s) not saved for lockfile-write-pending)',
                       'ok' if lockfile else 'not yet'))
    build = [e for e in misses if basename(unescape(e.get('path', '-'))) in ('BUILD', 'BUILD.bazel')
             or unescape(e.get('path', '-')).endswith('.bzl')]
    checks.append(item('a branch switch touching BUILD/.bzl files (confirm one was a switch)',
                       f"{len(build)} miss(es) naming a BUILD or .bzl file on {len({e.get('key') for e in build})} "
                       f"key(s)", 'look' if build else 'not yet'))
    binaries = sorted({detail_text(e.get('detail')).get('binary') for e in events if e.get('kind') == 'saved'}
                      - {None})
    checks.append(item('a bazel-rs upgrade (two binaries among the saves)',
                       f"{len(binaries)} binar{'y' if len(binaries) == 1 else 'ies'}"
                       + (f" ({', '.join(binaries)})" if binaries else ''),
                       'ok' if len(binaries) >= 2 else 'not yet'))
    mismatches = collections.Counter(e.get('reason', '-') for e in events if e.get('kind') == 'mismatch')
    cannot = mismatches.pop('cannot-verify', 0)
    heals = sum(report['heals'].values())
    checks.append(item('no mismatch, heal or cannot-verify you did not cause (explain each)',
                       f"mismatches {sum(mismatches.values())}"
                       + (' (' + ', '.join(f'{k} {v}' for k, v in mismatches.most_common()) + ')' if mismatches else '')
                       + f", heals {heals}, cannot-verify {cannot}",
                       'ok' if not mismatches and not heals and not cannot else 'look'))
    suspect = sum(1 for run in report['always_miss'] if run['class'] == 'suspect')
    checks.append(item('no SUSPECT always-miss pattern', suspect, 'ok' if suspect == 0 else 'look'))
    rate = report['hit_rate_unchanged']['rate']
    checks.append(item(f"hit rate on unchanged re-runs >= {100 * EXIT['hit_rate']:.0f}%",
                       '-' if rate is None else f'{100 * rate:.1f}%',
                       'not yet' if rate is None else ('ok' if rate >= EXIT['hit_rate'] else 'look')))
    local = report['local_tree']
    checks.append(item(f"local <= replay on >= {100 * (1 - EXIT['local_above']):.0f}% of hits",
                       '-' if local['share_above'] is None else
                       f"local > replay on {local['local_above_replay']} of {local['hits']} "
                       f"({100 * local['share_above']:.1f}%)",
                       'not yet' if local['share_above'] is None else
                       ('ok' if local['share_above'] <= EXIT['local_above'] else 'look')))
    p95 = (report['hit_latency_ms'].get('all/all') or {}).get('p95')
    checks.append(item(f"hit p95 <= {EXIT['hit_p95_ms'] / 1000:.0f} s", '-' if p95 is None else f'{p95:.0f} ms',
                       'not yet' if p95 is None else ('ok' if p95 <= EXIT['hit_p95_ms'] else 'look')))
    store = report['store']
    if store is not None:
        checks.append(item('store <= 4 GiB', f"{store['bytes'] / (1 << 20):.1f} MiB",
                           'ok' if store['bytes'] <= EXIT['store_bytes'] else 'look'))
    return checks


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
    store = report.get('store_path')
    lines = [f'analysis snapshot soak report: {cache}' + (f' (store {store})' if store else '')]
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
        mismatched = info.get('mismatched_hits') or {}
        lines.append(
            f"  {info['key']} {info['command'] or '-'} {targets}: lookups {info['lookups']}, hits {info['hits']}"
            + ((' (and ' + ', '.join(f'{k} {v}' for k, v in mismatched.items()) + ' hit then mismatch)')
               if mismatched else '')
            + f", changed {info['changed']}, unexplained {info['unexplained']}, after removal {info['after_removal']}, "
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
    outcomes = report['verify_outcomes']
    mismatched = outcomes['hit_then_mismatch']
    lines.append(f"verify-mode hits: {outcomes['hits']} - verified {outcomes['verified']}, hit then mismatch "
                 f"{sum(mismatched.values())}"
                 + (' (' + ', '.join(f'{k} {v}' for k, v in mismatched.items()) + ')' if mismatched else '')
                 + f", not concluded {outcomes['not_concluded']} (the command ended first); a hit then mismatch "
                   f"is no clean hit: counted apart, out of the hit rates")
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
    lines.append('heals by reason: ' + (', '.join(f'{k} {v}' for k, v in sorted(heals.items())) if heals else 'none')
                 + f"; failed hits that kept their snapshot (heal-kept: every tree verified): {report.get('heal_kept', 0)}")
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
    local = report['local_tree']
    lines.append(f"local-tree check (beside the replay; hits with local repositories): {local['hits']}"
                 + (f", local p50 {number(local['p50'])} / p95 {number(local['p95'])} / max {number(local['max'])} ms; "
                    f"local > replay on {local['local_above_replay']} ({100 * local['share_above']:.1f}%)"
                    if local['hits'] else ''))
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
    checks = report.get('exit_check') or []
    if checks:
        lines.append('')
        statuses = collections.Counter(check['status'] for check in checks)
        lines.append('soak exit check (docs/ANALYSIS_SNAPSHOT.md "Soak"; this cache): '
                     + ('all met' if statuses['ok'] == len(checks) else
                        f"{statuses['ok']} of {len(checks)} met, {statuses['not yet']} not yet, "
                        f"{statuses['look']} to look at"))
        for check in checks:
            lines.append(f"  [{check['status']:7}] {check['check']}: {check['value']}")
    return '\n'.join(lines) + '\n'


def whole_lines(path):
    """The lines of `path` that end with a newline (a line being appended right
    now is left for the next run)."""
    try:
        text = path.read_text(encoding='utf-8', errors='surrogateescape')
    except OSError:
        return []
    lines = text.split('\n')
    return lines[:-1]


def units(name, lines):
    """A log's units: its lines, or for verify-details.log its blocks."""
    if name != VERIFY_DETAILS_LOG:
        return [line + '\n' for line in lines if line]
    blocks = []
    for line in lines:
        if line.startswith('  ') and blocks:
            blocks[-1] += line + '\n'
        elif line.strip():
            blocks.append(line + '\n')
    return blocks


def archive_logs(snapshots, directory):
    """Append to `directory`/<log> the lines (verify-details.log: blocks) of
    the cache's logs, both generations, that the archive does not hold yet —
    counted, so an event logged twice is archived twice — in their order
    (L2-W5i: the logs keep ~7,500 events, the archive the whole soak). Returns
    the units appended per log."""
    directory.mkdir(parents=True, exist_ok=True)
    appended = {}
    for name in (EVENTS_LOG, VERIFY_LOG, VERIFY_DETAILS_LOG):
        current = units(name, whole_lines(snapshots / (name + '.1')) + whole_lines(snapshots / name))
        target = directory / name
        held = collections.Counter(units(name, whole_lines(target)))
        new = []
        for unit in current:
            if held[unit] > 0:
                held[unit] -= 1
            else:
                new.append(unit)
        if new:
            with open(target, 'a', encoding='utf-8', errors='surrogateescape') as out:
                out.write(''.join(new))
        appended[name] = len(new)
    return appended


def load(cache, archive=None, workspace=None):
    """`(events, verify_lines, store, details)` of the logs of `workspace`'s
    store in the cache ([`snapshot_store`]) — or, with `archive`, of the archive
    after the store's new lines were added to it — and the store's size."""
    snapshots = snapshot_store(cache, workspace)
    logs = snapshots
    if archive is not None:
        archive_logs(snapshots, Path(archive))
        logs = Path(archive)
    events = [e for e in (parse_line(l) for l in read_generations(logs, EVENTS_LOG)) if e is not None]
    verify_lines = [l for l in read_generations(logs, VERIFY_LOG) if l.strip()]
    details = parse_details(read_generations(logs, VERIFY_DETAILS_LOG))
    return events, verify_lines, store_size(snapshots), details


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--json', action='store_true', help='print the report as JSON')
    ap.add_argument('--workspace', help='the workspace whose snapshot store to read (its state directory in the '
                                        'cache; its `bazel-*` entries are output trees; default: the parent of a '
                                        'cache named bazel-rs-out, else the one workspace that used the cache)')
    ap.add_argument('--archive', metavar='DIR',
                    help="first append the cache's new log lines to DIR (one file per log, never rotated), then "
                         "report over DIR: run it daily during a soak (the logs keep ~7,500 events)")
    ap.add_argument('cache')
    a = ap.parse_args(argv)
    events, verify_lines, store, details = load(a.cache, a.archive, a.workspace)
    report = analyze(events, verify_lines, store, details, Places(a.cache, a.workspace))
    report['store_path'] = str(snapshot_store(a.cache, a.workspace))
    if a.json:
        sys.stdout.write(json.dumps(report, indent=1, default=str) + '\n')
    else:
        sys.stdout.write(render(report, os.path.abspath(a.cache)))
    return 0 if (events or verify_lines) else 2


if __name__ == '__main__':
    sys.exit(main())
