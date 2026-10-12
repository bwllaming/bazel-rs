"""Harness seams only: no native or official Bazel binary is launched."""

import argparse
import base64
import copy
import gzip
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
from urllib.error import HTTPError
from urllib.request import urlopen
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import validate_remote as remote


def record(stdout="", stderr="", returncode=0, **extra):
    return {"id": 0, "name": "mock", "stdout": stdout, "stderr": stderr,
            "returncode": returncode, "timed_out": False, **extra}


class FixtureTests(unittest.TestCase):
    def test_archives_are_deterministic_and_contain_binary_and_distinct_module(self):
        for kind in remote.FORMATS:
            with self.subTest(kind=kind):
                data = remote.archive_bytes(kind)
                self.assertEqual(data, remote.archive_bytes(kind))
                if kind == "zip":
                    with zipfile.ZipFile(io.BytesIO(data)) as archive:
                        files = {name: archive.read(name) for name in archive.namelist()}
                        self.assertTrue(all(item.date_time == (1980, 1, 1, 0, 0, 0) for item in archive.infolist()))
                else:
                    if kind == "tar.gz":
                        self.assertEqual(data[4:8], b"\0" * 4)
                        data = gzip.decompress(data)
                    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                        files = {item.name: archive.extractfile(item).read() for item in archive}
                self.assertEqual(files["pkg/nested/raw.bin"], bytes(range(256)))
                self.assertEqual(files["pkg/BUILD.bazel"], remote.BUILD)
                self.assertNotEqual(files["pkg/MODULE.bazel"], remote.MODULE)
                self.assertNotEqual(files["pkg/data.bin"], remote.DATA)
        with tarfile.open(fileobj=io.BytesIO(remote.archive_bytes("tar", hostile=True))) as archive:
            self.assertIn("../escape", archive.getnames())

    def test_real_http_allowlist_logs_status_hash_and_shutdown_join(self):
        report = {"requests": []}
        server = remote.FixtureServer(report)
        fixture = server.install("/test", "zip")
        server.start()
        try:
            server.phase = "unit.fetch"
            with urlopen(fixture["registry"] + "/modules/dep/1.0/source.json", timeout=3) as response:
                source = json.load(response)
            with urlopen(source["url"], timeout=3) as response:
                data = response.read()
            self.assertEqual(remote.sri(data), source["integrity"])
            self.assertEqual(remote.sha256(data), fixture["archive_sha256"])
            self.assertEqual(source["overlay"]["MODULE.bazel"], remote.sri(remote.OVERLAY_MODULE))
            self.assertFalse(any("bazel_tools" in path for path in server.routes))
            with self.assertRaises(HTTPError) as error:
                urlopen(server.url + "/../../etc/passwd", timeout=3)
            self.assertEqual(error.exception.code, 404)
            error.exception.close()
        finally:
            receipt = server.close()
        self.assertEqual([r["status"] for r in report["requests"]], [200, 200, 404])
        self.assertTrue(all(r["phase"] == "unit.fetch" for r in report["requests"]))
        self.assertEqual(report["requests"][1]["sha256"], remote.sha256(data))
        self.assertTrue(receipt["closed"] and receipt["shutdown"] and receipt["joined"])
        self.assertFalse(server.thread.is_alive())
        self.assertEqual(server.httpd.socket.fileno(), -1)

    def test_skipped_zip_crc_is_corrupt_but_source_integrity_and_selected_files_are_valid(self):
        server = remote.FixtureServer({"requests": []})
        try:
            fixture = server.install("/crc", kind="zip", skipped_crc=True)
            data = server.routes[fixture["archive_path"]][1]
            self.assertEqual(remote.sri(data), fixture["source"]["integrity"])
            self.assertEqual(remote.sha256(data), fixture["archive_sha256"])
            with zipfile.ZipFile(io.BytesIO(data)) as archive:
                self.assertEqual(archive.read("pkg/BUILD.bazel"), remote.BUILD)
                self.assertEqual(archive.read("pkg/nested/raw.bin"), remote.RAW)
                self.assertTrue(archive.read("pkg/data.bin"))
                with self.assertRaisesRegex(zipfile.BadZipFile, "CRC"):
                    archive.read("other/skipped.bin")
            # XORing the central CRC again restores a completely readable ZIP;
            # the payload/local header and all wanted members were untouched.
            repaired = remote.corrupt_zip_crc(data, "other/skipped.bin")
            offsets = [index for index, (a, b) in enumerate(zip(data, repaired)) if a != b]
            self.assertEqual(len(offsets), 1)
            self.assertEqual(data[offsets[0] - 16:offsets[0] - 12], b"PK\x01\x02")
            with zipfile.ZipFile(io.BytesIO(repaired)) as archive:
                self.assertIsNone(archive.testzip())
            with self.assertRaisesRegex(ValueError, "member not found"):
                remote.corrupt_zip_crc(data, "absent")
        finally:
            server.close()


class ParsingTests(unittest.TestCase):
    def test_explicit_alias_mapping_and_native_canonical_requirement(self):
        expected = ["@@dep+//:copy", "@@dep+//:sources"]
        self.assertEqual(remote.parse_labels("@remote//:sources\n@remote//:copy\n"), expected)
        self.assertEqual(remote.parse_labels("\n".join(expected), native=True), expected)
        for text, native in (("@remote//:copy", True), ("@@other+//:copy", False),
                             ("//:copy", False), ("@dep//:copy", False),
                             ("@remote//:copy\n@@dep+//:copy", False), ("not a label", False)):
            with self.subTest(text=text), self.assertRaises(ValueError):
                remote.parse_labels(text, native=native)

    def test_native_paths_reject_wrong_identity_escape_and_duplicates(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            scratch = Path(directory)
            path = scratch / "file.bin"
            path.write_bytes(remote.DATA)
            good = "@@dep+//:copy\t" + str(path)
            self.assertEqual(remote.native_paths(record(good), "@remote//:copy", scratch), [path])
            for text in (good + "\n" + good, good.replace("@@dep+", "@remote"),
                         good.replace("//:copy", "//:sources"), "@@dep+//:copy\trelative",
                         "@@dep+//:copy\t/etc/passwd", "@@dep+//:copy\t" + directory + "/../bad"):
                with self.subTest(text=text), self.assertRaises(ValueError):
                    remote.native_paths(record(text), "@remote//:copy", scratch)
            link = scratch / "escape"
            link.symlink_to("/etc/passwd")
            with self.assertRaisesRegex(ValueError, "unsafe"):
                remote.native_paths(record("@@dep+//:copy\t" + str(link)), "@remote//:copy", scratch)

    def test_manifests_require_full_inventory_identity_and_exact_binary_bytes(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            source = root / "repo"
            source.mkdir()
            paths = []
            for relative, data in remote.EXPECTED_SOURCES.items():
                path = source / relative
                path.parent.mkdir(exist_ok=True)
                path.write_bytes(data)
                paths.append(path)
            evidence = remote.manifest(paths, remote.EXPECTED_SOURCES, source)
            self.assertEqual(base64.b64decode(evidence["nested/raw.bin"]["bytes_base64"]), bytes(range(256)))
            for bad in (paths[:-1], paths + paths[:1], [root / "outside"]):
                with self.assertRaises(ValueError):
                    remote.manifest(bad, remote.EXPECTED_SOURCES, source)
            paths[0].write_bytes(b"wrong")
            with self.assertRaisesRegex(ValueError, "unexpected output bytes"):
                remote.manifest(paths, remote.EXPECTED_SOURCES, source)
            for config in ("rust-fastbuild", "k8-fastbuild"):
                generated = root / "bazel-out" / config / "bin/external/dep+/copy.bin"
                generated.parent.mkdir(parents=True)
                generated.write_bytes(remote.DATA)
                self.assertEqual(set(remote.manifest([generated], {"copy.bin": remote.DATA})), {"copy.bin"})
            for suffix in ("unknown/bin/external/dep+/copy.bin", "k8-fastbuild/bin/external/other+/copy.bin",
                           "k8-fastbuild/bin/copy.bin"):
                with self.assertRaises(ValueError):
                    remote.manifest([root / "bazel-out" / suffix], {"copy.bin": remote.DATA})

    def test_rejection_never_accepts_success_crash_timeout_or_unrelated_failure(self):
        good = record(stderr="sha256 integrity mismatch", returncode=1)
        remote.require_rejection(good, "integrity.*mismatch")
        for changes in ({"returncode": 0}, {"returncode": -9}, {"returncode": None},
                        {"returncode": 101}, {"returncode": 139}, {"timed_out": True},
                        {"stdout": "@@dep+//:copy"}, {"spawn_error": "missing"},
                        {"stderr": "panicked: integrity mismatch"}, {"stderr": "unsupported registry"}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                remote.require_rejection(dict(good, **changes), "integrity.*mismatch")

    def test_no_published_tree_detects_manifest_and_stage_leaks(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            cache = Path(directory)
            remote.no_published_tree(cache)
            trees = cache / "repositories/trees"
            trees.mkdir(parents=True)
            for name in ("key.json", ".stage-leftover"):
                path = trees / name
                path.write_bytes(b"leaked")
                with self.assertRaisesRegex(ValueError, "left published/staged"):
                    remote.no_published_tree(cache)
                path.unlink()


class InvocationTests(unittest.TestCase):
    def test_flags_empty_registry_default_and_reference_genuine_builtins(self):
        args = remote.argument_parser().parse_args([])
        self.assertEqual(args.report.name, "remote-validation.json")
        for value in ("0", "-1", "nan", "inf"):
            with self.assertRaises(argparse.ArgumentTypeError):
                remote.positive_seconds(value)
        argv = remote.native_command(Path("/native"), Path("/cache"), "mod", None)
        self.assertEqual(argv[:5], ["/native", "--ignore_all_rc_files", "--cache-dir=/cache", "mod", "graph"])
        self.assertIn("--lockfile_mode=off", argv)
        self.assertFalse(any(arg.startswith("--registry=") for arg in argv))
        runner = mock.Mock()
        reference = remote.RemoteReference(Path("/official"), Path("/fixture"), Path("/scratch"), runner, "remote")
        reference.registry = "http://127.0.0.1:12345/registry"
        reference.run("q", "query", "@remote//:all")
        argv = runner.run.call_args.args[1]
        self.assertEqual([arg for arg in argv if arg.startswith("--registry=")],
                         ["--registry=" + reference.registry, "--registry=https://bcr.bazel.build"])
        self.assertIn("--output_base=/scratch/remote-output-base", argv)
        self.assertFalse(any("override_" in arg or "repository_cache=" in arg for arg in argv))

    def test_pinned_binary_hash_required_even_after_elf_validation(self):
        args = remote.argument_parser().parse_args([])
        with mock.patch.object(remote, "validate_executable"), \
                mock.patch.object(remote, "file_evidence", return_value={"sha256": "wrong"}):
            with self.assertRaisesRegex(ValueError, "reference SHA256"):
                remote.validate_binaries(args)

    def test_runner_hashes_exact_output_even_on_exception(self):
        report = {"runs": [], "requests": []}
        runner = remote.RecordingRunner(report, 1)
        runner.server = mock.Mock()

        def shared_run(*args, **kwargs):
            report["runs"].append(record(stdout="replacement", stdout_base64=base64.b64encode(b"\xff\0").decode()))
            report["requests"].append({"path": "/fixture"})
            raise RuntimeError("injected runner error")

        with mock.patch.object(remote.Runner, "run", side_effect=shared_run), self.assertRaises(RuntimeError):
            runner.run("failing", ["unused"], Path("/unused"))
        self.assertEqual(report["runs"][0]["stdout_sha256"], remote.sha256(b"\xff\0"))
        self.assertEqual(report["runs"][0]["request_range"], [0, 1])
        self.assertEqual(runner.server.phase, "idle")


class RegressionProbeTests(unittest.TestCase):
    def url_policy_probe(self, scratch, fault=None):
        server = remote.FixtureServer({"requests": []})
        runner = mock.Mock()
        calls = []
        source_path = "/warm-blob-url-policy/registry/modules/dep/1.0/source.json"
        module_path = source_path.replace("source.json", "MODULE.bazel")
        archive_path = "/warm-blob-url-policy/source.tar"

        def run(name, argv, workspace):
            cache = Path(next(arg.split("=", 1)[1] for arg in argv if arg.startswith("--cache-dir=")))
            source = json.loads(server.routes[source_path][1])
            calls.append({"name": name, "cache": cache, "source": source, "argv": argv})
            server.requests.extend({"path": path} for path in (module_path, source_path))
            if name.endswith("warm-up"):
                server.requests.append({"path": archive_path})
                archive = server.routes[archive_path][1]
                blob = cache / "repositories/downloads/sha256" / remote.sha256(archive)
                blob.parent.mkdir(parents=True)
                blob.write_bytes(archive)
                root = cache / "repositories/trees/existing"
                for relative, data in remote.EXPECTED_SOURCES.items():
                    path = root / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(data)
                (root.parent / "existing.json").write_bytes(b"original manifest")
                return record("@@dep+//:copy\n@@dep+//:sources\n")
            if fault == "successful-query":
                return record("@@dep+//:copy\n@@dep+//:sources\n")
            if fault == "archive-fetch":
                server.requests.append({"path": archive_path})
            if fault == "new-tree":
                (cache / "repositories/trees/leaked").mkdir(parents=True)
            if fault == "changed-manifest":
                (cache / "repositories/trees/existing.json").write_bytes(b"modified manifest")
            if fault == "missing-source-request":
                server.requests.pop()
            return record(stderr="source URLs must not contain credentials", returncode=1)

        runner.run.side_effect = run
        try:
            details = remote.native_probe(remote.argument_parser().parse_args([]), runner, scratch,
                                          scratch / "workspace", server, "warm-blob-url-policy")
            return details, calls
        finally:
            server.close()

    def test_warm_and_cold_url_policy_use_same_sri_and_require_no_new_trees_or_fetches(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            details, calls = self.url_policy_probe(Path(directory))
        self.assertEqual([call["name"].rsplit(".", 1)[1] for call in calls], ["warm-up", "warm", "cold"])
        self.assertEqual(calls[0]["cache"], calls[1]["cache"])
        self.assertNotEqual(calls[1]["cache"], calls[2]["cache"])
        self.assertEqual(len({call["source"]["integrity"] for call in calls}), 1)
        self.assertEqual(calls[1]["source"]["url"], "http://@127.0.0.1:9/source.tar")
        self.assertEqual(calls[1]["source"], calls[2]["source"])
        self.assertTrue(all("query" in call["argv"] and "--lockfile_mode=off" in call["argv"] for call in calls))
        self.assertEqual([item["stage"] for item in details["rejections"]], ["warm", "cold"])
        self.assertTrue(details["warm_up"]["blob"]["sha256"])
        self.assertTrue(all(item["fixture_archive_fetches"] == 0 for item in details["rejections"]))
        self.assertEqual(details["rejections"][0]["tree_inventory_after"], details["tree_inventory_before"])
        self.assertEqual(details["rejections"][1]["tree_inventory_after"], {})

    def test_url_policy_probe_fails_on_wrong_cli_result_fetch_publication_or_stale_metadata(self):
        for fault in ("successful-query", "archive-fetch", "new-tree", "changed-manifest", "missing-source-request"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
                with self.assertRaises(ValueError):
                    self.url_policy_probe(Path(directory), fault)

    def test_skipped_crc_probe_requires_zip_crc_diagnostic_download_and_clean_staging(self):
        for fault in (None, "wrong-error", "no-download", "stage-leak", "published-root", "success"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
                scratch = Path(directory)
                server = remote.FixtureServer({"requests": []})
                runner = mock.Mock()

                def run(name, argv, workspace):
                    source_path = "/zip-skipped-crc/first/registry/modules/dep/1.0/source.json"
                    source = json.loads(server.routes[source_path][1])
                    archive_path = "/zip-skipped-crc/first/source.zip"
                    data = server.routes[archive_path][1]
                    self.assertEqual(source["integrity"], remote.sri(data))
                    self.assertEqual(source["strip_prefix"], "pkg")
                    if fault != "no-download":
                        server.requests.append({"path": archive_path})
                    if fault in ("stage-leak", "published-root"):
                        cache = Path(next(arg.split("=", 1)[1] for arg in argv if arg.startswith("--cache-dir=")))
                        (cache / "repositories/trees" / (".stage-leaked" if fault == "stage-leak" else "published")).mkdir(parents=True)
                    return record(stderr="integrity mismatch" if fault == "wrong-error" else "read skipped ZIP entry: invalid checksum",
                                  returncode=0 if fault == "success" else 1)

                runner.run.side_effect = run
                try:
                    if fault is None:
                        details = remote.native_probe(remote.argument_parser().parse_args([]), runner, scratch,
                                                      scratch / "workspace", server, "zip-skipped-crc")
                        self.assertEqual(details["request_counts"]["/zip-skipped-crc/first/source.zip"], 1)
                        self.assertTrue(details["fixture"]["archive_sha256"])
                    else:
                        with self.assertRaises(ValueError):
                            remote.native_probe(remote.argument_parser().parse_args([]), runner, scratch,
                                                scratch / "workspace", server, "zip-skipped-crc")
                finally:
                    server.close()


class LifecycleTests(unittest.TestCase):
    def test_main_wires_each_requested_format_to_descriptor_and_served_payload(self):
        observed = {}

        def inspect_fixture(args, runner, scratch, workspace, fixture, reference, kind):
            descriptor = fixture["source"]
            self.assertTrue(descriptor["url"].endswith("/source." + kind))
            data = runner.server.routes[fixture["archive_path"]][1]
            self.assertEqual(data, remote.archive_bytes(kind))
            self.assertEqual(descriptor["integrity"], remote.sri(data))
            if kind == "zip":
                with zipfile.ZipFile(io.BytesIO(data)) as archive:
                    self.assertIsNone(archive.testzip())
            elif kind == "tar.gz":
                self.assertEqual(gzip.decompress(data), remote.archive_bytes("tar"))
            else:
                with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
                    self.assertIn("pkg/BUILD.bazel", archive.getnames())
            observed[kind] = remote.sha256(data)
            return {"format_verified": kind}

        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_lifecycle(directory, inspect_fixture)
        self.assertEqual(code, 0, report["errors"])
        self.assertEqual(set(observed), set(remote.FORMATS))
        self.assertEqual(len(set(observed.values())), len(remote.FORMATS))

    def test_fail_closed_report_predicate_rejects_missing_duplicate_reordered_or_wrong_kind(self):
        report = {"checks": [{"name": name, "kind": kind, "status": "PASS"} for name, kind in remote.PLAN],
                  "errors": [], "scratch_cleaned": True, "http_cleaned": True, "reference_shutdown_attempted": True}
        self.assertTrue(remote.report_passes(report))
        # A previous complete 15-check report is now incomplete, not a pass.
        previous = copy.deepcopy(report)
        previous["checks"] = previous["checks"][:-2]
        self.assertEqual(len(previous["checks"]), 15)
        self.assertFalse(remote.report_passes(previous))
        self.assertEqual([(c["name"], c["kind"]) for c in report["checks"][-2:]],
                         [("native.warm-blob-url-policy", "native_only"), ("native.zip-skipped-crc", "native_only")])
        for mutation in ("missing", "duplicate", "reordered", "wrong-kind", "failed", "error", "scratch", "http", "reference"):
            changed = copy.deepcopy(report)
            if mutation == "missing":
                changed["checks"].pop()
            elif mutation == "duplicate":
                changed["checks"][-1] = changed["checks"][0]
            elif mutation == "reordered":
                changed["checks"].reverse()
            elif mutation == "wrong-kind":
                changed["checks"][0]["kind"] = "native_only"
            elif mutation == "failed":
                changed["checks"][0]["status"] = "REFERENCE_FAILURE"
            elif mutation == "error":
                changed["errors"].append("shutdown failed")
            else:
                changed[{"scratch": "scratch_cleaned", "http": "http_cleaned", "reference": "reference_shutdown_attempted"}[mutation]] = False
            with self.subTest(mutation=mutation):
                self.assertFalse(remote.report_passes(changed))

    def test_missing_executable_publishes_failure_without_starting_server_or_subprocess(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            output = Path(directory) / "report.json"
            with mock.patch.object(remote, "FixtureServer") as server, \
                    mock.patch.object(remote.Runner, "run", side_effect=AssertionError("no subprocess")), \
                    mock.patch("builtins.print"):
                code = remote.main(["--native", directory + "/missing", "--report", str(output)])
            report = json.loads(output.read_text())
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "FAIL")
        self.assertTrue(report["errors"])
        self.assertEqual(report["runs"], [])
        server.assert_not_called()

    def run_lifecycle(self, directory, failure, shutdown_failure=None):
        output = Path(directory) / "report.json"
        reference = mock.Mock(output_base=Path(directory) / "mock-output-base")
        reference.run.return_value = record("release 9.3.0\n")
        reference.shutdown.side_effect = shutdown_failure
        servers = []
        real_server = remote.FixtureServer

        def factory(report):
            server = real_server(report)
            servers.append(server)
            return server

        with mock.patch.object(remote, "validate_binaries", return_value={}), \
                mock.patch.object(remote, "RemoteReference", return_value=reference), \
                mock.patch.object(remote, "FixtureServer", side_effect=factory), \
                mock.patch.object(remote, "differential", side_effect=failure), \
                mock.patch.object(remote, "cache_check", return_value={}), \
                mock.patch.object(remote, "native_probe", return_value={}), \
                mock.patch.object(remote.Runner, "run", side_effect=AssertionError("no subprocess")), \
                mock.patch("builtins.print"):
            code = remote.main(["--scratch-parent", directory, "--report", str(output)])
        report = json.loads(output.read_text())
        self.assertTrue(report["scratch_cleaned"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        self.assertTrue(report["http_cleaned"])
        self.assertFalse(servers[0].thread.is_alive())
        self.assertEqual(servers[0].httpd.socket.fileno(), -1)
        self.assertTrue(reference.shutdown.called)
        self.assertTrue(any(item["kind"] == "http_server" and item["joined"] for item in report["cleanup"]))
        return code, report

    def test_exception_interrupt_and_failed_check_always_cleanup_and_fail_report(self):
        for failure, expected in ((RuntimeError("unexpected error"), 1),
                                  (KeyboardInterrupt(), 130), (ValueError("reference failed"), 1)):
            with self.subTest(failure=type(failure).__name__), tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
                code, report = self.run_lifecycle(directory, failure)
                self.assertEqual(code, expected)
                self.assertEqual(report["status"], "FAIL")
                self.assertTrue(report["errors"] or any(item["status"] == "FAIL" for item in report["checks"]))

    def test_shutdown_exception_does_not_skip_http_and_scratch_cleanup(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            code, report = self.run_lifecycle(directory, RuntimeError("stop"), ValueError("shutdown failed"))
        self.assertEqual(code, 1)
        self.assertTrue(any("reference shutdown" in error for error in report["errors"]))


if __name__ == "__main__":
    unittest.main()
