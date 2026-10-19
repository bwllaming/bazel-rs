#!/usr/bin/env python3
"""N56 validation output groups: receipt analysis and optional native replay.

The frozen receipt (tests/fixtures/validation_actions/reference_receipt.json)
is a paired genuine Bazel 9.3.0 vs native bazel-rs capture produced ONCE by
artifacts/validation_actions-capture.py plus exactly one determinism rerun;
it is immutable evidence, never a runtime dependency of the build. This
validator checks its structure and classification contract hermetically;
`--native BINARY` replays the native phases in a private scratch and asserts
they still match the receipt's native columns (rc, materialized bazel-bin
tree, reported outputs). Never a full parity claim: 1 phase is recorded
native-unsupported boundary (docs/VALIDATION_ACTIONS.md).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / 'tests/fixtures/validation_actions'
RECEIPT = ROOT / 'tests/fixtures/validation_actions/reference_receipt.json'
GENUINE_SHA256 = 'd302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8'
FIXTURE_FILES = ('MODULE.bazel', 'BUILD.bazel', 'defs.bzl', 'aspects.bzl',
                 'probe/BUILD.bazel', 'probe/defs.bzl')
PHASE_IDS = (
    'A01-transitive-collection', 'A02-run-validations-false',
    'A03-minus-validation-quirk', 'A04-output-groups-override',
    'A05-output-groups-add', 'A06-output-groups-missing',
    'A07-tool-dependency-not-collected', 'A08-private-attribute-collected',
    'A09-label-list-dict-collected', 'A10-output-file-target',
    'A11-output-file-consumer', 'A12-native-rule-propagation',
    'A13-failing-validation-action', 'A14-validation-transitive-private',
    'A15-skip-validations', 'A16-aspect-validation',
    'A17-validationprovider-absent', 'A18-query-labels',
)
NATIVE_UNSUPPORTED = ('A15-skip-validations',)
CLASSIFICATIONS = ('exact', 'documented_difference', 'native_unsupported', 'mismatch')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def load_receipt(path=RECEIPT):
    return json.loads(Path(path).read_text())


def structural_failures(receipt):
    """Hermetic checks of the frozen receipt's shape and pins."""
    problems = []
    if receipt.get('schema') != 'n56-validation-actions-reference-receipt/v1':
        problems.append('schema')
    if receipt.get('reference', {}).get('sha256') != GENUINE_SHA256:
        problems.append('reference sha256 pin')
    for name in FIXTURE_FILES:
        recorded = receipt.get('fixture', {}).get('files', {}).get(name)
        actual = digest((FIXTURE / name).read_bytes())
        if recorded != actual:
            problems.append(f'fixture digest {name}')
    ids = tuple(phase.get('id') for phase in receipt.get('phases', []))
    if ids != PHASE_IDS:
        problems.append(f'phase ids {ids}')
    for phase in receipt.get('phases', []):
        kind = phase.get('classification')
        if kind not in CLASSIFICATIONS:
            problems.append(f"{phase.get('id')} classification {kind!r}")
        if kind == 'native_unsupported' and phase['id'] not in NATIVE_UNSUPPORTED:
            problems.append(f"{phase['id']} unexpected native_unsupported")
        if kind == 'exact':
            g, n = phase['genuine'], phase['native']
            if None in (g, n):
                problems.append(f"{phase['id']} exact without both engines")
            elif not (g['returncode'] == n['returncode']
                      and g['bin'] == n['bin']
                      and g['reported'] == n['reported']):
                problems.append(f"{phase['id']} exact with differing columns")
    return problems


def post_failures(receipt):
    """Hermetic checks of the post-refinement rows, when present."""
    post = receipt.get('post_capture_changes')
    if post is None:
        return []
    problems = []
    for name in ('order/BUILD.bazel', 'order/defs.bzl'):
        recorded = post.get('files', {}).get(name)
        actual = digest((FIXTURE / name).read_bytes())
        if recorded != actual:
            problems.append(f'post fixture digest {name}')
    phases = post.get('phases', [])
    if [p.get('id') for p in phases] != ['P01-collection-order']:
        problems.append('post phase ids')
    for phase in phases:
        if phase['classification'] != 'exact':
            problems.append(f"{phase['id']} not exact")
        elif phase['genuine']['order'] != phase['native']['order']:
            problems.append(f"{phase['id']} order columns differ")
        elif phase['genuine']['order'] != ('order/t_toolchains.validated,'
                                           'order/t_z.validated,'
                                           'order/t_a.validated'):
            problems.append(f"{phase['id']} unexpected order")
    return problems


def counts(receipt):
    out = {}
    for phase in receipt['phases']:
        out[phase['classification']] = out.get(phase['classification'], 0) + 1
    return out


def expected_counts():
    return {'exact': len(PHASE_IDS) - len(NATIVE_UNSUPPORTED),
            'native_unsupported': len(NATIVE_UNSUPPORTED)}


def _materialized(workspace):
    found = {}
    for cache in workspace.rglob('actions/*/outputs'):
        for path in sorted(cache.rglob('*')):
            if path.is_file() and not path.is_symlink():
                relative = str(path.relative_to(cache))
                if relative == 'manifest.json':
                    continue
                if '/bin/' in relative:
                    relative = relative.split('/bin/', 1)[1]
                found.setdefault(relative, path.read_text('utf-8', 'replace'))
    return found


def _reported(stdout_text):
    names = set()
    for line in stdout_text.splitlines():
        if '\t' in line:
            names.add(line.split('\t')[1].rsplit('/', 1)[-1])
    return sorted(names)


def replay_native(binary, timeout=180):
    """Replay every phase's native column in a private scratch; the receipt's
    recorded native results are assertions, the genuine columns are not."""
    receipt = load_receipt()
    failures = []
    base = Path(tempfile.mkdtemp(prefix='n56-validation-replay-', dir='/tmp/opencode'))
    try:
        for phase in receipt['phases']:
            workspace = base / phase['id']
            shutil.copytree(FIXTURE, workspace)
            argv = [str(binary), '--ignore_all_rc_files', '--cache-dir=cache'] \
                + phase['argv'] + phase.get('native_extra', [])
            env = dict(os.environ, TMPDIR=str(base), LC_ALL='C.UTF-8')
            try:
                proc = subprocess.Popen(argv, cwd=str(workspace), env=env,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        start_new_session=True)
                out, err = proc.communicate(timeout=timeout)
                rc, timed_out = proc.returncode, False
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                out, err = proc.communicate()
                rc, timed_out = 1, True
            recorded = phase['native']
            bin_now = _materialized(workspace)
            reported_now = _reported(out.decode('utf-8', 'replace'))
            if (rc != recorded['returncode']
                    or timed_out != recorded['timed_out']
                    or bin_now != recorded['bin']
                    or reported_now != recorded['reported']):
                failures.append({
                    'id': phase['id'],
                    'rc': [recorded['returncode'], rc],
                    'bin': [sorted(recorded['bin']), sorted(bin_now)],
                    'reported': [recorded['reported'], reported_now],
                })
            shutil.rmtree(workspace)
    finally:
        shutil.rmtree(base, ignore_errors=True)
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--native', type=Path, default=None,
                        help='replay the receipt\'s native phases with this binary')
    args = parser.parse_args()
    receipt = load_receipt()
    problems = structural_failures(receipt) + post_failures(receipt)
    if counts(receipt) != expected_counts():
        problems.append(f"counts {counts(receipt)} != {expected_counts()}")
    if args.native is not None:
        args.native = args.native.resolve()
        if not args.native.is_file():
            return f'native binary not found: {args.native}'
        for failure in replay_native(args.native):
            problems.append(f"native replay diverged: {failure['id']} "
                            f"rc {failure['rc']} reported {failure['reported']}")
    if problems:
        print('FAIL: ' + '; '.join(problems))
        return 1
    print(f"OK: {len(receipt['phases'])} phases, {counts(receipt)}")
    return 0


if __name__ == '__main__':
    sys.exit(main())
