#!/usr/bin/env python3
"""Scoped localhost registry/archive QA and official Bazel 9.3 differential.

Stdlib only. Never invokes a wrapper, the real main repository, or Bazel test.
Reference failures fail the report; native-only safety checks are not parity.
"""

import argparse
import base64
import datetime
import gzip
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import io
import json
from pathlib import Path
import re
import shutil
import signal
import struct
import sys
import tarfile
import tempfile
import threading
import zipfile

from validate import (ROOT, Reference, Runner, file_evidence, positive_seconds,
                      reference_output_paths, require_success, validate_executable,
                      write_report)


REFERENCE_SHA256 = "d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8"
FORMATS = ("tar", "tar.gz", "zip")
MODULE = b'module(name = "dep", version = "1.0")\n'
ROOT_MODULE = (b'module(name = "remote_validation_root", version = "1.0")\n'
               b'bazel_dep(name = "dep", version = "1.0", repo_name = "remote")\n')
ARCHIVE_MODULE = MODULE + b'bazel_dep(name = "archive_only", version = "9.9")\n'
OVERLAY_MODULE = MODULE + b'bazel_dep(name = "overlay_only", version = "9.9")\n'
DATA = b"overlay replacement\x00\xff\x80\n"
RAW = bytes(range(256))
BUILD = (b'package(default_visibility = ["//visibility:public"])\n'
         b'filegroup(name = "sources", srcs = ["data.bin", "nested/raw.bin", "MODULE.bazel"])\n'
         b'genrule(name = "copy", srcs = ["data.bin"], outs = ["copy.bin"], cmd = "cat $(location data.bin) > $@")\n')
EXPECTED_SOURCES = {"data.bin": DATA, "nested/raw.bin": RAW, "MODULE.bazel": MODULE}
LABEL = re.compile(r"(?:(?:@@?)[A-Za-z0-9_.+~-]+|@@)?//[^\s:]*:[^\s:]+\Z")
GRAPH_FLAGS = ["--output=json", "--noinclude_builtin", "--extension_info=hidden"]
PLAN = ([("differential." + kind, "differential") for kind in FORMATS]
        + [("native.cache." + kind, "native_only") for kind in FORMATS]
        + [("native." + name, "native_only") for name in (
            "graph-only", "empty-registries", "fallback-404", "fallback-410", "hard-500",
            "selected-source-origin", "wrong-digest", "hostile-tar", "patches-unsupported",
            "warm-blob-url-policy", "zip-skipped-crc")])


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def sri(data):
    return "sha256-" + base64.b64encode(hashlib.sha256(data).digest()).decode("ascii")


def archive_bytes(kind, hostile=False, skipped_crc=False):
    """Fixed timestamps, order, permissions, and compression; no disk extraction."""
    files = {"pkg/BUILD.bazel": BUILD, "pkg/MODULE.bazel": ARCHIVE_MODULE,
             "pkg/data.bin": b"archive original\x00\xfe\n", "pkg/nested/raw.bin": RAW}
    if hostile:
        files["../escape"] = b"must never be written"
    if skipped_crc:
        if kind != "zip":
            raise ValueError("skipped CRC fixture requires ZIP")
        files["other/skipped.bin"] = b"outside strip_prefix but CRC must be checked\x00\xff"
    stream = io.BytesIO()
    if kind == "zip":
        with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_STORED) as archive:
            for name, data in sorted(files.items()):
                entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                entry.create_system = 3
                entry.external_attr = 0o100644 << 16
                archive.writestr(entry, data)
    elif kind in ("tar", "tar.gz"):
        with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            for name, data in sorted(files.items()):
                entry = tarfile.TarInfo(name)
                entry.size, entry.mode, entry.mtime = len(data), 0o644, 0
                archive.addfile(entry, io.BytesIO(data))
    else:
        raise ValueError("unsupported fixture archive format")
    data = stream.getvalue()
    if skipped_crc:
        data = corrupt_zip_crc(data, "other/skipped.bin")
    return gzip.compress(data, mtime=0) if kind == "tar.gz" else data


def corrupt_zip_crc(data, member):
    """Change only one central-directory CRC, leaving local bytes intact."""
    result = bytearray(data)
    end = result.rfind(b"PK\x05\x06")
    if end < 0:
        raise ValueError("fixture ZIP has no end-of-central-directory record")
    position = struct.unpack_from("<I", result, end + 16)[0]
    while position < end:
        if result[position:position + 4] != b"PK\x01\x02":
            raise ValueError("invalid fixture ZIP central-directory entry")
        name_size, extra_size, comment_size = struct.unpack_from("<HHH", result, position + 28)
        name = bytes(result[position + 46:position + 46 + name_size]).decode("utf-8")
        if name == member:
            crc = struct.unpack_from("<I", result, position + 16)[0]
            struct.pack_into("<I", result, position + 16, crc ^ 1)
            return bytes(result)
        position += 46 + name_size + extra_size + comment_size
    raise ValueError("fixture ZIP member not found: " + member)


class FixtureServer:
    """Only an in-memory allowlist is served, never a directory or user files."""

    def __init__(self, report):
        self.routes, self.phase = {}, "setup"
        self.requests = report["requests"]
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                status, data = owner.routes.get(self.path, (404, b"fixture not found\n"))
                owner.requests.append({"id": len(owner.requests), "phase": owner.phase,
                                       "method": "GET", "path": self.path, "status": status,
                                       "size_bytes": len(data), "sha256": sha256(data)})
                self.send_response(status)
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *args):
                pass

        self.httpd = HTTPServer(("127.0.0.1", 0), Handler)
        self.url = "http://127.0.0.1:" + str(self.httpd.server_port)
        self.thread = threading.Thread(target=self.httpd.serve_forever,
                                       kwargs={"poll_interval": 0.05}, name="remote-fixture-http")
        self.started = False

    def start(self):
        try:
            self.thread.start()
            self.started = True
        except BaseException:
            self.httpd.server_close()
            raise

    def close(self):
        receipt = {"kind": "http_server", "url": self.url, "shutdown": False,
                   "closed": False, "joined": False}
        try:
            if self.started:
                self.httpd.shutdown()
            receipt["shutdown"] = True
        finally:
            try:
                self.httpd.server_close()
                receipt["closed"] = True
            finally:
                if self.started:
                    self.thread.join(timeout=5)
                receipt["joined"] = not self.thread.is_alive()
        return receipt

    def install(self, prefix, kind="tar", *, module_status=200, source_status=200,
                wrong_digest=False, hostile=False, patches=False, skipped_crc=False):
        archive = archive_bytes(kind, hostile, skipped_crc)
        archive_path = prefix + "/source." + kind
        entry = prefix + "/registry/modules/dep/1.0/"
        source = {"url": self.url + archive_path, "integrity": sri(b"wrong") if wrong_digest else sri(archive),
                  "strip_prefix": "pkg", "overlay": {"data.bin": sri(DATA), "MODULE.bazel": sri(OVERLAY_MODULE)}}
        if patches:
            source["patches"] = {"unsupported.patch": sri(b"not applied")}
        routes = {archive_path: (200, archive),
                  prefix + "/registry/bazel_registry.json": (200, b"{}\n"),
                  prefix + "/registry/modules/dep/metadata.json": (200, b'{"versions":["1.0"],"yanked_versions":{}}\n'),
                  entry + "MODULE.bazel": (module_status, MODULE),
                  entry + "source.json": (source_status, json.dumps(source, sort_keys=True).encode()),
                  entry + "overlay/data.bin": (200, DATA),
                  entry + "overlay/MODULE.bazel": (200, OVERLAY_MODULE)}
        self.routes.update(routes)
        return {"registry": self.url + prefix + "/registry", "prefix": prefix,
                "module_path": entry + "MODULE.bazel", "source_path": entry + "source.json",
                "archive_path": archive_path, "archive_sha256": sha256(archive),
                "archive_size_bytes": len(archive), "source": source}


class RecordingRunner(Runner):
    def __init__(self, report, timeout):
        super().__init__(report, timeout)
        self.server = None

    def run(self, name, argv, cwd, timeout=None):
        start = len(self.report["requests"])
        if self.server is not None:
            self.server.phase = name
        run_start = len(self.report["runs"])
        try:
            return super().run(name, argv, cwd, timeout)
        finally:
            for record in self.report["runs"][run_start:]:
                record["request_range"] = [start, len(self.report["requests"])]
                for field in ("stdout", "stderr"):
                    data = (base64.b64decode(record[field + "_base64"]) if field + "_base64" in record
                            else record[field].encode("utf-8"))
                    record[field + "_sha256"] = sha256(data)
            if self.server is not None:
                self.server.phase = "idle"


class RemoteReference(Reference):
    def run(self, name, command, *args):
        # Keep genuine existing repository caches available. Only output/server
        # roots are private; never seed stubs or add module/repository overrides.
        return super().run(name, command, "--registry=" + self.registry,
                           "--registry=https://bcr.bazel.build", *args)


def native_command(executable, cache, command, target, registries=()):
    argv = [str(executable), "--ignore_all_rc_files", "--cache-dir=" + str(cache), command]
    if command == "mod":
        argv += ["graph"] + GRAPH_FLAGS
    argv += ["--lockfile_mode=off"] + ["--registry=" + url for url in registries]
    if target is not None:
        argv.append(target)
    return argv


def canonical_label(label):
    if not LABEL.fullmatch(label):
        raise ValueError("invalid query label: " + label)
    if label.startswith("@remote//"):
        return "@@dep+//" + label.split("//", 1)[1]
    if label.startswith("@@dep+//"):
        return label
    raise ValueError("unexpected fixture repository: " + label)


def parse_labels(stdout, native=False):
    labels = [line for line in stdout.splitlines() if line]
    canonical = [canonical_label(label) for label in labels]
    if len(canonical) != len(set(canonical)) or (native and labels != canonical):
        raise ValueError("duplicate labels or noncanonical native labels")
    return sorted(canonical)


def native_paths(record, target, scratch):
    paths = []
    for line in require_success(record).splitlines():
        fields = line.split("\t")
        if len(fields) != 2 or fields[0] != canonical_label(target):
            raise ValueError("unexpected native build output label or format")
        path = Path(fields[1])
        if (not path.is_absolute() or ".." in path.parts or path in paths
                or not path.resolve().is_relative_to(scratch.resolve())):
            raise ValueError("unsafe or duplicate native output path")
        paths.append(path)
    return paths


def manifest(paths, expected, source_root=None):
    result = {}
    for path in paths:
        if source_root is not None:
            try:
                relative = path.relative_to(source_root).as_posix()
            except ValueError as exc:
                raise ValueError("source output escaped expected repository identity") from exc
        else:
            parts = path.parts
            if parts.count("bazel-out") != 1:
                raise ValueError("generated artifact lacks unique configuration prefix")
            suffix = parts[parts.index("bazel-out") + 1:]
            if len(suffix) != 5 or suffix[:2] not in (("rust-fastbuild", "bin"), ("k8-fastbuild", "bin")) or suffix[2:4] != ("external", "dep+"):
                raise ValueError("unexpected generated repository/configuration identity")
            relative = suffix[4]
        if relative not in expected or relative in result:
            raise ValueError("unexpected or duplicate artifact: " + relative)
        result[relative] = file_evidence(path, expected[relative], include_bytes=True)
    if set(result) != set(expected):
        raise ValueError("incomplete artifact inventory")
    return result


def tree_root(cache):
    roots = [path for path in (cache / "repositories/trees").glob("*") if path.is_dir()]
    if len(roots) != 1 or roots[0].is_symlink() or roots[0].name.startswith("."):
        raise ValueError("expected exactly one published repository tree")
    return roots[0]


def require_rejection(record, pattern):
    code = record["returncode"]
    if (code is None or not 0 < code < 128 or code == 101 or record["timed_out"]
            or record.get("spawn_error") or record.get("interrupted") or record["stdout"].strip()
            or re.search(r"panicked|internal error|FATAL:|crashed", record["stderr"], re.I)
            or not re.search(pattern, record["stderr"], re.I)):
        raise ValueError("expected controlled rejection matching " + pattern)


def no_published_tree(cache):
    entries = list((cache / "repositories/trees").glob("*"))
    if entries:
        raise ValueError("failed materialization left published/staged tree entries: " + str(entries))


def request_counts(requests):
    counts = {}
    for request in requests:
        counts[request["path"]] = counts.get(request["path"], 0) + 1
    return counts


def invoke(args, runner, workspace, cache, name, command="query", target="@remote//:all", registries=()):
    return runner.run(name, native_command(args.native, cache, command, target, registries), workspace)


def differential(args, runner, scratch, workspace, fixture, reference, kind):
    name = "differential." + kind
    cache = scratch / (name + "-cache")
    expected = ["@@dep+//:copy", "@@dep+//:sources"]
    native = invoke(args, runner, workspace, cache, name + ".native.query", registries=[fixture["registry"]])
    official = reference.run(name + ".reference.query", "query", "@remote//:all")
    labels = {"native": require_success(native), "reference": require_success(official)}
    if parse_labels(labels["native"], native=True) != expected or parse_labels(labels["reference"]) != expected:
        raise ValueError("query repository identities differ from fixture inventory")
    details = {"exact_query_stdout": labels, "canonical_labels": expected,
               "mapping": {"remote": "dep+"}, "builds": {}}
    for target, expected_bytes in (("@remote//:sources", EXPECTED_SOURCES), ("@remote//:copy", {"copy.bin": DATA})):
        native = invoke(args, runner, workspace, cache, name + ".native.build." + target,
                        "build", target, [fixture["registry"]])
        paths = native_paths(native, target, scratch)
        native_manifest = manifest(paths, expected_bytes, tree_root(cache) if target.endswith(":sources") else None)
        require_success(reference.run(name + ".reference.build." + target, "build", target))
        listing = reference.run(name + ".reference.files." + target, "cquery", "--output=files", target)
        official_paths = reference_output_paths(require_success(listing), workspace, reference.output_base)
        source_root = reference.output_base / "execroot/_main/external/dep+" if target.endswith(":sources") else None
        official_manifest = manifest(official_paths, expected_bytes, source_root)
        details["builds"][target] = {"native": native_manifest, "reference": official_manifest}
    return details


def cache_check(args, runner, scratch, workspace, fixture, kind):
    name = "native.cache." + kind
    cache = scratch / (name + "-cache")
    details = []

    def run(stage, archive_requests, build=False):
        start = len(runner.report["requests"])
        result = invoke(args, runner, workspace, cache, name + "." + stage,
                        "build" if build else "query", "@remote//:sources" if build else "@remote//:all",
                        [fixture["registry"]])
        text = require_success(result)
        counts = request_counts(runner.report["requests"][start:])
        if counts.get(fixture["archive_path"], 0) != archive_requests or counts.get(fixture["module_path"], 0) < 1:
            raise ValueError("archive fetch count or fresh per-process registry MODULE count differs: " + str(counts))
        root = tree_root(cache)
        evidence = manifest(native_paths(result, "@remote//:sources", scratch) if build
                            else [root / path for path in EXPECTED_SOURCES], EXPECTED_SOURCES, root)
        if not build and parse_labels(text, native=True) != ["@@dep+//:copy", "@@dep+//:sources"]:
            raise ValueError("cached query labels changed")
        details.append({"stage": stage, "run_id": result["id"], "request_counts": counts, "files": evidence})

    run("cold-query", 1)
    run("warm-build", 0, build=True)
    run("warm-query", 0)
    root = tree_root(cache)
    (root / "nested/raw.bin").write_bytes(b"corrupt tree")
    (root / "extra").write_bytes(b"untracked corruption")
    blob = cache / "repositories/downloads/sha256" / fixture["archive_sha256"]
    if not blob.is_file() or blob.is_symlink():
        raise ValueError("verified archive blob not found at expected private cache seam")
    blob.write_bytes(b"corrupt blob")
    run("repair-query", 1)
    if (tree_root(cache) / "extra").exists() or sha256(blob.read_bytes()) != fixture["archive_sha256"]:
        raise ValueError("tree/blob corruption not repaired")
    run("repaired-build", 0, build=True)
    return details


def tree_inventory(cache):
    """Snapshot every tree/manifest/staging entry, without following symlinks."""
    directory = cache / "repositories/trees"
    result = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError("unexpected symlink in private repository tree cache")
        result[path.relative_to(directory).as_posix()] = (
            {"kind": "directory"} if path.is_dir() else
            {"kind": "file", "sha256": sha256(path.read_bytes())})
    return result


def warm_blob_url_policy(args, runner, scratch, workspace, server):
    """Real CLI warm-up, then the identical prohibited source in warm/cold caches."""
    name = "native.warm-blob-url-policy"
    warm_cache = scratch / (name + "-warm-cache")
    fixture = server.install("/warm-blob-url-policy")
    start = len(server.requests)
    warmed = invoke(args, runner, workspace, warm_cache, name + ".warm-up",
                    registries=[fixture["registry"]])
    if parse_labels(require_success(warmed), native=True) != ["@@dep+//:copy", "@@dep+//:sources"]:
        raise ValueError("URL-policy warm-up query labels differ")
    counts = request_counts(server.requests[start:])
    if counts.get(fixture["archive_path"], 0) != 1:
        raise ValueError("URL-policy warm-up did not download the valid archive exactly once")
    root = tree_root(warm_cache)
    files = manifest([root / path for path in EXPECTED_SOURCES], EXPECTED_SOURCES, root)
    blob = warm_cache / "repositories/downloads/sha256" / fixture["archive_sha256"]
    if blob.is_symlink() or not blob.is_file() or sha256(blob.read_bytes()) != fixture["archive_sha256"]:
        raise ValueError("URL-policy warm-up did not populate a verified archive blob")
    before = tree_inventory(warm_cache)
    original_source = dict(fixture["source"])
    changed_source = dict(original_source, url="http://@127.0.0.1:9/source.tar")
    server.routes[fixture["source_path"]] = (200, json.dumps(changed_source, sort_keys=True).encode())
    details = {"scope": "native-only; source URL policy must precede cached-blob reuse",
               "original_source": original_source, "changed_source": changed_source,
               "archive_sha256": fixture["archive_sha256"], "tree_inventory_before": before,
               "warm_up": {"run_id": warmed["id"], "request_counts": counts, "files": files,
                           "blob": file_evidence(blob)}, "rejections": []}
    for stage, cache in (("warm", warm_cache), ("cold", scratch / (name + "-cold-cache"))):
        start = len(server.requests)
        rejected = invoke(args, runner, workspace, cache, name + "." + stage,
                          registries=[fixture["registry"]])
        require_rejection(rejected, r"credentials")
        counts = request_counts(server.requests[start:])
        if counts.get(fixture["module_path"], 0) < 1 or counts.get(fixture["source_path"], 0) < 1:
            raise ValueError("URL-policy rejection did not read fresh registry MODULE/source metadata")
        if counts.get(fixture["archive_path"], 0) or any("/overlay/" in path for path in counts):
            raise ValueError("URL-policy rejection fetched fixture archive/overlay bytes")
        after = tree_inventory(cache)
        if stage == "warm":
            if after != before or sha256(blob.read_bytes()) != fixture["archive_sha256"]:
                raise ValueError("URL-policy rejection published/modified a warm tree or blob")
        else:
            no_published_tree(cache)
        details["rejections"].append({"stage": stage, "run_id": rejected["id"],
                                      "request_counts": counts, "fixture_archive_fetches": 0,
                                      "tree_inventory_after": after, "new_tree_published": False})
    return details


def native_probe(args, runner, scratch, workspace, server, scenario):
    if scenario == "warm-blob-url-policy":
        return warm_blob_url_policy(args, runner, scratch, workspace, server)
    name = "native." + scenario
    cache = scratch / (name + "-cache")
    options = {"wrong-digest": {"wrong_digest": True}, "hostile-tar": {"hostile": True},
               "patches-unsupported": {"patches": True},
               "zip-skipped-crc": {"kind": "zip", "skipped_crc": True}}
    fixture = server.install("/" + scenario + "/first", **options.get(scenario, {}))
    registries = [fixture["registry"]]
    second = None
    if scenario.startswith("fallback-") or scenario in ("hard-500", "selected-source-origin"):
        second = server.install("/" + scenario + "/second")
        registries.append(second["registry"])
        if scenario == "selected-source-origin":
            server.routes[fixture["source_path"]] = (404, b"selected source missing")
        else:
            status = int(scenario.rsplit("-", 1)[1])
            server.routes[fixture["module_path"]] = (status, b"registry status fixture")
    graph = scenario in ("graph-only", "empty-registries")
    if scenario == "empty-registries":
        registries = []
    start = len(server.requests)
    result = invoke(args, runner, workspace, cache, name, "mod" if graph else "query",
                    None if graph else "@remote//:all", registries)
    requests = server.requests[start:]
    counts = request_counts(requests)
    if scenario == "graph-only":
        actual = json.loads(require_success(result))
        expected = {"key": "<root>", "name": "remote_validation_root", "version": "1.0",
                    "apparentName": "remote_validation_root", "root": True,
                    "dependencies": [{"key": "dep@1.0", "name": "dep", "version": "1.0",
                                      "apparentName": "remote", "dependencies": [],
                                      "indirectDependencies": [], "cycles": []}],
                    "indirectDependencies": [], "cycles": []}
        if actual != expected or counts.get(fixture["module_path"], 0) < 1:
            raise ValueError("native graph differs from explicit empty-dependency fixture")
        if any("source.json" in r["path"] or "/source." in r["path"] or "/overlay/" in r["path"] for r in requests):
            raise ValueError("graph-only fetched source metadata/archive/overlay")
        no_published_tree(cache)
    elif scenario.startswith("fallback-"):
        if parse_labels(require_success(result), native=True) != ["@@dep+//:copy", "@@dep+//:sources"]:
            raise ValueError("fallback query labels differ")
        if counts.get(fixture["module_path"], 0) < 1 or counts.get(second["module_path"], 0) < 1 or counts.get(second["archive_path"], 0) != 1:
            raise ValueError("missing evidence of second-registry selection")
        root = tree_root(cache)
        manifest([root / path for path in EXPECTED_SOURCES], EXPECTED_SOURCES, root)
    else:
        patterns = {"empty-registries": r"not found|registr", "hard-500": r"500",
                    "selected-source-origin": r"source.*(?:missing|404)|missing.*source",
                    "wrong-digest": r"integrity.*mismatch", "hostile-tar": r"unsafe.*path",
                    "patches-unsupported": r"patch.*(?:not implemented|unsupported)",
                    "zip-skipped-crc": r"(?:CRC|checksum)"}
        require_rejection(result, patterns[scenario])
        if scenario == "zip-skipped-crc" and not re.search(r"zip", result["stderr"], re.I):
            raise ValueError("CRC rejection must identify ZIP reading, not SRI validation")
        no_published_tree(cache)
        if second and any(r["path"].startswith(second["prefix"] + "/") for r in requests):
            raise ValueError("hard/selected-source failure fell back to second registry")
        if scenario == "empty-registries" and requests:
            raise ValueError("empty native registries made fixture HTTP requests")
        if scenario in ("wrong-digest", "hostile-tar", "zip-skipped-crc") and counts.get(fixture["archive_path"], 0) != 1:
            raise ValueError("rejection did not fetch the intended archive")
        if scenario == "hostile-tar" and list(scratch.rglob("escape")):
            raise ValueError("hostile archive wrote outside its staging tree")
    details = {"run_id": result["id"], "request_counts": counts, "fixture": fixture,
               "scope": "native-only; not an official rejection or differential claim"}
    if scenario == "zip-skipped-crc":
        details.update(corrupt_member="other/skipped.bin",
                       mutation="central-directory CRC XOR 1 at entry + 16; source SRI recomputed",
                       tree_inventory_after=tree_inventory(cache), new_tree_published=False)
    return details


def run_check(report, name, kind, callback):
    item = {"name": name, "kind": kind, "status": "FAIL"}
    report["checks"].append(item)
    try:
        item["details"] = callback()
        item["status"] = "PASS"
    except (OSError, ValueError) as exc:
        item["error"] = str(exc)


def report_passes(report):
    return ([(c["name"], c["kind"]) for c in report["checks"]] == PLAN
            and all(c["status"] == "PASS" for c in report["checks"])
            and not report["errors"] and report["scratch_cleaned"]
            and report["http_cleaned"] and report["reference_shutdown_attempted"])


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--scratch-parent", type=Path, default=Path("/tmp/opencode"))
    parser.add_argument("--report", type=Path, default=ROOT / "artifacts/remote-validation.json")
    parser.add_argument("--timeout", type=positive_seconds, default=180)
    return parser


def validate_binaries(args):
    validate_executable(args.native)
    validate_executable(args.reference, official=True)
    evidence = {"native": file_evidence(args.native), "reference": file_evidence(args.reference)}
    if evidence["reference"]["sha256"] != REFERENCE_SHA256:
        raise ValueError("reference SHA256 differs from pinned official Bazel 9.3.0 Linux x86_64")
    return evidence


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "scratch_parent", "report"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    if args.report in (args.native, args.reference) or args.report.is_relative_to(ROOT / "tests/fixtures"):
        raise ValueError("report must not overwrite executables or tracked fixtures")
    args.report.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "status": "FAIL", "checks": [], "runs": [], "requests": [],
              "errors": [], "cleanup": [], "scratch_cleaned": False, "http_cleaned": False,
              "reference_shutdown_attempted": False, "test_commands_executed": False,
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              "planned_checks": [{"name": name, "kind": kind} for name, kind in PLAN],
              "reference_version_required": "9.3.0", "reference_sha256_required": REFERENCE_SHA256,
              "limitations": ["Scoped fixture query/build identity and byte parity only, not full Bazel compatibility.",
                              "Native-only cache/security/fallback checks are not negative differential parity.",
                              "Reference graph comparison NOT_RUN; native graph-only laziness is checked.",
                              "Reference uses genuine BCR built-ins and existing genuine download caches; no stubs.",
                              "Private output roots and native caches; OS cache not flushed; no performance claim.",
                              "No credential environment changes or wrapper invocation; not a network sandbox.",
                              "Traversal rejection is intentionally stricter, not claimed equivalent to official extraction.",
                              "Patches unsupported; report records a native-only controlled rejection."]}
    runner = RecordingRunner(report, args.timeout)
    scratch = server = None
    references = []
    interrupted = False
    try:
        report["executables"] = validate_binaries(args)
        scratch = Path(tempfile.mkdtemp(prefix="remote-validation-", dir=args.scratch_parent))
        report["scratch_directory"] = str(scratch)
        workspace = scratch / "workspace"
        workspace.mkdir()
        (workspace / "MODULE.bazel").write_bytes(ROOT_MODULE)
        (workspace / "BUILD.bazel").write_bytes(b"")
        server = FixtureServer(report)
        runner.server = server
        server.start()
        fixtures = {kind: server.install("/" + kind, kind=kind) for kind in FORMATS}
        report["fixtures"] = fixtures
        for kind in FORMATS:
            reference = RemoteReference(args.reference, workspace, scratch, runner, "reference-" + kind)
            reference.registry = fixtures[kind]["registry"]
            references.append(reference)
            if len(references) == 1:
                release = require_success(reference.run("reference.release", "info", "release")).strip()
                report["reference_release"] = release
                if release != "release 9.3.0":
                    raise ValueError("reference is not official release 9.3.0")
            run_check(report, "differential." + kind, "differential", lambda k=kind, r=reference: differential(
                args, runner, scratch, workspace, fixtures[k], r, k))
        for kind in FORMATS:
            run_check(report, "native.cache." + kind, "native_only", lambda k=kind: cache_check(
                args, runner, scratch, workspace, fixtures[k], k))
        for name, _ in PLAN[6:]:
            run_check(report, name, "native_only", lambda n=name: native_probe(
                args, runner, scratch, workspace, server, n.removeprefix("native.")))
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except Exception as exc:
        # Unexpected harness exceptions must still produce a FAIL artifact.
        report["errors"].append(type(exc).__name__ + ": " + str(exc))
    finally:
        for reference in reversed(references):
            try:
                report["reference_shutdown_attempted"] = True
                reference.shutdown()
                report["cleanup"].append({"kind": "reference", "output_base": str(reference.output_base), "shutdown_attempted": True})
            except Exception as exc:
                report["errors"].append("reference shutdown: " + str(exc))
        if server is not None:
            try:
                receipt = server.close()
                report["cleanup"].append(receipt)
                report["http_cleaned"] = all(receipt[key] for key in ("shutdown", "closed", "joined"))
                if not report["http_cleaned"]:
                    report["errors"].append("HTTP thread not joined")
            except Exception as exc:
                report["errors"].append("HTTP cleanup: " + str(exc))
        if scratch is not None:
            try:
                shutil.rmtree(scratch)
                report["scratch_cleaned"] = not scratch.exists()
                report["cleanup"].append({"kind": "scratch", "path": str(scratch), "removed": report["scratch_cleaned"]})
            except OSError as exc:
                report["errors"].append("scratch cleanup: " + str(exc))
        if report_passes(report):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.report, report)
    print("{} (scoped remote fixtures only): {}".format(report["status"], args.report))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
