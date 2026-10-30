#!/usr/bin/env python3
"""Capture a genuine Bazel 9.3 builtin discovery closure, never synthetic MODULEs.

The official reference, not the native resolver, supplies selected versions. All
registryFileHashes (including unselected MODULE versions) are mirrored verbatim.
Publication is atomic and new-only; a failed capture only writes the report.
"""

import argparse
import base64
import concurrent.futures
import datetime
import hashlib
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile
import urllib.request

from validate import ROOT, Reference, require_success, validate_executable, write_report
from validate_direct_repositories import RecordingRunner


REFERENCE_SHA256 = "7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c"
BCR = "https://bcr.bazel.build"
ROOT_MODULE = 'module(name = "opencode_builtin_fixture", version = "1.0")\n'
CASES = (
    ("empty-root", ROOT_MODULE),
    ("older-platforms", ROOT_MODULE + 'bazel_dep(name = "platforms", version = "0.0.10")\n'),
)
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
REGISTRY_PATH = re.compile(
    r"(?:bazel_registry\.json|modules/[a-z][a-z0-9._-]*/(?:metadata\.json|[a-zA-Z0-9._+-]+/"
    r"(?:MODULE\.bazel|source\.json|patches/[a-zA-Z0-9._+-]+)))\Z")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def registry_path(url):
    if not isinstance(url, str) or not url.startswith(BCR + "/"):
        raise ValueError("untrusted registry URL: " + str(url))
    relative = url[len(BCR) + 1:]
    if not REGISTRY_PATH.fullmatch(relative) or any(p in (".", "..") for p in relative.split("/")):
        raise ValueError("unsafe or unexpected registry path: " + relative)
    return relative


def registry_plan(locks):
    """Union exact discovery requests; reject missing hashes and conflicting cases."""
    result = {}
    for lock in locks:
        hashes = lock.get("registryFileHashes")
        if not isinstance(hashes, dict) or not hashes:
            raise ValueError("missing registryFileHashes")
        for url, expected in hashes.items():
            registry_path(url)
            if not isinstance(expected, str) or not SHA256.fullmatch(expected):
                raise ValueError("missing/invalid lockfile SHA256: " + url)
            if url in result and result[url] != expected:
                raise ValueError("conflicting registry bytes: " + url)
            result[url] = expected
    if BCR + "/bazel_registry.json" not in result:
        raise ValueError("lockfile omitted registry configuration")
    if not any(url.endswith("/MODULE.bazel") for url in result):
        raise ValueError("lockfile omitted discovered modules")
    for url in list(result):
        relative = registry_path(url)
        if relative.endswith("/MODULE.bazel"):
            result.setdefault(BCR + "/modules/" + relative.split("/")[1] + "/metadata.json", None)
    return dict(sorted(result.items()))


def flatten_graph(graph):
    """Flatten official JSON, retaining cycle/unexpanded occurrences as identities.

    JSON formatter order is NOT declaration order. Keep the raw tree separately;
    this normalized selection is explicitly lexicographically sorted.
    """
    if not isinstance(graph, dict) or graph.get("name") != "opencode_builtin_fixture":
        raise ValueError("unexpected official graph root")
    selected = {}

    def visit(node, root=False):
        if not isinstance(node, dict) or not all(isinstance(node.get(k), str) for k in ("key", "name", "version")):
            raise ValueError("malformed official module node")
        if node.get("unused"):
            raise ValueError("unexpected unused node in selected graph")
        if not root and node["key"] != graph["key"]:
            item = {"name": node["name"], "version": node["version"]}
            old = selected.setdefault(node["name"], item)
            if old != item:
                raise ValueError("multiple selected versions for " + node["name"])
        for field in ("dependencies", "indirectDependencies", "cycles"):
            children = node.get(field, [])
            if not isinstance(children, list):
                raise ValueError("malformed graph children")
            for child in children:
                visit(child)

    visit(graph, root=True)
    if "bazel_tools" not in selected:
        raise ValueError("official graph omitted builtin bazel_tools")
    return [selected[name] for name in sorted(selected)]


def validate_bytes(url, data, expected):
    registry_path(url)
    actual = digest(data)
    if expected is not None and (not SHA256.fullmatch(expected) or actual != expected):
        raise ValueError("registry SHA256 mismatch: " + url)
    if url.endswith(".json"):
        value = json.loads(data)
        if not isinstance(value, dict):
            raise ValueError("registry JSON is not an object: " + url)
        if url.endswith("/metadata.json") and not isinstance(value.get("versions"), list):
            raise ValueError("metadata has no versions: " + url)
    elif not data:
        raise ValueError("empty public MODULE: " + url)
    return actual


def fetch_entry(item):
    url, expected = item
    # Trusted HTTPS response bytes are preserved, not JSON reserialized.
    request = urllib.request.Request(url, headers={"User-Agent": "bazel-builtin-fixture-capture/1"})
    with urllib.request.urlopen(request, timeout=45) as response:
        if response.geturl() != url:
            raise ValueError("unexpected registry redirect: " + response.geturl())
        data = response.read()
    actual = validate_bytes(url, data, expected)
    return registry_path(url), data, {"source_url": url, "sha256": actual,
                                    "size_bytes": len(data), "lockfile_sha256": expected}


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def verify_fixture(directory):
    """Offline integrity/completeness check; not a native-versus-reference test."""
    expectations = json.loads((directory / "expectations.json").read_bytes())
    provenance = json.loads((directory / "provenance.json").read_bytes())
    if any(value.get("reference_sha256") != REFERENCE_SHA256 or value.get("reference_version") != "9.3.1"
           for value in (expectations, provenance)):
        raise ValueError("fixture reference release mismatch")
    cases = expectations["cases"]
    if [(case["name"], case["root_module"]) for case in cases] != list(CASES):
        raise ValueError("missing, reordered, or changed oracle case")
    locks = []
    for case in cases:
        prefix = "cases/" + case["name"] + "/"
        if case["reference_graph"] != prefix + "graph.json" or case["reference_lockfile"] != prefix + "MODULE.bazel.lock":
            raise ValueError("unexpected oracle file path")
        graph_bytes = (directory / case["reference_graph"]).read_bytes()
        lock_bytes = (directory / case["reference_lockfile"]).read_bytes()
        if digest(graph_bytes) != case["graph_sha256"] or digest(lock_bytes) != case["lockfile_sha256"]:
            raise ValueError("oracle bytes changed")
        if (directory / prefix / "MODULE.bazel").read_text() != case["root_module"]:
            raise ValueError("oracle root changed")
        if flatten_graph(json.loads(graph_bytes)) != case["selected"]:
            raise ValueError("selection differs from official graph projection")
        locks.append(json.loads(lock_bytes))
    plan = registry_plan(locks)
    files = provenance["files"]
    expected_paths = {"registry/" + registry_path(url) for url in plan}
    patches = verify_registry_patches(directory, provenance.get("registry_patches", {}), files)
    actual_paths = {str(path.relative_to(directory)) for path in (directory / "registry").rglob("*") if path.is_file()}
    if set(files) != expected_paths or actual_paths != expected_paths | set(patches):
        raise ValueError("registry mirror has missing or extra files")
    for url, expected in plan.items():
        relative = "registry/" + registry_path(url)
        data = (directory / relative).read_bytes()
        sha = validate_bytes(url, data, expected)
        if files[relative] != {"source_url": url, "sha256": sha, "size_bytes": len(data), "lockfile_sha256": expected}:
            raise ValueError("registry provenance mismatch: " + url)
    for case in cases:
        for module in case["selected"]:
            if module["name"] == "bazel_tools":
                continue
            url = BCR + "/modules/" + module["name"] + "/" + module["version"] + "/MODULE.bazel"
            if url not in plan:
                raise ValueError("selected module missing from discovery mirror: " + url)
    counts = {"requested_module_count": sum(p.endswith("/MODULE.bazel") for p in files),
              "metadata_count": sum(p.endswith("/metadata.json") for p in files)}
    if any(provenance[key] != value for key, value in counts.items()):
        raise ValueError("registry provenance counts mismatch")
    return {**counts, "registry_file_count": len(files),
            "registry_size_bytes": sum(value["size_bytes"] for value in files.values()),
            "registry_patch_count": len(patches)}


PATCH_EVIDENCE_KEYS = {"source_url", "sha256", "size_bytes", "source_json_integrity", "acquisition", "required_by"}


def verify_registry_patches(directory, patches, files):
    """Supplemental genuine registry patch files, not lockfile registry inputs.

    A capture never writes them (lockfile registryFileHashes omit patches). Each one
    must be declared by a mirrored source.json whose SRI equals the file's SHA256."""
    if not isinstance(patches, dict):
        raise ValueError("registry_patches must be an object")
    for relative, evidence in patches.items():
        parts = relative.split("/")
        if (len(parts) != 6 or parts[:2] != ["registry", "modules"] or parts[4] != "patches"
                or not isinstance(evidence, dict) or set(evidence) != PATCH_EVIDENCE_KEYS):
            raise ValueError("invalid supplemental registry patch entry: " + relative)
        url = BCR + "/" + "/".join(parts[1:])
        if registry_path(url) != "/".join(parts[1:]) or evidence["source_url"] != url:
            raise ValueError("supplemental patch URL mismatch: " + relative)
        source_relative = "/".join(parts[:4]) + "/source.json"
        if source_relative not in files:
            raise ValueError("supplemental patch without mirrored source.json: " + relative)
        declared = json.loads((directory / source_relative).read_bytes()).get("patches", {})
        integrity = declared.get(parts[5])
        data = (directory / relative).read_bytes()
        sri = "sha256-" + base64.b64encode(hashlib.sha256(data).digest()).decode("ascii")
        if (integrity is None or integrity != sri or evidence["source_json_integrity"] != integrity
                or evidence["sha256"] != digest(data) or evidence["size_bytes"] != len(data)):
            raise ValueError("supplemental patch is not the source.json-pinned genuine file: " + relative)
    return patches


def capture(args):
    report = {"schema_version": 1, "status": "FAIL", "runs": [], "errors": [], "cleanup": [],
              "scratch_cleaned": False, "reference_shutdown_attempted": False,
              "captured_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    references = []
    scratch = None
    try:
        if args.destination.exists():
            raise ValueError("refusing to replace existing fixture: " + str(args.destination))
        validate_executable(args.reference, official=True)
        release_sha = digest(args.reference.read_bytes())
        if release_sha != REFERENCE_SHA256:
            raise ValueError("official reference release SHA256 mismatch")
        report["reference_sha256"] = release_sha
        scratch = Path(tempfile.mkdtemp(prefix="builtin-capture-", dir="/tmp/opencode"))
        report["scratch"] = str(scratch)
        stage = scratch / "fixture"
        stage.mkdir()
        runner = RecordingRunner(report, 180)
        locks, case_records = [], []
        for name, root_module in CASES:
            workspace = scratch / name
            workspace.mkdir()
            (workspace / "MODULE.bazel").write_text(root_module)
            (workspace / "BUILD.bazel").write_bytes(b"")
            reference = Reference(args.reference, workspace, scratch, runner, name)
            if args.install_base:
                reference.startup.insert(2, "--install_base=" + str(args.install_base))
            references.append(reference)
            if not locks:
                reference.started = True
                version = runner.run("reference.version", reference.startup + ["version", "--gnu_format"], workspace)
                if require_success(version).strip() != "bazel 9.3.1":
                    raise ValueError("official reference version mismatch")
            flags = ["--registry=" + BCR, "--lockfile_mode=update",
                     "--repository_cache=" + str(args.repository_cache),
                     "--repo_contents_cache=" + str(args.contents_cache)]
            result = reference.run(name + ".graph", "mod", *flags, "graph", "--output=json",
                                   "--include_builtin", "--extension_info=hidden")
            graph_text = require_success(result)
            graph = json.loads(graph_text)
            selected = flatten_graph(graph)
            lock_bytes = (workspace / "MODULE.bazel.lock").read_bytes()
            lock = json.loads(lock_bytes)
            locks.append(lock)
            report.setdefault("reference_lockfiles", {})[name] = lock
            case_dir = stage / "cases" / name
            case_dir.mkdir(parents=True)
            (case_dir / "MODULE.bazel").write_text(root_module)
            (case_dir / "MODULE.bazel.lock").write_bytes(lock_bytes)
            (case_dir / "graph.json").write_text(graph_text)
            (case_dir / "stderr.txt").write_text(result["stderr"])
            case_records.append({"name": name, "root_module": root_module, "selected": selected,
                                 "reference_graph": "cases/" + name + "/graph.json",
                                 "reference_lockfile": "cases/" + name + "/MODULE.bazel.lock",
                                 "graph_sha256": digest(graph_text.encode()),
                                 "lockfile_sha256": digest(lock_bytes)})
            reference.shutdown()
            reference.started = False
        plan = registry_plan(locks)
        files = {}
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            for relative, data, evidence in pool.map(fetch_entry, plan.items()):
                path = stage / "registry" / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                files["registry/" + relative] = evidence
        expectations = {"schema_version": 1, "reference_sha256": REFERENCE_SHA256,
                        "reference_version": "9.3.1", "cases": case_records,
                        "selection_order": "lexicographic name; not source declaration order",
                        "canonical_names": "not projected: mod graph JSON supplies apparentName only"}
        save_json(stage / "expectations.json", expectations)
        provenance = {"schema_version": 1, "reference_sha256": REFERENCE_SHA256,
                      "reference_version": "9.3.1", "captured_at": report["captured_at"],
                      "registry": BCR, "files": files,
                      "requested_module_count": sum(p.endswith("/MODULE.bazel") for p in files),
                      "metadata_count": sum(p.endswith("/metadata.json") for p in files),
                      "source_archives_included": False,
                      "oracle": "official mod graph --include_builtin --extension_info=hidden --output=json",
                      "discovery": "union of both official lockfile registryFileHashes, not just selected versions"}
        save_json(stage / "provenance.json", provenance)
        report["integrity"] = verify_fixture(stage)
        report["capture"] = provenance
        report["cases"] = case_records
        # Move only after successful shutdown/cleanup below.
        report["ready_to_publish"] = True
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
    finally:
        report["reference_shutdown_attempted"] = True
        for reference in references:
            reference.shutdown()
        for run in report["runs"]:
            run["argv_sha256"] = digest(json.dumps(run["argv"], separators=(",", ":")).encode())
        if scratch is not None:
            if report.get("ready_to_publish") and not report["errors"]:
                args.destination.parent.mkdir(parents=True, exist_ok=True)
                # Same filesystem is expected; copy into an adjacent new-only staging dir.
                adjacent = Path(tempfile.mkdtemp(prefix=".builtin-fixture-", dir=args.destination.parent))
                try:
                    shutil.copytree(scratch / "fixture", adjacent, dirs_exist_ok=True)
                    adjacent.rename(args.destination)
                    report["status"] = "PASS"
                finally:
                    if adjacent.exists():
                        shutil.rmtree(adjacent)
            shutil.rmtree(scratch)
            report["scratch_cleaned"] = not scratch.exists()
        write_report(args.report, report)
    return report["status"] == "PASS"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.1-linux-x86_64")
    parser.add_argument("--destination", type=Path, default=ROOT / "tests/fixtures/builtin_modules")
    parser.add_argument("--report", type=Path, default=ROOT / "artifacts/builtin-module-contract/registry-capture.json")
    cache = Path.home() / ".cache/bazel/_bazel_coder/cache/repos/v1"
    parser.add_argument("--repository-cache", type=Path, default=cache / "content_addressable")
    parser.add_argument("--contents-cache", type=Path, default=cache / "contents")
    parser.add_argument("--install-base", type=Path)
    args = parser.parse_args()
    for key, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, key, value.resolve())
    passed = capture(args)
    print(("PASS: " if passed else "FAIL: ") + str(args.report))
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
