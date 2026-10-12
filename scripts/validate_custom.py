#!/usr/bin/env python3
"""Scoped Bazel 9.3 custom-rule differential checks, using private fixture copies.

No repository probes, wrapper, fallback, Bazel test, or arbitrary user actions.
The only shell actions are the finite, trusted commands in fixtures/custom.
"""

import argparse
import datetime
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile

from validate import (
    ROOT, Reference, Runner, check, directory_size, file_evidence,
    native_command, native_paths, parse_labels, positive_seconds,
    require_success, validate_executable, write_report,
)
from validate_loading import action_counts, expected_error, fixture_outputs, output_listing


FIXTURE = ROOT / "tests/fixtures/custom"
ROOT_LABELS = sorted("//:" + name for name in (
    "scalar", "snapshot", "chain", "chain_native", "independent", "chain_all",
    "forwarded", "inspected", "empty", "none", "hidden",
))
INSPECT_BYTES = b"b.txt|a.txt\nb.txt|a.txt\na.txt|a.txt\n"
OUTPUT_CASES = (
    ("//:scalar", {"scalar.txt": "@@//:scalar|héllo 雪|first|7|True\n".encode()}, 1),
    ("//:snapshot", {"snapshot.txt": b"@@//:snapshot|override|caller|-2|False\n"}, 1),
    ("//:forwarded", {"a.txt": b"alpha\n", "b.txt": b"bravo\n"}, 0),
    ("//:inspected", {name: INSPECT_BYTES for name in (
        "inspect-main.txt", "inspect-extra.txt", "inspect-other.txt")}, 3),
    ("//:inspect-extra.txt", {"inspect-extra.txt": INSPECT_BYTES}, 1),
)
EMPTY_CASES = (
    ("//:empty", "unused-dynamic.txt"),
    ("//:none", "unused-none.txt"),
    ("//:hidden", "hidden-explicit.txt"),
)
DIRECT_BYTES = (b"direct input\ntool data\n"
                b"literal $HOME; *.txt 'quoted' \"double\"\n"
                b"env $HOME; *.txt 'quoted' \"double\"\n")
DEFINITION_SOURCE = "advanced/label_helper/source.txt"
ADVANCED_OUTPUT_CASES = (
    ("//advanced/args:arguments", {"first.txt": b"first\n", "second.txt": b"first\nsecond\n"}, 2),
    ("//advanced/none:scalar", {"scalar.txt": "@@//advanced/none:scalar|héllo 雪|first|7|True\n".encode()}, 1),
    ("//advanced/label_caller:none_default", {
        "none_default.txt": (DEFINITION_SOURCE + "|" + DEFINITION_SOURCE + "\n").encode()}, 1),
    ("//advanced/consumer:consumed", {
        "consumed.txt": b"provider payload\nprovider message|one,two|nested|5\n"}, 2),
    ("//advanced/direct:file_exec", {"file_exec.txt": DIRECT_BYTES}, 2),
    ("//advanced/direct:string_exec", {"string_exec.txt": DIRECT_BYTES}, 2),
    ("//advanced/label_caller:defaults", {
        "defaults.txt": (DEFINITION_SOURCE + "|" + DEFINITION_SOURCE + "\n").encode()}, 1),
    ("//advanced/label_caller:caller_string", {
        "caller_string.txt": ("advanced/label_caller/source.txt|" + DEFINITION_SOURCE + "\n").encode()}, 1),
    ("//advanced/label_caller:immutable_label", {
        "immutable_label.txt": (DEFINITION_SOURCE + "|" + DEFINITION_SOURCE + "\n").encode()}, 1),
)
ADVANCED_EMPTY_CASES = (("//advanced/producer:payload", "provider-only.txt"),)
ADVANCED_PATH_SUFFIXES = {
    "//advanced/direct:" + name: "advanced/direct/" + name + "/" + name + ".txt"
    for name in ("file_exec", "string_exec")
}
# Broad within the semantic category only. A timeout, spawn failure, success,
# unrelated diagnostic, or one-sided rejection can never be counted as parity.
ERROR_CASES = (
    ("none-label", "query", "//errors/none_label:bad", r"mandatory.*attr|attr.*mandatory|required"),
    ("none-output", "query", "//errors/none_output:bad", r"mandatory.*attr|attr.*mandatory|required"),
    ("mandatory", "query", "//errors/mandatory:bad", r"mandatory.*attr|attr.*mandatory|required.*text|text.*required"),
    ("type", "query", "//errors/type:bad", r"expected.*int|want.*int|int.*(?:type|got)|type.*int"),
    ("unknown", "query", "//errors/unknown:bad", r"no such attribute|unknown.*attr|unexpected.*nonexistent"),
    ("implementation", "build", "//errors/implementation:bad", r"custom implementation deliberate failure"),
    ("unproduced", "build", "//errors/unproduced:bad", r"no.*(?:generating action|producer)|not.*(?:generated|produced)|unproduced (?:output|artifact)"),
    ("duplicate", "build", "//errors/duplicate:bad", r"conflicting actions|action.*conflict|duplicate.*(?:output|producer|action)|multiple.*(?:action|producer)|already.*(?:generated|registered)"),
    ("source", "build", "//errors/source:bad", r"source file|file.*not allowed|does not.*allow.*file|expected.*rule"),
    ("single", "build", "//errors/single:bad", r"single file|exactly one|must produce one|more than one"),
    ("foreign", "build", "//errors/foreign:bad", r"source.*(?:file|artifact)|not.*(?:derived|owned)|foreign (?:output|artifact)|owner|output.*(?:generated|declared)"),
    ("cycle", "build", "//errors/cycle:bad", r"dependency cycle|action cycle|cycle in|cycle detected|cycle.*(?:action|artifact)|circular"),
    ("visibility", "build", "//errors/visibility:bad", r"not visible|visibility.*(?:violation|denied|not|check|error)|private target"),
)
ADVANCED_ERROR_CASES = (
    ("provider-fields", "build", "//advanced/errors/invalid_fields:bad", r"unexpected.*(?:field|keyword)|unknown.*field|no such field"),
    ("provider-missing", "build", "//advanced/errors/missing:bad", r"doesn't contain declared provider|does not.*provid|missing.*provider|provider.*not.*(?:found|present)"),
    ("provider-duplicate", "build", "//advanced/errors/duplicate:bad", r"returned.*provider.*twice|multiple.*provider|duplicate.*provider|provided.*more than once"),
    ("provider-frozen", "build", "//advanced/errors/frozen:bad", r"frozen.*(?:list|value)|cannot.*(?:mutate|modify)|immutable"),
)


def expected_check_count():
    return (4 + len(OUTPUT_CASES) + len(ADVANCED_OUTPUT_CASES)
            + len(EMPTY_CASES) + len(ADVANCED_EMPTY_CASES)
            + len(ERROR_CASES) + len(ADVANCED_ERROR_CASES))


def native_invoke(native, cache, runner, workspace, name, command, target, jobs=1):
    argv = native_command(native, cache, command, target)
    if command == "build":
        argv.insert(-1, "--jobs=" + str(jobs))
    result = runner.run(name, argv, workspace)
    # No fallback is requested; reject any reported attempt rather than treating
    # official output obtained by fallback as native parity.
    if re.search(r"falling back|fallback(?:\s+to|\s+enabled|\s+used)",
                 result["stderr"] + result["stdout"], re.I):
        raise ValueError("native reported upstream fallback in run " + str(result["id"]))
    return result


def query_parity(native, reference, cache, runner, target, expected):
    records = {
        "native": native_invoke(native, cache, runner, reference.workspace,
                                target + ".native.query", "query", target),
        "reference": reference.run(target + ".reference.query", "query", target),
    }
    labels = {name: parse_labels(require_success(result)) for name, result in records.items()}
    if any(value != sorted(expected) for value in labels.values()):
        raise ValueError("query labels differ for " + target)
    return {"labels": labels, "run_ids": {name: result["id"] for name, result in records.items()}}


def build_pair(native, reference, cache, runner, target, expected, counts, name, jobs=1):
    native_run = native_invoke(native, cache, runner, reference.workspace,
                               name + ".native", "build", target, jobs)
    measured = action_counts(native_run, counts)
    native_files = fixture_outputs(native_paths(native_run, target), expected)
    reference_run = reference.run(name + ".reference", "build", "--jobs=" + str(jobs), target)
    require_success(reference_run)
    reference_files = fixture_outputs(output_listing(reference, target), expected)
    return {"target": target, "native_run_id": native_run["id"],
            "reference_run_id": reference_run["id"], "native": native_files,
            "reference": reference_files, "jobs": jobs, "cache_directory": str(cache), **measured}


def output_case(native, reference, cache, runner, case):
    target, expected, count = case
    details = [build_pair(native, reference, cache, runner, target, expected,
                          (count, 0) if iteration == 1 else (0, count),
                          target + ".build" + str(iteration)) for iteration in (1, 2)]
    suffix = ADVANCED_PATH_SUFFIXES.get(target)
    if suffix is not None:
        for build in details:
            for implementation in ("native", "reference"):
                if any(not evidence["path"].endswith("/" + suffix) for evidence in build[implementation]):
                    raise ValueError("declare_file sibling path mismatch for " + target)
            build["sibling_path_suffix"] = suffix
    return details


def chain_expected(source=b"alpha\n"):
    return {"chain-final.txt": source + b"seed\nargument value|env value\n",
            "independent.txt": b"independent\n"}


def chain_builds(native, reference, scratch, runner):
    """Four reachable actions; editing a source invalidates just two of them."""
    target = "//:chain_all"
    details = []
    caches = {jobs: scratch / ("chain-native-jobs" + str(jobs)) for jobs in (1, 4)}
    for jobs, cache in caches.items():
        for iteration in (1, 2):
            details.append(build_pair(
                native, reference, cache, runner, target, chain_expected(),
                (4, 0) if iteration == 1 else (0, 4),
                "chain.jobs{}.build{}".format(jobs, iteration), jobs))
    source = reference.workspace / "a.txt"
    if (reference.workspace.resolve() == FIXTURE.resolve()
            or reference.workspace.resolve().is_relative_to(FIXTURE.resolve())
            or not source.resolve().is_relative_to(scratch.resolve())):
        raise ValueError("source invalidation requires a private fixture copy")
    original = source.read_bytes()
    try:
        source.write_bytes(b"changed alpha\n")
        for jobs, cache in caches.items():
            details.append(build_pair(
                native, reference, cache, runner, target, chain_expected(b"changed alpha\n"),
                (2, 2), "chain.jobs{}.source-change".format(jobs), jobs))
    finally:
        source.write_bytes(original)
    return details


def empty_case(native, reference, cache, runner, case):
    target, unused_name = case
    details = build_pair(native, reference, cache, runner, target, {}, (0, 0), target + ".empty")
    # cquery above verifies DefaultInfo really is empty. Also verify that the
    # registered write did not materialize an unselected artifact in either tree.
    for root in (cache, reference.output_base / "execroot"):
        if any(path.is_file() for path in root.rglob(unused_name)):
            raise ValueError("unselected action executed: " + unused_name)
    details["unselected_output_absent"] = unused_name
    return details


def error_parity(native, reference, cache, runner, case):
    category, command, target, pattern = case
    records = {
        "native": native_invoke(native, cache, runner, reference.workspace,
                                category + ".native", command, target),
        "reference": reference.run(category + ".reference", command, target),
    }
    crash_pattern = r"bazel crashed|FATAL:|panicked at|internal (?:error|crash)"
    if re.search(crash_pattern, records["native"]["stderr"], re.I):
        raise ValueError("native crashed instead of rejecting " + category)
    reference_crashed = bool(re.search(crash_pattern, records["reference"]["stderr"], re.I))
    if reference_crashed and not (category == "foreign" and
                                  "Source in outputs:" in records["reference"]["stderr"]):
        raise ValueError("unexpected reference crash for " + category)
    result = {"category": category, "target": target, "command": command,
            "comparison": "both must reject with a category diagnostic; no accepted mismatch",
            "diagnostic_parity": not reference_crashed,
            **{name: expected_error(result, pattern) for name, result in records.items()}}
    if reference_crashed:
        result.update(
            comparison="failure outcome only: native rejects source output; Bazel 9.3 crashes internally",
            reference_failure_kind="KNOWN_REFERENCE_INTERNAL_CRASH",
            limitation="Source in outputs crashes official analysis; this is not diagnostic parity. "
                       "Only private fixture sources are involved; Reference cleanup still runs.",
        )
    return result


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/custom-validation.json")
    parser.add_argument("--timeout", type=positive_seconds, default=90)
    return parser


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "output"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.output.is_relative_to(FIXTURE.resolve()):
        raise ValueError("--output must be outside tracked fixtures")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "schema_version": 1, "status": "FAIL", "runs": [], "checks": [], "errors": [], "cleanup": [],
        "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "configuration": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "reference_version_required": "9.3.0", "test_commands_executed": False,
        "scope": ["custom rule/attr scalar defaults and snapshots, named calls and visibility",
                  "bare/list DefaultInfo returns, files, postorder depset, Target indexing, File source short_path/basename",
                  "ctx.attr/files/file/outputs/label and write/run_shell/declare_file",
                  "four-action custom/native chain, forward action dependency, independent branch",
                  "separate jobs=1/4 cold/warm native caches and source invalidation",
                  "empty explicit defaults, None defaults, direct predeclared output selection",
                  "canonical provider identity, membership/indexing, retained scalar/list/struct/File fields",
                  "provider-only generated File dependency with no producer default outputs",
                  "actions.run File/string executable, tools, literal arguments/env, executable writes, sibling paths",
                  "definition-relative attr label defaults, caller-relative strings, immutable Label through macro",
                  "query defers implementations; loading and analysis error categories"],
        "unsupported": ["provider init callbacks", "actions.args", "executable/test rules", "runfiles", "toolchains",
                        "binary NUL writes", "real repository builds"],
        "limitations": ["Scoped parity only, not complete Bazel compatibility.",
                        "No fallback or wrapper requested; reported fallback is rejected.",
                        "File.path is only passed to commands, never compared as artifact content.",
                        "Native shell execution is trusted and unsandboxed; no arbitrary user commands.",
                        "Foreign source-output fixture can crash Bazel 9.3 with Source in outputs; "
                        "that case establishes failure outcome only, not diagnostic parity.",
                        "Fresh native caches are not OS-cold; action counts do not prove concurrency.",
                        "Reference action totals are not equated to native totals (Bazel has internal actions)."],
    }
    runner = Runner(report, args.timeout)
    references, scratch = [], None
    interrupted = False
    expected_checks = expected_check_count()
    try:
        validate_executable(args.native)
        validate_executable(args.reference, official=True)
        report["executables"] = {"native": file_evidence(args.native), "reference": file_evidence(args.reference)}
        parent = Path("/tmp/opencode")
        scratch = Path(tempfile.mkdtemp(prefix="custom-validation-", dir=parent if parent.is_dir() else args.output.parent))
        report["scratch_directory"] = str(scratch)
        workspace = scratch / "fixture"
        shutil.copytree(FIXTURE, workspace)
        reference = Reference(args.reference, workspace, scratch, runner, "custom-fixture")
        references.append(reference)
        report["reference_release"] = require_success(reference.run("reference.release", "info", "release")).strip()
        if report["reference_release"] != "release 9.3.0":
            raise ValueError("reference binary is not official release 9.3.0")
        cache = scratch / "query-native-cache"
        check(report, "fixture.query", lambda: query_parity(args.native, reference, cache, runner, "//:all", ROOT_LABELS))
        check(report, "fixture.query-defers-implementation", lambda: query_parity(
            args.native, reference, cache, runner, "//errors/implementation:bad", ["//errors/implementation:bad"]))
        check(report, "fixture.chain", lambda: chain_builds(args.native, reference, scratch, runner))
        # Empty provider producers must be probed before a downstream consumer
        # materializes their nondefault Files in the shared reference output tree.
        for index, case in enumerate(EMPTY_CASES + ADVANCED_EMPTY_CASES):
            check(report, "fixture.empty." + case[0], lambda c=case, i=index: empty_case(
                args.native, reference, scratch / ("empty-native-" + str(i)), runner, c))
        for index, case in enumerate(OUTPUT_CASES + ADVANCED_OUTPUT_CASES):
            check(report, "fixture.output." + case[0], lambda c=case, i=index: output_case(
                args.native, reference, scratch / ("output-native-" + str(i)), runner, c))
        # Run only after the hidden rule's empty-default selection was checked.
        check(report, "fixture.explicit-hidden-output", lambda: output_case(
            args.native, reference, scratch / "hidden-native", runner,
            ("//:hidden-explicit.txt", {"hidden-explicit.txt": b"explicit predeclared selection\n"}, 1)))
        for case in ERROR_CASES + ADVANCED_ERROR_CASES:
            check(report, "fixture.error." + case[0], lambda c=case: error_parity(
                args.native, reference, scratch / "errors-native-cache", runner, c))
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
                report["cache_sizes_bytes_before_cleanup"] = {
                    path.name: directory_size(path) for path in scratch.iterdir() if path.is_dir()}
            except OSError as exc:
                report["errors"].append("cache size: " + str(exc))
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
    print("{} (scoped custom-rule parity only): {}".format(report["status"], args.output))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
