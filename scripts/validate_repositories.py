#!/usr/bin/env python3
"""Scoped local-repository differential checks, never arbitrary repository builds.

Only copied fixtures execute trusted cat/write actions. No test/run/publish,
wrapper, fallback, fake tool repositories, or pre-existing native caches.
"""

import argparse
import datetime
import json
from pathlib import Path
import re
import shutil
import signal
import stat
import sys
import tempfile

from validate import (
    ROOT, Reference, Runner, check, directory_size, file_evidence,
    native_command, positive_seconds, reference_output_paths, require_success,
    validate_executable, write_report,
)
from validate_custom import native_invoke as shared_native_invoke
from validate_loading import action_counts, expected_error


FIXTURE = ROOT / "tests/fixtures/repositories"
ALIASES = {"first": "fixture_alpha+", "second": "fixture_beta+"}
REPOSITORIES = {"fixture_alpha+": "repos/alpha", "fixture_beta+": "repos/beta"}
LABEL = re.compile(r"(?:(?:@@?)[A-Za-z0-9_.+~-]+|@@)?//[^\s:]*:[^\s:]+\Z")
ROOT_NAMES = ("copy", "copies", "sources", "inspect", "main_allowed", "private_denied", "pkg_denied")
ALPHA_NAMES = ("copy", "hidden", "forwarded", "inspect", "via_dep", "via_main",
               "private", "pkg_visible", "main_visible", "own_allowed")
QUERY_CASES = (
    ("//p:all", sorted("//p:" + name for name in ROOT_NAMES)),
    ("@first//p:all", sorted("@@fixture_alpha+//p:" + name for name in ALPHA_NAMES)),
    ("@second//p:all", ["@@fixture_beta+//p:copy"]),
    ("@@fixture_alpha+//p:copy", ["@@fixture_alpha+//p:copy"]),
)
MAPPING_ERROR = r"no repository visible|unknown (?:apparent )?(?:repository|repo)|repository.*(?:not visible|not found|could not be resolved)|unknown.*alias"
ERROR_CASES = (
    ("root-dependency-alias", "query", "@dep//p:data.txt", MAPPING_ERROR),
    ("external-root-alias", "build", "@first//errors/root_alias:bad", MAPPING_ERROR),
    ("external-empty-alias", "build", "@first//errors/empty_alias:bad", MAPPING_ERROR),
    ("cross-repository-private", "build", "//p:private_denied", r"not visible|visibility.*(?:error|denied|violation)|private target"),
    ("external-pkg-not-main", "build", "//p:pkg_denied", r"not visible|visibility.*(?:error|denied|violation)|private target"),
)
SOURCES = {
    "source:p/data.txt": b"main repository\n",
    "source:external/fixture_alpha+/p/data.txt": b"alpha repository\n",
    "source:external/fixture_beta+/p/data.txt": b"beta repository\n",
}


class RepositoryReference(Reference):
    """Private download/content caches too; genuine Bazel dependencies only."""

    def run(self, name, command, *args):
        registry = getattr(self, "registry_url", None)
        registries = ["--registry=" + registry, "--registry=https://bcr.bazel.build"] if registry else []
        return super().run(name, command,
                           "--repository_cache=" + str(self.output_base.parent / "repository-cache"),
                           "--repo_contents_cache=" + str(self.output_base.parent / "repository-contents"),
                           *registries,
                           *args)


def native_invoke(native, cache, runner, workspace, name, command, target, jobs=4, registry=None):
    if registry is None:
        return shared_native_invoke(native, cache, runner, workspace, name, command, target, jobs)
    argv = native_command(native, cache, command, target)
    argv.insert(-1, "--registry=" + registry)
    if command == "build":
        argv.insert(-1, "--jobs=" + str(jobs))
    record = runner.run(name, argv, workspace)
    if re.search(r"falling back|fallback(?:\s+to|\s+enabled|\s+used)",
                 record["stdout"] + record["stderr"], re.I):
        raise ValueError("native reported upstream fallback")
    return record


def parse_labels(stdout):
    labels = [line.strip() for line in stdout.splitlines() if line.strip()]
    if any(not LABEL.fullmatch(label) for label in labels) or len(labels) != len(set(labels)):
        raise ValueError("invalid or duplicate repository query labels")
    return sorted(labels)


def canonical_label(label):
    if not LABEL.fullmatch(label):
        raise ValueError("invalid label: " + label)
    if label.startswith("@@//"):
        return label[2:]
    if label.startswith("@@") or label.startswith("//"):
        return label
    alias, suffix = label[1:].split("//", 1)
    if alias not in ALIASES:
        raise ValueError("unrecognized apparent repository: " + alias)
    return "@@" + ALIASES[alias] + "//" + suffix


def query_parity(native, reference, cache, runner, target, expected, registry=None):
    records = {
        "native": native_invoke(native, cache, runner, reference.workspace,
                                target + ".native.query", "query", target, registry=registry),
        "reference": reference.run(target + ".reference.query", "query", target),
    }
    exact = {name: parse_labels(require_success(record)) for name, record in records.items()}
    identities = {name: sorted(canonical_label(label) for label in labels) for name, labels in exact.items()}
    if any(labels != sorted(expected) for labels in identities.values()):
        raise ValueError("repository query identity mismatch for " + target)
    if exact["native"] != sorted(expected):
        raise ValueError("native query did not return canonical repository labels")
    return {"target": target, "exact_labels": exact, "canonical_identities": identities,
            "exact_output_equal": exact["native"] == exact["reference"],
            "comparison": "label identity only; official query uses root apparent aliases, native uses canonical names",
            "alias_mapping": ALIASES, "run_ids": {name: record["id"] for name, record in records.items()}}


def native_paths(record, target):
    paths = []
    for line in require_success(record).splitlines():
        if not line:
            continue
        columns = line.split("\t")
        if len(columns) != 2 or canonical_label(columns[0]) != canonical_label(target):
            raise ValueError("unexpected native output label or format")
        path = Path(columns[1])
        if not path.is_absolute() or ".." in path.parts or path in paths:
            raise ValueError("unsafe or duplicate native output path")
        paths.append(path)
    return paths


def artifact_key(path, workspace):
    """Preserve repository/package identity; remove only two explicit configs."""
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("unsafe artifact path")
    if "bazel-out" in path.parts:
        if path.parts.count("bazel-out") != 1:
            raise ValueError("ambiguous generated artifact path")
        parts = path.parts[path.parts.index("bazel-out") + 1:]
        if len(parts) < 3 or parts[0] not in ("rust-fastbuild", "k8-fastbuild") or parts[1] != "bin":
            raise ValueError("unsupported generated configuration prefix")
        return "generated:" + "/".join(parts[2:])
    resolved = path.resolve()
    for repository, relative in REPOSITORIES.items():
        root = (workspace / relative).resolve()
        if resolved.is_relative_to(root):
            return "source:external/" + repository + "/" + resolved.relative_to(root).as_posix()
    root = workspace.resolve()
    if resolved.is_relative_to(root):
        return "source:" + resolved.relative_to(root).as_posix()
    raise ValueError("source artifact escaped private repository roots: " + str(path))


def artifact_manifest(paths, expected, workspace):
    result = {}
    for path in paths:
        key = artifact_key(path, workspace)
        if key in result or key not in expected:
            raise ValueError("duplicate or unexpected artifact identity: " + key)
        result[key] = file_evidence(path, expected[key], include_bytes=True)
        result[key]["mode_octal"] = format(stat.S_IMODE(path.stat().st_mode), "04o")
    if set(result) != set(expected):
        raise ValueError("missing artifact identities: " + str(sorted(set(expected) - set(result))))
    return dict(sorted(result.items()))


def reference_build(reference, target, expected, name):
    record = reference.run(name + ".reference", "build", "--jobs=4", target)
    require_success(record)
    listing = reference.run(name + ".reference.files", "cquery", "--output=files", target)
    paths = reference_output_paths(require_success(listing), reference.workspace, reference.output_base)
    return {"run_id": record["id"], "listing_run_id": listing["id"],
            "files": artifact_manifest(paths, expected, reference.workspace)}


def build_pair(native, reference, cache, runner, target, expected, counts, name, registry=None):
    record = native_invoke(native, cache, runner, reference.workspace,
                           name + ".native", "build", target, jobs=4, registry=registry)
    measured = action_counts(record, counts)
    native_files = artifact_manifest(native_paths(record, target), expected, reference.workspace)
    official = reference_build(reference, target, expected, name)
    return {"target": target, "native_run_id": record["id"], "cache_directory": str(cache),
            "jobs": 4, "native": native_files, "reference": official,
            "mode_differences": {key: {"native": native_files[key]["mode_octal"],
                                       "reference": official["files"][key]["mode_octal"]}
                                 for key in native_files
                                 if native_files[key]["mode_octal"] != official["files"][key]["mode_octal"]},
            **measured}


def copies_expected(alpha=b"alpha repository\n"):
    return {"generated:p/copy.txt": b"main repository\n",
            "generated:external/fixture_alpha+/p/copy.txt": alpha,
            "generated:external/fixture_beta+/p/copy.txt": b"beta repository\n"}


def custom_expected(external=False):
    label = "@@fixture_alpha+//p:inspect" if external else "@@//p:inspect"
    short = "../fixture_alpha+/p/inspect/copy.txt" if external else "p/inspect/copy.txt"
    source = "external/fixture_alpha+/p/data.txt|../fixture_alpha+/p/data.txt|@@fixture_alpha+//p:data.txt\n"
    text = label + "\n" + source + source + short + "|" + label + "\n"
    prefix = "generated:" + ("external/fixture_alpha+/" if external else "") + "p/inspect/"
    return {prefix + "metadata.txt": text.encode(), prefix + "copy.txt": b"alpha repository\n"}


OUTPUT_CASES = (
    ("//p:sources", SOURCES, 0),
    ("@first//p:via_dep", {"source:external/fixture_beta+/p/data.txt": b"beta repository\n"}, 0),
    ("@first//p:via_main", {"source:p/data.txt": b"main repository\n"}, 0),
    ("@first//p:own_allowed", {"source:external/fixture_alpha+/p/data.txt": b"alpha repository\n"}, 0),
    ("//p:main_allowed", {"source:external/fixture_alpha+/p/data.txt": b"alpha repository\n"}, 0),
    ("@first//p:hidden", {}, 0),
    ("@first//p:forwarded", {}, 0),
    ("//p:inspect", custom_expected(), 2),
    ("@first//p:inspect", custom_expected(external=True), 2),
)


def output_case(native, reference, cache, runner, case):
    target, expected, count = case
    return [build_pair(native, reference, cache, runner, target, expected, counts,
                       target + "." + name)
            for name, counts in (("cold", (count, 0)), ("warm", (0, count)))]


def private_source(reference, scratch):
    source = reference.workspace / "repos/alpha/p/data.txt"
    if (reference.workspace.resolve().is_relative_to(FIXTURE.resolve())
            or not source.resolve().is_relative_to(scratch.resolve())):
        raise ValueError("source invalidation requires a private fixture copy")
    return source


def collision_builds(native, reference, scratch, runner):
    cache = scratch / "copies-native-cache"
    details = output_case(native, reference, cache, runner, ("//p:copies", copies_expected(), 3))
    source = private_source(reference, scratch)
    original = source.read_bytes()
    try:
        source.write_bytes(b"changed alpha repository\n")
        details.append(build_pair(native, reference, cache, runner, "//p:copies",
                                  copies_expected(source.read_bytes()), (1, 2), "copies.source-change"))
    finally:
        source.write_bytes(original)
    return details


def semantic_error(record, pattern):
    if re.search(r"bazel crashed|FATAL:|panicked at|internal (?:error|crash)",
                 record["stderr"] + record["stdout"], re.I):
        raise ValueError("crash is not a semantic repository rejection")
    return expected_error(record, pattern)


def error_parity(native, reference, cache, runner, case, registry=None):
    category, command, target, pattern = case
    records = {
        "native": native_invoke(native, cache, runner, reference.workspace,
                                category + ".native", command, target, jobs=4, registry=registry),
        "reference": reference.run(category + ".reference", command, target),
    }
    return {"category": category, "target": target, "command": command,
            **{name: semantic_error(record, pattern) for name, record in records.items()}}


def expected_check_count():
    return len(QUERY_CASES) + len(OUTPUT_CASES) + len(ERROR_CASES) + 2


def registry_fixture(scratch):
    """Create an authoritative registry MODULE distinct from the live source MODULE."""
    workspace = scratch / "fixture"
    shutil.copytree(FIXTURE, workspace)
    (workspace / "MODULE.bazel").write_text(
        'module(name = "fixture_root", version = "1.0")\n'
        'bazel_dep(name = "fixture_alpha", version = "1.0", repo_name = "first")\n')
    registry = scratch / "registry"
    metadata = registry / "modules/fixture_alpha/1.0"
    metadata.mkdir(parents=True)
    (metadata / "MODULE.bazel").write_text('module(name = "fixture_alpha", version = "1.0")\n')
    source = workspace / "repos/alpha"
    (metadata / "source.json").write_text(json.dumps({"type": "local_path", "path": str(source)}) + "\n")
    return workspace, registry, metadata


def registry_case(native, executable, scratch, runner):
    """native=None is an explicit official-only fixture probe, not parity."""
    workspace, registry, metadata = registry_fixture(scratch)
    reference = RepositoryReference(executable, workspace, scratch, runner, "registry")
    reference.registry_url = registry.as_uri()
    cache = scratch / "native-cache"
    target = "@first//p:copy"
    expected = {"generated:external/fixture_alpha+/p/copy.txt": b"alpha repository\n"}
    details = {"scope": "reference fixtures only" if native is None else "local registry source parity",
               "registry_url": reference.registry_url, "builds": [],
               "metadata": {name: file_evidence(metadata / name, include_bytes=True)
                            for name in ("MODULE.bazel", "source.json")},
               "source_module": file_evidence(workspace / "repos/alpha/MODULE.bazel", include_bytes=True)}
    source = private_source(reference, scratch)
    original = source.read_bytes()
    try:
        if native is None:
            query = reference.run("registry.query", "query", target)
            labels = parse_labels(require_success(query))
            if [canonical_label(label) for label in labels] != ["@@fixture_alpha+//p:copy"]:
                raise ValueError("local registry query mismatch")
            details["query"] = {"run_id": query["id"], "exact_labels": labels}
        else:
            details["query"] = query_parity(native, reference, cache, runner, target,
                                              ["@@fixture_alpha+//p:copy"], registry=reference.registry_url)
        for name, counts, contents in (("cold", (1, 0), original), ("warm", (0, 1), original),
                                      ("source-change", (1, 0), b"changed registry alpha\n")):
            source.write_bytes(contents)
            expected["generated:external/fixture_alpha+/p/copy.txt"] = contents
            if native is None:
                result = reference_build(reference, target, expected, "registry." + name)
            else:
                result = build_pair(native, reference, cache, runner, target, expected, counts,
                                    "registry." + name, registry=reference.registry_url)
            details["builds"].append(result)
        case = ("registry-module-authority", "build", "@first//p:via_dep", MAPPING_ERROR)
        if native is None:
            details["authority"] = semantic_error(reference.run(case[0], case[1], case[2]), case[3])
        else:
            details["authority"] = error_parity(native, reference, cache, runner, case, registry=reference.registry_url)
    finally:
        try:
            source.write_bytes(original)
        finally:
            reference.shutdown()
    return details


def reference_fixture_checks(reference, scratch):
    """Official-only fixture validation API; does NOT establish native parity."""
    details = {"scope": "reference fixtures only; no native parity claim", "queries": [], "builds": [], "errors": []}
    for target, expected in QUERY_CASES:
        record = reference.run(target + ".query", "query", target)
        labels = parse_labels(require_success(record))
        if sorted(canonical_label(label) for label in labels) != expected:
            raise ValueError("reference fixture query inventory mismatch")
        details["queries"].append({"run_id": record["id"], "exact_labels": labels})
    for target, expected, _ in OUTPUT_CASES + (("//p:copies", copies_expected(), 3),):
        for iteration in ("cold", "warm"):
            details["builds"].append(reference_build(reference, target, expected, target + "." + iteration))
    source = private_source(reference, scratch)
    original = source.read_bytes()
    try:
        source.write_bytes(b"changed alpha repository\n")
        details["builds"].append(reference_build(reference, "//p:copies", copies_expected(source.read_bytes()), "copies.source-change"))
    finally:
        source.write_bytes(original)
    for category, command, target, pattern in ERROR_CASES:
        details["errors"].append({"category": category,
                                  **semantic_error(reference.run(category, command, target), pattern)})
    return details


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/repository-validation.json")
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
        "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        "reference_version_required": "9.3.0", "test_commands_executed": False,
        "scope": ["root-relative local_path_override roots and per-repository apparent mappings",
                  "three identical package/basename genrules, isolated bytes, cold/warm/source-change",
                  "external source File carried by nondefault provider through forwarding",
                  "definition-relative Label defaults, canonical ctx.label and File owner",
                  "external source exec/short paths, declare_file sibling paths in main/external repositories",
                  "repository-aware private/__pkg__ visibility and canonical main references",
                  "local registry source.json selects live source root; registry MODULE is authoritative"],
        "limitations": ["Scoped fixture parity only, not full Bazel compatibility.",
                        "Query spelling differs: exact outputs retained, identity compared using explicit root aliases.",
                        "Artifact keys remove only bazel-out/{rust-fastbuild,k8-fastbuild}/bin configuration prefixes.",
                        "Bytes, SHA256, sizes and repository-relative identities must match; modes are recorded, not equated.",
                        "Native trusted cat/write actions are unsandboxed. Fresh caches are not OS-cold.",
                        "Native action counts are exact; reference internal action counts are not compared.",
                        "Only genuine official tool dependencies may download; no fake repository modules or reused cache."],
    }
    runner = Runner(report, args.timeout)
    reference, scratch, interrupted = None, None, False
    try:
        validate_executable(args.native)
        validate_executable(args.reference, official=True)
        report["executables"] = {"native": file_evidence(args.native), "reference": file_evidence(args.reference)}
        scratch = Path(tempfile.mkdtemp(prefix="repository-validation-", dir="/tmp/opencode"))
        report["scratch_directory"] = str(scratch)
        workspace = scratch / "fixture"
        shutil.copytree(FIXTURE, workspace)
        reference = RepositoryReference(args.reference, workspace, scratch, runner, "repositories")
        report["reference_release"] = require_success(reference.run("reference.release", "info", "release")).strip()
        if report["reference_release"] != "release 9.3.0":
            raise ValueError("reference binary is not official release 9.3.0")
        for target, expected in QUERY_CASES:
            check(report, "query." + target, lambda t=target, e=expected: query_parity(
                args.native, reference, scratch / "query-native-cache", runner, t, e))
        check(report, "copies.collision-invalidation", lambda: collision_builds(args.native, reference, scratch, runner))
        for index, case in enumerate(OUTPUT_CASES):
            check(report, "output." + case[0], lambda c=case, i=index: output_case(
                args.native, reference, scratch / ("output-native-" + str(i)), runner, c))
        for case in ERROR_CASES:
            check(report, "error." + case[0], lambda c=case: error_parity(
                args.native, reference, scratch / "errors-native-cache", runner, c))
        check(report, "registry.local-source", lambda: registry_case(
            args.native, args.reference, scratch / "registry-case", runner))
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
    finally:
        if reference is not None:
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
        if (len(report["checks"]) == expected_check_count() and not report["errors"]
                and all(item["status"] == "PASS" for item in report["checks"])):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print("{} (scoped repository parity only): {}".format(report["status"], args.output))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
