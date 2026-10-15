#!/usr/bin/env python3
"""Scoped unified-patch graph/source differential, not full patch parity.

Private file-URI fixtures, one genuine pinned Bazel 9.3 server, no wrappers or
fake built-ins. Native-only safety/capability probes are explicitly not parity.
"""

import argparse
import base64
import datetime
import difflib
import hashlib
import io
import json
from pathlib import Path
import re
import shutil
import signal
import sys
import tarfile
import tempfile

from validate import (ROOT, COMMON_FLAGS, Reference, Runner, file_evidence,
                      positive_seconds, reference_output_paths, require_success,
                      validate_executable, write_report)
from validate_modules import compare_graphs, expected_graph, parse_graph


REFERENCE_SHA256 = "d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8"
NS = "opencode_patchcheck_"
DEP = NS + "dep"
PROFILE = ["--noinclude_builtin", "--extension_info=hidden", "--output=json"]
GRAPH_ARCHIVE_PLACEHOLDER = b"graph source archive deliberately unavailable\n"
MODULE = 'module(name = "' + DEP + '", version = "1.0")\n# registry authority café\n'
LOCAL_MODULE = MODULE.replace("registry authority", "local final")
ARCHIVE_MODULE = MODULE.replace("registry authority", "archive")
OVERLAY_MODULE = MODULE.replace("registry authority", "overlay")
REMOTE_MODULE = MODULE.replace("registry authority", "remote")
BUILD = (b'package(default_visibility = ["//visibility:public"])\n'
         b'filegroup(name = "sources", srcs = ["MODULE.bazel", "data", "created"])\n'
         b'genrule(name = "copy", srcs = ["data"], outs = ["copy.txt"], cmd = "cat $(location data) > $@")\n')


def data(stage):
    return ("header\n" + stage + " café\nfooter\n").encode()


def digest(value):
    return hashlib.sha256(value).hexdigest()


def sri(value):
    return "sha256-" + base64.b64encode(hashlib.sha256(value).digest()).decode("ascii")


def patch_text(path, before, after):
    """Generate explicitly separated file sections, not a Python patch applier."""
    body = "".join(difflib.unified_diff(
        before.splitlines(keepends=True), after.splitlines(keepends=True),
        fromfile="a/" + path if before else "/dev/null",
        tofile="b/" + path if after else "/dev/null"))
    return "diff --git a/" + path + " b/" + path + "\n" + body if body else ""


def module_dep(version):
    return MODULE + 'bazel_dep(name = "' + NS + 'leaf", version = "' + version + '")\n'


def case(name, mode="graph", *, expected="success", diagnostic=None, leaf=None, patch_strip=1, **options):
    return dict(name=name, mode=mode, expected=expected, diagnostic=diagnostic,
                leaf=leaf, patch_strip=patch_strip,
                kind="native_only" if name.startswith("native.") else "differential", **options)


CASES = [
    case("graph.dependency-before-mvs", leaf="2.0", module_start="1.0", shifts=["2.0"]),
    case("graph.nonmodule-unmaterialized-source", source_patch=True),
    case("graph.ordered-module-patches", leaf="3.0", module_start="1.0", shifts=["2.0", "3.0"]),
    case("graph.patch-commands-ignored", commands=True),
    case("graph.mixed-root-source-nested", leaf="2.0", module_start="1.0", shifts=["2.0"], mixed=True),
    case("graph.root-self-alias", leaf="2.0", module_start="1.0", shifts=["2.0"], self_alias=True),
    case("graph.canonical-external-skipped", canonical=True),
    case("graph.module-create-delete-skipped", create_delete=True),
    case("graph.dot-prefix-is-not-root", module_start="1.0", shifts=["2.0"], leaf="1.0", dot_prefix=True),
    case("graph.empty-component-strip", module_start="1.0", shifts=["2.0"], leaf="2.0", empty_component=True, patch_strip=2),
    case("negative.missing-package", expected="reject", diagnostic="missing_package", missing_package=True),
    case("negative.missing-patch-file", expected="reject", diagnostic="missing_file", missing_file=True),
    case("negative.unknown-apparent-label", expected="reject", diagnostic="unknown_label", unknown=True),
    case("native.nonmodule-no-source", source_patch=True, no_source_descriptor=True),
    case("native.malformed-skipped-source", expected="reject", diagnostic="structure", malformed=True),
    case("native.traversal-skipped-source", expected="reject", diagnostic="path", traversal=True),
    # N26: Bazel's PatchUtil.applyToSingleFile skips these non-MODULE sections
    # (PatchUtil.java:448-660); they are differentials, no longer native rejections.
    case("graph.adjacent-plain-sections-skipped", adjacent_plain=True),
    case("graph.git-binary-skipped", binary=True),
    case("graph.rename-skipped", rename=True),
    case("graph.mode-change-skipped", mode_change=True),
    case("materialize.transform-order", "source"),
    case("native.cache-local-patch-edit", "cache"),
    case("native.invalid-local-no-publication", "source", expected="reject", diagnostic="context", invalid_local=True),
    case("native.remote-integrity-descriptor-rejected", "source", expected="reject", diagnostic="integrity", bad_digest=True),
    case("native.commands-materialization", "source", expected="reject", diagnostic="unsupported", commands=True),
    case("native.canonical-materialization", "source", expected="reject", diagnostic="unsupported", canonical=True),
    case("native.local-path-transform", "source", expected="reject", diagnostic="unsupported", local_path=True),
]
PLAN = [(item["name"], item["kind"]) for item in CASES]
DIAGNOSTICS = {
    # Semantic classes, plus the captured official main-repository-label rejection.
    "missing_package": r"(?:no such package|package[^\n]*(?:missing|not found|does not exist|BUILD)|patch[^\n]*package)",
    "missing_file": r"(?:patch[^\n]*(?:not found|does not exist|No such file|missing)|(?:no such|missing|not found)[^\n]*patch)",
    "unknown_label": (r"(?:unknown[^\n]*(?:repository|repo|apparent)|(?:repository|repo)[^\n]*(?:not visible|could not be resolved|not defined))"
                      r"|Error in single_version_override: invalid label in 'patches': only patches in the main repository can be applied, not from '@[A-Za-z][A-Za-z0-9_.-]*'"),
    "structure": r"patch[^\n]*(?:hunk|malformed|invalid|count|truncated)",
    "section_boundary": r"unsupported patch section boundary|ambiguous[^\n]*patch|patch[^\n]*separator",
    "path": r"patch[^\n]*(?:unsafe|traversal|path|outside)",
    "unsupported": r"(?:patch[^\n]*(?:unsupported|not supported|not implemented)|(?:unsupported|not supported)[^\n]*(?:patch|rename|binary|mode|local.path))",
    "context": r"patch[^\n]*(?:context|match|hunk|apply|failed)",
    "integrity": r"(?:patch[^\n]*(?:integrity|digest|checksum|base64)|(?:integrity|digest|checksum)[^\n]*(?:invalid|malformed|mismatch|base64))",
}
UNRELATED = re.compile(r"panicked|internal error|FATAL:|crashed|falling back|fallback to|"
                       r"SSLHandshake|certificate|connection refused|network unreachable|"
                       r"failed to download|could not download|syntax error|unexpected token", re.I)


def success(record):
    text = require_success(record)
    if (record.get("spawn_error") or record.get("interrupted") or
            UNRELATED.search(record["stderr"])):
        raise ValueError("unrelated failure, fallback, or interruption")
    return text


def require_rejection(record, diagnostic):
    code = record["returncode"]
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record["timed_out"]
            or record.get("spawn_error") or record.get("interrupted") or record["stdout"].strip()
            or UNRELATED.search(record["stderr"])
            or not re.search(DIAGNOSTICS[diagnostic], record["stderr"], re.I)):
        raise ValueError("expected controlled patch rejection: " + diagnostic)


def snapshot(files):
    return {str(path): file_evidence(path, include_bytes=True) for path in files}


class RecordingRunner(Runner):
    def run(self, name, argv, cwd, timeout=None):
        start = len(self.report["runs"])
        try:
            return super().run(name, argv, cwd, timeout)
        finally:
            for record in self.report["runs"][start:]:
                for field in ("stdout", "stderr"):
                    raw = (base64.b64decode(record[field + "_base64"]) if field + "_base64" in record
                           else record[field].encode())
                    record[field + "_sha256"] = digest(raw)


class PatchReference(Reference):
    def flags(self):
        return ["--repository_cache=" + str(self.output_base.parent / "repository-cache"),
                "--repo_contents_cache=" + str(self.output_base.parent / "repository-contents"),
                "--registry=" + self.registry.as_uri(), "--registry=https://bcr.bazel.build"]

    def run(self, name, command, *args):
        if command == "mod":
            self.started = True
            return self.runner.run(name, self.startup + ["mod", "graph"] + list(COMMON_FLAGS)
                                   + self.flags() + PROFILE, self.workspace)
        return super().run(name, command, *self.flags(), *args)


def native_command(executable, cache, registry, command, target=None):
    argv = [str(executable), "--ignore_all_rc_files", "--cache-dir=" + str(cache), command]
    if command == "mod":
        argv += ["graph"] + PROFILE
    argv += ["--lockfile_mode=off", "--registry=" + registry.as_uri()]
    return argv + ([target] if target else [])


def archive_bytes():
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in sorted({"MODULE.bazel": ARCHIVE_MODULE.encode(), "BUILD.bazel": BUILD,
                                      "data": data("archive"), "obsolete": b"remove me\n"}.items()):
            entry = tarfile.TarInfo("pkg/" + name)
            entry.size, entry.mode, entry.mtime = len(contents), 0o644, 0
            archive.addfile(entry, io.BytesIO(contents))
    return stream.getvalue()


def local_source_patch(final="local final", invalid=False):
    module = patch_text("MODULE.bazel", MODULE, LOCAL_MODULE)
    source = patch_text("data", data("remote two").decode(), data(final).decode())
    # Deliberately inaccurate line coordinates exercise exact-context offset search.
    source = source.replace("@@ -1,3 +1,3 @@", "@@ -8,3 +8,3 @@")
    if invalid:
        source = source.replace(" header\n", " wrong context\n")
    return (module + source + patch_text("created", "", "créé\n")
            + patch_text("obsolete", "remove me\n", ""))


def materialize(case_root, workspace, item):
    """All registry URIs are case-unique; patch mapping insertion order is semantic."""
    registry = case_root / "registry"
    entry = registry / "modules" / DEP / "1.0"
    files = {}

    def put(path, contents):
        path.parent.mkdir(parents=True, exist_ok=True)
        contents = contents.encode() if isinstance(contents, str) else contents
        path.write_bytes(contents)
        files[path] = contents

    put(workspace / "BUILD.bazel", b"")
    put(registry / "bazel_registry.json", "{}\n")
    put(entry.parent / "metadata.json", '{"versions":["1.0"],"yanked_versions":{}}\n')
    original = module_dep(item["module_start"]) if item.get("module_start") else MODULE
    put(entry / "MODULE.bazel", original)
    graph_archive = case_root / "archive-must-not-be-fetched.tar"
    graph_descriptor = {"url": graph_archive.as_uri(), "integrity": sri(GRAPH_ARCHIVE_PLACEHOLDER)}
    for version in ("1.0", "2.0", "3.0"):
        leaf = registry / "modules" / (NS + "leaf") / version
        put(leaf / "MODULE.bazel", 'module(name = "' + NS + 'leaf", version = "' + version + '")\n')
        if item["mode"] == "graph":
            put(leaf / "source.json", json.dumps(graph_descriptor) + "\n")
    put(registry / "modules" / (NS + "leaf") / "metadata.json",
        '{"versions":["1.0","2.0","3.0"],"yanked_versions":{}}\n')
    patches = []
    previous = item.get("module_start")
    for index, version in enumerate(item.get("shifts", [])):
        text = patch_text("MODULE.bazel", module_dep(previous), module_dep(version))
        if item.get("dot_prefix"):
            text = text.replace("diff --git a/MODULE.bazel b/MODULE.bazel", "diff --git ./a/MODULE.bazel ./b/MODULE.bazel")
            text = text.replace("--- a/MODULE.bazel", "--- ./a/MODULE.bazel").replace("+++ b/MODULE.bazel", "+++ ./b/MODULE.bazel")
        if item.get("empty_component"):
            text = text.replace("diff --git a/MODULE.bazel b/MODULE.bazel", "diff --git a//MODULE.bazel b//MODULE.bazel")
            text = text.replace("--- a/MODULE.bazel", "--- a//MODULE.bazel").replace("+++ b/MODULE.bazel", "+++ b//MODULE.bazel")
        patches.append(text)
        previous = version
    if item.get("source_patch"):
        patches.append(patch_text("data", "absent source café\n", "changed café\n"))
    if item.get("mixed"):
        patches[-1] += (patch_text("data", "unavailable\n", "changed\n")
                        + patch_text("nested/MODULE.bazel", "nested old\n", "nested new\n"))
    if item.get("create_delete"):
        patches += [patch_text("MODULE.bazel", "", "this is not Starlark\n"),
                    patch_text("MODULE.bazel", "not the registry module\n", "")]
    if item.get("malformed"):
        patches.append("--- a/data\n+++ b/data\n@@ -1,2 +1,2 @@\n-old\n+new\n")
    if item.get("adjacent_plain"):
        # Valid individual sections, intentionally missing an explicit separator.
        patches.append(patch_text("data", "old\n", "new\n").split("\n", 1)[1]
                       + patch_text("nested/MODULE.bazel", "nested old\n", "nested new\n").split("\n", 1)[1])
    if item.get("traversal"):
        patches.append(patch_text("../escape", "old\n", "new\n"))
    if item.get("binary"):
        patches.append("diff --git a/data b/data\nGIT binary patch\nliteral 1\nA00000\n")
    if item.get("rename"):
        patches.append("diff --git a/data b/other\nsimilarity index 100%\nrename from data\nrename to other\n")
    if item.get("mode_change"):
        patches.append("diff --git a/data b/data\nold mode 100644\nnew mode 100755\n")
    descriptor = None
    if item["mode"] == "graph":
        # Official mod graph reads repo specs, but must not fetch fixture archives.
        # Only the explicitly native-only probe omits the dependency descriptor.
        if not item.get("no_source_descriptor"):
            descriptor = graph_descriptor
            put(entry / "source.json", json.dumps(descriptor) + "\n")
    else:
        patches = [local_source_patch(invalid=item.get("invalid_local", False))]
        blob = archive_bytes()
        put(case_root / "source.tar", blob)
        remote = {"z-first.patch": patch_text("data", data("overlay").decode(), data("remote one").decode())
                  + patch_text("MODULE.bazel", OVERLAY_MODULE, REMOTE_MODULE),
                  "a-second.patch": patch_text("data", data("remote one").decode(), data("remote two").decode())}
        descriptor = {"url": (case_root / "source.tar").as_uri(), "integrity": sri(blob),
                      "strip_prefix": "pkg", "overlay": {"data": sri(data("overlay")),
                                                          "MODULE.bazel": sri(OVERLAY_MODULE.encode())},
                      "patch_strip": 1, "patches": {name: sri(text.encode()) for name, text in remote.items()}}
        for name, text in remote.items():
            put(entry / "patches" / name, text)
        put(entry / "overlay/data", data("overlay"))
        put(entry / "overlay/MODULE.bazel", OVERLAY_MODULE)
        if item.get("bad_digest"):
            descriptor["patches"]["z-first.patch"] = "sha256-not-base64!"
        if item.get("local_path"):
            source = case_root / "local-source"
            for name, contents in {"MODULE.bazel": MODULE.encode(), "BUILD.bazel": BUILD,
                                   "data": data("remote two"), "obsolete": b"remove me\n"}.items():
                put(source / name, contents)
            descriptor = {"type": "local_path", "path": str(source)}
        # NEVER sort_keys: Bazel applies the remote patch JSON mapping in order.
        put(entry / "source.json", json.dumps(descriptor, ensure_ascii=False) + "\n")
    labels = []
    for index, text in enumerate(patches):
        filename = "local-" + str(index) + ".patch"
        put(workspace / filename, text)
        labels.append(("@" + NS + "root" if item.get("self_alias") else "") + "//:" + filename)
    if item.get("missing_package"):
        put(workspace / "not-a-package/missing.patch", patch_text("data", "a\n", "b\n"))
        labels = ["//not-a-package:missing.patch"]
    if item.get("missing_file"):
        labels = ["//:missing.patch"]
    if item.get("unknown"):
        labels = ["@not_visible//:missing.patch"]
    if item.get("canonical"):
        labels = ["@@opencode_patchcheck_absent+//:missing.patch"]
    marker = case_root / "COMMAND_MUST_NOT_RUN"
    root = ('module(name = "' + NS + 'root", version = "1.0")\n'
            'bazel_dep(name = "' + DEP + '", version = "1.0", repo_name = "patched")\n'
            'single_version_override(module_name = "' + DEP + '", patches = ' + json.dumps(labels)
            + ', patch_strip = ' + str(item["patch_strip"]))
    if item.get("commands"):
        root += ', patch_cmds = ' + json.dumps(["printf forbidden > " + str(marker)])
    put(workspace / "MODULE.bazel", root + ")\n")
    return {"registry": registry, "files": files, "marker": marker, "descriptor": descriptor,
            "entry": entry, "graph_archive": graph_archive,
            "patches": [workspace / ("local-" + str(i) + ".patch") for i in range(len(patches))]}


def graph_expected(item):
    node = {"module": "dep@1.0", "alias": "patched"}
    if item["leaf"]:
        node["dependencies"] = [{"module": "leaf@" + item["leaf"]}]
    return expected_graph({"expected": [node]}, {"namespace": NS})


def expected_sources(final="local final"):
    return {"MODULE.bazel": LOCAL_MODULE.encode(), "data": data(final), "created": "créé\n".encode()}


def canonical(target):
    return target.replace("@patched//", "@@" + DEP + "+//", 1)


def query_result(record, native=False):
    labels = success(record).splitlines()
    expected = ["@patched//:copy", "@patched//:sources"]
    mapped = [canonical(label) for label in labels]
    if sorted(mapped) != sorted(map(canonical, expected)) or (native and labels != mapped):
        raise ValueError("query must identify exactly the fixture targets")
    return labels


def tree_roots(cache):
    directory = cache / "repositories/trees"
    return sorted(path for path in directory.glob("*") if path.is_dir() and not path.is_symlink()
                  and not path.name.startswith("."))


def no_publication(cache):
    if list((cache / "repositories/trees").glob("*")):
        raise ValueError("rejection/graph published or staged a repository tree")


def native_paths(record, target, scratch):
    paths = []
    for line in success(record).splitlines():
        fields = line.split("\t")
        if len(fields) != 2 or fields[0] != canonical(target):
            raise ValueError("invalid native source/output label")
        path = Path(fields[1])
        if (not path.is_absolute() or ".." in path.parts or path in paths
                or not path.resolve().is_relative_to(scratch.resolve())):
            raise ValueError("unsafe/duplicate native output path")
        paths.append(path)
    return paths


def manifest(paths, expected, source_root=None):
    result = {}
    for path in paths:
        if source_root is not None:
            name = path.relative_to(source_root).as_posix()
        else:
            parts = path.parts
            if parts.count("bazel-out") != 1:
                raise ValueError("generated artifact has no unique configuration")
            suffix = parts[parts.index("bazel-out") + 1:]
            if (len(suffix) != 5 or suffix[0] not in ("rust-fastbuild", "k8-fastbuild")
                    or suffix[1:4] != ("bin", "external", DEP + "+")):
                raise ValueError("wrong generated repository identity")
            name = suffix[4]
        if name not in expected or name in result:
            raise ValueError("unexpected/duplicate artifact identity")
        result[name] = file_evidence(path, expected[name], include_bytes=True)
    if set(result) != set(expected):
        raise ValueError("incomplete artifact inventory")
    return result


def run_case(args, runner, scratch, workspace, reference, item):
    check = {"name": item["name"], "kind": item["kind"], "input": item, "status": "FAIL",
             "parity_claim": False, "run_ids": {}, "artifacts": {}}
    runner.report["checks"].append(check)
    fixture = None
    cache = scratch / "native" / item["name"]

    def invoke(engine, phase, command, target=None):
        name = item["name"] + "." + engine + "." + phase
        if engine == "native":
            record = runner.run(name, native_command(args.native, cache, fixture["registry"], command, target), workspace)
        else:
            record = reference.run(name, command, *(["--output=files", target] if command == "cquery"
                                                   else [target] if target else []))
        check["run_ids"][engine + "." + phase] = record["id"]
        return record

    def build(engine, target, phase, final="local final"):
        result = invoke(engine, phase, "build", target)
        expected = expected_sources(final) if target.endswith(":sources") else {"copy.txt": data(final)}
        if engine == "native":
            paths = native_paths(result, target, scratch)
            roots = tree_roots(cache)
            candidates = [root for root in roots if (root / "data").read_bytes() == data(final)]
            if len(candidates) != 1:
                raise ValueError("missing or ambiguous published transformed tree")
            source_root = candidates[0]
        else:
            success(result)
            listing = invoke(engine, phase + ".files", "cquery", target)
            paths = reference_output_paths(success(listing), workspace, reference.output_base)
            source_root = reference.output_base / "execroot/_main/external" / (DEP + "+")
        if target.endswith(":sources") and (source_root / "obsolete").exists():
            raise ValueError("source patch deletion did not remove obsolete")
        check["artifacts"][engine + "." + phase] = manifest(
            paths, expected, source_root if target.endswith(":sources") else None)
        return source_root

    try:
        fixture = materialize(scratch / "cases" / item["name"], workspace, item)
        check["files_before"] = snapshot(fixture["files"])
        check["registry"] = fixture["registry"].as_uri()
        check["source_descriptor"] = fixture["descriptor"]
        check["source_json_absent"] = not (fixture["entry"] / "source.json").exists()
        reference.registry = fixture["registry"]
        engines = ("native", "reference") if item["kind"] == "differential" else ("native",)
        if item["mode"] == "graph" or item["expected"] == "reject":
            records = {engine: invoke(engine, "result", "mod" if item["mode"] == "graph" else "query",
                                      None if item["mode"] == "graph" else "@patched//:all") for engine in engines}
            if item["expected"] == "reject":
                for engine, record in records.items():
                    try:
                        require_rejection(record, item["diagnostic"])
                    except ValueError:
                        if engine == "reference":
                            check["status"] = "REFERENCE_FAILURE"
                        raise
            else:
                for record in records.values():
                    success(record)
                check["expected_graph"] = graph_expected(item)
                if item["kind"] == "differential":
                    compare_graphs(parse_graph(records["native"]), parse_graph(records["reference"]), check["expected_graph"])
                elif parse_graph(records["native"]) != check["expected_graph"]:
                    raise ValueError("native-only graph differs from explicit expectation")
            no_publication(cache)
            check["no_publication"] = True
        else:
            for engine in engines:
                query_result(invoke(engine, "query", "query", "@patched//:all"), native=engine == "native")
                root = build(engine, "@patched//:sources", "sources")
                build(engine, "@patched//:copy", "copy")
                if item["mode"] == "cache":
                    path = fixture["patches"][0]
                    original = path.read_bytes()
                    check["cache_root_before"] = str(root)
                    try:
                        path.write_text(local_source_patch("edited final"), encoding="utf-8")
                        check["edited_patch"] = file_evidence(path, include_bytes=True)
                        changed_root = build(engine, "@patched//:sources", "edited.sources", "edited final")
                        build(engine, "@patched//:copy", "edited.copy", "edited final")
                        check["cache_root_after"] = str(changed_root)
                        if changed_root == root or (root / "data").read_bytes() != data("local final"):
                            raise ValueError("patch edit did not produce a new immutable repository key")
                    finally:
                        path.write_bytes(original)
        check["parity_claim"] = item["kind"] == "differential"
        check["status"] = "PASS"
    except (OSError, ValueError) as exc:
        check["error"] = str(exc)
    finally:
        if fixture is not None:
            try:
                check["files_after"] = snapshot(fixture["files"])
                check["inputs_unchanged"] = check["files_before"] == check["files_after"]
                check["command_marker_absent"] = not fixture["marker"].exists()
                if item["mode"] == "graph":
                    check["graph_archive_absent"] = not fixture["graph_archive"].exists()
                    if not check["graph_archive_absent"]:
                        raise ValueError("graph fixture archive unexpectedly exists")
                if not check["inputs_unchanged"] or not check["command_marker_absent"]:
                    raise ValueError("CLI mutated fixture inputs or executed patch_cmds")
            except (OSError, ValueError, KeyError) as exc:
                check["status"], check["parity_claim"], check["error"] = "FAIL", False, str(exc)


def evidence_matches(evidence, expected=None):
    raw = base64.b64decode(evidence["bytes_base64"], validate=True)
    return (evidence["sha256"] == digest(raw) and evidence["size_bytes"] == len(raw)
            and (expected is None or raw == expected))


def report_passes(report):
    """Recheck run/evidence links after cleanup; empty or forged PASS is insufficient."""
    try:
        if ([(c["name"], c["kind"]) for c in report["checks"]] != PLAN or report["errors"]
                or any(report[key] is not True for key in ("scratch_cleaned", "reference_shutdown_attempted", "sources_unchanged"))
                or report["reference_release"] != "bazel 9.3.0"
                or report["executables"]["reference"]["sha256"] != REFERENCE_SHA256):
            return False
        runs = report["runs"]
        if [r["id"] for r in runs] != list(range(len(runs))):
            return False
        for record in runs:
            for field in ("stdout", "stderr"):
                raw = (base64.b64decode(record[field + "_base64"], validate=True) if field + "_base64" in record
                       else record[field].encode())
                if digest(raw) != record[field + "_sha256"]:
                    return False
        for item, check in zip(CASES, report["checks"]):
            if (check["input"] != item or check["status"] != "PASS" or check.get("error")
                    or check["parity_claim"] != (item["kind"] == "differential")
                    or check["inputs_unchanged"] is not True or check["command_marker_absent"] is not True
                    or not check["files_before"] or check["files_before"] != check["files_after"]
                    or not all(evidence_matches(e) for e in check["files_before"].values())):
                return False
            engines = ("native", "reference") if item["kind"] == "differential" else ("native",)
            phases = ["result"] if item["mode"] == "graph" or item["expected"] == "reject" else ["query", "sources", "copy"]
            wanted = {engine + "." + phase for engine in engines for phase in phases}
            if "sources" in phases and "reference" in engines:
                wanted.update(("reference.sources.files", "reference.copy.files"))
            if item["mode"] == "cache":
                wanted.update(("native.edited.sources", "native.edited.copy"))
                if check["cache_root_before"] == check["cache_root_after"] or not evidence_matches(check["edited_patch"]):
                    return False
            if set(check["run_ids"]) != wanted:
                return False
            records = {}
            for key, index in check["run_ids"].items():
                if type(index) is not int or index < 0:
                    return False
                record = runs[index]
                if record["name"] != item["name"] + "." + key or not record["argv"] or not record["cwd"]:
                    return False
                records[key] = record
                if item["expected"] == "reject":
                    require_rejection(record, item["diagnostic"])
                else:
                    success(record)
                    if key.endswith("query"):
                        query_result(record, native=key.startswith("native."))
            if item["mode"] == "graph" and item["expected"] == "success":
                if item["kind"] == "differential":
                    compare_graphs(parse_graph(records["native.result"]), parse_graph(records["reference.result"]), graph_expected(item))
                elif parse_graph(records["native.result"]) != graph_expected(item):
                    return False
            if phases == ["result"]:
                if check["no_publication"] is not True or check["artifacts"]:
                    return False
                if item["mode"] == "graph":
                    if (check["source_json_absent"] is not bool(item.get("no_source_descriptor"))
                            or check["graph_archive_absent"] is not True):
                        return False
            else:
                artifact_keys = wanted - {k for k in wanted if k.endswith(("query", ".files"))}
                if set(check["artifacts"]) != artifact_keys:
                    return False
                for key, manifest_value in check["artifacts"].items():
                    final = "edited final" if ".edited." in key else "local final"
                    expected = expected_sources(final) if key.endswith("sources") else {"copy.txt": data(final)}
                    if set(manifest_value) != set(expected) or not all(
                            evidence_matches(manifest_value[name], value) for name, value in expected.items()):
                        return False
        return True
    except (KeyError, TypeError, ValueError, IndexError, AttributeError):
        return False


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/patch-validation.json")
    parser.add_argument("--scratch-parent", type=Path, default=Path("/tmp/opencode"))
    parser.add_argument("--timeout", type=positive_seconds, default=180)
    return parser


def validate_binaries(args):
    for name in ("native", "reference"):
        validate_executable(getattr(args, name), official=True)
    result = {name: file_evidence(getattr(args, name)) for name in ("native", "reference")}
    if result["reference"]["sha256"] != REFERENCE_SHA256:
        raise ValueError("reference SHA256 differs from pinned official Bazel 9.3.0 Linux x86_64")
    return result


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "output", "scratch_parent"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    sources = [Path(__file__).resolve(), ROOT / "scripts/validate.py", ROOT / "scripts/validate_modules.py",
               ROOT / "tests/test_validate_patches.py"]
    if (args.output in [args.native, args.reference, *sources]
            or any(args.output.is_relative_to(ROOT / directory) for directory in ("scripts", "tests"))):
        raise ValueError("report must not overwrite executables, harness sources, or fixtures")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "status": "FAIL", "checks": [], "runs": [], "errors": [], "cleanup": [],
              "scratch_cleaned": False, "reference_shutdown_attempted": False, "sources_unchanged": False,
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              "planned_checks": CASES, "case_inputs_sha256": digest(json.dumps(CASES, sort_keys=True).encode()),
              "reference_version_required": "9.3.0", "reference_sha256_required": REFERENCE_SHA256,
              "test_commands_executed": False,
              "limitations": ["Scoped UTF-8 unified text modifications/create/delete only; not general Git patch parity.",
                              "PatchUtil port (java-diff-utils applyFuzzy(..., 2)); non-MODULE adjacent-plain, binary, rename and mode-change sections are skipped during discovery like Bazel.",
                              "Discovery patches only old==new==root MODULE.bazel; validates skipped paths/structure too.",
                              "Graph parity supplies valid source descriptors with nonexistent archives; no claim that official graph skips source.json.",
                              "Missing dependency source.json is a separate native-only graph laziness probe.",
                              "Patch strip counts lexical slashes before normalizing dot or empty components.",
                              "MODULE create/delete and canonical external labels skipped in discovery; unknown apparent labels fail.",
                              "patch_cmds never executed; commands/canonical external/local_path transforms are native-only materialization boundaries.",
                              "Archive -> overlay -> ordered registry patches -> original registry MODULE -> ordered local patches.",
                              "Local patch edit uses fresh processes and checks distinct immutable tree keys; no mid-process race claim.",
                              "Native-only malformed/path/unsupported/integrity checks are not official rejection parity.",
                              "Malformed remote integrity probe tests descriptor rejection, not a downloaded patch-byte digest mismatch.",
                              "Diagnostic classes include a captured official label rejection; synthetic test responses are not CLI evidence.",
                              "Exact graph JSON and output bytes; query alone is not source-content evidence.",
                              "One private reference server; shared private genuine caches; BCR only for real built-ins, no stubs.",
                              "No wrapper, real main repository, credential changes, external archive tools, or network sandbox claim."]}
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report["sources_before"] = snapshot(sources)
        report["executables"] = validate_binaries(args)
        scratch = Path(tempfile.mkdtemp(prefix="patch-validation-", dir=args.scratch_parent))
        report["scratch_directory"] = str(scratch)
        workspace = scratch / "workspace"
        workspace.mkdir()
        reference = PatchReference(args.reference, workspace, scratch, runner, "patches")
        reference.started = True
        version = runner.run("reference.version", reference.startup + ["version", "--gnu_format"], workspace)
        report["reference_release"] = success(version).strip()
        if report["reference_release"] != "bazel 9.3.0":
            raise ValueError("reference is not official Bazel 9.3.0")
        for item in CASES:
            run_case(args, runner, scratch, workspace, reference, item)
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except Exception as exc:
        report["errors"].append(type(exc).__name__ + ": " + str(exc))
    finally:
        if reference is not None:
            try:
                report["reference_shutdown_attempted"] = True
                reference.shutdown()
            except Exception as exc:
                report["errors"].append("reference shutdown: " + str(exc))
        if scratch is not None:
            try:
                shutil.rmtree(scratch)
                report["scratch_cleaned"] = not scratch.exists()
            except OSError as exc:
                report["errors"].append("scratch cleanup: " + str(exc))
        try:
            report["sources_after"] = snapshot(sources)
            report["sources_unchanged"] = report.get("sources_before") == report["sources_after"]
        except OSError as exc:
            report["errors"].append("source evidence: " + str(exc))
        report["completed_differential_cases"] = sum(c["kind"] == "differential" and c["status"] == "PASS"
                                                      for c in report["checks"])
        report["build_commands_executed"] = any("build" in r["argv"] for r in report["runs"])
        if report_passes(report):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print("{} (scoped UTF-8 patch fixtures only): {}".format(report["status"], args.output))
    return 130 if interrupted else 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
