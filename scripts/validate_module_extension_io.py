#!/usr/bin/env python3
"""Bounded genuine REFERENCE-ONLY module download/read capture and replay.

Native execution is fail-closed until the parent freezes the independent core.
Only owned QA, fixture, registry, official-reference and executable inputs are
bound. No current Rust/native-source equivalence is asserted or inspected.
"""

import argparse
import base64
from dataclasses import asdict, dataclass
import gzip
import hashlib
import importlib.util
import io
import json
import math
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tarfile
import time


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/module_extension_io"
PLATFORMS = ROOT / "tests/fixtures/label_repo_name/platforms_archive.json"
PLATFORM_SHA256 = "3384eb1c30762704fbe38e440204e114154086c8fc8a8c2e3e28441028c019a8"
PLATFORM_BASENAME = "platforms-1.0.0.tar.gz"
PLATFORM_URL = "https://github.com/bazelbuild/platforms/releases/download/1.0.0/" + PLATFORM_BASENAME
PLATFORM_ROUTE = "/__setup__/" + PLATFORM_BASENAME
PLATFORM_REWRITE_PATTERN = r"^github\.com/bazelbuild/platforms/releases/download/1\.0\.0/platforms-1\.0\.0\.tar\.gz$"
SETUP_CASE = "platform-distdir-smoke"
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
EMPTY_BLAKE3 = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
MARKER = b"MODULE_IO_JSON "


def digest(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=True, allow_nan=False).encode("ascii")


def equal(actual, expected, label):
    # Python's True == 1 must never make a typed proof valid.
    if canonical(actual) != canonical(expected):
        raise ValueError(label)


def load_json(data):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    def invalid(value):
        raise ValueError("nonfinite JSON: " + value)

    return json.loads(data, object_pairs_hook=unique, parse_constant=invalid)


def payloads():
    versions = (FIXTURES / "versions.json").read_bytes()
    contract = load_json((FIXTURES / "contract.json").read_bytes())
    equal([len(versions), digest(versions)],
          [contract["versions_size"], contract["versions_sha256"]], "versions pin")
    old = contract["owned_seed"]["text"].encode("ascii")
    return {"versions": versions, "old": old, "all256": bytes(range(256)), "empty": b"",
            "html": b"<!doctype html><title>fixture proxy</title>\n",
            **{str(n): bytes(i % 251 for i in range(n))
               for n in (32767, 32768, 32769, 40000)}}


def platforms_archive():
    record = load_json(PLATFORMS.read_bytes())
    data = base64.b64decode(record["bytes_base64"], validate=True)
    equal([record["sha256"], digest(data), record["size_bytes"], len(data)],
          [PLATFORM_SHA256, PLATFORM_SHA256, 7879, 7879], "platform archive pin")
    return data


def downloader_config(origin):
    if not re.fullmatch(r"http://127\.0\.0\.1:[1-9][0-9]{0,4}", origin) or int(origin.rsplit(":", 1)[1]) > 65535:
        raise ValueError("owned loopback origin required")
    # UrlRewriter.java:262-296 matches the scheme-stripped URL and accepts an
    # explicit replacement scheme. No public host is allowlisted.
    return ("block *\nallow 127.0.0.1\nallow localhost\nrewrite " + PLATFORM_REWRITE_PATTERN
            + " " + origin + PLATFORM_ROUTE + "\n").encode("ascii")


def platform_distdir_pin(root):
    path = Path(root) / "inputs/distdir" / PLATFORM_BASENAME
    pin = file_pin(path)
    equal([pin["sha256"], pin["size"]], [PLATFORM_SHA256, 7879], "fixed staged platform pin")
    return {"path": str(path), **pin}


def platform_closure(source=None, read_registry=None):
    """Governed closure, including the implicit registry module-file overlay.

    Optional arguments are pure validator-test inputs, never capture CLI options.
    Production always reads the pinned, existing tracked registry bytes.
    """
    pin = load_json((FIXTURES / "platform_closure.json").read_bytes())
    registry = ROOT / pin["registry_relative_root"]
    prefix = "modules/" + pin["module"] + "/"
    production = source is None and read_registry is None
    if read_registry is None:
        read_registry = lambda name: (registry / name).read_bytes()
    if source is None:
        raw = read_registry(prefix + "source.json")
        equal(digest(raw), pin["source_json_sha256"], "tracked platforms source metadata pin")
        source = load_json(raw)
    module = read_registry(prefix + "MODULE.bazel")
    module_sri = result_fields(module)["integrity"]
    if production:
        equal([digest(module), len(module), module_sri],
              [pin["module_file_sha256"], pin["module_file_size"], pin["module_file_integrity"]], "tracked implicit module-file pin")
        equal(source.get("patches", {}), pin["declared_patches"], "declared platform patches")
        equal(source.get("overlay", {}), pin["declared_overlays"], "declared platform overlays")
    equal(source["url"], PLATFORM_URL, "pinned release URL")
    rows = []

    def add(kind, registry_path, basename, integrity, data):
        equal(result_fields(data)["integrity"], integrity, "declared prerequisite SRI: " + basename)
        sha = digest(data)
        rows.append({"kind": kind, "registry_path": registry_path, "basename": basename,
                     "integrity": integrity, "sha256": sha, "size": len(data), "data": blob(data),
                     "distdir": "inputs/distdir" if kind == "archive" else "inputs/distdir/" + sha})

    add("archive", None, PLATFORM_BASENAME, source["integrity"], platforms_archive())
    for category, kind in (("patches", "patch"), ("overlay", "overlay")):
        for name, integrity in sorted(source.get(category, {}).items()):
            path = Path(name)
            if path.is_absolute() or ".." in path.parts or str(path) != name:
                raise ValueError("unsafe declared prerequisite path")
            relative = prefix + category + "/" + name
            add(kind, relative, path.name, integrity, read_registry(relative))
    # This prerequisite exists even when source.json declares NO patches/overlay.
    add("module-file", prefix + "MODULE.bazel", "MODULE.bazel", module_sri, module)
    return rows


def platform_closure_pins(root):
    rows = []
    for requirement in platform_closure():
        path = Path(root) / requirement["distdir"] / requirement["basename"]
        actual = file_pin(path)
        equal([actual["sha256"], actual["size"]], [requirement["sha256"], requirement["size"]], "governed platform prerequisite missing/corrupt")
        rows.append({"kind": requirement["kind"], "path": str(path), "integrity": requirement["integrity"], **actual})
    return rows


def selected_cases(setup=False, api_smoke=False):
    return (Case(SETUP_CASE, (), (), flags=("--repository_disable_download",),
                 profile="reference-only-platform-setup"),) if setup else cases()[:1] if api_smoke else cases()


def result_fields(data, algorithm="sha256"):
    checksum = hashlib.new(algorithm, data).digest()
    fields = {"success": True, "size_bytes": len(data),
              "integrity": algorithm + "-" + base64.b64encode(checksum).decode("ascii")}
    if algorithm == "sha256":
        fields["sha256"] = checksum.hex()
    return fields


@dataclass(frozen=True)
class Case:
    name: str
    operations: tuple
    requests: tuple
    result: str = "success"
    flags: tuple = ()
    profile: str = "planned-reference-cli/native-pending-freeze"


def download(url="/versions?mode=json&include=all", output="metadata.json", **kwargs):
    return {"method": "download", "args": [url], "kwargs": {"output": output, **kwargs}}


def read(path="metadata.json", watch="auto"):
    return {"method": "read", "args": [path], "kwargs": {"watch": watch}}


def cases():
    """Concrete, bounded CLI candidate recipes, not completed coverage."""
    p = payloads()
    sha = digest(p["versions"])
    url = "/versions?mode=json&include=all"
    seed = download(sha256=sha)
    rows = [Case("checksummed-json", (seed, read()), (url,)),
            Case("mirrors-404-success", (download(["/404", url], sha256=sha), read()),
                 ("/404", url)),
            Case("html-no-fallback", (download(["/html", url], sha256=digest(p["html"])), read()),
                 ("/html",)),
            Case("matching-destination", (seed, download("/never", sha256=sha), read()), (url,)),
            Case("all256-auto-no", (download("/all256", sha256=digest(p["all256"])),
                                        read(), read(watch="no")), ("/all256",)),
            Case("empty-output", (download(output="", sha256=sha), read("")), (url,))]
    for allow in (False, True):
        suffix = str(allow).lower()
        outcome = "success" if allow else "callback-error"
        rows.extend([
            Case("http-filter-" + suffix, (download(allow_fail=allow),), (), outcome),
            Case("allfail-" + suffix,
                 (download(["/404", "/also404"], sha256=sha, allow_fail=allow),),
                 ("/404", "/also404"), outcome),
            Case("disabled-miss-" + suffix, (download(sha256=sha, allow_fail=allow),), (),
                 outcome, ("--repository_disable_download",)),
            Case("empty-list-" + suffix, (download([], sha256=sha, allow_fail=allow),), (), outcome),
            Case("empty-list-existing-" + suffix,
                 (seed, download([], sha256=sha, allow_fail=allow)), (url,), outcome)])
    # Pinned Checksum.java:86-105 includes BLAKE3, not SHA-512/256.
    # Its empty-input vector avoids adding a Python digest package to this scaffold.
    for algorithm in ("sha1", "sha256", "sha384", "sha512", "blake3"):
        value = bytes.fromhex(EMPTY_BLAKE3) if algorithm == "blake3" else hashlib.new(algorithm, p["versions"]).digest()
        checksum = base64.b64encode(value).decode("ascii")
        route = "/empty" if algorithm == "blake3" else url
        rows.append(Case("sri-" + algorithm,
                         (download(route, integrity=algorithm + "-" + checksum), read()), (route,)))
    for route, paths in (("gzip", ("/gzip",)), ("x-gzip", ("/x-gzip",)),
                         ("original.gz", ("/original.gz", "/compressed-bare")),
                         ("to-final-suffix", ("/to-final-suffix", "/compressed.tgz"))):
        body = compressed(p["versions"]) if len(paths) == 2 else p["versions"]
        rows.append(Case(route, (download("/" + route, sha256=digest(body)), read()), paths))
    rows.extend([
        Case("malformed-sha-transfer-success", (download(sha256="not-hex", allow_fail=True),),
             (url,), "callback-error"),
        Case("malformed-sha-transfer-failure", (download("/404", sha256="not-hex", allow_fail=True),),
             ("/404",)),
        Case("malformed-sha-disabled", (download(sha256="not-hex", allow_fail=True),), (),
             flags=("--repository_disable_download",)),
        Case("read-watch-yes", (seed, read(watch="yes")), (url,), "callback-error"),
        Case("read-missing", (read("missing"),), (), "callback-error"),
        Case("read-invalid-watch", (read("missing", watch="invalid"),), (), "callback-error"),
        Case("root-file-then-child", (download(output="", sha256=sha),
             download(output="child", sha256=sha, allow_fail=True), read("")), (url,)),
        Case("child-then-root-directory", (download(output="child", sha256=sha),
             download(output="", sha256=sha, allow_fail=True), read("child")), (url, url)),
    ])
    if len({c.name for c in rows}) != len(rows):
        raise ValueError("duplicate case")
    return tuple(rows)


def starlark(value):
    if value is None:
        return "None"
    if type(value) is bool:
        return "True" if value else "False"
    if type(value) is str:
        return json.dumps(value, ensure_ascii=True)
    if type(value) is list:
        return "[" + ", ".join(map(starlark, value)) + "]"
    if type(value) is dict:
        return "{" + ", ".join(starlark(k) + ": " + starlark(v) for k, v in value.items()) + "}"
    if type(value) is int:
        return str(value)
    raise ValueError("unsupported fixture value type")


def compressed(data):
    # Normalize only this generated fixture's gzip OS byte, never captured streams.
    raw = gzip.compress(data, mtime=0)
    return raw[:9] + b"\xff" + raw[10:]


def response_plan():
    """Public server inputs; no server is started by this module."""
    p = payloads()
    routes = {}

    def add(path, body=b"", status=200, headers=None):
        routes[path] = {"status": status, "headers": {"Content-Length": str(len(body)), **(headers or {})},
                        "body": blob(body)}

    for route, key in (("/versions?mode=json&include=all", "versions"),
                       ("/all256", "all256"), ("/empty", "empty"), ("/html", "html")):
        add(route, p[key])
    for route in ("/404", "/also404"):
        add(route, status=404)
    for route in ("/gzip", "/x-gzip", "/compressed-bare", "/compressed.tgz"):
        add(route, compressed(p["versions"]), headers={"Content-Encoding": "x-gzip" if route == "/x-gzip" else "gzip"})
    add("/original.gz", status=302, headers={"Location": "/compressed-bare"})
    add("/to-final-suffix", status=303, headers={"Location": "/compressed.tgz"})
    add(PLATFORM_ROUTE, platforms_archive())
    routes[PLATFORM_ROUTE]["purpose"] = "platform-setup"
    return routes


def render(case, origin="http://127.0.0.1:18080"):
    """Real module_ctx calls -> actual repository attr -> materialized consumer.

    No provider globals, fabricated result objects, ctx.path or ctx.execute.
    Consumer assertions must be collected independently, not from callback prints.
    """
    if not re.fullmatch(r"http://127\.0\.0\.1:[1-9][0-9]{0,4}", origin) or int(origin.rsplit(":", 1)[1]) > 65535:
        raise ValueError("owned loopback HTTP origin required")
    if case.name == SETUP_CASE:
        return {"MODULE.bazel": b'module(name = "module_io_setup")\n',
                "BUILD.bazel": b'load(":consumer.bzl", "consume_observation")\nexports_files(["seed.txt"])\nconsume_observation(name = "consume", src = "seed.txt")\n',
                "consumer.bzl": (FIXTURES / "consumer.bzl").read_bytes(),
                "seed.txt": (FIXTURES / "setup_seed.txt").read_bytes()}
    lines = ['load(":repository.bzl", "observed_repository")',
             'def _impl(ctx):', '    observations = []',
             '    print("MODULE_IO_JSON " + json.encode({"event": "begin", "case": ' + starlark(case.name) + '}))']
    for index, op in enumerate(case.operations):
        args = list(op["args"])
        if op["method"] == "download":
            args[0] = [origin + u for u in args[0]] if type(args[0]) is list else origin + args[0]
        arguments = [starlark(arg) for arg in args]
        arguments += [k + " = " + starlark(v) for k, v in op["kwargs"].items()]
        pre = {"event": "pre", "case": case.name, "index": index, "operation": op}
        lines.append('    print("MODULE_IO_JSON " + json.encode(' + starlark(pre) + '))')
        lines.append('    value = ctx.' + op["method"] + '(' + ', '.join(arguments) + ')')
        if op["method"] == "download":
            lines.append('    value = {"type": type(value), "fields": {k: getattr(value, k) for k in dir(value)}}')
        lines += ['    observations.append(value)',
                  '    print("MODULE_IO_JSON " + json.encode({"event": "post", "index": ' + str(index) + ', "value": value}))']
    if case.name == "checksummed-json":
        lines += ['    versions = json.decode(observations[-1])',
                  '    parsed = {}',
                  '    for release in versions:',
                  '        archives = {}',
                  '        for entry in release["files"]:',
                  '            if entry["kind"] == "archive":',
                  '                archives[entry["os"] + "_" + entry["arch"]] = [entry["filename"], entry["sha256"]]',
                  '        parsed[release["version"][2:]] = archives',
                  '    observations.append(parsed)']
    lines += ['    observed_repository(name = "made", observed = json.encode(observations))',
              '    print("MODULE_IO_JSON " + json.encode({"event": "declared", "name": "made"}))',
              'io = module_extension(implementation = _impl)']
    return {"MODULE.bazel": b'module(name = "module_io")\ne = use_extension("//:extension.bzl", "io")\nuse_repo(e, observed = "made")\n',
            "extension.bzl": ("\n".join(lines) + "\n").encode("utf-8"),
            **{name: (FIXTURES / name).read_bytes() for name in ("BUILD.bazel", "repository.bzl", "consumer.bzl")}}


def environment(root):
    return {"HOME": root + "/home", "TMPDIR": root + "/tmp",
            "PATH": "/usr/bin:/bin", "LC_ALL": "C"}


def reference_plan(root="/MODULE_IO_OWNED", executable="/PINNED_REFERENCE", setup=False, api_smoke=False):
    """Complete reference chronology; event rows are NOT subprocess commands."""
    scratch = root + "/scratch"
    startup = [executable, "--batch", "--ignore_all_rc_files",
               "--output_user_root=" + scratch + "/user",
               "--host_jvm_args=-Duser.home=" + scratch + "/home",
               "--host_jvm_args=-Djava.io.tmpdir=" + scratch + "/tmp",
               "--host_jvm_args=-XX:ActiveProcessorCount=2", "--host_jvm_args=-Xmx512m"]
    common = ["--registry=file://" + root + "/inputs/registry", "--lockfile_mode=off",
              "--incompatible_autoload_externally=", "--repository_cache=",
              "--repo_contents_cache=" + scratch + "/contents",
              "--downloader_config=" + root + "/inputs/downloader.conf",
              "--noshow_progress", "--color=no", "--curses=no", "--jobs=2"]
    common += ["--distdir=" + root + "/" + path for path in dict.fromkeys(r["distdir"] for r in platform_closure())]
    rows = []

    def add(phase, case="", argv=None):
        rows.append({"ordinal": len(rows), "phase": phase, "case": case,
                     "purpose": "platform-setup" if setup else "ctx-smoke" if api_smoke else "ctx-api", "platform_requests": [],
                     "cycle": 0, "engine": "reference", "row_type": "event" if argv is None else "command",
                     "argv": [] if argv is None else argv,
                     "cwd": scratch + ("/workspace/" + case if case else ""),
                     "env": environment(scratch)})

    for phase in ("pins-start", "public-input-snapshot", "registry-local-copy",
                  "distdir-pin", "server-start"):
        add(phase)
    add("version", argv=startup + ["--output_base=" + scratch + "/version", "version"])
    for case in selected_cases(setup, api_smoke):
        add("case-start", case.name)
        for phase in (("cold",) if setup else ("cold", "warm")) if case.result == "success" else ("negative",):
            add(phase, case.name, startup + ["--output_base=" + scratch + "/base/" + case.name, "build", "//:consume"] + common + list(case.flags)
                + ["--spawn_strategy=local", "--execution_log_json_file=" + root + "/raw/" + case.name + "/" + phase + ".exec.json"])
            add(phase + "-observation", case.name)
        if case.result == "success":
            add("cquery", case.name, startup + ["--output_base=" + scratch + "/base/" + case.name,
                "cquery", "set(//:consume //:seed.txt)" if setup else "set(//:consume @observed//:data.json)", "--output=files"] + common + list(case.flags))
            add("consumer-observation", case.name)
        add("case-end", case.name)
    for phase in ("server-stop", "cleanup", "pins-end"):
        add(phase)
    return rows


def callback_events(raw, profile):
    """Only LF framing; preserve high bytes before logical JSON decoding.

    Prefix extraction is a collector responsibility and MUST be source-bound to
    the actual callback path/line. This primitive accepts marker-only raw frames,
    not arbitrary stderr substring searches. Full stderr is retained separately.
    """
    encoding = {"reference": "latin-1", "native": "utf-8"}[profile]
    rows = []
    for line in raw.split(b"\n"):
        if not line:
            continue
        if not line.startswith(MARKER):
            raise ValueError("non-marker callback frame")
        rows.append(load_json(line[len(MARKER):].decode(encoding, errors="strict")))
    return rows


def logical_bytes(text):
    return text.encode("latin-1", errors="strict")


def manifest_entry(path, kind, mode, data=b""):
    if kind not in ("file", "directory") or (kind == "directory" and data):
        raise ValueError("invalid tree entry")
    return {"path": path, "kind": kind, "mode": mode, "size": len(data), "sha256": digest(data)}


def blob(data):
    return {"size": len(data), "sha256": digest(data),
            "base64": base64.b64encode(data).decode("ascii")}


def unblob(value):
    equal(sorted(value), ["base64", "sha256", "size"], "raw blob schema")
    data = base64.b64decode(value["base64"], validate=True)
    equal([value["size"], value["sha256"]], [len(data), digest(data)], "raw blob pin")
    return data


def verify_record(actual, expected):
    """Shared LIVE/REPLAY boundary; expectations never come from actual report.

    This scaffold deliberately requires EXACT raw diagnostic bytes. No generic
    unsupported footer stripping, substring error tests or exit-only negatives.
    Future variable path parsing requires a reviewed, source-bound adapter.
    """
    equal(sorted(actual), sorted(expected), "record schema")
    equal(actual["invocation"], expected["invocation"], "trusted chronology/argv/env")
    equal(actual["health"], {"timeout": False, "signal": None, "spawn_error": None,
                            "truncated": False}, "infrastructure failure")
    if type(actual["status"]) is not int or actual["status"] not in (0, 1, 7):
        raise ValueError("crash/infrastructure status")
    for stream in ("stdout", "stderr", "callback_frames"):
        unblob(actual[stream])
    equal(callback_events(unblob(actual["callback_frames"]), actual["profile"]),
          expected["events"], "callback sequence/typed parameters")
    # Full trusted comparison binds requests, source paths, executable/input pins,
    # action input/output paths, cquery bytes, publication, root entries and modes.
    # Repaired IDs, observation refs or hashes do not change the external contract.
    equal(actual, expected, "frozen record mismatch")


def replay(report, trusted, trusted_sha256, current_pins):
    """Structural replay only; never returns native acceptance.

    trusted_sha256 is an OUT-OF-BAND freeze pin, not report-supplied. The synthetic
    unit fixture cannot be promoted to a captured contract by changing its label.
    No user-facing replay/run CLI is exposed before parent authorization.
    """
    equal(digest(canonical(trusted)), trusted_sha256, "external freeze pin")
    equal(sorted(report), ["classification", "pins", "rows"], "report schema")
    equal(report["classification"], "preparation-scaffold", "no acceptance classification")
    equal(report["pins"], current_pins, "current executable/source/input pins")
    equal(report["pins"], trusted["pins"], "frozen pins")
    if len(report["rows"]) != len(trusted["rows"]):
        raise ValueError("incomplete coverage")
    for index, (actual, expected) in enumerate(zip(report["rows"], trusted["rows"])):
        equal(actual["invocation"]["ordinal"], index, "chronological ordinal")
        verify_record(actual, expected)
    return "SCAFFOLD_REPLAY_VALIDATED_NOT_ACCEPTANCE"


def preparation_plan():
    rows = reference_plan()
    return {"state": "PREPARATION_ONLY", "native_acceptance": False,
            "cases": [asdict(c) for c in cases()], "chronology": rows,
            "responses": response_plan(),
            "counts": {"cli_candidate_cases": len(cases()), "chronology_rows": len(rows),
                       "reference_commands": sum(bool(r["argv"]) for r in rows)},
            "deferred": load_json((FIXTURES / "deferred.json").read_bytes())}


def expected_observations(case):
    """Independent input-derived oracle, NEVER substituted into a callback."""
    p = payloads()
    values = []
    failures = {"http-filter-true", "allfail-true", "disabled-miss-true", "empty-list-true",
                "malformed-sha-transfer-failure", "malformed-sha-disabled"}
    for i, op in enumerate(case.operations):
        if case.result == "callback-error" and i == len(case.operations) - 1:
            break
        if op["method"] == "read":
            body = p["all256"] if case.name == "all256-auto-no" else p["html"] if case.name == "html-no-fallback" else p["empty"] if case.name == "sri-blake3" else compressed(p["versions"]) if case.name in ("original.gz", "to-final-suffix") else p["versions"]
            values.append(body.decode("latin-1"))
        else:
            failed = case.name in failures or (case.name in ("root-file-then-child", "child-then-root-directory") and i == 1)
            algorithm = case.name.removeprefix("sri-") if case.name.startswith("sri-") else "sha256"
            body = p["all256"] if case.name == "all256-auto-no" else p["html"] if case.name == "html-no-fallback" else compressed(p["versions"]) if case.name in ("original.gz", "to-final-suffix") else p["versions"]
            fields = ({"success": False} if failed else
                      {"success": True, "size_bytes": 0, "integrity": "blake3-" + base64.b64encode(bytes.fromhex(EMPTY_BLAKE3)).decode()} if algorithm == "blake3" else
                      result_fields(body, algorithm))
            values.append({"type": "struct", "fields": fields})
    return values


def expected_events(case):
    values = expected_observations(case)
    events = [{"event": "begin", "case": case.name}]
    for i, op in enumerate(case.operations):
        events.append({"event": "pre", "case": case.name, "index": i, "operation": op})
        if i < len(values):
            events.append({"event": "post", "index": i, "value": values[i]})
    if case.result == "success":
        events.append({"event": "declared", "name": "made"})
    return events


def expected_consumer(case):
    values = expected_observations(case)
    if case.name == "checksummed-json":
        versions = load_json(payloads()["versions"])
        values.append({v["version"][2:]: {f["os"] + "_" + f["arch"]: [f["filename"], f["sha256"]]
                                        for f in v["files"] if f["kind"] == "archive"} for v in versions})
    return values


def reference_frames(stderr, source_path, source):
    """Bind each debug frame to its actual source print line and column."""
    sites = {i for i, line in enumerate(source.decode("utf-8").splitlines(), 1)
             if line.startswith('    print("MODULE_IO_JSON " + json.encode(')}
    pattern = re.compile(rb"DEBUG: " + re.escape(str(source_path).encode()) + rb":([0-9]+):10: (MODULE_IO_JSON .*)")
    frames, locations = [], []
    for line in stderr.split(b"\n"):
        match = pattern.fullmatch(line)
        if match:
            if int(match[1]) not in sites:
                raise ValueError("callback frame has wrong source line")
            frames.append(match[2] + b"\n")
            locations.append(int(match[1]))
        elif MARKER in line:
            raise ValueError("unbound marker/source echo")
    return callback_events(b"".join(frames), "reference"), locations, b"".join(frames)


def negative_profile(spec, case, source, origin):
    """Derive the negative command/operation/target/site only from trusted inputs."""
    if spec["engine"] != "reference" or spec["phase"] != "negative" or case.result != "callback-error":
        raise ValueError("not a planned reference negative")
    fixture = render(case, origin)
    if source != fixture["extension.bzl"]:
        raise ValueError("negative source differs from trusted recipe")
    usages = re.findall(rb'^e = use_extension\("(//:[A-Za-z0-9_.-]+)", "([A-Za-z_][A-Za-z0-9_]*)"\)$',
                        fixture["MODULE.bazel"], re.M)
    names = re.findall(rb'^    name = "([A-Za-z_][A-Za-z0-9_]*)",$', fixture["BUILD.bazel"], re.M)
    if len(usages) != 1 or len(names) != 1:
        raise ValueError("ambiguous trusted extension/target")
    label, symbol = (v.decode("ascii") for v in usages[0])
    if source.decode().splitlines().count(symbol + " = module_extension(implementation = _impl)") != 1:
        raise ValueError("extension symbol does not bind implementation")
    argv = spec["argv"]
    if type(argv) is not list or not argv or any(type(arg) is not str for arg in argv):
        raise ValueError("typed trusted argv required")
    positions = [i for i in range(1, len(argv)) if type(argv[i]) is str and not argv[i].startswith("--")]
    if not positions:
        raise ValueError("missing trusted command")
    command_at = positions[0]
    command = argv[command_at]
    if command not in ("build", "query"):
        raise ValueError("unplanned negative command profile")
    targets = [arg for arg in argv[command_at + 1:] if not arg.startswith("--")]
    target = "//:" + names[0].decode("ascii")
    equal(targets, [target], "negative target not bound to trusted fixture")
    calls = [(n, line) for n, line in enumerate(source.decode().splitlines(), 1) if line.startswith("    value = ctx.")]
    equal(len(calls), len(case.operations), "trusted operation count")
    line, statement = calls[-1]
    operation = case.operations[-1]["method"]
    if operation not in ("download", "read") or not statement.startswith("    value = ctx." + operation + "("):
        raise ValueError("wrong trusted operation")
    extension = "@@" + label + "%" + symbol
    terminal = ("ERROR: Analysis of target '" + target + "' failed; build aborted: error evaluating module extension " + extension
                if command == "build" else "ERROR: Target parsing failed due to unexpected exception: error evaluating module extension " + extension)
    return {"command": command, "status": 1 if command == "build" else 7, "target": target,
            "extension": extension, "symbol": symbol, "operation": operation,
            "source_path": str(Path(spec["cwd"]) / label.removeprefix("//:")),
            "line": line, "column": statement.index("(") + 1, "statement": statement.strip(), "terminal": terminal}


def negative_diagnostic(case, base, origin, extension=None):
    if extension is None:
        usage = re.findall(rb'use_extension\("(//:[A-Za-z0-9_.-]+)", "([A-Za-z_][A-Za-z0-9_]*)"\)', render(case, origin)["MODULE.bazel"])
        if len(usage) != 1:
            raise ValueError("ambiguous extension diagnostic binding")
        extension = "@@" + usage[0][0].decode() + "%" + usage[0][1].decode()
    wd = str(base) + "/modextwd/+" + extension.rsplit("%", 1)[1]
    name = case.name
    if name == "http-filter-false":
        return "Error in download: java.io.IOException: No URLs left after removing plain http URLs due to missing checksum. Please provide either a checksum or an https download location."
    if name == "allfail-false":
        return "Error in download: java.io.IOException: Error downloading [" + origin + "/404, " + origin + "/also404] to " + wd + "/metadata.json: GET returned 404 Not Found"
    if name == "disabled-miss-false":
        return "Error in download: java.io.IOException: Failed to download module extension " + extension + ": download is disabled."
    if name.startswith("empty-list"):
        return "Error in download: java.io.IOException: urls not set"
    if name == "malformed-sha-transfer-success":
        return "Error in download: Checksum error in module extension " + extension + ": Invalid SHA-256 checksum 'not-hex'"
    if name == "read-watch-yes":
        return "Error in read: attempted to watch path under working directory"
    if name == "read-invalid-watch":
        return "Error in read: bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got invalid"
    if name == "read-missing":
        return "Error in read: java.io.FileNotFoundException: " + wd + "/missing (No such file or directory)"
    raise ValueError("unplanned negative")


def validate_negative(stderr, stdout, case, spec, source, base, origin):
    profile = negative_profile(spec, case, source, origin)
    line, column, statement = profile["line"], profile["column"], profile["statement"]
    source_path = profile["source_path"]
    header = f"ERROR: {source_path}:{line}:{column}: Traceback (most recent call last):"
    trace = (header + f'\n\tFile "{source_path}", line {line}, column {column}, in _impl\n\t\t'
             + statement + "\n" + negative_diagnostic(case, base, origin, profile["extension"]) + "\n").encode()
    if stderr.count(trace) != 1 or stdout:
        raise ValueError("negative lacks exact source-bound traceback/diagnostic")
    errors = [x for x in stderr.decode("latin-1").split("\n") if x.startswith("ERROR:")]
    terminal = profile["terminal"]
    allowed = [header, terminal] + (["ERROR: Build did NOT complete successfully"] if profile["command"] == "build" else [])
    equal(errors, allowed, "negative wrong error phase/footer")
    trailer = re.escape(terminal.encode()) + rb"\n"
    if profile["command"] == "build":
        trailer += (rb"INFO: Elapsed time: [0-9]+(?:\.[0-9]+)?s, Critical Path: [0-9]+(?:\.[0-9]+)?s\n"
                    rb"INFO: 1 process: 1 internal\.\nERROR: Build did NOT complete successfully\n")
    if not re.search(re.escape(trace) + trailer + rb"\Z", stderr):
        raise ValueError("negative trailer/order/count is not the planned command profile")
    error_in = [x for x in stderr.decode("latin-1").split("\n") if x.startswith("Error in ")]
    equal(error_in, [negative_diagnostic(case, base, origin, profile["extension"])], "negative wrong global/extra error")
    if re.search(rb"(?im)^.*(?:cache hits|actions executed|all actions.*cache|build completed successfully).*$", stderr + stdout):
        raise ValueError("negative action/cache fallback")
    remainder = stderr[:stderr.index(trace)].decode("latin-1").split("\n")
    java_warning = "OpenJDK 64-Bit Server VM warning: Options -Xverify:none and -noverify were deprecated in JDK 13 and will likely be removed in a future release."
    warnings = ["WARNING: Download from " + origin + path + " failed: class java.io.FileNotFoundException GET returned 404 Not Found"
                for path in case.requests] if case.name == "allfail-false" else []
    sites = [i for i, text in enumerate(source.decode().splitlines(), 1)
             if text.startswith('    print("MODULE_IO_JSON " + json.encode(')]
    expected_stream = [["callback", line, event] for line, event in zip(sites, expected_events(case))]
    # Download warnings belong to the failing operation: after its PRE, before
    # its traceback, never before BEGIN or between earlier callback operations.
    expected_stream += [["warning", text] for text in warnings]
    actual_stream = []
    if remainder and remainder[0] == java_warning:
        remainder = remainder[1:]
    marker = re.compile(r"DEBUG: " + re.escape(str(source_path)) + r":([0-9]+):10: MODULE_IO_JSON (.*)")
    for text in remainder:
        if not text:
            continue
        match = marker.fullmatch(text)
        if match:
            actual_stream.append(["callback", int(match[1]), load_json(match[2])])
            continue
        if text in warnings:
            actual_stream.append(["warning", text])
            continue
        raise ValueError("unplanned negative diagnostic/footer line")
    equal(actual_stream, expected_stream, "combined callback/warning chronology")


def json_objects(raw):
    text = raw.decode("utf-8", errors="strict")
    decoder, result = json.JSONDecoder(), []
    while text.strip():
        text = text.lstrip()
        _, end = decoder.raw_decode(text)
        result.append(load_json(text[:end]))
        text = text[end:]
    return result


def validate_stage(row, spec, case, source, base, origin):
    """Shared LIVE/REPLAY predicate. Returns semantic errors; health is separate."""
    errors = []

    def check(label, fn):
        try:
            fn()
        except (ValueError, KeyError, TypeError, UnicodeError, IndexError) as exc:
            errors.append(label + ": " + str(exc))

    check("planned invocation", lambda: equal(row["spec"], spec, "argv/engine/case/cycle/order"))
    check("health", lambda: equal(row["health"], {"timeout": False, "signal": None, "spawn_error": None,
          "truncated": False, "group_absent": True}, "reference process health"))
    stdout, stderr = unblob(row["stdout"]), unblob(row["stderr"])
    check("exit", lambda: equal(row["status"], negative_profile(spec, case, source, origin)["status"]
                               if case.result == "callback-error" else 0, "reference command-profile exit"))
    source_path = Path(spec["cwd"]) / "extension.bzl"
    events, locations, frames = [], [], b""
    try:
        events, locations, frames = reference_frames(stderr, source_path, source)
    except (ValueError, UnicodeError) as exc:
        errors.append("callback framing: " + str(exc))
    want = expected_events(case)
    allowed_empty = spec["phase"] in ("warm", "cquery") and case.result == "success"
    if not (allowed_empty and not events):
        check("callback sequence", lambda: equal(events, want, "exact callback operations/types/counts"))
        sites = [i for i, line in enumerate(source.decode().splitlines(), 1)
                 if line.startswith('    print("MODULE_IO_JSON " + json.encode(')]
        check("callback sites", lambda: equal(locations, sites[:len(want)], "print source sequence"))
    check("platform requests", lambda: equal([r["path"] for r in row["requests"] if r.get("purpose") == "platform-setup"], spec["platform_requests"], "planned setup requests"))
    check("requests", lambda: equal([r["path"] for r in row["requests"] if r.get("purpose", "ctx-api") == "ctx-api"], list(case.requests) if events else [], "ordered ctx requests"))
    if case.result == "callback-error":
        check("negative", lambda: validate_negative(stderr, stdout, case, spec, source, base, origin))
        check("no publication", lambda: equal(row["artifacts"]["repository"], None, "failed callback published"))
        check("no action output", lambda: equal(row["artifacts"]["outputs"], [], "negative output/cache fallback"))
        check("no spawns", lambda: equal(json_objects(unblob(row["artifacts"]["execlog"])), [], "negative action execution"))
    else:
        if spec["phase"] != "cquery" and stdout:
            errors.append("unexpected positive build stdout")
        if re.search(rb"(?m)^(?:ERROR:|Error in |FATAL:)", stderr):
            errors.append("success has failure diagnostics")
        repository = row["artifacts"]["repository"]
        if repository is None:
            errors.append("repository not materialized")
        else:
            check("repository content", lambda: equal(load_json(unblob(repository["data"]).decode("latin-1")), expected_consumer(case), "independent consumer oracle"))
            check("repository root", lambda: equal(repository["root"], manifest_entry("", "directory", "0755"), "root manifest"))
            check("repository file mode", lambda: equal(repository["data_mode"], "0644", "source file mode"))
            check("output count", lambda: equal(len(row["artifacts"]["outputs"]), 1, "consumer outputs"))
            for output in row["artifacts"]["outputs"]:
                check("consumer bytes", lambda: equal(output["data"], repository["data"], "consumer differs from repository"))
                check("consumer mode", lambda: equal(output["mode"], "0555", "completed action mode"))
    return {"errors": errors, "events": events, "callback_frames": blob(frames)}


def validate_completed_case(case, records):
    """No positive MATCHED until cquery, real files and cold/warm action log agree."""
    if case.result != "success":
        return []
    try:
        cold, warm, query = (records[k] for k in ("cold", "warm", "cquery"))
        lines = unblob(query["stdout"]).decode("ascii").splitlines()
        source_path = "external/+io+made/data.json"
        outputs = [p for p in lines if re.fullmatch(r"bazel-out/[A-Za-z0-9_-]+/bin/consumed\.json", p)]
        equal(sorted(lines), sorted([source_path, *outputs]), "cquery actual provenance")
        equal(len(outputs), 1, "cquery unique output")
        output_path = outputs[0]
        for row in (cold, warm, query):
            base = Path(row["spec"]["cwd"]).parents[1] / "base" / case.name
            scratch = base.parents[1]
            repo = row["artifacts"]["repository"]
            equal(repo["path"], str(base / "external/+io+made"), "actual repository source path")
            if not Path(repo["resolved_path"]).is_relative_to(scratch):
                raise ValueError("repository resolution escapes owned scratch")
            execroot = Path(row["artifacts"]["execroot"])
            equal(str(execroot.parent), str(base / "execroot"), "actual execroot provenance")
            equal(row["artifacts"]["repository"]["exec_path"], source_path, "source identity")
            equal([o["exec_path"] for o in row["artifacts"]["outputs"]], [output_path], "output identity")
            output = row["artifacts"]["outputs"][0]
            equal(output["path"], str(execroot / output_path), "actual cquery/output path")
            if not Path(output["resolved_path"]).is_relative_to(base):
                raise ValueError("consumer resolution escapes actual output base")
            equal(row["artifacts"]["outputs"][0]["data"], cold["artifacts"]["outputs"][0]["data"], "cold/warm/cquery content stability")
        spawns = json_objects(unblob(cold["artifacts"]["execlog"]))
        equal(len(spawns), 1, "cold actual action count")
        spawn = spawns[0]
        equal([spawn["mnemonic"], spawn["exitCode"], spawn.get("cacheHit", False)], ["ModuleIOConsumer", 0, False], "cold action/cache")
        equal([i["path"] for i in spawn["inputs"]], [source_path], "exact action input path")
        equal([o["path"] for o in spawn["actualOutputs"]], [output_path], "exact action output path")
        for part in (spawn["inputs"][0], spawn["actualOutputs"][0]):
            equal(part["digest"]["hash"], cold["artifacts"]["outputs"][0]["data"]["sha256"], "actual action digest")
        equal(spawn["commandArgs"], ["/bin/bash", "-c", '/bin/cat "$1" > "$2"', "", source_path, output_path], "fixture action command")
        equal(json_objects(unblob(warm["artifacts"]["execlog"])), [], "warm executed action/cache stability")
        return []
    except (ValueError, KeyError, TypeError, UnicodeError, IndexError) as exc:
        return ["completed consumer/cache provenance: " + str(exc)]


def expected_platform_files():
    with tarfile.open(fileobj=io.BytesIO(platforms_archive()), mode="r:gz") as archive:
        files = {member.name: archive.extractfile(member).read() for member in archive.getmembers() if member.isfile()}
    module = next(r for r in platform_closure() if r["kind"] == "module-file")
    files["MODULE.bazel"] = unblob(module["data"])
    return files


def validate_setup_stage(row, spec, root):
    """Separate setup gate: disabled downloads, verified distdir, no extension API."""
    errors = []
    try:
        equal(row["spec"], spec, "setup invocation")
        equal(row["status"], 0, "setup command exit")
        if "--repository_disable_download" not in spec["argv"] or "--repository_cache=" not in spec["argv"]:
            raise ValueError("setup must prove distdir reuse before explicit deny with checksum cache disabled")
        equal(row["requests"], [], "setup required zero HTTP requests (distdir reuse)")
        stderr = unblob(row["stderr"])
        if MARKER in stderr or re.search(rb"(?m)^(?:ERROR:|Error in |FATAL:)", stderr):
            raise ValueError("setup diagnostics/global")
        if spec["phase"] == "cold":
            equal(row["setup_before"], {"platforms_absent": True, "contents_cache_empty": True}, "cold private cache proof")
            equal(unblob(row["stdout"]).hex(), "", "setup build stdout")
        artifacts = row["artifacts"]
        seed = (FIXTURES / "setup_seed.txt").read_bytes()
        source = artifacts["setup_source"]
        equal(source["path"], str(root / "scratch/workspace" / SETUP_CASE / "seed.txt"), "setup source path")
        equal(source["data"], blob(seed), "setup source checksum/bytes")
        equal(source["mode"], "0644", "setup source mode")
        equal(source["root"], manifest_entry("", "directory", "0755"), "setup workspace root manifest")
        equal(len(artifacts["outputs"]), 1, "setup completed output count")
        equal(artifacts["outputs"][0]["data"], blob(seed), "setup action consumer bytes")
        equal(artifacts["outputs"][0]["mode"], "0555", "setup completed action mode")
        expected = expected_platform_files()
        actual = artifacts["platform_files"]
        equal(sorted(actual), sorted(expected), "actual genuine platform file coverage")
        for name, data in expected.items():
            equal(actual[name]["data"], blob(data), "actual pinned platform file: " + name)
            equal(actual[name]["path"], str(root / "scratch/base" / SETUP_CASE / "external/platforms" / name), "platform source path")
    except (ValueError, KeyError, TypeError, UnicodeError, IndexError) as exc:
        errors.append("platform setup criterion: " + str(exc))
    return {"errors": errors, "events": [], "callback_frames": blob(b"")}


def validate_setup_completed(records):
    try:
        cold, query = records["cold"], records["cquery"]
        paths = unblob(query["stdout"]).decode("ascii").splitlines()
        output = cold["artifacts"]["outputs"][0]
        output_path = output["exec_path"]
        if not re.fullmatch(r"bazel-out/[A-Za-z0-9_-]+/bin/consumed\.json", output_path):
            raise ValueError("setup output identity")
        equal(sorted(paths), sorted(["seed.txt", output_path]), "setup cquery input/output provenance")
        for row in (cold, query):
            base = Path(row["spec"]["cwd"]).parents[1] / "base" / SETUP_CASE
            execroot = Path(row["artifacts"]["execroot"])
            equal(str(execroot.parent), str(base / "execroot"), "setup actual execroot")
            equal(row["artifacts"]["outputs"][0]["path"], str(execroot / output_path), "setup actual output path")
            equal(row["artifacts"]["outputs"][0]["data"], output["data"], "setup build/cquery output stability")
        spawns = json_objects(unblob(cold["artifacts"]["execlog"]))
        equal(len(spawns), 1, "setup cold action count")
        spawn = spawns[0]
        equal([spawn["mnemonic"], spawn["exitCode"], spawn.get("cacheHit", False)], ["ModuleIOConsumer", 0, False], "setup actual action, not fallback")
        equal([i["path"] for i in spawn["inputs"]], ["seed.txt"], "setup action input path")
        equal([i["path"] for i in spawn["actualOutputs"]], [output_path], "setup action output path")
        for part in (spawn["inputs"][0], spawn["actualOutputs"][0]):
            equal(part["digest"]["hash"], output["data"]["sha256"], "setup action digest")
        equal(spawn["commandArgs"], ["/bin/bash", "-c", '/bin/cat "$1" > "$2"', "", "seed.txt", output_path], "setup action command")
        return []
    except (ValueError, KeyError, TypeError, UnicodeError, IndexError) as exc:
        return ["platform setup completed criterion: " + str(exc)]


class HealthFailure(RuntimeError):
    pass


def file_pin(path):
    path = Path(path)
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise HealthFailure("not a regular pinned input: " + str(path))
    return {"sha256": digest(path.read_bytes()), "size": info.st_size,
            "mode": format(stat.S_IMODE(info.st_mode), "04o")}


def files_under(root):
    root = Path(root)
    if root.is_symlink():
        raise HealthFailure("symlink input root: " + str(root))
    if not root.exists():
        return []
    if not root.is_dir():
        raise HealthFailure("input root is not a directory: " + str(root))
    result = []
    for path in sorted(root.rglob("*")):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            raise HealthFailure("symlink in public input: " + str(path))
        if stat.S_ISREG(mode):
            result.append(path)
        elif not stat.S_ISDIR(mode):
            raise HealthFailure("special file in input inventory: " + str(path))
    return result


def inventory_key(name):
    if (type(name) is not str or not name or "\\" in name or "\x00" in name
            or any(part in ("", ".", "..") for part in name.split("/"))):
        raise ValueError("noncanonical/traversing inventory path")
    return name


def inventory_map(value):
    if type(value) is not dict:
        raise ValueError("inventory must be a path map")
    for name, pin in value.items():
        inventory_key(name)
        if name == "SHA256SUMS.json":
            raise ValueError("top inventory cannot hash itself")
        equal(sorted(pin), ["mode", "sha256", "size"], "inventory pin schema")
        if (type(pin["size"]) is not int or pin["size"] < 0 or type(pin["sha256"]) is not str
                or not re.fullmatch(r"[0-9a-f]{64}", pin["sha256"])
                or type(pin["mode"]) is not str or not re.fullmatch(r"[0-7]{4}", pin["mode"])):
            raise ValueError("invalid typed inventory pin")
    return value


def archive_inventory(root):
    root = Path(root).absolute()
    if not root.is_dir() or root.resolve() != root:
        raise ValueError("archive root must be a real canonical directory")
    # Only THIS manifest's relative path is excluded. Every nested manifest is
    # an ordinary hashed input; basename matching would silently omit evidence.
    return {inventory_key(str(p.relative_to(root))): file_pin(p) for p in files_under(root)
            if p.relative_to(root).as_posix() != "SHA256SUMS.json"}


def verify_inventory(root, manifest):
    expected = inventory_map(manifest)
    equal(archive_inventory(root), expected, "exact archive inventory membership/bytes/modes")


def load_verified_inventory(root, expected_sha256):
    actual = archive_inventory(root)  # Reject links/special files before reading the manifest.
    raw = (Path(root) / "SHA256SUMS.json").read_bytes()
    equal(digest(raw), expected_sha256, "external archive/report pin")
    manifest = inventory_map(load_json(raw))
    equal(actual, manifest, "complete exact inventory")
    return manifest


def historical_inventory(root, freeze, manifest):
    """Explicit audit of the old basename-exclusion defect; NOT a new-format pass."""
    root = Path(root).absolute()
    expected, actual = inventory_map(manifest), archive_inventory(root)
    legacy_omissions = {}
    for path, pin in freeze["pins"].items():
        p = Path(path)
        if p.is_relative_to(root):
            relative = str(p.relative_to(root))
            if p.name == "SHA256SUMS.json" and relative != "SHA256SUMS.json":
                inventory_key(relative)
                legacy_omissions[relative] = pin
    equal(sorted(set(actual) - set(expected)), sorted(legacy_omissions), "exact historical nested omissions")
    equal({k: v for k, v in actual.items() if k not in legacy_omissions}, expected, "historical top manifest pins")
    equal({k: actual[k] for k in legacy_omissions}, legacy_omissions, "historical frozen nested-input pins")
    return {"classification": "LEGACY_EXCLUSIONS_AUDITED_NOT_TOP_MANIFEST_MEMBERS", "omitted_nested": legacy_omissions,
            "complete_inventory": actual}


def save_json(path, value):
    Path(path).write_bytes(json.dumps(value, sort_keys=True, indent=2, ensure_ascii=True, allow_nan=False).encode("ascii") + b"\n")


def process_group_exists(pid):
    try:
        os.killpg(pid, 0)
        return True
    except ProcessLookupError:
        return False


def kill_group(process):
    if process_group_exists(process.pid):
        os.killpg(process.pid, signal.SIGKILL)
    process.wait(timeout=10)


def safe_read(path, root):
    resolved = path.resolve(strict=True)
    if not resolved.is_relative_to(root.resolve()) or not resolved.is_file():
        raise HealthFailure("artifact outside owned scratch or not regular: " + str(path))
    if resolved.stat().st_size > 8 * 1024 * 1024:
        raise HealthFailure("artifact size budget")
    return resolved.read_bytes()


def observe_artifacts(root, case, phase):
    scratch = root / "scratch"
    base = scratch / "base" / case.name
    wd = base / "modextwd/+io"
    if wd.exists() or wd.is_symlink():
        raise HealthFailure("extension workdir leaked: " + str(wd))
    repository = base / "external/+io+made"
    result = {"repository": None, "outputs": [], "execlog": blob(b""),
              "workdir_absent": True, "workdir_path": str(wd), "execroot": None}
    log = root / "raw" / case.name / (phase + ".exec.json")
    if log.exists():
        result["execlog"] = blob(log.read_bytes())
    if repository.exists():
        data = safe_read(repository / "data.json", scratch)
        info = repository.stat()
        if not stat.S_ISDIR(info.st_mode) or not repository.resolve().is_relative_to(scratch):
            raise HealthFailure("repository root provenance")
        result["repository"] = {"path": str(repository), "resolved_path": str(repository.resolve()),
            "exec_path": "external/+io+made/data.json", "data": blob(data),
            "data_mode": format(stat.S_IMODE((repository / "data.json").stat().st_mode), "04o"),
            "root": manifest_entry("", "directory", format(stat.S_IMODE(info.st_mode), "04o"))}
    exec_parent = base / "execroot"
    if exec_parent.exists():
        roots = [p for p in exec_parent.iterdir() if p.is_dir()]
        if len(roots) != 1:
            raise HealthFailure("ambiguous actual execroot")
        execroot = roots[0]
        result["execroot"] = str(execroot)
        for path in sorted(execroot.glob("bazel-out/*/bin/consumed.json")):
            data = safe_read(path, scratch)
            result["outputs"].append({"path": str(path), "resolved_path": str(path.resolve()),
                "exec_path": str(path.relative_to(execroot)), "mode": format(stat.S_IMODE(path.stat().st_mode), "04o"),
                "data": blob(data)})
    if case.name == SETUP_CASE:
        source = scratch / "workspace" / SETUP_CASE / "seed.txt"
        result["setup_source"] = {"path": str(source), "data": blob(safe_read(source, scratch)),
            "mode": format(stat.S_IMODE(source.stat().st_mode), "04o"),
            "root": manifest_entry("", "directory", format(stat.S_IMODE(source.parent.stat().st_mode), "04o"))}
        platform_root = base / "external/platforms"
        result["platform_files"] = {}
        if platform_root.exists():
            for name in expected_platform_files():
                path = platform_root / name
                if path.is_file():
                    result["platform_files"][name] = {"path": str(path), "resolved_path": str(path.resolve()),
                        "mode": format(stat.S_IMODE(path.stat().st_mode), "04o"), "data": blob(safe_read(path, scratch))}
    return result


def server_header_policy():
    name = "module_extension_io_http_header_policy"
    if name not in sys.modules:
        spec = importlib.util.spec_from_file_location(name, FIXTURES / "http_server.py")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    return sys.modules[name]


def request_header_view(request, origin, *, allow_legacy_headers=False):
    """Validate the same ordered fields as the server, without rewriting evidence."""
    match = re.fullmatch(r"http://127\.0\.0\.1:([1-9][0-9]{0,4})", origin or "")
    if not match or int(match[1]) > 65535:
        raise ValueError("exact owned fixture authority/port is required")
    headers = request["headers"]
    if type(headers) is not dict:
        raise ValueError("captured HTTP header dictionary required")
    legacy = "header_items" not in request
    if legacy and not allow_legacy_headers:
        raise ValueError("dict-only historical headers cannot prove field multiplicity; preparation-only")
    items = list(headers.items()) if legacy else request["header_items"]
    view = server_header_policy().validate_header_fields(items, origin.removeprefix("http://"))
    # Admission/deduplication above happens BEFORE dict construction. This
    # equality only binds the convenience rendering to the preserved field list.
    equal(dict(items), headers, "captured raw header list/dictionary binding")
    return {"semantic_name_view": view,
            "evidence": "DICT_ONLY_LIMITED_PREPARATION" if legacy else "ORDERED_FIELD_ITEMS",
            "duplicate_absence_proven": not legacy}


def stage_health(row, routes, origin=None, *, allow_legacy_headers=False):
    equal(row["health"], {"timeout": False, "signal": None, "spawn_error": None,
                          "truncated": False, "group_absent": True}, "process health")
    if type(row["status"]) is not int or row["status"] not in (0, 1, 7):
        raise HealthFailure("crash/infrastructure exit: " + str(row["status"]))
    if re.search(rb"(?m)^(?:FATAL:|A fatal error has been detected|java.lang.OutOfMemoryError)", unblob(row["stderr"])):
        raise HealthFailure("JVM infrastructure failure")
    header_evidence = []
    for request in row["requests"]:
        if (request["unsafe"] or request["method"] != "GET" or request.get("parsed", True) is not True
                or request["peer"] != "127.0.0.1" or request["path"] not in routes):
            raise HealthFailure("unsafe fixture request")
        try:
            header_evidence.append(request_header_view(request, origin, allow_legacy_headers=allow_legacy_headers))
        except ValueError as error:
            raise HealthFailure("fixture HTTP field admission: " + str(error)) from error
        if "request_line_base64" in request:
            raw_line = base64.b64decode(request["request_line_base64"], validate=True)
            words = raw_line.decode("ascii").strip().split()
            if len(words) != 3 or words[2] not in ("HTTP/1.0", "HTTP/1.1"):
                raise HealthFailure("invalid audited request line")
            equal(words[:2], [request["method"], request["path"]], "raw method/path audit binding")
        route = routes[request["path"]]
        equal(request.get("purpose", "ctx-api"), route.get("purpose", "ctx-api"), "setup/API request classification")
        equal([request["method"], request["status"], request["response_headers"], request["body_sha256"], request["body_size"]],
               ["GET", route["status"], route["headers"], route["body"]["sha256"], route["body"]["size"]], "server input drift")
    return {"header_evidence": header_evidence,
            "legacy_dict_only_requests": sum(not row["duplicate_absence_proven"] for row in header_evidence)}


def validate_checkpoint(value, freeze):
    keys = {"pins_sha256", "scratch_bytes", "elapsed_seconds"}
    if "platform_staging" in freeze:
        keys.add("platform_distdir")
    if "platform_closure_staging" in freeze:
        keys.add("platform_closure")
    if "runtime_binding" in freeze:
        keys.add("runtime_binding")
    equal(sorted(value), sorted(keys), "checkpoint schema")
    equal(value["pins_sha256"], digest(canonical(freeze["pins"])), "stage source binding")
    if type(value["scratch_bytes"]) is not int or not 0 <= value["scratch_bytes"] <= freeze["scratch_limit_bytes"]:
        raise ValueError("typed scratch resource proof")
    elapsed = value["elapsed_seconds"]
    if type(elapsed) not in (int, float) or not math.isfinite(elapsed) or not 0 <= elapsed <= freeze["aggregate_timeout_seconds"]:
        raise ValueError("typed aggregate time proof")
    if "platform_staging" in freeze:
        equal(value["platform_distdir"], freeze["platform_staging"], "per-launch staged archive pin")
    if "platform_closure_staging" in freeze:
        equal(value["platform_closure"], freeze["platform_closure_staging"], "per-launch governed closure pin")
    if "runtime_binding" in freeze:
        equal(value["runtime_binding"], freeze["runtime_binding"], "per-launch frozen native runtime ledger binding")


def validate_server_ready(ready, freeze):
    raw = unblob(ready)
    if raw.count(b"\n") != 1 or not raw.endswith(b"\n"):
        raise ValueError("one server readiness frame required")
    equal(load_json(raw), {"ready": True, "address": ["127.0.0.1", int(freeze["origin"].rsplit(":", 1)[1])]}, "server startup binding")


def validate_server_stop(server, ready, freeze):
    validate_server_ready(ready, freeze)
    equal(sorted(server), ["group_absent", "launched", "status", "stderr", "stdout"], "server stop schema")
    equal([server["launched"], server["status"], server["group_absent"]], [True, 0, True], "server lifecycle status")
    equal(unblob(server["stderr"]).hex(), "", "server lifecycle stderr")
    equal(unblob(server["stdout"]).hex(), (unblob(ready) + b"SERVER_CLOSED\n").hex(), "exact ready/closed server stream")


def validate_lifecycle_row(row, spec, freeze, ready=None):
    """Identical phase-specific proof requirements for live completion and replay."""
    equal(row["spec"], spec, "lifecycle trusted invocation")
    phase = spec["phase"]
    required = ({"before"} if phase == "server-start" else {"before", "after"} if spec["row_type"] == "command"
                else set() if phase in ("server-stop", "cleanup", "pins-end") else {"checkpoint"})
    equal(sorted(set(row) & {"before", "after", "checkpoint"}), sorted(required), "mandatory phase checkpoints")
    for key in required:
        validate_checkpoint(row[key], freeze)
    if required == {"before", "after"} and row["after"]["elapsed_seconds"] < row["before"]["elapsed_seconds"]:
        raise ValueError("reversed command resource checkpoints")
    if spec["row_type"] == "command" and (type(row["pid"]) is not int or row["pid"] <= 0):
        raise ValueError("typed owned process identity")
    if phase == "server-start":
        validate_server_ready(row["ready"], freeze)
    elif phase == "server-stop":
        if ready is None:
            raise ValueError("server stop lacks prior trusted readiness")
        validate_server_stop(row["server"], ready, freeze)
    elif phase == "cleanup":
        value = row["cleanup"]
        equal(sorted(value), ["all_groups_absent", "groups", "scratch_absent"], "cleanup schema")
        equal([value["all_groups_absent"], value["scratch_absent"]], [True, True], "cleanup proof")
        if (type(value["groups"]) is not list or any(type(pid) is not int or pid <= 0 for pid in value["groups"])
                or len(set(value["groups"])) != len(value["groups"])):
            raise ValueError("typed unique cleanup process groups")
    elif phase == "pins-end":
        root = Path(freeze["plan"][0]["cwd"]).parent
        remaining = {p: pin for p, pin in freeze["pins"].items() if not Path(p).is_relative_to(root / "scratch")}
        equal(row["pins_sha256"], digest(canonical(remaining)), "terminal source binding")


class CaptureRunner:
    def __init__(self, root, freeze, timeout):
        self.root, self.freeze, self.timeout = root, freeze, timeout
        self.report = {"state": "RUNNING", "scope": freeze["scope"], "freeze_sha256": digest((root / "freeze.json").read_bytes()),
                       "rows": [], "cases": [], "cleanup": None, "blocker": None}
        self.started = time.monotonic()
        self.groups = []
        self.server = None
        self.server_ready = b""
        self.server_stopped = False
        self.active_process = None

    def persist(self):
        save_json(self.root / "report.json", self.report)

    def checkpoint(self):
        staged_platform = platform_distdir_pin(self.root)
        staged_closure = platform_closure_pins(self.root)
        for path, pin in self.freeze["pins"].items():
            equal(file_pin(path), pin, "stable reference-only input drift: " + path)
        if time.monotonic() - self.started > 1200:
            raise HealthFailure("aggregate 1200-second budget")
        if len(self.requests()) > 512:
            raise HealthFailure("aggregate 512-request budget")
        usage = sum(p.stat().st_size for p in (self.root / "scratch").rglob("*") if p.is_file() and not p.is_symlink())
        if usage > 1536 * 1024 * 1024:
            raise HealthFailure("scratch 1536-MiB budget")
        result = {"pins_sha256": digest(canonical(self.freeze["pins"])), "scratch_bytes": usage,
                "platform_distdir": staged_platform,
                "platform_closure": staged_closure,
                "elapsed_seconds": time.monotonic() - self.started}
        validate_checkpoint(result, self.freeze)
        return result

    def append(self, spec, **observations):
        equal(spec, self.freeze["plan"][len(self.report["rows"])], "trusted runner chronology")
        row = {"spec": spec, "time_ns": time.time_ns(), **observations}
        # Reference-command rows are persisted provisionally, then validated
        # once their mandatory AFTER checkpoint has actually been measured.
        if spec["row_type"] != "command" or spec["phase"] == "server-start":
            validate_lifecycle_row(row, spec, self.freeze, blob(self.server_ready))
        self.report["rows"].append(row)
        self.persist()
        return row

    def requests(self):
        path = self.root / "requests.jsonl"
        return [load_json(line) for line in path.read_bytes().split(b"\n") if line] if path.exists() else []

    def command(self, spec):
        before = self.checkpoint()
        setup_before = None
        if spec["case"] == SETUP_CASE and spec["phase"] == "cold":
            setup_before = {"platforms_absent": not (self.root / "scratch/base" / SETUP_CASE / "external/platforms").exists(),
                            "contents_cache_empty": not files_under(self.root / "scratch/contents")}
        request_start = len(self.requests())
        health = {"timeout": False, "signal": None, "spawn_error": None,
                  "truncated": False, "group_absent": True}
        directory = self.root / "commands" / f'{spec["ordinal"]:04d}'
        directory.mkdir(parents=True)
        stdout = stderr = b""
        status = None
        process = None
        try:
            process = subprocess.Popen(spec["argv"], cwd=spec["cwd"], env=spec["env"],
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
                                       umask=spec.get("umask", -1))
            self.active_process = process
            self.groups.append(process.pid)
            try:
                stdout, stderr = process.communicate(timeout=self.timeout)
            except subprocess.TimeoutExpired:
                health["timeout"] = True
                kill_group(process)
                stdout, stderr = process.communicate()
            status = process.returncode
            health["signal"] = -status if status < 0 else None
            health["group_absent"] = not process_group_exists(process.pid)
            if not health["group_absent"]:
                kill_group(process)
        except OSError as exc:
            health["spawn_error"] = str(exc)
        health["truncated"] = max(len(stdout), len(stderr)) > 8 * 1024 * 1024
        (directory / "stdout.raw").write_bytes(stdout)
        (directory / "stderr.raw").write_bytes(stderr)
        row = self.append(spec, status=status, health=health, pid=process.pid if process else None,
                          stdout=blob(stdout), stderr=blob(stderr), before=before,
                          requests=self.requests()[request_start:], raw_directory=str(directory))
        save_json(directory / "process.json", row)
        if setup_before is not None:
            row["setup_before"] = setup_before
        stage_health(row, self.freeze["responses"], self.freeze["origin"])
        row["after"] = self.checkpoint()
        validate_lifecycle_row(row, spec, self.freeze)
        self.persist()
        self.active_process = None
        return row

    def start_server(self, spec, sock):
        before = self.checkpoint()
        self.server = subprocess.Popen(spec["argv"], cwd=spec["cwd"], env=spec["env"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, pass_fds=(sock.fileno(),), start_new_session=True,
            umask=spec.get("umask", -1))
        self.groups.append(self.server.pid)
        with selectors.DefaultSelector() as selector:
            selector.register(self.server.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=10):
                raise HealthFailure("server readiness timeout")
            self.server_ready = self.server.stdout.readline()
        equal(load_json(self.server_ready), {"ready": True, "address": list(sock.getsockname())}, "server binding")
        self.append(spec, pid=self.server.pid, before=before, ready=blob(self.server_ready))
        sock.close()

    def stop_server(self):
        if self.server is None:
            return {"launched": False}
        if self.server.poll() is None:
            os.killpg(self.server.pid, signal.SIGTERM)
        try:
            stdout, stderr = self.server.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            kill_group(self.server)
            stdout, stderr = self.server.communicate()
        (self.root / "server.stdout.raw").write_bytes(self.server_ready + stdout)
        (self.root / "server.stderr.raw").write_bytes(stderr)
        self.server_stopped = True
        result = {"launched": True, "status": self.server.returncode,
                  "stdout": blob(self.server_ready + stdout), "stderr": blob(stderr),
                  "group_absent": not process_group_exists(self.server.pid)}
        validate_server_stop(result, blob(self.server_ready), self.freeze)
        return result


def freeze_reference(args, sock):
    setup = getattr(args, "setup_only", False)
    api_smoke = getattr(args, "api_smoke_only", False)
    gate = None if setup else require_setup_receipt(args.setup_receipt, args.setup_freeze_sha256,
                                                  args.setup_archive_sha256, args.reference)
    api_gate = None if setup or api_smoke else require_api_smoke_receipt(args.api_smoke_receipt,
        args.api_smoke_freeze_sha256, args.api_smoke_archive_sha256, args.reference)
    planned_cases = selected_cases(setup, api_smoke)
    root = Path(args.artifact_dir).absolute()
    if root.parent != Path("/tmp/opencode") or not root.name.startswith("module-extension-io-") or root.exists():
        raise ValueError("new owned /tmp/opencode/module-extension-io-* artifact directory required")
    reference = Path(args.reference).resolve(strict=True)
    expected_elf = load_json((FIXTURES / "contract.json").read_bytes())["reference_sha256"]
    equal(args.reference_sha256, expected_elf, "official reference ELF pin")
    equal(file_pin(reference)["sha256"], expected_elf, "reference executable bytes")
    with reference.open("rb") as stream:
        equal(stream.read(4).hex(), "7f454c46", "reference must be direct ELF, not wrapper")
    official = load_json((FIXTURES / "official_sources.json").read_bytes())
    official_paths = [Path(args.official_source) / name for name in official["sha256"]]
    for path in official_paths:
        equal(file_pin(path)["sha256"], official["sha256"][path.name], "immutable official source")
    closure_metadata = load_json((FIXTURES / "platform_closure.json").read_bytes())
    closure_sources = [Path(args.official_tree) / name for name in closure_metadata["official_files"]]
    for path in closure_sources:
        equal(file_pin(path)["sha256"], closure_metadata["official_files"][str(path.relative_to(args.official_tree))], "official closure source pin")
    closure = platform_closure()
    tracked_registry = ROOT / closure_metadata["registry_relative_root"]
    tracked_inputs = [tracked_registry / "modules/platforms/1.0.0/source.json"]
    tracked_inputs += [tracked_registry / r["registry_path"] for r in closure if r["registry_path"]]
    for path in tracked_inputs:
        equal((Path(args.registry_input) / path.relative_to(tracked_registry)).read_bytes().hex(),
              path.read_bytes().hex(), "runtime registry closure differs from tracked bytes")
    registry_paths = files_under(args.registry_input)
    if not registry_paths or not (Path(args.registry_input) / "bazel_registry.json").is_file():
        raise ValueError("explicit local registry metadata input required")
    qa_paths = [ROOT / "scripts/validate_module_extension_io.py", ROOT / "tests/test_validate_module_extension_io.py",
                *files_under(FIXTURES), PLATFORMS]
    qa_paths = [p for p in qa_paths if "__pycache__" not in p.parts and p.suffix != ".pyc"]
    original_paths = [reference, Path(sys.executable).resolve(), Path("/bin/cat").resolve(), Path("/bin/bash").resolve(),
                      *qa_paths, *official_paths, *closure_sources, *tracked_inputs, *registry_paths]
    if gate:
        original_paths.extend(files_under(gate["path"]))
    if api_gate:
        original_paths.extend(files_under(api_gate["path"]))
    pins = {str(p): file_pin(p) for p in original_paths}
    root.mkdir(mode=0o700)
    scratch = root / "scratch"
    for name in ("home", "tmp", "workspace", "base"):
        (scratch / name).mkdir(parents=True, exist_ok=True, mode=0o700)
    for path in qa_paths:
        target = root / "inputs/qa" / path.relative_to(ROOT)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    for path in official_paths:
        target = root / "inputs/official" / path.name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    for path in closure_sources:
        target = root / "inputs/official-tree" / path.relative_to(args.official_tree)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    for path in registry_paths:
        target = root / "inputs/registry" / path.relative_to(Path(args.registry_input))
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    for requirement in closure:
        distdir = root / requirement["distdir"]
        distdir.mkdir(parents=True, exist_ok=True)
        destination = distdir / requirement["basename"]
        data = unblob(requirement["data"])
        if destination.exists():
            equal(destination.read_bytes().hex(), data.hex(), "distfile basename collision")
        else:
            destination.write_bytes(data)
    origin = "http://127.0.0.1:" + str(sock.getsockname()[1])
    (root / "inputs/downloader.conf").write_bytes(downloader_config(origin))
    if gate:
        (root / "inputs/setup-gate").mkdir()
        for name in ("freeze.json", "report.json", "SHA256SUMS.json"):
            shutil.copyfile(Path(gate["path"]) / name, root / "inputs/setup-gate" / name)
    if api_gate:
        (root / "inputs/api-smoke-gate").mkdir()
        for name in ("freeze.json", "report.json", "SHA256SUMS.json"):
            shutil.copyfile(Path(api_gate["path"]) / name, root / "inputs/api-smoke-gate" / name)
    fixtures = {}
    for case in planned_cases:
        fixtures[case.name] = {name: blob(data) for name, data in render(case, origin).items()}
        for name, value in fixtures[case.name].items():
            for parent in (root / "inputs/workspaces" / case.name, scratch / "workspace" / case.name):
                parent.mkdir(parents=True, exist_ok=True)
                (parent / name).write_bytes(unblob(value))
        (root / "raw" / case.name).mkdir(parents=True)
    responses = response_plan()
    # /never is safe to serve if a broken destination-reuse path requests it;
    # that is a semantic mismatch, not an unplanned external-host request.
    responses["/never"] = {"status": 200, "headers": {"Content-Length": str(len(payloads()["versions"]))}, "body": blob(payloads()["versions"])}
    save_json(root / "inputs/responses.json", responses)
    plan = reference_plan(str(root), str(reference), setup=setup, api_smoke=api_smoke)
    server = next(row for row in plan if row["phase"] == "server-start")
    server.update(row_type="command", engine="fixture", argv=[str(Path(sys.executable).resolve()), "-I",
        str(root / "inputs/qa/tests/fixtures/module_extension_io/http_server.py"), str(sock.fileno()),
        str(root / "inputs/responses.json"), str(root / "requests.jsonl")])
    for path in files_under(root / "inputs"):
        pins[str(path)] = file_pin(path)
    for case in planned_cases:
        for name in fixtures[case.name]:
            path = scratch / "workspace" / case.name / name
            pins[str(path)] = file_pin(path)
    freeze = {"scope": "REFERENCE_ONLY: QA/fixtures/registry/archive/official copies/reference ELF; changing native/Rust EXCLUDED",
              "mode": "platform-setup" if setup else "ctx-smoke" if api_smoke else "ctx-api", "setup_receipt": gate,
              "api_smoke_receipt": api_gate,
              "platform_staging": platform_distdir_pin(root),
              "platform_closure_staging": platform_closure_pins(root),
              "platform_closure": [{**r, "url": PLATFORM_URL if r["kind"] == "archive" else
                                    (root / "inputs/registry" / r["registry_path"]).as_uri()} for r in closure],
              "reference": str(reference), "origin": origin, "pins": pins, "plan": plan,
              "fixtures": fixtures, "responses": responses, "official_source_commit": official["source_commit"],
              "case_definitions": [asdict(c) for c in planned_cases], "command_timeout_seconds": args.timeout,
              "aggregate_timeout_seconds": 1200, "scratch_limit_bytes": 1536 * 1024 * 1024,
              "deferred": load_json((FIXTURES / "deferred.json").read_bytes())}
    save_json(root / "freeze.json", freeze)
    return root, freeze


def check_postcapture_retained(root, report, freeze):
    if report["state"] in ("COMPLETED_CAPTURE", "COMPLETED_SETUP", "COMPLETED_API_SMOKE"):
        try:
            bind_retained_bytes(root, report, freeze)
        except (ValueError, KeyError, OSError, HealthFailure) as exc:
            report["state"] = "EXECUTION_BLOCKER"
            report["blocker"] = "retained postcapture consistency: " + str(exc)


def capture_reference(args):
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sock.listen(8)
    try:
        root, freeze = freeze_reference(args, sock)
    except BaseException:
        sock.close()
        raise
    runner = CaptureRunner(root, freeze, args.timeout)
    setup = freeze["mode"] == "platform-setup"
    api_smoke = freeze["mode"] == "ctx-smoke"
    cases_by_name = {c.name: c for c in selected_cases(setup, api_smoke)}
    records, errors = {}, {}
    try:
        for spec in freeze["plan"]:
            phase, name = spec["phase"], spec["case"]
            if phase == "server-start":
                runner.start_server(spec, sock)
            elif spec["row_type"] == "command":
                row = runner.command(spec)
                if phase == "version":
                    if row["status"] != 0 or unblob(row["stdout"]).split(b"\n").count(b"Build label: 9.3.0") != 1:
                        raise HealthFailure("pinned reference version mismatch")
                else:
                    case = cases_by_name[name]
                    row["artifacts"] = observe_artifacts(root, case, phase)
                    row["validation"] = (validate_setup_stage(row, spec, root) if setup else validate_stage(row, spec, case,
                        unblob(freeze["fixtures"][name]["extension.bzl"]), root / "scratch/base" / name, freeze["origin"]))
                    records.setdefault(name, {})[phase] = row
                    errors.setdefault(name, []).extend(phase + ": " + e for e in row["validation"]["errors"])
                runner.persist()
                if (setup or api_smoke) and name and errors[name]:
                    raise HealthFailure(freeze["mode"] + " smoke stopped before matrix: " + "; ".join(errors[name]))
            elif phase == "case-end":
                errors[name].extend(validate_setup_completed(records[name]) if setup else validate_completed_case(cases_by_name[name], records[name]))
                result = {"name": name, "classification": "UNMATCHED" if errors[name] else "MATCHED", "errors": errors[name]}
                runner.report["cases"].append(result)
                runner.append(spec, result=result, checkpoint=runner.checkpoint())
                if (setup or api_smoke) and errors[name]:
                    raise HealthFailure(freeze["mode"] + " smoke completed criterion: " + "; ".join(errors[name]))
            elif phase == "server-stop":
                runner.append(spec, server=runner.stop_server())
            elif phase == "cleanup":
                # Final full pins check while the private fixture copies still exist.
                runner.checkpoint()
                absent = all(not process_group_exists(pid) for pid in runner.groups)
                if not absent:
                    raise HealthFailure("owned process group survived")
                shutil.rmtree(root / "scratch")
                runner.report["cleanup"] = {"groups": runner.groups, "all_groups_absent": absent,
                                            "scratch_absent": not (root / "scratch").exists()}
                runner.append(spec, cleanup=runner.report["cleanup"])
            elif phase == "pins-end":
                remaining = {p: pin for p, pin in freeze["pins"].items() if not Path(p).is_relative_to(root / "scratch")}
                equal({p: file_pin(p) for p in remaining}, remaining, "final reference-only pins")
                runner.append(spec, pins_sha256=digest(canonical(remaining)))
            else:
                runner.append(spec, checkpoint=runner.checkpoint())
        runner.report["state"] = "COMPLETED_SETUP" if setup else "COMPLETED_API_SMOKE" if api_smoke else "COMPLETED_CAPTURE"
    except (Exception, KeyboardInterrupt) as exc:
        runner.report["state"] = "EXECUTION_BLOCKER"
        runner.report["blocker"] = type(exc).__name__ + ": " + str(exc)
    finally:
        sock.close()
        if runner.server is not None and not runner.server_stopped:
            try:
                runner.stop_server()
            except Exception as exc:
                runner.report["cleanup_error"] = str(exc)
                kill_group(runner.server)
        for pid in runner.groups:
            if process_group_exists(pid):
                os.killpg(pid, signal.SIGKILL)
        if (root / "scratch").exists():
            shutil.rmtree(root / "scratch")
        if runner.report["cleanup"] is None:
            runner.report["cleanup"] = {"groups": runner.groups, "all_groups_absent": all(not process_group_exists(p) for p in runner.groups),
                                        "scratch_absent": not (root / "scratch").exists()}
        runner.report["counts"] = {"planned_cases": len(cases_by_name), "captured_cases": len(runner.report["cases"]),
            "matched": sum(c["classification"] == "MATCHED" for c in runner.report["cases"]),
            "unmatched": sum(c["classification"] == "UNMATCHED" for c in runner.report["cases"]),
            "planned_rows": len(freeze["plan"]), "actual_rows": len(runner.report["rows"]),
            "planned_commands": sum(s["row_type"] == "command" for s in freeze["plan"]),
            "actual_commands": sum(r["spec"]["row_type"] == "command" for r in runner.report["rows"]),
            "http_requests": len(runner.requests())}
        runner.report["request_counts"] = {purpose: sum(r["purpose"] == purpose for r in runner.requests())
                                           for purpose in ("platform-setup", "ctx-api")}
        check_postcapture_retained(root, runner.report, freeze)
        runner.persist()
        manifest = archive_inventory(root)
        save_json(root / "SHA256SUMS.json", manifest)
    return runner.report


def validate_capture(report, freeze, *, preparation=False):
    """Offline semantic replay from the independent frozen source/command plan."""
    setup = freeze.get("mode") == "platform-setup"
    api_smoke = freeze.get("mode") == "ctx-smoke"
    equal(report["state"], "COMPLETED_SETUP" if setup else "COMPLETED_API_SMOKE" if api_smoke else "COMPLETED_CAPTURE", "incomplete/blocked capture")
    equal(report["scope"], freeze["scope"], "reference-only scope")
    equal(freeze["case_definitions"], [asdict(c) for c in selected_cases(setup, api_smoke)], "frozen case parameters")
    equal(len(report["rows"]), len(freeze["plan"]), "full chronological coverage")
    plan = freeze["plan"]
    root = Path(plan[0]["cwd"]).parent
    expected_plan = reference_plan(str(root), freeze["reference"], setup=setup, api_smoke=api_smoke)
    # The inherited socket descriptor is bound by the out-of-band freeze, not an
    # archive-reported row. All other command/event templates are code-derived.
    expected_plan[4] = plan[4]
    equal(plan, expected_plan, "trusted complete plan")
    by_name = {c.name: c for c in selected_cases(setup, api_smoke)}
    grouped, errors, classifications, requests, pids = {}, {}, [], [], []
    historical_cases, assessments, legacy_header_rows = [], [], []
    server_ready = None
    for index, (row, spec) in enumerate(zip(report["rows"], plan)):
        equal(spec["ordinal"], index, "freeze chronological index")
        equal(row["spec"], spec, "reordered/rebound row")
        phase, name = spec["phase"], spec["case"]
        validate_lifecycle_row(row, spec, freeze, server_ready)
        if phase == "server-start":
            pids.append(row["pid"])
            server_ready = row["ready"]
        elif spec["row_type"] == "command":
            pids.append(row["pid"])
            health = stage_health(row, freeze["responses"], freeze["origin"], allow_legacy_headers=preparation)
            if health["legacy_dict_only_requests"]:
                legacy_header_rows.append({"ordinal": index, "requests": health["legacy_dict_only_requests"]})
            requests.extend(row["requests"])
            equal(sorted(k for k in ("before", "after") if k in row), ["after", "before"], "command checkpoints missing")
            if phase == "version":
                equal(row["status"], 0, "version command")
                if unblob(row["stdout"]).split(b"\n").count(b"Build label: 9.3.0") != 1:
                    raise ValueError("version label")
            else:
                case = by_name[name]
                validation = (validate_setup_stage(row, spec, root) if setup else validate_stage(row, spec, case,
                    unblob(freeze["fixtures"][name]["extension.bzl"]), root / "scratch/base" / name, freeze["origin"]))
                if not preparation:
                    equal(row["validation"], validation, "reported live validation differs from replay")
                assessments.append({"ordinal": index, "case": name, "phase": phase, "current": validation,
                                    "historical_validation_sha256": digest(canonical(row["validation"]))})
                grouped.setdefault(name, {})[phase] = row
                errors.setdefault(name, []).extend(phase + ": " + e for e in validation["errors"])
                equal(row["artifacts"]["workdir_absent"], True, "callback lifetime cleanup")
                equal(row["artifacts"]["workdir_path"], str(root / "scratch/base" / name / "modextwd/+io"), "cleanup path provenance")
        elif phase == "case-end":
            errors[name].extend(validate_setup_completed(grouped[name]) if setup else validate_completed_case(by_name[name], grouped[name]))
            result = {"name": name, "classification": "UNMATCHED" if errors[name] else "MATCHED", "errors": errors[name]}
            if not preparation:
                equal(row["result"], result, "case verdict")
            equal(row["result"]["name"], name, "historical case identity")
            if row["result"]["classification"] not in ("MATCHED", "UNMATCHED"):
                raise ValueError("historical skipped/unknown case classification")
            historical_cases.append(row["result"])
            classifications.append(result)
        elif phase == "cleanup":
            equal(row["cleanup"], report["cleanup"], "cleanup row")
    equal(report["cases"], historical_cases if preparation else classifications, "full case classifications")
    equal(report["cleanup"], {"groups": pids, "all_groups_absent": True, "scratch_absent": True}, "owned process/resource cleanup")
    equal([r["ordinal"] for r in requests], list(range(len(requests))), "global request chronology")
    counts = {"planned_cases": len(by_name), "captured_cases": len(classifications),
              "matched": sum(c["classification"] == "MATCHED" for c in classifications),
              "unmatched": sum(c["classification"] == "UNMATCHED" for c in classifications),
              "planned_rows": len(plan), "actual_rows": len(report["rows"]),
              "planned_commands": sum(r["row_type"] == "command" for r in plan),
              "actual_commands": sum(r["spec"]["row_type"] == "command" for r in report["rows"]),
              "http_requests": len(requests)}
    historical_counts = {**counts, "matched": sum(c["classification"] == "MATCHED" for c in historical_cases),
                         "unmatched": sum(c["classification"] == "UNMATCHED" for c in historical_cases)}
    equal(report["counts"], historical_counts if preparation else counts, "actual row/command counts")
    if "mode" in freeze:
        equal(report["request_counts"], {purpose: sum(r["purpose"] == purpose for r in requests)
              for purpose in ("platform-setup", "ctx-api")}, "all platform/API requests accounted")
    if preparation:
        return {"state": "PREPARATION_REPLAY_ONLY", "native_acceptance": False, "runtime_acceptance": False,
                "historical_counts": historical_counts, "counts": counts, "cases": classifications, "stages": assessments,
                "historical_header_limitations": {"dict_only_request_rows": legacy_header_rows,
                    "note": "Dictionary-only evidence cannot establish original field multiplicity/order; never full capture proof."}}
    return counts


def replay_reference(root, freeze_sha256, archive_sha256, reference):
    root = Path(root).absolute()
    load_verified_inventory(root, archive_sha256)
    equal(digest((root / "freeze.json").read_bytes()), freeze_sha256, "external freeze pin")
    equal(digest((root / "SHA256SUMS.json").read_bytes()), archive_sha256, "external archive/report pin")
    freeze = load_json((root / "freeze.json").read_bytes())
    report = load_json((root / "report.json").read_bytes())
    equal(report["freeze_sha256"], freeze_sha256, "report freeze reference")
    manifest = load_json((root / "SHA256SUMS.json").read_bytes())
    verify_inventory(root, manifest)
    equal(str(Path(reference).resolve()), freeze["reference"], "current reference executable path")
    equal(file_pin(reference), freeze["pins"][freeze["reference"]], "current reference ELF pin")
    # Deliberately no current native files, Git status, Rust tree or native binary.
    for path, pin in freeze["pins"].items():
        if Path(path).is_relative_to(ROOT) and (Path(path).is_relative_to(FIXTURES) or Path(path) in
                (ROOT / "scripts/validate_module_extension_io.py", ROOT / "tests/test_validate_module_extension_io.py", PLATFORMS)):
            equal(file_pin(path), pin, "current QA/fixture pin")
    bind_retained_bytes(root, report, freeze)
    counts = validate_capture(report, freeze)
    return {"state": "REFERENCE_CAPTURE_REPLAYED", "native_acceptance": False, "counts": counts}


def bind_retained_execution_log(root, row, spec, freeze):
    """Actual planned files, not embedded success records, supply action proof."""
    equal(row["spec"], spec, "retained log trusted invocation")
    original_root = Path(freeze["plan"][0]["cwd"]).parent
    inventory_key(spec["case"])
    relative = Path("raw") / spec["case"] / (spec["phase"] + ".exec.json")
    log = Path(root) / relative
    flags = [arg for arg in spec["argv"] if arg.startswith("--execution_log_json_file=")]
    if spec["phase"] == "cquery":
        equal(flags, [], "cquery has no execution-log command")
        if log.exists() or log.is_symlink():
            raise ValueError("unplanned cquery execution log")
        equal(row["artifacts"]["execlog"], blob(b""), "cquery embedded execution log")
        return
    if spec["phase"] not in ("cold", "warm", "negative"):
        raise ValueError("unknown execution log phase")
    equal(flags, ["--execution_log_json_file=" + str(original_root / relative)], "case/phase-detached execution log path")
    file_pin(log)  # Required regular file, including intentional EMPTY warm/negative logs.
    raw = log.read_bytes()
    equal(blob(raw), row["artifacts"]["execlog"], "retained execution log bytes/hash/size")
    spawns = json_objects(raw)
    equal(spawns, json_objects(unblob(row["artifacts"]["execlog"])), "retained parsed action log")
    if spec["phase"] in ("warm", "negative"):
        equal(spawns, [], "planned nonexecuting phase has spawns")
    elif row["status"] == 0:
        target_at = spec["argv"].index("build") + 1
        target = spec["argv"][target_at]
        equal(target, "//:consume", "fixture action target")
        equal(len(spawns), 1, "retained cold consumer action count")
        outputs = row["artifacts"]["outputs"]
        equal(len(outputs), 1, "cold retained consumer output count")
        output, spawn = outputs[0], spawns[0]
        output_path = output["exec_path"]
        if not re.fullmatch(r"bazel-out/[A-Za-z0-9_-]+/bin/consumed\.json", output_path):
            raise ValueError("retained output identity")
        source_path = "seed.txt" if spec["case"] == SETUP_CASE else "external/+io+made/data.json"
        equal([spawn["mnemonic"], spawn["targetLabel"], spawn["exitCode"], spawn["cacheHit"], spawn["runner"]],
              ["ModuleIOConsumer", target, 0, False, "local"], "retained target/producer/runner/cache proof")
        equal([item["path"] for item in spawn["inputs"]], [source_path], "retained input path")
        equal([item["path"] for item in spawn["actualOutputs"]], [output_path], "retained actual output path")
        equal(spawn["listedOutputs"], [output_path], "retained listed output path")
        for item in (spawn["inputs"][0], spawn["actualOutputs"][0]):
            equal(item["digest"], {"hash": output["data"]["sha256"], "sizeBytes": str(output["data"]["size"]),
                                   "hashFunctionName": "SHA-256"}, "retained input/output file digest")
        equal(spawn["commandArgs"], ["/bin/bash", "-c", '/bin/cat "$1" > "$2"', "", source_path, output_path],
              "retained fixture action arguments")


def bind_retained_bytes(root, report, freeze):
    """Shared LIVE postcapture, normal REPLAY and historical preparation binding."""
    root = Path(root)
    original_root = Path(freeze["plan"][0]["cwd"]).parent
    requests = []
    for index, row in enumerate(report["rows"]):
        spec = freeze["plan"][index]
        equal(row["spec"], spec, "retained trusted chronological row")
        if spec["phase"] == "server-stop":
            for stream in ("stdout", "stderr"):
                equal(blob((root / ("server." + stream + ".raw")).read_bytes()), row["server"][stream], "retained server lifecycle stream")
        if spec["row_type"] != "command" or spec["phase"] == "server-start":
            continue
        relative = Path("commands") / f'{spec["ordinal"]:04d}'
        equal(row["raw_directory"], str(original_root / relative), "historical raw provenance")
        for stream in ("stdout", "stderr"):
            equal(blob((root / relative / (stream + ".raw")).read_bytes()), row[stream], "historical raw stream binding")
        requests.extend(row["requests"])
        if spec["case"]:
            bind_retained_execution_log(root, row, spec, freeze)
    request_file = root / "requests.jsonl"
    retained = [load_json(line) for line in request_file.read_bytes().split(b"\n") if line] if request_file.exists() else []
    equal(requests, retained, "historical ordered request log")
    for case, files in freeze["fixtures"].items():
        inventory_key(case)
        for name, content in files.items():
            inventory_key(name)
            path = root / "inputs/workspaces" / case / name
            equal(blob(path.read_bytes()), content, "historical source snapshot bytes")


def preparation_replay(root, freeze_sha256, archive_sha256, output):
    """Offline review only: copy evidence unchanged, assess new predicates separately."""
    root, output = Path(root).absolute(), Path(output).absolute()
    if (output.parent != Path("/tmp/opencode") or not output.name.startswith("module-extension-io-profile-review-")
            or output.exists() or output == root):
        raise ValueError("fresh owned profile-review artifact directory required")
    archive_inventory(root)  # Path/type guard before reading any external proof.
    equal(digest((root / "freeze.json").read_bytes()), freeze_sha256, "historical freeze pin")
    equal(digest((root / "SHA256SUMS.json").read_bytes()), archive_sha256, "historical archive pin")
    freeze = load_json((root / "freeze.json").read_bytes())
    report = load_json((root / "report.json").read_bytes())
    equal(report["freeze_sha256"], freeze_sha256, "historical report/freeze binding")
    legacy = historical_inventory(root, freeze, load_json((root / "SHA256SUMS.json").read_bytes()))
    bind_retained_bytes(root, report, freeze)
    provenance = load_json((FIXTURES / "negative_build/provenance.json").read_bytes())
    equal([str(root), freeze_sha256, archive_sha256, digest((root / "report.json").read_bytes())],
          [provenance["capture_root"], provenance["freeze_sha256"], provenance["archive_sha256"], provenance["report_sha256"]],
          "authorized immutable preparation input")
    qa_paths = [ROOT / "scripts/validate_module_extension_io.py", ROOT / "tests/test_validate_module_extension_io.py",
                PLATFORMS, *[p for p in files_under(FIXTURES) if "__pycache__" not in p.parts and p.suffix != ".pyc"]]
    closure_pin = load_json((FIXTURES / "platform_closure.json").read_bytes())
    registry = ROOT / closure_pin["registry_relative_root"]
    qa_paths += [registry / "modules/platforms/1.0.0" / name for name in ("MODULE.bazel", "source.json")]
    qa_pins = {str(p): file_pin(p) for p in qa_paths}
    assessment = validate_capture(report, freeze, preparation=True)
    bindings = []
    by_name = {c.name: c for c in cases()}
    for expected in provenance["rows"]:
        index, name = expected["ordinal"], expected["case"]
        row, spec = report["rows"][index], freeze["plan"][index]
        equal([spec["case"], spec["phase"], row["status"]], [name, "negative", expected["status"]], "nine genuine row identities")
        source = unblob(freeze["fixtures"][name]["extension.bzl"])
        equal([len(source), digest(source)], [expected["source_size"], expected["source_sha256"]], "captured source pin")
        stderr = unblob(row["stderr"])
        equal([len(stderr), digest(stderr)], [expected["stderr_size"], expected["stderr_sha256"]], "captured stderr pin")
        equal(stderr.hex(), (FIXTURES / "negative_build" / (name + ".stderr.raw")).read_bytes().hex(), "portable raw proof copy")
        equal([row["stdout"]["size"], row["stdout"]["sha256"]], [provenance["stdout_size"], provenance["stdout_sha256"]], "captured stdout pin")
        bindings.append({"ordinal": index, "case": name, "trusted_invocation": spec,
            "profile": negative_profile(spec, by_name[name], source, freeze["origin"]),
            "operation": by_name[name].operations[-1], "source_snapshot": "captured/inputs/workspaces/" + name + "/extension.bzl",
            "source_sha256": digest(source), "source_size": len(source),
            "streams": {key: {"path": f"captured/commands/{index:04d}/{key}.raw", "sha256": row[key]["sha256"], "size": row[key]["size"]}
                        for key in ("stdout", "stderr")},
            "request_sequence_sha256": digest(canonical(row["requests"])),
            "historical_validation_sha256": digest(canonical(row["validation"]))})
    equal(len(bindings), 9, "complete genuine negative proof coverage")
    output.mkdir(mode=0o700)
    shutil.copytree(root, output / "captured", copy_function=shutil.copy2)
    equal(archive_inventory(output / "captured"), legacy["complete_inventory"], "unchanged historical evidence copy")
    equal(file_pin(output / "captured/SHA256SUMS.json"), file_pin(root / "SHA256SUMS.json"), "copied original top manifest")
    for path in qa_paths:
        destination = output / "current-qa" / path.relative_to(ROOT)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, destination)
    assessment.update(input_freeze_sha256=freeze_sha256, input_archive_sha256=archive_sha256,
                      input_report_sha256=provenance["report_sha256"],
                      binding_scope="CURRENT OWNED QA/fixtures only; historical reference proof; native and legacy validator source NOT bound")
    assessment["historical_instrumentation_limitations"] = [
        "Historical server logged GET only; its retained log cannot retroactively prove absence of non-GET requests.",
        "Current dispatch-boundary method auditing requires a newly authorized run; this receipt is preparation only."]
    save_json(output / "assessment.json", assessment)
    save_json(output / "negative_bindings.json", {"state": "PREPARATION_REPLAY_ONLY", "rows": bindings})
    save_json(output / "historical_inventory_audit.json", legacy)
    equal({str(p): file_pin(p) for p in qa_paths}, qa_pins, "current verifier QA changed during preparation")
    equal(archive_inventory(root), legacy["complete_inventory"], "historical capture changed during preparation")
    equal(digest((root / "SHA256SUMS.json").read_bytes()), archive_sha256, "historical top manifest changed")
    save_json(output / "qa_binding.json", {"before": qa_pins, "after": qa_pins,
        "reference_executions": 0, "native_executions": 0, "cleanup": "no runtime scratch/server/process created"})
    manifest = archive_inventory(output)
    save_json(output / "SHA256SUMS.json", manifest)
    verify_inventory(output, manifest)
    return {"state": "PREPARATION_REPLAY_ONLY", "historical_counts": assessment["historical_counts"],
            "current_predicate_counts": assessment["counts"], "negative_bindings": len(bindings),
            "inventory_files": len(manifest), "manifest_sha256": digest((output / "SHA256SUMS.json").read_bytes()),
            "native_acceptance": False, "runtime_acceptance": False}


def require_setup_receipt(root, freeze_sha256, archive_sha256, reference):
    if not root or not freeze_sha256 or not archive_sha256:
        raise ValueError("a separately completed, pinned setup smoke is mandatory before the API matrix")
    result = replay_reference(root, freeze_sha256, archive_sha256, reference)
    freeze = load_json((Path(root) / "freeze.json").read_bytes())
    equal(freeze.get("mode"), "platform-setup", "receipt is not setup proof")
    equal([result["counts"][k] for k in ("planned_cases", "captured_cases", "matched", "unmatched", "http_requests")],
          [1, 1, 1, 0, 0], "setup must match entirely with zero HTTP before API capture")
    return {"path": str(Path(root).absolute()), "freeze_sha256": freeze_sha256, "archive_sha256": archive_sha256,
            "report_sha256": digest((Path(root) / "report.json").read_bytes())}


def require_api_smoke_receipt(root, freeze_sha256, archive_sha256, reference):
    if not root or not freeze_sha256 or not archive_sha256:
        raise ValueError("a separate pinned enabled download/read smoke is mandatory before the API matrix")
    result = replay_reference(root, freeze_sha256, archive_sha256, reference)
    freeze = load_json((Path(root) / "freeze.json").read_bytes())
    equal(freeze.get("mode"), "ctx-smoke", "receipt is not enabled API smoke")
    equal([result["counts"][k] for k in ("planned_cases", "captured_cases", "matched", "unmatched")],
          [1, 1, 1, 0], "enabled API smoke must entirely match before matrix")
    if result["counts"]["http_requests"] < 1:
        raise ValueError("enabled API smoke did not reach real HTTP download")
    for spec in freeze["plan"]:
        if "--repository_disable_download" in spec["argv"]:
            raise ValueError("enabled API smoke inherited disabled policy")
    return {"path": str(Path(root).absolute()), "freeze_sha256": freeze_sha256, "archive_sha256": archive_sha256,
            "report_sha256": digest((Path(root) / "report.json").read_bytes())}


def native_collector():
    name = "module_extension_io_native_collector"
    if name not in sys.modules:
        spec = importlib.util.spec_from_file_location(name, FIXTURES / "native_capture.py")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    module = sys.modules[name]
    module.configure(sys.modules[__name__])
    return module


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--plan", action="store_true")
    mode.add_argument("--reference-only", action="store_true")
    mode.add_argument("--setup-only", action="store_true", help="one disabled-download genuine file/action build+cquery smoke")
    mode.add_argument("--api-smoke-only", action="store_true", help="one enabled real module_ctx download/read-to-consumer smoke")
    mode.add_argument("--replay-reference", action="store_true")
    mode.add_argument("--preparation-replay", action="store_true", help="offline predicate review of immutable historical evidence; never runtime acceptance")
    mode.add_argument("--native", help="pinned ELF for native-only diagnostic capture; never starts a reference/paired run")
    mode.add_argument("--native-replay", action="store_true")
    mode.add_argument("--native-preparation-replay", action="store_true")
    mode.add_argument("--paired-replay", action="store_true", help="strict current-source paired evidence replay only; no execution")
    parser.add_argument("--native-sha256")
    parser.add_argument("--native-path")
    parser.add_argument("--runtime-manifest")
    parser.add_argument("--runtime-manifest-sha256")
    parser.add_argument("--reference-artifact")
    parser.add_argument("--reference-freeze-sha256")
    parser.add_argument("--reference-archive-sha256")
    parser.add_argument("--reference")
    parser.add_argument("--reference-sha256")
    parser.add_argument("--registry-input")
    parser.add_argument("--official-source")
    parser.add_argument("--official-tree")
    parser.add_argument("--artifact-dir")
    parser.add_argument("--output-dir")
    parser.add_argument("--freeze-sha256")
    parser.add_argument("--archive-sha256")
    parser.add_argument("--setup-receipt")
    parser.add_argument("--setup-freeze-sha256")
    parser.add_argument("--setup-archive-sha256")
    parser.add_argument("--api-smoke-receipt")
    parser.add_argument("--api-smoke-freeze-sha256")
    parser.add_argument("--api-smoke-archive-sha256")
    parser.add_argument("--timeout", type=int, default=90)
    args = parser.parse_args(argv)
    native_mode = args.native or args.native_replay or args.native_preparation_replay or args.paired_replay
    if args.native_sha256 and not native_mode:
        parser.error("--native-sha256 requires an explicit native diagnostic/replay mode")
    if native_mode:
        required = ["native_sha256", "runtime_manifest", "runtime_manifest_sha256", "artifact_dir"]
        required += ["registry_input"] if args.native else ["native_path", "freeze_sha256", "archive_sha256"]
        if args.paired_replay:
            required += ["reference", "reference_artifact", "reference_freeze_sha256", "reference_archive_sha256"]
        for name in required:
            if not getattr(args, name):
                parser.error("--" + name.replace("_", "-") + " is required for native collection/replay")
        if not 1 <= args.timeout <= 120:
            parser.error("timeout must be 1..120 seconds")
        collector = native_collector()
        if args.native:
            args.native = str(Path(args.native).resolve(strict=True))
            result = collector.capture(args)
            result = {key: result[key] for key in ("state", "counts", "cleanup", "blocker", "freeze_sha256", "uncompleted_cases")}
        elif args.paired_replay:
            result = collector.paired_replay(args)
        else:
            result = collector.replay(args, preparation=args.native_preparation_replay)
        print(json.dumps(result, sort_keys=True, indent=2))
        return 2 if result["state"].endswith("BLOCKER") else 0
    if args.plan:
        result = preparation_plan()
    elif args.preparation_replay:
        for name in ("artifact_dir", "freeze_sha256", "archive_sha256", "output_dir"):
            if not getattr(args, name):
                parser.error("--" + name.replace("_", "-") + " is required")
        result = preparation_replay(args.artifact_dir, args.freeze_sha256, args.archive_sha256, args.output_dir)
    elif args.reference_only or args.setup_only or args.api_smoke_only:
        for name in ("reference", "reference_sha256", "registry_input", "official_source", "official_tree", "artifact_dir"):
            if not getattr(args, name):
                parser.error("--" + name.replace("_", "-") + " is required")
        if not 1 <= args.timeout <= 120:
            parser.error("timeout must be 1..120 seconds")
        if (args.reference_only or args.api_smoke_only) and not all((args.setup_receipt, args.setup_freeze_sha256, args.setup_archive_sha256)):
            parser.error("reference API capture requires --setup-receipt/--setup-freeze-sha256/--setup-archive-sha256")
        if args.reference_only and not all((args.api_smoke_receipt, args.api_smoke_freeze_sha256, args.api_smoke_archive_sha256)):
            parser.error("full capture requires --api-smoke-receipt/--api-smoke-freeze-sha256/--api-smoke-archive-sha256")
        result = capture_reference(args)
        result = {k: result[k] for k in ("state", "counts", "cleanup", "blocker", "freeze_sha256")}
    else:
        for name in ("reference", "artifact_dir", "freeze_sha256", "archive_sha256"):
            if not getattr(args, name):
                parser.error("--" + name.replace("_", "-") + " is required")
        result = replay_reference(args.artifact_dir, args.freeze_sha256, args.archive_sha256, args.reference)
    print(json.dumps(result, sort_keys=True, indent=2))
    return 2 if result.get("state") == "EXECUTION_BLOCKER" else 0


if __name__ == "__main__":
    sys.exit(main())
