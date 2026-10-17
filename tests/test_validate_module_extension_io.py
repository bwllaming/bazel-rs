"""Portable preparation tests. Synthetic transcripts are NOT CLI evidence."""

import base64
import copy
import contextlib
import http.client
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from types import SimpleNamespace


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("module_io_preparation", ROOT / "scripts/validate_module_extension_io.py")
qa = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = qa
SPEC.loader.exec_module(qa)


def synthetic_record(ordinal, phase):
    """An oracle for validator tests only, never returned as a host result."""
    events = [] if phase != "negative" else [
        {"event": "begin", "case": "unsupported-headers"},
        {"event": "pre", "case": "unsupported-headers", "index": 0,
         "operation": {"method": "download", "args": ["/versions"],
                       "kwargs": {"headers": {"X-Fixture": "1"}, "allow_fail": True}}}]
    frames = b"".join(qa.MARKER + qa.canonical(event) + b"\n" for event in events)
    return {
        "invocation": {"ordinal": ordinal, "phase": phase, "case": "synthetic",
                       "argv": ["/owned/pinned-executable", "build", "//:consume"],
                       "cwd": "/owned/workspace", "env": qa.environment("/owned")},
        "profile": "native", "status": 1 if phase == "negative" else 0,
        "health": {"timeout": False, "signal": None, "spawn_error": None, "truncated": False},
        "stdout": qa.blob(b""),
        "stderr": qa.blob(b"fixture-only exact negative at //:extension.bzl:9\n" if phase == "negative" else b""),
        "callback_frames": qa.blob(frames), "events": events, "requests": [],
        "pins": {"executable": "test-executable-pin", "source": "test-source-pin"},
        "publication": [], "observations": {"source_path": "/owned/workspace/extension.bzl",
            "source_sha256": qa.digest(b"synthetic input, not Starlark evidence"),
            "cache_hits": 0, "actions_executed": 0, "action_inputs": [], "action_outputs": [],
            "cquery": qa.blob(b""), "consumer": None,
            "tree": [qa.manifest_entry("", "directory", "0755")],
            "observation_ref": ordinal},
    }


class FixtureTests(unittest.TestCase):
    def test_exact_portable_input_and_receipt_shapes(self):
        p = qa.payloads()
        contract = qa.load_json((qa.FIXTURES / "contract.json").read_bytes())
        self.assertEqual(len(p["versions"]), 620)
        self.assertEqual(qa.digest(p["old"]), contract["owned_seed"]["sha256"])
        for receipt in contract["receipts"]:
            for key in ("sha256", "manifest_sha256"):
                self.assertRegex(receipt[key], r"^[a-f0-9]{64}$")
        self.assertEqual([r["accepted"] for r in contract["receipts"]], [10, 15, 3, 15])

    def test_executed_boundaries_not_atomic_assumption(self):
        p = qa.payloads()
        contract = qa.load_json((qa.FIXTURES / "contract.json").read_bytes())
        for case in contract["checksum_boundary"]:
            expected = p["old"] if case["retained"] == "old" else p[str(case["served"])]
            self.assertEqual([len(expected), qa.digest(expected)], [case["size"], case["sha256"]])
        truncation = contract["truncation_failed_expectation"]
        self.assertEqual(qa.digest(p["40000"][:33000]), truncation["sha256"])
        self.assertFalse(truncation["retry_proven"])

    def test_tracked_platform_archive_no_historical_path_dependency(self):
        self.assertEqual(qa.digest(qa.platforms_archive()), qa.PLATFORM_SHA256)

    def test_plan_counts_and_complete_chronology_derived(self):
        plan = qa.preparation_plan()
        rows = plan["chronology"]
        self.assertEqual(plan["counts"], {"cli_candidate_cases": len(qa.cases()),
                         "chronology_rows": len(rows),
                         "reference_commands": sum(bool(r["argv"]) for r in rows)})
        self.assertEqual([r["ordinal"] for r in rows], list(range(len(rows))))
        self.assertEqual(rows[0]["phase"], "pins-start")
        self.assertEqual(rows[-1]["phase"], "pins-end")
        self.assertEqual([r["phase"] for r in rows[:6]],
                         ["pins-start", "public-input-snapshot", "registry-local-copy", "distdir-pin", "server-start", "version"])
        for case in qa.cases():
            phases = [r["phase"] for r in rows if r["case"] == case.name]
            expected = ["case-start", "negative", "negative-observation", "case-end"] if case.result == "callback-error" else [
                "case-start", "cold", "cold-observation", "warm", "warm-observation", "cquery", "consumer-observation", "case-end"]
            self.assertEqual(phases, expected)
        self.assertFalse(plan["native_acceptance"])

    def test_real_callback_to_repository_to_consumer(self):
        for case in qa.cases():
            files = qa.render(case)
            text = files["extension.bzl"].decode()
            self.assertEqual(text.count("    value = ctx."), len(case.operations))
            self.assertEqual(text.count('"event": "pre"'), len(case.operations))
            self.assertIn('observed_repository(name = "made", observed = json.encode(observations))', text)
            self.assertNotIn("ctx.path(", text)
            self.assertNotIn("ctx.execute(", text)
            self.assertIn('ctx.attr.observed', files["repository.bzl"].decode())
            self.assertIn('src = "@observed//:data.json"', files["BUILD.bazel"].decode())
            self.assertIn('inputs = [ctx.file.src]', files["consumer.bzl"].decode())
        text = qa.render(qa.cases()[0])["extension.bzl"].decode()
        self.assertIn('versions = json.decode(observations[-1])', text)
        self.assertIn('entry["kind"] == "archive"', text)

    def test_bad_origins_are_never_rendered(self):
        for origin in ("https://example.com", "http://localhost:8080", "http://127.0.0.1:65536", "http://127.0.0.1:1/x"):
            with self.subTest(origin=origin), self.assertRaises(ValueError):
                qa.render(qa.cases()[0], origin)

    def test_exact_typed_recipes_and_all_five_sri(self):
        cases = {c.name: c for c in qa.cases()}
        sri = [c for c in cases if c.startswith("sri-")]
        self.assertEqual(sri, ["sri-" + a for a in ("sha1", "sha256", "sha384", "sha512", "blake3")])
        self.assertEqual(cases["http-filter-true"].requests, ())
        self.assertIs(cases["http-filter-true"].operations[0]["kwargs"]["allow_fail"], True)
        self.assertEqual(cases["mirrors-404-success"].requests, ("/404", "/versions?mode=json&include=all"))
        self.assertEqual(cases["empty-list-existing-false"].result, "callback-error")
        self.assertEqual(cases["empty-list-existing-true"].result, "success")

    def test_disable_is_only_intended_commands(self):
        for row in qa.reference_plan():
            if "--repository_disable_download" in row["argv"]:
                self.assertIn(row["case"], ("disabled-miss-false", "disabled-miss-true", "malformed-sha-disabled"))
            self.assertEqual(set(row["env"]), {"HOME", "TMPDIR", "PATH", "LC_ALL"})

    def test_response_bytes_requests_and_suffixes(self):
        routes = qa.response_plan()
        for case in qa.cases():
            for request in case.requests:
                self.assertIn(request, routes)
        for path in ("/gzip", "/x-gzip", "/compressed-bare", "/compressed.tgz"):
            self.assertEqual(gzip.decompress(qa.unblob(routes[path]["body"])), qa.payloads()["versions"])
        self.assertEqual(routes["/original.gz"]["headers"]["Location"], "/compressed-bare")
        self.assertEqual(routes["/to-final-suffix"]["headers"]["Location"], "/compressed.tgz")

    def test_latin1_raw_versus_utf8_logical_string(self):
        logical = bytes(range(256)).decode("latin-1")
        json_text = json.dumps({"value": logical}, ensure_ascii=False)
        reference = qa.MARKER + json_text.encode("latin-1") + b"\n"
        native = qa.MARKER + json_text.encode("utf-8") + b"\n"
        self.assertNotEqual(reference, native)
        for raw, profile in ((reference, "reference"), (native, "native")):
            self.assertEqual(qa.logical_bytes(qa.callback_events(raw, profile)[0]["value"]), bytes(range(256)))
        with self.assertRaises(UnicodeDecodeError):
            qa.callback_events(reference, "native")
        with self.assertRaises(ValueError):
            qa.callback_events(b"untrusted prefix " + reference, "reference")
        with self.assertRaises(UnicodeEncodeError):
            qa.logical_bytes("\ufffd")

    def test_result_fields_and_root_modes_distinct(self):
        self.assertEqual(set(qa.result_fields(b"", "sha512")), {"success", "integrity", "size_bytes"})
        self.assertEqual(set(qa.result_fields(b"")), {"success", "integrity", "size_bytes", "sha256"})
        root = qa.manifest_entry("", "directory", "0755")
        self.assertEqual(root, qa.load_json((qa.FIXTURES / "contract.json").read_bytes())["root_manifest"])
        self.assertNotEqual(qa.manifest_entry("data", "file", "0555", b"x"), qa.manifest_entry("data", "file", "0640", b"x"))

    def test_json_duplicate_nonfinite_and_bool_integer_rejected(self):
        for raw in ('{"x": 1, "x": 2}', '{"x": NaN}'):
            with self.assertRaises(ValueError):
                qa.load_json(raw)
        with self.assertRaises(ValueError):
            qa.equal({"allow_fail": 1}, {"allow_fail": True}, "type")


class ReplayTests(unittest.TestCase):
    def setUp(self):
        self.trusted = {"classification": "preparation-scaffold", "pins": {"fixture": "synthetic-only"},
                        "rows": [synthetic_record(i, phase) for i, phase in enumerate(("pins-start", "version", "negative", "pins-end"))]}
        self.pin = qa.digest(qa.canonical(self.trusted))
        self.report = copy.deepcopy(self.trusted)

    def replay(self):
        return qa.replay(self.report, self.trusted, self.pin, self.trusted["pins"])

    def test_valid_synthetic_not_acceptance(self):
        self.assertEqual(self.replay(), "SCAFFOLD_REPLAY_VALIDATED_NOT_ACCEPTANCE")

    def test_live_and_replay_both_reject_valid_rehashed_wrong_proofs(self):
        def mutate_events(row, event):
            row["events"].append(event)
            row["callback_frames"] = qa.blob(b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in row["events"]))

        mutations = {
            "fallback": lambda r: mutate_events(r, {"event": "post", "index": 0, "value": {"success": False}}),
            "callback-complete": lambda r: mutate_events(r, {"event": "declared", "name": "made"}),
            "zero-action-cache": lambda r: r.update(stdout=qa.blob(b"0 actions executed, 1 cache hits\n")),
            "all-action-cache": lambda r: r.update(stdout=qa.blob(b"All actions retrieved from cache\n")),
            "footer-only": lambda r: r.update(stderr=qa.blob(b"unsupported feature\n")),
            "arbitrary-footer": lambda r: r.update(stderr=qa.blob(qa.unblob(r["stderr"]) + b"unsupported extra\n")),
            "wrong-global": lambda r: r.update(stderr=qa.blob(b"repository_ctx.download unsupported\n")),
            "wrong-phase": lambda r: r["invocation"].update(phase="cquery"),
            "wrong-keyword": lambda r: r["events"][1]["operation"]["kwargs"].update(allow_failure=True),
            "wrong-type": lambda r: r["events"][1]["operation"]["kwargs"].update(allow_fail=1),
            "request": lambda r: r["requests"].append({"path": "/never", "status": 200}),
            "publication": lambda r: r["publication"].append("made"),
            "cache-count": lambda r: r["observations"].update(cache_hits=1),
            "same-basename": lambda r: r["observations"].update(source_path="/wrong/extension.bzl"),
            "execlog-path": lambda r: r["observations"].update(action_inputs=["/wrong/data.json"]),
            "output-path": lambda r: r["observations"].update(action_outputs=["/wrong/consumed.json"]),
            "cquery": lambda r: r["observations"].update(cquery=qa.blob(b"/wrong/consumed.json\n")),
            "root-omitted": lambda r: r["observations"].update(tree=[]),
            "root-mode": lambda r: r["observations"]["tree"][0].update(mode="0555"),
            "root-size": lambda r: r["observations"]["tree"][0].update(size=4096),
            "env-proxy": lambda r: r["invocation"]["env"].update(HTTPS_PROXY="http://proxy.invalid"),
            "exit-only": lambda r: r.update(stderr=qa.blob(b""), callback_frames=qa.blob(b""), events=[]),
        }
        for name, mutate in mutations.items():
            with self.subTest(mutation=name):
                row = copy.deepcopy(self.trusted["rows"][2])
                mutate(row)
                if name in ("wrong-keyword", "wrong-type"):
                    row["callback_frames"] = qa.blob(b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in row["events"]))
                # Every altered raw blob has a VALID repaired digest and length.
                for stream in ("stdout", "stderr", "callback_frames"):
                    qa.unblob(row[stream])
                with self.assertRaises(ValueError):
                    qa.verify_record(row, self.trusted["rows"][2])
                self.report = copy.deepcopy(self.trusted)
                self.report["rows"][2] = row
                with self.assertRaises(ValueError):
                    self.replay()

    def test_reordered_rows_repaired_ids_and_observation_refs(self):
        self.report["rows"][1:3] = reversed(self.report["rows"][1:3])
        for i, row in enumerate(self.report["rows"]):
            row["invocation"]["ordinal"] = i
            row["observations"]["observation_ref"] = i
        with self.assertRaises(ValueError):
            self.replay()

    def test_incomplete_extra_and_skipped_coverage(self):
        for replacement in (self.report["rows"][:-1], self.report["rows"] + [self.report["rows"][-1]]):
            self.report = {**self.trusted, "rows": replacement}
            with self.assertRaises(ValueError):
                self.replay()


def synthetic_stage(case, phase):
    """Collector predicate inputs only: not a fake API, subprocess or CLI result."""
    root = Path("/MODULE_IO_OWNED")
    origin = "http://127.0.0.1:18080"
    spec = next(r for r in qa.reference_plan() if r["case"] == case.name and r["phase"] == phase)
    source = qa.render(case, origin)["extension.bzl"]
    sites = [n for n, line in enumerate(source.decode().splitlines(), 1)
             if line.startswith('    print("MODULE_IO_JSON " + json.encode(')]
    events = qa.expected_events(case)
    stderr = b"".join(f'DEBUG: {spec["cwd"]}/extension.bzl:{line}:10: '.encode() + qa.MARKER + qa.canonical(event) + b"\n"
                      for line, event in zip(sites, events))
    base = root / "scratch/base" / case.name
    execroot = base / "execroot/_main"
    source_path, output_path = "external/+io+made/data.json", "bazel-out/k8-fastbuild/bin/consumed.json"
    stdout = (output_path + "\n" + source_path + "\n").encode() if phase == "cquery" else b""
    artifacts = {"repository": None, "outputs": [], "execlog": qa.blob(b""), "execroot": str(execroot),
                 "workdir_absent": True, "workdir_path": str(base / "modextwd/+io")}
    if case.result == "callback-error":
        line, statement = [(n, s) for n, s in enumerate(source.decode().splitlines(), 1) if s.startswith("    value = ctx.")][-1]
        column = statement.index("(") + 1
        path = Path(spec["cwd"]) / "extension.bzl"
        profile = qa.negative_profile(spec, case, source, origin)
        if case.name == "allfail-false":
            stderr += b"".join(("WARNING: Download from " + origin + request + " failed: class java.io.FileNotFoundException GET returned 404 Not Found\n").encode()
                               for request in case.requests)
        stderr += (f'ERROR: {path}:{line}:{column}: Traceback (most recent call last):\n'
                   f'\tFile "{path}", line {line}, column {column}, in _impl\n\t\t{statement.strip()}\n'
                   + qa.negative_diagnostic(case, base, origin) + "\n"
                   + profile["terminal"] + "\nINFO: Elapsed time: 1.000s, Critical Path: 0.02s\n"
                   + "INFO: 1 process: 1 internal.\nERROR: Build did NOT complete successfully\n").encode()
    else:
        data = json.dumps(qa.expected_consumer(case), ensure_ascii=False, separators=(",", ":")).encode("latin-1")
        artifacts["repository"] = {"path": str(base / "external/+io+made"), "resolved_path": str(base / "external/+io+made"),
            "exec_path": source_path, "data": qa.blob(data), "data_mode": "0644", "root": qa.manifest_entry("", "directory", "0755")}
        artifacts["outputs"] = [{"path": str(execroot / output_path), "resolved_path": str(execroot / output_path),
            "exec_path": output_path, "mode": "0555", "data": qa.blob(data)}]
        if phase == "cold":
            artifacts["execlog"] = qa.blob(qa.canonical({"mnemonic": "ModuleIOConsumer", "exitCode": 0, "cacheHit": False,
                "inputs": [{"path": source_path, "digest": {"hash": qa.digest(data)}}],
                "actualOutputs": [{"path": output_path, "digest": {"hash": qa.digest(data)}}],
                "commandArgs": ["/bin/bash", "-c", '/bin/cat "$1" > "$2"', "", source_path, output_path]}))
    routes = qa.response_plan()
    requests = [{"ordinal": i, "method": "GET", "path": path, "peer": "127.0.0.1", "unsafe": False,
                 "headers": {"Host": "127.0.0.1:18080"},
                 "header_items": [["Host", "127.0.0.1:18080"]],
                 "status": routes[path]["status"], "response_headers": routes[path]["headers"],
                 "body_sha256": routes[path]["body"]["sha256"], "body_size": routes[path]["body"]["size"]}
                for i, path in enumerate(case.requests)]
    row = {"spec": spec, "status": 1 if case.result == "callback-error" else 0,
           "health": {"timeout": False, "signal": None, "spawn_error": None, "truncated": False, "group_absent": True},
           "stdout": qa.blob(stdout), "stderr": qa.blob(stderr), "requests": requests, "artifacts": artifacts}
    return row, source, base, origin


class LiveStageTests(unittest.TestCase):
    def validate(self, row, case, source, base, origin):
        spec = next(r for r in qa.reference_plan() if r["case"] == case.name and r["phase"] == row["spec"]["phase"])
        return qa.validate_stage(row, spec, case, source, base, origin)["errors"]

    def test_all_planned_predicates_have_exact_oracles(self):
        for case in qa.cases():
            phase = "negative" if case.result == "callback-error" else "cold"
            row, source, base, origin = synthetic_stage(case, phase)
            with self.subTest(case=case.name):
                qa.stage_health(row, qa.response_plan(), origin)
                self.assertEqual(self.validate(row, case, source, base, origin), [])

    def test_negative_source_echo_footer_fallback_and_wrong_global(self):
        case = next(c for c in qa.cases() if c.name == "http-filter-false")
        original, source, base, origin = synthetic_stage(case, "negative")
        mutations = [
            lambda raw: raw.replace(b"Error in download:", b"Error in repository_ctx.download:"),
            lambda raw: raw + b"unsupported arbitrary terminal footer\n",
            lambda raw: raw + b"0 actions executed, 1 cache hits\n",
            lambda raw: raw + b"All actions retrieved from cache\n",
            lambda raw: raw.replace(b"DEBUG: ", b"SOURCE ECHO: ", 1),
            lambda raw: raw.replace(b"extension.bzl:4:10", b"extension.bzl:99:10", 1),
            lambda raw: raw.replace(b'"event":"pre"', b'"event":"post"'),
            lambda raw: raw.replace(b'"output":"metadata.json"', b'"output":1'),
        ]
        for mutate in mutations:
            row = copy.deepcopy(original)
            row["stderr"] = qa.blob(mutate(qa.unblob(row["stderr"])))
            qa.unblob(row["stderr"])
            self.assertTrue(self.validate(row, case, source, base, origin))
        row = copy.deepcopy(original)
        row["stdout"] = qa.blob(b"struct(success=False)\n")
        self.assertTrue(self.validate(row, case, source, base, origin))

    def test_positive_actual_paths_modes_and_cache_required(self):
        case = qa.cases()[0]
        records = {phase: synthetic_stage(case, phase)[0] for phase in ("cold", "warm", "cquery")}
        self.assertEqual(qa.validate_completed_case(case, records), [])
        mutations = [
            lambda r: r["cold"]["artifacts"]["outputs"][0].update(path="/elsewhere/consumed.json"),
            lambda r: r["warm"]["artifacts"].update(execlog=r["cold"]["artifacts"]["execlog"]),
            lambda r: r["cquery"].update(stdout=qa.blob(b"/elsewhere/consumed.json\nexternal/+io+made/data.json\n")),
            lambda r: r["cold"]["artifacts"]["repository"].update(resolved_path="/outside/data.json"),
        ]
        for mutate in mutations:
            bad = copy.deepcopy(records)
            mutate(bad)
            self.assertTrue(qa.validate_completed_case(case, bad))
        row, source, base, origin = synthetic_stage(case, "cold")
        row["artifacts"]["outputs"][0]["mode"] = "0644"
        self.assertTrue(self.validate(row, case, source, base, origin))
        row, source, base, origin = synthetic_stage(case, "cold")
        row["artifacts"]["repository"]["root"]["size"] = 4096
        self.assertTrue(self.validate(row, case, source, base, origin))

    def test_engine_cycle_source_and_health_binding(self):
        case = qa.cases()[0]
        original, source, base, origin = synthetic_stage(case, "cold")
        for key, value in (("engine", "native"), ("cycle", 1), ("ordinal", 999)):
            row = copy.deepcopy(original)
            row["spec"][key] = value
            self.assertTrue(self.validate(row, case, source, base, origin))
        for key, value in (("timeout", True), ("group_absent", False), ("truncated", True)):
            row = copy.deepcopy(original)
            row["health"][key] = value
            with self.assertRaises(ValueError):
                qa.stage_health(row, qa.response_plan(), origin)
        with self.assertRaises(ValueError):
            qa.reference_frames(qa.unblob(original["stderr"]), Path("/wrong/extension.bzl"), source)

    def test_native_interface_has_zero_effect_gate(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
            qa.main(["--native", "/must/not/run", "--native-sha256", "0" * 64])
        self.assertEqual(caught.exception.code, 2)

    def test_owned_server_real_lifecycle_not_bazel_evidence(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-unit-") as directory:
            root = Path(directory)
            for name in ("home", "tmp"):
                (root / name).mkdir()
            qa.save_json(root / "routes.json", qa.response_plan())
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            sock.listen(4)
            address = sock.getsockname()
            process = subprocess.Popen([sys.executable, "-I", str(qa.FIXTURES / "http_server.py"), str(sock.fileno()),
                str(root / "routes.json"), str(root / "requests.jsonl")], cwd=root, env=qa.environment(str(root)),
                pass_fds=(sock.fileno(),), start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ)
                    self.assertTrue(selector.select(timeout=5))
                    ready = process.stdout.readline()
                self.assertEqual(json.loads(ready), {"ready": True, "address": list(address)})
                sock.close()
                connection = http.client.HTTPConnection(*address, timeout=5)
                connection.request("GET", "/versions?mode=json&include=all")
                response = connection.getresponse()
                self.assertEqual((response.status, response.read()), (200, qa.payloads()["versions"]))
                connection.close()
                os.killpg(process.pid, signal.SIGTERM)
                stdout, stderr = process.communicate(timeout=5)
                self.assertEqual((process.returncode, stdout, stderr), (0, b"SERVER_CLOSED\n", b""))
                self.assertFalse(qa.process_group_exists(process.pid))
                request = qa.load_json((root / "requests.jsonl").read_bytes())
                self.assertFalse(request["unsafe"])
            finally:
                sock.close()
                if process.poll() is None:
                    qa.kill_group(process)
                process.stdout.close()
                process.stderr.close()


class ReplayBindingTests(unittest.TestCase):
    setUp = ReplayTests.setUp
    replay = ReplayTests.replay

    def test_skipped_coverage(self):
        self.report = copy.deepcopy(self.trusted)
        self.report["rows"][2]["skipped"] = True
        with self.assertRaises(ValueError):
            self.replay()

    def test_crash_infrastructure_and_boolean_status(self):
        for status in (-9, 101, 124, 125, 126, 127, 137, 999, True):
            self.report = copy.deepcopy(self.trusted)
            self.report["rows"][2]["status"] = status
            with self.subTest(status=status), self.assertRaises(ValueError):
                self.replay()
        for key, value in (("timeout", True), ("signal", 9), ("spawn_error", "ENOENT"), ("truncated", True)):
            self.report = copy.deepcopy(self.trusted)
            self.report["rows"][2]["health"][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.replay()

    def test_current_pins_and_external_freeze_not_archive_supplied(self):
        with self.assertRaises(ValueError):
            qa.replay(self.report, self.trusted, self.pin, {"fixture": "changed"})
        tampered = copy.deepcopy(self.trusted)
        tampered["rows"][2]["status"] = 0
        with self.assertRaises(ValueError):
            qa.replay(tampered, tampered, self.pin, tampered["pins"])
        self.report["classification"] = "NATIVE_PASS"
        with self.assertRaises(ValueError):
            self.replay()

    def test_raw_streams_cannot_be_normalized_away(self):
        for data in (b"fixture-only exact negative at //:extension.bzl:9\r\n",
                     b"fixture-only exact negative at //:extension.bzl:9\n\xff"):
            self.report = copy.deepcopy(self.trusted)
            self.report["rows"][2]["stderr"] = qa.blob(data)
            with self.assertRaises(ValueError):
                self.replay()


def synthetic_capture():
    """A complete synthetic collector ledger used only to attack replay predicates."""
    plan = qa.reference_plan()
    plan[4].update(row_type="command", engine="fixture", argv=["/synthetic-server"])
    origin = "http://127.0.0.1:18080"
    freeze = {"scope": "REFERENCE_ONLY synthetic predicate test; no native claim", "plan": plan,
              "reference": "/PINNED_REFERENCE", "pins": {}, "origin": origin,
              "case_definitions": [qa.asdict(c) for c in qa.cases()], "responses": qa.response_plan(),
              "fixtures": {c.name: {k: qa.blob(v) for k, v in qa.render(c, origin).items()} for c in qa.cases()},
              "aggregate_timeout_seconds": 1200, "scratch_limit_bytes": 1536 * 1024 * 1024}
    checkpoint = {"pins_sha256": qa.digest(qa.canonical({})), "scratch_bytes": 0, "elapsed_seconds": 0}
    rows, groups, results = [], [], []
    request_count = 0
    by_name = {c.name: c for c in qa.cases()}
    cleanup = None
    ready = qa.blob(qa.canonical({"ready": True, "address": ["127.0.0.1", 18080]}) + b"\n")
    for spec in plan:
        phase, name = spec["phase"], spec["case"]
        row = {"spec": copy.deepcopy(spec)}
        if phase == "server-start":
            row.update(pid=5000000 + spec["ordinal"], before=copy.deepcopy(checkpoint),
                       ready=copy.deepcopy(ready))
            groups.append(row["pid"])
        elif spec["row_type"] == "command":
            if phase == "version":
                row.update(status=0, stdout=qa.blob(b"Build label: 9.3.0\n"), stderr=qa.blob(b""), requests=[],
                    health={"timeout": False, "signal": None, "spawn_error": None, "truncated": False, "group_absent": True})
            else:
                row, source, base, _ = synthetic_stage(by_name[name], phase)
                row["spec"] = copy.deepcopy(spec)
                for request in row["requests"]:
                    request["ordinal"] = request_count
                    request_count += 1
                row["validation"] = qa.validate_stage(row, spec, by_name[name], source, base, origin)
            row.update(pid=5000000 + spec["ordinal"], before=copy.deepcopy(checkpoint), after=copy.deepcopy(checkpoint))
            groups.append(row["pid"])
        elif phase == "case-end":
            result = {"name": name, "classification": "MATCHED", "errors": []}
            row["result"] = result
            results.append(result)
        elif phase == "server-stop":
            row["server"] = {"launched": True, "status": 0, "group_absent": True, "stderr": qa.blob(b""),
                             "stdout": qa.blob(qa.unblob(ready) + b"SERVER_CLOSED\n")}
        elif phase == "cleanup":
            cleanup = {"groups": groups, "all_groups_absent": True, "scratch_absent": True}
            row["cleanup"] = cleanup
        elif phase == "pins-end":
            row["pins_sha256"] = qa.digest(qa.canonical({}))
        if spec["row_type"] == "event" and phase not in ("server-stop", "cleanup", "pins-end"):
            row["checkpoint"] = copy.deepcopy(checkpoint)
        rows.append(row)
    report = {"state": "COMPLETED_CAPTURE", "scope": freeze["scope"], "rows": rows, "cases": results, "cleanup": cleanup,
              "counts": {"planned_cases": len(by_name), "captured_cases": len(by_name), "matched": len(by_name), "unmatched": 0,
                         "planned_rows": len(plan), "actual_rows": len(plan), "planned_commands": len(groups),
                         "actual_commands": len(groups), "http_requests": request_count}}
    return report, freeze


class CaptureReplayTests(unittest.TestCase):
    def test_full_plan_replay_and_rehashed_mutations(self):
        report, freeze = synthetic_capture()
        self.assertEqual(qa.validate_capture(report, freeze), report["counts"])
        command_index = next(i for i, row in enumerate(report["rows"]) if row["spec"]["phase"] == "cold")
        mutations = [
            lambda r: r["rows"][command_index]["before"].update(pins_sha256="0" * 64),
            lambda r: r["rows"][command_index]["spec"].update(engine="native"),
            lambda r: r["rows"][command_index]["spec"].update(cycle=1),
            lambda r: r["rows"][command_index]["artifacts"]["repository"]["root"].update(mode="0555"),
            lambda r: r["rows"][command_index]["requests"][0]["headers"].update(Authorization="synthetic"),
            lambda r: r["rows"][command_index]["before"].update(elapsed_seconds=1201),
            lambda r: r["cleanup"].update(all_groups_absent=False),
            lambda r: r["rows"][-1].update(pins_sha256="0" * 64),
            lambda r: r["rows"].pop(),
        ]
        for mutate in mutations:
            bad = copy.deepcopy(report)
            mutate(bad)
            # The enclosing JSON hash can be repaired; semantic verification must
            # still reject independently of that archive checksum.
            qa.digest(qa.canonical(bad))
            with self.assertRaises((ValueError, qa.HealthFailure)):
                qa.validate_capture(bad, freeze)
        bad = copy.deepcopy(report)
        bad["rows"][1:3] = reversed(bad["rows"][1:3])
        for i, row in enumerate(bad["rows"]):
            row["spec"]["ordinal"] = i
        with self.assertRaises(ValueError):
            qa.validate_capture(bad, freeze)


class PlatformSetupTests(unittest.TestCase):
    def test_exact_rewrite_no_public_host_allowance(self):
        origin = "http://127.0.0.1:18080"
        lines = qa.downloader_config(origin).decode("ascii").splitlines()
        self.assertEqual(lines[:3], ["block *", "allow 127.0.0.1", "allow localhost"])
        directive, pattern, destination = lines[3].split()
        self.assertEqual((directive, destination), ("rewrite", origin + qa.PLATFORM_ROUTE))
        self.assertEqual(destination.rsplit("/", 1)[1], qa.PLATFORM_BASENAME)
        scheme_stripped = qa.PLATFORM_URL.split("://", 1)[1]
        self.assertTrue(re.fullmatch(pattern, scheme_stripped))
        for changed in (scheme_stripped + "?token=x", scheme_stripped + "/extra",
                        scheme_stripped.replace("github.com", "evil.github.com"),
                        scheme_stripped.replace("github.com", "githubXcom"),
                        scheme_stripped.replace("1.0.0", "1.0.1"),
                        "github.com/other/archive.tar.gz"):
            self.assertIsNone(re.fullmatch(pattern, changed))
        self.assertEqual(len(lines), 4)
        with self.assertRaises(ValueError):
            qa.downloader_config("http://github.com:80")

    def test_fixed_archive_pin_before_launch_and_corrupt_distdir_rejected(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-pin-") as directory:
            root = Path(directory)
            distdir = root / "inputs/distdir"
            distdir.mkdir(parents=True)
            path = distdir / qa.PLATFORM_BASENAME
            data = qa.platforms_archive()
            path.write_bytes(data)
            pin = qa.platform_distdir_pin(root)
            self.assertEqual((pin["size"], pin["sha256"]), (7879, qa.PLATFORM_SHA256))
            path.write_bytes(bytes([data[0] ^ 1]) + data[1:])
            with self.assertRaises(ValueError):
                qa.platform_distdir_pin(root)

    def test_setup_plan_distinct_and_disable_never_waived(self):
        plan = qa.reference_plan(setup=True)
        commands = [r for r in plan if r["row_type"] == "command"]
        self.assertEqual([r["phase"] for r in commands], ["version", "cold", "cquery"])
        self.assertTrue(all(r["purpose"] == "platform-setup" and r["platform_requests"] == [] for r in plan))
        for row in commands[1:]:
            self.assertIn("--repository_disable_download", row["argv"])
            self.assertIn("--repository_cache=", row["argv"])
        fixture = qa.render(qa.selected_cases(True)[0])
        self.assertNotIn("extension.bzl", fixture)
        self.assertNotIn(b"use_extension", fixture["MODULE.bazel"])
        self.assertEqual(fixture["seed.txt"], (qa.FIXTURES / "setup_seed.txt").read_bytes())
        self.assertEqual(qa.selected_cases(False), qa.cases())
        self.assertEqual(len(qa.cases()), 33)
        with self.assertRaises(ValueError):
            qa.require_setup_receipt(None, None, None, "/not/executed")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            qa.main(["--reference-only", "--reference", "/not/executed", "--reference-sha256", "x",
                     "--registry-input", "/unused", "--official-source", "/unused", "--artifact-dir", "/unused"])

    def test_platform_requests_are_classified_and_never_ignored(self):
        case = qa.cases()[0]
        row, source, base, origin = synthetic_stage(case, "cold")
        route = qa.response_plan()[qa.PLATFORM_ROUTE]
        self.assertEqual((route["purpose"], route["body"]["sha256"], route["body"]["size"]),
                         ("platform-setup", qa.PLATFORM_SHA256, 7879))
        platform = {"ordinal": 0, "method": "GET", "path": qa.PLATFORM_ROUTE, "peer": "127.0.0.1",
                    "unsafe": False, "headers": {"Host": "127.0.0.1:18080"}, "purpose": "platform-setup",
                    "header_items": [["Host", "127.0.0.1:18080"]],
                    "status": 200, "response_headers": route["headers"], "body_sha256": qa.PLATFORM_SHA256, "body_size": 7879}
        row["requests"].insert(0, platform)
        qa.stage_health(row, qa.response_plan(), origin)
        errors = qa.validate_stage(row, row["spec"], case, source, base, origin)["errors"]
        self.assertIn("platform requests: planned setup requests", errors)
        platform["purpose"] = "ctx-api"
        with self.assertRaises(ValueError):
            qa.stage_health(row, qa.response_plan(), origin)

    def test_setup_stage_requires_real_source_output_modes_and_private_miss(self):
        root = Path("/MODULE_IO_OWNED")
        spec = next(r for r in qa.reference_plan(setup=True) if r["phase"] == "cold")
        source = root / "scratch/workspace" / qa.SETUP_CASE / "seed.txt"
        seed = (qa.FIXTURES / "setup_seed.txt").read_bytes()
        row = {"spec": spec, "status": 0, "stdout": qa.blob(b""), "stderr": qa.blob(b""), "requests": [],
               "setup_before": {"platforms_absent": True, "contents_cache_empty": True},
               "artifacts": {"setup_source": {"path": str(source), "data": qa.blob(seed), "mode": "0644",
                                               "root": qa.manifest_entry("", "directory", "0755")},
                             "outputs": [{"data": qa.blob(seed), "mode": "0555"}],
                             "platform_files": {name: {"data": qa.blob(data),
                                 "path": str(root / "scratch/base" / qa.SETUP_CASE / "external/platforms" / name)}
                                 for name, data in qa.expected_platform_files().items()}}}
        self.assertEqual(qa.validate_setup_stage(row, spec, root)["errors"], [])
        mutations = [lambda r: r["setup_before"].update(contents_cache_empty=False),
                     lambda r: r["artifacts"]["setup_source"].update(mode="0555"),
                     lambda r: r["artifacts"]["outputs"][0].update(mode="0644"),
                     lambda r: r["artifacts"]["outputs"][0].update(data=qa.blob(b"wrong")),
                     lambda r: r["artifacts"]["platform_files"].pop("MODULE.bazel"),
                     lambda r: r["requests"].append({"purpose": "platform-setup"})]
        for mutate in mutations:
            bad = copy.deepcopy(row)
            mutate(bad)
            self.assertTrue(qa.validate_setup_stage(bad, spec, root)["errors"])


class PlatformClosureTests(unittest.TestCase):
    def test_actual_tracked_closure_includes_implicit_module_file(self):
        closure = qa.platform_closure()
        self.assertEqual([r["kind"] for r in closure], ["archive", "module-file"])
        archive, module = closure
        self.assertEqual((archive["sha256"], archive["size"]), (qa.PLATFORM_SHA256, 7879))
        self.assertEqual((module["sha256"], module["size"], module["integrity"]),
                         ("f05feb42b48f1b3c225e4ccf351f367be0371411a803198ec34a389fb22aa580", 286,
                          "sha256-8F/rQrSPGzwiXkzPNR82e+A3FBGoAxmOw0o4n7IqpYA="))
        tracked = qa.ROOT / "tests/fixtures/builtin_modules/registry" / module["registry_path"]
        self.assertEqual(qa.unblob(module["data"]), tracked.read_bytes())
        self.assertEqual(qa.expected_platform_files()["MODULE.bazel"], tracked.read_bytes())
        self.assertNotEqual(archive["distdir"], module["distdir"])

    def test_declared_patch_overlay_completeness_and_collision_safety(self):
        # Synthetic verifier inputs only; these patches are never served/executed.
        genuine_module = qa.unblob(qa.platform_closure()[-1]["data"])
        prefix = "modules/platforms/1.0.0/"
        bodies = {prefix + "MODULE.bazel": genuine_module,
                  prefix + "patches/one/fix.patch": b"synthetic patch one\n",
                  prefix + "patches/two/fix.patch": b"synthetic patch two\n",
                  prefix + "overlay/nested/MODULE.bazel": b"synthetic verifier overlay\n"}
        source = {"url": qa.PLATFORM_URL, "integrity": qa.result_fields(qa.platforms_archive())["integrity"],
                  "patches": {name: qa.result_fields(bodies[prefix + "patches/" + name])["integrity"]
                              for name in ("one/fix.patch", "two/fix.patch")},
                  "overlay": {"nested/MODULE.bazel": qa.result_fields(bodies[prefix + "overlay/nested/MODULE.bazel"])["integrity"]}}
        rows = qa.platform_closure(source, bodies.__getitem__)
        self.assertEqual([r["kind"] for r in rows], ["archive", "patch", "patch", "overlay", "module-file"])
        self.assertEqual(rows[1]["basename"], rows[2]["basename"])
        self.assertNotEqual(rows[1]["distdir"], rows[2]["distdir"])
        self.assertEqual(rows[3]["basename"], rows[4]["basename"])
        self.assertNotEqual(rows[3]["distdir"], rows[4]["distdir"])
        missing = dict(bodies)
        del missing[prefix + "patches/two/fix.patch"]
        with self.assertRaises(KeyError):
            qa.platform_closure(source, missing.__getitem__)
        corrupt = dict(bodies)
        corrupt[prefix + "patches/one/fix.patch"] = b"synthetic patch BAD\n"
        with self.assertRaises(ValueError):
            qa.platform_closure(source, corrupt.__getitem__)

    def test_archive_only_cannot_pass_closure_checkpoint(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-closure-") as directory:
            root = Path(directory)
            closure = qa.platform_closure()
            archive = closure[0]
            path = root / archive["distdir"] / archive["basename"]
            path.parent.mkdir(parents=True)
            path.write_bytes(qa.unblob(archive["data"]))
            qa.platform_distdir_pin(root)
            with self.assertRaises(FileNotFoundError):
                qa.platform_closure_pins(root)
            module = closure[-1]
            path = root / module["distdir"] / module["basename"]
            path.parent.mkdir(parents=True)
            path.write_bytes(qa.unblob(module["data"]))
            self.assertEqual(len(qa.platform_closure_pins(root)), 2)
            data = path.read_bytes()
            path.write_bytes(bytes([data[0] ^ 1]) + data[1:])
            with self.assertRaises(ValueError):
                qa.platform_closure_pins(root)

    def test_tracked_source_and_module_pins_reject_changed_bytes(self):
        pin = qa.load_json((qa.FIXTURES / "platform_closure.json").read_bytes())
        prefix = Path(pin["registry_relative_root"]) / "modules/platforms/1.0.0"
        originals = {name: (qa.ROOT / prefix / name).read_bytes() for name in ("source.json", "MODULE.bazel")}
        with tempfile.TemporaryDirectory(prefix="module-extension-io-sourcepin-") as directory:
            root = Path(directory)
            (root / prefix).mkdir(parents=True)
            for changed in originals:
                for name, data in originals.items():
                    (root / prefix / name).write_bytes(data + (b"\n" if name == changed else b""))
                with mock.patch.object(qa, "ROOT", root), self.assertRaises(ValueError):
                    qa.platform_closure()

    def test_enabled_api_smoke_and_matrix_do_not_inherit_disable(self):
        setup = qa.reference_plan(setup=True)
        enabled = qa.reference_plan(api_smoke=True)
        self.assertEqual([r["phase"] for r in enabled if r["row_type"] == "command"], ["version", "cold", "warm", "cquery"])
        self.assertTrue(all("--repository_disable_download" not in r["argv"] for r in enabled))
        self.assertTrue(all(r["purpose"] == "ctx-smoke" for r in enabled))
        for plan in (setup, enabled, qa.reference_plan()):
            for row in plan:
                if row["row_type"] == "command" and row["phase"] != "version":
                    distdirs = [a for a in row["argv"] if a.startswith("--distdir=")]
                    self.assertEqual(len(distdirs), 2)
                    self.assertNotEqual(distdirs[0], distdirs[1])
        self.assertEqual(qa.selected_cases(api_smoke=True), qa.cases()[:1])
        with self.assertRaises(ValueError):
            qa.require_api_smoke_receipt(None, None, None, "/not/executed")


def genuine_negative_inputs():
    """Real retained diagnostic bytes + source pins; surrounding test rows are hooks only."""
    directory = qa.FIXTURES / "negative_build"
    provenance = qa.load_json((directory / "provenance.json").read_bytes())
    plan = qa.reference_plan(provenance["capture_root"], provenance["reference"])
    by_name = {case.name: case for case in qa.cases()}
    for proof in provenance["rows"]:
        case = by_name[proof["case"]]
        row, _, _, _ = synthetic_stage(case, "negative")
        spec = plan[proof["ordinal"]]
        source = qa.render(case, provenance["origin"])["extension.bzl"]
        raw = (directory / (case.name + ".stderr.raw")).read_bytes()
        qa.equal([len(raw), qa.digest(raw)], [proof["stderr_size"], proof["stderr_sha256"]], "portable genuine stream pin")
        qa.equal([len(source), qa.digest(source)], [proof["source_size"], proof["source_sha256"]], "portable genuine source pin")
        row.update(spec=copy.deepcopy(spec), status=proof["status"], stderr=qa.blob(raw))
        base = Path(provenance["capture_root"]) / "scratch/base" / case.name
        yield row, spec, case, source, base, provenance["origin"]


class NegativeProfileTests(unittest.TestCase):
    def test_nine_actual_build_streams_and_sources(self):
        cases = list(genuine_negative_inputs())
        self.assertEqual(len(cases), 9)
        for row, spec, case, source, base, origin in cases:
            with self.subTest(case=case.name):
                profile = qa.negative_profile(spec, case, source, origin)
                self.assertEqual((profile["command"], profile["status"], profile["target"], profile["extension"]),
                                 ("build", 1, "//:consume", "@@//:extension.bzl%io"))
                self.assertEqual(profile["operation"], case.operations[-1]["method"])
                self.assertEqual(qa.validate_stage(row, spec, case, source, base, origin)["errors"], [])

    def test_valid_rehashed_wrong_semantics_rejected_for_all_nine(self):
        for original, spec, case, source, base, origin in genuine_negative_inputs():
            profile = qa.negative_profile(spec, case, source, origin)
            operation = profile["operation"].encode()
            other = b"read" if operation == b"download" else b"download"
            mutations = {
                "operation": lambda raw: raw.replace(b"ctx." + operation + b"(", b"ctx." + other + b"("),
                "target": lambda raw: raw.replace(b"target '//:consume'", b"target '//:other'"),
                "extension": lambda raw: raw.replace(b"@@//:extension.bzl%io", b"@@//:extension.bzl%other"),
                "footer": lambda raw: raw.replace(profile["terminal"].encode(), b"ERROR: unsupported feature"),
                "query-footer": lambda raw: raw.replace(profile["terminal"].encode(), b"ERROR: Target parsing failed due to unexpected exception: error evaluating module extension @@//:extension.bzl%io"),
                "site": lambda raw: raw.replace(("extension.bzl:" + str(profile["line"]) + ":" + str(profile["column"])).encode(), b"extension.bzl:999:1"),
                "source-echo": lambda raw: raw.replace(b"DEBUG: ", b"SOURCE: ", 1),
                "extra-footer": lambda raw: raw + b"unsupported arbitrary footer\n",
                "duplicate-summary": lambda raw: raw.replace(b"INFO: 1 process: 1 internal.\n", b"INFO: 1 process: 1 internal.\n" * 2),
                "cache-fallback": lambda raw: raw.replace(b"INFO: 1 process: 1 internal.", b"INFO: All actions retrieved from cache"),
                "wrong-global": lambda raw: raw.replace(b"Error in " + operation + b":", b"Error in repository_ctx." + operation + b":"),
            }
            for name, mutate in mutations.items():
                with self.subTest(case=case.name, mutation=name):
                    row = copy.deepcopy(original)
                    row["stderr"] = qa.blob(mutate(qa.unblob(row["stderr"])))
                    qa.unblob(row["stderr"])
                    qa.digest(qa.canonical(row))  # A rebuilt enclosing proof hash is still invalid.
                    self.assertTrue(qa.validate_stage(row, spec, case, source, base, origin)["errors"])
            for status in (0, 7, 101, 124, True):
                row = copy.deepcopy(original)
                row["status"] = status
                self.assertTrue(qa.validate_stage(row, spec, case, source, base, origin)["errors"])
            changed_source = source.replace(b"ctx." + operation + b"(", b"ctx." + other + b"(")
            self.assertTrue(qa.validate_stage(original, spec, case, changed_source, base, origin)["errors"])

    def test_query_profile_keeps_query_exit_and_footer_only(self):
        row, spec, case, source, base, origin = next(genuine_negative_inputs())
        query_spec = copy.deepcopy(spec)
        query_spec["argv"][query_spec["argv"].index("build")] = "query"
        query_spec["argv"] = [a for a in query_spec["argv"] if not a.startswith(("--spawn_strategy=", "--execution_log_json_file="))]
        profile = qa.negative_profile(query_spec, case, source, origin)
        self.assertEqual(profile["status"], 7)
        raw = qa.unblob(row["stderr"])
        build_terminal = qa.negative_profile(spec, case, source, origin)["terminal"].encode()
        query_raw = raw[:raw.index(build_terminal)] + profile["terminal"].encode() + b"\n"
        row.update(spec=query_spec, status=7, stderr=qa.blob(query_raw))
        self.assertEqual(qa.validate_stage(row, query_spec, case, source, base, origin)["errors"], [])
        row["status"] = 1
        self.assertTrue(qa.validate_stage(row, query_spec, case, source, base, origin)["errors"])
        row.update(status=7, stderr=qa.blob(raw))
        self.assertTrue(qa.validate_stage(row, query_spec, case, source, base, origin)["errors"])

    def test_plan_not_observation_controls_target_command_and_operation(self):
        row, spec, case, source, base, origin = next(genuine_negative_inputs())
        for mutate in (lambda r: r["spec"]["argv"].__setitem__(r["spec"]["argv"].index("build"), "query"),
                       lambda r: r["spec"]["argv"].__setitem__(r["spec"]["argv"].index("//:consume"), "//:other"),
                       lambda r: r["health"].update(timeout=True),
                       lambda r: r["artifacts"]["outputs"].append({"data": qa.blob(b"fallback")}),
                       lambda r: r["artifacts"].update(repository={"name": "made"})):
            bad = copy.deepcopy(row)
            mutate(bad)
            self.assertTrue(qa.validate_stage(bad, spec, case, source, base, origin)["errors"])

    def test_preparation_assessment_never_mutates_historical_verdicts(self):
        report, freeze = synthetic_capture()
        for row in report["rows"]:
            if row["spec"]["phase"] == "negative":
                row["validation"]["errors"] = ["historical exit mismatch", "historical footer mismatch"]
            if row["spec"]["phase"] == "case-end" and next(c for c in qa.cases() if c.name == row["spec"]["case"]).result == "callback-error":
                row["result"].update(classification="UNMATCHED", errors=["historical frozen mismatch"])
        report["counts"].update(matched=24, unmatched=9)
        before = qa.canonical(report)
        assessment = qa.validate_capture(report, freeze, preparation=True)
        self.assertEqual(qa.canonical(report), before)
        self.assertEqual(assessment["state"], "PREPARATION_REPLAY_ONLY")
        self.assertEqual((assessment["historical_counts"]["matched"], assessment["historical_counts"]["unmatched"]), (24, 9))
        self.assertEqual((assessment["counts"]["matched"], assessment["counts"]["unmatched"]), (33, 0))
        self.assertFalse(assessment["runtime_acceptance"])
        with self.assertRaises(ValueError):
            qa.validate_capture(report, freeze)


class InventoryTests(unittest.TestCase):
    def fixture(self, root):
        (root / "nested").mkdir()
        (root / "nested/SHA256SUMS.json").write_bytes(b'{"retained":true}\n')
        (root / "data.bin").write_bytes(b"exact raw bytes\x00\xff")
        manifest = qa.archive_inventory(root)
        qa.save_json(root / "SHA256SUMS.json", manifest)
        return manifest, qa.digest((root / "SHA256SUMS.json").read_bytes())

    def test_only_top_relative_manifest_excluded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest, pin = self.fixture(root)
            self.assertEqual(set(manifest), {"data.bin", "nested/SHA256SUMS.json"})
            self.assertEqual(qa.load_verified_inventory(root, pin), manifest)
            (root / "nested/deeper").mkdir()
            (root / "nested/deeper/SHA256SUMS.json").write_bytes(b"{}\n")
            self.assertIn("nested/deeper/SHA256SUMS.json", qa.archive_inventory(root))
            with self.assertRaises(ValueError):
                qa.verify_inventory(root, manifest)

    def test_nested_tamper_added_renamed_missing_rebuilt_proofs_rejected(self):
        for mutation in ("tamper", "add", "rename", "remove"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                trusted, trusted_hash = self.fixture(root)
                nested = root / "nested/SHA256SUMS.json"
                if mutation == "tamper": nested.write_bytes(b'{"retained":false}\n')
                elif mutation == "add":
                    (root / "new").mkdir()
                    (root / "new/SHA256SUMS.json").write_bytes(b"{}\n")
                elif mutation == "rename": nested.rename(root / "nested/renamed-SHA256SUMS.json")
                else: nested.unlink()
                rebuilt = qa.archive_inventory(root)
                qa.save_json(root / "SHA256SUMS.json", rebuilt)
                # Internal hashes and membership are coherent with the BAD tree;
                # they do not replace the out-of-band trusted inventory/pin.
                qa.verify_inventory(root, rebuilt)
                with self.assertRaises(ValueError): qa.verify_inventory(root, trusted)
                with self.assertRaises(ValueError): qa.load_verified_inventory(root, trusted_hash)

    def test_duplicate_traversal_noncanonical_and_self_entries_rejected(self):
        with self.assertRaises(ValueError):
            qa.load_json('{"nested/SHA256SUMS.json": {}, "nested/SHA256SUMS.json": {}}')
        pin = {"sha256": qa.EMPTY_SHA256, "size": 0, "mode": "0644"}
        for name in ("../escape", "/absolute", "a/../b", "a//b", "./x", "a\\b", "", "SHA256SUMS.json"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                qa.inventory_map({name: pin})
        with self.assertRaises(ValueError):
            qa.inventory_map({"nested/SHA256SUMS.json": {**pin, "size": False}})

    def test_symlinks_and_special_files_including_manifest_rejected(self):
        for target in ("nested/SHA256SUMS.json", "SHA256SUMS.json", "data.bin"):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.fixture(root)
                (root / target).unlink()
                (root / target).symlink_to(root / "missing")
                with self.assertRaises(qa.HealthFailure): qa.archive_inventory(root)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            os.mkfifo(root / "pipe")
            with self.assertRaises(qa.HealthFailure): qa.archive_inventory(root)

    def test_legacy_exclusion_is_explicit_and_not_new_inventory_acceptance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            complete, _ = self.fixture(root)
            legacy = {k: v for k, v in complete.items() if k != "nested/SHA256SUMS.json"}
            freeze = {"pins": {str(root / "nested/SHA256SUMS.json"): complete["nested/SHA256SUMS.json"]}}
            audit = qa.historical_inventory(root, freeze, legacy)
            self.assertEqual(audit["classification"], "LEGACY_EXCLUSIONS_AUDITED_NOT_TOP_MANIFEST_MEMBERS")
            with self.assertRaises(ValueError): qa.verify_inventory(root, legacy)
            (root / "nested/SHA256SUMS.json").write_bytes(b"{}\n")
            with self.assertRaises(ValueError): qa.historical_inventory(root, freeze, legacy)


class OrderedDiagnosticTests(unittest.TestCase):
    def test_rehashed_warnings_cannot_move_before_callback_or_between_wrong_sites(self):
        row, spec, case, source, base, origin = next(v for v in genuine_negative_inputs() if v[2].name == "allfail-false")
        raw = qa.unblob(row["stderr"])
        lines = raw.splitlines(keepends=True)
        warnings = [line for line in lines if line.startswith(b"WARNING:")]
        without = [line for line in lines if not line.startswith(b"WARNING:")]
        moved = without[:1] + warnings + without[1:]
        variants = [b"".join(moved), raw.replace(warnings[0], warnings[0] * 2),
                    raw.replace(warnings[0], warnings[0] + lines[1]),
                    raw.replace(warnings[0], warnings[0] + lines[1].replace(b"allfail-false", b"wrong-case")),
                    raw.replace(warnings[0], warnings[0] + lines[1].replace(b"extension.bzl", b"wrong.bzl"))]
        for mutated in variants:
            bad = copy.deepcopy(row)
            bad["stderr"] = qa.blob(mutated)
            qa.unblob(bad["stderr"])
            qa.digest(qa.canonical(bad))
            with self.assertRaises(ValueError):
                qa.validate_negative(mutated, b"", case, spec, source, base, origin)
            self.assertTrue(qa.validate_stage(bad, spec, case, source, base, origin)["errors"])


class LifecycleHardeningTests(unittest.TestCase):
    def test_mandatory_phase_checkpoints_and_typed_resource_pins(self):
        report, freeze = synthetic_capture()
        self.assertEqual(qa.validate_capture(report, freeze), report["counts"])
        for index, row in enumerate(report["rows"]):
            required = set(row) & {"before", "after", "checkpoint"}
            for key in required:
                bad = copy.deepcopy(row)
                del bad[key]
                qa.digest(qa.canonical(bad))
                with self.subTest(phase=row["spec"]["phase"], field=key), self.assertRaises(ValueError):
                    qa.validate_lifecycle_row(bad, freeze["plan"][index], freeze)
        bad = copy.deepcopy(report)
        del bad["rows"][0]["checkpoint"]
        with self.assertRaises(ValueError):
            qa.validate_capture(bad, freeze)
        for key, value in (("scratch_bytes", True), ("elapsed_seconds", True), ("pins_sha256", "0" * 64)):
            checkpoint = copy.deepcopy(report["rows"][0]["checkpoint"])
            checkpoint[key] = value
            with self.assertRaises(ValueError):
                qa.validate_checkpoint(checkpoint, freeze)

    def test_exact_same_ready_closed_stream_in_live_and_replay(self):
        report, freeze = synthetic_capture()
        start = next(r for r in report["rows"] if r["spec"]["phase"] == "server-start")
        stop_at = next(i for i, r in enumerate(report["rows"]) if r["spec"]["phase"] == "server-stop")
        original = report["rows"][stop_at]["server"]
        qa.validate_server_stop(original, start["ready"], freeze)
        for raw in (qa.unblob(start["ready"]) + b"garbage\nSERVER_CLOSED\n",
                    b"garbage\n" + qa.unblob(original["stdout"]),
                    qa.unblob(original["stdout"]) + b"SERVER_CLOSED\n"):
            bad = copy.deepcopy(report)
            server = bad["rows"][stop_at]["server"]
            server["stdout"] = qa.blob(raw)
            qa.unblob(server["stdout"])
            with self.assertRaises(ValueError):
                qa.validate_server_stop(server, start["ready"], freeze)
            with self.assertRaises(ValueError):
                qa.validate_capture(bad, freeze)


class AllMethodServerTests(unittest.TestCase):
    def test_real_post_then_get_and_all_other_methods_are_audited_once(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-methods-") as directory:
            root = Path(directory)
            for name in ("home", "tmp"):
                (root / name).mkdir()
            qa.save_json(root / "routes.json", qa.response_plan())
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            sock.listen(4)
            address = sock.getsockname()
            origin = "http://127.0.0.1:" + str(address[1])
            process = subprocess.Popen([sys.executable, "-I", str(qa.FIXTURES / "http_server.py"), str(sock.fileno()),
                str(root / "routes.json"), str(root / "requests.jsonl")], cwd=root, env=qa.environment(str(root)),
                pass_fds=(sock.fileno(),), start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ)
                    self.assertTrue(selector.select(timeout=5))
                    ready = process.stdout.readline()
                sock.close()
                methods = ["POST", "GET", "HEAD", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "WHATEVER"]
                for method in methods:
                    connection = http.client.HTTPConnection(*address, timeout=5)
                    connection.request(method, "/versions?mode=json&include=all")
                    response = connection.getresponse()
                    data = response.read()
                    self.assertEqual(response.status, 200 if method == "GET" else 405)
                    self.assertEqual(data, qa.payloads()["versions"] if method == "GET" else b"")
                    connection.close()
                with socket.create_connection(address, timeout=5) as malformed:
                    malformed.sendall(b"BROKEN REQUEST\r\n\r\n")
                    while malformed.recv(4096):
                        pass
                os.killpg(process.pid, signal.SIGTERM)
                stdout, stderr = process.communicate(timeout=5)
                qa.validate_server_stop({"launched": True, "status": process.returncode, "stdout": qa.blob(ready + stdout),
                    "stderr": qa.blob(stderr), "group_absent": not qa.process_group_exists(process.pid)}, qa.blob(ready), {"origin": origin})
                requests = [qa.load_json(line) for line in (root / "requests.jsonl").read_bytes().splitlines()]
                self.assertEqual([r["method"] for r in requests], methods + ["BROKEN"])
                self.assertEqual([r["ordinal"] for r in requests], list(range(10)))
                self.assertEqual(sum(r["method"] == "GET" for r in requests), 1)
                self.assertTrue(all(r["unsafe"] for r in requests if r["method"] != "GET"))
                self.assertFalse(requests[-1]["parsed"])
                row, *_ = synthetic_stage(qa.cases()[0], "cold")
                row["requests"] = [requests[1]]
                qa.stage_health(row, qa.response_plan(), origin)
                row["requests"] = requests[:2]
                with self.assertRaises(qa.HealthFailure):
                    qa.stage_health(row, qa.response_plan(), origin)
                report, freeze = synthetic_capture()
                cold = next(r for r in report["rows"] if r["spec"]["phase"] == "cold")
                cold["requests"] = copy.deepcopy(requests[:2])
                ordinal = 0
                for record in report["rows"]:
                    for request in record.get("requests", []):
                        request["ordinal"] = ordinal
                        ordinal += 1
                report["counts"]["http_requests"] = ordinal
                qa.digest(qa.canonical(report))
                with self.assertRaises(qa.HealthFailure):
                    qa.validate_capture(report, freeze)
            finally:
                sock.close()
                if process.poll() is None:
                    qa.kill_group(process)
                process.stdout.close()
                process.stderr.close()
            self.assertFalse(qa.process_group_exists(process.pid))
        self.assertFalse(root.exists())


# Exact retained bytes from closure-capture-01/raw/checksummed-json/cold.exec.json.
# Report pin299e2643...; this is action-log validator input, NOT a new CLI result.
GENUINE_COLD_EXECLOG = r'''{
  "commandArgs": ["/bin/bash", "-c", "/bin/cat \"$1\" \u003e \"$2\"", "", "external/+io+made/data.json", "bazel-out/k8-fastbuild/bin/consumed.json"],
  "environmentVariables": [],
  "inputs": [{
    "path": "external/+io+made/data.json",
    "digest": {
      "hash": "0c97f86ec29e452966fce496c2853d3c68f3c38df8aac6322eb70c7e325fee24",
      "sizeBytes": "1039",
      "hashFunctionName": "SHA-256"
    },
    "isTool": false,
    "symlinkTargetPath": ""
  }],
  "listedOutputs": ["bazel-out/k8-fastbuild/bin/consumed.json"],
  "remotable": true,
  "cacheable": true,
  "timeoutMillis": "0",
  "mnemonic": "ModuleIOConsumer",
  "actualOutputs": [{
    "path": "bazel-out/k8-fastbuild/bin/consumed.json",
    "digest": {
      "hash": "0c97f86ec29e452966fce496c2853d3c68f3c38df8aac6322eb70c7e325fee24",
      "sizeBytes": "1039",
      "hashFunctionName": "SHA-256"
    },
    "isTool": false,
    "symlinkTargetPath": ""
  }],
  "runner": "local",
  "cacheHit": false,
  "status": "",
  "exitCode": 0,
  "remoteCacheable": true,
  "targetLabel": "//:consume",
  "metrics": {
    "totalTime": "0.017s",
    "queueTime": "0.001s",
    "setupTime": "0.002s",
    "executionWallTime": "0.004s",
    "inputBytes": "0",
    "inputFiles": "1",
    "memoryEstimateBytes": "0",
    "inputBytesLimit": "0",
    "inputFilesLimit": "0",
    "outputBytesLimit": "0",
    "outputFilesLimit": "0",
    "memoryBytesLimit": "0",
    "startTime": "2026-10-14T12:51:41.589Z",
    "measuredMemoryPeakBytes": "3145728"
  }
}'''.encode("ascii")


class RetainedLogTests(unittest.TestCase):
    def fixture(self, root):
        qa.equal([len(GENUINE_COLD_EXECLOG), qa.digest(GENUINE_COLD_EXECLOG)],
                 [1505, "64af78eb2472f7713d631ea6cc6d8e461e5b39c23aeb31e8fd930e340735c471"], "genuine log pin")
        plan = qa.reference_plan("/tmp/opencode/module-extension-io-closure-capture-01",
                                 "/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64")
        spec = plan[7]
        qa.equal(qa.digest(qa.canonical(spec)), "2136ae720ea02be209155992fd844a419248735758348eaebce79c98eae658a0", "actual captured spec")
        row = {"spec": copy.deepcopy(spec), "status": 0, "artifacts": {
            "execlog": qa.blob(GENUINE_COLD_EXECLOG),
            "outputs": [{"exec_path": "bazel-out/k8-fastbuild/bin/consumed.json", "data": {
                "size": 1039, "sha256": "0c97f86ec29e452966fce496c2853d3c68f3c38df8aac6322eb70c7e325fee24"}}]}}
        log = root / "raw/checksummed-json/cold.exec.json"
        log.parent.mkdir(parents=True)
        log.write_bytes(GENUINE_COLD_EXECLOG)
        return row, spec, {"plan": plan}, log

    def test_retained_empty_or_missing_cold_file_not_embedded_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            row, spec, freeze, log = self.fixture(root)
            qa.bind_retained_execution_log(root, row, spec, freeze)
            log.write_bytes(b"")
            manifest = qa.archive_inventory(root)
            qa.save_json(root / "SHA256SUMS.json", manifest)
            qa.load_verified_inventory(root, qa.digest((root / "SHA256SUMS.json").read_bytes()))
            # The freshly rebuilt archive is internally valid, but its actual
            # empty log cannot be replaced by the embedded successful spawn.
            with self.assertRaises(ValueError):
                qa.bind_retained_execution_log(root, row, spec, freeze)
            log.unlink()
            with self.assertRaises(FileNotFoundError):
                qa.bind_retained_execution_log(root, row, spec, freeze)

    def test_rehashed_substitutions_still_bind_target_paths_and_digests(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original, spec, freeze, log = self.fixture(root)
            mutations = [lambda s: s.update(targetLabel="//:other"),
                         lambda s: s["inputs"][0].update(path="external/+other+made/data.json"),
                         lambda s: s["actualOutputs"][0].update(path="other/consumed.json"),
                         lambda s: s["inputs"][0]["digest"].update(hash="0" * 64),
                         lambda s: s.update(cacheHit=True),
                         lambda s: s.update(runner="remote-cache")]
            for mutate in mutations:
                spawn = qa.json_objects(GENUINE_COLD_EXECLOG)[0]
                mutate(spawn)
                raw = qa.canonical(spawn)
                log.write_bytes(raw)
                row = copy.deepcopy(original)
                row["artifacts"]["execlog"] = qa.blob(raw)
                qa.unblob(row["artifacts"]["execlog"])
                with self.assertRaises(ValueError):
                    qa.bind_retained_execution_log(root, row, spec, freeze)

    def test_case_detached_log_argument_rejected_even_if_record_rebound(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            row, spec, freeze, _ = self.fixture(root)
            detached = copy.deepcopy(spec)
            detached["argv"] = [arg.replace("/raw/checksummed-json/", "/raw/mirrors-404-success/") for arg in detached["argv"]]
            row["spec"] = detached
            with self.assertRaises(ValueError):
                qa.bind_retained_execution_log(root, row, detached, freeze)

    def test_normal_replay_and_live_postcapture_use_shared_binding(self):
        # Pipeline unit hook only: the stand-in file is never executed or counted
        # as a reference/native binary. Injected failure must prevent acceptance.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reference = root / "not-executed"
            reference.write_bytes(b"unit binding input")
            freeze = {"reference": str(reference), "pins": {str(reference): qa.file_pin(reference)}}
            qa.save_json(root / "freeze.json", freeze)
            freeze_hash = qa.digest((root / "freeze.json").read_bytes())
            report = {"freeze_sha256": freeze_hash}
            qa.save_json(root / "report.json", report)
            qa.save_json(root / "SHA256SUMS.json", qa.archive_inventory(root))
            archive_hash = qa.digest((root / "SHA256SUMS.json").read_bytes())
            with mock.patch.object(qa, "bind_retained_bytes", side_effect=ValueError("retained cold file mismatch")) as binding:
                with self.assertRaisesRegex(ValueError, "retained cold file mismatch"):
                    qa.replay_reference(root, freeze_hash, archive_hash, reference)
                binding.assert_called_once()
            report = {"state": "COMPLETED_CAPTURE"}
            with mock.patch.object(qa, "bind_retained_bytes", side_effect=ValueError("retained cold file mismatch")) as binding:
                qa.check_postcapture_retained(root, report, freeze)
                binding.assert_called_once()
                self.assertEqual(report["state"], "EXECUTION_BLOCKER")


def unit_runtime_v2_tree(root):
    """Minimal complete policy tree; ELF-shaped bytes are NEVER executed."""
    n = qa.native_collector()
    for name in n.REQUIRED_RUNTIME_ROOTS:
        (root / name).mkdir(parents=True, exist_ok=True)
    names = [*n.REQUIRED_RUNTIME_FILES, "src/unit.rs", "vendor/unit.txt", "resources/unit.dat", "tests/unit.rs"]
    for name in names:
        (root / name).write_bytes(b"UNITONLY source binding input; not compiled\n")
    native = root / "not-executed-elf"
    native.write_bytes(b"\x7fELFUNITONLY; never executed")
    sources = {name: qa.file_pin(root / name)["sha256"] for name in names}
    ledger = {"schema": n.RUNTIME_SCHEMA, "coverage_policy": dict(n.COVERAGE_POLICY),
              "worktree": str(root), "native_path": str(native), "native_sha256": qa.file_pin(native)["sha256"],
              "runtime_source_count": len(sources), "runtime_sources": sources, "binding_scope": n.RUNTIME_SCOPE,
              "reviewed_additional_files": [], "reviewed_additional_roots": []}
    path = root / "unit-runtime.json"
    qa.save_json(path, ledger)
    return native, path, ledger


def native_artifact_fixture(root, case=None, phase="cold", data_encoding="latin-1"):
    """Filesystem/schema unit fixture only, never a simulated native execution."""
    collector = qa.native_collector()
    case = case or qa.cases()[0]
    origin = "http://127.0.0.1:18080"
    (root / "scratch/tmp").mkdir(parents=True)
    cache = root / "scratch/cache" / case.name
    tree_id, action_id = "a" * 64, "b" * 64
    tree = cache / "repositories/direct/trees" / tree_id
    tree.mkdir(parents=True)
    tree.chmod(0o755)
    data = json.dumps(qa.expected_consumer(case), sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode(data_encoding)
    for name, content in (("data.json", data), ("BUILD.bazel", b'exports_files(["data.json"])\n'), ("REPO.bazel", b"")):
        (tree / name).write_bytes(content)
        (tree / name).chmod(0o644)
    repo_manifest = cache / "repositories/direct/manifests" / (tree_id + ".json")
    repo_manifest.parent.mkdir(parents=True)
    qa.save_json(repo_manifest, {"format": 1, "profile": "direct-file-template-v1", "identity": tree_id, "files": collector.tree_entries(tree)})
    action_root = cache / "actions" / action_id
    output = action_root / "outputs" / collector.OUTPUT
    output.parent.mkdir(parents=True)
    # N36 layout (docs/HARDLINK_STAGING.md): the staged input's store copy
    # cas/<sha>-444 and the output blob cas/<sha>-555, which IS the published
    # output inode (one hardlink group), plus a v7 action manifest.
    store = cache / "cas"
    store.mkdir()
    (store / (qa.digest(data) + "-444")).write_bytes(data)
    (store / (qa.digest(data) + "-444")).chmod(0o444)
    blob = store / (qa.digest(data) + "-555")
    blob.write_bytes(data)
    blob.chmod(0o555)
    os.link(blob, output)
    qa.save_json(action_root / "manifest.json", {"format": "native-action-v8", "key": action_id,
        "outputs": [{"name": collector.OUTPUT.encode().hex(), "digest": qa.digest(data), "mode": 0o555,
                     "tree": None, "symlink": None}]})
    planned = collector.plan(root, "/unit-native-not-executed")
    spec = next(r for r in planned if r["case"] == case.name and r["phase"] == phase)
    source = qa.render(case, origin)["extension.bzl"]
    stderr = b"".join(qa.MARKER + json.dumps(event, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n"
                      for event in qa.expected_events(case))
    executed, hits = (1, 0) if phase == "cold" else (0, 1)
    stderr += f"bazel-rs: {executed} actions executed, {hits} cache hits (experimental local execution; not sandboxed)\n".encode()
    row = {"spec": spec, "status": 0, "stdout": qa.blob(("//:consume\t" + str(output) + "\n").encode()),
           "stderr": qa.blob(stderr), "requests": [{"method": "GET", "path": path} for path in case.requests],
           "artifacts": collector.observe(root, case)}
    frozen = {"plan": planned, "origin": origin, "fixtures": {case.name: {"extension.bzl": qa.blob(source)}}}
    return collector, case, row, spec, frozen


class NativeCollectorTests(unittest.TestCase):
    def test_native_plan_has_only_real_supported_commands_and23_deferred_lanes(self):
        collector = qa.native_collector()
        plan = collector.plan("/NATIVE_OWNED", "/pinned-native")
        commands = [r for r in plan if r["row_type"] == "command"]
        self.assertEqual(len(commands), 1 + sum(2 if c.result == "success" else 1 for c in qa.cases()))
        self.assertEqual(len(plan), 189)
        self.assertEqual(len(collector.deferred_lanes()), 23)
        self.assertTrue(all(r["state"] == "DEFERRED_NOT_EXECUTED" for r in collector.deferred_lanes()))
        for row in commands:
            self.assertEqual(row["engine"], "native")
            self.assertEqual(row["umask"], 0o022)
            self.assertFalse(any(a.startswith(("--distdir", "--repository_cache", "--execution_log", "--override_repository", "--batch")) for a in row["argv"]))
            self.assertFalse(any(a in ("cquery", "aquery") for a in row["argv"]))
            if "--repository_disable_download" in row["argv"]:
                self.assertIn(row["case"], ("disabled-miss-false", "disabled-miss-true", "malformed-sha-disabled"))

    def test_runtime_v2_ledger_and_executable_drift_rejected_without_execution(self):
        collector = qa.native_collector()
        with tempfile.TemporaryDirectory(prefix="module-extension-io-runtime-unit-") as directory:
            root = Path(directory)
            native, ledger, value = unit_runtime_v2_tree(root)
            native_hash = qa.file_pin(native)["sha256"]
            ledger_hash = qa.file_pin(ledger)["sha256"]
            _, pins, binding = collector.runtime_binding(ledger, ledger_hash, native, native_hash, root)
            self.assertEqual(binding["source_count"], len(value["runtime_sources"]))
            self.assertEqual(len(pins), len(value["runtime_sources"]) + 2)
            (root / "src/unit.rs").write_bytes(b"changed")
            with self.assertRaises(ValueError):
                collector.runtime_binding(ledger, ledger_hash, native, native_hash, root)
            (root / "src/unit.rs").write_bytes(b"UNITONLY source binding input; not compiled\n")
            native.write_bytes(b"\x7fELFchanged")
            with self.assertRaises(ValueError):
                collector.runtime_binding(ledger, ledger_hash, native, native_hash, root)

    def test_native_utf8_frames_not_reference_latin1_or_source_echo(self):
        collector = qa.native_collector()
        case = next(c for c in qa.cases() if c.name == "all256-auto-no")
        source = qa.render(case)["extension.bzl"]
        event = {"event": "post", "value": bytes(range(256)).decode("latin-1")}
        text = json.dumps(event, ensure_ascii=False)
        native = qa.MARKER + text.encode("utf-8") + b"\n"
        reference = qa.MARKER + text.encode("latin-1") + b"\n"
        observation = collector.native_events(native, case, source, "http://127.0.0.1:18080")
        self.assertEqual(qa.logical_bytes(observation["events"][0]["value"]), bytes(range(256)))
        self.assertEqual(observation["site_binding"]["kind"], "frozen-fixture-sites-not-emitted-runtime-locations")
        with self.assertRaises(UnicodeDecodeError):
            collector.native_events(reference, case, source, "http://127.0.0.1:18080")
        with self.assertRaises(ValueError):
            collector.native_events(b'7 | print("' + native + b'")\n', case, source, "http://127.0.0.1:18080")
        with self.assertRaises(ValueError):
            collector.native_events(b"error: phase\n" + native, case, source, "http://127.0.0.1:18080")

    def test_native_frame_snippet_matches_pinned_starlark_truncation(self):
        collector = qa.native_collector()
        # Expected values mirror starlark_syntax 0.14.2 frame.rs test_truncate_snippet
        # scaled to max_len 80 (keep 77 chars + "..." only if >3 chars remain).
        for length, expected in ((0, 0), (77, 77), (80, 80), (81, 80), (200, 80)):
            text = "x" * length
            got = collector.frame_snippet("    " + text + "  ")
            self.assertEqual(len(got), expected)
            self.assertEqual(got.endswith("..."), length > 80)
        self.assertEqual(collector.frame_snippet("п" * 81), "п" * 77 + "...")

    def test_native_negative_requires_builtin_frame_and_truncated_snippet(self):
        collector = qa.native_collector()
        case = next(c for c in qa.cases() if c.name == "http-filter-false")
        source = qa.render(case, "http://127.0.0.1:18080")["extension.bzl"]
        good = unit_native_diagnostic(case, source)
        collector.negative_diagnostic(good, case, source)
        without_builtin = good.replace(b"  File <builtin>, in <module>\n", b"")
        with self.assertRaises(ValueError):
            collector.negative_diagnostic(without_builtin, case, source)
        untruncated = good.replace(b"...\n", b"", 1)
        with self.assertRaises(ValueError):
            collector.negative_diagnostic(untruncated, case, source)

    def test_native_repository_data_is_latin1_byte_string_not_utf8(self):
        case = next(c for c in qa.cases() if c.name == "all256-auto-no")
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen = native_artifact_fixture(Path(directory), case)
            validation = collector.validate_stage(row, spec, case, frozen)
            self.assertEqual(validation["issues"], [])
            self.assertEqual(validation["consumer"]["logical"], qa.expected_consumer(case))
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen = native_artifact_fixture(Path(directory), case, data_encoding="utf-8")
            with self.assertRaises((collector.RuntimeViolation, ValueError)):
                validation = collector.validate_stage(row, spec, case, frozen)
                if validation["issues"]:
                    raise collector.RuntimeViolation("issues")

    def test_native_actual_manifests_bytes_modes_and_no_query_fabrication(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen = native_artifact_fixture(Path(directory))
            validation = collector.validate_stage(row, spec, case, frozen)
            self.assertEqual(validation["issues"], [])
            self.assertEqual(validation["consumer"]["logical"], qa.expected_consumer(case))
            raw_manifest = qa.load_json(qa.unblob(row["artifacts"]["repositories"][0]["manifest"]))
            self.assertEqual(raw_manifest["files"][""], {"directory": True, "mode": 493, "size": 0, "sha256": ""})
            self.assertNotIn("execlog", row["artifacts"])
            self.assertNotIn("cquery", row["artifacts"])
            wrong = copy.deepcopy(row)
            wrong["artifacts"]["files"][next(p for p in wrong["artifacts"]["files"] if p.endswith("/data.json"))]["mode"] = "0555"
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(wrong, spec, case, frozen)
            wrong = copy.deepcopy(row)
            events = qa.expected_events(case)
            events[2]["value"]["fields"]["size_bytes"] = 619
            wrong["stderr"] = qa.blob(b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in events))
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(wrong, spec, case, frozen)

    def test_native_rehashed_bad_result_or_request_is_hard_not_checker_match(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            root = Path(directory)
            collector, case, row, spec, frozen = native_artifact_fixture(root)
            bad = copy.deepcopy(row)
            bad["requests"].append({"method": "GET", "path": "/never"})
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(bad, spec, case, frozen)
            bad = copy.deepcopy(row)
            bad["status"] = 1
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(bad, spec, case, frozen)
            # Rebuild native publication/manifest hashes around WRONG JSON data.
            data = json.dumps([{"success": False}]).encode()
            tree = root / "scratch/cache" / case.name / "repositories/direct/trees" / ("a" * 64)
            (tree / "data.json").write_bytes(data)
            manifest = tree.parents[1] / "manifests" / ("a" * 64 + ".json")
            qa.save_json(manifest, {"format": 1, "profile": "direct-file-template-v1", "identity": "a" * 64, "files": collector.tree_entries(tree)})
            bad["artifacts"] = collector.observe(root, case)
            qa.digest(qa.canonical(bad))
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(bad, spec, case, frozen)

    def test_native_checker_wording_mismatch_not_any_nonzero_pass(self):
        collector = qa.native_collector()
        case = next(c for c in qa.cases() if c.name == "http-filter-false")
        root = Path("/NATIVE_UNIT")
        spec = next(r for r in collector.plan(root, "/not-executed") if r["case"] == case.name and r["phase"] == "negative")
        source = qa.render(case)["extension.bzl"]
        frames = b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in qa.expected_events(case))
        row = {"spec": spec, "status": 1, "stdout": qa.blob(b""), "stderr": qa.blob(frames + b"unrelated error\n"),
               "requests": [], "artifacts": {"cache_root": str(root / "scratch/cache" / case.name),
                   "repositories": [], "actions": [], "decode_errors": [], "files": {}, "directories": {}, "links": [],
                   "cache_exists": False, "publication_entries": [], "unaccounted_publications": []}}
        frozen = {"plan": collector.plan(root, "/not-executed"), "origin": "http://127.0.0.1:18080",
                  "fixtures": {case.name: {"extension.bzl": qa.blob(source)}}}
        self.assertTrue(collector.validate_stage(row, spec, case, frozen)["issues"])
        row["status"] = 0
        with self.assertRaises(collector.RuntimeViolation):
            collector.validate_stage(row, spec, case, frozen)

    def test_strict_pair_rejects_stale_qa_and_incomplete_reference(self):
        collector = qa.native_collector()
        definitions = [qa.asdict(c) for c in qa.cases()]
        binding = {"schema": collector.RUNTIME_SCHEMA, "coverage_policy": dict(collector.COVERAGE_POLICY),
                   "scope": collector.RUNTIME_SCOPE, "source_count": 8, "ledger_sha256": "0" * 64,
                   "native_sha256": "1" * 64, "sources_sha256": "2" * 64, "source_membership_sha256": "3" * 64}
        nf = {"case_definitions": definitions, "qa_sources": {"scripts/validate_module_extension_io.py": {"sha256": "new"}},
              "runtime_binding": binding}
        rf = {"case_definitions": definitions, "pins": {}}
        counts = {"captured_cases": 33, "matched": 33, "unmatched": 0}
        native_result = {"counts": counts, "runtime_binding": binding}
        with self.assertRaises(ValueError):
            collector.compare_pair({}, nf, {}, rf, native_result, {"counts": counts})
        with self.assertRaises(ValueError):
            collector.compare_pair({}, nf, {}, rf, native_result, {"counts": {**counts, "matched": 24, "unmatched": 9}})
        with self.assertRaises(ValueError):
            collector.compare_pair({}, nf, {}, rf, {"counts": counts, "runtime_binding": {**binding, "ledger_sha256": "f" * 64}}, {"counts": counts})
        with self.assertRaises(ValueError):
            collector.compare_pair({}, {**nf, "runtime_binding": {"source_count": 8}}, {}, rf, native_result, {"counts": counts})


class NativeHardlinkStoreTests(unittest.TestCase):
    """N36 content-addressed store: in-cache hardlink groups, cas/ accounting, v7 manifests."""

    def fixture(self, directory):
        collector, case, row, spec, frozen = native_artifact_fixture(Path(directory))
        cache = Path(row["artifacts"]["cache_root"])
        key = row["artifacts"]["actions"][0]["key"]
        output = "actions/" + key + "/outputs/" + collector.OUTPUT
        blob = next(name for name in row["artifacts"]["files"] if name.endswith("-555"))
        return collector, case, row, spec, frozen, cache, output, blob

    def test_in_cache_hardlink_group_is_measured_recorded_and_accepted(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen, cache, output, blob = self.fixture(directory)
            self.assertEqual(row["artifacts"]["links"], [sorted([output, blob])])
            self.assertEqual((cache / output).stat().st_nlink, 2)
            self.assertEqual(row["artifacts"]["unaccounted_publications"], [])
            self.assertEqual(row["artifacts"]["decode_errors"], [])
            self.assertIn(blob, row["artifacts"]["publication_entries"])
            self.assertEqual(collector.validate_stage(row, spec, case, frozen)["issues"], [])
            # A copy/rename fallback publication (no group) is equally valid.
            fallback = copy.deepcopy(row)
            fallback["artifacts"]["links"] = []
            self.assertEqual(collector.validate_stage(fallback, spec, case, frozen)["issues"], [])

    def test_link_with_a_name_outside_the_cache_or_standalone_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen, cache, output, blob = self.fixture(directory)
            outside = Path(directory) / "outside-name"
            os.link(cache / blob, outside)
            with self.assertRaisesRegex(collector.RuntimeViolation, "outside the snapshotted cache"):
                collector.observe(Path(directory), case)
            # Another case's cache is outside this case's snapshot too.
            outside.unlink()
            other = cache.parent / "other-case"
            other.mkdir()
            os.link(cache / blob, other / "alias")
            with self.assertRaisesRegex(collector.RuntimeViolation, "outside the snapshotted cache"):
                collector.observe(Path(directory), case)
            (other / "alias").unlink()
            collector.observe(Path(directory), case)
            # Without the whole-tree proof a single file must still be single-link.
            with self.assertRaisesRegex(collector.RuntimeViolation, "linked native publication"):
                collector.snapshot_file(cache / blob, cache)

    def test_symlinks_and_special_files_are_still_rejected(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen, cache, output, blob = self.fixture(directory)
            (cache / "cas/link").symlink_to(cache / blob)
            with self.assertRaises(collector.RuntimeViolation):
                collector.observe(Path(directory), case)
            (cache / "cas/link").unlink()
            os.mkfifo(cache / "cas/fifo")
            with self.assertRaises(collector.RuntimeViolation):
                collector.observe(Path(directory), case)

    def test_recorded_link_groups_must_have_n36_shape(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen, cache, output, blob = self.fixture(directory)
            files = row["artifacts"]["files"]
            data_file = next(name for name in files if name.endswith("/data.json"))
            input_blob = next(name for name in files if name.endswith("-444"))
            manifest = "actions/" + row["artifacts"]["actions"][0]["key"] + "/manifest.json"
            for groups in ([sorted([blob, data_file])],              # repository tree aliasing a blob
                           [sorted([blob, input_blob])],             # two blobs on one inode (and bytes/mode differ)
                           [sorted([output, manifest])],             # no blob, aliased manifest
                           [[output]],                               # a group of one name
                           [sorted([blob, output, "cas/missing"])],  # names a non-recorded file
                           [[max(output, blob), min(output, blob)]],  # noncanonical order
                           [sorted([blob, output])] * 2):            # a name in two groups
                with self.subTest(groups=groups):
                    bad = copy.deepcopy(row)
                    bad["artifacts"]["links"] = groups
                    with self.assertRaises((collector.RuntimeViolation, ValueError)):
                        collector.validate_stage(bad, spec, case, frozen)
            bad = copy.deepcopy(row)
            del bad["artifacts"]["links"]
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(bad, spec, case, frozen)

    def test_cas_blobs_must_name_their_own_digest_and_read_only_mode(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen, cache, output, blob = self.fixture(directory)
            digest = qa.digest(b"stray")
            variants = [("cas/" + "0" * 64 + "-444", b"stray", 0o444),   # name is not the content digest
                        ("cas/" + digest + "-555", b"stray", 0o444),     # mode differs from the name
                        ("cas/" + digest + "-644", b"stray", 0o644),     # writable blob
                        ("cas/" + digest, b"stray", 0o444),              # no mode suffix
                        ("cas/.tmp-" + digest, b"stray", 0o644),         # leftover temporary
                        ("cas/nested/" + digest + "-444", b"stray", 0o444)]
            for relative, data, mode in variants:
                with self.subTest(relative=relative):
                    path = cache / relative
                    path.parent.mkdir(exist_ok=True)
                    path.write_bytes(data)
                    path.chmod(mode)
                    artifacts = collector.observe(Path(directory), case)
                    self.assertIn(relative, artifacts["unaccounted_publications"])
                    bad = {**row, "artifacts": artifacts}
                    with self.assertRaises(collector.RuntimeViolation):
                        collector.validate_stage(bad, spec, case, frozen)
                    path.unlink()
                    if path.parent.name == "nested":
                        path.parent.rmdir()
            # A stale but valid blob is legitimate: the store has no GC.
            (cache / ("cas/" + digest + "-444")).write_bytes(b"stray")
            (cache / ("cas/" + digest + "-444")).chmod(0o444)
            artifacts = collector.observe(Path(directory), case)
            self.assertEqual(artifacts["unaccounted_publications"], [])
            self.assertEqual(collector.validate_stage({**row, "artifacts": artifacts}, spec, case, frozen)["issues"], [])

    def test_action_manifest_must_be_v8_regular_file_outputs(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            collector, case, row, spec, frozen, cache, output, blob = self.fixture(directory)
            path = cache / "actions" / row["artifacts"]["actions"][0]["key"] / "manifest.json"
            good = qa.load_json(path.read_bytes())
            name, entry = good["outputs"][0]["name"], good["outputs"][0]
            variants = [{**good, "format": "native-action-v5",
                         "outputs": [{"name": name, "digest": entry["digest"], "executable": 0o111}]},
                        {**good, "format": "native-action-v6"},
                        {**good, "format": "native-action-v7"},
                        {**good, "outputs": [{**entry, "mode": 0o755}]},
                        {**good, "outputs": [{**entry, "symlink": "2f6574632f706173737764"}]},
                        {**good, "outputs": [{**entry, "tree": {"files": [], "directories": []}}]},
                        {**good, "outputs": [{k: v for k, v in entry.items() if k != "tree"}]}]
            for manifest in variants:
                with self.subTest(manifest=manifest):
                    qa.save_json(path, manifest)
                    artifacts = collector.observe(Path(directory), case)
                    self.assertTrue(artifacts["decode_errors"])
                    with self.assertRaises(collector.RuntimeViolation):
                        collector.validate_stage({**row, "artifacts": artifacts}, spec, case, frozen)
            qa.save_json(path, good)
            self.assertEqual(collector.observe(Path(directory), case)["decode_errors"], [])

    def test_negative_case_cannot_leave_store_blobs_or_link_groups(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-native-unit-") as directory:
            root = Path(directory)
            collector = qa.native_collector()
            case = next(c for c in qa.cases() if c.name == "http-filter-false")
            source = qa.render(case, "http://127.0.0.1:18080")["extension.bzl"]
            planned = collector.plan(root, "/unit-native-not-executed")
            spec = next(r for r in planned if r["case"] == case.name and r["phase"] == "negative")
            frozen = {"plan": planned, "origin": "http://127.0.0.1:18080",
                      "fixtures": {case.name: {"extension.bzl": qa.blob(source)}}}
            (root / "scratch/tmp").mkdir(parents=True)
            cache = root / "scratch/cache" / case.name
            (cache / "cas").mkdir(parents=True)  # an empty namespace is not a publication
            frames = b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in qa.expected_events(case))
            row = {"spec": spec, "status": 1, "stdout": qa.blob(b""), "requests": [],
                   "stderr": qa.blob(frames + unit_native_diagnostic(case, source)),
                   "artifacts": collector.observe(root, case)}
            self.assertEqual(collector.validate_stage(row, spec, case, frozen)["issues"], [])
            blob = cache / "cas" / (qa.digest(b"x") + "-444")
            blob.write_bytes(b"x")
            blob.chmod(0o444)
            row["artifacts"] = collector.observe(root, case)
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(row, spec, case, frozen)
            os.link(blob, cache / "alias")
            row["artifacts"] = collector.observe(root, case)
            self.assertEqual(row["artifacts"]["links"], [sorted(["alias", "cas/" + blob.name])])
            with self.assertRaises(collector.RuntimeViolation):
                collector.validate_stage(row, spec, case, frozen)


class HTTPHeaderCaseTests(unittest.TestCase):
    def row(self, items):
        row, *_ = synthetic_stage(qa.cases()[0], "cold")
        request = row["requests"][0]
        request.update(header_items=copy.deepcopy(items), headers=dict(items), parsed=True,
                       request_line_base64=base64.b64encode(b"GET /versions?mode=json&include=all HTTP/1.1\r\n").decode())
        return row

    def test_ascii_name_variants_preserve_raw_order_spelling_and_values(self):
        origin = "http://127.0.0.1:18080"
        for name in ("Host", "host", "HOST", "hOsT"):
            row = self.row([["uSeR-aGeNt", "MiXeD Agent VALUE"], [name, "127.0.0.1:18080"], ["AcCePt", "*/*"]])
            before = qa.canonical(row)
            result = qa.stage_health(row, qa.response_plan(), origin)
            self.assertEqual(qa.canonical(row), before)
            view = result["header_evidence"][0]
            self.assertEqual(view["semantic_name_view"]["host"], "127.0.0.1:18080")
            self.assertEqual(view["semantic_name_view"]["user-agent"], "MiXeD Agent VALUE")
            self.assertEqual(view["evidence"], "ORDERED_FIELD_ITEMS")
            self.assertTrue(view["duplicate_absence_proven"])
            self.assertEqual(result["legacy_dict_only_requests"], 0)

    def test_valid_rehashed_missing_wrong_and_duplicate_host_rejected(self):
        authority = "127.0.0.1:18080"
        variants = [[], [["Accept", "*/*"]],
                    [["host", "127.0.0.1:18081"]], [["Host", "127.0.0.1"]],
                    [["Host", "127.0.0.1:018080"]], [["Host", authority + " "]],
                    [["Host", " " + authority]], [["Host", "LOCALHOST:18080"]],
                    [["Host", "http://" + authority]], [["Host", authority + "/path"]],
                    [["Host", authority], ["host", authority]],
                    [["Host", authority], ["host", "127.0.0.1:18081"]],
                    [["host", "127.0.0.1:18081"], ["Host", authority]],
                    [["Host", authority], ["Host", authority]],
                    [["Host", "127.0.0.1:18081"], ["Host", authority]],
                    [["hoſt", authority]], [["Ho st", authority]], [["Host:", authority]],
                    [["Host", authority], ["Authorization", "not-permitted"]]]
        for items in variants:
            with self.subTest(items=items):
                row = self.row(items)
                # Fully rebuilt JSON/blob digest; raw unsafe=False is deliberately
                # unchanged so the validator must independently reject ambiguity.
                row = qa.load_json(qa.unblob(qa.blob(qa.canonical(row))))
                self.assertFalse(row["requests"][0]["unsafe"])
                with self.assertRaises(qa.HealthFailure):
                    qa.stage_health(row, qa.response_plan(), "http://" + authority)
        row = self.row([["host", authority]])
        row["requests"][0]["headers"] = {"Host": authority}
        with self.assertRaises(qa.HealthFailure):
            qa.stage_health(row, qa.response_plan(), "http://" + authority)

    def test_value_path_query_body_and_authority_are_not_normalized(self):
        origin = "http://127.0.0.1:18080"
        for path in ("/Versions?mode=json&include=all", "/versions?mode=JSON&include=all", "/versions?mode=json&Include=all"):
            row = self.row([["hOst", "127.0.0.1:18080"]])
            row["requests"][0]["path"] = path
            row["requests"][0]["request_line_base64"] = base64.b64encode(("GET " + path + " HTTP/1.1\r\n").encode()).decode()
            with self.assertRaises(qa.HealthFailure):
                qa.stage_health(row, qa.response_plan(), origin)
        row = self.row([["host", "127.0.0.1:18080"]])
        row["requests"][0]["body_sha256"] = row["requests"][0]["body_sha256"].upper()
        with self.assertRaises(ValueError):
            qa.stage_health(row, qa.response_plan(), origin)
        with self.assertRaises(qa.HealthFailure):
            qa.stage_health(self.row([["host", "127.0.0.1:18080"]]), qa.response_plan(), "http://127.0.0.1:18081")
        with self.assertRaises(qa.HealthFailure):
            qa.stage_health(self.row([["host", "127.0.0.1:18080"]]), qa.response_plan())

    def test_dict_only_headers_are_limited_preparation_not_capture_proof(self):
        row = self.row([["host", "127.0.0.1:18080"]])
        del row["requests"][0]["header_items"]
        before = qa.canonical(row)
        with self.assertRaises(qa.HealthFailure):
            qa.stage_health(row, qa.response_plan(), "http://127.0.0.1:18080")
        result = qa.stage_health(row, qa.response_plan(), "http://127.0.0.1:18080", allow_legacy_headers=True)
        self.assertEqual(result["legacy_dict_only_requests"], 1)
        self.assertEqual(result["header_evidence"][0]["evidence"], "DICT_ONLY_LIMITED_PREPARATION")
        self.assertFalse(result["header_evidence"][0]["duplicate_absence_proven"])
        self.assertEqual(qa.canonical(row), before)
        report, frozen = synthetic_capture()
        cold = next(r for r in report["rows"] if r["spec"]["phase"] == "cold")
        del cold["requests"][0]["header_items"]
        with self.assertRaises(qa.HealthFailure):
            qa.validate_capture(report, frozen)
        assessment = qa.validate_capture(report, frozen, preparation=True)
        self.assertEqual(assessment["state"], "PREPARATION_REPLAY_ONLY")
        self.assertEqual(assessment["historical_header_limitations"]["dict_only_request_rows"],
                         [{"ordinal": cold["spec"]["ordinal"], "requests": 1}])
        self.assertFalse(assessment["runtime_acceptance"])

    def test_real_server_raw_items_capture_preserves_duplicates_and_case(self):
        with tempfile.TemporaryDirectory(prefix="module-extension-io-header-unit-") as directory:
            root = Path(directory)
            for name in ("home", "tmp"):
                (root / name).mkdir()
            qa.save_json(root / "routes.json", qa.response_plan())
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            sock.listen(4)
            address = sock.getsockname()
            authority = "127.0.0.1:" + str(address[1])
            origin = "http://" + authority
            process = subprocess.Popen([sys.executable, "-I", str(qa.FIXTURES / "http_server.py"), str(sock.fileno()),
                str(root / "routes.json"), str(root / "requests.jsonl")], cwd=root, env=qa.environment(str(root)),
                pass_fds=(sock.fileno(),), start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ)
                    self.assertTrue(selector.select(timeout=5))
                    ready = process.stdout.readline()
                sock.close()
                variants = [([[name, authority], ["uSeR-aGeNt", "MiXeD Agent Value"]], True)
                            for name in ("Host", "host", "hOsT")]
                variants += [([["Accept", "*/*"]], False), ([["Host", "127.0.0.1:1"]], False),
                             ([["Host", authority], ["host", authority]], False),
                             ([["Host", "127.0.0.1:1"], ["host", authority]], False),
                             ([["Host", authority], ["Host", authority]], False),
                             ([["Host", authority + " "]], False)]
                for items, valid in variants:
                    wire = ("GET /versions?mode=json&include=all HTTP/1.1\r\n" +
                            "".join(name + ": " + value + "\r\n" for name, value in items) + "\r\n").encode("ascii")
                    with socket.create_connection(address, timeout=5) as connection:
                        connection.sendall(wire)
                        chunks = []
                        while True:
                            part = connection.recv(4096)
                            if not part:
                                break
                            chunks.append(part)
                    response = b"".join(chunks)
                    self.assertEqual(int(response.split(b" ", 2)[1]), 200 if valid else 400)
                    self.assertEqual(response.split(b"\r\n\r\n", 1)[1], qa.payloads()["versions"] if valid else b"")
                os.killpg(process.pid, signal.SIGTERM)
                stdout, stderr = process.communicate(timeout=5)
                qa.validate_server_stop({"launched": True, "status": process.returncode, "stdout": qa.blob(ready + stdout),
                    "stderr": qa.blob(stderr), "group_absent": not qa.process_group_exists(process.pid)}, qa.blob(ready), {"origin": origin})
                requests = [qa.load_json(line) for line in (root / "requests.jsonl").read_bytes().split(b"\n") if line]
                self.assertEqual([r["ordinal"] for r in requests], list(range(len(variants))))
                for request, (items, valid) in zip(requests, variants):
                    self.assertEqual(request["header_items"], items)
                    self.assertEqual(request["headers"], dict(items))
                    self.assertEqual(request["header_items_source"], "HTTPMessage.raw_items")
                    self.assertEqual(request["unsafe"], not valid)
                    row, *_ = synthetic_stage(qa.cases()[0], "cold")
                    row["requests"] = [request]
                    if valid:
                        qa.stage_health(row, qa.response_plan(), origin)
                    else:
                        with self.assertRaises(qa.HealthFailure):
                            qa.stage_health(row, qa.response_plan(), origin)
            finally:
                sock.close()
                if process.poll() is None:
                    qa.kill_group(process)
                process.stdout.close()
                process.stderr.close()
            self.assertFalse(qa.process_group_exists(process.pid))
        self.assertFalse(root.exists())


def unit_native_diagnostic(case, source):
    """Synthetic diagnostic layout; not evidence of an executed native command."""
    n = qa.native_collector()
    number, statement = [(i, line) for i, line in enumerate(source.decode().splitlines(), 1)
                         if line.startswith("    value = ctx.")][-1]
    column = statement.index("ctx.") + 1
    digits = len(str(number))
    gutter = " " * (digits + 1) + "|"
    snippet = statement.strip()
    if len(snippet[77:]) > 3:  # independent restatement of starlark_syntax frame.rs
        snippet = snippet[:77] + "..."
    return ("\n".join(["bazel-rs: module extension //:extension.bzl: Traceback (most recent call last):",
        "  File <builtin>, in <module>",
        f"  * //:extension.bzl:{number}, in _impl", "      " + snippet,
        "error: " + n.diagnostic_message(case), " " * digits + f"--> //:extension.bzl:{number}:{column}",
        gutter, f"{number} | {statement}", gutter + " " * column + "^" * (len(statement) - column + 1), gutter]) + "\n").encode()


class CompleteNativeReceiptTests(unittest.TestCase):
    """UNITONLY two-case complete pipeline, never native/Bazel execution evidence."""
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="module-extension-io-native-replay-unit-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root = self.base / "receipt"
        self.worktree = self.base / "unit-worktree"
        self.root.mkdir()
        (self.worktree / "src").mkdir(parents=True)
        self.n = qa.native_collector()
        self.cases = (qa.cases()[0], next(c for c in qa.cases() if c.name == "http-filter-false"))
        self.case_patch = mock.patch.object(qa, "cases", return_value=self.cases)
        self.case_patch.start()
        self.addCleanup(self.case_patch.stop)
        self.root_patch = mock.patch.object(qa, "ROOT", self.worktree)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.native, self.ledger, ledger_value = unit_runtime_v2_tree(self.worktree)
        self.native_sha = qa.file_pin(self.native)["sha256"]
        self.ledger_sha = qa.file_pin(self.ledger)["sha256"]
        _, runtime_pins, binding = self.n.runtime_binding(self.ledger, self.ledger_sha, self.native, self.native_sha, self.worktree)
        verifier = self.worktree / "unit-verifier.py"
        verifier.write_bytes(b"UNITONLY current QA pin\n")
        self.origin = "http://127.0.0.1:18080"
        _, positive, cold_template, _, _ = native_artifact_fixture(self.root, self.cases[0])
        self.plan = self.n.plan(self.root, self.native)
        self.plan[4].update(row_type="command", engine="fixture", argv=["UNITONLY-no-server-executed"])
        fixtures = {}
        for case in self.cases:
            fixtures[case.name] = {name: qa.blob(data) for name, data in qa.render(case, self.origin).items()}
            for name, value in fixtures[case.name].items():
                path = self.root / "inputs/workspaces" / case.name / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(qa.unblob(value))
        pins = {**runtime_pins, str(verifier): qa.file_pin(verifier)}
        self.frozen = {"schema": self.n.SCHEMA, "mode": "native-diagnostic", "scope": "UNITONLY synthetic complete receipt; no runtime acceptance",
            "plan": self.plan, "native": str(self.native), "origin": self.origin, "pins": pins, "runtime_binding": binding,
            "runtime_manifest_path": str(self.ledger), "source_binding_limits": dict(self.n.SOURCE_BINDING_LIMITS),
            "qa_sources": {"unit-verifier.py": qa.file_pin(verifier)}, "fixtures": fixtures, "responses": qa.response_plan(),
            "case_definitions": [qa.asdict(c) for c in self.cases], "deferred_lanes": self.n.deferred_lanes(),
            "aggregate_timeout_seconds": 1200, "scratch_limit_bytes": 1536 * 1024 * 1024, "fs_limits": dict(self.n.FS_LIMITS)}
        checkpoint = {"pins_sha256": qa.digest(qa.canonical(pins)), "runtime_binding": binding, "scratch_bytes": 0, "elapsed_seconds": 0}
        ready = qa.blob(b'{"ready":true,"address":["127.0.0.1",18080]}\n')
        rows, pids, results, requests = [], [], [], []
        for spec in self.plan:
            phase, name = spec["phase"], spec["case"]
            row = {"spec": copy.deepcopy(spec)}
            if phase == "server-start":
                row.update(before=copy.deepcopy(checkpoint), ready=ready, pid=5000000 + spec["ordinal"])
                pids.append(row["pid"])
            elif spec["row_type"] == "command":
                row.update(pid=5000000 + spec["ordinal"], before=copy.deepcopy(checkpoint), after=copy.deepcopy(checkpoint),
                    health={"timeout": False, "signal": None, "spawn_error": None, "truncated": False, "group_absent": True},
                    status=0, stdout=qa.blob(b""), stderr=qa.blob(b""), requests=[])
                pids.append(row["pid"])
                if phase == "version":
                    row["stdout"] = qa.blob(self.n.NATIVE_VERSION)
                else:
                    case = next(c for c in self.cases if c.name == name)
                    source = qa.unblob(fixtures[name]["extension.bzl"])
                    if case.result == "success":
                        row.update(stdout=cold_template["stdout"], artifacts=self.n.observe(self.root, case))
                        frames = b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in qa.expected_events(case))
                        row["stderr"] = qa.blob(frames + (b"bazel-rs: 1 actions executed, 0 cache hits (experimental local execution; not sandboxed)\n"
                            if phase == "cold" else b"bazel-rs: 0 actions executed, 1 cache hits (experimental local execution; not sandboxed)\n"))
                        route = self.frozen["responses"][case.requests[0]]
                        request = {"ordinal": len(requests), "method": "GET", "path": case.requests[0], "peer": "127.0.0.1", "unsafe": False,
                            "headers": {"host": "127.0.0.1:18080"}, "header_items": [["host", "127.0.0.1:18080"]],
                            "status": 200, "response_headers": route["headers"], "body_sha256": route["body"]["sha256"], "body_size": route["body"]["size"]}
                        row["requests"] = [request]
                        requests.append(request)
                    else:
                        # Inert metadata is legitimate; publications are not.
                        metadata = self.root / "scratch/cache" / name / "repositories/metadata/registry.json"
                        metadata.parent.mkdir(parents=True)
                        metadata.write_bytes(b"{}\n")
                        frames = b"".join(qa.MARKER + qa.canonical(e) + b"\n" for e in qa.expected_events(case))
                        row.update(status=1, stderr=qa.blob(frames + unit_native_diagnostic(case, source)), artifacts=self.n.observe(self.root, case))
                    row["artifacts"]["row_ordinal"] = spec["ordinal"]
                    (self.root / "commands" / f'{spec["ordinal"]:04d}').mkdir(parents=True)
                    self.n.retain_cache(self.root, case, phase, row["artifacts"])
                    row["validation"] = self.n.validate_stage(row, spec, case, self.frozen)
                    self.assertEqual(row["validation"]["issues"], [])
                directory = self.root / "commands" / f'{spec["ordinal"]:04d}'
                directory.mkdir(parents=True, exist_ok=True)
                row["raw_directory"] = str(directory)
                for stream in ("stdout", "stderr"):
                    (directory / (stream + ".raw")).write_bytes(qa.unblob(row[stream]))
            elif phase == "case-end":
                row["result"] = {"name": name, "classification": "MATCHED", "errors": []}
                results.append(row["result"])
            elif phase == "server-stop":
                row["server"] = {"launched": True, "status": 0, "group_absent": True, "stdout": qa.blob(qa.unblob(ready) + b"SERVER_CLOSED\n"), "stderr": qa.blob(b"")}
                for stream in ("stdout", "stderr"):
                    (self.root / ("server." + stream + ".raw")).write_bytes(qa.unblob(row["server"][stream]))
            elif phase == "cleanup":
                row["cleanup"] = {"groups": pids, "all_groups_absent": True, "scratch_absent": True}
            elif phase == "pins-end":
                row["pins_sha256"] = qa.digest(qa.canonical(pins))
            if spec["row_type"] == "event" and phase not in ("server-stop", "cleanup", "pins-end"):
                row["checkpoint"] = copy.deepcopy(checkpoint)
            rows.append(row)
        (self.root / "requests.jsonl").write_bytes(b"".join(qa.canonical(r) + b"\n" for r in requests))
        shutil.rmtree(self.root / "scratch")
        qa.save_json(self.root / "freeze.json", self.frozen)
        self.freeze_sha = qa.file_pin(self.root / "freeze.json")["sha256"]
        self.report = {"schema": self.n.SCHEMA, "state": "NATIVE_DIAGNOSTIC_CAPTURE", "scope": self.frozen["scope"],
            "unit_only": True, "native_acceptance": False, "paired_acceptance": False, "uncompleted_cases": [],
            "freeze_sha256": self.freeze_sha, "rows": rows, "cases": results,
            "cleanup": {"groups": pids, "all_groups_absent": True, "scratch_absent": True},
            "counts": {"planned_cases": 2, "captured_cases": 2, "matched": 2, "unmatched": 0,
                       "planned_rows": len(self.plan), "actual_rows": len(rows), "planned_commands": len(pids),
                       "actual_commands": len(pids), "http_requests": len(requests), "deferred_not_executed": 23}}
        self.seal()

    def seal(self):
        qa.save_json(self.root / "report.json", self.report)
        qa.save_json(self.root / "SHA256SUMS.json", self.n.budget_for().inventory(self.root))
        self.args = SimpleNamespace(artifact_dir=str(self.root), archive_sha256=qa.file_pin(self.root / "SHA256SUMS.json")["sha256"],
            freeze_sha256=self.freeze_sha, runtime_manifest=str(self.ledger), runtime_manifest_sha256=self.ledger_sha,
            native_path=str(self.native), native_sha256=self.native_sha)

    def row(self, phase):
        return next(row for row in self.report["rows"] if row["spec"]["phase"] == phase)

    def refresh(self, row):
        retained = self.root / "native-cache" / row["spec"]["case"] / row["spec"]["phase"]
        snapshot = self.n.snapshot_tree(retained, self.n.budget_for(snapshot=True), missing=True)
        row["artifacts"].update(snapshot)
        row["artifacts"]["publication_entries"] = self.n.publication_entries(snapshot)
        qa.save_json(self.root / "commands" / f'{row["spec"]["ordinal"]:04d}' / "native-observation.json", row["artifacts"])
        self.seal()

    def test_complete_unitonly_assess_replay_and_current_qa_binding(self):
        self.assertEqual(self.n.assess(self.report, self.frozen)["counts"]["matched"], 2)
        result = self.n.replay(self.args)
        self.assertFalse(result["native_acceptance"])
        self.assertEqual(result["counts"]["matched"], 2)
        (self.worktree / "unit-verifier.py").write_bytes(b"changed current QA\n")
        with self.assertRaises(ValueError): self.n.replay(self.args)

    def test_recorded_live_link_groups_replay_against_single_link_retained_copies(self):
        row = self.row("cold")
        key = row["artifacts"]["actions"][0]["key"]
        self.assertEqual(len(row["artifacts"]["links"]), 1)
        self.assertIn("actions/" + key + "/outputs/" + self.n.OUTPUT, row["artifacts"]["links"][0])
        retained = self.root / "native-cache" / row["spec"]["case"] / "cold"
        self.assertEqual(self.n.snapshot_tree(retained, self.n.budget_for(snapshot=True))["links"], [])
        self.assertEqual(self.n.replay(self.args)["counts"]["matched"], 2)
        # A rehashed observation whose group aliases the repository tree fails.
        data_file = next(name for name in row["artifacts"]["files"] if name.endswith("/data.json"))
        row["artifacts"]["links"] = [sorted(row["artifacts"]["links"][0] + [data_file])]
        qa.save_json(self.root / "commands" / f'{row["spec"]["ordinal"]:04d}' / "native-observation.json", row["artifacts"])
        self.seal()
        with self.assertRaises((ValueError, self.n.RuntimeViolation)): self.n.replay(self.args)
        # Retained evidence that is itself hardlinked is not an independent copy.
        row["artifacts"]["links"] = []
        qa.save_json(self.root / "commands" / f'{row["spec"]["ordinal"]:04d}' / "native-observation.json", row["artifacts"])
        blob = next(retained / name for name in row["artifacts"]["files"] if name.endswith("-555"))
        output = retained / "actions" / key / "outputs" / self.n.OUTPUT
        output.unlink()
        os.link(blob, output)
        self.seal()
        with self.assertRaises((ValueError, self.n.RuntimeViolation)): self.n.replay(self.args)

    def test_rehashed_negative_orphans_and_malformed_manifest_hard_reject(self):
        row = self.row("negative")
        retained = self.root / "native-cache" / row["spec"]["case"] / "negative"
        for relative, data in (("repositories/direct/trees/empty-orphan", None),
                               ("repositories/direct/trees/orphan/data.json", b"orphan"),
                               ("actions/orphan/outputs/out", b"orphan"),
                               ("repositories/direct/manifests/bad.json", b"not JSON")):
            path = retained / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            if data is None:
                path.mkdir()
            else:
                path.write_bytes(data)
            self.refresh(row)
            with self.assertRaises((ValueError, self.n.RuntimeViolation)):
                self.n.replay(self.args)
            path.rmdir() if path.is_dir() else path.unlink()
            parent = path.parent
            while parent != retained and not any(parent.iterdir()):
                parent.rmdir()
                parent = parent.parent
            self.refresh(row)
        row["artifacts"]["decode_errors"] = [{"error": "malformed manifest"}]
        with self.assertRaises(self.n.RuntimeViolation):
            self.n.validate_stage(row, row["spec"], self.cases[1], self.frozen)

    def test_rehashed_both_entry_digests_cannot_override_retained_bytes(self):
        row = self.row("cold")
        repo = row["artifacts"]["repositories"][0]
        manifest = qa.load_json(qa.unblob(repo["manifest"]))
        manifest["files"]["data.json"]["sha256"] = "0" * 64
        repo["actual_entries"]["data.json"]["sha256"] = "0" * 64
        raw = qa.canonical(manifest)
        repo["manifest"] = qa.blob(raw)
        relative = "repositories/direct/manifests/" + repo["identity"] + ".json"
        retained = self.root / "native-cache" / row["spec"]["case"] / "cold"
        (retained / relative).write_bytes(raw)
        self.refresh(row)
        with self.assertRaises((ValueError, self.n.RuntimeViolation)): self.n.replay(self.args)

    def test_extra_safe_get_with_callback_mismatch_is_hard_failure(self):
        row = self.row("cold")
        row["requests"].append(copy.deepcopy(row["requests"][0]))
        row["stderr"] = qa.blob(qa.unblob(row["stderr"]).replace(qa.MARKER, b"SOURCE_ECHO ", 1))
        with self.assertRaises(self.n.RuntimeViolation):
            self.n.validate_stage(row, row["spec"], self.cases[0], self.frozen)
        requests = []
        for record in self.report["rows"]:
            for request in record.get("requests", []):
                request["ordinal"] = len(requests)
                requests.append(request)
        self.report["counts"]["http_requests"] = len(requests)
        (self.root / "requests.jsonl").write_bytes(b"".join(qa.canonical(r) + b"\n" for r in requests))
        (self.root / "commands" / f'{row["spec"]["ordinal"]:04d}' / "stderr.raw").write_bytes(qa.unblob(row["stderr"]))
        self.seal()
        with self.assertRaises(self.n.RuntimeViolation): self.n.replay(self.args)

    def test_retained_directory_mode_is_measured_not_invented(self):
        row = self.row("cold")
        repo = row["artifacts"]["repositories"][0]
        prefix = "repositories/direct/trees/" + repo["identity"]
        row["artifacts"]["directories"][prefix] = "0700"
        repo["actual_entries"][""]["mode"] = 0o700
        qa.save_json(self.root / "commands" / f'{row["spec"]["ordinal"]:04d}' / "native-observation.json", row["artifacts"])
        self.seal()
        with self.assertRaises((ValueError, self.n.RuntimeViolation)): self.n.replay(self.args)

    def test_coherent_wrong_bytes_still_fail_independent_fixture_oracle(self):
        row = self.row("cold")
        retained = self.root / "native-cache" / row["spec"]["case"] / "cold"
        repo = row["artifacts"]["repositories"][0]
        action = row["artifacts"]["actions"][0]
        prefix = "repositories/direct/trees/" + repo["identity"]
        bad = qa.canonical([{"not": "the frozen fixture result"}])
        (retained / prefix / "data.json").write_bytes(bad)
        output = retained / "actions" / action["key"] / "outputs" / self.n.OUTPUT
        output.chmod(0o644)
        output.write_bytes(bad)
        output.chmod(0o555)
        entries = self.n.tree_entries(retained / prefix)
        manifest = qa.load_json(qa.unblob(repo["manifest"]))
        manifest["files"] = entries
        repo.update(actual_entries=entries, manifest=qa.blob(qa.canonical(manifest)))
        (retained / "repositories/direct/manifests" / (repo["identity"] + ".json")).write_bytes(qa.unblob(repo["manifest"]))
        action_manifest = qa.load_json(qa.unblob(action["manifest"]))
        action_manifest["outputs"][0]["digest"] = qa.digest(bad)
        action["manifest"] = qa.blob(qa.canonical(action_manifest))
        (retained / "actions" / action["key"] / "manifest.json").write_bytes(qa.unblob(action["manifest"]))
        self.refresh(row)
        with self.assertRaises(self.n.RuntimeViolation): self.n.replay(self.args)


    def test_positive_discovery_rejects_hidden_second_valid_action_and_repo(self):
        row = self.row("cold")
        retained = self.root / "native-cache" / row["spec"]["case"] / "cold"
        original = copy.deepcopy(row["artifacts"])
        extra = "c" * 64
        for kind in ("action", "repository"):
            with self.subTest(kind=kind):
                if kind == "action":
                    old = original["actions"][0]
                    source = retained / "actions" / old["key"]
                    target = retained / "actions" / extra
                    shutil.copytree(source, target)
                    manifest = qa.load_json((target / "manifest.json").read_bytes())
                    manifest["key"] = extra
                    qa.save_json(target / "manifest.json", manifest)
                else:
                    old = original["repositories"][0]
                    source = retained / "repositories/direct/trees" / old["identity"]
                    target = retained / "repositories/direct/trees" / extra
                    shutil.copytree(source, target)
                    manifest = qa.load_json(qa.unblob(old["manifest"]))
                    manifest["identity"] = extra
                    qa.save_json(retained / "repositories/direct/manifests" / (extra + ".json"), manifest)
                # Rebuild actual files, directory modes, publication accounting,
                # observation JSON and the archive hash; keep reported lists old.
                self.refresh(row)
                self.assertEqual(len(row["artifacts"]["actions"]), 1)
                self.assertEqual(len(row["artifacts"]["repositories"]), 1)
                with self.assertRaises((ValueError, self.n.RuntimeViolation)):
                    self.n.replay(self.args)
                # Even an honest complete discovery list cannot authorize a
                # second publication for this one-repository/one-action fixture.
                row["artifacts"].update(self.n.discover_publications(row["artifacts"], row["artifacts"]["cache_root"]))
                self.refresh(row)
                with self.assertRaises((ValueError, self.n.RuntimeViolation)):
                    self.n.replay(self.args)
                shutil.rmtree(target)
                if kind == "repository":
                    (retained / "repositories/direct/manifests" / (extra + ".json")).unlink()
                row["artifacts"] = copy.deepcopy(original)
                self.refresh(row)

    def test_positive_orphan_output_tree_and_malformed_manifest_rejected(self):
        row = self.row("cold")
        retained = self.root / "native-cache" / row["spec"]["case"] / "cold"
        original = copy.deepcopy(row["artifacts"])
        key = original["actions"][0]["key"]
        variants = [("repositories/direct/trees/orphan", None),
                    ("actions/" + key + "/outputs/undeclared.txt", b"orphan"),
                    ("actions/orphan/manifest.json", b"not-json"),
                    ("repositories/direct/manifests/orphan.json", b"not-json")]
        for relative, data in variants:
            with self.subTest(relative=relative):
                path = retained / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                if data is None: path.mkdir()
                else: path.write_bytes(data)
                self.refresh(row)
                with self.assertRaises((ValueError, self.n.RuntimeViolation)): self.n.replay(self.args)
                row["artifacts"].update(self.n.discover_publications(row["artifacts"], row["artifacts"]["cache_root"]))
                self.refresh(row)
                with self.assertRaises((ValueError, self.n.RuntimeViolation)): self.n.replay(self.args)
                path.rmdir() if path.is_dir() else path.unlink()
                parent = path.parent
                while parent != retained and not any(parent.iterdir()):
                    parent.rmdir()
                    parent = parent.parent
                row["artifacts"] = copy.deepcopy(original)
                self.refresh(row)


class NativeFilesystemBudgetTests(unittest.TestCase):
    def test_incremental_entry_byte_depth_and_deadline_limits(self):
        n = qa.native_collector()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index in range(6):
                (root / str(index)).write_bytes(b"1234")
            budget = n.FSBudget({**n.FS_LIMITS, "entries": 2})
            with self.assertRaisesRegex(n.RuntimeViolation, "entry limit"):
                budget.files(root)
            self.assertEqual(budget.entries, 3)  # stop before enumerating the rest
            budget = n.FSBudget({**n.FS_LIMITS, "total_bytes": 3})
            with self.assertRaisesRegex(n.RuntimeViolation, "byte limit"):
                budget.files(root)
            (root / "a/b/c").mkdir(parents=True)
            with self.assertRaisesRegex(n.RuntimeViolation, "depth"):
                n.FSBudget({**n.FS_LIMITS, "depth": 1}).files(root / "a")
            with self.assertRaisesRegex(n.RuntimeViolation, "deadline"):
                n.FSBudget(clock=lambda: 2, deadline=1).files(root)
            with self.assertRaisesRegex(n.RuntimeViolation, "read-size"):
                n.FSBudget({**n.FS_LIMITS, "file_bytes": 2}).read(root / "0")

    def test_deadline_checked_after_directory_scan_and_read_growth(self):
        n = qa.native_collector()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            clock = [0]
            original_scandir = os.scandir
            class ExpiringIterator:
                def __init__(self, path): self.inner = original_scandir(path)
                def __enter__(self): return self
                def __exit__(self, *_): self.inner.close()
                def __iter__(self): return self
                def __next__(self):
                    try: return next(self.inner)
                    except StopIteration:
                        clock[0] = 2
                        raise
            with mock.patch.object(n.os, "scandir", side_effect=ExpiringIterator):
                with self.assertRaisesRegex(n.RuntimeViolation, "deadline"):
                    n.FSBudget(clock=lambda: clock[0], deadline=1).files(root)
            path = root / "growing"
            path.write_bytes(b"abc")
            original_read = os.read
            changed = [False]
            def grow(fd, count):
                result = original_read(fd, count)
                if not changed[0]:
                    changed[0] = True
                    with path.open("ab") as stream: stream.write(b"extra")
                return result
            with mock.patch.object(n.os, "read", side_effect=grow):
                with self.assertRaisesRegex(n.RuntimeViolation, "growth|changed"):
                    n.FSBudget().read(path)

    def test_no_symlink_traversal_and_explicit_limits_cannot_expand(self):
        n = qa.native_collector()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "real").mkdir()
            (root / "real/data").write_bytes(b"bounded")
            (root / "link").symlink_to(root / "real", target_is_directory=True)
            with self.assertRaises(n.RuntimeViolation): n.FSBudget().read(root / "link/data")
            with self.assertRaises(n.RuntimeViolation): n.FSBudget().files(root)
        with self.assertRaises(ValueError):
            n.budget_for({"fs_limits": {**n.FS_LIMITS, "entries": n.FS_LIMITS["entries"] + 1}})


class NativeInitializationOwnershipTests(unittest.TestCase):
    def test_copy_freeze_and_interruption_faults_remove_only_owned_scratch(self):
        n = qa.native_collector()
        with tempfile.TemporaryDirectory(prefix="module-extension-io-owner-unit-") as directory:
            parent = Path(directory)
            registry = parent / "registry"
            registry.mkdir()
            (registry / "bazel_registry.json").write_bytes(b"{}\n")
            ledger = parent / "unit-ledger.json"
            ledger.write_bytes(b"{}\n")
            outside = parent / "outside"
            outside.mkdir()
            (outside / "keep").write_bytes(b"must survive")
            for kind in ("copy", "freeze", "interrupt"):
                root = parent / ("module-extension-io-native-" + kind)
                args = SimpleNamespace(artifact_dir=str(root), runtime_manifest=str(ledger), runtime_manifest_sha256="unit",
                    native="/unit-not-executed", native_sha256="unit", registry_input=str(registry), timeout=1)
                fake_binding = ({"worktree": str(qa.ROOT), "source_commit": "UNITONLY"}, {}, {"scope": "UNITONLY initialization fault; runtime binding not exercised"})
                real_save = qa.save_json
                def fail_freeze(path, value):
                    if Path(path).name == "freeze.json":
                        raise OSError("injected freeze write failure")
                    return real_save(path, value)
                def fail_copy(*_):
                    (root / "scratch/link").symlink_to(outside, target_is_directory=True)
                    if kind == "interrupt": raise KeyboardInterrupt("injected initialization interruption")
                    raise OSError("injected copy failure")
                with mock.patch.object(n, "runtime_binding", return_value=fake_binding):
                    with self.assertRaises((OSError, KeyboardInterrupt)):
                        with n.OwnedRoot(root, parent=parent) as owner:
                            sock = socket.socket()
                            sock.bind(("127.0.0.1", 0))
                            owner.sock = sock
                            if kind == "freeze":
                                with mock.patch.object(qa, "save_json", side_effect=fail_freeze):
                                    n.freeze_inputs(args, sock, owner)
                            else:
                                with mock.patch.object(n, "copy_input", side_effect=fail_copy):
                                    n.freeze_inputs(args, sock, owner)
                self.assertFalse((root / "scratch").exists())
                self.assertEqual((outside / "keep").read_bytes(), b"must survive")
                receipt = qa.load_json((root / "initialization-failure.json").read_bytes())
                self.assertTrue(receipt["cleanup"]["scratch_absent"])
                self.assertTrue(receipt["cleanup"]["all_groups_absent"])
                self.assertEqual(owner.sock.fileno(), -1)

    def test_existing_or_symlink_root_never_becomes_cleanup_owned(self):
        n = qa.native_collector()
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            root = parent / "module-extension-io-native-existing"
            root.mkdir()
            (root / "keep").write_bytes(b"existing")
            with self.assertRaises(ValueError):
                with n.OwnedRoot(root, parent=parent): pass
            link = parent / "module-extension-io-native-link"
            link.symlink_to(root, target_is_directory=True)
            with self.assertRaises(ValueError):
                with n.OwnedRoot(link, parent=parent): pass
            self.assertEqual((root / "keep").read_bytes(), b"existing")


class RuntimeCoverageV2Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="module-extension-io-coverage-unit-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.n = qa.native_collector()
        self.native, self.path, self.ledger = unit_runtime_v2_tree(self.root)
        self.native_sha = self.ledger["native_sha256"]

    def bind(self, ledger=None, path=None):
        path = path or self.path
        if ledger is not None:
            qa.save_json(path, ledger)
        return self.n.runtime_binding(path, qa.file_pin(path)["sha256"], self.native, self.native_sha, self.root)

    def test_complete_minimal_policy_and_seven_authorized_support_files(self):
        expected = set(self.n.REQUIRED_RUNTIME_FILES) | {"src/unit.rs", "vendor/unit.txt", "resources/unit.dat", "tests/unit.rs"}
        self.assertEqual(self.n.enumerate_runtime_sources(self.root), expected)
        old_bytes = self.path.read_bytes()
        self.bind()
        names = ["extension_io_preflight_cases.rs", "extension_io_boundary_cli.rs", "extension_io_boundary_unit.rs",
                 "extension_io_boundary_context.rs", "extension_io_boundary_provenance.rs", "extension_io_bounded_tls.rs",
                 "extension_io_callback_budget.rs"]
        (self.root / "tests/support").mkdir()
        new = copy.deepcopy(self.ledger)
        for name in names:
            relative = "tests/support/" + name
            (self.root / relative).write_bytes(("UNITONLY support " + name).encode())
            new["runtime_sources"][relative] = qa.file_pin(self.root / relative)["sha256"]
        with self.assertRaises(ValueError): self.bind()
        new["runtime_source_count"] = len(new["runtime_sources"])
        _, _, binding = self.bind(new, self.root / "authorized-next-ledger.json")
        self.assertEqual(binding["source_count"], len(expected) + len(names))
        self.assertEqual(binding["sources_sha256"], qa.digest(qa.canonical(new["runtime_sources"])))
        self.assertEqual(self.path.read_bytes(), old_bytes)

    def test_missing_mandatory_equal_count_wrong_set_and_adjusted_omission(self):
        omitted = copy.deepcopy(self.ledger)
        del omitted["runtime_sources"]["src/unit.rs"]
        omitted["runtime_source_count"] -= 1
        with self.assertRaises(ValueError): self.bind(omitted)
        wrong = copy.deepcopy(self.ledger)
        del wrong["runtime_sources"]["src/unit.rs"]
        (self.root / "not-reviewed.dat").write_bytes(b"UNITONLY")
        wrong["runtime_sources"]["not-reviewed.dat"] = qa.file_pin(self.root / "not-reviewed.dat")["sha256"]
        self.assertEqual(len(wrong["runtime_sources"]), self.ledger["runtime_source_count"])
        with self.assertRaises(ValueError): self.bind(wrong)
        (self.root / ".envrc").unlink()
        with self.assertRaises(FileNotFoundError): self.bind(self.ledger)

    def test_bad_counts_policy_and_old_unversioned_ledger_fail_closed(self):
        for count in (True, False, 0, -1, "8", self.ledger["runtime_source_count"] + 1):
            bad = {**self.ledger, "runtime_source_count": count}
            with self.subTest(count=count), self.assertRaises(ValueError): self.bind(bad)
        for policy in ({"name": "bazel-rs-runtime-inputs", "version": 2}, {"name": "other", "version": 1}, None):
            with self.assertRaises(ValueError): self.bind({**self.ledger, "coverage_policy": policy})
        old = {key: value for key, value in self.ledger.items() if key not in ("schema", "coverage_policy", "runtime_source_count")}
        with self.assertRaisesRegex(ValueError, "historical only"):
            self.bind(old)
        self.assertNotIn("schema", qa.load_json(self.path.read_bytes()))  # never synthesized/upgraded

    def test_duplicate_traversal_python_and_noncanonical_hash_rejected(self):
        pair = json.dumps("src/unit.rs") + ":" + json.dumps(self.ledger["runtime_sources"]["src/unit.rs"])
        raw = qa.canonical(self.ledger).replace(pair.encode(), (pair + "," + pair).encode())
        self.path.write_bytes(raw)
        with self.assertRaisesRegex(ValueError, "duplicate JSON"):
            self.bind()
        for name in ("../escape.rs", "/absolute.rs", "src/../unit.rs", "src//unit.rs", "src\\unit.rs", "src/hidden.py"):
            bad = copy.deepcopy(self.ledger)
            bad["runtime_sources"][name] = "0" * 64
            bad["runtime_source_count"] = len(bad["runtime_sources"])
            with self.subTest(name=name), self.assertRaises(ValueError): self.bind(bad)
        bad = copy.deepcopy(self.ledger)
        bad["runtime_sources"]["src/unit.rs"] = bad["runtime_sources"]["src/unit.rs"].upper()
        with self.assertRaises(ValueError): self.bind(bad)
        with self.assertRaises(ValueError):
            self.bind({**self.ledger, "reviewed_additional_files": ["extra.inc", "extra.inc"]})

    def test_symlinks_and_missing_mandatory_roots_are_not_omitted(self):
        outside = self.root / "outside.rs"
        outside.write_bytes(b"UNITONLY outside")
        (self.root / "tests/support").mkdir()
        link = self.root / "tests/support/linked.rs"
        link.symlink_to(outside)
        with self.assertRaises(self.n.RuntimeViolation): self.bind()
        link.unlink()
        shutil.rmtree(self.root / "vendor")
        with self.assertRaises(FileNotFoundError): self.bind()

    def test_reviewed_additional_roots_are_complete_and_files_sha_bound(self):
        (self.root / "support-assets/nested").mkdir(parents=True)
        (self.root / "support-assets/a.dat").write_bytes(b"first")
        (self.root / "support-assets/nested/b.dat").write_bytes(b"second")
        (self.root / "reviewed.inc").write_bytes(b"explicit")
        value = copy.deepcopy(self.ledger)
        value["reviewed_additional_roots"] = ["support-assets"]
        value["reviewed_additional_files"] = ["reviewed.inc"]
        for name in ("support-assets/a.dat", "support-assets/nested/b.dat", "reviewed.inc"):
            value["runtime_sources"][name] = qa.file_pin(self.root / name)["sha256"]
        value["runtime_source_count"] = len(value["runtime_sources"])
        self.bind(value)
        (self.root / "support-assets/nested/unlisted.dat").write_bytes(b"new")
        with self.assertRaises(ValueError): self.bind()
        (self.root / "support-assets/nested/unlisted.dat").unlink()
        (self.root / "support-assets/qa.py").write_bytes(b"not runtime coverage")
        with self.assertRaises(self.n.RuntimeViolation): self.bind()

    def test_optional_build_and_cargo_inputs_and_ignored_python(self):
        (self.root / ".cargo").mkdir()
        (self.root / ".cargo/config.toml").write_bytes(b"UNITONLY")
        (self.root / "build.rs").write_bytes(b"UNITONLY")
        (self.root / "src/not-runtime.py").write_bytes(b"UNITONLY Python excluded")
        (self.root / "tests/not-rust.txt").write_bytes(b"UNITONLY unreviewed data")
        actual = self.n.enumerate_runtime_sources(self.root)
        self.assertIn(".cargo/config.toml", actual)
        self.assertIn("build.rs", actual)
        self.assertNotIn("src/not-runtime.py", actual)
        self.assertNotIn("tests/not-rust.txt", actual)
        with self.assertRaises(ValueError): self.bind()
        value = copy.deepcopy(self.ledger)
        for name in (".cargo/config.toml", "build.rs"):
            value["runtime_sources"][name] = qa.file_pin(self.root / name)["sha256"]
        value["runtime_source_count"] = len(value["runtime_sources"])
        self.bind(value)

    def test_membership_drift_during_binding_and_after_freeze_rejected(self):
        _, pins, binding = self.bind()
        frozen = {"runtime_binding": binding, "runtime_manifest_path": str(self.path), "pins": pins}
        with mock.patch.object(qa, "ROOT", self.root):
            self.n.verify_frozen_membership(frozen)
            (self.root / "tests/support").mkdir()
            new_path = self.root / "tests/support/unlisted.rs"
            new_path.write_bytes(b"UNITONLY new mandatory input")
            with self.assertRaises(ValueError): self.n.verify_frozen_membership(frozen)
            new_path.unlink()
        original = self.n.FSBudget.pin
        changed = [False]
        def add_during_hash(budget, path):
            result = original(budget, path)
            if Path(path) == self.root / "src/unit.rs" and not changed[0]:
                changed[0] = True
                new_path.write_bytes(b"UNITONLY added during hash pass")
            return result
        with mock.patch.object(self.n.FSBudget, "pin", add_during_hash):
            with self.assertRaises(ValueError): self.bind()

    def test_source_bounds_not_captured_artifact_limit_and_enumeration_is_bounded(self):
        path = self.root / "resources/over-artifact-cap.bin"
        path.write_bytes(b"x" * (self.n.FS_LIMITS["artifact_file_bytes"] + 1))
        value = copy.deepcopy(self.ledger)
        value["runtime_sources"][str(path.relative_to(self.root))] = qa.file_pin(path)["sha256"]
        value["runtime_source_count"] = len(value["runtime_sources"])
        self.bind(value)  # source file is bounded by64MiB, not publication8MiB
        with self.assertRaises(self.n.RuntimeViolation): self.n.snapshot_file(path, self.root)
        budget = self.n.FSBudget({**self.n.SOURCE_BINDING_LIMITS, "entries": 2})
        with self.assertRaises(self.n.RuntimeViolation):
            self.n.enumerate_runtime_sources(self.root, budget=budget)
        self.assertEqual(budget.entries, 3)


if __name__ == "__main__":
    unittest.main()
