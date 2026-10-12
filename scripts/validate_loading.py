#!/usr/bin/env python3
"""Scoped local-loading differential validation; never a full repository test.

Only trusted fixture genrules execute shell commands. Repository probes are
allowlisted source-only targets, never executable scripts or generated outputs.
All execution, official-server isolation and cleanup reuse validate.py.
"""

import argparse
import datetime
import hashlib
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile

from validate import (
    ROOT, Reference, Runner, check, file_evidence, native_command, native_paths,
    parse_labels, positive_seconds, reference_output_paths, require_success,
    validate_executable, write_report,
)


FIXTURE = ROOT / "tests/fixtures/loading"
ROOT_LABELS = sorted("//:" + name for name in (
    "diamond", "diamond_seed", "diamond_left", "diamond_right",
    "diamond_independent", "diamond_join", "nested", "flattened",
    "public_default", "export_default",
))
REPO_TARGETS = (
    ("//src/beyondwork.ai/bworker/config:config_files", 7, ".yaml"),
    ("//src/k8s/crossplane:files", 109, ".yaml"),
    ("//docs:runbook-temporal-encryption-key.md", 1, None),
    ("//thirdparty:build_stack_rules_proto/proto_compile_bazel9.patch", 1, None),
    ("//bin:bw-devenv", 1, None),
    ("//src/beyondwork.ai/fleettrust/examples:tailscale_pilot", None, None),
)
# Diagnostics are compared by semantic category, not wording or exit-code number.
ERROR_CASES = (
    ("cycle", "query", "//errors/cycle:all", r"cycle|circular"),
    ("missing-symbol", "query", "//errors/missing:all", r"absent_symbol"),
    ("private-symbol", "query", "//errors/private:all", r"private|_private"),
    ("empty-glob", "query", "//errors/empty:all", r"empty|no files|match"),
    ("glob-parent-boundary", "query", "//errors/pattern:all", r"glob|pattern|segment"),
    ("frozen-global", "query", "//errors/frozen:all", r"frozen|freeze|immutable"),
    ("load-package-boundary", "query", "//errors/boundary:all", r"subpackage|boundary|package"),
    ("private-override", "build", "//errors/visibility:denied", r"visib|private"),
    ("pkg-not-descendants", "build", "//consumer/child:denied", r"visib|private"),
    ("output-prefix-conflict", "build", "//errors/outputprefix:collision", r"conflict|overlap|prefix"),
)


def native_invoke(native, cache, runner, workspace, name, command, target, jobs=1):
    argv = native_command(native, cache, command, target)
    if command == "build":
        argv.insert(-1, "--jobs=" + str(jobs))
    return runner.run(name, argv, workspace)


def action_counts(record, expected):
    require_success(record)
    match = re.search(r"(\d+) actions executed, (\d+) cache hits", record["stderr"])
    if not match or tuple(map(int, match.groups())) != expected:
        raise ValueError("native action/cache counts differ from {} in run {}".format(expected, record["id"]))
    return dict(zip(("actions_executed", "cache_hits"), expected))


def query_parity(native, reference, cache, runner, target, expected=None):
    labels = {
        "native": parse_labels(require_success(native_invoke(
            native, cache, runner, reference.workspace, target + ".native.query", "query", target))),
        "reference": parse_labels(require_success(reference.run(target + ".reference.query", "query", target))),
    }
    if labels["native"] != labels["reference"] or (expected is not None and labels["native"] != sorted(expected)):
        raise ValueError("query label mismatch for " + target)
    return labels


def output_listing(reference, target):
    result = reference.run(target + ".reference.files", "cquery", "--output=files", target)
    return reference_output_paths(require_success(result), reference.workspace, reference.output_base)


def fixture_outputs(paths, expected):
    # Fixture basenames are deliberately unique, even across source/generated outputs.
    actual = {path.name: path for path in paths}
    if len(actual) != len(paths) or set(actual) != set(expected):
        raise ValueError("unexpected fixture output set: " + str(sorted(actual)))
    return [file_evidence(actual[name], expected[name], include_bytes=True) for name in sorted(actual)]


def fixture_builds(native, reference, scratch, runner):
    target = "//:flattened"
    expected = {"diamond.txt": b"alpha\nalpha\nbeta\n", "a.txt": b"alpha\n"}
    details = {"target": target, "native": [], "reference": []}
    # Independent empty caches ensure jobs=4 is not just a warm-cache exercise.
    for jobs in (1, 4):
        cache = scratch / ("fixture-native-jobs" + str(jobs))
        for iteration in (1, 2):
            result = native_invoke(native, cache, runner, reference.workspace,
                                   "fixture.native.jobs{}.build{}".format(jobs, iteration),
                                   "build", target, jobs)
            counts = action_counts(result, (6, 0) if iteration == 1 else (0, 6))
            details["native"].append({
                "jobs": jobs, "iteration": iteration, "cache_directory": str(cache),
                "run_id": result["id"], **counts,
                "files": fixture_outputs(native_paths(result, target), expected),
            })
    for iteration in (1, 2):
        result = reference.run("fixture.reference.build" + str(iteration), "build", target)
        require_success(result)
        details["reference"].append({"run_id": result["id"], "iteration": iteration,
                                     "files": fixture_outputs(output_listing(reference, target), expected)})
    return details


def source_manifest(paths, workspace, count=None, suffix=None):
    """Reject generated/external/escaping files BEFORE repository build commands."""
    workspace = workspace.resolve()
    manifest = {}
    for path in paths:
        resolved = path.resolve()
        try:
            relative = resolved.relative_to(workspace)
        except ValueError as exc:
            raise ValueError("not a workspace source: " + str(path)) from exc
        if (not relative.parts or relative.parts[0] in ("bazel-out", "external")
                or any(part.startswith("bazel-") for part in relative.parts)
                or not resolved.is_file()):
            raise ValueError("not a regular source file: " + str(path))
        key = relative.as_posix()
        if key in manifest:
            raise ValueError("duplicate source path: " + key)
        if suffix is not None and resolved.suffix != suffix:
            raise ValueError("unexpected source extension: " + key)
        evidence = file_evidence(resolved)
        evidence["path"] = key
        manifest[key] = evidence
    if not manifest or (count is not None and len(manifest) != count):
        raise ValueError("unexpected source count: " + str(len(manifest)))
    return dict(sorted(manifest.items()))


def edge_builds(native, reference, cache, runner):
    details = []
    for target, expected in (
        ("//edges/source-parent:gen", b"generated"),
        ("//edges/locations:gen", b"A\nZ\n"),
    ):
        query_parity(native, reference, cache, runner, target, [target])
        native_run = native_invoke(native, cache, runner, reference.workspace,
                                   target + ".native.build", "build", target, 2)
        native_files = fixture_outputs(native_paths(native_run, target), {"out": expected})
        action_counts(native_run, (1, 0))
        require_success(reference.run(target + ".reference.build", "build", target))
        reference_files = fixture_outputs(output_listing(reference, target), {"out": expected})
        details.append({"target": target, "native": native_files, "reference": reference_files})
    return details


def source_parity(native, reference, cache, runner, target, count=None, suffix=None):
    labels = query_parity(native, reference, cache, runner, target, [target])
    # Analysis has no actions. Validate every reference path before either build.
    reference_files = output_listing(reference, target)
    before = source_manifest(reference_files, reference.workspace, count, suffix)
    result = native_invoke(native, cache, runner, reference.workspace,
                           target + ".native.build", "build", target, 4)
    counts = action_counts(result, (0, 0))
    native_files = source_manifest(native_paths(result, target), reference.workspace, count, suffix)
    require_success(reference.run(target + ".reference.build", "build", target))
    after = source_manifest(reference_files, reference.workspace, count, suffix)
    if before != after or native_files != after:
        raise ValueError("source path/size/SHA256 parity mismatch for " + target)
    return {"target": target, "labels": labels, "native": native_files,
            "reference": after, "native_counts": counts, "source_only": True}


def expected_error(record, pattern):
    if (record["returncode"] is None or record["returncode"] <= 0 or record["timed_out"]
            or record["stdout"].strip() or not re.search(pattern, record["stderr"], re.I)):
        raise ValueError("not the expected semantic rejection in run " + str(record["id"]))
    return {"run_id": record["id"], "returncode": record["returncode"], "status": "EXPECTED_FAILURE"}


def error_parity(native, reference, cache, runner, case):
    category, command, target, pattern = case
    records = {
        "native": native_invoke(native, cache, runner, reference.workspace,
                                category + ".native", command, target),
        "reference": reference.run(category + ".reference", command, target),
    }
    return {"category": category, "target": target, "command": command,
            "comparison": "both reject with category diagnostic; wording and exit numbers may differ",
            **{name: expected_error(record, pattern) for name, record in records.items()}}


def git_snapshot(runner, repo, name):
    snapshot = {}
    for field, args in (
        ("status", ["status", "--porcelain=v1", "-z", "--untracked-files=all"]),
        ("unstaged_diff", ["diff", "--no-ext-diff", "--no-textconv", "--binary"]),
        ("staged_diff", ["diff", "--cached", "--no-ext-diff", "--no-textconv", "--binary"]),
    ):
        record = runner.run(name + "." + field, ["git", "--no-optional-locks", "-C", str(repo)] + args, repo)
        try:
            text = require_success(record)
            # Runner retains exact non-UTF8 bytes; hash those rather than replacement characters.
            if "stdout_base64" in record:
                import base64
                data = base64.b64decode(record["stdout_base64"])
            else:
                data = text.encode("utf-8")
            snapshot[field + "_sha256"] = hashlib.sha256(data).hexdigest()
            snapshot[field + "_size_bytes"] = len(data)
            if field == "status":
                snapshot["status_porcelain"] = text
        finally:
            # Never serialize repository diff contents, even on an unsuccessful git invocation.
            record["stdout"] = "[redacted: repository state hashed, not stored]"
            record.pop("stdout_base64", None)
    return snapshot


def repository_checks(report, native, reference, cache, runner):
    before = git_snapshot(runner, reference.workspace, "repo.before")
    report["repository_state"] = {"before": before}
    try:
        for target, count, suffix in REPO_TARGETS:
            check(report, "repo." + target, lambda t=target, c=count, s=suffix: source_parity(
                native, reference, cache, runner, t, c, s))
    finally:
        after = git_snapshot(runner, reference.workspace, "repo.after")
        report["repository_state"].update(after=after, unchanged=before == after)
        if before != after:
            # Concurrent edits are deliberately neither attributed nor reverted.
            report["errors"].append("Repository status/diff changed during probes; no edits reverted.")


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--repo", type=Path, default=Path.home() / "code")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/loading-validation.json")
    parser.add_argument("--timeout", type=positive_seconds, default=90)
    parser.add_argument("--skip-repo", action="store_true")
    return parser


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "repo", "output"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if not args.skip_repo and args.output.is_relative_to(args.repo):
        raise ValueError("--output must be outside the probed repository")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "schema_version": 1, "status": "FAIL", "runs": [], "checks": [], "errors": [], "cleanup": [],
        "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "configuration": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "reference_version_required": "9.3.0", "test_commands_executed": False,
        "scope": ["local nested/aliased loads and legacy native macros", "native/global struct and frozen globals",
                  "package defaults including declaration after rule, exports_files default visibility",
                  "glob include/exclude/directories/empty/subpackage boundaries", "__pkg__/__subpackages__ visibility",
                  "six-action chain/shared diamond/independent genrules; flattened filegroup",
                  "native jobs=1 and jobs=4 with separate cold caches and warm rebuilds",
                  "semantic loading/visibility error categories"] + ([] if args.skip_repo else ["allowlisted repository source-only targets"]),
        "unsupported": [
            {"feature": item, "status": "NOT_TESTED", "reason": "outside this harness scope; no fallback"}
            for item in ("external loads/repository rules", "custom rules/providers/aspects/toolchains",
                         "select/configuration transitions", "test execution", "full repository build")
        ],
        "limitations": ["PASS is scoped differential evidence, not full parity or full repository build/testing success.",
                        "Only trusted fixture copies/concatenations execute; native execution is not sandboxed.",
                        "No repository wrapper or .envrc; no source-content snapshots for repository probes.",
                        "Fresh caches are not OS-cold; action counts do not prove wall-clock concurrency.",
                        "Official rc, remote cache/executor/BES, lockfile writes and convenience symlinks disabled.",
                        "DO_NOT_TRACK/BAZEL_METRICS/NEXT_TELEMETRY_DISABLED set; not a network sandbox.",
                        "Git state checks cannot attribute concurrent edits; no edits are ever reverted."],
    }
    runner = Runner(report, args.timeout)
    references, scratch = [], None
    interrupted = False
    expected_checks = 1 + 1 + 2 + 4 + len(ERROR_CASES) + (0 if args.skip_repo else len(REPO_TARGETS))
    try:
        validate_executable(args.native)
        validate_executable(args.reference, official=True)
        report["executables"] = {"native": file_evidence(args.native), "reference": file_evidence(args.reference)}
        parent = Path("/tmp/opencode")
        scratch = Path(tempfile.mkdtemp(prefix="loading-validation-", dir=parent if parent.is_dir() else args.output.parent))
        report["scratch_directory"] = str(scratch)
        fixture = scratch / "fixture"
        shutil.copytree(FIXTURE, fixture)
        reference = Reference(args.reference, fixture, scratch, runner, "loading-fixture")
        references.append(reference)
        # Independent release check: an executable name/version flag is not sufficient.
        report["reference_release"] = require_success(reference.run("reference.release", "info", "release")).strip()
        if report["reference_release"] != "release 9.3.0":
            raise ValueError("reference binary is not official release 9.3.0")
        cache = scratch / "fixture-query-source-cache"
        check(report, "fixture.query", lambda: query_parity(args.native, reference, cache, runner, "//:all", ROOT_LABELS))
        check(report, "fixture.builds", lambda: fixture_builds(args.native, reference, scratch, runner))
        check(report, "fixture.glob-edge-semantics", lambda: query_parity(
            args.native, reference, cache, runner, "//edges/glob:check", ["//edges/glob:check"]))
        check(report, "fixture.generated-paths-and-locations", lambda: edge_builds(
            args.native, reference, scratch / "fixture-edge-cache", runner))
        for target in ("//:public_default", "//:export_default", "//consumer:allowed", "//consumer/child:allowed"):
            check(report, "fixture.source." + target, lambda t=target: source_parity(args.native, reference, cache, runner, t, 1))
        for case in ERROR_CASES:
            check(report, "fixture.error." + case[0], lambda c=case: error_parity(args.native, reference, cache, runner, c))
        if not args.skip_repo:
            if not (args.repo / "MODULE.bazel").is_file():
                raise ValueError("--repo must be a module workspace")
            repo_reference = Reference(args.reference, args.repo, scratch, runner, "loading-repo")
            references.append(repo_reference)
            repository_checks(report, args.native, repo_reference, scratch / "repo-native-cache", runner)
        else:
            report["repository_state"] = {"status": "NOT_RUN", "reason": "--skip-repo"}
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
    finally:
        for reference in reversed(references):
            try:
                reference.shutdown()
            except (OSError, ValueError) as exc:
                report["errors"].append("shutdown: " + str(exc))
        if scratch is not None:
            try:
                shutil.rmtree(scratch)
                report["scratch_cleaned"] = True
            except OSError as exc:
                report["scratch_cleaned"] = False
                report["errors"].append("cleanup: " + str(exc))
        if (len(report["checks"]) == expected_checks and not report["errors"]
                and all(item["status"] == "PASS" for item in report["checks"])):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print("{} (scoped loading parity only): {}".format(report["status"], args.output))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
